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
    self, Background, Border, Color, Length, Limits, Subscription, event, keyboard, window,
};
use cosmic::widget::{button, container, dropdown, icon, row};
use cosmic::{Application, Element, executor};
use tokio::sync::oneshot;
use wayland_client::protocol::wl_output::WlOutput;

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
    /// Toplevels available for `Source::Window` capture. Refreshed when the
    /// user enters Window mode (cosmic-protocols' toplevel_info doesn't
    /// stream from a long-lived connection here yet — see capture::toplevels).
    toplevels: Vec<crate::capture::toplevels::ToplevelSummary>,
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

    /// Toplevel enumeration finished. `Vec` may be empty if cosmic-comp
    /// doesn't advertise `ext_foreign_toplevel_list_v1` or the user has no
    /// windows open. Errors are logged and the list is left empty.
    ToplevelsLoaded(Vec<crate::capture::toplevels::ToplevelSummary>),
    /// User picked a specific window — pulls the identifier from the toplevel
    /// snapshot and kicks the screenshot pipeline at it.
    CaptureToplevel(String),

    /// Periodic tick during recording, fired by an `iced::time::every`
    /// subscription so the elapsed-time label on the stop pill keeps moving.
    Tick,

    ToggleClipboard,

    PrimaryAction,
    RecordingFinished(Result<String, String>),
    GifFinished(Result<String, String>),
    ScreenshotFinished(Result<String, String>),

    Output(OutputEvent, WlOutput),
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
            toplevels: Vec::new(),
            region: settings.last_region,
            drag: None,
            stop_pill_id: None,
            capture: CaptureState::default(),
            recording_started_at: None,
            saving_started_at: None,
            cancel_pending: false,
        };

        // No toolbar surface yet — we wait for OutputEvent::Created and open
        // one per output so users can capture from any monitor regardless of
        // which one they were focused on when launching. See
        // `ensure_toolbar_for_output`.
        (panel, Task::none())
    }

    fn update(&mut self, msg: Msg) -> Task<Msg> {
        match msg {
            Msg::SetMode(m) => {
                if !self.locked() && self.mode != m {
                    self.mode = m;
                    self.persist("mode", &m);
                }
            }
            Msg::SetSource(s) => {
                if !self.locked() && self.source != s {
                    self.source = s;
                    self.persist("source", &s);
                    if matches!(s, Source::Window) {
                        // Kick off a fresh toplevel enumeration.
                        return Task::perform(load_toplevels(), |list| {
                            cosmic::action::app(Msg::ToplevelsLoaded(list))
                        });
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
                            },
                        );
                        // First time we have any geometry, validate the
                        // persisted region — if the monitor it lived on is
                        // gone, drop it so the user doesn't see a phantom
                        // rect pointing into empty space.
                        self.prune_stale_region();
                        return self.ensure_toolbar_for(key);
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

            Msg::ToplevelsLoaded(list) => {
                tracing::debug!(count = list.len(), "toplevels loaded");
                self.toplevels = list;
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
            Msg::CaptureToplevel(identifier) => {
                if !matches!(self.capture, CaptureState::Idle) {
                    return Task::none();
                }
                let destination = self.current_destination();
                let notify_user = self.notify;
                let cursor = self.rec_cursor;
                self.capture = CaptureState::Saving;
                return Task::perform(
                    run_toplevel_screenshot(identifier, cursor, destination, notify_user),
                    |r| cosmic::action::app(Msg::ScreenshotFinished(r)),
                );
            }

            Msg::PrimaryAction => {
                tracing::info!(state = ?self.capture, "Msg::PrimaryAction");
                return self.on_primary_action();
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
                                    pipeline::screenshot::copy_bytes_to_clipboard(bytes, mime)
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
                Key::Character(s) if s.as_str() == " " => Some(Msg::PrimaryAction),
                _ => None,
            },
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
        Subscription::batch([outputs, keys, tick])
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
            // Screenshot+Window: captures fire from picker button clicks
            // (Msg::CaptureToplevel). Toolbar's primary action stays
            // inactive because there's no implicit "default window".
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
                    let stem = chrono::Local::now().format("cosmic-capture-%Y%m%d-%H%M%S");
                    d.join(format!("{stem}.png"))
                });
                pipeline::screenshot::Destination::File(dest)
            }
        }
    }

    fn on_primary_action(&mut self) -> Task<Msg> {
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
                    let Some(output_name) = self.active_output_name() else {
                        tracing::warn!("no output detected; can't screenshot");
                        return Task::none();
                    };
                    let crop = match self.source {
                        Source::Region => self.crop_from_region(),
                        Source::Screen | Source::Window => None,
                    };
                    let destination = match self.save_target {
                        SaveTarget::Clipboard => pipeline::screenshot::Destination::Clipboard,
                        SaveTarget::Pictures => pipeline::screenshot::Destination::File(None),
                        SaveTarget::Documents => {
                            let dest = dirs::document_dir().map(|d| {
                                let stem = chrono::Local::now()
                                    .format("cosmic-capture-%Y%m%d-%H%M%S");
                                d.join(format!("{stem}.png"))
                            });
                            pipeline::screenshot::Destination::File(dest)
                        }
                    };
                    self.capture = CaptureState::Saving;
                    let notify_user = self.notify;
                    let cursor = self.rec_cursor;
                    Task::perform(
                        run_screenshot(output_name, cursor, crop, destination, notify_user),
                        |r| cosmic::action::app(Msg::ScreenshotFinished(r)),
                    )
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

    /// Centered picker rendered when `Source::Window` is active. Each
    /// toplevel becomes a button in a column; clicking it kicks off a
    /// screencopy capture of that window.
    ///
    /// No thumbnails yet — this is title-only. Pre-capturing thumbnails per
    /// toplevel needs a long-lived wayland connection that streams updates
    /// into the GUI; that's a follow-up.
    fn view_window_picker(&self) -> Element<'_, Msg> {
        use cosmic::iced::widget::scrollable;

        let mut col = iced::widget::column::with_capacity(self.toplevels.len().max(1))
            .spacing(6)
            .align_x(iced::Alignment::Center);
        if self.toplevels.is_empty() {
            col = col.push(
                container(cosmic::widget::text::body("No windows available."))
                    .padding(12),
            );
        } else {
            for tl in &self.toplevels {
                let label = if tl.title.is_empty() {
                    if tl.app_id.is_empty() {
                        tl.identifier.clone()
                    } else {
                        tl.app_id.clone()
                    }
                } else {
                    tl.title.clone()
                };
                let id = tl.identifier.clone();
                col = col.push(
                    button::standard(label)
                        .on_press(Msg::CaptureToplevel(id))
                        .width(Length::Fixed(360.0)),
                );
            }
        }

        let inner = container(scrollable(col))
            .padding(16)
            .width(Length::Shrink)
            .height(Length::Shrink)
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
            })));

        container(inner)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(iced::Alignment::Center)
            .align_y(iced::Alignment::Center)
            .into()
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
            .then_some(Msg::PrimaryAction);
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
        let press = action_enabled.then_some(Msg::PrimaryAction);
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
            stack = stack.push(fullscreen_border());
        }

        if pre_capture
            && matches!(self.source, Source::Window)
            && matches!(self.mode, Mode::Screenshot)
        {
            // Record-mode window capture is driven through the ScreenCast
            // portal dialog (it already advertises Window as a source type),
            // so we only render our own picker for screenshots.
            stack = stack.push(self.view_window_picker());
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
    fn active_output_name(&self) -> Option<String> {
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

/// Run toplevel enumeration on the blocking pool. Empty result on error so
/// the caller doesn't need to thread a Result through the message enum.
async fn load_toplevels() -> Vec<crate::capture::toplevels::ToplevelSummary> {
    match tokio::task::spawn_blocking(crate::capture::toplevels::list).await {
        Ok(Ok(list)) => list,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "toplevel enumeration failed");
            Vec::new()
        }
        Err(e) => {
            tracing::warn!(error = %e, "toplevel enumeration task join failed");
            Vec::new()
        }
    }
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
