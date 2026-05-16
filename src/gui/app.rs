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
    set_keyboard_interactivity,
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

use crate::cli::{
    CommonArgs, GifArgs, RecordArgs, ScreenshotArgs, VideoContainer, VideoEncoder,
};
use crate::encode::video::CropRect;
use crate::pipeline;

use super::widget::{DragKind, RectMode, RectangleSelection, SelectionRect};

const APP_ID: &str = "com.system76.CosmicCapture";
const TOOLBAR_BOTTOM_MARGIN: u16 = 32;

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

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Mode {
    Screenshot,
    Record,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, Default)]
pub enum Source {
    #[default]
    Region,
    /// Per-window capture — not yet wired through the screencast pipeline.
    Window,
    Screen,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, Default)]
pub enum SaveTarget {
    #[default]
    Pictures,
    Documents,
    Clipboard,
}

const SAVE_TARGETS: [SaveTarget; 3] = [
    SaveTarget::Pictures,
    SaveTarget::Documents,
    SaveTarget::Clipboard,
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

#[derive(Copy, Clone, Eq, PartialEq, Debug, Default)]
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
    layer_id: Option<window::Id>,
}

pub struct Panel {
    core: Core,
    toolbar_id: window::Id,
    mode: Mode,
    source: Source,
    save_target: SaveTarget,
    record_format: RecordFormat,

    rec_fps: u32,
    rec_encoder: VideoEncoder,
    rec_audio: bool,
    rec_cursor: bool,

    sshot_interactive: bool,
    sshot_modal: bool,
    sshot_delay_ms: u64,

    notify: bool,
    clipboard: bool,

    outputs: HashMap<u32, OutputInfo>,
    region: Option<SelectionRect>,
    capture: CaptureState,
}

#[derive(Clone, Debug)]
pub enum Msg {
    SetMode(Mode),
    SetSource(Source),
    SetSaveTarget(usize),
    SetRecordFormat(usize),

    SelectRegion,
    RegionChanged(SelectionRect, DragKind),
    CancelSelection,
    EscPressed,

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
        let toolbar_id = window::Id::unique();
        let panel = Self {
            core,
            toolbar_id,
            mode: Mode::Record,
            source: Source::Region,
            save_target: SaveTarget::default(),
            record_format: RecordFormat::default(),
            rec_fps: 60,
            rec_encoder: VideoEncoder::Auto,
            rec_audio: false,
            rec_cursor: true,
            sshot_interactive: true,
            sshot_modal: true,
            sshot_delay_ms: 0,
            notify: true,
            clipboard: false,
            outputs: HashMap::new(),
            region: None,
            capture: CaptureState::default(),
        };

        // Toolbar is a fullscreen layer surface with the pill aligned to the
        // bottom. Why fullscreen instead of a small bottom-anchored surface:
        //
        //  * Click capture — input_zone is None, so clicks outside the pill
        //    are absorbed by the (transparent) surface instead of leaking to
        //    apps underneath. Matches cosmic-screenshot's modal feel.
        //  * Popups — popup_dropdown spawns its menu as an xdg-popup parented
        //    to the toolbar. The popup needs room to render above the pill;
        //    if the parent surface is only the pill's height, the popup
        //    geometry extends outside the parent and cosmic-comp drops
        //    pointer events on it.
        //
        // Keyboard starts Exclusive so popup selections don't bounce focus
        // to other apps. We drop to None on recording start (see
        // on_primary_action).
        let open_toolbar = get_layer_surface(SctkLayerSurfaceSettings {
            id: toolbar_id,
            layer: Layer::Overlay,
            keyboard_interactivity: KeyboardInteractivity::Exclusive,
            input_zone: None,
            anchor: Anchor::all(),
            output: IcedOutput::Active,
            namespace: "cosmic-capture-toolbar".to_string(),
            size: Some((None, None)),
            exclusive_zone: -1,
            size_limits: Limits::NONE.min_height(1.0).min_width(1.0),
            margin: IcedMargin::default(),
        });

        (panel, open_toolbar)
    }

    fn update(&mut self, msg: Msg) -> Task<Msg> {
        match msg {
            Msg::SetMode(m) => {
                if !self.locked() {
                    self.mode = m;
                }
            }
            Msg::SetSource(s) => {
                if !self.locked() {
                    self.source = s;
                }
            }
            Msg::ToggleClipboard => self.clipboard = !self.clipboard,
            Msg::SetSaveTarget(i) => {
                if let Some(&t) = SAVE_TARGETS.get(i) {
                    self.save_target = t;
                }
            }
            Msg::SetRecordFormat(i) => {
                if let Some(&f) = RECORD_FORMATS.get(i) {
                    self.record_format = f;
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
                                layer_id: None,
                            },
                        );
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
                    }
                    OutputEvent::Removed => {
                        if let Some(info) = self.outputs.remove(&key) {
                            if let Some(id) = info.layer_id {
                                return destroy_layer_surface(id);
                            }
                        }
                    }
                }
            }

            Msg::SelectRegion => {
                // Legacy message kept for the keyboard-wrapper code paths; in
                // the stacked architecture the selector is always live in
                // Region mode, so this is a no-op.
            }
            Msg::RegionChanged(rect, kind) => {
                let n = rect.normalize();
                tracing::trace!(?kind, w = n.width(), h = n.height(), "region updated");
                self.region = Some(rect);
                let _ = kind;
                // Selector stays open at all times so the user can keep
                // resizing via the corner / edge handles. Recording, Esc, or
                // ✕ closes it.
            }
            Msg::CancelSelection => {
                tracing::info!("selection cancelled");
                self.region = None;
                return self.close_selector_surfaces();
            }

            Msg::PrimaryAction => {
                tracing::info!(state = ?self.capture, "Msg::PrimaryAction");
                return self.on_primary_action();
            }
            Msg::EscPressed => match &mut self.capture {
                CaptureState::Recording { stop_tx, .. } => {
                    if let Some(tx) = stop_tx.take() {
                        let _ = tx.send(());
                    }
                    self.capture = CaptureState::Saving;
                }
                CaptureState::Idle => {
                    let any_open = self.outputs.values().any(|o| o.layer_id.is_some());
                    if any_open {
                        return self.close_selector_surfaces();
                    }
                }
                _ => {}
            },

            Msg::RecordingFinished(r) | Msg::GifFinished(r) | Msg::ScreenshotFinished(r) => {
                // Done — close everything and exit. Desktop notifications
                // surface the saved-path / error to the user.
                if let Err(e) = &r {
                    tracing::warn!(error = %e, "capture pipeline finished with error");
                } else if let Ok(p) = &r {
                    tracing::info!(path = %p, "capture pipeline finished");
                }
                let close_selector = self.close_selector_surfaces();
                let close_toolbar = destroy_layer_surface(self.toolbar_id);
                return Task::batch([close_selector, close_toolbar, iced::exit()]);
            }

            Msg::Quit => {
                let close_selector = self.close_selector_surfaces();
                let close_toolbar = destroy_layer_surface(self.toolbar_id);
                return Task::batch([close_selector, close_toolbar, iced::exit()]);
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
        if id == self.toolbar_id {
            return self.view_toolbar();
        }
        // Per-output recording overlay (input_zone is empty so clicks pass
        // through to apps). Renders the red border only.
        let info = self.outputs.values().find(|o| o.layer_id == Some(id));
        let Some(info) = info else {
            return iced::widget::Space::new().into();
        };
        let output_rect = SelectionRect {
            left: info.logical_pos.0,
            top: info.logical_pos.1,
            right: info.logical_pos.0 + info.logical_size.0 as i32,
            bottom: info.logical_pos.1 + info.logical_size.1 as i32,
        };
        let selection = self.region.unwrap_or_default();
        RectangleSelection::new(output_rect, selection, RectMode::Recording, Msg::RegionChanged)
            .into()
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
            iced::Event::Keyboard(keyboard::Event::KeyPressed { key, .. }) => {
                if let Key::Named(Named::Escape) = key {
                    Some(Msg::EscPressed)
                } else {
                    None
                }
            }
            _ => None,
        });
        Subscription::batch([outputs, keys])
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
            (Mode::Screenshot, _) => true,
            (Mode::Record, Source::Screen) => true,
            (Mode::Record, Source::Region) => self.region.is_some(),
            (Mode::Record, Source::Window) => false, // not yet wired
        }
    }

    fn close_selector_surfaces(&mut self) -> Task<Msg> {
        let mut tasks: Vec<Task<Msg>> = Vec::new();
        for info in self.outputs.values_mut() {
            if let Some(id) = info.layer_id.take() {
                tasks.push(destroy_layer_surface(id));
            }
        }
        Task::batch(tasks)
    }

    /// Swap the per-output overlays to a pointer-transparent recording layer
    /// (input_zone is an empty Vec, so all clicks pass through to apps; the
    /// surface just draws the red border).
    fn open_recording_overlays(&mut self) -> Task<Msg> {
        let mut tasks: Vec<Task<Msg>> = Vec::new();
        for info in self.outputs.values_mut() {
            if let Some(old) = info.layer_id.take() {
                tasks.push(destroy_layer_surface(old));
            }
            let id = window::Id::unique();
            info.layer_id = Some(id);
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

    fn on_primary_action(&mut self) -> Task<Msg> {
        match &mut self.capture {
            CaptureState::Recording { stop_tx, .. } => {
                if let Some(tx) = stop_tx.take() {
                    let _ = tx.send(());
                }
                self.capture = CaptureState::Saving;
                Task::none()
            }
            CaptureState::Idle => match self.mode {
                Mode::Screenshot => {
                    let args = self.build_screenshot_args();
                    self.capture = CaptureState::Saving;
                    let close = self.close_selector_surfaces();
                    Task::batch([
                        close,
                        Task::perform(run_screenshot(args), |r| {
                            cosmic::action::app(Msg::ScreenshotFinished(r))
                        }),
                    ])
                }
                Mode::Record => {
                    if !self.can_start() {
                        return Task::none();
                    }
                    let crop = match self.source {
                        Source::Region => self.crop_from_region(),
                        Source::Screen | Source::Window => None,
                    };
                    let swap = self.open_recording_overlays();
                    // Recording → toolbar must stop hogging keyboard so the
                    // user can drive the apps they're capturing.
                    let release_kb =
                        set_keyboard_interactivity(self.toolbar_id, KeyboardInteractivity::None);
                    match self.record_format {
                        RecordFormat::Gif => {
                            let args = self.build_gif_args();
                            self.capture = CaptureState::Recording { stop_tx: None };
                            Task::batch([
                                swap,
                                release_kb,
                                Task::perform(run_gif(args, crop), |r| {
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
                            Task::batch([
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

    fn view_toolbar(&self) -> Element<'_, Msg> {
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
        let sources = row::with_capacity(3)
            .push(source_icon(
                "screenshot-selection-symbolic",
                Source::Region,
                region_press,
            ))
            // Window mode not yet implemented — present but disabled.
            .push(source_icon("screenshot-window-symbolic", Source::Window, None))
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

        let close = button::icon(icon::from_name("window-close-symbolic"))
            .medium()
            .on_press(Msg::Quit);

        // 2px separators that span nearly the full pill height.
        let sep = || {
            iced::widget::rule::vertical(2)
                .class(cosmic::theme::Rule::LightDivider)
                .height(Length::Fixed(56.0))
        };

        let pill = row::with_capacity(9)
            .push(modes)
            .push(sep())
            .push(sources)
            .push(sep())
            .push(action)
            .push(sep())
            .push(options)
            .push(sep())
            .push(close)
            .spacing(10)
            .align_y(iced::Alignment::Center);

        // Styled pill background — matches xdg-desktop-portal-cosmic's
        // screenshot toolbar: opaque component bg with small corner radius.
        // Width fixed so the pill stays compact even though it lives inside
        // a fullscreen surface.
        let styled = container(pill)
            .padding([8, 12, 8, 12])
            .width(Length::Shrink)
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

        // Pill container — fills the surface width/height transparently and
        // pins the styled chip to the bottom-center.
        let pill_layer = container(styled)
            .padding([0, 0, TOOLBAR_BOTTOM_MARGIN, 0])
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(iced::Alignment::Center)
            .align_y(iced::Alignment::End);

        // While in Region mode and not recording, render the selection
        // widget *beneath* the pill in a Stack. iced routes pointer events
        // top-down — the pill (front) absorbs clicks on its bounds, drags
        // anywhere else fall through to the selection widget. No need for
        // separate per-output selector surfaces during the select phase.
        let in_select_phase = matches!(self.source, Source::Region)
            && !matches!(
                self.capture,
                CaptureState::Recording { .. } | CaptureState::Saving
            );
        if in_select_phase {
            let output_rect = self.active_output_rect();
            let sel = RectangleSelection::new(
                output_rect,
                self.region.unwrap_or_default(),
                RectMode::Selecting,
                Msg::RegionChanged,
            );
            iced::widget::Stack::new()
                .push(Element::from(sel))
                .push(pill_layer)
                .into()
        } else {
            pill_layer.into()
        }
    }

    fn active_output_rect(&self) -> SelectionRect {
        // Single-output assumption for now — pick whichever output we have.
        // Multi-output layout (toolbar on the active one, selection widget
        // on each) can be layered on later.
        self.outputs
            .values()
            .next()
            .map(|info| SelectionRect {
                left: info.logical_pos.0,
                top: info.logical_pos.1,
                right: info.logical_pos.0 + info.logical_size.0 as i32,
                bottom: info.logical_pos.1 + info.logical_size.1 as i32,
            })
            .unwrap_or_default()
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
            fps: self.rec_fps,
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
    fn build_screenshot_args(&self) -> ScreenshotArgs {
        let (file, clipboard) = match self.save_target {
            SaveTarget::Pictures => (None, false),
            SaveTarget::Documents => {
                let dest = dirs::document_dir().map(|d| {
                    let stem = chrono::Local::now().format("cosmic-capture-%Y%m%d-%H%M%S");
                    d.join(format!("{stem}.png"))
                });
                (dest, false)
            }
            SaveTarget::Clipboard => (None, true),
        };
        ScreenshotArgs {
            common: CommonArgs {
                file,
                notify: self.notify,
                clipboard,
            },
            interactive: self.sshot_interactive,
            modal: self.sshot_modal,
            delay_ms: self.sshot_delay_ms,
        }
    }
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

    let inner_size = if recording { 14.0 } else { 18.0 };
    let inner_radius: f32 = if recording { 3.0 } else { inner_size / 2.0 };
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
        .map_err(|e| e.to_string())
}
async fn run_gif(args: GifArgs, crop: Option<CropRect>) -> Result<String, String> {
    pipeline::gif::gif_with_crop(args, crop)
        .await
        .map(path_to_string)
        .map_err(|e| e.to_string())
}
async fn run_screenshot(args: ScreenshotArgs) -> Result<String, String> {
    pipeline::screenshot::run(args)
        .await
        .map(|_| String::from("(saved)"))
        .map_err(|e| e.to_string())
}
fn path_to_string(p: PathBuf) -> String {
    p.to_string_lossy().into_owned()
}

fn wayland_proxy_id(o: &WlOutput) -> u32 {
    use wayland_client::Proxy;
    o.id().protocol_id()
}
