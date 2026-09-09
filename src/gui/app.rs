//! libcosmic floating toolbar + per-output selector overlays.
//!
//! Architecture:
//!  * **No xdg toplevel.** We run with `Settings::no_main_window(true)` and
//!    create our control surface as a wlr-layer-shell surface anchored to the
//!    bottom-center of the active output (`Layer::Overlay`, `OnDemand`
//!    keyboard, zero exclusive zone). This mirrors how
//!    `xdg-desktop-portal-cosmic`'s screenshot UI works and matches the
//!    floating-pill look the user wanted.
//!  * **Selector overlays** are full-output `Layer::Overlay` surfaces with
//!    `Exclusive` keyboard, opened on demand (one per `WlOutput`). They draw
//!    the dim overlay + selection rect via the `RectangleSelection` widget.
//!  * **Dispatch.** `view()` is never called (no main window). `view_window`
//!    routes by `window::Id`: the toolbar id renders the floating pill;
//!    selector ids render the rectangle selector for the matching output.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use cosmic::app::{Core, Settings, Task};
use cosmic::iced::core::event::wayland::OutputEvent;
use cosmic::iced::keyboard::Key;
use cosmic::iced::keyboard::key::Named;
use cosmic::iced::platform_specific::shell::commands::layer_surface::{
    Anchor, KeyboardInteractivity, Layer, destroy_layer_surface, get_layer_surface,
};
use cosmic::iced::platform_specific::shell::wayland::subsurface_widget::{
    BufferSource, Shmbuf, Subsurface, SubsurfaceBuffer,
};
use cosmic::iced::runtime::platform_specific::wayland::layer_surface::{
    IcedMargin, IcedOutput, SctkLayerSurfaceSettings,
};
use cosmic::iced::{
    self, Background, Border, Color, Length, Limits, Subscription, event, keyboard, mouse,
    window,
};
use cosmic::widget::{button, container, dropdown, icon, row};
use cosmic::{Application, Element, executor};
use tokio::sync::oneshot;
use wayland_client::protocol::wl_output::WlOutput;

use crate::capture::screencopy::CapturedFrame;
use crate::capture::toplevel_capture;
use crate::capture::wayland::{WaylandHelper, WindowCapture};
use crate::cli::{CommonArgs, GifArgs, RecordArgs, VideoContainer, VideoEncoder};
use crate::encode::video::CropRect;
use crate::pipeline;
use crate::pipeline::record::ScreencopyTarget;

use super::widget::streaming_thumb;
use super::widget::{
    DragSession, IntrinsicShader, RawFrame, RectMode, RectangleSelection, SelectionEvent,
    SelectionRect, SharedFrame,
};

const APP_ID: &str = "com.system76.CosmicCapture";
const TOOLBAR_BOTTOM_MARGIN: u16 = 32;
/// Shared id for the synthetic DnD operation our selection widgets use to
/// track cursor motion across per-output layer surfaces. Constant because
/// only one selection drag exists at a time; collisions with real DnD ops
/// are avoided by the matching `DND_MIME` filter on incoming events.
const SELECTION_DND_ID: u128 = 0x4341_5054_5552_452D_5345_4C45_4354_494F;

/// Single-instance guard. Holds an advisory `flock` on a file under
/// `$XDG_RUNTIME_DIR` for the life of the process; a second launch
/// (e.g. the user mashing the capture hotkey while the toolbar is up)
/// sees the lock held and exits instead of opening a second set of
/// overlays on top of the first. The kernel drops the lock when the
/// process exits, however it exits, so a crash can't wedge future
/// launches. The `File` is opened `O_CLOEXEC` (Rust's default), so the
/// re-exec'd `__clipboard_serve` child never inherits the lock.
struct InstanceLock {
    _file: std::fs::File,
}

impl InstanceLock {
    fn try_acquire() -> Result<Option<Self>> {
        use anyhow::Context as _;
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = dir.join("cosmic-capture.lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open instance lock {}", path.display()))?;
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("flock {}: {e}", path.display())),
        }
    }
}

/// Every icon the toolbar can show, for `init`'s lookup prewarm.
const TOOLBAR_ICONS: &[&str] = &[
    "camera-photo-symbolic",
    "camera-video-symbolic",
    "screenshot-selection-symbolic",
    "screenshot-screen-symbolic",
    "screenshot-window-symbolic",
    "audio-input-microphone-symbolic",
    "audio-speakers-symbolic",
    "window-close-symbolic",
];

pub fn launch() -> Result<()> {
    let _lock = match InstanceLock::try_acquire()? {
        Some(lock) => lock,
        None => {
            tracing::info!("another cosmic-capture instance is already running; exiting");
            return Ok(());
        }
    };
    tracing::info!("cosmic-capture GUI starting (layer-shell toolbar)");
    // iced builds its font database lazily on the render thread, at
    // the first toolbar's first frame: it mmaps and parses every
    // system font (~20ms warm, ~600ms on a cold page cache). Kick it
    // off now so it overlaps wgpu init and the freeze captures instead
    // of sitting on the first frame's critical path.
    std::thread::Builder::new()
        .name("font-db-warm".into())
        .spawn(|| {
            let _ = cosmic::iced::advanced::graphics::text::font_system();
        })
        .ok();
    let settings = Settings::default()
        .no_main_window(true)
        .exit_on_close(false)
        .transparent(true);
    // WaylandHelper is created inside `Application::init` — direct port
    // of xdg-desktop-portal-cosmic's `CosmicPortal::init`. Creating it
    // *before* `cosmic::app::run` was a divergence from the canonical
    // reference and made the picker take ~10s to respond on the first
    // Window-source click. The portal opens its wayland connection only
    // after iced's main loop is up; we now do the same.
    let result = cosmic::app::run::<Panel>(settings, ())
        .map_err(|e| anyhow::anyhow!("libcosmic exited with error: {e}"));
    tracing::info!(?result, "GUI exited");
    result
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum Mode {
    #[default]
    Screenshot,
    Record,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum Source {
    #[default]
    Region,
    /// Per-window capture — not yet wired through the screencast pipeline.
    Window,
    Screen,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum SaveTarget {
    #[default]
    Clipboard,
    Pictures,
    Documents,
}

// Clipboard first — typical screenshot flow is "grab and paste somewhere",
// so we put it at the top of the dropdown and make it the default.
const SAVE_TARGETS: [SaveTarget; 3] = [
    SaveTarget::Clipboard,
    SaveTarget::Pictures,
    SaveTarget::Documents,
];

fn save_target_labels() -> Vec<String> {
    SAVE_TARGETS
        .iter()
        .map(|t| match t {
            SaveTarget::Pictures => "Save to Pictures",
            SaveTarget::Documents => "Save to Documents",
            SaveTarget::Clipboard => "Save to Clipboard",
        }
        .to_string())
        .collect()
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum RecordFormat {
    #[default]
    Mp4,
    Mkv,
    WebM,
    Gif,
}

const RECORD_FORMATS: [RecordFormat; 4] = [
    RecordFormat::Mp4,
    RecordFormat::Mkv,
    RecordFormat::WebM,
    RecordFormat::Gif,
];

fn record_format_labels() -> Vec<String> {
    RECORD_FORMATS
        .iter()
        .map(|f| match f {
            RecordFormat::Mp4 => "MP4",
            RecordFormat::Mkv => "MKV",
            RecordFormat::WebM => "WebM",
            RecordFormat::Gif => "GIF",
        }
        .to_string())
        .collect()
}

impl RecordFormat {
    /// The wayland clipboard mime type to advertise when copying a finished
    /// recording so paste targets get an actual media payload.
    pub fn mime(self) -> &'static str {
        match self {
            RecordFormat::Mp4 => "video/mp4",
            RecordFormat::Mkv => "video/x-matroska",
            RecordFormat::WebM => "video/webm",
            RecordFormat::Gif => "image/gif",
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum RecordFps {
    Fps24,
    #[default]
    Fps30,
    Fps60,
}

const RECORD_FPS: [RecordFps; 3] = [RecordFps::Fps24, RecordFps::Fps30, RecordFps::Fps60];

fn record_fps_labels() -> Vec<String> {
    RECORD_FPS
        .iter()
        .map(|f| match f {
            RecordFps::Fps24 => "24 FPS",
            RecordFps::Fps30 => "30 FPS",
            RecordFps::Fps60 => "60 FPS",
        }
        .to_string())
        .collect()
}

impl RecordFps {
    pub fn as_u32(self) -> u32 {
        match self {
            RecordFps::Fps24 => 24,
            RecordFps::Fps30 => 30,
            RecordFps::Fps60 => 60,
        }
    }
}

#[derive(Default, Debug)]
enum CaptureState {
    #[default]
    Idle,
    Recording {
        stop_tx: Option<oneshot::Sender<()>>,
    },
    Saving,
}

#[derive(Debug, Clone)]
struct OutputInfo {
    output: WlOutput,
    name: String,
    logical_pos: (i32, i32),
    logical_size: (u32, u32),
    /// Size of the output's current mode in physical pixels, already
    /// swapped for 90°/270° transforms so it lines up with
    /// `logical_size`. This — not `wl_output.scale` — is what screencopy
    /// buffers are sized to. `wl_output.scale` is an integer, so a
    /// laptop panel at 125%/150% reports `2` while its buffer is only
    /// 1.25×/1.5× the logical size; every crop computed from the
    /// integer lands off-target and oversized.
    physical_size: (u32, u32),
    /// Whether the freeze capture for this output has completed (in
    /// either direction). The toolbar surface is only mapped once this
    /// is set so the frozen frame never contains our own overlay, and
    /// so the compositor doesn't yank keyboard focus (dismissing any
    /// open panel popup) before we've captured what's on screen.
    freeze_done: bool,
    /// Per-output toolbar layer surface (the pill + selection canvas).
    /// One per output so users can capture from whichever monitor they like.
    toolbar_id: Option<window::Id>,
    /// Pointer-transparent recording-border surface, opened only while
    /// recording is in progress.
    recording_id: Option<window::Id>,
    /// Frame captured at toolbar-open time, shared with the screenshot save
    /// path. `Some` means the freeze succeeded; `None` means we haven't
    /// captured yet (or capture failed). Cached as Arc so the iced image
    /// handle and the save path can both reference the same bytes without
    /// cloning the pixels.
    frozen: Option<Arc<CapturedFrame>>,
    /// Pre-built iced handle for `frozen`. Cached because building it
    /// involves a stride-repack copy that we don't want to redo every
    /// frame.
    frozen_handle: Option<cosmic::iced::widget::image::Handle>,
    /// Image-cache pin for `frozen_handle`. Set together with the
    /// handle (post-`image::allocate`) so the swap is texture-ready on
    /// the next frame, and held thereafter so iced's trim pass
    /// doesn't evict the entry between renders.
    #[allow(dead_code)]
    frozen_alloc: Option<cosmic::iced::runtime::image::Allocation>,
    /// Zero-copy alternative to `frozen_handle`: the freeze's own shm
    /// memfd wrapped for libcosmic's subsurface widget. The view
    /// prefers this when present — the compositor samples the buffer
    /// directly, so we never pay the 15–24MB-per-output texture
    /// upload on the render thread that `frozen_handle` costs.
    frozen_sub: Option<SubsurfaceBuffer>,
    /// Output's wallpaper config — pulled from `cosmic_bg_config::state`
    /// on output discovery. Window-mode picker paints this as its
    /// background (path → image, color → solid/gradient) so the
    /// foreground toplevel tiles aren't competing with their own
    /// reflections in a frozen output capture.
    bg_source: Option<cosmic_bg_config::Source>,
    /// Decoded wallpaper for `Source::Path` bg sources, cover-scaled
    /// to this output and written into a memfd for the subsurface
    /// widget — the same zero-upload path as `frozen_sub`. Decoded on
    /// a blocking thread (see `spawn_wallpaper_decode`).
    ///
    /// Not an iced image handle on purpose: `Handle::from_path`
    /// decodes a 5K JPEG synchronously in the renderer's first
    /// `prepare`, and `image::allocate` only pins the texture in
    /// whichever window iced's window manager lists first (per-window
    /// image caches), so gating the picker on it waited on a texture
    /// the picker's own window never had.
    wallpaper_sub: Option<SubsurfaceBuffer>,
}

impl OutputInfo {
    /// Physical-per-logical pixel ratio along each axis. Fractional on
    /// scaled outputs (1.25, 1.5, …); exactly 1.0 at 100%.
    fn scale_xy(&self) -> (f64, f64) {
        let (lw, lh) = self.logical_size;
        let (pw, ph) = self.physical_size;
        let sx = if lw > 0 { pw as f64 / lw as f64 } else { 1.0 };
        let sy = if lh > 0 { ph as f64 / lh as f64 } else { 1.0 };
        (sx.max(f64::EPSILON), sy.max(f64::EPSILON))
    }

    /// Crop, in this output's physical pixels, of the part of `region`
    /// (global logical coords) that falls on this output. `None` if the
    /// intersection is empty.
    fn crop_for_region(&self, region: SelectionRect) -> Option<CropRect> {
        crop_for_region(output_rect_of(self), self.physical_size, region)
    }
}

/// Map the part of `region` (global logical coords) that falls inside
/// `out` (an output's global logical rect) onto that output's physical
/// pixel grid of size `physical`. Fractional scales are handled by
/// taking the real physical/logical ratio per axis. `None` if the
/// intersection is empty.
fn crop_for_region(
    out: SelectionRect,
    physical: (u32, u32),
    region: SelectionRect,
) -> Option<CropRect> {
    let l = region.left.max(out.left);
    let t = region.top.max(out.top);
    let r = region.right.min(out.right);
    let b = region.bottom.min(out.bottom);
    if r <= l || b <= t {
        return None;
    }
    let (lw, lh) = (out.width(), out.height());
    let (pw, ph) = physical;
    let sx = if lw > 0 { pw as f64 / lw as f64 } else { 1.0 };
    let sy = if lh > 0 { ph as f64 / lh as f64 } else { 1.0 };
    // Round each edge independently rather than rounding an origin
    // and a size — otherwise a 1.5× output accumulates a one-pixel
    // seam between adjacent parts of a cross-screen region.
    let px_l = ((((l - out.left) as f64) * sx).round() as i64).clamp(0, pw as i64);
    let px_t = ((((t - out.top) as f64) * sy).round() as i64).clamp(0, ph as i64);
    let px_r = ((((r - out.left) as f64) * sx).round() as i64).clamp(px_l, pw as i64);
    let px_b = ((((b - out.top) as f64) * sy).round() as i64).clamp(px_t, ph as i64);
    Some(CropRect {
        x: px_l as i32,
        y: px_t as i32,
        w: (px_r - px_l).max(1) as u32,
        h: (px_b - px_t).max(1) as u32,
    })
}

/// Physical pixel size of an output's current mode, oriented to match
/// its logical size (i.e. swapped for 90°/270° transforms). Falls back
/// to `logical × scale_factor` when no current mode has been reported
/// yet, which is at least right for integer scales.
fn physical_size_of(info: &cosmic::cctk::sctk::output::OutputInfo) -> (u32, u32) {
    use wayland_client::protocol::wl_output::Transform;
    let mode = info
        .modes
        .iter()
        .find(|m| m.current)
        .map(|m| (m.dimensions.0.max(0) as u32, m.dimensions.1.max(0) as u32));
    match mode {
        Some((w, h)) if w > 0 && h > 0 => {
            let rotated = matches!(
                info.transform,
                Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270
            );
            if rotated { (h, w) } else { (w, h) }
        }
        _ => {
            let s = info.scale_factor.max(1) as u32;
            let (w, h) = info
                .logical_size
                .map(|(w, h)| (w.max(0) as u32, h.max(0) as u32))
                .unwrap_or((0, 0));
            (w.saturating_mul(s), h.saturating_mul(s))
        }
    }
}

pub struct Panel {
    core: Core,
    /// cosmic-config handle used to persist setting changes. None means
    /// cosmic-config init failed at startup — runtime mutations still work,
    /// they just don't survive a restart.
    config: Option<cosmic_config::Config>,

    mode: Mode,
    source: Source,
    save_target: SaveTarget,
    record_format: RecordFormat,
    record_fps: RecordFps,

    rec_encoder: VideoEncoder,
    rec_audio_mic: bool,
    rec_audio_system: bool,
    rec_cursor: bool,

    notify: bool,
    clipboard: bool,

    outputs: HashMap<u32, OutputInfo>,
    region: Option<SelectionRect>,
    /// Active drag session lifted out of the widget so every per-output
    /// instance computes new rect coords from the same anchor + start_rect.
    /// `Some` only while the user is actively dragging.
    drag: Option<DragSession>,
    /// Stop pill surface — opened only during recording. Lives at the
    /// bottom-center of the active output as a compact stop+close pill
    /// instead of the full toolbar (which would flicker as cursor moved
    /// across its buttons and block clicks to the apps being recorded).
    stop_pill_id: Option<window::Id>,
    capture: CaptureState,
    /// `Some(Instant)` while a recording is in progress — used to render the
    /// elapsed-time label on the stop pill. Cleared on shutdown.
    recording_started_at: Option<std::time::Instant>,
    /// `Some(Instant)` once the user has requested stop and we're waiting
    /// for the pipeline to drain. If it lingers past a threshold the Tick
    /// handler force-shuts to avoid the user being trapped behind a hung
    /// gst pipeline.
    saving_started_at: Option<std::time::Instant>,
    /// Output name the current recording is pinned to (for both Screen
    /// and Window recordings). Used by the per-output recording border
    /// overlays so only the actively-recorded display paints the red
    /// border — without this, every output would paint the border for
    /// Source::Screen / Source::Window recordings (the legacy region
    /// path used `self.region` to disambiguate, which doesn't exist
    /// outside of Region mode).
    recording_target_output: Option<String>,
    /// Identifier of the toplevel the current recording is capturing, if
    /// we're in window-record mode. Kept separately from
    /// `recording_target_output` so a future revision can swap the
    /// fullscreen-on-the-monitor border for a window-shaped one.
    recording_window_id: Option<String>,
    /// Set true when Esc fires during recording. gst can't abort mid-stream
    /// (the muxer would leave a corrupt file), so we still let it drain;
    /// the *Finished message handler reads this flag and deletes the
    /// pipeline's output instead of presenting it.
    cancel_pending: bool,
    /// Toolbar surface the pointer is currently over, if any. Drives the
    /// Source::Screen border so only the display the user is actually on
    /// gets highlighted — multi-monitor users were seeing every output
    /// flash a border at once, which obscured the "which one am I about
    /// to capture" affordance the border is supposed to provide.
    hovered_toolbar: Option<window::Id>,
    /// Window-picker state: toplevels with thumbnails grouped by the
    /// output they sit on. Mirrors xdg-desktop-portal-cosmic's
    /// `toplevel_images: HashMap<String, Vec<ScreenshotImage>>`. Filled
    /// in by the per-output stream from `WaylandHelper`.
    windows: HashMap<String, Vec<WindowEntry>>,
    /// Reusable `SharedFrame` slots keyed by toplevel identifier. The
    /// picker re-fetches entries on every mode/source toggle, but we
    /// want the underlying wgpu texture to stay mapped across those
    /// refreshes — recreating `SharedFrame`s with fresh ids would
    /// force `ThumbPipeline` to allocate a new texture per refresh,
    /// and the old ones would linger until `trim` (and flash visibly
    /// on transition). Keeping one slot per identifier means a refresh
    /// publishes fresh pixels into the same id; the existing texture
    /// is reused and `queue.write_texture` lands on the next frame.
    shared_frames: HashMap<String, Arc<SharedFrame>>,
    /// Per-output "the picker has had a moment to paint" flag. Set
    /// asynchronously a short delay after each `WindowsForOutputReady`
    /// — long enough for iced to build the streaming-thumbnail wgpu
    /// pipeline on first use and for `image::Handle::from_path` (the
    /// wallpaper) to land its texture. Until this flips true for a
    /// given output, we hold back both the wallpaper bg and the picker
    /// layer; otherwise the wallpaper renders first, then tiles "pop
    /// in" a frame later (the user's "BG flash"). Cleared when the
    /// user leaves Source::Window or the output goes away.
    picker_painted: HashSet<String>,
    /// Per-toplevel streaming-thumbnail capture sessions, keyed by
    /// `WindowEntry.identifier`. Each session is a continuous
    /// `toplevel_capture::start` running at picker pace; the pump task
    /// publishes frames into the matching `WindowEntry.streaming` slot
    /// and pings the redraw bus. Dropping the `Capture` (e.g. when we
    /// leave Source::Window or the window disappears) stops the loop.
    streaming_sessions: HashMap<String, toplevel_capture::Capture>,
    /// Invisible 6x6 placeholder layer surface opened at startup. Direct
    /// port of `CosmicPortal::init`'s `dummy_id` surface
    /// (`xdg-desktop-portal-cosmic/src/app.rs`): with `no_main_window`
    /// the iced runtime sits idle until at least one surface exists, so
    /// wgpu + the wayland event loop don't fully spin up. Without this
    /// dummy, the first real layer surface (toolbar / picker) is created
    /// against a cold runtime and its first frame — plus every
    /// `Task::perform` result queued during the cold-start — gets stalled
    /// for seconds while iced finally initializes. Holding one tiny
    /// always-present surface keeps the runtime warm; real surfaces
    /// opened later just attach to it.
    dummy_id: window::Id,
    /// Long-lived wayland connection + state used for every screencopy
    /// + toplevel-info call. Replaces the prior pattern of opening a
    /// fresh wayland connection inside each `capture::screencopy::capture`
    /// / `capture::toplevels::list` call (which serialized everything
    /// through a global mutex and still hung when one toplevel was
    /// unresponsive).
    helper: WaylandHelper,
    /// Output showing the toplevel that had keyboard focus when we
    /// launched. `None` = not known yet; `Some(None)` = nothing focused
    /// (or the compositor can't tell us). Drives toolbar map/destroy
    /// ordering — see `map_ready_toolbars` for why.
    focus_output: Option<Option<String>>,
    /// Invisible 1x1 `Exclusive` overlay surface on `focus_output`,
    /// mapped after the toolbars so it is what the compositor considers
    /// focused, and destroyed last so focus is restored from the right
    /// output. See `ensure_focus_catcher`.
    focus_catcher_id: Option<window::Id>,
}

#[derive(Clone, Debug)]
pub struct WindowEntry {
    identifier: String,
    /// Carried for future use (label rendering, debugging, telemetry).
    #[allow(dead_code)]
    title: String,
    /// Carried for future use (label rendering, debugging, telemetry).
    #[allow(dead_code)]
    app_id: String,
    /// Pre-built iced image handle for the captured thumbnail. Cached
    /// per entry because Handle::from_rgba copies the pixel buffer once
    /// and we don't want to redo that every redraw.
    thumb: cosmic::iced::widget::image::Handle,
    /// Full-resolution capture that produced `thumb`. Held so that when
    /// the user clicks the tile in Screenshot mode, we save *this*
    /// frame instead of firing a fresh capture — matching the
    /// region/display screenshot path, which serializes the
    /// already-frozen output capture rather than capturing again.
    /// Without this, the saved screenshot is from a different (later)
    /// moment than the thumbnail the user clicked on.
    frozen: Arc<CapturedFrame>,
    /// Source window pixel width — used to size each picker tile
    /// proportionally (`Length::FillPortion`) so a 1920px-wide window
    /// gets a wider tile than a 480px utility palette. Same arithmetic
    /// the portal applies in `widget/screenshot.rs`.
    width: u32,
    /// Explicit iced texture-cache pin. Without it, iced's wgpu cache
    /// evicts a texture as soon as a frame renders without that image
    /// in the tree (`raster::Cache::trim`); every toggle back to Window
    /// then re-uploads N full-res RGBA buffers for the picker. Holding
    /// an `Allocation` keeps the cache entry's `strong_count > 0` so
    /// trim retains the entry. Filled asynchronously via
    /// `iced::runtime::image::allocate` after the entry first lands;
    /// `None` while the allocation task is in flight (initial render
    /// still pays one upload, but every subsequent toggle is a cache
    /// hit). Unused once `streaming` takes over rendering, but retained
    /// as a fallback for the pre-first-frame state.
    #[allow(dead_code)]
    alloc: Option<cosmic::iced::runtime::image::Allocation>,
    /// Streaming-thumbnail publish slot. `Some` when a continuous
    /// `toplevel_capture::start` session is feeding this tile; the
    /// `StreamingThumb` widget reads from it directly during its wgpu
    /// `prepare`. `None` before the session starts — the picker falls
    /// back to the static `thumb` handle then.
    streaming: Option<Arc<SharedFrame>>,
}

/// Cross-screen region geometry shared by `stitch_region_screenshot`
/// and the cross-screen recording path. `canvas_w`/`canvas_h` are the
/// composite dimensions in physical pixels; `target_scale` is the
/// max-of-overlap scale we picked; `parts` is the per-output input
/// rectangles + their canvas placement.
#[allow(dead_code)] // canvas_h + target_scale are surfaced for callers that may want them
struct RegionLayout {
    canvas_w: u32,
    canvas_h: u32,
    target_scale: f64,
    parts: Vec<pipeline::record::RegionPart>,
}

#[derive(Clone, Debug)]
pub enum Msg {
    SetMode(Mode),
    SetSource(Source),
    SetSaveTarget(usize),
    SetRecordFormat(usize),
    SetRecordFps(usize),

    SelectRegion,
    Selection(SelectionEvent),
    CancelSelection,
    EscPressed,

    /// Periodic tick during recording, fired by an `iced::time::every`
    /// subscription so the elapsed-time label on the stop pill keeps moving.
    Tick,
    /// Redraw nudge from the streaming-thumbnail pump tasks. Handler is
    /// a no-op — the message itself is the signal that wakes iced and
    /// triggers a redraw, which in turn drives the `StreamingThumb`
    /// widget's `Primitive::prepare` so it can upload the newly-published
    /// frame. See `widget/streaming_thumb.rs::nudge_redraw`.
    ThumbDirty,

    ToggleClipboard,
    ToggleAudioMic,
    ToggleAudioSystem,

    /// Trigger the main action (capture / start recording / stop recording).
    /// The optional output name carries the toolbar surface the click came
    /// from, so Source::Screen targets the display you actually clicked
    /// instead of an arbitrary one from the output map.
    PrimaryAction(Option<String>),
    RecordingFinished(Result<String, String>),
    GifFinished(Result<String, String>),
    ScreenshotFinished(Result<String, String>),

    Output(OutputEvent, WlOutput),
    /// Pointer entered or left a toolbar surface. Tracked per-window so we
    /// can highlight only the display the user is currently on.
    PointerOn(Option<window::Id>),
    /// Frozen-background capture for one output completed. Carries the
    /// output key + the helper-side `CapturedFrame`. The result is
    /// optional because the helper returns `None` for screencopy
    /// failures.
    FrozenFrameReady(u32, Option<Arc<CapturedFrame>>),
    /// `image::allocate` completed for a freshly-captured freeze. The
    /// handle is already installed in `info.frozen_handle` (see
    /// `FrozenFrameReady`); this just stores the pin so iced's trim
    /// pass can't evict the texture between renders.
    FrozenAllocated(u32, Option<cosmic::iced::runtime::image::Allocation>),
    /// Per-output toplevel thumbnails arrived as a batch. Carries the
    /// output name (matched against `OutputInfo.name`) and the captured
    /// entries in stream order. Replaces the previous per-window
    /// `Msg::WindowThumbReady` pattern — the helper's
    /// `capture_output_toplevels_shm` already streams sessions on the
    /// shared connection.
    WindowsForOutputReady(String, Vec<WindowEntry>),
    /// Fired ~100ms after `WindowsForOutputReady` for a given output.
    /// Marks the picker layer as "painted" so the wallpaper bg + tile
    /// row can show together. Without this delay, the wallpaper renders
    /// a frame before iced finishes building the streaming-thumb wgpu
    /// pipeline on first use, producing a visible bg-then-tiles flash.
    PickerPainted(String),
    /// `iced::widget::image::allocate` completed for `(output_name,
    /// index)`. The `Allocation` (if Some) pins the thumbnail's texture
    /// in the wgpu image cache so it doesn't get evicted when the
    /// picker leaves the widget tree (Region/Screen mode) and reupload
    /// on the next Window toggle.
    ThumbAllocated(String, usize, Option<cosmic::iced::runtime::image::Allocation>),
    /// Background wallpaper decode finished for some outputs. `None`
    /// means the file couldn't be decoded (or the memfd failed); the
    /// picker then falls back to the frozen capture as its backdrop.
    WallpaperDecoded(Vec<(u32, Option<SubsurfaceBuffer>)>),
    /// User clicked a window card → capture that window full-res and save.
    CaptureToplevel(String),
    Quit,
    /// cosmic-toplevel-info told us which output held the focused
    /// window at launch (see `Panel::focus_output`).
    FocusOutputKnown(Option<String>),
}

impl Application for Panel {
    type Executor = executor::Default;
    type Flags = ();
    type Message = Msg;
    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &Core {
        &self.core
    }
    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, _flags: Self::Flags) -> (Self, Task<Self::Message>) {
        let (settings, config) = super::config::UserSettings::load();
        let dummy_id = window::Id::unique();
        // Direct port of `CosmicPortal::init`: open the wayland connection
        // *after* iced has started its main loop. Opening it earlier (in
        // `launch` before `cosmic::app::run`) made the first picker open
        // take ~10s — the helper's dispatch thread was running but iced's
        // own wayland connection was still cold, so capture events stalled
        // until iced caught up.
        // Resolve the toolbar's icon names to paths off the main
        // thread. The first `icon::from_name(..).path()` per name walks
        // the icon-theme directories (a stat storm that otherwise lands
        // inside the first `view()`); the result goes into
        // freedesktop-icons' global cache, keyed by (theme, name, size,
        // scale), which the view's un-sized lookups then hit. Done here
        // rather than in `launch` so libcosmic has already applied the
        // user's icon theme.
        std::thread::Builder::new()
            .name("icon-warm".into())
            .spawn(|| {
                for name in TOOLBAR_ICONS {
                    let _ = icon::from_name(*name).path();
                }
            })
            .ok();
        let wayland_conn = wayland_client::Connection::connect_to_env()
            .expect("connect wayland");
        let helper = WaylandHelper::new(wayland_conn)
            .expect("wayland helper init");
        let panel = Self {
            core,
            config,
            mode: settings.mode,
            source: settings.source,
            save_target: settings.save_target,
            record_format: settings.record_format,
            record_fps: settings.record_fps,
            rec_encoder: VideoEncoder::Auto,
            rec_audio_mic: false,
            rec_audio_system: false,
            rec_cursor: true,
            notify: true,
            clipboard: false,
            outputs: HashMap::new(),
            region: settings.last_region,
            drag: None,
            stop_pill_id: None,
            capture: CaptureState::default(),
            recording_started_at: None,
            saving_started_at: None,
            recording_target_output: None,
            recording_window_id: None,
            cancel_pending: false,
            hovered_toolbar: None,
            windows: HashMap::new(),
            shared_frames: HashMap::new(),
            picker_painted: HashSet::new(),
            streaming_sessions: HashMap::new(),
            helper,
            dummy_id,
            focus_output: None,
            focus_catcher_id: None,
        };

        // Direct port of `CosmicPortal::init` in
        // xdg-desktop-portal-cosmic/src/app.rs: open an invisible 6x6
        // bottom-layer placeholder surface so iced's runtime, wgpu, and
        // wayland event loop come up immediately instead of sitting cold
        // until the first real surface arrives. Real surfaces opened
        // later (toolbar in `ensure_toolbar_for`, picker, recording
        // overlays) attach to the already-warm runtime and render
        // without the multi-second cold-start stall. `view_window`
        // returns an empty space for this id.
        let dummy = get_layer_surface(SctkLayerSurfaceSettings {
            id: dummy_id,
            layer: Layer::Bottom,
            keyboard_interactivity: KeyboardInteractivity::None,
            input_zone: Some(Vec::new()),
            anchor: Anchor::empty(),
            output: IcedOutput::Active,
            namespace: "cosmic-capture-dummy".into(),
            margin: IcedMargin::default(),
            size: Some((Some(6), Some(6))),
            exclusive_zone: -1,
            size_limits: Limits::NONE,
        });

        // Which output holds the focused window? Feeds
        // `ensure_focus_catcher`. Read from the first burst of
        // toplevel-info events (~45ms), before any of our surfaces can
        // take focus and move `Activated` elsewhere.
        let helper = panel.helper.clone();
        let focus_lookup = Task::perform(
            async move {
                helper.wait_for_activated_toplevel().await;
                helper.activated_toplevel_output()
            },
            |name| cosmic::action::app(Msg::FocusOutputKnown(name)),
        );
        (panel, Task::batch([dummy, focus_lookup]))
    }

    fn update(&mut self, msg: Msg) -> Task<Msg> {
        match msg {
            Msg::SetMode(m) => {
                if !self.locked() && self.mode != m {
                    self.mode = m;
                    self.persist("mode", &m);
                    let mut tasks: Vec<Task<Msg>> = Vec::new();
                    match m {
                        Mode::Screenshot => {
                            // Leaving Record mode — drop any streaming
                            // thumbnail sessions; Screenshot wants the
                            // frozen view, not live frames.
                            self.stop_all_stream_sessions();
                            // Deliberately keep `picker_painted` set if
                            // it already is. While we re-fetch entries
                            // async, the existing static-thumbnail
                            // tiles stay visible so the picker view
                            // doesn't flicker back to "normal screen"
                            // during a Record↔Screenshot swap. New
                            // entries replace in place via
                            // `windows.insert` once they land.
                            // NOTE: we deliberately *don't* refresh the
                            // output freezes here. The freezes captured
                            // on `Output::Created` are pre-allocated
                            // and pinned via `frozen_alloc`; replacing
                            // them mid-session causes iced's renderer
                            // to swap to a fresh handle whose first
                            // sample races the upload, and the live
                            // desktop bleeds through for a frame on the
                            // next source change. The trade-off is
                            // staleness — Region/Screen show the
                            // screen as it was at app start — which is
                            // acceptable for selection framing since
                            // the actual capture is always live.
                            if matches!(self.source, Source::Window) {
                                tasks.push(self.force_refresh_window_picker());
                            }
                        }
                        Mode::Record => {
                            // Keep existing frozen captures around as a
                            // stale fallback for the next Screenshot
                            // entry. Screenshot mode no longer
                            // refreshes freezes on mode/source changes
                            // (re-capture would race iced's renderer
                            // and also bake any open overlays — picker,
                            // toolbar — into the freeze), so the
                            // capture from `Output::Created` is the
                            // canonical clean version.
                            // Window thumbnails are also frozen
                            // snapshots; in Record mode the user is
                            // about to capture live action against
                            // those windows, so showing the stale
                            // toolbar-open snapshot is the wrong
                            // affordance — refresh on each entry so the
                            // tiles match the current window contents.
                            // (Deliberately keep `picker_painted` so the
                            // picker view persists across the swap.)
                            if matches!(self.source, Source::Window) {
                                tasks.push(self.force_refresh_window_picker());
                            }
                        }
                    }
                    if !tasks.is_empty() {
                        return Task::batch(tasks);
                    }
                }
            }
            Msg::SetSource(s) => {
                if !self.locked() && self.source != s {
                    self.source = s;
                    self.persist("source", &s);
                    if matches!(s, Source::Window) {
                        // Force a fresh picker capture so tiles match
                        // "right now". We deliberately *don't* refresh
                        // the output freezes here even in Screenshot
                        // mode — screencopy includes our own layer
                        // surfaces (toolbar pill, picker bg, etc.) so
                        // re-capturing while the picker is open bakes
                        // the picker overlay into the freeze, which
                        // then shows through when the user switches
                        // back to Region/Screen. The freeze captured
                        // at `Output::Created` (before any overlay was
                        // painted) is the clean version; keep it.
                        let keys: Vec<u32> = self.outputs.keys().copied().collect();
                        return Task::batch([
                            self.spawn_wallpaper_decode(&keys),
                            self.force_refresh_window_picker(),
                        ]);
                    } else {
                        // Leaving Window source — stop all streaming
                        // sessions so we're not paying screencopy
                        // overhead on toplevels the user can't see.
                        // Deliberately keep `picker_painted` set: on
                        // re-entry to Source::Window the wallpaper +
                        // tile row should appear immediately with the
                        // last-known entries (refreshed in place when
                        // the new captures land) rather than falling
                        // back to the "frozen alone" loading state for
                        // 100ms+ per round-trip, which flickers
                        // visibly when the user toggles
                        // Region↔Window in Screenshot mode.
                    }
                }
            }
            Msg::ToggleClipboard => self.clipboard = !self.clipboard,
            Msg::ToggleAudioMic => self.rec_audio_mic = !self.rec_audio_mic,
            Msg::ToggleAudioSystem => self.rec_audio_system = !self.rec_audio_system,
            Msg::SetSaveTarget(i) => {
                if let Some(&t) = SAVE_TARGETS.get(i) {
                    if self.save_target != t {
                        self.save_target = t;
                        self.persist("save_target", &t);
                    }
                }
            }
            Msg::SetRecordFormat(i) => {
                if let Some(&f) = RECORD_FORMATS.get(i) {
                    if self.record_format != f {
                        self.record_format = f;
                        self.persist("record_format", &f);
                    }
                }
            }
            Msg::SetRecordFps(i) => {
                if let Some(&f) = RECORD_FPS.get(i) {
                    if self.record_fps != f {
                        self.record_fps = f;
                        self.persist("record_fps", &f);
                    }
                }
            }

            Msg::Output(ev, output) => {
                let key = wayland_proxy_id(&output);
                match ev {
                    OutputEvent::Created(Some(info)) => {
                        let name =
                            info.name.clone().unwrap_or_else(|| format!("output-{key}"));
                        let logical_pos = info.logical_position.unwrap_or((0, 0));
                        let logical_size = info
                            .logical_size
                            .map(|(w, h)| (w as u32, h as u32))
                            .unwrap_or((0, 0));
                        let physical_size = physical_size_of(&info);
                        tracing::info!(
                            output = %name,
                            ?logical_pos,
                            ?logical_size,
                            ?physical_size,
                            wl_scale = info.scale_factor,
                            "output registered"
                        );
                        let name_for_capture = name.clone();
                        let bg_source = load_bg_for_output(&name);
                        self.outputs.insert(
                            key,
                            OutputInfo {
                                output,
                                name,
                                logical_pos,
                                logical_size,
                                physical_size,
                                freeze_done: false,
                                toolbar_id: None,
                                recording_id: None,
                                frozen: None,
                                frozen_handle: None,
                                frozen_sub: None,
                                frozen_alloc: None,
                                bg_source,
                                wallpaper_sub: None,
                            },
                        );
                        // Don't `prune_stale_region` here: outputs arrive
                        // one at a time, and a persisted region for
                        // monitor 2 would be wiped the moment monitor 1
                        // shows up (its rect doesn't contain the region
                        // center). The region simply doesn't render
                        // until its owning monitor comes online; the
                        // user can still see it once all outputs are
                        // registered. If the monitor is truly gone, the
                        // region just sits invisibly until the user
                        // drags a new one.
                        let with_cursor = self.rec_cursor;
                        let mut tasks: Vec<Task<Msg>> = Vec::new();
                        // Freeze background *before* the toolbar maps.
                        // The toolbar is opened from `FrozenAllocated`
                        // (or `FrozenFrameReady(None)`), never here —
                        // same ordering as xdg-desktop-portal-cosmic,
                        // which captures every output and only then
                        // creates its layer surfaces. Issuing both in
                        // one batch raced: the toolbar is an
                        // `Exclusive`-keyboard overlay, so the
                        // compositor moves focus to it the moment it
                        // maps, which dismisses any open panel popup —
                        // and whether the freeze caught the popup (or
                        // our own toolbar) depended on which request
                        // the compositor serviced first.
                        //
                        // Captured unconditionally, not just in
                        // Screenshot mode, so a later Mode::Screenshot
                        // entry doesn't pay a visible "transparent until
                        // freeze lands" gap. The view only paints it in
                        // Screenshot mode.
                        {
                            let helper = self.helper.clone();
                            let name_for_freeze = name_for_capture.clone();
                            tasks.push(Task::perform(
                                async move {
                                    capture_output_via_helper(
                                        &helper,
                                        &name_for_freeze,
                                        with_cursor,
                                    )
                                    .await
                                },
                                move |r| cosmic::action::app(Msg::FrozenFrameReady(key, r)),
                            ));
                        }
                        // Per-output window picker — runs whenever
                        // Source::Window is active, regardless of mode.
                        // Screenshot+Window clicks save the toplevel;
                        // Record+Window clicks start continuous
                        // toplevel-screencopy recording.
                        if matches!(self.source, Source::Window) {
                            let picker_name = name_for_capture.clone();
                            tasks.push(self.spawn_picker_capture(picker_name, with_cursor));
                        }
                        // The wallpaper is only ever painted by the
                        // Window picker, so don't touch it unless
                        // that's the source we're launching into;
                        // `SetSource(Window)` decodes it lazily
                        // otherwise. Decoding it eagerly for every
                        // output was ~100ms of JPEG decode + a 59MB
                        // texture upload per output at launch.
                        if matches!(self.source, Source::Window) {
                            tasks.push(self.spawn_wallpaper_decode(&[key]));
                        }
                        return Task::batch(tasks);
                    }
                    OutputEvent::Created(None) => {}
                    OutputEvent::InfoUpdate(info) => {
                        if let Some(o) = self.outputs.get_mut(&key) {
                            o.physical_size = physical_size_of(&info);
                            if let Some(n) = info.name {
                                o.name = n;
                            }
                            if let Some(p) = info.logical_position {
                                o.logical_pos = p;
                            }
                            if let Some((w, h)) = info.logical_size {
                                o.logical_size = (w as u32, h as u32);
                            }
                        }
                        // If we'd missed opening a toolbar (e.g. the Created
                        // event arrived without geometry), retry now — but
                        // only once the freeze has landed; before that the
                        // deferred open in `FrozenAllocated` is still
                        // pending and must stay ordered after the capture.
                        if self.outputs.get(&key).is_some_and(|o| o.freeze_done) {
                            return self.map_ready_toolbars();
                        }
                    }
                    OutputEvent::Removed => {
                        if let Some(info) = self.outputs.remove(&key) {
                            // Drop this output's picker entries so the
                            // map doesn't keep stale data after a
                            // monitor disconnect. `refresh_window_picker`
                            // skips outputs that still have entries; if
                            // a reattached output reappears with the
                            // same name we want a fresh capture.
                            self.windows.remove(&info.name);
                            self.picker_painted.remove(&info.name);
                            let mut tasks: Vec<Task<Msg>> = Vec::new();
                            if let Some(id) = info.toolbar_id {
                                tasks.push(destroy_layer_surface(id));
                            }
                            if let Some(id) = info.recording_id {
                                tasks.push(destroy_layer_surface(id));
                            }
                            return Task::batch(tasks);
                        }
                    }
                }
            }

            Msg::SelectRegion => {
                // Legacy no-op — selector is always live in Region mode.
            }
            Msg::Selection(SelectionEvent::DragStart {
                session,
                initial_rect,
            }) => {
                tracing::trace!(?session, "drag started");
                self.drag = Some(session);
                self.region = Some(initial_rect);
                // Don't persist mid-drag — wait for DragEnd.
            }
            Msg::Selection(SelectionEvent::DragMove(rect)) => {
                self.region = Some(rect);
            }
            Msg::Selection(SelectionEvent::DragEnd) => {
                tracing::trace!("drag ended");
                self.drag = None;
                // Persist the final rect now that the user has released.
                if let Some(r) = self.region {
                    self.persist("last_region", &Some(r));
                }
            }
            Msg::CancelSelection => {
                tracing::info!("selection cancelled");
                self.region = None;
                self.drag = None;
                self.persist("last_region", &Option::<SelectionRect>::None);
            }

            Msg::ThumbDirty => {
                // No-op. Receipt of the message itself wakes iced and
                // triggers a redraw, which is all the streaming-thumbnail
                // pump tasks need. Frame data already lives in the
                // shared slot the `StreamingThumb` widget reads from.
            }

            Msg::Tick => {
                // While Saving, watch for a hung pipeline (e.g. gst stuck
                // on EOS after the pwsrc caps assertion) and force a
                // shutdown after a generous grace period so the user isn't
                // trapped behind a frozen "Saving…" UI. cancel_pending
                // ensures any in-flight output file is deleted.
                //
                // 30s is the upper bound for a legitimate finalize: gifski
                // on a multi-second recording at quality 75 can spend
                // several seconds quantizing + writing frames after the
                // collector closes; H.264 muxer faststart rewrites likewise
                // can take a moment on long recordings. Smaller values
                // killed real saves mid-finalize.
                if matches!(self.capture, CaptureState::Saving) {
                    if let Some(started) = self.saving_started_at {
                        if started.elapsed() > std::time::Duration::from_secs(30) {
                            tracing::warn!(
                                elapsed_ms = started.elapsed().as_millis() as u64,
                                "Saving stalled — forcing shutdown"
                            );
                            self.cancel_pending = true;
                            return self.shutdown();
                        }
                    }
                }
                // Otherwise no state mutation needed — the tick just
                // triggers a re-render so view_stop_pill picks up a fresh
                // elapsed time.
            }
            Msg::PrimaryAction(clicked_output) => {
                tracing::info!(
                    state = ?self.capture,
                    clicked_output = ?clicked_output,
                    "Msg::PrimaryAction"
                );
                return self.on_primary_action(clicked_output);
            }
            Msg::EscPressed => match &mut self.capture {
                CaptureState::Recording { stop_tx } => {
                    // Esc while recording = cancel: flag the pending output
                    // for deletion, then signal the pipeline to drain. We
                    // can't kill it mid-stream without corrupting the file.
                    self.cancel_pending = true;
                    if let Some(tx) = stop_tx.take() {
                        let _ = tx.send(());
                    }
                    self.saving_started_at = Some(std::time::Instant::now());
                }
                CaptureState::Idle => {
                    // Not recording → Esc exits the application entirely.
                    return self.shutdown();
                }
                CaptureState::Saving => {
                    // Escape hatch for a stuck save (e.g. gst hung waiting
                    // on EOS that never arrives, as happens after the
                    // pipewiresrc caps assertion). Forces shutdown so the
                    // user isn't trapped with a frozen "Saving…" UI.
                    tracing::warn!("Esc during Saving → forcing shutdown");
                    self.cancel_pending = true;
                    return self.shutdown();
                }
            },

            msg @ (Msg::RecordingFinished(_) | Msg::GifFinished(_) | Msg::ScreenshotFinished(_)) => {
                // is_recording_path lets us decide whether to copy the saved
                // file path onto the clipboard — only for actual recordings
                // (video / gif), since screenshot paths are the file the
                // user asked for and they may have chosen the Clipboard
                // target already.
                let is_recording_path =
                    matches!(msg, Msg::RecordingFinished(_) | Msg::GifFinished(_));
                let r = match msg {
                    Msg::RecordingFinished(r) | Msg::GifFinished(r) | Msg::ScreenshotFinished(r) => r,
                    _ => unreachable!(),
                };
                if self.cancel_pending {
                    if let Ok(p) = &r {
                        let path = std::path::PathBuf::from(p);
                        match std::fs::remove_file(&path) {
                            Ok(()) => tracing::info!(path = %p, "cancelled — deleted output"),
                            Err(e) => tracing::warn!(path = %p, error = %e,
                                "cancelled — failed to delete output"),
                        }
                    }
                } else if let Err(e) = &r {
                    tracing::warn!("capture pipeline finished with error: {}", e);
                } else if let Ok(p) = &r {
                    tracing::info!(path = %p, "capture pipeline finished");
                    if is_recording_path {
                        // Load the file off disk and put the bytes on the
                        // clipboard with the right video/* MIME, so apps
                        // that accept media on paste (chat clients, image
                        // editors with video support, etc.) get the actual
                        // payload rather than a path string. Read on the
                        // blocking pool — recordings can be tens of MB.
                        let path = std::path::PathBuf::from(p);
                        let mime = self.record_format.mime();
                        match std::fs::read(&path) {
                            Ok(bytes) => {
                                if let Err(e) =
                                    pipeline::screenshot::copy_bytes_with_uri_to_clipboard(
                                        bytes,
                                        mime,
                                        path.clone(),
                                    )
                                {
                                    tracing::warn!(error = %e,
                                        "failed to copy recording to clipboard");
                                }
                            }
                            Err(e) => {
                                tracing::warn!(path = %p, error = %e,
                                    "failed to read recording for clipboard");
                            }
                        }
                    }
                }
                return self.shutdown();
            }

            Msg::WindowsForOutputReady(output_name, entries) => {
                tracing::info!(
                    output = %output_name,
                    count = entries.len(),
                    "window picker: per-output capture stream finished"
                );
                // Kick off explicit `image::allocate` for every thumb
                // so iced's wgpu cache pins the texture. Without this,
                // a Region↔Window toggle pays a full reupload of each
                // picker thumbnail every cycle (see `WindowEntry.alloc`
                // docstring).
                let mut allocate_tasks: Vec<Task<Msg>> = Vec::new();
                let mut entries = entries;
                for (i, entry) in entries.iter().enumerate() {
                    let name_for_msg = output_name.clone();
                    allocate_tasks.push(
                        cosmic::iced::runtime::image::allocate(entry.thumb.clone())
                            .map(move |r: Result<_, _>| {
                                cosmic::action::app(Msg::ThumbAllocated(
                                    name_for_msg.clone(),
                                    i,
                                    r.ok(),
                                ))
                            }),
                    );
                }
                // Assign each entry a `SharedFrame` slot from the
                // process-wide pool, keyed by toplevel identifier. If
                // a slot already exists (because we've shown this
                // toplevel in a prior refresh), reuse it — the
                // `ThumbPipeline` wgpu texture mapped to that slot's
                // id stays valid across refreshes, so we only pay the
                // upload cost once per toplevel rather than per
                // refresh. Either way, publish the fresh pixels into
                // the slot so the next prepare uploads the new frame.
                for entry in entries.iter_mut() {
                    let shared = self
                        .shared_frames
                        .entry(entry.identifier.clone())
                        .or_insert_with(SharedFrame::new)
                        .clone();
                    shared.publish(RawFrame {
                        pixels: entry.frozen.pixels.clone(),
                        width: entry.frozen.width,
                        height: entry.frozen.height,
                        stride: entry.frozen.stride,
                    });
                    entry.streaming = Some(shared);
                }
                // Record mode: attach a continuous capture pump on top
                // of the reused slot. Screenshot mode leaves the slot
                // static, frozen on the last published pixels.
                if matches!(self.source, Source::Window)
                    && matches!(self.mode, Mode::Record)
                {
                    for entry in entries.iter_mut() {
                        if let Some(shared) = entry.streaming.clone() {
                            let ident = entry.identifier.clone();
                            self.attach_stream_to(&ident, shared);
                        }
                    }
                }
                // Prune `shared_frames` slots that no toplevel
                // references anymore. After insert, sweep against the
                // union of all currently-known identifiers across
                // every output's entries.
                let live: HashSet<&str> = self
                    .windows
                    .values()
                    .flat_map(|v| v.iter().map(|e| e.identifier.as_str()))
                    .chain(entries.iter().map(|e| e.identifier.as_str()))
                    .collect();
                self.shared_frames
                    .retain(|ident, _| live.contains(ident.as_str()));
                self.windows.insert(output_name.clone(), entries);
                // Schedule the "picker painted" flip ~100ms out, long
                // enough for iced to build our streaming-thumb wgpu
                // pipeline on first use and for the wallpaper image
                // texture to land. View gates wallpaper + picker layer
                // on this so both appear together rather than wallpaper
                // first then tiles popping in.
                let painted_name = output_name;
                let paint_settled: Task<Msg> = Task::perform(
                    async move {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        painted_name
                    },
                    |n| cosmic::action::app(Msg::PickerPainted(n)),
                );
                let mut all = allocate_tasks;
                all.push(paint_settled);
                return Task::batch(all);
            }
            Msg::PickerPainted(output_name) => {
                tracing::debug!(output = %output_name, "picker painted");
                self.picker_painted.insert(output_name);
            }
            Msg::ThumbAllocated(output_name, idx, alloc) => {
                if let Some(entries) = self.windows.get_mut(&output_name) {
                    if let Some(entry) = entries.get_mut(idx) {
                        entry.alloc = alloc;
                    }
                }
            }
            Msg::WallpaperDecoded(results) => {
                for (key, sub) in results {
                    let Some(info) = self.outputs.get_mut(&key) else {
                        continue;
                    };
                    let Some(sub) = sub else {
                        tracing::warn!(output = %info.name, "wallpaper decode failed; picker uses freeze bg");
                        info.bg_source = None;
                        continue;
                    };
                    tracing::debug!(output = %info.name, "wallpaper ready");
                    info.wallpaper_sub = Some(sub);
                }
            }
            Msg::CaptureToplevel(identifier) => {
                if !matches!(self.capture, CaptureState::Idle) {
                    return Task::none();
                }
                // About to capture / record this toplevel — tear down
                // every streaming-thumbnail session so the picker
                // sessions don't fight the recording session for the
                // same toplevel's screencopy queue.
                self.stop_all_stream_sessions();
                // Raise + focus the picked toplevel before tearing down
                // the picker. cosmic-screencopy is happy to capture a
                // minimized/background window, but the user's clicked it
                // because they want to interact with it (especially in
                // Record mode — they need it in the foreground to demo).
                // Best-effort: silently no-ops on compositors without the
                // unstable management protocol.
                self.helper.activate_toplevel(&identifier);
                let cursor = self.rec_cursor;
                let close_bars = self.close_toolbars();
                match self.mode {
                    Mode::Screenshot => {
                        let destination = self.current_destination();
                        let notify_user = self.notify;
                        // Reuse the cached thumbnail frame so the saved
                        // PNG is from the same instant as the tile the
                        // user clicked — same trick the Region/Display
                        // path uses with `info.frozen`. If no entry is
                        // cached (e.g. the picker is still loading)
                        // fall back to a fresh capture_toplevel so the
                        // click still does something useful.
                        let cached_frame = self
                            .windows
                            .values()
                            .flat_map(|v| v.iter())
                            .find(|e| e.identifier == identifier)
                            .map(|e| (*e.frozen).clone());
                        self.capture = CaptureState::Saving;
                        self.saving_started_at = Some(std::time::Instant::now());
                        let task = if let Some(frame) = cached_frame {
                            Task::perform(
                                run_save_frame(frame, None, destination, notify_user),
                                |r| cosmic::action::app(Msg::ScreenshotFinished(r)),
                            )
                        } else {
                            Task::perform(
                                run_toplevel_screenshot(
                                    identifier,
                                    cursor,
                                    destination,
                                    notify_user,
                                ),
                                |r| cosmic::action::app(Msg::ScreenshotFinished(r)),
                            )
                        };
                        return Task::batch([close_bars, task]);
                    }
                    Mode::Record => {
                        let helper = self.helper.clone();
                        let stop_pill = self.open_stop_pill();
                        let swap = self.open_recording_overlays();
                        let (stop_tx, stop_rx) = oneshot::channel();
                        let recording_started = std::time::Instant::now();
                        let target = ScreencopyTarget::Toplevel {
                            identifier: identifier.clone(),
                        };
                        self.recording_window_id = Some(identifier.clone());
                        self.recording_target_output =
                            self.helper.output_name_for_toplevel(&identifier);
                        match self.record_format {
                            RecordFormat::Gif => {
                                let args = self.build_gif_args();
                                self.capture = CaptureState::Recording {
                                    stop_tx: Some(stop_tx),
                                };
                                self.recording_started_at = Some(recording_started);
                                return Task::batch([
                                    close_bars,
                                    stop_pill,
                                    swap,
                                    Task::perform(
                                        run_gif_screencopy(
                                            helper, target, args, None, stop_rx,
                                        ),
                                        |r| cosmic::action::app(Msg::GifFinished(r)),
                                    ),
                                ]);
                            }
                            _ => {
                                let args = self.build_record_args();
                                self.capture = CaptureState::Recording {
                                    stop_tx: Some(stop_tx),
                                };
                                self.recording_started_at = Some(recording_started);
                                return Task::batch([
                                    close_bars,
                                    stop_pill,
                                    swap,
                                    Task::perform(
                                        run_record_screencopy(
                                            helper, target, args, None, stop_rx,
                                        ),
                                        |r| cosmic::action::app(Msg::RecordingFinished(r)),
                                    ),
                                ]);
                            }
                        }
                    }
                }
            }

            Msg::FrozenFrameReady(key, result) => {
                if !self.outputs.contains_key(&key) {
                    return Task::none();
                }
                let Some(frame) = result else {
                    tracing::warn!(
                        key,
                        "freeze capture returned None; toolbar will run live-screen"
                    );
                    if let Some(o) = self.outputs.get_mut(&key) {
                        o.freeze_done = true;
                    }
                    return self.map_ready_toolbars();
                };
                // Install the freeze *now* and map the toolbar in the
                // same batch (it must not open any earlier — see
                // `Output::Created`). Preferred path: hand the
                // compositor the screencopy memfd itself through a
                // subsurface, so the first frame shows the frozen
                // desktop with no texture upload at all.
                if let Some(sub) = frozen_subsurface_buffer(&frame) {
                    if let Some(info) = self.outputs.get_mut(&key) {
                        info.frozen = Some(frame);
                        info.frozen_sub = Some(sub);
                        info.freeze_done = true;
                    }
                    return self.map_ready_toolbars();
                }
                // No shm fd (or dup failed): fall back to a texture. The
                // toolbar's first `prepare` uploads it synchronously, so
                // its first frame still shows the freeze. We kick
                // `image::allocate` to pin the wgpu cache entry, but
                // can't *wait* on it before opening the toolbar:
                // allocation only completes during a window redraw, and
                // with no toolbar mapped there is nothing to redraw.
                let handle = shared_frame_to_image_handle(&frame);
                let pin = cosmic::iced::runtime::image::allocate(handle.clone()).map(
                    move |r: Result<_, _>| {
                        cosmic::action::app(Msg::FrozenAllocated(key, r.ok()))
                    },
                );
                if let Some(info) = self.outputs.get_mut(&key) {
                    info.frozen = Some(frame);
                    info.frozen_handle = Some(handle);
                    info.freeze_done = true;
                }
                return Task::batch([self.map_ready_toolbars(), pin]);
            }
            Msg::FrozenAllocated(key, alloc) => {
                if let Some(info) = self.outputs.get_mut(&key) {
                    info.frozen_alloc = alloc;
                }
            }

            Msg::PointerOn(id) => {
                // Only accept hover transitions that name an actual
                // toolbar surface — random window ids (stop pill,
                // recording overlay) shouldn't suppress the border on
                // the toolbar the user is on. `None` clears. Skip the
                // assignment if nothing actually changed so iced doesn't
                // schedule a redraw for a no-op message.
                let next = id.filter(|id| {
                    self.outputs
                        .values()
                        .any(|o| o.toolbar_id == Some(*id))
                });
                if next == self.hovered_toolbar {
                    return Task::none();
                }
                self.hovered_toolbar = next;
            }

            Msg::Quit => {
                return self.shutdown();
            }
            Msg::FocusOutputKnown(name) => {
                tracing::info!(output = ?name, "focused toplevel's output at launch");
                self.focus_output = Some(name);
                return self.map_ready_toolbars();
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Msg> {
        // We run with no_main_window — iced should never call this. Return an
        // empty space defensively.
        iced::widget::Space::new()
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn view_window(&self, id: window::Id) -> Element<'_, Msg> {
        // Dummy warm-up surface from init() — invisible 6x6 placeholder.
        // Returns empty space matching xdg-desktop-portal-cosmic's
        // `view_window` branch for its dummy_id.
        if id == self.dummy_id || Some(id) == self.focus_catcher_id {
            return iced::widget::Space::new()
                .width(Length::Fill)
                .height(Length::Fill)
                .into();
        }
        // Stop pill — small bottom-centered surface that replaces the
        // toolbars while a recording is in progress. Owns the Exclusive
        // keyboard grab during recording so Space/Esc continue to work.
        if Some(id) == self.stop_pill_id {
            return self.view_stop_pill();
        }
        // Toolbar surface — every output gets its own. The pill renders
        // identically on each; the selection canvas underneath is scoped to
        // that output's bounds.
        if let Some(info) = self.outputs.values().find(|o| o.toolbar_id == Some(id)) {
            return self.view_toolbar(info);
        }
        // Recording overlay — pointer-transparent surface that just paints
        // the red border around the active capture region. Gated on the
        // *active source* (not `self.region.is_some()`): the user might
        // have a persisted region sitting in state while currently
        // recording a whole display, and we don't want the rect overlay
        // to take precedence over the fullscreen border in that case.
        // Other outputs render empty so we don't suggest they're also
        // being captured.
        if let Some(info) = self.outputs.values().find(|o| o.recording_id == Some(id)) {
            let on_target = self.recording_target_output.as_deref()
                == Some(info.name.as_str());
            // Region recording: draw the rect outline on *every* output
            // that overlaps the region, not just the target. For single-
            // output regions this is the same set as before (one output);
            // for cross-screen it lets each participating output draw its
            // slice of the rect. `RectangleSelection` clips the outline to
            // `output_rect`, so the rect's outer perimeter only renders
            // where it sits inside each output — no red drawn at the
            // seam between outputs (the seam runs through the rect's
            // interior, not its perimeter).
            if matches!(self.source, Source::Region) {
                if let Some(region) = self.region {
                    let output_rect = output_rect_of(info);
                    let overlaps = region.normalize().left < output_rect.right
                        && region.normalize().right > output_rect.left
                        && region.normalize().top < output_rect.bottom
                        && region.normalize().bottom > output_rect.top;
                    if overlaps {
                        return RectangleSelection::new(
                            output_rect,
                            region,
                            RectMode::Recording,
                            SELECTION_DND_ID,
                            id,
                            None,
                            Msg::Selection,
                        )
                        .into();
                    }
                    // Non-overlapping output during a Region recording —
                    // empty surface, no border. Fall through to the
                    // bottom of this match.
                    return iced::widget::Space::new().into();
                }
            }
            if matches!(self.source, Source::Window) && on_target {
                // Window mode: draw the border *around the window* by
                // feeding the toplevel's compositor-global rect into
                // RectangleSelection (its Recording mode is exactly the
                // "thin red rect, no handles" we want). Falls back to
                // a fullscreen border if the compositor hasn't
                // advertised toplevel geometry yet (zcosmic_toplevel_
                // info_v1 v2+) — better an oversized border than
                // none.
                if let Some(window_id) = self.recording_window_id.as_deref() {
                    if let Some((wx, wy, ww, wh)) = self
                        .helper
                        .toplevel_geometry_on_output(window_id, &info.name)
                    {
                        let output_rect = output_rect_of(info);
                        let selection = SelectionRect {
                            left: info.logical_pos.0 + wx,
                            top: info.logical_pos.1 + wy,
                            right: info.logical_pos.0 + wx + ww,
                            bottom: info.logical_pos.1 + wy + wh,
                        };
                        return RectangleSelection::new(
                            output_rect,
                            selection,
                            RectMode::Recording,
                            SELECTION_DND_ID,
                            id,
                            None,
                            Msg::Selection,
                        )
                        .into();
                    }
                }
                return fullscreen_recording_border();
            }
            if matches!(self.source, Source::Screen) && on_target {
                return fullscreen_recording_border();
            }
            return iced::widget::Space::new().into();
        }
        iced::widget::Space::new().into()
    }

    fn subscription(&self) -> Subscription<Msg> {
        let outputs = event::listen_with(|e, _, _| match e {
            iced::Event::PlatformSpecific(event::PlatformSpecific::Wayland(w)) => match w {
                event::wayland::Event::Output(o, out) => Some(Msg::Output(o, out)),
                _ => None,
            },
            _ => None,
        });
        let keys = event::listen_with(|e, _, _| match e {
            iced::Event::Keyboard(keyboard::Event::KeyPressed { key, .. }) => match key {
                Key::Named(Named::Escape) => Some(Msg::EscPressed),
                // Space triggers Capture / Start record / Stop record —
                // mirrors cosmic-screenshot's hotkey. The wayland keymap in
                // iced translates `XK_space` to `Key::Character(" ")`, so
                // match that here (Named::Space is the cross-platform form
                // some other backends emit).
                Key::Character(s) if s.as_str() == " " => Some(Msg::PrimaryAction(None)),
                _ => None,
            },
            _ => None,
        });
        // Pointer enter/exit per toolbar surface. iced's `listen_with`
        // surfaces a window id alongside each event; we use that to track
        // which output's toolbar the pointer is on so Source::Screen can
        // highlight only that display.
        //
        // We also forward `CursorMoved`: iced/wayland doesn't always emit
        // `CursorEntered` when a freshly-created layer surface appears
        // under an already-on-output pointer (the wl_pointer.enter is
        // sent, but its delivery to iced is racy with our event subscription
        // attaching). Without `CursorMoved` the highlight would stick on
        // the HashMap-order fallback output until the user crossed a
        // display boundary. `Msg::PointerOn` early-returns when the
        // hovered id doesn't change, so the per-frame motion stream
        // doesn't drive any real redraws.
        let pointer = event::listen_with(|e, _, id| match e {
            iced::Event::Mouse(mouse::Event::CursorEntered)
            | iced::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                Some(Msg::PointerOn(Some(id)))
            }
            iced::Event::Mouse(mouse::Event::CursorLeft) => Some(Msg::PointerOn(None)),
            _ => None,
        });
        // Periodic tick during recording/saving so the elapsed-time label
        // refreshes once per second and the saving-stall watchdog has a
        // pulse to check against. Skipped when idle — no subscription at
        // all means no wakeups.
        let tick = if matches!(
            self.capture,
            CaptureState::Recording { .. } | CaptureState::Saving
        ) {
            iced::time::every(std::time::Duration::from_secs(1)).map(|_| Msg::Tick)
        } else {
            Subscription::none()
        };
        // Streaming-thumbnail redraw bridge: the per-tile pump tasks
        // publish into shared frame slots and ping a global mpsc; this
        // subscription forwards each ping as `Msg::ThumbDirty`, which
        // wakes iced just enough to redraw the picker and run the
        // shader widget's `Primitive::prepare` (where the new frame
        // actually gets uploaded into the long-lived wgpu texture).
        let thumbs = Subscription::run(thumb_redraw_stream);
        Subscription::batch([outputs, keys, pointer, tick, thumbs])
    }
}

impl Panel {
    fn locked(&self) -> bool {
        matches!(
            self.capture,
            CaptureState::Recording { .. } | CaptureState::Saving
        )
    }

    fn can_start(&self) -> bool {
        if !matches!(self.capture, CaptureState::Idle) {
            return false;
        }
        match (self.mode, self.source) {
            // Window in both modes: captures fire from clicks on the
            // fan-out picker cards (`Msg::CaptureToplevel`), so the
            // toolbar's primary action stays inactive.
            (_, Source::Window) => false,
            (Mode::Screenshot, _) => true,
            (Mode::Record, Source::Screen) => true,
            (Mode::Record, Source::Region) => self.region.is_some(),
        }
    }

    /// Kick off per-output toplevel capture streams. Each output's
    /// stream replaces its entry in `self.windows` as it completes —
    /// matches xdg-desktop-portal-cosmic's
    /// `interactive_toplevel_images` which gathers per-output snapshots
    /// in parallel via `FuturesUnordered`.
    /// Kick off per-output toplevel capture streams **only for outputs
    /// that don't have entries yet**. Doesn't clear `self.windows` and
    /// doesn't redo captures that have already produced entries —
    /// toggling Region↔Window doesn't imply the toplevel list has
    /// changed, and re-firing was the source of the "flash to
    /// Loading…" the user saw on every toggle. Outputs that go away
    /// drop their entry through `Output::Removed`; new outputs trigger
    /// their own per-output capture from `Output::Created`.
    /// Re-capture window picker thumbnails for every known output,
    /// bypassing the "skip outputs that already have entries"
    /// optimization that `refresh_window_picker` uses. The cached
    /// behavior is right for stray toggles, but for explicit mode/source
    /// entries the cached thumbnails are stale by design — the user is
    /// asking us to show them what each window looks like *now*, not
    /// what it looked like when the toolbar first opened.
    /// Decode (and cover-scale) the path wallpapers of `keys` on a
    /// blocking thread. Outputs sharing a file share one decode — the
    /// common case is the same wallpaper on every display, and a 5K
    /// JPEG is ~100ms to decode. Skips color/gradient sources and
    /// outputs whose handle is already built.
    fn spawn_wallpaper_decode(&self, keys: &[u32]) -> Task<Msg> {
        let mut by_path: HashMap<PathBuf, Vec<(u32, (u32, u32))>> = HashMap::new();
        for &key in keys {
            let Some(info) = self.outputs.get(&key) else {
                continue;
            };
            if info.wallpaper_sub.is_some() {
                continue;
            }
            let Some(cosmic_bg_config::Source::Path(path)) = info.bg_source.clone() else {
                continue;
            };
            by_path.entry(path).or_default().push((key, info.physical_size));
        }
        if by_path.is_empty() {
            return Task::none();
        }
        let tasks: Vec<Task<Msg>> = by_path
            .into_iter()
            .map(|(path, targets)| {
                Task::perform(
                    async move {
                        let keys: Vec<u32> = targets.iter().map(|&(k, _)| k).collect();
                        tokio::task::spawn_blocking(move || decode_wallpaper(&path, &targets))
                            .await
                            .unwrap_or_else(|_| keys.into_iter().map(|k| (k, None)).collect())
                    },
                    |results| cosmic::action::app(Msg::WallpaperDecoded(results)),
                )
            })
            .collect();
        Task::batch(tasks)
    }

    fn force_refresh_window_picker(&mut self) -> Task<Msg> {
        let with_cursor = self.rec_cursor;
        let mut tasks: Vec<Task<Msg>> = Vec::new();
        for info in self.outputs.values() {
            tasks.push(self.spawn_picker_capture(info.name.clone(), with_cursor));
        }
        Task::batch(tasks)
    }

    /// Kick off a per-output toplevel capture stream and deliver the
    /// result through the picker subscription channel — same shape as
    /// xdg-desktop-portal-cosmic's screenshot handler pushing onto its
    /// tokio mpsc (`subscription.rs`). Falls back to a direct
    /// `Task::perform` only if the subscription hasn't yet handed us the
    /// sender (a launch-edge case; the subscription sends
    /// `PickerChannelReady` on its first poll).
    /// Kick off the per-output picker capture and deliver the result via
    /// `Task::perform`. The result message lives inside iced's runtime so
    /// it gets drained on completion without an external wakeup. We
    /// previously tried a tokio::spawn → tokio mpsc → iced subscription
    /// path; the captures completed in ~30ms but the messages sat for
    /// >10s until the next input event because the tokio Waker doesn't
    /// reach iced's wayland event loop (cross-runtime wakeup). The dummy
    /// surface keeps iced warm, so the cold-start concern that drove the
    /// subscription detour no longer applies.
    fn spawn_picker_capture(&self, name: String, with_cursor: bool) -> Task<Msg> {
        let helper = self.helper.clone();
        let name_for_msg = name.clone();
        Task::perform(
            async move {
                capture_toplevels_for_output(&helper, &name, with_cursor).await
            },
            move |entries| {
                cosmic::action::app(Msg::WindowsForOutputReady(
                    name_for_msg.clone(),
                    entries,
                ))
            },
        )
    }

    /// Attach a continuous toplevel-capture pump to an existing
    /// `SharedFrame`. The slot is expected to already be seeded by the
    /// initial picker capture (`capture_toplevels_for_output`), so the
    /// widget always has something to render; this just adds the
    /// stream that pushes fresh frames into the same slot. Records the
    /// underlying `Capture` in `self.streaming_sessions`; dropping the
    /// panel (or stopping all sessions) tears it down. Idempotent per
    /// identifier.
    fn attach_stream_to(&mut self, identifier: &str, shared: Arc<SharedFrame>) {
        if self.streaming_sessions.contains_key(identifier) {
            return;
        }

        // ~30fps cap on the producer; iced still redraws only on the
        // bus pings the pump emits, so the cadence is the slower of
        // (compositor rate, this cap) — i.e. always compositor-paced
        // for typical toplevels.
        let (capture, fmt_rx, mut frame_rx) = match toplevel_capture::start(
            self.helper.clone(),
            identifier.to_string(),
            false,
            30,
        ) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(identifier, error = %e,
                    "streaming_thumb: failed to start toplevel capture");
                return;
            }
        };

        let shared_for_pump = shared.clone();
        // Pump task: wait for the first format announce so we know
        // width/height/stride for each frame, then forward frames into
        // the shared slot. The capture loop pads rows via `stride`, so
        // we must pass that through — the widget's `write_texture` honors
        // it. Exits when the producer drops (Capture dropped → task
        // aborted → fmt_rx / frame_rx return None).
        tokio::spawn(async move {
            let fmt = match fmt_rx.await {
                Ok(f) => f,
                Err(_) => {
                    tracing::debug!("streaming_thumb pump: fmt_rx closed before first frame");
                    return;
                }
            };
            let (width, height, stride) = (fmt.width, fmt.height, fmt.stride);
            let expected = (stride as usize) * (height as usize);
            while let Some(frame) = frame_rx.recv().await {
                // The producer always emits `stride * height` bytes per
                // frame at the negotiated format. A toplevel resize
                // tears the session — we'd see a new fmt on the next
                // start; until that's wired, skip torn frames rather
                // than upload undersized pixels.
                if frame.bytes.len() < expected {
                    tracing::trace!(
                        len = frame.bytes.len(),
                        expected,
                        "streaming_thumb pump: short frame, skipping"
                    );
                    continue;
                }
                shared_for_pump.publish(RawFrame {
                    pixels: frame.bytes,
                    width,
                    height,
                    stride,
                });
                streaming_thumb::nudge_redraw();
            }
            tracing::debug!("streaming_thumb pump: frame_rx closed, exiting");
        });

        self.streaming_sessions
            .insert(identifier.to_string(), capture);
    }

    /// Stop every active toplevel-capture pump. Used when the user
    /// leaves Source::Window (pumps are pure screencopy overhead
    /// off-picker), enters Screenshot mode (Screenshot wants the
    /// frozen view, not live frames), or is about to start a
    /// recording (the recording session needs the toplevel's
    /// screencopy queue to itself). Dropping each `Capture` signals
    /// its loop to stop and aborts the tokio task. Each tile's
    /// `SharedFrame` is left in place — it's still the picker's
    /// render source via `IntrinsicShader`, just frozen on whatever
    /// frame was last published.
    fn stop_all_stream_sessions(&mut self) {
        if self.streaming_sessions.is_empty() {
            return;
        }
        tracing::debug!(
            count = self.streaming_sessions.len(),
            "streaming_thumb: stopping all sessions"
        );
        self.streaming_sessions.clear();
    }

    /// Open a fullscreen toolbar layer surface anchored to the given output if
    /// one isn't already open. Each output gets its own surface so users can
    /// drive capture from whichever monitor they're focused on.
    ///
    /// Toolbars start at `KeyboardInteractivity::Exclusive` so iced's dropdown
    /// overlays receive key events without bouncing focus to apps below.
    /// Multiple Exclusive layer surfaces are allowed by wlr-layer-shell —
    /// cosmic-comp routes focus to whichever surface the pointer is over.
    /// Map every toolbar whose freeze is done, then (once we know it)
    /// the focus catcher on the focused window's output.
    fn map_ready_toolbars(&mut self) -> Task<Msg> {
        // Hold until the pre-map `Activated` snapshot is latched (or
        // its wait expires — `FocusOutputKnown` always arrives): an
        // Exclusive toolbar mapping first would move `Activated` to a
        // window on *its* output and poison the snapshot. The latch
        // usually beats the freeze captures; worst case it adds the
        // remainder of the 300ms cap.
        if self.focus_output.is_none() {
            return Task::none();
        }
        let mut tasks: Vec<Task<Msg>> = Vec::new();
        let keys: Vec<u32> = self.outputs.keys().copied().collect();
        for key in keys {
            let info = &self.outputs[&key];
            if info.freeze_done && info.toolbar_id.is_none() {
                tasks.push(self.ensure_toolbar_for(key));
            }
        }
        // The catcher must be the *last* Exclusive surface to map. If
        // a toolbar mapped after it (late freeze, hotplug), tear the
        // catcher down and re-create it behind the new toolbars.
        if !tasks.is_empty() {
            if let Some(old) = self.focus_catcher_id.take() {
                tasks.push(destroy_layer_surface(old));
            }
        }
        // Chained, not batched: ordering on the wire is the point.
        Task::batch(tasks).chain(self.ensure_focus_catcher())
    }

    /// Focus restoration on exit is decided by cosmic-comp like this:
    /// an `Exclusive` layer surface takes keyboard focus the moment it
    /// maps, and the seat's "focused output" becomes that surface's
    /// output. When the focused surface is destroyed, focus goes to the
    /// top of the focus stack of *that* output. Our toolbars map in
    /// freeze-completion order, so the output that happened to finish
    /// last decided where focus went on quit — usually not the output
    /// the user's window was on, which looks like focus never coming
    /// back. Waiting for toplevel-info before mapping anything would
    /// fix the order but costs ~300ms (cosmic-comp sends the initial
    /// `done` late), so instead: once we know the focused window's
    /// output, map an invisible 1x1 `Exclusive` surface there. It
    /// becomes the focused surface; `destroy_toolbars_ordered` tears it
    /// down last. Key handling is per-app, not per-surface, so the
    /// toolbar hotkeys don't care which of our surfaces has focus.
    fn ensure_focus_catcher(&mut self) -> Task<Msg> {
        if self.focus_catcher_id.is_some() {
            return Task::none();
        }
        // Only once every output's toolbar is up — a toolbar mapping
        // later would take focus away from the catcher.
        if self.outputs.is_empty() || !self.outputs.values().all(|o| o.toolbar_id.is_some()) {
            return Task::none();
        }
        let Some(Some(name)) = self.focus_output.as_ref() else {
            return Task::none();
        };
        let Some(info) = self.outputs.values().find(|o| o.name == *name) else {
            return Task::none();
        };
        let id = window::Id::unique();
        self.focus_catcher_id = Some(id);
        get_layer_surface(SctkLayerSurfaceSettings {
            id,
            layer: Layer::Overlay,
            keyboard_interactivity: KeyboardInteractivity::Exclusive,
            input_zone: Some(Vec::new()),
            anchor: Anchor::empty(),
            output: IcedOutput::Output(info.output.clone()),
            namespace: "cosmic-capture-focus".to_string(),
            size: Some((Some(1), Some(1))),
            exclusive_zone: -1,
            size_limits: Limits::NONE.min_height(1.0).min_width(1.0),
            margin: IcedMargin::default(),
        })
    }

    /// Destroy all toolbars, then the focus catcher (see
    /// `ensure_focus_catcher`). Chained so the requests hit the wire in
    /// that order.
    fn destroy_toolbars_ordered(&mut self) -> Task<Msg> {
        let mut tasks: Vec<Task<Msg>> = Vec::new();
        for info in self.outputs.values_mut() {
            if let Some(id) = info.toolbar_id.take() {
                tasks.push(destroy_layer_surface(id));
            }
        }
        match self.focus_catcher_id.take() {
            Some(id) => Task::batch(tasks).chain(destroy_layer_surface(id)),
            None => Task::batch(tasks),
        }
    }

    fn ensure_toolbar_for(&mut self, key: u32) -> Task<Msg> {
        let Some(info) = self.outputs.get_mut(&key) else {
            return Task::none();
        };
        if info.toolbar_id.is_some() {
            return Task::none();
        }
        // While recording, don't spawn new toolbars for outputs that come
        // online late — they'd appear over recorded apps.
        if matches!(self.capture, CaptureState::Recording { .. }) {
            return Task::none();
        }
        let id = window::Id::unique();
        info.toolbar_id = Some(id);
        get_layer_surface(SctkLayerSurfaceSettings {
            id,
            layer: Layer::Overlay,
            keyboard_interactivity: KeyboardInteractivity::Exclusive,
            input_zone: None,
            anchor: Anchor::all(),
            output: IcedOutput::Output(info.output.clone()),
            namespace: "cosmic-capture-toolbar".to_string(),
            size: Some((None, None)),
            exclusive_zone: -1,
            size_limits: Limits::NONE.min_height(1.0).min_width(1.0),
            margin: IcedMargin::default(),
        })
    }

    /// Open pointer-transparent recording-border surfaces on every output.
    /// These sit at `Layer::Top` (below the toolbar overlays) and just draw
    /// the red selection rect. Clicks pass through because `input_zone` is an
    /// empty region.
    fn open_recording_overlays(&mut self) -> Task<Msg> {
        let mut tasks: Vec<Task<Msg>> = Vec::new();
        for info in self.outputs.values_mut() {
            if let Some(old) = info.recording_id.take() {
                tasks.push(destroy_layer_surface(old));
            }
            let id = window::Id::unique();
            info.recording_id = Some(id);
            tasks.push(get_layer_surface(SctkLayerSurfaceSettings {
                id,
                layer: Layer::Top,
                keyboard_interactivity: KeyboardInteractivity::None,
                input_zone: Some(Vec::new()),
                anchor: Anchor::all(),
                output: IcedOutput::Output(info.output.clone()),
                namespace: "cosmic-capture-recording".to_string(),
                size: Some((None, None)),
                exclusive_zone: -1,
                size_limits: Limits::NONE.min_height(1.0).min_width(1.0),
                margin: IcedMargin::default(),
            }));
        }
        Task::batch(tasks)
    }

    /// Tear down every per-output toolbar surface. Called when recording
    /// starts so cursor motion across the (no-longer-needed) pills can't
    /// trigger hover-state redraws — that's the flicker.
    fn close_toolbars(&mut self) -> Task<Msg> {
        self.destroy_toolbars_ordered()
    }

    /// Open a compact "recording in progress" pill at the bottom-center of
    /// the active output. Fixed size because the wlr-layer-shell protocol
    /// requires an explicit width when the surface is only anchored to a
    /// single edge (`set_size(0, 0)` is only valid when anchored to both
    /// sides on at least one axis) — iced's layer-surface backend calls
    /// `set_size(w.unwrap_or(0), h.unwrap_or(0))` unconditionally, so
    /// `Some((None, None))` here would silently break the configure.
    ///
    /// Keyboard interactivity is `OnDemand`: the surface receives keys only
    /// when the user clicks into it. Default state lets keys flow through to
    /// the apps being recorded, but the pill itself is focusable so Space/
    /// Esc work once the user has interacted with it. (Pure `None` had a
    /// problem where cosmic-comp didn't seem to deliver mouse events to the
    /// surface either, making the Stop button unclickable.)
    fn open_stop_pill(&mut self) -> Task<Msg> {
        if self.stop_pill_id.is_some() {
            return Task::none();
        }
        let id = window::Id::unique();
        self.stop_pill_id = Some(id);
        // Width covers stop + elapsed-time label + cancel button + padding;
        // height covers a single 32px button row with 8/8 vertical padding.
        const PILL_W: u32 = 220;
        const PILL_H: u32 = 56;
        // Pin to the recording display. The earlier "put it on a
        // non-recording output" plan kept the pill out of the captured
        // file, but landed it on an arbitrary HashMap-order display
        // when more than one non-recording output existed — users had
        // to hunt for the pill across monitors. Pinning to the
        // recording display is predictable; the pill being in the
        // saved file is the cost. Falls back to `IcedOutput::Active`
        // if we don't yet have geometry for the recording target.
        let output = self
            .recording_target_output
            .as_deref()
            .and_then(|name| self.outputs.values().find(|o| o.name == name))
            .map(|o| IcedOutput::Output(o.output.clone()))
            .unwrap_or(IcedOutput::Active);
        get_layer_surface(SctkLayerSurfaceSettings {
            id,
            layer: Layer::Overlay,
            keyboard_interactivity: KeyboardInteractivity::OnDemand,
            input_zone: None,
            anchor: Anchor::BOTTOM,
            output,
            namespace: "cosmic-capture-stop-pill".to_string(),
            size: Some((Some(PILL_W), Some(PILL_H))),
            exclusive_zone: -1,
            size_limits: Limits::NONE.min_height(1.0).min_width(1.0),
            margin: IcedMargin {
                top: 0,
                right: 0,
                bottom: TOOLBAR_BOTTOM_MARGIN as i32,
                left: 0,
            },
        })
    }

    /// Destroy every surface we own and exit the iced runtime. Used by both
    /// the explicit close (`Msg::Quit`, Esc-when-idle) and pipeline-completion
    /// paths.
    fn shutdown(&mut self) -> Task<Msg> {
        let mut tasks: Vec<Task<Msg>> = Vec::new();
        if let Some(id) = self.stop_pill_id.take() {
            tasks.push(destroy_layer_surface(id));
        }
        for info in self.outputs.values_mut() {
            if let Some(id) = info.recording_id.take() {
                tasks.push(destroy_layer_surface(id));
            }
        }
        tasks.push(self.destroy_toolbars_ordered());
        tasks.push(iced::exit());
        Task::batch(tasks)
    }

/// Write a single field to cosmic-config. We bypass the derived
    /// `set_<field>` helpers from `CosmicConfigEntry` because their
    /// "if self.x != value, write" diff is meaningless when called on a
    /// transient snapshot: we already built the snapshot from the caller's
    /// up-to-date state, so the diff is always false and nothing lands on
    /// disk. Callers filter at the message-handler level already, so we go
    /// straight to `ConfigSet::set` and trust them.
    fn persist<T: serde::Serialize>(&self, key: &str, value: &T) {
        use cosmic_config::ConfigSet;
        let Some(config) = self.config.as_ref() else {
            return;
        };
        if let Err(e) = config.set(key, value) {
            tracing::warn!(key, error = %e, "cosmic-config write failed");
        }
    }

    /// Stitch frozen captures from every output that intersects `region`
    /// into a single image cropped to the region's bounds. Returns
    /// `None` if no output overlaps or none of them have frozen frames
    /// available. Used to make region screenshots work when the region
    /// straddles a monitor boundary (cosmic-screenshot supports this;
    /// cosmic-screencopy only delivers per-output captures, so we have
    /// to composite ourselves).
    ///
    /// Mixed-scale handling: pick the largest scale among overlapping
    /// outputs as the target and upscale lower-scale outputs via
    /// nearest-neighbor. Quality compromise vs. the work of a proper
    /// resampler, but fine for screenshots that are typically saved at
    /// 1:1 and viewed at the original size.
    fn stitch_region_screenshot(&self) -> Option<CapturedFrame> {
        let layout = self.compute_region_parts()?;
        let canvas_w = layout.canvas_w;
        let canvas_h = layout.canvas_h;
        let canvas_stride = canvas_w.saturating_mul(4);
        let mut canvas = vec![0u8; (canvas_stride as usize) * (canvas_h as usize)];
        let mut copied_parts = 0usize;
        let mut copied_pixels = 0usize;
        for part in &layout.parts {
            let Some(frame) = self
                .outputs
                .values()
                .find(|o| o.name == part.output_name)
                .and_then(|o| o.frozen.as_ref())
            else {
                tracing::warn!(
                    output = %part.output_name,
                    "stitch_region_screenshot: no frozen frame for output, skipping"
                );
                continue;
            };
            let src_x = part.src_crop.x as u32;
            let src_y = part.src_crop.y as u32;
            let src_w = part.src_crop.w;
            let src_h = part.src_crop.h;
            let dst_x = part.dst_pos.0;
            let dst_y = part.dst_pos.1;
            let dst_w = part.dst_size.0;
            let dst_h = part.dst_size.1;
            if dst_w == 0 || dst_h == 0 || src_w == 0 || src_h == 0 {
                continue;
            }
            let before = copied_pixels;
            for y in 0..dst_h {
                let sy = src_y + ((y as u64 * src_h as u64) / dst_h as u64) as u32;
                let src_row_off = (sy as usize) * (frame.stride as usize);
                let dst_row_off =
                    ((dst_y + y) as usize) * (canvas_stride as usize) + (dst_x as usize) * 4;
                for x in 0..dst_w {
                    let sx = src_x + ((x as u64 * src_w as u64) / dst_w as u64) as u32;
                    let src_off = src_row_off + (sx as usize) * 4;
                    let dst_off = dst_row_off + (x as usize) * 4;
                    if src_off + 4 <= frame.pixels.len() && dst_off + 4 <= canvas.len() {
                        canvas[dst_off..dst_off + 4]
                            .copy_from_slice(&frame.pixels[src_off..src_off + 4]);
                        copied_pixels += 1;
                    }
                }
            }
            if copied_pixels > before {
                copied_parts += 1;
            }
        }
        if copied_parts == 0 {
            // All parts skipped (no frozen frames yet, or every per-pixel
            // bounds check failed). Saving the all-zero canvas as a PNG
            // would produce a blank/garbage image — better to fail
            // upward and let the caller log + abort the save.
            tracing::warn!(
                parts = layout.parts.len(),
                "stitch_region_screenshot: no parts copied, aborting"
            );
            return None;
        }
        Some(CapturedFrame {
            pixels: canvas,
            width: canvas_w,
            height: canvas_h,
            stride: canvas_stride,
            shm_fd: None,
        })
    }

    /// Geometry of one region recording / screenshot. Each `RegionPart`
    /// names an overlapping output, the rectangle within its physical
    /// pixel grid that contributes to the canvas, and the target
    /// position+size on the canvas. The canvas itself is
    /// `region × target_scale` pixels, where `target_scale =
    /// max(overlap.scale)` so mixed-DPI layouts produce a single uniform
    /// canvas resolution.
    ///
    /// Shared between `stitch_region_screenshot` (CPU blit) and
    /// `pipeline::record::record_via_screencopy_multi` (GPU composite)
    /// so the two paths can't drift on geometry.
    fn compute_region_parts(&self) -> Option<RegionLayout> {
        let region = self.region?.normalize();
        if region.width() <= 0 || region.height() <= 0 {
            return None;
        }
        let overlapping: Vec<&OutputInfo> = self
            .outputs
            .values()
            .filter(|info| {
                let r_left = info.logical_pos.0;
                let r_top = info.logical_pos.1;
                let r_right = r_left + info.logical_size.0 as i32;
                let r_bottom = r_top + info.logical_size.1 as i32;
                region.left < r_right
                    && region.right > r_left
                    && region.top < r_bottom
                    && region.bottom > r_top
            })
            .collect();
        if overlapping.is_empty() {
            return None;
        }
        // Canvas density = the densest overlapping output, measured as
        // a real physical/logical ratio (fractional scales included) so
        // that output's pixels land 1:1 and lower-density outputs get
        // upscaled rather than the reverse.
        let target_scale = overlapping
            .iter()
            .map(|o| {
                let (sx, sy) = o.scale_xy();
                sx.max(sy)
            })
            .fold(1.0_f64, f64::max);
        let to_canvas = |logical: i32| -> i64 { ((logical as f64) * target_scale).round() as i64 };
        let canvas_w = (to_canvas(region.right) - to_canvas(region.left)).max(0) as u32;
        let canvas_h = (to_canvas(region.bottom) - to_canvas(region.top)).max(0) as u32;
        if canvas_w == 0 || canvas_h == 0 {
            return None;
        }
        let mut parts: Vec<pipeline::record::RegionPart> = Vec::with_capacity(overlapping.len());
        for info in overlapping {
            let out = output_rect_of(info);
            let ix_l = region.left.max(out.left);
            let iy_t = region.top.max(out.top);
            let ix_r = region.right.min(out.right);
            let iy_b = region.bottom.min(out.bottom);
            let Some(src_crop) = info.crop_for_region(region) else {
                continue;
            };
            // Destination edges are rounded independently (same as the
            // source crop) so neighbouring parts tile without a seam.
            let dst_l = (to_canvas(ix_l) - to_canvas(region.left)).clamp(0, canvas_w as i64);
            let dst_t = (to_canvas(iy_t) - to_canvas(region.top)).clamp(0, canvas_h as i64);
            let dst_r = (to_canvas(ix_r) - to_canvas(region.left)).clamp(dst_l, canvas_w as i64);
            let dst_b = (to_canvas(iy_b) - to_canvas(region.top)).clamp(dst_t, canvas_h as i64);
            let dst_w = (dst_r - dst_l) as u32;
            let dst_h = (dst_b - dst_t) as u32;
            if dst_w == 0 || dst_h == 0 {
                continue;
            }
            parts.push(pipeline::record::RegionPart {
                output_name: info.name.clone(),
                src_crop,
                dst_pos: (dst_l as u32, dst_t as u32),
                dst_size: (dst_w, dst_h),
            });
        }
        if parts.is_empty() {
            return None;
        }
        Some(RegionLayout {
            canvas_w,
            canvas_h,
            target_scale,
            parts,
        })
    }

    /// Count how many known outputs the current region overlaps.
    /// `0` means no region or off-screen; `1` is the fast single-output
    /// path; `>1` means we need to stitch.
    fn region_output_count(&self) -> usize {
        let Some(region) = self.region else {
            return 0;
        };
        let region = region.normalize();
        if region.width() <= 0 || region.height() <= 0 {
            return 0;
        }
        self.outputs
            .values()
            .filter(|o| {
                let r_left = o.logical_pos.0;
                let r_top = o.logical_pos.1;
                let r_right = r_left + o.logical_size.0 as i32;
                let r_bottom = r_top + o.logical_size.1 as i32;
                region.left < r_right
                    && region.right > r_left
                    && region.top < r_bottom
                    && region.bottom > r_top
            })
            .count()
    }

    /// Resolve the current `self.region` to the output it sits on plus
    /// the crop rect expressed in that output's physical pixels. Used by
    /// the screencopy-based Record path so we can pin recording to a
    /// specific monitor rather than going through the screencast portal
    /// (which silently substitutes whichever output the user picked
    /// first via its restore-token).
    fn region_output_and_crop(&self) -> Option<(String, CropRect)> {
        let region = self.region?.normalize();
        let cx = (region.left + region.right) / 2;
        let cy = (region.top + region.bottom) / 2;
        let info = self.outputs.values().find(|o| {
            let r = output_rect_of(o);
            cx >= r.left && cx < r.right && cy >= r.top && cy < r.bottom
        })?;
        let crop = info.crop_for_region(region)?;
        Some((info.name.clone(), crop))
    }

    /// Compute the crop that excludes the on-screen recording border
    /// from a Display recording on `output_name`. Returns `None` if we
    /// don't have geometry for the output yet, or if the inset would
    /// collapse the recording to zero pixels. Toplevel (window)
    /// recordings don't need this — the border layer surface isn't
    /// part of the toplevel's surface tree, so cosmic-screencopy
    /// doesn't include it when capturing a Toplevel.
    fn screen_border_crop(&self, output_name: &str) -> Option<CropRect> {
        let info = self.outputs.values().find(|o| o.name == output_name)?;
        let (phys_w, phys_h) = info.physical_size;
        let inset = SCREEN_CAPTURE_BORDER_INSET;
        if phys_w <= inset * 2 || phys_h <= inset * 2 {
            return None;
        }
        Some(CropRect {
            x: inset as i32,
            y: inset as i32,
            w: phys_w - inset * 2,
            h: phys_h - inset * 2,
        })
    }

    fn crop_from_region(&self) -> Option<CropRect> {
        self.region_output_and_crop().map(|(_, crop)| crop)
    }

    /// Translate the current `save_target` into a screenshot pipeline
    /// `Destination`. Shared between the region/screen capture path and the
    /// per-window capture path.
    fn current_destination(&self) -> pipeline::screenshot::Destination {
        match self.save_target {
            SaveTarget::Clipboard => pipeline::screenshot::Destination::Clipboard,
            SaveTarget::Pictures => pipeline::screenshot::Destination::File(None),
            SaveTarget::Documents => {
                let dest = dirs::document_dir().map(|d| {
                    let stem = chrono::Local::now().format("Screenshot-%Y-%m-%d_%H-%M-%S");
                    d.join(format!("{stem}.png"))
                });
                pipeline::screenshot::Destination::File(dest)
            }
        }
    }

    fn on_primary_action(&mut self, clicked_output: Option<String>) -> Task<Msg> {
        match &mut self.capture {
            CaptureState::Recording { stop_tx, .. } => {
                if let Some(tx) = stop_tx.take() {
                    match tx.send(()) {
                        Ok(()) => tracing::info!(
                            "stop_tx sent — VideoSession::run should now wake"
                        ),
                        Err(()) => tracing::warn!(
                            "stop_tx send failed — receiver was dropped, \
                             pipeline future already exited?"
                        ),
                    }
                } else {
                    tracing::warn!(
                        "stop click but stop_tx is None — already sent? \
                         or gif path forgot to wire one in?"
                    );
                }
                self.capture = CaptureState::Saving;
                self.saving_started_at = Some(std::time::Instant::now());
                Task::none()
            }
            CaptureState::Idle => match self.mode {
                Mode::Screenshot => {
                    let destination = self.current_destination();
                    let notify_user = self.notify;
                    // Window source has no toolbar primary action — capture
                    // is driven by the user clicking a card in the fan-out
                    // picker (see `view_window_picker`), which fires
                    // `Msg::CaptureToplevel` directly.
                    if matches!(self.source, Source::Window) {
                        return Task::none();
                    }
                    // Cross-screen region screenshot: when the region spans
                    // more than one output we can't get the whole image
                    // from any single output's frozen capture. Stitch
                    // every overlapping output's frozen frame into one
                    // canvas sized to the region, then save that.
                    // single-output regions and Screen/Window keep the
                    // existing single-output path (faster, zero-copy
                    // via the encoder's crop).
                    if matches!(self.source, Source::Region)
                        && self.region_output_count() >= 2
                    {
                        let Some(frame) = self.stitch_region_screenshot() else {
                            tracing::warn!(
                                "cross-screen region screenshot: no overlapping \
                                 frozen frames available yet"
                            );
                            return Task::none();
                        };
                        self.capture = CaptureState::Saving;
                        return Task::perform(
                            run_save_frame(frame, None, destination, notify_user),
                            |r| cosmic::action::app(Msg::ScreenshotFinished(r)),
                        );
                    }
                    let Some(output_name) = self.active_output_name(clicked_output.as_deref())
                    else {
                        tracing::warn!("no output detected; can't screenshot");
                        return Task::none();
                    };
                    let crop = match self.source {
                        Source::Region => self.crop_from_region(),
                        Source::Screen | Source::Window => None,
                    };
                    // Prefer the frame captured at toolbar-open time so the
                    // saved screenshot matches what the user is looking at
                    // (the frozen background). If freeze hadn't completed
                    // yet — Capture clicked before the capture task
                    // returned — fall back to a fresh live screencopy.
                    let frozen = self
                        .outputs
                        .values()
                        .find(|o| o.name == output_name)
                        .and_then(|o| o.frozen.as_ref())
                        .map(|f| (**f).clone());
                    self.capture = CaptureState::Saving;
                    let cursor = self.rec_cursor;
                    if let Some(frame) = frozen {
                        Task::perform(
                            run_save_frame(frame, crop, destination, notify_user),
                            |r| cosmic::action::app(Msg::ScreenshotFinished(r)),
                        )
                    } else {
                        Task::perform(
                            run_screenshot(output_name, cursor, crop, destination, notify_user),
                            |r| cosmic::action::app(Msg::ScreenshotFinished(r)),
                        )
                    }
                }
                Mode::Record => {
                    if !self.can_start() {
                        return Task::none();
                    }
                    // Cross-screen region recording branches before the
                    // single-output (target, crop) match below: we don't
                    // have a single ScreencopyTarget — we have N, one per
                    // overlapping output, composited on the GPU via
                    // `record_via_screencopy_multi`. GIF on cross-screen
                    // isn't supported (animated GIF assembly is sysmem-
                    // path only); we abort cleanly in that combo.
                    if matches!(self.source, Source::Region)
                        && self.region_output_count() >= 2
                    {
                        let Some(layout) = self.compute_region_parts() else {
                            tracing::warn!(
                                "cross-screen region: compute_region_parts returned None"
                            );
                            return Task::none();
                        };
                        if matches!(self.record_format, RecordFormat::Gif) {
                            tracing::warn!(
                                "GIF doesn't support cross-screen region recording; \
                                 aborting"
                            );
                            return Task::none();
                        }
                        // Pin the stop pill to the first overlapping
                        // output — predictable and visible. (Same UX
                        // trade-off as single-output: pill lands in the
                        // recording.)
                        self.recording_target_output =
                            layout.parts.first().map(|p| p.output_name.clone());
                        let close_bars = self.close_toolbars();
                        let stop_pill = self.open_stop_pill();
                        let swap = self.open_recording_overlays();
                        let release_kb = Task::none();
                        let helper = self.helper.clone();
                        let args = self.build_record_args();
                        let (stop_tx, stop_rx) = oneshot::channel();
                        self.capture = CaptureState::Recording {
                            stop_tx: Some(stop_tx),
                        };
                        self.recording_started_at = Some(std::time::Instant::now());
                        return Task::batch([
                            close_bars,
                            stop_pill,
                            swap,
                            release_kb,
                            Task::perform(
                                run_record_screencopy_multi(
                                    helper,
                                    layout.parts,
                                    args,
                                    stop_rx,
                                ),
                                |r| cosmic::action::app(Msg::RecordingFinished(r)),
                            ),
                        ]);
                    }
                    // Pick the target monitor + optional crop. For Region,
                    // crop_from_region already lands on the output whose
                    // logical rect contains the region's center and returns
                    // the crop in *that* output's physical-pixel space — so
                    // we just need to look up its name here. For Screen, use
                    // the same active_output_name selector that the
                    // screenshot path uses.
                    let (target, crop) = match self.source {
                        Source::Region => {
                            let Some((name, crop)) = self.region_output_and_crop() else {
                                tracing::warn!(
                                    "Region selected but no output contains it; \
                                     aborting record start"
                                );
                                return Task::none();
                            };
                            self.recording_target_output = Some(name.clone());
                            (ScreencopyTarget::Output { output_name: name }, Some(crop))
                        }
                        Source::Screen => {
                            let Some(name) = self.active_output_name(clicked_output.as_deref())
                            else {
                                tracing::warn!("no output detected; can't record");
                                return Task::none();
                            };
                            self.recording_target_output = Some(name.clone());
                            // Crop SCREEN_CAPTURE_BORDER_INSET pixels off
                            // each edge so the red recording border
                            // (drawn flush against the output edge) is
                            // excluded from the saved file. Output
                            // screencopy captures everything composited
                            // to the output, including our layer-shell
                            // overlay, so the only way to keep the
                            // border out of the recording is to record a
                            // slightly smaller area than the full
                            // output.
                            let crop = self.screen_border_crop(&name);
                            (ScreencopyTarget::Output { output_name: name }, crop)
                        }
                        // can_start() rules out Source::Window here — the
                        // window picker drives CaptureToplevel directly.
                        Source::Window => return Task::none(),
                    };
                    // Recording switchover: tear down the per-output
                    // toolbars (their hover-state redraws were flickering)
                    // and replace with a compact bottom stop pill + the
                    // pointer-transparent recording border overlays.
                    let close_bars = self.close_toolbars();
                    let stop_pill = self.open_stop_pill();
                    let swap = self.open_recording_overlays();
                    let release_kb = Task::none();
                    let helper = self.helper.clone();
                    match self.record_format {
                        RecordFormat::Gif => {
                            let args = self.build_gif_args();
                            let (stop_tx, stop_rx) = oneshot::channel();
                            self.capture = CaptureState::Recording {
                                stop_tx: Some(stop_tx),
                            };
                            self.recording_started_at = Some(std::time::Instant::now());
                            Task::batch([
                                close_bars,
                                stop_pill,
                                swap,
                                release_kb,
                                Task::perform(
                                    run_gif_screencopy(helper, target, args, crop, stop_rx),
                                    |r| cosmic::action::app(Msg::GifFinished(r)),
                                ),
                            ])
                        }
                        _ => {
                            let args = self.build_record_args();
                            let (stop_tx, stop_rx) = oneshot::channel();
                            self.capture = CaptureState::Recording {
                                stop_tx: Some(stop_tx),
                            };
                            self.recording_started_at = Some(std::time::Instant::now());
                            Task::batch([
                                close_bars,
                                stop_pill,
                                swap,
                                release_kb,
                                Task::perform(
                                    run_record_screencopy(helper, target, args, crop, stop_rx),
                                    |r| cosmic::action::app(Msg::RecordingFinished(r)),
                                ),
                            ])
                        }
                    }
                }
            },
            CaptureState::Saving => Task::none(),
        }
    }

    /// Fan-out window picker — horizontal row of toplevel thumbnails, each
    /// tile sized proportionally to the source window's pixel width. Click
    /// a tile to capture that window. Mirrors xdg-desktop-portal-cosmic's
    /// screenshot widget (`ScreenshotSelection` in Window choice mode):
    /// `row(FillPortion(window_width/total_width)).align_y(Center)`.
    ///
    /// Renders only the toplevels the `WaylandHelper` reported sitting
    /// on this output — same scoping the portal applies via its
    /// `output_toplevels` map.
    fn view_window_picker(&self, info: &OutputInfo) -> Element<'_, Msg> {
        let on_this_output: &[WindowEntry] = self
            .windows
            .get(&info.name)
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        if on_this_output.is_empty() {
            let label = if self.windows.contains_key(&info.name) {
                "No windows on this display."
            } else {
                "Loading windows…"
            };
            return container(cosmic::widget::text::body(label))
                .padding(24)
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(iced::Alignment::Center)
                .align_y(iced::Alignment::Center)
                .into();
        }

        // Direct port of xdg-desktop-portal-cosmic's `ScreenshotSelection`
        // Window-choice branch in `widget/screenshot.rs`. The portal sets
        // `content_fit(ScaleDown)` on the image and `width(FillPortion(..))`
        // / `height(Shrink)` on the *container* (not the image), with a
        // `layer_container` wrapper that paints the toolbar's component bg
        // behind each tile.
        let total_width: u64 = on_this_output.iter().map(|w| w.width.max(1) as u64).sum();
        let total_width = total_width.max(1);

        let img_buttons: Vec<Element<'_, Msg>> = on_this_output
            .iter()
            .map(|entry| {
                let portion = ((entry.width.max(1) as u64 * u16::MAX as u64) / total_width)
                    .max(1) as u16;
                let id = entry.identifier.clone();
                // Always `IntrinsicShader`, in both modes. The
                // `SharedFrame` is seeded with the initial capture at
                // `WindowEntry` construction time so the widget has
                // pixels to render from the first frame; Record mode
                // additionally attaches a stream pump that publishes
                // fresh frames into the same slot. Using one render
                // path for both modes avoids iced's image-atlas
                // first-frame upload race that produces the flash
                // when fresh `Handle::from_rgba` thumbs land in
                // Screenshot mode.
                let inner: Element<'_, Msg> = match entry.streaming.clone() {
                    Some(shared) => IntrinsicShader::new(
                        shared,
                        entry.frozen.width,
                        entry.frozen.height,
                    )
                    .into(),
                    None => cosmic::iced::widget::image(entry.thumb.clone())
                        .content_fit(cosmic::iced::ContentFit::ScaleDown)
                        .into(),
                };
                cosmic::widget::layer_container(
                    button::custom(inner)
                        .on_press(Msg::CaptureToplevel(id))
                        .class(cosmic::theme::Button::Image),
                )
                .align_x(iced::Alignment::Center)
                .width(Length::FillPortion(portion))
                .height(Length::Shrink)
                .into()
            })
            .collect();

        cosmic::widget::layer_container(
            iced::widget::Row::with_children(img_buttons)
                .spacing(24)
                .width(Length::Fill)
                .align_y(iced::Alignment::Center)
                .padding(24),
        )
        .align_x(iced::Alignment::Center)
        .align_y(iced::Alignment::Center)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    /// Build the wallpaper background element for the Window-mode picker.
    /// Same shape as xdg-desktop-portal-cosmic's `bg_element` match in
    /// `widget/screenshot.rs`: a `Path` becomes a cover-fit Image; a
    /// `Color::Single` becomes a solid-filled container; a
    /// `Color::Gradient` becomes a linear-gradient container at the
    /// configured radius. Returns `None` when no bg source is available
    /// (cosmic-bg not running) so the caller can fall back to the freeze.
    fn window_picker_bg<'a>(&self, info: &OutputInfo) -> Option<Element<'a, Msg>> {
        use cosmic::iced::core::gradient::Linear;
        use cosmic::iced::Degrees;
        use cosmic_bg_config::{Color, Source};

        let source = info.bg_source.as_ref()?;
        let element: Element<'_, Msg> = match source {
            // Already cover-scaled to the output in `decode_wallpaper`,
            // so `Fill` is exact. Sits below the toolbar surface like
            // the freeze; the tiles draw in the parent above it.
            Source::Path(_) => Element::new(
                Subsurface::new(info.wallpaper_sub.clone()?)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .content_fit(cosmic::iced::ContentFit::Fill)
                    .z(-1),
            ),
            Source::Color(color) => {
                let color = color.clone();
                container(iced::widget::Space::new())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .class(cosmic::theme::Container::Custom(Box::new(move |_| {
                        let bg = match color.clone() {
                            Color::Single(c) => Background::Color(
                                cosmic::iced::Color::from_rgba(c[0], c[1], c[2], 1.0),
                            ),
                            Color::Gradient(g) => {
                                let stops = g.colors.len().max(1);
                                let stop_step = 1.0 / (stops.saturating_sub(1).max(1) as f32);
                                let mut linear = Linear::new(Degrees(g.radius));
                                let mut t = 0.0;
                                for &[r, gc, b] in g.colors.iter() {
                                    linear = linear.add_stop(
                                        t,
                                        cosmic::iced::Color::from_rgb(r, gc, b),
                                    );
                                    t += stop_step;
                                }
                                Background::Gradient(cosmic::iced::core::Gradient::Linear(linear))
                            }
                        };
                        cosmic::iced::widget::container::Style {
                            background: Some(bg),
                            ..Default::default()
                        }
                    })))
                    .into()
            }
        };
        Some(element)
    }

    /// Compact pill shown while a recording is in progress. Just the Stop
    /// button (which forwards to `PrimaryAction`) and a Quit icon for
    /// cancel-with-discard via the existing Esc path.
    fn view_stop_pill(&self) -> Element<'_, Msg> {
        // Upgrade to info so it shows under the default RUST_LOG filter —
        // helps diagnose "stop button doesn't work" reports because we can
        // see (a) whether the pill is being rendered at all, (b) what
        // capture state it sees, and (c) by inference whether the click is
        // even reaching iced (compare with on_primary_action's log).
        tracing::info!(state = ?self.capture, has_pill_id = self.stop_pill_id.is_some(),
            "rendering stop pill");
        let press_stop = matches!(self.capture, CaptureState::Recording { .. })
            .then_some(Msg::PrimaryAction(None));
        let stop = record_button(&self.capture, press_stop.is_some(), press_stop);
        let cancel = button::icon(icon::from_name("window-close-symbolic"))
            .medium()
            .on_press(Msg::EscPressed);

        let elapsed = self
            .recording_started_at
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        let label = format_elapsed(elapsed);
        let timer = cosmic::widget::text::body(label);

        // Mirror the main toolbar pill's separator exactly — same 2px
        // rule, same LightDivider class, same 56px height — so the
        // stop pill reads as a member of the same UI family rather
        // than a one-off chip.
        let sep = || {
            iced::widget::rule::vertical(2)
                .class(cosmic::theme::Rule::LightDivider)
                .height(Length::Fixed(56.0))
        };
        let row = row::with_capacity(5)
            .push(stop)
            .push(sep())
            .push(timer)
            .push(sep())
            .push(cancel)
            .spacing(10)
            .align_y(iced::Alignment::Center);

        container(row)
            .padding([8, 12, 8, 12])
            .class(cosmic::theme::Container::Custom(Box::new(|theme| {
                let t = theme.cosmic();
                cosmic::iced::widget::container::Style {
                    background: Some(Background::Color(t.background.component.base.into())),
                    text_color: Some(t.background.component.on.into()),
                    border: Border {
                        radius: t.corner_radii.radius_s.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }
            })))
            .into()
    }

    fn view_toolbar(&self, info: &OutputInfo) -> Element<'_, Msg> {
        // All iconic buttons share `.medium()` sizing for a uniform row.
        // `Button::IconVertical` (as opposed to plain `Button::Icon`) renders
        // a lighter-background overlay on `.selected()`, which is the visible
        // mode/source indicator. Plain `Icon` only retints the glyph, which
        // is too subtle on symbolic icons against the toolbar's component bg.
        let mode_icon = |name: &'static str, m: Mode| {
            let press = (!self.locked()).then_some(Msg::SetMode(m));
            button::icon(icon::from_name(name))
                .medium()
                .selected(self.mode == m)
                .class(cosmic::theme::Button::IconVertical)
                .on_press_maybe(press)
        };
        let modes = row::with_capacity(2)
            .push(mode_icon("camera-photo-symbolic", Mode::Screenshot))
            .push(mode_icon("camera-video-symbolic", Mode::Record))
            .spacing(4)
            .align_y(iced::Alignment::Center);

        let source_icon = |name: &'static str, s: Source, on_press: Option<Msg>| {
            button::icon(icon::from_name(name))
                .medium()
                .selected(self.source == s)
                .class(cosmic::theme::Button::IconVertical)
                .on_press_maybe(on_press)
        };
        let region_press = (!self.locked()).then_some(Msg::SetSource(Source::Region));
        let screen_press = (!self.locked()).then_some(Msg::SetSource(Source::Screen));
        // Window source works in both modes. In Screenshot mode we show our
        // own toplevel picker; in Record mode the screencast portal dialog
        // already advertises Window as a source type so picking it there
        // routes the PipeWire stream through unchanged.
        let window_press = (!self.locked()).then_some(Msg::SetSource(Source::Window));
        let sources = row::with_capacity(3)
            .push(source_icon(
                "screenshot-selection-symbolic",
                Source::Region,
                region_press,
            ))
            .push(source_icon(
                "screenshot-window-symbolic",
                Source::Window,
                window_press,
            ))
            .push(source_icon(
                "screenshot-screen-symbolic",
                Source::Screen,
                screen_press,
            ))
            .spacing(4)
            .align_y(iced::Alignment::Center);

        // Primary action.
        let (action_label, action_kind) = match &self.capture {
            CaptureState::Idle => (self.idle_action_label(), ActionKind::Normal),
            CaptureState::Recording { stop_tx, .. } => (
                String::new(),
                if stop_tx.is_some() {
                    ActionKind::Normal
                } else {
                    ActionKind::Disabled
                },
            ),
            CaptureState::Saving => ("Saving…".to_string(), ActionKind::Disabled),
        };
        let action_enabled = match action_kind {
            ActionKind::Disabled => false,
            ActionKind::Normal => self.can_start()
                || matches!(self.capture, CaptureState::Recording { .. }),
        };
        // Carry the toolbar surface's output name with the click so
        // Source::Screen captures whichever display the user actually
        // pressed Capture on, not a HashMap-iteration-order fallback.
        let press = action_enabled.then_some(Msg::PrimaryAction(Some(info.name.clone())));
        let action: Element<'_, Msg> = match self.mode {
            Mode::Record => record_button(&self.capture, action_enabled, press).into(),
            // Screenshot capture: grey "Capture" button (not suggested green).
            Mode::Screenshot => button::standard(action_label)
                .on_press_maybe(press)
                .into(),
        };

        // Plain `dropdown` (not `popup_dropdown`) — the menu renders as an
        // iced overlay inside this same fullscreen surface, which is how
        // cosmic-screenshot does it. `popup_dropdown` spawns a nested
        // xdg-popup that fights the toolbar's Exclusive keyboard grab and
        // gets dismissed immediately.
        let options: Element<'_, Msg> = match self.mode {
            Mode::Screenshot => {
                let selected = SAVE_TARGETS.iter().position(|t| *t == self.save_target);
                dropdown(save_target_labels(), selected, Msg::SetSaveTarget).into()
            }
            Mode::Record => {
                let selected = RECORD_FORMATS.iter().position(|f| *f == self.record_format);
                dropdown(record_format_labels(), selected, Msg::SetRecordFormat).into()
            }
        };

        // FPS selector — record mode only. Standalone dropdown so it's a
        // single click away from the toolbar, not buried in an options popup.
        let fps_dropdown: Option<Element<'_, Msg>> = match self.mode {
            Mode::Record => {
                let selected = RECORD_FPS.iter().position(|f| *f == self.record_fps);
                Some(dropdown(record_fps_labels(), selected, Msg::SetRecordFps).into())
            }
            Mode::Screenshot => None,
        };

        // Audio cluster — record mode only, and never in GIF mode (the
        // container can't carry an audio stream). Mic + system are two
        // independent toggles; both default off. When "on" we swap class
        // from IconVertical to Suggested so the toggle reads green/accent
        // and stands out against the otherwise grey toolbar pill.
        let audio_supported = matches!(self.mode, Mode::Record)
            && !matches!(self.record_format, RecordFormat::Gif);
        let audio_cluster: Option<Element<'_, Msg>> = if audio_supported {
            let audio_button = |icon_name: &'static str, on: bool, msg: Msg| {
                let class = if on {
                    cosmic::theme::Button::Suggested
                } else {
                    cosmic::theme::Button::IconVertical
                };
                button::icon(icon::from_name(icon_name))
                    .medium()
                    .selected(on)
                    .class(class)
                    .on_press_maybe((!self.locked()).then_some(msg))
            };
            Some(
                row::with_capacity(2)
                    .push(audio_button(
                        "audio-input-microphone-symbolic",
                        self.rec_audio_mic,
                        Msg::ToggleAudioMic,
                    ))
                    .push(audio_button(
                        "audio-speakers-symbolic",
                        self.rec_audio_system,
                        Msg::ToggleAudioSystem,
                    ))
                    .spacing(4)
                    .align_y(iced::Alignment::Center)
                    .into(),
            )
        } else {
            None
        };

        let close = button::icon(icon::from_name("window-close-symbolic"))
            .medium()
            .on_press(Msg::Quit);

        // 2px separators that span nearly the full pill height.
        let sep = || {
            iced::widget::rule::vertical(2)
                .class(cosmic::theme::Rule::LightDivider)
                .height(Length::Fixed(56.0))
        };

        let mut pill = row::with_capacity(11)
            .push(modes)
            .push(sep())
            .push(sources)
            .push(sep())
            .push(action);
        if let Some(fps) = fps_dropdown {
            pill = pill.push(sep()).push(fps);
        }
        let mut pill = pill
            .push(sep())
            .push(options);
        if let Some(audio) = audio_cluster {
            pill = pill.push(sep()).push(audio);
        }
        let pill = pill
            .push(sep())
            .push(close)
            .spacing(10)
            .align_y(iced::Alignment::Center);

        // Styled pill background — matches xdg-desktop-portal-cosmic's
        // screenshot toolbar: opaque component bg with small corner radius.
        // Width fixed so the pill stays compact even though it lives inside
        // a fullscreen surface. While recording, the pill drops to 30%
        // opacity so it doesn't dominate the captured frame; the user can
        // still see it well enough to click Stop.
        let recording = matches!(self.capture, CaptureState::Recording { .. });
        let pill_alpha: f32 = if recording { 0.30 } else { 1.0 };
        let styled = container(pill)
            .padding([8, 12, 8, 12])
            .width(Length::Shrink)
            .class(cosmic::theme::Container::Custom(Box::new(move |theme| {
                let t = theme.cosmic();
                let mut bg: Color = t.background.component.base.into();
                bg.a *= pill_alpha;
                let mut fg: Color = t.background.component.on.into();
                fg.a *= pill_alpha;
                cosmic::iced::widget::container::Style {
                    background: Some(Background::Color(bg)),
                    text_color: Some(fg),
                    border: Border {
                        radius: t.corner_radii.radius_s.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }
            })));

        // Pill container — fills the surface width/height transparently and
        // pins the styled chip to the bottom-center.
        let pill_layer = container(styled)
            .padding([0, 0, TOOLBAR_BOTTOM_MARGIN, 0])
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(iced::Alignment::Center)
            .align_y(iced::Alignment::End);

        // Compose extra layers (selection widget, fullscreen border) under
        // the pill via iced Stack. iced routes pointer events top-down: the
        // pill (front) absorbs clicks on its bounds, drags anywhere else
        // fall through to whatever sits below.
        let pre_capture = !matches!(
            self.capture,
            CaptureState::Recording { .. } | CaptureState::Saving
        );
        let output_rect = output_rect_of(info);
        let mut stack = iced::widget::Stack::new();

        // Background painter. Mode-aware: Screenshot freezes the display
        // (the surface paints the frozen output capture, so the user sees
        // a stable image while framing the shot); Record leaves the
        // surface transparent so the user can frame action that's
        // happening live underneath. Source::Window is special in both
        // modes — it paints the wallpaper so the picker tiles aren't
        // drawn on top of copies of themselves.
        //
        // Earlier this code skipped the frozen bg entirely to avoid an
        // iced wgpu-cache eviction antagonist on Region↔Window toggles
        // (the frozen 4K texture was in the Region tree but not the
        // Window tree, so each toggle paid a multi-texture reupload).
        // The fix here is to make the frozen bg unconditional in
        // Screenshot mode — present in every Source's tree — so it's
        // never the texture that gets evicted on a toggle.
        // Per-output picker tile readiness. The wallpaper bg + picker
        // layer should appear together; both need to be GPU-ready
        // before we expose them, or the transition shows live desktop
        // for a frame. We require BOTH:
        //   * `picker_painted` — set ~100ms after `WindowsForOutputReady`
        //     so the `IntrinsicShader` wgpu pipeline is warm and tile
        //     textures are uploaded into `ThumbPipeline`.
        //   * `wallpaper_sub` — the decoded wallpaper memfd exists, so
        //     the compositor can show it the same frame the tiles land.
        // For non-Window sources both checks are bypassed. Color /
        // gradient sources go through a `container` widget with no
        // texture upload, so they're always ready; same for outputs
        // with no wallpaper at all (cosmic-bg not running).
        let wallpaper_ready = match info.bg_source.as_ref() {
            Some(cosmic_bg_config::Source::Path(_)) => info.wallpaper_sub.is_some(),
            _ => true,
        };
        let tiles_ready = !matches!(self.source, Source::Window)
            || (self.picker_painted.contains(&info.name) && wallpaper_ready);

        // Frozen bg is the backdrop for Region/Screen sources in
        // Screenshot mode. We deliberately skip it in Source::Window —
        // Record mode does the same (its gate is just Mode::Screenshot
        // anyway), and matching that here avoids the "stale freeze
        // flashes through unloaded wallpaper" issue: the frozen layer
        // sits underneath wallpaper, so any single-frame race in the
        // wallpaper's texture upload exposes the freeze. With no
        // frozen layer under the picker, the worst case is a
        // transparent flash to live desktop, identical to Record mode.
        if pre_capture
            && matches!(self.mode, Mode::Screenshot)
            && !matches!(self.source, Source::Window)
        {
            if let Some(sub) = info.frozen_sub.as_ref() {
                // z < 0 stacks the subsurface *below* the toolbar
                // surface; the toolbar is transparent wherever it
                // doesn't paint, so the freeze shows through.
                // `From<Subsurface>` is only implemented for
                // `Element<'static>`; go through `Element::new`.
                let freeze: Element<'_, Msg> = Element::new(
                    Subsurface::new(sub.clone())
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .content_fit(cosmic::iced::ContentFit::Fill)
                        .z(-1),
                );
                stack = stack.push(freeze);
            } else if let Some(handle) = info.frozen_handle.as_ref() {
                stack = stack.push(
                    cosmic::iced::widget::image(handle.clone())
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .content_fit(cosmic::iced::ContentFit::Fill),
                );
            }
        }
        if pre_capture && matches!(self.source, Source::Window) && tiles_ready {
            if let Some(bg) = self.window_picker_bg(info) {
                stack = stack.push(bg);
            }
        }

        if pre_capture && matches!(self.source, Source::Region) {
            // view_toolbar is only invoked via the dispatch path that found
            // this id, so info.toolbar_id must be Some.
            if let Some(toolbar_id) = info.toolbar_id {
                let sel = RectangleSelection::new(
                    output_rect,
                    self.region.unwrap_or_default(),
                    RectMode::Selecting,
                    SELECTION_DND_ID,
                    toolbar_id,
                    self.drag,
                    Msg::Selection,
                );
                stack = stack.push(Element::from(sel));
            }
        }

        if pre_capture && matches!(self.source, Source::Screen) {
            // Only paint the "you'll capture this monitor" border on the
            // display the pointer is currently on. Without this every output
            // shows a border simultaneously, which defeats the purpose.
            //
            // No fallback before the first pointer event: toolbars map
            // one output at a time, and guessing "the first output with
            // a toolbar" meant the border appeared on whichever output
            // came up first, then jumped to the real one when the
            // compositor's pointer enter landed a few ms later — a
            // visible flash on every launch. The enter arrives within a
            // frame of the surface mapping, so waiting for it costs
            // nothing perceptible.
            let is_hovered = self.hovered_toolbar.is_some_and(|h| Some(h) == info.toolbar_id);
            if is_hovered {
                stack = stack.push(fullscreen_border());
            }
        }

        // Source::Window in both Screenshot and Record modes: render the
        // fan-out picker on every toolbar (each output scopes the picker
        // to its own toplevels). Matches xdg-desktop-portal-cosmic which
        // opens one layer surface per output with that output's
        // toplevel_images. Clicking a tile fires `Msg::CaptureToplevel`,
        // which routes by mode: Screenshot → one-shot save; Record →
        // start continuous toplevel-screencopy recording.
        if pre_capture && matches!(self.source, Source::Window) && tiles_ready {
            stack = stack.push(self.view_window_picker(info));
        }

        stack.push(pill_layer).into()
    }

    fn idle_action_label(&self) -> String {
        match self.mode {
            Mode::Screenshot => "Capture".into(),
            Mode::Record => "Start".into(),
        }
    }

    fn build_record_args(&self) -> RecordArgs {
        let container = match self.record_format {
            RecordFormat::Mp4 | RecordFormat::Gif => VideoContainer::Mp4,
            RecordFormat::Mkv => VideoContainer::Mkv,
            RecordFormat::WebM => VideoContainer::WebM,
        };
        RecordArgs {
            common: CommonArgs {
                file: None,
                notify: self.notify,
                clipboard: self.clipboard,
            },
            fps: self.record_fps.as_u32(),
            encoder: self.rec_encoder,
            container,
            mic: self.rec_audio_mic,
            system_audio: self.rec_audio_system,
            cursor: self.rec_cursor,
            full: matches!(self.source, Source::Screen | Source::Window),
            duration_secs: None,
        }
    }
    fn build_gif_args(&self) -> GifArgs {
        GifArgs {
            common: CommonArgs {
                file: None,
                notify: self.notify,
                clipboard: self.clipboard,
            },
            fps: 20,
            quality: 90,
            max_width: 800,
            cursor: self.rec_cursor,
            full: matches!(self.source, Source::Screen | Source::Window),
            duration_secs: 0,
        }
    }
    fn active_output_name(&self, override_: Option<&str>) -> Option<String> {
        // Caller-provided override wins: the toolbar's `PrimaryAction(Some(name))`
        // means "the user clicked Capture on this output's bar", which is the
        // most direct signal of intent for Source::Screen. We still validate
        // it against the current output set in case the display vanished
        // between click and dispatch.
        if let Some(name) = override_ {
            if self.outputs.values().any(|o| o.name == name) {
                return Some(name.to_string());
            }
        }
        // Space-key path (override=None): use the toolbar surface the
        // pointer currently sits on. Without this the keyboard hotkey would
        // bind to a HashMap-iteration-order output that has nothing to do
        // with where the user is actually looking.
        if let Some(hover) = self.hovered_toolbar {
            if let Some(info) = self
                .outputs
                .values()
                .find(|o| o.toolbar_id == Some(hover))
            {
                return Some(info.name.clone());
            }
        }
        // Multi-output: pick whichever output the region's center sits on,
        // falling back to the first output if no region (Source::Screen).
        // This makes "capture this monitor" do the right thing whichever
        // toolbar the user clicks Capture on.
        if let Some(region) = self.region {
            let region = region.normalize();
            let cx = (region.left + region.right) / 2;
            let cy = (region.top + region.bottom) / 2;
            if let Some(info) = self.outputs.values().find(|o| {
                let r_left = o.logical_pos.0;
                let r_top = o.logical_pos.1;
                let r_right = r_left + o.logical_size.0 as i32;
                let r_bottom = r_top + o.logical_size.1 as i32;
                cx >= r_left && cx < r_right && cy >= r_top && cy < r_bottom
            }) {
                return Some(info.name.clone());
            }
        }
        self.outputs.values().next().map(|o| o.name.clone())
    }
}

fn output_rect_of(info: &OutputInfo) -> SelectionRect {
    SelectionRect {
        left: info.logical_pos.0,
        top: info.logical_pos.1,
        right: info.logical_pos.0 + info.logical_size.0 as i32,
        bottom: info.logical_pos.1 + info.logical_size.1 as i32,
    }
}

/// Rounded inset border that fills the surface — shown when Source::Screen
/// is active to signal which display will be captured. Pointer-transparent
/// (it's just a styled container) so the user can still interact with the
/// toolbar pill that sits above it in the Stack.
/// Pre-recording hover affordance — "you'll capture this monitor"
/// border drawn on the display the pointer is over. Uses the cosmic
/// accent so it reads as a *selection hint* rather than active
/// recording state.
fn fullscreen_border<'a>() -> Element<'a, Msg> {
    fullscreen_border_inner(BorderTint::Accent)
}

/// Active-recording variant — same shape but red. Drawn on the
/// recording-target output by the recording overlay surfaces during
/// Display + Window recording (Region uses the rectangle widget).
fn fullscreen_recording_border<'a>() -> Element<'a, Msg> {
    fullscreen_border_inner(BorderTint::Red)
}

#[derive(Copy, Clone)]
enum BorderTint {
    Accent,
    Red,
}

fn fullscreen_border_inner<'a>(tint: BorderTint) -> Element<'a, Msg> {
    let border_only = container(iced::widget::Space::new())
        .width(Length::Fill)
        .height(Length::Fill)
        .class(cosmic::theme::Container::Custom(Box::new(move |theme| {
            let t = theme.cosmic();
            let color = match tint {
                BorderTint::Accent => Color::from(t.accent_color()),
                // Match RectangleSelection's RectMode::Recording red.
                BorderTint::Red => Color::from_rgb(0.93, 0.20, 0.20),
            };
            cosmic::iced::widget::container::Style {
                background: None,
                border: Border {
                    // Small 4px radius — same value RectangleSelection
                    // uses for its recording border. Subtle enough that
                    // the transparent corner gaps inside the capture
                    // are essentially invisible (a few pixels at each
                    // corner), but rounded enough that the on-screen
                    // affordance doesn't look like a harsh frame.
                    radius: 4.0.into(),
                    width: 3.0,
                    color,
                },
                ..Default::default()
            }
        })));
    // No outer padding: the border sits flush against the output edge.
    // The 8px inset we had earlier still showed up in the captured file
    // because cosmic-screencopy captures *everything* composited on the
    // output. Flushing the border to the edge means the 3px stroke can
    // be cropped out by trimming 3px from each side of the recording
    // (see `SCREEN_CAPTURE_BORDER_INSET`), which loses less of the
    // actual content than the old 11px (8 padding + 3 stroke) inset
    // would. The rounded corners do leave a few transparent pixels at
    // each corner that fall inside the crop — acceptable: those are
    // outside the rectangular recording region anyway.
    container(border_only)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Pixel inset applied to display + window recordings so the visible
/// red border (drawn at the output edge) is excluded from the saved
/// file. Equals the border stroke width in [`fullscreen_border`].
pub const SCREEN_CAPTURE_BORDER_INSET: u32 = 3;

enum ActionKind {
    Normal,
    Disabled,
}

/// Render the classic "record button": red dot inside a white ring while idle;
/// red rounded-square inside the same ring while recording.
fn record_button<'a>(
    capture: &CaptureState,
    enabled: bool,
    press: Option<Msg>,
) -> button::Button<'a, Msg> {
    let recording = matches!(capture, CaptureState::Recording { .. });
    let alpha = if enabled { 1.0 } else { 0.35 };
    let red = Color::from_rgba(0.93, 0.20, 0.20, alpha);

    // Idle = full circle (red dot ready-to-record); recording = a chunkier,
    // distinctly-square red block so it reads as the universal "stop" glyph.
    let inner_size = if recording { 16.0 } else { 18.0 };
    let inner_radius: f32 = if recording { 2.0 } else { inner_size / 2.0 };
    let inner = container(iced::widget::Space::new())
        .width(Length::Fixed(inner_size))
        .height(Length::Fixed(inner_size))
        .class(cosmic::theme::Container::Custom(Box::new(move |_t| {
            cosmic::iced::widget::container::Style {
                background: Some(Background::Color(red)),
                border: Border {
                    radius: inner_radius.into(),
                    width: 0.0,
                    color: Color::TRANSPARENT,
                },
                ..Default::default()
            }
        })));

    let ring = container(inner)
        .width(Length::Fixed(32.0))
        .height(Length::Fixed(32.0))
        .align_x(iced::Alignment::Center)
        .align_y(iced::Alignment::Center)
        .class(cosmic::theme::Container::Custom(Box::new(move |_t| {
            cosmic::iced::widget::container::Style {
                background: None,
                border: Border {
                    radius: 16.0.into(),
                    width: 2.0,
                    color: Color {
                        a: alpha,
                        ..Color::WHITE
                    },
                },
                ..Default::default()
            }
        })));

    button::custom(ring)
        .padding(2)
        .class(cosmic::theme::Button::Transparent)
        .on_press_maybe(press)
}

/// Build the stream that backs the streaming-thumbnail redraw
/// subscription. Takes the redraw receiver out of the global bus exactly
/// once and forwards each ping as a `Msg::ThumbDirty`. `Subscription::run`
/// only invokes this fn once per identity (a stable function pointer), so
/// the one-shot `take_redraw_receiver` lines up with iced's subscription
/// lifecycle. If the bus is already taken (shouldn't happen — there's
/// one subscription per app), we park forever so iced doesn't see a
/// stream-end and drop us.
fn thumb_redraw_stream() -> impl iced::futures::Stream<Item = Msg> {
    iced::stream::channel(64, async |mut sender| {
        let Some(mut rx) = streaming_thumb::take_redraw_receiver() else {
            tracing::warn!(
                "thumb_redraw_stream: redraw receiver already taken — \
                 parking subscription"
            );
            std::future::pending::<()>().await;
            return;
        };
        while let Some(()) = rx.recv().await {
            if sender.try_send(Msg::ThumbDirty).is_err() {
                // Channel full or closed. If full, the consumer is
                // already behind on redraws and we don't need to pile
                // on. If closed, iced has dropped us; loop will exit
                // next iter when `recv()` returns None.
            }
        }
    })
}

async fn run_screenshot(
    output_name: String,
    cursor: bool,
    crop: Option<CropRect>,
    destination: pipeline::screenshot::Destination,
    notify_user: bool,
) -> Result<String, String> {
    pipeline::screenshot::capture_with(output_name, cursor, crop, destination, notify_user)
        .await
        .map(path_to_string)
        .map_err(|e| format!("{:#}", e))
}
async fn run_toplevel_screenshot(
    identifier: String,
    cursor: bool,
    destination: pipeline::screenshot::Destination,
    notify_user: bool,
) -> Result<String, String> {
    pipeline::screenshot::capture_toplevel(identifier, cursor, destination, notify_user)
        .await
        .map(path_to_string)
        .map_err(|e| format!("{:#}", e))
}
async fn run_record_screencopy(
    helper: WaylandHelper,
    target: ScreencopyTarget,
    args: RecordArgs,
    crop: Option<CropRect>,
    stop_rx: oneshot::Receiver<()>,
) -> Result<String, String> {
    pipeline::record::record_via_screencopy(helper, target, args, crop, stop_rx)
        .await
        .map(path_to_string)
        .map_err(|e| format!("{:#}", e))
}

async fn run_record_screencopy_multi(
    helper: WaylandHelper,
    parts: Vec<pipeline::record::RegionPart>,
    args: RecordArgs,
    stop_rx: oneshot::Receiver<()>,
) -> Result<String, String> {
    pipeline::record::record_via_screencopy_multi(helper, parts, args, stop_rx)
        .await
        .map(path_to_string)
        .map_err(|e| format!("{:#}", e))
}

async fn run_gif_screencopy(
    helper: WaylandHelper,
    target: ScreencopyTarget,
    args: GifArgs,
    crop: Option<CropRect>,
    stop_rx: oneshot::Receiver<()>,
) -> Result<String, String> {
    pipeline::gif::gif_via_screencopy(helper, target, args, crop, stop_rx)
        .await
        .map(path_to_string)
        .map_err(|e| format!("{:#}", e))
}

async fn run_save_frame(
    frame: CapturedFrame,
    crop: Option<CropRect>,
    destination: pipeline::screenshot::Destination,
    notify_user: bool,
) -> Result<String, String> {
    pipeline::screenshot::save_frame(frame, crop, destination, notify_user)
        .await
        .map(path_to_string)
        .map_err(|e| format!("{:#}", e))
}
/// Capture one output's frozen frame via the persistent WaylandHelper.
/// Replaces the prior `screencopy::capture` + `spawn_blocking` shape;
/// the helper's dispatch thread drives the screencopy session on the
/// shared connection, so concurrent captures don't open extra wayland
/// connections (which is what produced the heap corruption that used
/// to need a global lock).
async fn capture_output_via_helper(
    helper: &WaylandHelper,
    output_name: &str,
    with_cursor: bool,
) -> Option<Arc<CapturedFrame>> {
    let output = helper.output_for_name(output_name)?;
    tracing::info!(output = %output_name, "freeze: capturing output");
    let source = cosmic_client_toolkit::screencopy::CaptureSource::Output(output);
    let frame = helper.capture_source_shm(source, with_cursor).await?;
    Some(Arc::new(frame))
}

/// Drain the toplevel-image stream for a single output. Mirrors the
/// portal's per-output `capture_output_toplevels_shm(...)` collect into
/// `Vec<ScreenshotImage>`. Each yielded item already carries the
/// identifier/title/app_id we need for picker labels.
async fn capture_toplevels_for_output(
    helper: &WaylandHelper,
    output_name: &str,
    with_cursor: bool,
) -> Vec<WindowEntry> {
    // Wait for cosmic-toplevel-info's initial batch to land before
    // snapshotting `output_toplevels`. Without this, a refresh fired
    // ~150ms after launch (e.g. persisted Source::Window) snapshots an
    // empty map and the picker shows "No windows on this display"
    // forever. The portal doesn't need this — its helper has been warm
    // for the entire session by the time anyone opens its picker.
    helper.wait_for_toplevel_info().await;
    let Some(output) = helper.output_for_name(output_name) else {
        return Vec::new();
    };
    use futures_util::StreamExt;
    let stream = helper.capture_output_toplevels_shm(&output, with_cursor);
    let captures: Vec<WindowCapture> = stream.collect().await;
    captures
        .into_iter()
        .map(|c| {
            let width = c.frame.width;
            let thumb = frame_to_image_handle(&c.frame);
            // `streaming` is filled in by the handler (which owns
            // `shared_frames`) so the `SharedFrame` id stays stable
            // across picker refreshes for the same toplevel identifier.
            WindowEntry {
                identifier: c.identifier,
                title: c.title,
                app_id: c.app_id,
                thumb,
                frozen: Arc::new(c.frame),
                width,
                alloc: None,
                streaming: None,
            }
        })
        .collect()
}

/// Repack a `CapturedFrame` (which may have row padding via `stride >
/// width*4`) into a tightly packed RGBA buffer and wrap it in an iced
/// image handle. The handle internally stores an `Arc<Bytes>`, so it's
/// cheap to clone for every redraw.
fn frame_to_image_handle(frame: &CapturedFrame) -> cosmic::iced::widget::image::Handle {
    cosmic::iced::widget::image::Handle::from_rgba(frame.width, frame.height, packed_rows(frame))
}

/// Wrap a freeze's memfd as a subsurface buffer. `None` if the frame
/// didn't come through shm or the fd couldn't be dup'd.
fn frozen_subsurface_buffer(frame: &Arc<CapturedFrame>) -> Option<SubsurfaceBuffer> {
    let fd = match frame.shm_fd.as_ref()?.try_clone() {
        Ok(fd) => fd,
        Err(e) => {
            tracing::warn!(error = %e, "dup freeze memfd failed; uploading as texture");
            return None;
        }
    };
    let (buffer, _release) = SubsurfaceBuffer::new(Arc::new(BufferSource::Shm(Shmbuf {
        fd,
        offset: 0,
        width: frame.width as i32,
        height: frame.height as i32,
        stride: frame.stride as i32,
        format: cosmic::cctk::wayland_client::protocol::wl_shm::Format::Abgr8888,
    })));
    Some(buffer)
}

/// Same as `frame_to_image_handle` but shares the pixel buffer with the
/// `Arc<CapturedFrame>` when the rows are already tightly packed, so a
/// 24MB freeze isn't memcpy'd (and page-faulted) again on the main
/// thread just to hand it to iced.
fn shared_frame_to_image_handle(
    frame: &Arc<CapturedFrame>,
) -> cosmic::iced::widget::image::Handle {
    struct Pixels(Arc<CapturedFrame>);
    impl AsRef<[u8]> for Pixels {
        fn as_ref(&self) -> &[u8] {
            &self.0.pixels
        }
    }
    let bytes = if frame.stride as usize == frame.width as usize * 4 {
        cosmic::iced::core::Bytes::from_owner(Pixels(frame.clone()))
    } else {
        cosmic::iced::core::Bytes::from(packed_rows(frame))
    };
    cosmic::iced::widget::image::Handle::from_rgba(frame.width, frame.height, bytes)
}

/// Copy `frame` into a tightly packed `width * 4` row layout.
fn packed_rows(frame: &CapturedFrame) -> Vec<u8> {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let s = frame.stride as usize;
    let row_bytes = w * 4;
    if s == row_bytes {
        return frame.pixels.clone();
    }
    let mut out = Vec::with_capacity(row_bytes * h);
    for y in 0..h {
        let off = y * s;
        out.extend_from_slice(&frame.pixels[off..off + row_bytes]);
    }
    out
}

/// Decode a wallpaper file and scale it to cover `target` (output
/// physical size), matching the picker's `ContentFit::Cover`. Scaling
/// here keeps the uploaded texture at output size instead of the
/// source's (a 5K wallpaper is 59MB as RGBA8).
fn decode_wallpaper(
    path: &Path,
    targets: &[(u32, (u32, u32))],
) -> Vec<(u32, Option<SubsurfaceBuffer>)> {
    let started = std::time::Instant::now();
    let img = match image::open(path) {
        Ok(img) => img.into_rgba8(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "wallpaper decode failed");
            return targets.iter().map(|&(k, _)| (k, None)).collect();
        }
    };
    let decode_ms = started.elapsed().as_millis();
    let (iw, ih) = img.dimensions();
    targets
        .iter()
        .map(|&(key, (tw, th))| {
            let rgba = if tw > 0 && th > 0 && (iw > tw || ih > th) {
                let scale = (f64::from(tw) / f64::from(iw)).max(f64::from(th) / f64::from(ih));
                let w = (f64::from(iw) * scale).round().max(1.0) as u32;
                let h = (f64::from(ih) * scale).round().max(1.0) as u32;
                // Box-filter downscale: ~4x faster than `resize` with
                // Triangle and indistinguishable for a backdrop.
                image::imageops::thumbnail(&img, w, h)
            } else {
                img.clone()
            };
            let (w, h) = rgba.dimensions();
            tracing::debug!(
                path = %path.display(), w, h, decode_ms,
                total_ms = started.elapsed().as_millis(),
                "wallpaper decoded"
            );
            (key, rgba_to_shm_buffer(w, h, rgba.as_raw()))
        })
        .collect()
}

/// Copy tightly packed RGBA8 pixels into a fresh memfd and wrap it as
/// an `Abgr8888` shm subsurface buffer (little-endian ABGR is the
/// R,G,B,A byte order in memory).
fn rgba_to_shm_buffer(width: u32, height: u32, rgba: &[u8]) -> Option<SubsurfaceBuffer> {
    let stride = width as usize * 4;
    let size = stride * height as usize;
    if rgba.len() < size || size == 0 {
        return None;
    }
    let fd = crate::capture::wayland::create_memfd(size)?;
    {
        use std::io::Write;
        let mut file = std::fs::File::from(fd.try_clone().ok()?);
        if let Err(e) = file.write_all(&rgba[..size]) {
            tracing::warn!(error = %e, "wallpaper memfd write failed");
            return None;
        }
    }
    let (buffer, _release) = SubsurfaceBuffer::new(Arc::new(BufferSource::Shm(Shmbuf {
        fd,
        offset: 0,
        width: width as i32,
        height: height as i32,
        stride: stride as i32,
        format: cosmic::cctk::wayland_client::protocol::wl_shm::Format::Abgr8888,
    })));
    Some(buffer)
}


/// Read the wallpaper source configured for a given output. Mirrors
/// xdg-desktop-portal-cosmic's lookup in
/// `screenshot.rs::update_args` — pull `wallpapers: Vec<(String, Source)>`
/// from the cosmic-bg state config, match by output name, fall back to
/// the same default path the portal uses when none is set. Returns
/// `None` only if the state config can't be opened at all (compositor
/// without cosmic-bg).
fn load_bg_for_output(name: &str) -> Option<cosmic_bg_config::Source> {
    use cosmic::cosmic_config::CosmicConfigEntry;
    let config = cosmic::cosmic_config::Config::new_state(
        cosmic_bg_config::NAME,
        cosmic_bg_config::state::State::version(),
    )
    .ok()?;
    let state = match cosmic_bg_config::state::State::get_entry(&config) {
        Ok(s) => s,
        Err((err, partial)) => {
            tracing::debug!(error = ?err, "cosmic-bg state read partial; using partial");
            partial
        }
    };
    Some(
        state
            .wallpapers
            .iter()
            .find(|(o, _)| o == name)
            .map(|(_, src)| src.clone())
            .unwrap_or_else(|| {
                cosmic_bg_config::Source::Path(std::path::PathBuf::from(
                    "/usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg",
                ))
            }),
    )
}

fn path_to_string(p: PathBuf) -> String {
    p.to_string_lossy().into_owned()
}

/// Format an elapsed-second count for the recording timer label.
/// MM:SS under an hour, HH:MM:SS beyond — same shape OBS / cosmic-screenshot
/// use for live recordings.
fn format_elapsed(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

fn wayland_proxy_id(o: &WlOutput) -> u32 {
    use wayland_client::Proxy;
    o.id().protocol_id()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> SelectionRect {
        SelectionRect { left, top, right, bottom }
    }

    #[test]
    fn crop_at_100_percent_is_identity() {
        let out = rect(2559, 0, 2559 + 3840, 1600);
        let c = crop_for_region(out, (3840, 1600), rect(2600, 100, 2700, 250)).unwrap();
        assert_eq!((c.x, c.y, c.w, c.h), (41, 100, 100, 150));
    }

    #[test]
    fn crop_on_fractional_scale_uses_real_ratio() {
        // 1920x1200 panel at 115%: logical 1670x1043. wl_output.scale
        // would report 2 here; the crop must use 1920/1670 ≈ 1.15.
        let out = rect(0, 0, 1670, 1043);
        let c = crop_for_region(out, (1920, 1200), rect(0, 0, 1670, 1043)).unwrap();
        assert_eq!((c.x, c.y, c.w, c.h), (0, 0, 1920, 1200));
        let c = crop_for_region(out, (1920, 1200), rect(835, 0, 1670, 521)).unwrap();
        assert_eq!((c.x, c.w), (960, 960));
        assert_eq!((c.y, c.h), (0, 599));
    }

    #[test]
    fn crop_clips_to_output_and_rejects_disjoint() {
        let out = rect(0, 160, 2560, 1600);
        let c = crop_for_region(out, (2560, 1440), rect(-50, 100, 100, 300)).unwrap();
        assert_eq!((c.x, c.y, c.w, c.h), (0, 0, 100, 140));
        assert!(crop_for_region(out, (2560, 1440), rect(3000, 0, 3100, 100)).is_none());
    }

    #[test]
    fn adjacent_parts_on_fractional_output_tile_without_seam() {
        let out = rect(0, 0, 1670, 1043);
        let a = crop_for_region(out, (1920, 1200), rect(0, 0, 333, 100)).unwrap();
        let b = crop_for_region(out, (1920, 1200), rect(333, 0, 666, 100)).unwrap();
        assert_eq!(a.x + a.w as i32, b.x);
    }
}

