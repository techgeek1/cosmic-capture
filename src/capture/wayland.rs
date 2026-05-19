//! Persistent wayland helper. Mirrors `xdg-desktop-portal-cosmic`'s
//! `WaylandHelper`: one long-lived `Connection`, a background event-pump
//! thread, and live `output_toplevels` / `output_infos` / `toplevels`
//! state. Captures are async sessions on the same connection — no fresh
//! `connect_to_env` per request.
//!
//! Two capture paths:
//! * `capture_source_shm` — the legacy memfd/`wl_shm` path. Still used by
//!   the screenshot pipeline and the live-thumbnail picker (consumers
//!   that want CPU-resident RGBA bytes anyway).
//! * `capture_source_dmabuf` — GBM-allocated dmabuf written into directly
//!   by the compositor. The fd is handed to gst's `vapostproc` for the
//!   zero-copy recording path; no CPU touch from capture to encode.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;

use anyhow::{Context, Result};
use tokio::sync::Notify;
use cosmic_client_toolkit::{
    cosmic_protocols::toplevel_management::v1::client::zcosmic_toplevel_manager_v1,
    screencopy::{
        CaptureFrame, CaptureOptions, CaptureSession, CaptureSource, Capturer, FailureReason,
        Formats, Frame, ScreencopyFrameData, ScreencopyFrameDataExt, ScreencopyHandler,
        ScreencopySessionData, ScreencopySessionDataExt, ScreencopyState,
    },
    toplevel_info::{ToplevelInfo, ToplevelInfoHandler, ToplevelInfoState},
    toplevel_management::{ToplevelManagerHandler, ToplevelManagerState},
    workspace::{WorkspaceHandler, WorkspaceState},
};
use futures_util::stream::{FuturesOrdered, Stream, StreamExt};
use tokio::sync::oneshot;
use smithay_client_toolkit::{
    dmabuf::DmaBufferData,
    output::{OutputHandler, OutputInfo, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    seat::{Capability, SeatHandler, SeatState},
    shm::{Shm, ShmHandler},
};
use std::os::fd::{AsFd, OwnedFd};
use wayland_client::{
    Connection, QueueHandle, WEnum, delegate_noop,
    globals::{registry_queue_init, GlobalList},
    protocol::{wl_buffer, wl_output, wl_seat, wl_shm, wl_shm_pool},
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1;
use wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};
use smithay_client_toolkit::globals::GlobalData;

use super::dmabuf::{DmabufFrame, GbmRegistry, pick_format};
use super::screencopy::CapturedFrame;

/// One toplevel's captured thumbnail plus the metadata the GUI needs
/// to label and identify it.
pub struct WindowCapture {
    pub identifier: String,
    pub title: String,
    pub app_id: String,
    pub frame: CapturedFrame,
}

#[derive(Clone)]
pub struct WaylandHelper {
    inner: Arc<WaylandHelperInner>,
}

struct WaylandHelperInner {
    /// Connection handle — kept around so request-side code (running on
    /// tokio tasks, not the dispatch thread) can `flush()` after pushing
    /// new requests. Without this, the dispatch loop blocks on read,
    /// our screencopy/buffer creation requests never reach the
    /// compositor, and every capture session times out.
    conn: Connection,
    qh: QueueHandle<AppData>,
    capturer: Capturer,
    /// `wl_shm` is a wayland proxy that's cheaply cloneable. We use it
    /// directly to allocate per-capture pools from a memfd, sidestepping
    /// the smithay `Shm` state (which isn't Clone and lives in the
    /// dispatch thread's AppData).
    wl_shm: wl_shm::WlShm,
    outputs: Mutex<Vec<wl_output::WlOutput>>,
    output_infos: Mutex<HashMap<wl_output::WlOutput, OutputInfo>>,
    /// Active toplevels grouped by output. Maintained reactively from
    /// `ToplevelInfoHandler` callbacks via `update_output_toplevels` —
    /// same machinery the portal uses, scoped to currently-active
    /// workspace so windows on hidden workspaces don't appear.
    output_toplevels: Mutex<HashMap<wl_output::WlOutput, Vec<ExtForeignToplevelHandleV1>>>,
    toplevels: Mutex<Vec<ToplevelInfo>>,
    /// Latches `true` once cosmic-toplevel-info emits its `Done` event
    /// (the protocol's signal that the initial batch of toplevels has been
    /// fully delivered). The portal doesn't need this because its helper
    /// outlives any user interaction — by the time someone clicks "Window"
    /// in its UI the state has long since converged. We're a short-lived
    /// GUI: the first picker open often races the dispatch thread and
    /// sees an empty `output_toplevels`. Callers `await
    /// wait_for_toplevel_info()` before snapshotting.
    info_done: AtomicBool,
    info_done_notify: Notify,
    /// `wl_seat` handles for `activate_toplevel`. cosmic-comp's
    /// toplevel-management `activate` request takes both a toplevel and a
    /// seat (it scopes the focus change to the seat that asked). We track
    /// every seat the registry advertises and use the first one — most
    /// installations have exactly one seat.
    seats: Mutex<Vec<wl_seat::WlSeat>>,
    /// `Some` when cosmic-comp advertises the unstable
    /// `zcosmic_toplevel_manager_v1` global, `None` otherwise (e.g.
    /// vanilla Wayland compositors). Activate is a best-effort hint
    /// either way — we silently no-op when unavailable.
    toplevel_manager: Mutex<Option<zcosmic_toplevel_manager_v1::ZcosmicToplevelManagerV1>>,
    /// Lazily-populated GBM device cache keyed by render-node `dev_t`.
    /// Used by `capture_source_dmabuf` to allocate GPU-resident buffers
    /// the compositor writes into directly.
    gbm_registry: Arc<GbmRegistry>,
    /// `wl_globals` retained so we can bind the `zwp_linux_dmabuf_v1`
    /// global on first dmabuf capture rather than eagerly at GUI
    /// startup. Eager binding triggered cosmic-comp to flood the
    /// dispatch thread with Format/Modifier events that competed with
    /// the picker's screencopy events + iced's surface setup, surfacing
    /// as a multi-second picker-render lag. Now the bind cost is paid
    /// only when recording starts.
    globals: Arc<GlobalList>,
    /// Cached `zwp_linux_dmabuf_v1` proxy, bound lazily on first
    /// dmabuf capture. `None` until the first bind succeeds; `Some` for
    /// the rest of the session.
    linux_dmabuf: Mutex<Option<ZwpLinuxDmabufV1>>,
}

struct AppData {
    helper: WaylandHelper,
    registry_state: RegistryState,
    output_state: OutputState,
    shm_state: Shm,
    seat_state: SeatState,
    screencopy_state: ScreencopyState,
    workspace_state: WorkspaceState,
    toplevel_info_state: ToplevelInfoState,
    /// Optional because cosmic-comp may not advertise the unstable
    /// management protocol on every build — we still want to start
    /// without it and just no-op `activate_toplevel` if it's missing.
    toplevel_manager_state: Option<ToplevelManagerState>,
}

impl AppData {
    /// Recompute `output_toplevels` from the current toplevel + workspace
    /// state. Direct port of xdg-desktop-portal-cosmic's
    /// `update_output_toplevels`: for each toplevel, find an active
    /// workspace it sits on, and map every output of that workspace to
    /// the toplevel handle. Toplevels on inactive workspaces are skipped
    /// (same scoping the portal applies — you can't pick a window that
    /// isn't visible).
    fn update_output_toplevels(&self) {
        let toplevels = self.toplevel_info_state.toplevels();
        let mut guard = self.helper.inner.output_toplevels.lock().unwrap();
        *guard = toplevels
            .filter_map(|info| {
                let outputs = self.workspace_state.workspace_groups().find_map(|wg| {
                    wg.workspaces
                        .iter()
                        .filter_map(|h| self.workspace_state.workspace_info(h))
                        .find_map(|w| {
                            info.workspace
                                .iter()
                                .any(|x| {
                                    x == &w.handle
                                        && w.state.contains(
                                            ext_workspace_handle_v1::State::Active,
                                        )
                                })
                                .then(|| info.output.iter().cloned().collect::<Vec<_>>())
                        })
                })?;
                Some((outputs, info.foreign_toplevel.clone()))
            })
            .fold(HashMap::new(), |mut map, (outputs, toplevel)| {
                for o in outputs {
                    map.entry(o).or_insert_with(Vec::new).push(toplevel.clone());
                }
                map
            });

        *self.helper.inner.toplevels.lock().unwrap() =
            self.toplevel_info_state.toplevels().cloned().collect();
    }
}

#[derive(Default)]
struct SessionState {
    formats: Option<Formats>,
    stopped: bool,
    wakers: Vec<std::task::Waker>,
}

struct SessionInner {
    capture_session: CaptureSession,
    state: Mutex<SessionState>,
}

pub struct Session(Arc<SessionInner>);

impl Session {
    fn for_session(session: &CaptureSession) -> Option<Self> {
        session.data::<SessionData>()?.session.upgrade().map(Self)
    }

    fn update<F: FnOnce(&mut SessionState)>(&self, f: F) {
        let mut state = self.0.state.lock().unwrap();
        f(&mut state);
        for waker in std::mem::take(&mut state.wakers) {
            waker.wake();
        }
    }

    async fn wait_for_formats<T, F: FnMut(&Formats) -> T>(&self, mut cb: F) -> Option<T> {
        std::future::poll_fn(|cx| {
            let mut state = self.0.state.lock().unwrap();
            if state.stopped {
                std::task::Poll::Ready(None)
            } else if let Some(f) = &state.formats {
                std::task::Poll::Ready(Some(cb(f)))
            } else {
                state.wakers.push(cx.waker().clone());
                std::task::Poll::Pending
            }
        })
        .await
    }
}

struct SessionData {
    session: Weak<SessionInner>,
    session_data: ScreencopySessionData,
}

impl ScreencopySessionDataExt for SessionData {
    fn screencopy_session_data(&self) -> &ScreencopySessionData {
        &self.session_data
    }
}

struct FrameData {
    frame_data: ScreencopyFrameData,
    /// Sender returns either the read-back `CapturedFrame` or a screencopy
    /// failure reason. Filled by the dispatch thread inside `ready` /
    /// `failed`.
    sender: Mutex<
        Option<oneshot::Sender<std::result::Result<CapturedFrame, WEnum<FailureReason>>>>,
    >,
    /// Memfd-backed shm: the fd is mmap'd in `ready` to copy the bytes
    /// out before the pool is destroyed. Held in `Option` so `ready` can
    /// `take()` it.
    shm_fd: Mutex<Option<OwnedFd>>,
    width: u32,
    height: u32,
    stride: u32,
}

impl ScreencopyFrameDataExt for FrameData {
    fn screencopy_frame_data(&self) -> &ScreencopyFrameData {
        &self.frame_data
    }
}

/// Per-capture userdata for the dmabuf path. Carries only the completion
/// signal — the BO itself lives on the outer task that initiated the
/// capture, so the dispatch thread doesn't need to touch GPU memory.
struct DmabufFrameData {
    frame_data: ScreencopyFrameData,
    sender: Mutex<Option<oneshot::Sender<std::result::Result<(), WEnum<FailureReason>>>>>,
}

impl ScreencopyFrameDataExt for DmabufFrameData {
    fn screencopy_frame_data(&self) -> &ScreencopyFrameData {
        &self.frame_data
    }
}

impl WaylandHelperInner {
    /// Lazy-bind the `zwp_linux_dmabuf_v1` global. Returns the cached
    /// proxy on subsequent calls. Held across the session because
    /// re-binding is wasted work and re-triggers the Format/Modifier
    /// flood on every recording start.
    fn ensure_linux_dmabuf(&self) -> Result<ZwpLinuxDmabufV1> {
        let mut slot = self.linux_dmabuf.lock().unwrap();
        if let Some(p) = slot.as_ref() {
            return Ok(p.clone());
        }
        let proxy: ZwpLinuxDmabufV1 = self
            .globals
            .bind(&self.qh, 3..=5, GlobalData)
            .map_err(|e| anyhow::anyhow!("bind zwp_linux_dmabuf_v1 v3-5: {e}"))?;
        *slot = Some(proxy.clone());
        Ok(proxy)
    }
}

impl WaylandHelper {
    /// Direct port of xdg-desktop-portal-cosmic's `WaylandHelper::new`.
    /// Takes an already-opened connection (the portal opens it inside
    /// `CosmicPortal::init` *after* iced has started — see app.rs), does
    /// a single roundtrip to bind globals, then hands the event queue to
    /// a background dispatch thread. **No `info_done` wait**: the portal
    /// has no such wait (its `toplevel.rs` even notes `TODO any indication
    /// when we have all toplevels?`), and the wait was previously
    /// blocking iced startup before init() ran.
    pub fn new(conn: Connection) -> Result<Self> {
        let (globals, mut q) =
            registry_queue_init::<AppData>(&conn).context("registry_queue_init")?;
        let qh = q.handle();

        // Wrap globals in Arc up-front so we can both retain it on the
        // helper (for lazy `zwp_linux_dmabuf_v1` binding later) and
        // continue using it via `&*globals` for the immediate
        // bindings below.
        let globals = Arc::new(globals);
        let registry_state = RegistryState::new(&*globals);
        let screencopy_state = ScreencopyState::new(&*globals, &qh);
        let shm_state = Shm::bind(&*globals, &qh).context("bind wl_shm")?;
        // Note: zwp_linux_dmabuf_v1 is *not* bound here. Binding it
        // eagerly at startup floods the dispatch thread with the
        // compositor's Format/Modifier event list, delaying the picker
        // captures + iced's surface setup. The dmabuf record path
        // lazy-binds via `WaylandHelperInner::ensure_linux_dmabuf` on
        // first `capture_source_dmabuf` call.

        let helper = WaylandHelper {
            inner: Arc::new(WaylandHelperInner {
                conn: conn.clone(),
                qh: qh.clone(),
                capturer: screencopy_state.capturer().clone(),
                wl_shm: shm_state.wl_shm().clone(),
                outputs: Mutex::new(Vec::new()),
                output_infos: Mutex::new(HashMap::new()),
                output_toplevels: Mutex::new(HashMap::new()),
                toplevels: Mutex::new(Vec::new()),
                info_done: AtomicBool::new(false),
                info_done_notify: Notify::new(),
                seats: Mutex::new(Vec::new()),
                toplevel_manager: Mutex::new(None),
                gbm_registry: Arc::new(GbmRegistry::new()),
                globals: globals.clone(),
                linux_dmabuf: Mutex::new(None),
            }),
        };

        // SeatState seeds itself from the registry on construction; copy
        // those initial seats into the helper so activate_toplevel works
        // before any hotplug event fires.
        let seat_state = SeatState::new(&*globals, &qh);
        *helper.inner.seats.lock().unwrap() = seat_state.seats().collect();
        let toplevel_manager_state = ToplevelManagerState::try_new(&registry_state, &qh);
        if let Some(s) = &toplevel_manager_state {
            *helper.inner.toplevel_manager.lock().unwrap() = Some(s.manager.clone());
        } else {
            tracing::info!(
                "compositor did not advertise zcosmic_toplevel_manager_v1 — \
                 window picker will not raise the chosen toplevel"
            );
        }

        let mut data = AppData {
            helper: helper.clone(),
            output_state: OutputState::new(&*globals, &qh),
            shm_state,
            seat_state,
            screencopy_state,
            workspace_state: WorkspaceState::new(&registry_state, &qh),
            toplevel_info_state: ToplevelInfoState::new(&registry_state, &qh),
            toplevel_manager_state,
            registry_state,
        };

        // Explicit flush then a single roundtrip — matches the portal's
        // init shape. The dispatch thread takes it from here.
        q.flush().context("initial flush")?;
        q.roundtrip(&mut data).context("initial roundtrip")?;

        // Background event-pump thread. Runs for process lifetime; if
        // dispatch ever errors (compositor disconnected) we log and
        // exit the loop — the helper handles will surface that as
        // captures returning None.
        thread::spawn(move || {
            loop {
                if let Err(e) = q.blocking_dispatch(&mut data) {
                    tracing::error!(error = %e, "wayland helper event pump exited");
                    break;
                }
            }
        });

        Ok(helper)
    }

    pub fn outputs(&self) -> Vec<wl_output::WlOutput> {
        self.inner.outputs.lock().unwrap().clone()
    }

    pub fn output_info(&self, output: &wl_output::WlOutput) -> Option<OutputInfo> {
        self.inner.output_infos.lock().unwrap().get(output).cloned()
    }

    /// Async wait for cosmic-toplevel-info to emit its initial `Done`
    /// event. Returns immediately if it has already fired. Bounded so a
    /// compositor that never emits `Done` can't wedge a picker open.
    pub async fn wait_for_toplevel_info(&self) {
        if self.inner.info_done.load(Ordering::Acquire) {
            return;
        }
        // Register for notification *before* the second check, so we
        // can't miss a wakeup between the load and the await.
        let notified = self.inner.info_done_notify.notified();
        if self.inner.info_done.load(Ordering::Acquire) {
            return;
        }
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            notified,
        )
        .await;
    }

    /// Resolve a stable `identifier` (from `ext_foreign_toplevel_list_v1`)
    /// back to the live wayland proxy. Used by the recording path to take
    /// a click in our picker and start a screencopy session against the
    /// matching toplevel.
    pub fn toplevel_handle_for_identifier(
        &self,
        id: &str,
    ) -> Option<ExtForeignToplevelHandleV1> {
        self.inner
            .toplevels
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.identifier == id)
            .map(|t| t.foreign_toplevel.clone())
    }

    /// Ask the compositor to raise + focus the toplevel identified by
    /// `identifier`. Used right after the user clicks a tile in the window
    /// picker so the captured window comes to the foreground — otherwise a
    /// minimized/background pick would screenshot a stale buffer and the
    /// user would see no visible state change confirming their click.
    ///
    /// Best-effort: returns silently if the compositor doesn't expose
    /// `zcosmic_toplevel_manager_v1` (vanilla Wayland), if no `wl_seat` is
    /// available, or if the identifier no longer maps to a live toplevel.
    /// The activate request flips focus state via `state` events on the
    /// `zcosmic_toplevel_handle_v1`; we don't await those — by the time
    /// the screencopy fires the compositor will already have started
    /// painting the raised buffer.
    pub fn activate_toplevel(&self, identifier: &str) {
        let manager = self.inner.toplevel_manager.lock().unwrap().clone();
        let Some(manager) = manager else {
            tracing::debug!(identifier, "activate_toplevel: no manager available");
            return;
        };
        let seat = self.inner.seats.lock().unwrap().first().cloned();
        let Some(seat) = seat else {
            tracing::warn!(identifier, "activate_toplevel: no wl_seat known yet");
            return;
        };
        // cosmic-comp ties the management protocol to the cosmic toplevel
        // handle (not the ext_foreign one we use for screencopy). We look
        // up both: the cosmic handle drives activate, the foreign handle
        // is what the rest of the codebase keys on.
        let cosmic_handle = self
            .inner
            .toplevels
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.identifier == identifier)
            .and_then(|t| t.cosmic_toplevel.clone());
        let Some(handle) = cosmic_handle else {
            tracing::warn!(
                identifier,
                "activate_toplevel: no cosmic_toplevel_handle for identifier \
                 (compositor did not advertise zcosmic_toplevel_info_v1?)"
            );
            return;
        };
        manager.activate(&handle, &seat);
        if let Err(e) = self.inner.conn.flush() {
            tracing::warn!(error = %e, "flush after activate failed");
        }
    }

    /// Return the toplevel's geometry on the named output, if known.
    /// Coordinates are output-local logical pixels — `(x, y)` is the
    /// upper-left of the toplevel relative to the upper-left of the
    /// output. Returns `None` if the toplevel is unknown, isn't on
    /// that output, or the compositor hasn't advertised the
    /// `geometry` event yet (needs `zcosmic_toplevel_info_v1` v2).
    pub fn toplevel_geometry_on_output(
        &self,
        identifier: &str,
        output_name: &str,
    ) -> Option<(i32, i32, i32, i32)> {
        let toplevels = self.inner.toplevels.lock().unwrap();
        let info = toplevels.iter().find(|t| t.identifier == identifier)?;
        let infos = self.inner.output_infos.lock().unwrap();
        let output = infos
            .iter()
            .find(|(_, i)| i.name.as_deref() == Some(output_name))
            .map(|(o, _)| o.clone())?;
        let g = info.geometry.get(&output)?;
        Some((g.x, g.y, g.width, g.height))
    }

    /// Return the wayland output name for the first output the given
    /// toplevel currently sits on. Used by the GUI to place the
    /// recording border overlay on the right display when the user
    /// records a window. Returns `None` if the toplevel is unknown or
    /// none of its outputs report a name yet.
    pub fn output_name_for_toplevel(&self, identifier: &str) -> Option<String> {
        let toplevels = self.inner.toplevels.lock().unwrap();
        let info = toplevels.iter().find(|t| t.identifier == identifier)?;
        let infos = self.inner.output_infos.lock().unwrap();
        info.output
            .iter()
            .find_map(|o| infos.get(o).and_then(|i| i.name.clone()))
    }

    pub fn output_for_name(&self, name: &str) -> Option<wl_output::WlOutput> {
        self.inner
            .output_infos
            .lock()
            .unwrap()
            .iter()
            .find(|(_, info)| info.name.as_deref() == Some(name))
            .map(|(out, _)| out.clone())
    }

    fn set_output_info(&self, output: &wl_output::WlOutput, info_opt: Option<OutputInfo>) {
        let mut infos = self.inner.output_infos.lock().unwrap();
        match info_opt {
            Some(info) => {
                infos.insert(output.clone(), info);
            }
            None => {
                infos.remove(output);
            }
        }
    }

    /// Capture a single source (output or toplevel) and return the raw
    /// RGBA8 bytes. Returns `None` on screencopy failure (timeout,
    /// stopped, unsupported format) so the caller can degrade gracefully.
    pub async fn capture_source_shm(
        &self,
        source: CaptureSource,
        overlay_cursor: bool,
    ) -> Option<CapturedFrame> {
        let start = std::time::Instant::now();
        let label = match &source {
            CaptureSource::Output(_) => "output",
            CaptureSource::Toplevel(_) => "toplevel",
            CaptureSource::Workspace(_) => "workspace",
        };
        let session = self.create_session(source, overlay_cursor);
        let formats = session.wait_for_formats(|f| f.clone()).await?;
        let t_formats = start.elapsed();
        let (width, height) = formats.buffer_size;
        if width == 0 || height == 0 {
            tracing::warn!(width, height, "screencopy reported zero buffer size");
            return None;
        }
        if !formats
            .shm_formats
            .iter()
            .any(|f| *f == wl_shm::Format::Abgr8888)
        {
            tracing::warn!(
                advertised = ?formats.shm_formats,
                "screencopy lacks Abgr8888 — bailing"
            );
            return None;
        }

        let stride = width * 4;
        let total = (stride as usize) * (height as usize);
        let fd = create_memfd(total)?;

        // Create a transient wl_shm pool + buffer backed by the memfd.
        // The pool is destroyed immediately after the buffer is created;
        // wayland keeps the buffer alive until we destroy it, and the
        // dispatch thread reads the underlying memfd via mmap in `ready`.
        let pool = self.inner.wl_shm.create_pool(
            fd.as_fd(),
            total as i32,
            &self.inner.qh,
            (),
        );
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            wl_shm::Format::Abgr8888,
            &self.inner.qh,
            (),
        );
        pool.destroy();

        let (tx, rx) = oneshot::channel();
        session.0.capture_session.capture(
            &buffer,
            &[],
            &self.inner.qh,
            FrameData {
                frame_data: ScreencopyFrameData::default(),
                sender: Mutex::new(Some(tx)),
                shm_fd: Mutex::new(Some(fd)),
                width,
                height,
                stride,
            },
        );
        // Flush so the create-pool / create-buffer / capture requests
        // actually reach the compositor — the dispatch thread blocks on
        // read and won't auto-flush our outgoing queue. Without this,
        // every capture session times out at 3s without the compositor
        // ever knowing we asked.
        if let Err(e) = self.inner.conn.flush() {
            tracing::warn!(error = %e, "flush after screencopy capture failed");
        }

        // Bound the await — if the compositor never sends `ready` or
        // `failed` (toplevel closed mid-flight, capture restricted, etc.)
        // a stuck session would block the whole per-output stream. 3s is
        // generous; cosmic-comp normally completes a session in <50ms.
        let t_capture_sent = start.elapsed();
        let result = match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            rx,
        )
        .await
        {
            Ok(Ok(Ok(captured))) => Some(captured),
            Ok(Ok(Err(reason))) => {
                tracing::warn!(reason = ?reason, "screencopy failed");
                None
            }
            Ok(Err(_)) => {
                tracing::warn!("screencopy oneshot dropped (session stopped?)");
                None
            }
            Err(_) => {
                tracing::warn!("screencopy timed out after 3s");
                None
            }
        };
        buffer.destroy();
        let t_done = start.elapsed();
        tracing::info!(
            label,
            ok = result.is_some(),
            ms_to_formats = t_formats.as_millis() as u64,
            ms_to_capture_sent = t_capture_sent.as_millis() as u64,
            ms_total = t_done.as_millis() as u64,
            "capture_source_shm timing"
        );
        result
    }

    /// Capture a single source into a GBM-allocated dmabuf. Returns the
    /// `gbm::BufferObject` the compositor wrote into; the caller can map
    /// it (debug readback) or hand its fd to gst (recording path).
    ///
    /// Failure modes are surfaced as `Result::Err` so the caller can
    /// distinguish "no dmabuf support advertised" from "session timed
    /// out" — both matter for the recording path's diagnostics.
    pub async fn capture_source_dmabuf(
        &self,
        source: CaptureSource,
        overlay_cursor: bool,
        consumer_formats: Option<&[(u32, Vec<u64>)]>,
    ) -> Result<DmabufFrame> {
        let start = std::time::Instant::now();
        let label = match &source {
            CaptureSource::Output(_) => "output",
            CaptureSource::Toplevel(_) => "toplevel",
            CaptureSource::Workspace(_) => "workspace",
        };
        let session = self.create_session(source, overlay_cursor);
        let formats = session
            .wait_for_formats(|f| f.clone())
            .await
            .ok_or_else(|| anyhow::anyhow!("screencopy session stopped before formats"))?;
        let t_formats = start.elapsed();
        let (width, height) = formats.buffer_size;
        if width == 0 || height == 0 {
            anyhow::bail!("screencopy reported zero buffer size ({width}x{height})");
        }
        let dev_id = formats
            .dmabuf_device
            .ok_or_else(|| anyhow::anyhow!("screencopy advertised no dmabuf_device"))?;
        let (fourcc, modifiers) =
            pick_format(&formats.dmabuf_formats, consumer_formats).with_context(|| {
                format!("screencopy advertised no usable dmabuf format for {label}")
            })?;
        tracing::debug!(
            label,
            compositor_formats = ?formats.dmabuf_formats,
            ?consumer_formats,
            chosen_fourcc = ?fourcc,
            chosen_modifiers = ?modifiers,
            "dmabuf format negotiation"
        );

        let gbm = self.inner.gbm_registry.get_or_open(dev_id)?;

        // GBM allocation. `create_buffer_object_with_modifiers2` lets the
        // driver pick the best modifier from the list cosmic-comp gave
        // us — Mesa returns the highest-perf one its allocator + the
        // target consumer support, falling back to LINEAR when nothing
        // tiled is feasible. Held in a block so the mutex guard releases
        // before we await on the screencopy roundtrip.
        let bo = {
            let gbm_guard = gbm.lock().unwrap();
            let gbm_format = gbm::Format::try_from(fourcc as u32)
                .map_err(|e| anyhow::anyhow!("gbm rejected fourcc {:?}: {e:?}", fourcc))?;
            gbm_guard
                .device
                .create_buffer_object_with_modifiers2::<()>(
                    width,
                    height,
                    gbm_format,
                    modifiers.iter().copied().map(gbm::Modifier::from),
                    gbm::BufferObjectFlags::RENDERING,
                )
                .with_context(|| {
                    format!(
                        "gbm: create_buffer_object {width}x{height} \
                         fourcc={:?} modifiers={:#x?}",
                        fourcc, modifiers,
                    )
                })?
        };
        let modifier_u64: u64 = bo.modifier().into();
        let plane_count = bo.plane_count() as i32;

        // Wrap the BO as a wl_buffer via linux-dmabuf-v1. Lazy-bind the
        // `zwp_linux_dmabuf_v1` global on first dmabuf capture rather
        // than at GUI startup — eager binding triggered a Format/
        // Modifier event flood from cosmic-comp that stalled the
        // picker's first render. The proxy is cached for the rest of
        // the session after the first bind.
        let dmabuf_proxy = self.inner.ensure_linux_dmabuf()?;
        let params_proxy = dmabuf_proxy.create_params(&self.inner.qh, GlobalData);
        let modifier_hi = (modifier_u64 >> 32) as u32;
        let modifier_lo = (modifier_u64 & 0xffff_ffff) as u32;
        for plane in 0..plane_count {
            let fd = bo
                .fd_for_plane(plane)
                .map_err(|e| anyhow::anyhow!("bo.fd_for_plane({plane}): {e:?}"))?;
            params_proxy.add(
                fd.as_fd(),
                plane as u32,
                bo.offset(plane),
                bo.stride_for_plane(plane),
                modifier_hi,
                modifier_lo,
            );
        }
        let wl_buffer = params_proxy.create_immed(
            width as i32,
            height as i32,
            fourcc as u32,
            zwp_linux_buffer_params_v1::Flags::empty(),
            &self.inner.qh,
            DmaBufferData,
        );

        let (tx, rx) = oneshot::channel();
        session.0.capture_session.capture(
            &wl_buffer,
            &[],
            &self.inner.qh,
            DmabufFrameData {
                frame_data: ScreencopyFrameData::default(),
                sender: Mutex::new(Some(tx)),
            },
        );
        if let Err(e) = self.inner.conn.flush() {
            tracing::warn!(error = %e, "flush after dmabuf capture failed");
        }

        let t_capture_sent = start.elapsed();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), rx).await;
        // Tear down the wl_buffer + params proxy regardless of outcome.
        // The compositor already finished writing (success) or never wrote
        // (failure/timeout); either way the BO is ours, the wayland
        // resource is no longer needed.
        wl_buffer.destroy();
        params_proxy.destroy();
        let t_done = start.elapsed();

        let outcome = match result {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(reason))) => Err(anyhow::anyhow!("screencopy failed: {reason:?}")),
            Ok(Err(_)) => Err(anyhow::anyhow!(
                "screencopy oneshot dropped (session stopped?)"
            )),
            Err(_) => Err(anyhow::anyhow!("screencopy timed out after 3s")),
        };
        tracing::info!(
            label,
            ok = outcome.is_ok(),
            ms_to_formats = t_formats.as_millis() as u64,
            ms_to_capture_sent = t_capture_sent.as_millis() as u64,
            ms_total = t_done.as_millis() as u64,
            width,
            height,
            fourcc = ?fourcc,
            modifier = format!("{modifier_u64:#x}"),
            "capture_source_dmabuf timing"
        );
        outcome?;
        Ok(DmabufFrame {
            bo,
            fourcc,
            modifier: modifier_u64,
            width,
            height,
        })
    }

    /// Stream every toplevel that's visible on `output` (i.e. on its
    /// active workspace) as `(identifier, title, app_id, captured)`. Same
    /// shape as the portal's `capture_output_toplevels_shm` plus the
    /// metadata callers need to label the picker without a second lookup.
    pub fn capture_output_toplevels_shm<'a>(
        &'a self,
        output: &wl_output::WlOutput,
        overlay_cursor: bool,
    ) -> impl Stream<Item = WindowCapture> + 'a {
        let handles = self
            .inner
            .output_toplevels
            .lock()
            .unwrap()
            .get(output)
            .cloned()
            .unwrap_or_default();

        // Snapshot metadata up front so the futures don't all reach into
        // the toplevels mutex.
        let infos: Vec<ToplevelInfo> = self.inner.toplevels.lock().unwrap().clone();
        let meta_by_handle: HashMap<_, _> = infos
            .into_iter()
            .map(|i| (i.foreign_toplevel.clone(), (i.identifier, i.title, i.app_id)))
            .collect();

        handles
            .into_iter()
            .map(move |toplevel| {
                let helper = self.clone();
                let meta = meta_by_handle.get(&toplevel).cloned();
                async move {
                    let (identifier, title, app_id) = meta?;
                    let src = CaptureSource::Toplevel(toplevel.clone());
                    let frame = helper.capture_source_shm(src, overlay_cursor).await?;
                    Some(WindowCapture {
                        identifier,
                        title,
                        app_id,
                        frame,
                    })
                }
            })
            .collect::<FuturesOrdered<_>>()
            .filter_map(|x| async { x })
    }

    fn create_session(&self, source: CaptureSource, overlay_cursor: bool) -> Session {
        let session = Session(Arc::new_cyclic(|weak: &Weak<SessionInner>| {
            let opts = if overlay_cursor {
                CaptureOptions::PaintCursors
            } else {
                CaptureOptions::empty()
            };
            // cosmic-comp always supports this; .unwrap is the same shape
            // the portal uses.
            let capture_session = self
                .inner
                .capturer
                .create_session(
                    &source,
                    opts,
                    &self.inner.qh,
                    SessionData {
                        session: weak.clone(),
                        session_data: Default::default(),
                    },
                )
                .expect("cosmic-comp screencopy create_session");
            SessionInner {
                capture_session,
                state: Mutex::new(SessionState::default()),
            }
        }));
        // Flush so the create-session request reaches the compositor
        // before we await on `wait_for_formats`. Without this, the
        // dispatch thread sits on `read_events()` and the formats event
        // never arrives.
        if let Err(e) = self.inner.conn.flush() {
            tracing::warn!(error = %e, "flush after create_session failed");
        }
        session
    }
}

// ---- Dispatch impls / handlers ----

impl ProvidesRegistryState for AppData {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    smithay_client_toolkit::registry_handlers!(OutputState);
}

impl ShmHandler for AppData {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
    }
}

impl OutputHandler for AppData {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let info_opt = self.output_state.info(&output);
        self.helper.set_output_info(&output, info_opt);
        self.helper.inner.outputs.lock().unwrap().push(output);
        self.update_output_toplevels();
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let info_opt = self.output_state.info(&output);
        self.helper.set_output_info(&output, info_opt);
        self.update_output_toplevels();
    }
    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.helper.set_output_info(&output, None);
        let mut outputs = self.helper.inner.outputs.lock().unwrap();
        if let Some(idx) = outputs.iter().position(|x| x == &output) {
            outputs.remove(idx);
        }
        self.update_output_toplevels();
    }
}

impl WorkspaceHandler for AppData {
    fn workspace_state(&mut self) -> &mut WorkspaceState {
        &mut self.workspace_state
    }
    fn done(&mut self) {
        self.update_output_toplevels();
    }
}

impl ToplevelInfoHandler for AppData {
    fn toplevel_info_state(&mut self) -> &mut ToplevelInfoState {
        &mut self.toplevel_info_state
    }
    fn new_toplevel(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ExtForeignToplevelHandleV1,
    ) {
        self.update_output_toplevels();
    }
    fn update_toplevel(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ExtForeignToplevelHandleV1,
    ) {
        self.update_output_toplevels();
    }
    fn toplevel_closed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ExtForeignToplevelHandleV1,
    ) {
        self.update_output_toplevels();
    }
    fn info_done(&mut self, _: &Connection, _: &QueueHandle<Self>) {
        // Refresh once more — the per-toplevel events that arrived in
        // this batch may have only landed *after* the previous
        // update_output_toplevels call (events fire as they're parsed,
        // not all-at-once before `Done`).
        self.update_output_toplevels();
        self.helper.inner.info_done.store(true, Ordering::Release);
        self.helper.inner.info_done_notify.notify_waiters();
    }
}

impl ScreencopyHandler for AppData {
    fn screencopy_state(&mut self) -> &mut ScreencopyState {
        &mut self.screencopy_state
    }

    fn init_done(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        session: &CaptureSession,
        formats: &Formats,
    ) {
        if let Some(session) = Session::for_session(session) {
            session.update(|s| {
                s.formats = Some(formats.clone());
            });
        }
    }

    fn stopped(&mut self, _: &Connection, _: &QueueHandle<Self>, session: &CaptureSession) {
        if let Some(session) = Session::for_session(session) {
            session.update(|s| {
                s.stopped = true;
            });
        }
    }

    fn ready(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        capture_frame: &CaptureFrame,
        _frame: Frame,
    ) {
        if let Some(data) = capture_frame.data::<FrameData>() {
            let Some(fd) = data.shm_fd.lock().unwrap().take() else {
                return;
            };
            let pixels = match unsafe { memmap2::Mmap::map(&fd) } {
                Ok(map) => map.to_vec(),
                Err(e) => {
                    tracing::warn!(error = %e, "mmap shm fd failed");
                    return;
                }
            };
            let captured = CapturedFrame {
                pixels,
                width: data.width,
                height: data.height,
                stride: data.stride,
            };
            if let Some(tx) = data.sender.lock().unwrap().take() {
                let _ = tx.send(Ok(captured));
            }
            return;
        }
        if let Some(data) = capture_frame.data::<DmabufFrameData>() {
            // Dmabuf path: the compositor has finished writing into our
            // pre-allocated GBM BO. Nothing to read out on this side —
            // just signal the outer task.
            if let Some(tx) = data.sender.lock().unwrap().take() {
                let _ = tx.send(Ok(()));
            }
        }
    }

    fn failed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        capture_frame: &CaptureFrame,
        reason: WEnum<FailureReason>,
    ) {
        if let Some(data) = capture_frame.data::<FrameData>() {
            if let Some(tx) = data.sender.lock().unwrap().take() {
                let _ = tx.send(Err(reason));
            }
            return;
        }
        if let Some(data) = capture_frame.data::<DmabufFrameData>() {
            if let Some(tx) = data.sender.lock().unwrap().take() {
                let _ = tx.send(Err(reason));
            }
        }
    }
}

// Manual no-op dispatch impls for the linux-dmabuf protocol types we
// use. We don't go through `DmabufState`/`delegate_dmabuf` because that
// would bind the global eagerly in `WaylandHelper::new` (see comment
// above the bind call). The events we'd receive — Format/Modifier
// announcements, params Created/Failed, wl_buffer Release — are
// either ignored (we drive allocation off cosmic-screencopy's
// per-session formats, use `create_immed`, and destroy buffers right
// after the capture roundtrip) or fire only for protocol versions we
// don't request. No-op handlers are safe and keep the dispatch thread
// free of the dmabuf event flood until a recording actually starts.

impl wayland_client::Dispatch<ZwpLinuxDmabufV1, smithay_client_toolkit::globals::GlobalData>
    for AppData
{
    fn event(
        _: &mut Self,
        _: &ZwpLinuxDmabufV1,
        _: <ZwpLinuxDmabufV1 as wayland_client::Proxy>::Event,
        _: &smithay_client_toolkit::globals::GlobalData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl
    wayland_client::Dispatch<
        zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        smithay_client_toolkit::globals::GlobalData,
    > for AppData
{
    fn event(
        _: &mut Self,
        _: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        _: <zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1 as wayland_client::Proxy>::Event,
        _: &smithay_client_toolkit::globals::GlobalData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl wayland_client::Dispatch<wl_buffer::WlBuffer, DmaBufferData> for AppData {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        _: <wl_buffer::WlBuffer as wayland_client::Proxy>::Event,
        _: &DmaBufferData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl SeatHandler for AppData {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.helper.inner.seats.lock().unwrap().push(seat);
    }
    fn new_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _: Capability,
    ) {
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _: Capability,
    ) {
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.helper.inner.seats.lock().unwrap().retain(|s| s != &seat);
    }
}

impl ToplevelManagerHandler for AppData {
    fn toplevel_manager_state(&mut self) -> &mut ToplevelManagerState {
        // Safe because we only get capability events when the global
        // bound successfully; cosmic-client-toolkit only routes events
        // here through the delegate macro which requires the global
        // to exist. If we ever invoke this with the `None` branch, it
        // means the protocol vanished mid-session — fail loud.
        self.toplevel_manager_state
            .as_mut()
            .expect("toplevel_manager_state delegated without binding")
    }
    fn capabilities(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _capabilities: Vec<
            wayland_client::WEnum<
                zcosmic_toplevel_manager_v1::ZcosmicToplelevelManagementCapabilitiesV1,
            >,
        >,
    ) {
    }
}

smithay_client_toolkit::delegate_output!(AppData);
smithay_client_toolkit::delegate_registry!(AppData);
smithay_client_toolkit::delegate_seat!(AppData);
smithay_client_toolkit::delegate_shm!(AppData);
cosmic_client_toolkit::delegate_screencopy!(AppData);
cosmic_client_toolkit::delegate_toplevel_info!(AppData);
cosmic_client_toolkit::delegate_toplevel_manager!(AppData);
cosmic_client_toolkit::delegate_workspace!(AppData);
delegate_noop!(AppData: ignore wl_buffer::WlBuffer);
delegate_noop!(AppData: ignore wl_shm_pool::WlShmPool);

/// Create a memfd large enough to back an `Abgr8888` buffer of `size`
/// bytes. Direct port of `xdg-desktop-portal-cosmic`'s
/// `buffer::create_memfd`, simplified for our single-format path.
fn create_memfd(size: usize) -> Option<OwnedFd> {
    let name = c"cosmic-capture-screencopy";
    let fd = rustix::fs::memfd_create(name, rustix::fs::MemfdFlags::CLOEXEC)
        .map_err(|e| tracing::warn!(error = %e, "memfd_create failed"))
        .ok()?;
    rustix::fs::ftruncate(&fd, size as u64)
        .map_err(|e| tracing::warn!(error = %e, "ftruncate failed"))
        .ok()?;
    Some(fd)
}
