//! Smithay-client-toolkit driver for the region selector.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat, wl_shm, wl_surface},
    Connection, Proxy, QueueHandle,
};

use super::{render, Selection, SelectionCancelled};

pub fn run_blocking(
    selection_tx: SyncSender<Result<Selection>>,
    stop_flag: Arc<AtomicBool>,
) -> Result<()> {
    tracing::debug!("selector: connecting to wayland");
    let conn = Connection::connect_to_env().context("connect to wayland display")?;
    let (globals, mut event_queue) =
        registry_queue_init(&conn).context("init wayland registry queue")?;
    let qh: QueueHandle<State> = event_queue.handle();

    let compositor =
        CompositorState::bind(&globals, &qh).context("wl_compositor not available")?;
    let layer_shell =
        LayerShell::bind(&globals, &qh).context("zwlr_layer_shell_v1 not available")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm not available")?;
    let registry_state = RegistryState::new(&globals);
    let output_state = OutputState::new(&globals, &qh);
    let seat_state = SeatState::new(&globals, &qh);

    let accent = crate::theme::accent_argb();
    let recording_blue = crate::theme::palette_blue_argb();
    tracing::debug!(
        accent = format!("{accent:#010x}"),
        recording_blue = format!("{recording_blue:#010x}"),
        "loaded theme colors"
    );

    let mut state = State {
        registry_state,
        output_state,
        seat_state,
        compositor,
        layer_shell,
        shm,
        qh: qh.clone(),
        surfaces: HashMap::new(),
        keyboard: None,
        pointer: None,
        drag: None,
        result: Outcome::Pending,
        accent,
        recording_blue,
        mode: Mode::Selecting,
        selection_tx: Some(selection_tx),
    };

    // Pump events until at least one output has full info AND we've created
    // surfaces for everything we can see. wl_output / xdg_output info arrives
    // across multiple events; one roundtrip often isn't enough.
    for attempt in 0..10 {
        event_queue.roundtrip(&mut state).context("initial roundtrip")?;
        state.sync_surfaces();
        tracing::debug!(
            attempt,
            outputs_known = state.output_state.outputs().count(),
            surfaces_built = state.surfaces.len(),
            "selector: bringup roundtrip"
        );
        if !state.surfaces.is_empty() {
            break;
        }
    }
    if state.surfaces.is_empty() {
        anyhow::bail!(
            "no wl_output surfaces could be created — \
             does cosmic-comp expose zwlr_layer_shell_v1 and xdg_output?"
        );
    }

    // CRITICAL: flush pending commits (the initial wl_surface.commit on each
    // layer-surface) so the compositor sends us configure events.
    event_queue.flush().context("flush after bringup")?;

    // Phase 1 — Selecting. Frame-callback driven repaint:
    //  * pointer motion only sets `dirty=true` on the affected surface; it
    //    does not paint.
    //  * `render_dirty` paints only surfaces with `dirty && !frame_pending`.
    //    After painting it requests a `wl_surface.frame()` callback and sets
    //    `frame_pending=true`.
    //  * When the compositor is ready for the next frame (i.e. at the
    //    display's native rate — 60 / 144 / 240 / VRR-whatever), it fires
    //    the callback. `CompositorHandler::frame` clears `frame_pending`.
    //  * `blocking_dispatch` wakes for that callback event (and for motion
    //    events), the loop runs `render_dirty` again, and any `dirty` set
    //    by intervening motion events flushes as one paint.
    //
    // Net effect: at most one paint per display refresh, no timer-based
    // throttle that would mismatch the actual display.
    while matches!(state.result, Outcome::Pending) {
        event_queue.blocking_dispatch(&mut state).context("dispatch")?;
        state.sync_surfaces();
        state.render_dirty();
        event_queue.flush().context("flush after dispatch")?;
    }

    match state.result {
        Outcome::Cancelled => {
            tracing::info!("selection cancelled");
            if let Some(tx) = state.selection_tx.take() {
                let _ = tx.send(Err(SelectionCancelled.into()));
            }
            state.surfaces.clear();
            event_queue.roundtrip(&mut state).ok();
            return Ok(());
        }
        Outcome::Confirmed => {
            tracing::info!("entering recording-overlay mode");
        }
        Outcome::Pending => unreachable!(),
    }

    // Phase 2 — Recording overlay. We can't use `blocking_dispatch` here
    // because the stop signal arrives via an atomic flag in another thread,
    // not via a wayland event, so the loop would never wake up. Instead:
    // prepare_read → poll the wayland fd with a 50ms timeout → read on
    // wake → dispatch_pending. Stop responsiveness is bounded by the poll
    // timeout.
    let mut last_render = Instant::now();
    while !stop_flag.load(Ordering::Relaxed) {
        event_queue.flush().context("flush in overlay loop")?;
        let dispatched = event_queue
            .dispatch_pending(&mut state)
            .context("dispatch_pending in overlay")?;
        if dispatched > 0 {
            state.sync_surfaces();
        }
        if last_render.elapsed() >= Duration::from_millis(16) {
            state.render_dirty();
            event_queue.flush().context("flush after render in overlay")?;
            last_render = Instant::now();
        }

        if let Some(guard) = conn.prepare_read() {
            let fd = guard.connection_fd();
            let mut pfd = [rustix::event::PollFd::new(&fd, rustix::event::PollFlags::IN)];
            // 50ms timeout — bounds stop_flag responsiveness, lets us
            // wake up periodically even if the compositor sends nothing.
            let _ = rustix::event::poll(&mut pfd, 50);
            if pfd[0].revents().contains(rustix::event::PollFlags::IN) {
                if let Err(e) = guard.read() {
                    tracing::warn!(error = ?e, "wayland read failed in overlay");
                    break;
                }
            }
            // If timeout: drop guard, loop again, check stop_flag.
        } else {
            // Events already buffered — let dispatch_pending pick them up
            // on the next iteration. Brief sleep so we don't busy-loop.
            std::thread::sleep(Duration::from_millis(16));
        }
    }

    tracing::debug!("recording overlay: stop signalled, tearing down");
    state.surfaces.clear();
    event_queue.roundtrip(&mut state).ok();
    Ok(())
}

#[derive(Debug)]
enum Outcome {
    Pending,
    Cancelled,
    /// Drag committed; we've sent the Selection to the caller and are now
    /// in [`Mode::Recording`].
    Confirmed,
}

#[derive(Debug, Clone, Copy)]
enum Mode {
    /// Drag selection phase: dim overlay across every output, full input.
    Selecting,
    /// Post-commit: one full-output layer surface with a transparent
    /// background and the rounded accent border drawn around the
    /// selected rect. Now safe because the Selecting phase uses
    /// `OnDemand` rather than `Exclusive` keyboard interactivity, which
    /// avoids cosmic-comp's exclusive-layer focus lock.
    Recording {
        /// wl_surface protocol id of the recording-mode surface.
        surface_pid: u32,
        /// Selection rectangle in physical buffer pixels (relative to
        /// the surface's top-left).
        rect_buffer: (i32, i32, u32, u32),
    },
}

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    qh: QueueHandle<State>,

    surfaces: HashMap<u32, OutputSurface>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    drag: Option<Drag>,
    result: Outcome,
    /// User's accent color — used for the selection border during Selecting.
    accent: u32,
    /// COSMIC palette blue — used for the recording-active border.
    recording_blue: u32,
    mode: Mode,
    /// Selection sender; taken once on commit/cancel.
    selection_tx: Option<SyncSender<Result<Selection>>>,
}

struct OutputSurface {
    /// Stable id of the backing wl_output (for output-add/remove tracking).
    output_pid: u32,
    output_name: String,
    output_logical_pos: (i32, i32),
    output_logical_size: (u32, u32),
    output_physical_size: (u32, u32),
    scale: i32,
    layer: LayerSurface,
    pool: SlotPool,
    buffer_size: (u32, u32),
    /// State has changed (drag moved, scale changed, etc.) — needs repaint.
    dirty: bool,
    /// We've committed a frame and are waiting for the compositor's frame
    /// callback. While true, skip repaints — the new state will be flushed
    /// the moment the callback arrives.
    frame_pending: bool,
    configured: bool,
}

struct Drag {
    /// Numeric id of the surface (wl_surface::id().protocol_id()) the drag started on.
    surface_id: u32,
    /// Drag start in surface-local logical coords.
    start: (f64, f64),
    /// Latest pointer position in surface-local logical coords.
    current: (f64, f64),
    /// Mouse is currently held (selecting). False after release on the same output.
    active: bool,
}

impl State {
    /// Reconcile our surfaces against the current set of known wl_outputs.
    /// Idempotent and cheap to call after every dispatch.
    fn sync_surfaces(&mut self) {
        let outputs: Vec<_> = self.output_state.outputs().collect();
        for wl_output in outputs {
            self.create_surface_for(wl_output);
        }
    }

    fn create_surface_for(&mut self, wl_output: wl_output::WlOutput) {
        let info = match self.output_state.info(&wl_output) {
            Some(i) => i,
            None => {
                tracing::trace!("output info not ready yet, skipping");
                return;
            }
        };
        let output_pid = wl_output.id().protocol_id();
        if self.surfaces.values().any(|s| s.output_pid == output_pid) {
            return;
        }
        tracing::debug!(
            output = info.name.as_deref().unwrap_or("?"),
            scale = info.scale_factor,
            logical = ?info.logical_size,
            "creating layer-surface for output"
        );

        let surface = self.compositor.create_surface(&self.qh);
        // Layer::Top rather than Overlay: cosmic-comp gates Overlay for
        // system clients (panels, notifications). Top still renders above
        // normal windows; for an interactive selector it's the right tier.
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Top,
            Some("cosmic-capture-selector"),
            Some(&wl_output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        // OnDemand, NOT Exclusive. cosmic-comp's focus stack puts the
        // entire compositor into "exclusive-layer-only focus" mode the
        // moment any Exclusive surface exists, and that state appears
        // to be sticky — even after we destroy our Exclusive surfaces
        // and create None surfaces, focus doesn't reliably return to
        // the underlying app. OnDemand lets the click-to-drag itself
        // grant us focus and a subsequent destroy releases cleanly.
        layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
        layer.set_size(0, 0);
        layer.commit();
        tracing::debug!(output = ?info.name, "layer-surface committed (Layer::Top, OnDemand)");

        let logical_size = info
            .logical_size
            .map(|(w, h)| (w as u32, h as u32))
            .or_else(|| info.modes.iter().find(|m| m.current).map(|m| (m.dimensions.0 as u32, m.dimensions.1 as u32)))
            .unwrap_or((0, 0));
        let logical_pos = info.logical_position.unwrap_or((0, 0));
        let physical = info
            .modes
            .iter()
            .find(|m| m.current)
            .map(|m| (m.dimensions.0 as u32, m.dimensions.1 as u32))
            .unwrap_or(logical_size);

        // SlotPool sized for the largest plausible frame (4 bytes/px, double-buffered).
        let initial_bytes = ((physical.0.max(1) * physical.1.max(1)) as usize) * 4 * 2;
        let pool = SlotPool::new(initial_bytes.max(4096), &self.shm).expect("create slot pool");

        // Key the map by wl_surface protocol id — that's what configure and
        // pointer events provide. (Earlier this was wl_output's id, which
        // never matched and caused every event handler to silently no-op.)
        let surface_pid = layer.wl_surface().id().protocol_id();
        self.surfaces.insert(
            surface_pid,
            OutputSurface {
                output_pid,
                output_name: info.name.unwrap_or_else(|| format!("output-{output_pid}")),
                output_logical_pos: logical_pos,
                output_logical_size: logical_size,
                output_physical_size: physical,
                scale: info.scale_factor,
                layer,
                pool,
                buffer_size: (0, 0),
                dirty: true,
                frame_pending: false,
                configured: false,
            },
        );
    }

    fn render_dirty(&mut self) {
        let drag = self.drag.as_ref();
        let mode = self.mode;
        let accent = self.accent;
        let recording_blue = self.recording_blue;
        let qh = self.qh.clone();
        for surf in self.surfaces.values_mut() {
            if !surf.configured || !surf.dirty || surf.frame_pending {
                continue;
            }

            let surf_pid = surf.layer.wl_surface().id().protocol_id();
            // Selecting: accent (system color) border on dim background.
            // Recording: a fixed bright blue so the "REC on" state is
            // visually unambiguous and doesn't blend into whatever
            // accent the user has configured.
            let (cutout, background, border_color) = match mode {
                Mode::Selecting => {
                    let cutout = drag.and_then(|d| {
                        if d.surface_id == surf_pid {
                            let s = surf.scale.max(1) as f64;
                            let x0 = d.start.0.min(d.current.0) * s;
                            let y0 = d.start.1.min(d.current.1) * s;
                            let x1 = d.start.0.max(d.current.0) * s;
                            let y1 = d.start.1.max(d.current.1) * s;
                            Some((
                                x0.round() as i32,
                                y0.round() as i32,
                                (x1 - x0).round().max(0.0) as u32,
                                (y1 - y0).round().max(0.0) as u32,
                            ))
                        } else {
                            None
                        }
                    });
                    (cutout, render::DIM_BACKGROUND, accent)
                }
                Mode::Recording { surface_pid, rect_buffer } => {
                    if surface_pid != surf_pid {
                        continue;
                    }
                    (Some(rect_buffer), render::TRANSPARENT_BACKGROUND, recording_blue)
                }
            };

            let (w, h) = surf.buffer_size;
            let stride = w as i32 * 4;
            let (buffer, canvas) = match surf.pool.create_buffer(
                w as i32,
                h as i32,
                stride,
                wl_shm::Format::Argb8888,
            ) {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(error = %e, "create_buffer failed");
                    continue;
                }
            };
            render::paint(canvas, w, h, cutout, border_color, background);

            let wl_surf = surf.layer.wl_surface();
            wl_surf.set_buffer_scale(surf.scale.max(1));
            wl_surf.damage_buffer(0, 0, w as i32, h as i32);
            buffer
                .attach_to(wl_surf)
                .expect("attach buffer to surface");
            // Request a frame callback BEFORE commit so the compositor
            // links it to this frame's presentation. Routed via sctk's
            // CompositorHandler::frame because the user-data is the
            // wl_surface itself.
            wl_surf.frame(&qh, wl_surf.clone());
            wl_surf.commit();
            surf.dirty = false;
            surf.frame_pending = true;
        }
    }
}

// ─── sctk handler impls ────────────────────────────────────────────────────

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        let pid = surface.id().protocol_id();
        if let Some(s) = self.surfaces.get_mut(&pid) {
            s.scale = new_factor;
            s.dirty = true;
        }
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        let pid = surface.id().protocol_id();
        if let Some(s) = self.surfaces.get_mut(&pid) {
            s.frame_pending = false;
            // Note: we deliberately do NOT set s.dirty = true here. The
            // next render_dirty pass only paints if motion (or another
            // event) has marked the surface dirty since the last commit.
            // No dirt → no paint, even though the compositor woke us.
        }
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        // Surface creation happens in sync_surfaces() after each dispatch,
        // so we don't need to do anything here. Logging only.
        tracing::trace!("new_output event");
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let pid = output.id().protocol_id();
        self.surfaces.retain(|_, s| s.output_pid != pid);
    }
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Keyboard if self.keyboard.is_none() => {
                self.keyboard = self
                    .seat_state
                    .get_keyboard(qh, &seat, None)
                    .ok();
            }
            Capability::Pointer if self.pointer.is_none() => {
                self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
            }
            _ => {}
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Keyboard => {
                if let Some(k) = self.keyboard.take() {
                    k.release();
                }
            }
            Capability::Pointer => {
                if let Some(p) = self.pointer.take() {
                    p.release();
                }
            }
            _ => {}
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {
    }
}

impl LayerShellHandler for State {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        let pid = layer.wl_surface().id().protocol_id();
        self.surfaces.remove(&pid);
        // During Selecting, an unsolicited close is a cancel signal. During
        // Recording, the compositor closing our overlay (rare) just removes
        // the border — don't fault the recording pipeline.
        if matches!(self.mode, Mode::Selecting) {
            self.result = Outcome::Cancelled;
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let pid = layer.wl_surface().id().protocol_id();
        let Some(surf) = self.surfaces.get_mut(&pid) else { return };
        let (logical_w, logical_h) = configure.new_size;
        let scale = surf.scale.max(1) as u32;
        let w = if logical_w == 0 {
            surf.output_physical_size.0
        } else {
            logical_w * scale
        };
        let h = if logical_h == 0 {
            surf.output_physical_size.1
        } else {
            logical_h * scale
        };
        tracing::debug!(
            output = %surf.output_name,
            logical_w, logical_h, scale, buffer_w = w, buffer_h = h,
            "layer-surface configure"
        );
        if w == 0 || h == 0 {
            tracing::warn!("configure with zero size; skipping render");
            return;
        }
        surf.buffer_size = (w, h);
        surf.configured = true;
        surf.dirty = true;
    }
}

impl PointerHandler for State {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        // In Recording mode the chosen surface is input-transparent, but
        // events that were buffered before the new region took effect can
        // still arrive. Drop them rather than starting a new drag.
        if !matches!(self.mode, Mode::Selecting) {
            return;
        }
        for ev in events {
            let pid = ev.surface.id().protocol_id();
            let on_known_surface = self.surfaces.contains_key(&pid);
            if !on_known_surface {
                continue;
            }
            match ev.kind {
                PointerEventKind::Press { button, .. } if button == 0x110 /* BTN_LEFT */ => {
                    self.drag = Some(Drag {
                        surface_id: pid,
                        start: ev.position,
                        current: ev.position,
                        active: true,
                    });
                    if let Some(s) = self.surfaces.get_mut(&pid) {
                        s.dirty = true;
                    }
                }
                PointerEventKind::Motion { .. } => {
                    if let Some(drag) = self.drag.as_mut() {
                        if drag.active && drag.surface_id == pid {
                            drag.current = ev.position;
                            if let Some(s) = self.surfaces.get_mut(&pid) {
                                s.dirty = true;
                            }
                        }
                    }
                }
                PointerEventKind::Release { button, .. } if button == 0x110 => {
                    if let Some(drag) = self.drag.as_mut() {
                        if drag.surface_id == pid {
                            drag.current = ev.position;
                            drag.active = false;
                            self.commit_drag();
                        }
                    }
                }
                PointerEventKind::Press { button, .. } if button == 0x111 /* BTN_RIGHT */ => {
                    // Right-click cancels — works without keyboard focus,
                    // since layer-shell OnDemand only grants keys after a
                    // first interaction with the surface. This gives the
                    // user a focus-independent escape hatch.
                    tracing::info!("right-click → cancelling selection");
                    self.result = Outcome::Cancelled;
                }
                _ => {}
            }
        }
    }
}

impl State {
    fn commit_drag(&mut self) {
        let Some(drag) = self.drag.take() else { return };
        let Some(surf) = self.surfaces.get(&drag.surface_id) else { return };
        let x0 = drag.start.0.min(drag.current.0).max(0.0) as i32;
        let y0 = drag.start.1.min(drag.current.1).max(0.0) as i32;
        let x1 = drag.start.0.max(drag.current.0).max(0.0) as i32;
        let y1 = drag.start.1.max(drag.current.1).max(0.0) as i32;
        let w = (x1 - x0).max(1) as u32;
        let h = (y1 - y0).max(1) as u32;
        if w < 8 || h < 8 {
            self.result = Outcome::Cancelled;
            return;
        }

        let selection = Selection {
            output_name: surf.output_name.clone(),
            output_logical_pos: surf.output_logical_pos,
            output_logical_size: surf.output_logical_size,
            region_logical: (x0, y0, w, h),
            scale: surf.scale.max(1) as f64,
        };
        tracing::info!(
            output = %selection.output_name,
            region = ?selection.region_logical,
            "selection committed"
        );

        // Send to caller. If they've already dropped the receiver, fall
        // through to teardown rather than entering recording mode.
        if let Some(tx) = self.selection_tx.take() {
            if tx.send(Ok(selection.clone())).is_err() {
                self.result = Outcome::Cancelled;
                return;
            }
        } else {
            self.result = Outcome::Cancelled;
            return;
        }

        // Transition to recording mode: destroy every existing surface
        // (releases keyboard focus), then bring up a single fresh full-
        // output layer surface with `KeyboardInteractivity::None` and an
        // empty input region. The render path paints transparent + a
        // rounded accent border around the selection rect.
        let scale = surf.scale.max(1) as f64;
        let chosen_output_pid = surf.output_pid;
        let chosen_output_name = surf.output_name.clone();
        let chosen_logical_pos = surf.output_logical_pos;
        let chosen_logical_size = surf.output_logical_size;
        let chosen_physical_size = surf.output_physical_size;
        let chosen_scale = surf.scale;

        let rect_buffer = (
            (x0 as f64 * scale).round() as i32,
            (y0 as f64 * scale).round() as i32,
            (w as f64 * scale).round().max(1.0) as u32,
            (h as f64 * scale).round().max(1.0) as u32,
        );

        self.result = Outcome::Confirmed;

        let wl_output = self
            .output_state
            .outputs()
            .find(|o| o.id().protocol_id() == chosen_output_pid);

        if let Some(kb) = self.keyboard.take() {
            kb.release();
        }
        if let Some(pt) = self.pointer.take() {
            pt.release();
        }
        self.surfaces.clear();

        let Some(wl_output) = wl_output else {
            tracing::warn!(
                output_pid = chosen_output_pid,
                "output disappeared before recording overlay could be created"
            );
            self.mode = Mode::Recording { surface_pid: 0, rect_buffer };
            return;
        };

        let fresh_surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            fresh_surface,
            Layer::Top,
            Some("cosmic-capture-recording"),
            Some(&wl_output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);
        let empty = self.compositor.wl_compositor().create_region(&self.qh, ());
        layer.wl_surface().set_input_region(Some(&empty));
        empty.destroy();
        layer.commit();

        let new_pid = layer.wl_surface().id().protocol_id();
        let bytes = (chosen_physical_size.0.max(1) as usize)
            * (chosen_physical_size.1.max(1) as usize)
            * 4
            * 2;
        let pool = SlotPool::new(bytes.max(4096), &self.shm).expect("create slot pool");

        self.surfaces.insert(
            new_pid,
            OutputSurface {
                output_pid: chosen_output_pid,
                output_name: chosen_output_name,
                output_logical_pos: chosen_logical_pos,
                output_logical_size: chosen_logical_size,
                output_physical_size: chosen_physical_size,
                scale: chosen_scale,
                layer,
                pool,
                buffer_size: (0, 0),
                dirty: true,
                frame_pending: false,
                configured: false,
            },
        );
        self.mode = Mode::Recording { surface_pid: new_pid, rect_buffer };
        tracing::debug!(
            new_pid,
            output = %self.surfaces[&new_pid].output_name,
            "recording overlay surface created"
        );
    }
}

impl KeyboardHandler for State {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
        tracing::debug!(
            surface = surface.id().protocol_id(),
            "keyboard enter — Esc now routed to selector"
        );
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        tracing::debug!(keysym = ?event.keysym, ?self.mode, "press_key");
        // Escape cancels — but only during Selecting. In Recording mode the
        // surface is input-transparent so we shouldn't receive keys, but be
        // defensive about it.
        if event.keysym == Keysym::Escape && matches!(self.mode, Mode::Selecting) {
            tracing::info!("Esc → cancelling selection");
            self.result = Outcome::Cancelled;
        }
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _modifiers: Modifiers,
        _layout: u32,
    ) {
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(State);

// wl_region has no events, so sctk's delegate_compositor doesn't bother
// providing a Dispatch impl for it. We hold short-lived empty regions
// (just to make a surface input-transparent) and need this stub.
impl wayland_client::Dispatch<wl_region::WlRegion, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_region::WlRegion,
        _: wl_region::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
delegate_output!(State);
delegate_seat!(State);
delegate_pointer!(State);
delegate_keyboard!(State);
delegate_layer!(State);
delegate_shm!(State);
delegate_registry!(State);

