//! Persistent wayland helper. Mirrors `xdg-desktop-portal-cosmic`'s
//! `WaylandHelper`: one long-lived `Connection`, a background event-pump
//! thread, and live `output_toplevels` / `output_infos` / `toplevels`
//! state. Captures are async sessions on the same connection — no fresh
//! `connect_to_env` per request.
//!
//! Only the screencopy + shm path is implemented; dmabuf is left out
//! (cosmic-comp's screencopy advertises Abgr8888 shm, which is what we
//! need for png/gst).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;

use anyhow::{Context, Result};
use tokio::sync::Notify;
use cosmic_client_toolkit::{
    screencopy::{
        CaptureFrame, CaptureOptions, CaptureSession, CaptureSource, Capturer, FailureReason,
        Formats, Frame, ScreencopyFrameData, ScreencopyFrameDataExt, ScreencopyHandler,
        ScreencopySessionData, ScreencopySessionDataExt, ScreencopyState,
    },
    toplevel_info::{ToplevelInfo, ToplevelInfoHandler, ToplevelInfoState},
    workspace::{WorkspaceHandler, WorkspaceState},
};
use futures_util::stream::{FuturesOrdered, Stream, StreamExt};
use tokio::sync::oneshot;
use smithay_client_toolkit::{
    output::{OutputHandler, OutputInfo, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    shm::{Shm, ShmHandler},
};
use std::os::fd::{AsFd, OwnedFd};
use wayland_client::{
    Connection, QueueHandle, WEnum, delegate_noop,
    globals::registry_queue_init,
    protocol::{wl_buffer, wl_output, wl_shm, wl_shm_pool},
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1;
use wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1;

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
}

struct AppData {
    helper: WaylandHelper,
    registry_state: RegistryState,
    output_state: OutputState,
    shm_state: Shm,
    screencopy_state: ScreencopyState,
    workspace_state: WorkspaceState,
    toplevel_info_state: ToplevelInfoState,
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

        let registry_state = RegistryState::new(&globals);
        let screencopy_state = ScreencopyState::new(&globals, &qh);
        let shm_state = Shm::bind(&globals, &qh).context("bind wl_shm")?;

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
            }),
        };

        let mut data = AppData {
            helper: helper.clone(),
            output_state: OutputState::new(&globals, &qh),
            shm_state,
            screencopy_state,
            workspace_state: WorkspaceState::new(&registry_state, &qh),
            toplevel_info_state: ToplevelInfoState::new(&registry_state, &qh),
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
        let Some(data) = capture_frame.data::<FrameData>() else {
            return;
        };
        let Some(fd) = data.shm_fd.lock().unwrap().take() else {
            return;
        };
        // mmap the memfd to read pixels into an owned Vec. Once the Vec
        // is built we drop both the mmap and the fd; wayland holds the
        // wl_buffer alive separately and we destroy it after the await.
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
        }
    }
}

smithay_client_toolkit::delegate_output!(AppData);
smithay_client_toolkit::delegate_registry!(AppData);
smithay_client_toolkit::delegate_shm!(AppData);
cosmic_client_toolkit::delegate_screencopy!(AppData);
cosmic_client_toolkit::delegate_toplevel_info!(AppData);
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
