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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use cosmic::app::{Core, Settings, Task};
use cosmic::iced::core::event::wayland::OutputEvent;
use cosmic::iced::keyboard::Key;
use cosmic::iced::keyboard::key::Named;
use cosmic::iced::platform_specific::shell::commands::layer_surface::{
    Anchor, KeyboardInteractivity, Layer, destroy_layer_surface, get_layer_surface,
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
use crate::capture::wayland::{WaylandHelper, WindowCapture};
use crate::cli::{CommonArgs, GifArgs, RecordArgs, VideoContainer, VideoEncoder};
use crate::encode::video::CropRect;
use crate::pipeline;

use super::widget::{
    DragSession, RectMode, RectangleSelection, SelectionEvent, SelectionRect,
};

const APP_ID: &str = "com.system76.CosmicCapture";
const TOOLBAR_BOTTOM_MARGIN: u16 = 32;
/// Shared id for the synthetic DnD operation our selection widgets use to
/// track cursor motion across per-output layer surfaces. Constant because
/// only one selection drag exists at a time; collisions with real DnD ops
/// are avoided by the matching `DND_MIME` filter on incoming events.
const SELECTION_DND_ID: u128 = 0x4341_5054_5552_452D_5345_4C45_4354_494F;

pub fn launch() -> Result<()> {
    tracing::info!("cosmic-capture GUI starting (layer-shell toolbar)");
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
    scale: i32,
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
    /// Output's wallpaper config — pulled from `cosmic_bg_config::state`
    /// on output discovery. Window-mode picker paints this as its
    /// background (path → image, color → solid/gradient) so the
    /// foreground toplevel tiles aren't competing with their own
    /// reflections in a frozen output capture.
    bg_source: Option<cosmic_bg_config::Source>,
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
    rec_audio: bool,
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
    /// Source window pixel width — used to size each picker tile
    /// proportionally (`Length::FillPortion`) so a 1920px-wide window
    /// gets a wider tile than a 480px utility palette. Same arithmetic
    /// the portal applies in `widget/screenshot.rs`.
    width: u32,
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

    ToggleClipboard,

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
    /// Per-output toplevel thumbnails arrived as a batch. Carries the
    /// output name (matched against `OutputInfo.name`) and the captured
    /// entries in stream order. Replaces the previous per-window
    /// `Msg::WindowThumbReady` pattern — the helper's
    /// `capture_output_toplevels_shm` already streams sessions on the
    /// shared connection.
    WindowsForOutputReady(String, Vec<WindowEntry>),
    /// User clicked a window card → capture that window full-res and save.
    CaptureToplevel(String),
    Quit,
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
            rec_audio: false,
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
            cancel_pending: false,
            hovered_toolbar: None,
            windows: HashMap::new(),
            helper,
            dummy_id,
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

        (panel, dummy)
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
                            let with_cursor = self.rec_cursor;
                            for (&key, info) in &self.outputs {
                                if info.frozen_handle.is_some() {
                                    continue;
                                }
                                let helper = self.helper.clone();
                                let name = info.name.clone();
                                tasks.push(Task::perform(
                                    async move {
                                        capture_output_via_helper(&helper, &name, with_cursor)
                                            .await
                                    },
                                    move |r| cosmic::action::app(Msg::FrozenFrameReady(key, r)),
                                ));
                            }
                            if matches!(self.source, Source::Window) {
                                tasks.push(self.refresh_window_picker());
                            }
                        }
                        Mode::Record => {
                            for info in self.outputs.values_mut() {
                                info.frozen = None;
                                info.frozen_handle = None;
                            }
                            self.windows.clear();
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
                    if matches!(s, Source::Window) && matches!(self.mode, Mode::Screenshot) {
                        return self.refresh_window_picker();
                    }
                }
            }
            Msg::ToggleClipboard => self.clipboard = !self.clipboard,
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
                        let scale = info.scale_factor;
                        let name_for_capture = name.clone();
                        let bg_source = load_bg_for_output(&name);
                        self.outputs.insert(
                            key,
                            OutputInfo {
                                output,
                                name,
                                logical_pos,
                                logical_size,
                                scale,
                                toolbar_id: None,
                                recording_id: None,
                                frozen: None,
                                frozen_handle: None,
                                bg_source,
                            },
                        );
                        // First time we have any geometry, validate the
                        // persisted region — if the monitor it lived on is
                        // gone, drop it so the user doesn't see a phantom
                        // rect pointing into empty space.
                        self.prune_stale_region();
                        // Open the toolbar. Freeze capture only fires in
                        // Screenshot mode — Record needs the live screen
                        // visible so the user can frame the action they're
                        // about to record.
                        let open = self.ensure_toolbar_for(key);
                        if matches!(self.mode, Mode::Screenshot) {
                            let with_cursor = self.rec_cursor;
                            let mut tasks = vec![open];
                            // Freeze background for this output.
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
                            // Per-output window picker — only when the
                            // user actually wants Window source. Without
                            // this, a persisted Screenshot+Window state
                            // would land on an empty picker until the
                            // user toggled Source.
                            if matches!(self.source, Source::Window) {
                                let picker_name = name_for_capture.clone();
                                tasks.push(self.spawn_picker_capture(picker_name, with_cursor));
                            }
                            return Task::batch(tasks);
                        }
                        return open;
                    }
                    OutputEvent::Created(None) => {}
                    OutputEvent::InfoUpdate(info) => {
                        if let Some(o) = self.outputs.get_mut(&key) {
                            if let Some(n) = info.name {
                                o.name = n;
                            }
                            if let Some(p) = info.logical_position {
                                o.logical_pos = p;
                            }
                            if let Some((w, h)) = info.logical_size {
                                o.logical_size = (w as u32, h as u32);
                            }
                            o.scale = info.scale_factor;
                        }
                        // If we'd missed opening a toolbar (e.g. the Created
                        // event arrived without geometry), retry now.
                        return self.ensure_toolbar_for(key);
                    }
                    OutputEvent::Removed => {
                        if let Some(info) = self.outputs.remove(&key) {
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

            Msg::Tick => {
                // While Saving, watch for a hung pipeline (e.g. gst stuck
                // on EOS after the pwsrc caps assertion) and force a
                // shutdown after a generous grace period so the user isn't
                // trapped behind a frozen "Saving…" UI. cancel_pending
                // ensures any in-flight output file is deleted.
                if matches!(self.capture, CaptureState::Saving) {
                    if let Some(started) = self.saving_started_at {
                        if started.elapsed() > std::time::Duration::from_secs(5) {
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
                self.windows.insert(output_name, entries);
            }
            Msg::CaptureToplevel(identifier) => {
                if !matches!(self.capture, CaptureState::Idle) {
                    return Task::none();
                }
                let destination = self.current_destination();
                let notify_user = self.notify;
                let cursor = self.rec_cursor;
                let close_bars = self.close_toolbars();
                self.capture = CaptureState::Saving;
                self.saving_started_at = Some(std::time::Instant::now());
                return Task::batch([
                    close_bars,
                    Task::perform(
                        run_toplevel_screenshot(identifier, cursor, destination, notify_user),
                        |r| cosmic::action::app(Msg::ScreenshotFinished(r)),
                    ),
                ]);
            }

            Msg::FrozenFrameReady(key, result) => {
                let Some(info) = self.outputs.get_mut(&key) else {
                    return Task::none();
                };
                match result {
                    Some(frame) => {
                        let handle = frame_to_image_handle(&frame);
                        info.frozen = Some(frame);
                        info.frozen_handle = Some(handle);
                    }
                    None => {
                        tracing::warn!(
                            output = %info.name,
                            "freeze capture returned None; toolbar will run live-screen"
                        );
                    }
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
        if id == self.dummy_id {
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
        // the red border around the active capture region. The widget is in
        // Recording mode so it ignores input; drag/dnd plumbing here is
        // unused but the API requires it.
        if let Some(info) = self.outputs.values().find(|o| o.recording_id == Some(id)) {
            let output_rect = output_rect_of(info);
            let selection = self.region.unwrap_or_default();
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
        // highlight only that display. `CursorMoved` fires at the cursor
        // sample rate (60+ Hz × N outputs) and triggers a redraw every
        // tick, so we only listen for the discrete enter/leave events.
        let pointer = event::listen_with(|e, _, id| match e {
            iced::Event::Mouse(mouse::Event::CursorEntered) => Some(Msg::PointerOn(Some(id))),
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
        Subscription::batch([outputs, keys, pointer, tick])
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
            // Screenshot+Window: captures fire from clicks on the fan-out
            // picker cards (`Msg::CaptureToplevel`), so the toolbar's
            // primary action stays inactive.
            (Mode::Screenshot, Source::Window) => false,
            (Mode::Screenshot, _) => true,
            // Record+Window: the screencast portal already advertises both
            // Monitor and Window source types — the user picks one in the
            // portal dialog. Our toolbar just kicks the flow.
            (Mode::Record, Source::Window) => true,
            (Mode::Record, Source::Screen) => true,
            (Mode::Record, Source::Region) => self.region.is_some(),
        }
    }

    /// Kick off per-output toplevel capture streams. Each output's
    /// stream replaces its entry in `self.windows` as it completes —
    /// matches xdg-desktop-portal-cosmic's
    /// `interactive_toplevel_images` which gathers per-output snapshots
    /// in parallel via `FuturesUnordered`.
    fn refresh_window_picker(&mut self) -> Task<Msg> {
        self.windows.clear();
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

    /// Open a fullscreen toolbar layer surface anchored to the given output if
    /// one isn't already open. Each output gets its own surface so users can
    /// drive capture from whichever monitor they're focused on.
    ///
    /// Toolbars start at `KeyboardInteractivity::Exclusive` so iced's dropdown
    /// overlays receive key events without bouncing focus to apps below.
    /// Multiple Exclusive layer surfaces are allowed by wlr-layer-shell —
    /// cosmic-comp routes focus to whichever surface the pointer is over.
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
        let mut tasks: Vec<Task<Msg>> = Vec::new();
        for info in self.outputs.values_mut() {
            if let Some(id) = info.toolbar_id.take() {
                tasks.push(destroy_layer_surface(id));
            }
        }
        Task::batch(tasks)
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
        get_layer_surface(SctkLayerSurfaceSettings {
            id,
            layer: Layer::Overlay,
            keyboard_interactivity: KeyboardInteractivity::OnDemand,
            input_zone: None,
            anchor: Anchor::BOTTOM,
            output: IcedOutput::Active,
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
            if let Some(id) = info.toolbar_id.take() {
                tasks.push(destroy_layer_surface(id));
            }
            if let Some(id) = info.recording_id.take() {
                tasks.push(destroy_layer_surface(id));
            }
        }
        tasks.push(iced::exit());
        Task::batch(tasks)
    }

    /// Drop a persisted region if its center doesn't fall inside any current
    /// output (e.g. the monitor was unplugged between launches). Better to
    /// start clean than show a rect floating in nothingness.
    fn prune_stale_region(&mut self) {
        let Some(region) = self.region else {
            return;
        };
        let region = region.normalize();
        let cx = (region.left + region.right) / 2;
        let cy = (region.top + region.bottom) / 2;
        let on_an_output = self.outputs.values().any(|o| {
            let r_left = o.logical_pos.0;
            let r_top = o.logical_pos.1;
            let r_right = r_left + o.logical_size.0 as i32;
            let r_bottom = r_top + o.logical_size.1 as i32;
            cx >= r_left && cx < r_right && cy >= r_top && cy < r_bottom
        });
        if !on_an_output {
            tracing::info!(?region, "saved region is off-screen; clearing");
            self.region = None;
            self.persist("last_region", &Option::<SelectionRect>::None);
        }
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

    fn crop_from_region(&self) -> Option<CropRect> {
        let region = self.region?.normalize();
        let cx = (region.left + region.right) / 2;
        let cy = (region.top + region.bottom) / 2;
        let info = self.outputs.values().find(|o| {
            let r_left = o.logical_pos.0;
            let r_top = o.logical_pos.1;
            let r_right = r_left + o.logical_size.0 as i32;
            let r_bottom = r_top + o.logical_size.1 as i32;
            cx >= r_left && cx < r_right && cy >= r_top && cy < r_bottom
        })?;
        let scale = info.scale.max(1);
        let lx = (region.left - info.logical_pos.0).max(0);
        let ly = (region.top - info.logical_pos.1).max(0);
        let lw = region.width().min(info.logical_size.0 as i32 - lx).max(1);
        let lh = region.height().min(info.logical_size.1 as i32 - ly).max(1);
        Some(CropRect {
            x: lx * scale,
            y: ly * scale,
            w: (lw * scale) as u32,
            h: (lh * scale) as u32,
        })
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
                    let crop = match self.source {
                        Source::Region => self.crop_from_region(),
                        Source::Screen | Source::Window => None,
                    };
                    // Recording switchover: tear down the per-output
                    // toolbars (their hover-state redraws were flickering)
                    // and replace with a compact bottom stop pill + the
                    // pointer-transparent recording border overlays.
                    let close_bars = self.close_toolbars();
                    let stop_pill = self.open_stop_pill();
                    let swap = self.open_recording_overlays();
                    let release_kb = Task::none();
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
                                Task::perform(run_gif(args, crop, stop_rx), |r| {
                                    cosmic::action::app(Msg::GifFinished(r))
                                }),
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
                                Task::perform(run_record(args, crop, stop_rx), |r| {
                                    cosmic::action::app(Msg::RecordingFinished(r))
                                }),
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
                cosmic::widget::layer_container(
                    button::custom(
                        cosmic::iced::widget::image(entry.thumb.clone())
                            .content_fit(cosmic::iced::ContentFit::ScaleDown),
                    )
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
            Source::Path(path) => cosmic::iced::widget::image(
                cosmic::iced::widget::image::Handle::from_path(path),
            )
            .content_fit(cosmic::iced::ContentFit::Cover)
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
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

        let row = row::with_capacity(3)
            .push(stop)
            .push(timer)
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
        let pill = pill
            .push(sep())
            .push(options)
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

        // Background painter. Source-specific to mirror
        // xdg-desktop-portal-cosmic's `bg_element` switch:
        //   * Region / Screen → frozen output capture (matches what the
        //     user is about to crop/select).
        //   * Window → the output's wallpaper (path or gradient/solid),
        //     so the foreground toplevel tiles don't fight against a
        //     duplicate of themselves drawn behind.
        // Skipped entirely outside Screenshot mode and during Saving so
        // Record shows the live screen.
        if pre_capture && matches!(self.mode, Mode::Screenshot) {
            let bg: Option<Element<'_, Msg>> = match self.source {
                Source::Window => self.window_picker_bg(info),
                Source::Region | Source::Screen => info.frozen_handle.clone().map(|handle| {
                    cosmic::iced::widget::image(handle)
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .content_fit(cosmic::iced::ContentFit::Fill)
                        .into()
                }),
            };
            if let Some(bg) = bg {
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
            // Fallback: if hover hasn't been established yet (initial frame
            // before any pointer event), highlight the first output so the
            // affordance isn't completely missing.
            let is_hovered = self
                .hovered_toolbar
                .map(|h| Some(h) == info.toolbar_id)
                .unwrap_or_else(|| {
                    self.outputs
                        .values()
                        .find(|o| o.toolbar_id.is_some())
                        .map_or(false, |o| o.toolbar_id == info.toolbar_id)
                });
            if is_hovered {
                stack = stack.push(fullscreen_border());
            }
        }

        // Screenshot+Window: render the fan-out picker on every toolbar
        // (each output scopes the picker to its own toplevels). Matches
        // xdg-desktop-portal-cosmic which opens one layer surface per
        // output with that output's toplevel_images. Record+Window still
        // delegates to the ScreenCast portal.
        if pre_capture
            && matches!(self.source, Source::Window)
            && matches!(self.mode, Mode::Screenshot)
        {
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
            audio: self.rec_audio,
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
fn fullscreen_border<'a>() -> Element<'a, Msg> {
    let border_only = container(iced::widget::Space::new())
        .width(Length::Fill)
        .height(Length::Fill)
        .class(cosmic::theme::Container::Custom(Box::new(|theme| {
            let t = theme.cosmic();
            cosmic::iced::widget::container::Style {
                background: None,
                border: Border {
                    radius: t.corner_radii.radius_m.into(),
                    width: 3.0,
                    color: t.accent_color().into(),
                },
                ..Default::default()
            }
        })));
    // Outer container pads 8px on every side so the border sits visibly
    // inset from the screen edge rather than clipping against it.
    container(border_only)
        .padding(8)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

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

async fn run_record(
    args: RecordArgs,
    crop: Option<CropRect>,
    stop_rx: oneshot::Receiver<()>,
) -> Result<String, String> {
    pipeline::record::record_with_crop(args, crop, stop_rx)
        .await
        .map(path_to_string)
        .map_err(|e| format!("{:#}", e))
}
async fn run_gif(
    args: GifArgs,
    crop: Option<CropRect>,
    stop_rx: oneshot::Receiver<()>,
) -> Result<String, String> {
    pipeline::gif::gif_with_crop(args, crop, stop_rx)
        .await
        .map(path_to_string)
        .map_err(|e| format!("{:#}", e))
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
            WindowEntry {
                identifier: c.identifier,
                title: c.title,
                app_id: c.app_id,
                thumb,
                width,
            }
        })
        .collect()
}

/// Repack a `CapturedFrame` (which may have row padding via `stride >
/// width*4`) into a tightly packed RGBA buffer and wrap it in an iced
/// image handle. The handle internally stores an `Arc<Bytes>`, so it's
/// cheap to clone for every redraw.
fn frame_to_image_handle(frame: &CapturedFrame) -> cosmic::iced::widget::image::Handle {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let s = frame.stride as usize;
    let row_bytes = w * 4;
    let buf = if s == row_bytes {
        frame.pixels.clone()
    } else {
        let mut out = Vec::with_capacity(row_bytes * h);
        for y in 0..h {
            let off = y * s;
            out.extend_from_slice(&frame.pixels[off..off + row_bytes]);
        }
        out
    };
    cosmic::iced::widget::image::Handle::from_rgba(frame.width, frame.height, buf)
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
