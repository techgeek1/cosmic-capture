//! GPU-rendered region-selection widget for layer-shell surfaces.
//!
//! Per-output layer-shell surfaces don't share pointer focus — when the cursor
//! crosses from one output to another, the originating widget stops receiving
//! `mouse::CursorMoved` events. To keep selection working across monitor
//! boundaries we drive motion via wayland DnD: on press the widget calls
//! `start_dnd`, every per-output widget registers itself as a DnD destination,
//! and `OfferEvent::Motion` events deliver coordinates inside whichever
//! surface the cursor is currently over.
//!
//! Drag state lives in the parent (`Panel`) and is fed back into each widget
//! on render so all surfaces share a consistent view of the in-flight drag.
//!
//! Drawing strategy:
//!
//!  * Selection border drawn as a `Quad` clipped to this output's bounds.
//!  * Corner handles drawn as small rounded `Quad`s.
//!  * No dim overlay (per user preference).

use std::borrow::Cow;

use cosmic::iced::clipboard::dnd::{
    self, DndAction, DndDestinationRectangle, DndEvent, OfferEvent, SourceEvent,
};
use cosmic::iced::clipboard::mime::{AllowedMimeTypes, AsMimeTypes};
use cosmic::iced::core::clipboard::DndSource;
use cosmic::iced::core::layout::Node;
use cosmic::iced::core::renderer::Quad;
use cosmic::iced::core::{
    Border, Color, Element, Length, Point, Rectangle, Renderer as _, Shadow, Size, layout, mouse,
    widget::{Tree, tree},
    window,
};
use cosmic::iced::Event;
use cosmic::widget::Widget;

/// Output-relative integer rect.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        (self.right - self.left).abs()
    }
    pub fn height(&self) -> i32 {
        (self.bottom - self.top).abs()
    }
    pub fn normalize(&self) -> Rect {
        Rect {
            left: self.left.min(self.right),
            top: self.top.min(self.bottom),
            right: self.left.max(self.right),
            bottom: self.top.max(self.bottom),
        }
    }
}

/// Which edge/corner is the user currently dragging (or `None` if no drag is
/// in progress). `New` is the first drag from an empty selection — mapped to
/// `SE` for the rect math but kept as a distinct entry so callers can
/// distinguish "user is making a fresh selection" from "user is resizing".
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragKind {
    #[default]
    None,
    New,
    NW,
    N,
    NE,
    E,
    SE,
    S,
    SW,
    W,
    Move,
}

/// Visual mode — Selecting shows the accent border + handles; Recording shows
/// only a thin red border (no handles) for the active-capture indicator.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum Mode {
    #[default]
    Selecting,
    Recording,
}

const EDGE_GRAB: f32 = 8.0;
const CORNER_DIAM: f32 = 16.0;
const BORDER_W: f32 = 3.0;
const HANDLE_RADIUS: f32 = CORNER_DIAM / 2.0;

/// MIME type used to carry the synthetic DnD payload. The actual bytes don't
/// matter — we just need a registered type so the compositor will deliver
/// DnD events. Namespaced to avoid colliding with anything else on the
/// clipboard.
pub const DND_MIME: &str = "X-COSMIC-CAPTURE-SelectionDrag";

/// Synthetic DnD payload. Required by iced's `start_dnd` API but unused.
struct SelectionDragData;

impl AllowedMimeTypes for SelectionDragData {
    fn allowed() -> Cow<'static, [String]> {
        Cow::Owned(vec![DND_MIME.to_string()])
    }
}

impl From<(Vec<u8>, String)> for SelectionDragData {
    fn from(_: (Vec<u8>, String)) -> Self {
        SelectionDragData
    }
}

impl AsMimeTypes for SelectionDragData {
    fn available(&self) -> Cow<'static, [String]> {
        Cow::Owned(vec![DND_MIME.to_string()])
    }
    fn as_bytes(&self, _: &str) -> Option<Cow<'static, [u8]>> {
        Some(Cow::Borrowed(b"selection"))
    }
}

/// Anchor + starting rect for an in-flight drag, lifted out of the widget so
/// every per-output widget can compute the new rect consistently when the
/// cursor crosses surfaces.
#[derive(Copy, Clone, Debug)]
pub struct DragSession {
    pub kind: DragKind,
    pub start_rect: Rect,
    /// Cursor position in compositor-global coords at the moment of press.
    pub anchor: (i32, i32),
}

pub struct RectangleSelection<'a, Msg> {
    /// Output bounds in compositor-global coords.
    output_rect: Rect,
    /// Current selection in compositor-global coords.
    selection: Rect,
    mode: Mode,
    /// Shared DnD operation id — must match across all per-output widgets so
    /// they listen to the same drag.
    dnd_id: u128,
    /// `window::Id` of the surface hosting this widget — needed by
    /// `clipboard.start_dnd` when this widget initiates a drag.
    window_id: window::Id,
    /// Active drag (None when idle). Parent feeds this in on each render so
    /// every widget has the same view of the in-flight drag.
    drag: Option<DragSession>,
    on_event: Box<dyn Fn(WidgetEvent) -> Msg + 'a>,
}

/// Events the widget hands back to the parent.
#[derive(Clone, Debug)]
pub enum WidgetEvent {
    DragStart {
        session: DragSession,
        initial_rect: Rect,
    },
    DragMove(Rect),
    DragEnd,
}

impl<'a, Msg> RectangleSelection<'a, Msg> {
    pub fn new(
        output_rect: Rect,
        selection: Rect,
        mode: Mode,
        dnd_id: u128,
        window_id: window::Id,
        drag: Option<DragSession>,
        on_event: impl Fn(WidgetEvent) -> Msg + 'a,
    ) -> Self {
        Self {
            output_rect,
            selection,
            mode,
            dnd_id,
            window_id,
            drag,
            on_event: Box::new(on_event),
        }
    }

    /// Selection translated into this output's local coordinate frame (so
    /// (0,0) is the top-left of the output, not the desktop).
    fn local_selection(&self) -> Rectangle {
        let s = self.selection.normalize();
        Rectangle::new(
            Point::new(
                (s.left - self.output_rect.left) as f32,
                (s.top - self.output_rect.top) as f32,
            ),
            Size::new(s.width() as f32, s.height() as f32),
        )
    }

    fn hit_test(&self, cursor: mouse::Cursor) -> DragKind {
        let r = self.local_selection();
        if r.width <= 0.0 || r.height <= 0.0 {
            return DragKind::New;
        }
        let make = |x: f32, y: f32| {
            Rectangle::new(
                Point::new(x - CORNER_DIAM / 2.0, y - CORNER_DIAM / 2.0),
                Size::new(CORNER_DIAM, CORNER_DIAM),
            )
        };
        if cursor.is_over(make(r.x, r.y)) {
            return DragKind::NW;
        }
        if cursor.is_over(make(r.x + r.width, r.y)) {
            return DragKind::NE;
        }
        if cursor.is_over(make(r.x, r.y + r.height)) {
            return DragKind::SW;
        }
        if cursor.is_over(make(r.x + r.width, r.y + r.height)) {
            return DragKind::SE;
        }
        let n = Rectangle::new(
            Point::new(r.x, r.y - EDGE_GRAB / 2.0),
            Size::new(r.width, EDGE_GRAB),
        );
        if cursor.is_over(n) {
            return DragKind::N;
        }
        let s = Rectangle::new(
            Point::new(r.x, r.y + r.height - EDGE_GRAB / 2.0),
            Size::new(r.width, EDGE_GRAB),
        );
        if cursor.is_over(s) {
            return DragKind::S;
        }
        let w = Rectangle::new(
            Point::new(r.x - EDGE_GRAB / 2.0, r.y),
            Size::new(EDGE_GRAB, r.height),
        );
        if cursor.is_over(w) {
            return DragKind::W;
        }
        let e = Rectangle::new(
            Point::new(r.x + r.width - EDGE_GRAB / 2.0, r.y),
            Size::new(EDGE_GRAB, r.height),
        );
        if cursor.is_over(e) {
            return DragKind::E;
        }
        if cursor.is_over(r) {
            return DragKind::Move;
        }
        DragKind::None
    }

    /// Compute the new global-coord rect given the current cursor in global
    /// coords. Uses the active drag session's anchor + start_rect.
    fn drag_to(&self, gx: i32, gy: i32) -> Rect {
        let Some(session) = self.drag else {
            return self.selection;
        };
        let s = session.start_rect.normalize();
        let dx = gx - session.anchor.0;
        let dy = gy - session.anchor.1;
        match session.kind {
            DragKind::Move => Rect {
                left: s.left + dx,
                top: s.top + dy,
                right: s.right + dx,
                bottom: s.bottom + dy,
            },
            DragKind::NW => Rect {
                left: gx,
                top: gy,
                right: s.right,
                bottom: s.bottom,
            },
            DragKind::N => Rect {
                left: s.left,
                top: gy,
                right: s.right,
                bottom: s.bottom,
            },
            DragKind::NE => Rect {
                left: s.left,
                top: gy,
                right: gx,
                bottom: s.bottom,
            },
            DragKind::E => Rect {
                left: s.left,
                top: s.top,
                right: gx,
                bottom: s.bottom,
            },
            DragKind::SE | DragKind::New => Rect {
                left: s.left,
                top: s.top,
                right: gx,
                bottom: gy,
            },
            DragKind::S => Rect {
                left: s.left,
                top: s.top,
                right: s.right,
                bottom: gy,
            },
            DragKind::SW => Rect {
                left: gx,
                top: s.top,
                right: s.right,
                bottom: gy,
            },
            DragKind::W => Rect {
                left: gx,
                top: s.top,
                right: s.right,
                bottom: s.bottom,
            },
            DragKind::None => s,
        }
    }
}

impl<'a, Msg: 'a + Clone> Widget<Msg, cosmic::Theme, cosmic::Renderer>
    for RectangleSelection<'a, Msg>
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<()>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> Node {
        Node::new(limits.max())
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        _layout: layout::Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        if matches!(self.mode, Mode::Recording) {
            return mouse::Interaction::default();
        }
        if self.drag.is_some() {
            return mouse::Interaction::Grabbing;
        }
        match self.hit_test(cursor) {
            DragKind::None | DragKind::New => mouse::Interaction::Crosshair,
            DragKind::NW | DragKind::SE => mouse::Interaction::ResizingDiagonallyDown,
            DragKind::NE | DragKind::SW => mouse::Interaction::ResizingDiagonallyUp,
            DragKind::N | DragKind::S => mouse::Interaction::ResizingVertically,
            DragKind::E | DragKind::W => mouse::Interaction::ResizingHorizontally,
            DragKind::Move => mouse::Interaction::Grab,
        }
    }

    fn update(
        &mut self,
        _tree: &mut Tree,
        event: &Event,
        layout: layout::Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &cosmic::Renderer,
        clipboard: &mut dyn cosmic::iced::core::Clipboard,
        shell: &mut cosmic::iced::core::Shell<'_, Msg>,
        _viewport: &Rectangle,
    ) {
        if matches!(self.mode, Mode::Recording) {
            return;
        }

        match event {
            // DnD-driven cross-surface motion. Fires on every surface the
            // cursor enters during the active drag, including the source.
            Event::Dnd(DndEvent::Offer(id, e)) if *id == Some(self.dnd_id) => {
                if self.drag.is_none() {
                    return;
                }
                match e {
                    OfferEvent::Enter { x, y, .. } | OfferEvent::Motion { x, y } => {
                        let gx = (*x).round() as i32 + self.output_rect.left;
                        let gy = (*y).round() as i32 + self.output_rect.top;
                        let new_rect = self.drag_to(gx, gy);
                        shell.publish((self.on_event)(WidgetEvent::DragMove(new_rect)));
                        shell.capture_event();
                    }
                    OfferEvent::Drop => {
                        shell.publish((self.on_event)(WidgetEvent::DragEnd));
                        shell.capture_event();
                    }
                    _ => {}
                }
            }
            // Source-side completion — fires when the user releases the mouse
            // regardless of which surface they're over.
            Event::Dnd(DndEvent::Source(e)) => {
                if matches!(
                    e,
                    SourceEvent::Finished | SourceEvent::Cancelled | SourceEvent::Dropped
                ) && self.drag.is_some()
                {
                    shell.publish((self.on_event)(WidgetEvent::DragEnd));
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if !cursor.is_over(layout.bounds()) {
                    return;
                }
                if self.drag.is_some() {
                    return; // Already in a drag, started elsewhere.
                }
                let pos = cursor.position().unwrap_or_default();
                let gx = pos.x as i32 + self.output_rect.left;
                let gy = pos.y as i32 + self.output_rect.top;
                let hit = self.hit_test(cursor);
                let kind = match hit {
                    DragKind::None | DragKind::New => DragKind::New,
                    other => other,
                };
                let (start_rect, initial_rect) = if matches!(kind, DragKind::New) {
                    let r = Rect {
                        left: gx,
                        top: gy,
                        right: gx,
                        bottom: gy,
                    };
                    (r, r)
                } else {
                    (self.selection, self.selection)
                };
                let session = DragSession {
                    kind,
                    start_rect,
                    anchor: (gx, gy),
                };
                // Kick off the wayland DnD so future motion events cross
                // surface boundaries cleanly.
                clipboard.start_dnd(
                    false,
                    Some(DndSource::Surface(self.window_id)),
                    None,
                    Box::new(SelectionDragData),
                    DndAction::Copy,
                );
                shell.publish((self.on_event)(WidgetEvent::DragStart {
                    session,
                    initial_rect,
                }));
                shell.capture_event();
            }
            _ => {}
        }
    }

    fn drag_destinations(
        &self,
        _state: &Tree,
        layout: layout::Layout<'_>,
        _renderer: &cosmic::Renderer,
        dnd_rectangles: &mut cosmic::iced::core::clipboard::DndDestinationRectangles,
    ) {
        // Register the entire surface as a DnD destination for `dnd_id`, so
        // the compositor delivers OfferEvent::Motion to us whenever the
        // cursor sits anywhere on this output during a selection drag.
        let bounds = layout.bounds();
        dnd_rectangles.push(DndDestinationRectangle {
            id: self.dnd_id,
            rectangle: dnd::Rectangle {
                x: bounds.x as f64,
                y: bounds.y as f64,
                width: bounds.width as f64,
                height: bounds.height as f64,
            },
            mime_types: vec![Cow::Borrowed(DND_MIME)],
            actions: DndAction::Copy,
            preferred: DndAction::Copy,
        });
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        _style: &cosmic::iced::core::renderer::Style,
        _layout: layout::Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let cosmic_theme = theme.cosmic();
        let accent = Color::from(cosmic_theme.accent_color());
        let inner = self.local_selection();
        let output_size = Size::new(
            self.output_rect.width() as f32,
            self.output_rect.height() as f32,
        );
        let output_rect = Rectangle::new(Point::new(0.0, 0.0), output_size);

        let border_color = match self.mode {
            Mode::Selecting => accent,
            Mode::Recording => Color::from_rgb(0.93, 0.20, 0.20),
        };

        if inner.width > 0.0 && inner.height > 0.0 {
            // In Recording mode, push the border BORDER_W pixels outside
            // the region rect so the visible stroke lives entirely
            // outside the captured area. iced renders the border inset
            // (bounds.x..bounds.x+width occupies the stroke), so growing
            // bounds outward by BORDER_W lands those pixels at
            // (inner.x - BORDER_W .. inner.x), excluded by a crop that
            // matches the region exactly. Selecting keeps the original
            // inset shape — there's no recording, so no need to cheat
            // for a crop.
            let bounds = match self.mode {
                Mode::Selecting => inner,
                Mode::Recording => Rectangle {
                    x: inner.x - BORDER_W,
                    y: inner.y - BORDER_W,
                    width: inner.width + BORDER_W * 2.0,
                    height: inner.height + BORDER_W * 2.0,
                },
            };
            let clipped = bounds.intersection(&output_rect).unwrap_or(bounds);
            // Same subtle 4px corner radius for both modes — small
            // enough that the transparent gaps at each corner of the
            // recording frame are essentially invisible, large enough
            // that the on-screen affordance reads as a softened rect.
            let radius = 4.0;
            renderer.fill_quad(
                Quad {
                    bounds: clipped,
                    border: Border {
                        radius: radius.into(),
                        width: BORDER_W,
                        color: border_color,
                    },
                    shadow: Shadow::default(),
                    snap: true,
                },
                Color::TRANSPARENT,
            );
        }

        if matches!(self.mode, Mode::Selecting) && inner.width > 0.0 && inner.height > 0.0 {
            for (x, y) in [
                (inner.x, inner.y),
                (inner.x + inner.width, inner.y),
                (inner.x, inner.y + inner.height),
                (inner.x + inner.width, inner.y + inner.height),
            ] {
                if !output_rect.contains(Point::new(x, y)) {
                    continue;
                }
                renderer.fill_quad(
                    Quad {
                        bounds: Rectangle::new(
                            Point::new(x - CORNER_DIAM / 2.0, y - CORNER_DIAM / 2.0),
                            Size::new(CORNER_DIAM, CORNER_DIAM),
                        ),
                        border: Border {
                            radius: HANDLE_RADIUS.into(),
                            width: 0.0,
                            color: Color::TRANSPARENT,
                        },
                        shadow: Shadow::default(),
                        snap: true,
                    },
                    border_color,
                );
            }
        }
    }
}

impl<'a, Msg: 'a + Clone> From<RectangleSelection<'a, Msg>>
    for Element<'a, Msg, cosmic::Theme, cosmic::Renderer>
{
    fn from(w: RectangleSelection<'a, Msg>) -> Self {
        Element::new(w)
    }
}
