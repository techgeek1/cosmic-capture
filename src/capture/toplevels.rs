//! Per-window enumeration + capture via cosmic-protocols' toplevel_info.
//!
//! Each call (`list()`, `capture()`) opens its own short-lived wayland
//! connection: it enumerates all toplevels, performs the requested action,
//! and tears the connection down. `ExtForeignToplevelHandleV1` instances
//! can't be shared across connections, but each toplevel ships a stable
//! `identifier` string (from `ext_foreign_toplevel_list_v1`), so we round-
//! trip selections through that.
//!
//! This is racy — a window can close between `list()` and `capture()` — but
//! that race is benign: the capture call surfaces a clear "no such toplevel"
//! error and the GUI re-lists on retry.
//!
//! NOTE: no thumbnails yet. We deliver titles + app_ids; a thumbnail pass
//! (per-toplevel screencopy at a small size) is a follow-up.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use cosmic_client_toolkit::{
    screencopy::{
        CaptureFrame, CaptureOptions, CaptureSession, CaptureSource, FailureReason, Formats,
        ScreencopyFrameData, ScreencopyFrameDataExt, ScreencopyHandler, ScreencopySessionData,
        ScreencopySessionDataExt, ScreencopyState,
    },
    toplevel_info::{ToplevelInfoHandler, ToplevelInfoState},
    workspace::{WorkspaceHandler, WorkspaceState},
};
use smithay_client_toolkit::{
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    shm::{Shm, ShmHandler, raw::RawPool},
};
use wayland_client::{
    Connection, QueueHandle, WEnum, delegate_noop,
    globals::registry_queue_init,
    protocol::{wl_buffer, wl_output, wl_shm},
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1;

use super::screencopy::CapturedFrame;

/// User-facing snapshot of a single toplevel.
#[derive(Clone, Debug)]
pub struct ToplevelSummary {
    pub identifier: String,
    pub title: String,
    pub app_id: String,
    /// Output names the toplevel currently sits on (from cosmic-toplevel-
    /// info's `output_enter` / `output_leave` events). Used by the GUI to
    /// filter the window picker per output — xdg-desktop-portal-cosmic
    /// only shows a window on the picker of the monitor it's actually on,
    /// and we match that behaviour.
    pub outputs: Vec<String>,
}

/// Enumerate all current toplevels. Blocking — run via `spawn_blocking`.
///
/// The wayland protocol delivers toplevel state across several events
/// (`identifier`, `title`, `app_id`, `done`, plus per-toplevel `done`),
/// possibly spread over multiple roundtrips. We dispatch up to ~500ms,
/// breaking early as soon as `info_done` fires (cosmic-protocol v3+ signal
/// for "initial batch complete").
pub fn list() -> Result<Vec<ToplevelSummary>> {
    let conn = Connection::connect_to_env().context("connect wayland")?;
    let (globals, mut q) = registry_queue_init::<EnumData>(&conn).context("registry_queue_init")?;
    let qh = q.handle();

    let registry_state = RegistryState::new(&globals);
    // Output and Workspace must exist before ToplevelInfo — toplevel_info
    // events reference workspace handles and output handles, so wayland-
    // client needs Dispatch impls for those types before processing toplevel
    // events that create them.
    let mut data = EnumData {
        output_state: OutputState::new(&globals, &qh),
        workspace_state: WorkspaceState::new(&registry_state, &qh),
        toplevel_info_state: ToplevelInfoState::new(&registry_state, &qh),
        registry_state,
        info_done: false,
    };

    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_millis(500);
    while !data.info_done && Instant::now() < deadline {
        q.roundtrip(&mut data).context("roundtrip for toplevels")?;
        if !data.info_done {
            // Give the compositor a moment to emit follow-up events
            // (title/app_id arrive separately from the `toplevel` event).
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    let mut out: Vec<ToplevelSummary> = data
        .toplevel_info_state
        .toplevels()
        .map(|info| {
            let outputs: Vec<String> = info
                .output
                .iter()
                .filter_map(|o| data.output_state.info(o).and_then(|i| i.name))
                .collect();
            ToplevelSummary {
                identifier: info.identifier.clone(),
                title: info.title.clone(),
                app_id: info.app_id.clone(),
                outputs,
            }
        })
        .collect();
    out.sort_by(|a, b| a.title.cmp(&b.title));
    tracing::debug!(count = out.len(), info_done = data.info_done, "toplevel enumeration complete");
    Ok(out)
}

/// Capture a single toplevel by its stable `identifier`. Blocking — run via
/// `spawn_blocking`.
pub fn capture(identifier: &str, with_cursor: bool) -> Result<CapturedFrame> {
    let conn = Connection::connect_to_env().context("connect wayland")?;
    let (globals, mut q) = registry_queue_init::<AppData>(&conn).context("registry_queue_init")?;
    let qh = q.handle();

    let result_slot: Arc<Mutex<Option<Result<CapturedFrame, String>>>> =
        Arc::new(Mutex::new(None));
    let registry_state = RegistryState::new(&globals);
    let mut data = AppData {
        output_state: OutputState::new(&globals, &qh),
        workspace_state: WorkspaceState::new(&registry_state, &qh),
        toplevel_info_state: ToplevelInfoState::new(&registry_state, &qh),
        shm_state: Shm::bind(&globals, &qh).context("bind wl_shm")?,
        screencopy_state: ScreencopyState::new(&globals, &qh),
        registry_state,
        result: result_slot.clone(),
        info_done: false,
    };

    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_millis(500);
    while !data.info_done && Instant::now() < deadline {
        q.roundtrip(&mut data).context("roundtrip for toplevels")?;
        if !data.info_done {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    let handle = data
        .toplevel_info_state
        .toplevels()
        .find(|i| i.identifier == identifier)
        .map(|i| i.foreign_toplevel.clone())
        .ok_or_else(|| anyhow!("no toplevel with identifier {identifier:?}"))?;

    let opts = if with_cursor {
        CaptureOptions::PaintCursors
    } else {
        CaptureOptions::empty()
    };

    let _session = data
        .screencopy_state
        .capturer()
        .create_session(
            &CaptureSource::Toplevel(handle),
            opts,
            &qh,
            SessionData {
                session_data: ScreencopySessionData::default(),
            },
        )
        .map_err(|e| anyhow!("screencopy create_session: {e:?}"))?;

    // Bound the screencopy roundtrip so a toplevel that the compositor
    // never responds for (closed mid-flight, restricted, unmapped) can't
    // wedge the picker for the rest of the session. cosmic-comp normally
    // satisfies a Toplevel capture in <50ms; 2s is generous and still
    // bounded.
    let cap_deadline = Instant::now() + Duration::from_secs(2);
    while result_slot.lock().unwrap().is_none() {
        if Instant::now() >= cap_deadline {
            bail!("screencopy timed out for toplevel {identifier:?}");
        }
        q.blocking_dispatch(&mut data)
            .context("wayland blocking_dispatch")?;
    }
    let outcome = result_slot.lock().unwrap().take().unwrap();
    outcome.map_err(|e| anyhow!(e))
}

// ---- enumeration-only handlers ----

struct EnumData {
    output_state: OutputState,
    registry_state: RegistryState,
    workspace_state: WorkspaceState,
    toplevel_info_state: ToplevelInfoState,
    info_done: bool,
}

impl WorkspaceHandler for EnumData {
    fn workspace_state(&mut self) -> &mut WorkspaceState {
        &mut self.workspace_state
    }
    fn done(&mut self) {}
}

impl ProvidesRegistryState for EnumData {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    smithay_client_toolkit::registry_handlers!(OutputState);
}

impl OutputHandler for EnumData {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ToplevelInfoHandler for EnumData {
    fn toplevel_info_state(&mut self) -> &mut ToplevelInfoState {
        &mut self.toplevel_info_state
    }
    fn new_toplevel(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
    }
    fn update_toplevel(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
    }
    fn toplevel_closed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
    }
    fn info_done(&mut self, _: &Connection, _: &QueueHandle<Self>) {
        self.info_done = true;
    }
}

smithay_client_toolkit::delegate_output!(EnumData);
smithay_client_toolkit::delegate_registry!(EnumData);
cosmic_client_toolkit::delegate_toplevel_info!(EnumData);
cosmic_client_toolkit::delegate_workspace!(EnumData);

// ---- enumeration + screencopy handlers ----

struct AppData {
    output_state: OutputState,
    registry_state: RegistryState,
    workspace_state: WorkspaceState,
    toplevel_info_state: ToplevelInfoState,
    shm_state: Shm,
    screencopy_state: ScreencopyState,
    result: Arc<Mutex<Option<Result<CapturedFrame, String>>>>,
    info_done: bool,
}

impl WorkspaceHandler for AppData {
    fn workspace_state(&mut self) -> &mut WorkspaceState {
        &mut self.workspace_state
    }
    fn done(&mut self) {}
}

impl ProvidesRegistryState for AppData {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    smithay_client_toolkit::registry_handlers!(OutputState);
}

impl OutputHandler for AppData {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for AppData {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
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
        _: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
    }
    fn update_toplevel(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
    }
    fn toplevel_closed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
    }
    fn info_done(&mut self, _: &Connection, _: &QueueHandle<Self>) {
        self.info_done = true;
    }
}

impl ScreencopyHandler for AppData {
    fn screencopy_state(&mut self) -> &mut ScreencopyState {
        &mut self.screencopy_state
    }

    fn init_done(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        session: &CaptureSession,
        formats: &Formats,
    ) {
        let (width, height) = formats.buffer_size;
        if width == 0 || height == 0 {
            *self.result.lock().unwrap() =
                Some(Err(format!("toplevel reported zero buffer size {width}x{height}")));
            return;
        }
        let stride = (width as i32) * 4;
        let pool = match RawPool::new((width as usize) * (height as usize) * 4, &self.shm_state) {
            Ok(p) => p,
            Err(e) => {
                *self.result.lock().unwrap() = Some(Err(format!("alloc shm pool: {e}")));
                return;
            }
        };
        if !formats
            .shm_formats
            .iter()
            .any(|f| *f == wl_shm::Format::Abgr8888)
        {
            *self.result.lock().unwrap() = Some(Err(format!(
                "compositor doesn't advertise Abgr8888 for toplevel; supported = {:?}",
                formats.shm_formats
            )));
            return;
        }
        let pool = Mutex::new(pool);
        let buf = pool.lock().unwrap().create_buffer(
            0,
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Abgr8888,
            (),
            qh,
        );
        session.capture(
            &buf,
            &[],
            qh,
            FrameData {
                frame_data: ScreencopyFrameData::default(),
                pool,
                width,
                height,
                stride: stride as u32,
            },
        );
    }

    fn stopped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &CaptureSession) {}

    fn ready(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        capture_frame: &CaptureFrame,
        _: cosmic_client_toolkit::screencopy::Frame,
    ) {
        let fd = capture_frame.data::<FrameData>().unwrap();
        let mut pool = fd.pool.lock().unwrap();
        let bytes = pool.mmap();
        let mut pixels = vec![0u8; bytes.len()];
        pixels.copy_from_slice(bytes);
        *self.result.lock().unwrap() = Some(Ok(CapturedFrame {
            pixels,
            width: fd.width,
            height: fd.height,
            stride: fd.stride,
        }));
    }

    fn failed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &CaptureFrame,
        reason: WEnum<FailureReason>,
    ) {
        *self.result.lock().unwrap() =
            Some(Err(format!("screencopy failed: {reason:?}")));
    }
}

struct SessionData {
    session_data: ScreencopySessionData,
}
impl ScreencopySessionDataExt for SessionData {
    fn screencopy_session_data(&self) -> &ScreencopySessionData {
        &self.session_data
    }
}

struct FrameData {
    frame_data: ScreencopyFrameData,
    pool: Mutex<RawPool>,
    width: u32,
    height: u32,
    stride: u32,
}
impl ScreencopyFrameDataExt for FrameData {
    fn screencopy_frame_data(&self) -> &ScreencopyFrameData {
        &self.frame_data
    }
}

smithay_client_toolkit::delegate_output!(AppData);
smithay_client_toolkit::delegate_registry!(AppData);
smithay_client_toolkit::delegate_shm!(AppData);
cosmic_client_toolkit::delegate_toplevel_info!(AppData);
cosmic_client_toolkit::delegate_workspace!(AppData);
cosmic_client_toolkit::delegate_screencopy!(AppData);
delegate_noop!(AppData: ignore wl_buffer::WlBuffer);

// Compiler hint that `bail!` is exercised somewhere — keeps the import on a
// possible future code path without disabling lints.
#[allow(dead_code)]
fn _bail_marker() -> Result<()> {
    bail!("never called");
}
