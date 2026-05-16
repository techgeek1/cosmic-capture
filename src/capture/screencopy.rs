//! Direct wlr-screencopy capture using cosmic-client-toolkit.
//!
//! Replaces the old xdg-desktop-portal Screenshot delegation. We open our
//! own short-lived wayland connection, ask the compositor for a screencopy
//! session on the target output, allocate an shm buffer, and read out the
//! raw pixels — no second process, no portal UI, no temp file.
//!
//! Returns RGBA8 pixels in row-major order. The caller is responsible for
//! cropping (we hand back the full output frame) and for encoding.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use cosmic_client_toolkit::screencopy::{
    CaptureFrame, CaptureOptions, CaptureSession, CaptureSource, FailureReason, Formats,
    ScreencopyFrameData, ScreencopyFrameDataExt, ScreencopyHandler, ScreencopySessionData,
    ScreencopySessionDataExt, ScreencopyState,
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

/// Captured frame in compositor-native bytes (interpreted as RGBA8 after
/// the format swap we perform in `ready`). `stride` may exceed `width * 4`
/// if the compositor padded rows, so consumers should iterate row by row.
pub struct CapturedFrame {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

/// Which output to capture. Either a specific output name (from
/// `xdg_output.name` / `wl_output.name`) or whichever output the caller's
/// region center happens to sit on (caller resolves that themselves and
/// passes the name).
pub enum Target {
    /// Capture the named output. The output is found by `wl_output.name`
    /// (e.g. "DP-1", "eDP-1") — same value libcosmic surfaces via
    /// `OutputInfo.name`.
    OutputName(String),
}

/// Capture a single frame from the requested output.
///
/// This is a blocking operation but typically completes in <50ms. Run it
/// off the main UI thread (e.g. `tokio::task::spawn_blocking`).
pub fn capture(target: Target, with_cursor: bool) -> Result<CapturedFrame> {
    let conn = Connection::connect_to_env().context("connect to wayland")?;
    let (globals, mut event_queue) =
        registry_queue_init::<AppData>(&conn).context("registry_queue_init")?;
    let qh = event_queue.handle();

    let registry_state = RegistryState::new(&globals);
    let shm_state = Shm::bind(&globals, &qh).context("bind wl_shm")?;
    let screencopy_state = ScreencopyState::new(&globals, &qh);
    let output_state = OutputState::new(&globals, &qh);

    let result_slot: Arc<Mutex<Option<Result<CapturedFrame, String>>>> =
        Arc::new(Mutex::new(None));

    let mut data = AppData {
        output_state,
        shm_state,
        registry_state,
        screencopy_state,
        result: result_slot.clone(),
    };

    // Drain output advertisements + screencopy formats.
    event_queue
        .roundtrip(&mut data)
        .context("initial wayland roundtrip")?;

    let Target::OutputName(want) = target;
    let output = data
        .output_state
        .outputs()
        .find(|o| {
            data.output_state
                .info(o)
                .and_then(|i| i.name)
                .as_deref()
                == Some(want.as_str())
        })
        .ok_or_else(|| anyhow!("no output matches name {want:?}"))?;

    let opts = if with_cursor {
        CaptureOptions::PaintCursors
    } else {
        CaptureOptions::empty()
    };

    let _session = data
        .screencopy_state
        .capturer()
        .create_session(
            &CaptureSource::Output(output),
            opts,
            &qh,
            SessionData {
                session_data: ScreencopySessionData::default(),
            },
        )
        .map_err(|e| anyhow!("screencopy create_session: {e:?}"))?;

    // Pump events until our handler fills `result_slot` (success or failure).
    while result_slot.lock().unwrap().is_none() {
        event_queue
            .blocking_dispatch(&mut data)
            .context("wayland blocking_dispatch")?;
    }

    let outcome = result_slot.lock().unwrap().take().unwrap();
    outcome.map_err(|e| anyhow!(e))
}

struct AppData {
    shm_state: Shm,
    registry_state: RegistryState,
    output_state: OutputState,
    screencopy_state: ScreencopyState,
    result: Arc<Mutex<Option<Result<CapturedFrame, String>>>>,
}

impl ProvidesRegistryState for AppData {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    smithay_client_toolkit::registry_handlers!();
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
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
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
        let stride = (width as i32) * 4;
        let pool = match RawPool::new((width as usize) * (height as usize) * 4, &self.shm_state) {
            Ok(p) => p,
            Err(e) => {
                *self.result.lock().unwrap() = Some(Err(format!("alloc shm pool: {e}")));
                return;
            }
        };
        // Argb8888 / Xrgb8888 / Abgr8888 — pick one supported by the
        // compositor. Cosmic-comp advertises Abgr8888 in our checkouts; we
        // swap channels into Rgba8 at readout time below.
        if !formats
            .shm_formats
            .iter()
            .any(|f| *f == wl_shm::Format::Abgr8888)
        {
            *self.result.lock().unwrap() = Some(Err(format!(
                "compositor doesn't advertise Abgr8888; supported = {:?}",
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
        // Abgr8888 little-endian == bytes [R, G, B, A] in memory, which is
        // already the RGBA8 layout the png crate expects. No swap needed.
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
cosmic_client_toolkit::delegate_screencopy!(AppData);
delegate_noop!(AppData: ignore wl_buffer::WlBuffer);

/// Crop an RGBA frame in place, returning a fresh buffer of `cw*ch*4` bytes.
/// Caller guarantees the rect is fully inside `frame`'s bounds.
pub fn crop_rgba(frame: &CapturedFrame, cx: u32, cy: u32, cw: u32, ch: u32) -> Result<Vec<u8>> {
    if cx + cw > frame.width || cy + ch > frame.height {
        bail!(
            "crop {cx},{cy} {cw}x{ch} doesn't fit in frame {}x{}",
            frame.width, frame.height
        );
    }
    let mut out = Vec::with_capacity((cw as usize) * (ch as usize) * 4);
    for row in 0..ch {
        let y = cy + row;
        let row_start = (y as usize) * (frame.stride as usize) + (cx as usize) * 4;
        let row_end = row_start + (cw as usize) * 4;
        out.extend_from_slice(&frame.pixels[row_start..row_end]);
    }
    Ok(out)
}
