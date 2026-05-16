//! GPU-rendered region-selection widget for layer-shell surfaces.
//!
//! Adapted from `xdg-desktop-portal-cosmic`'s `RectangleSelection`, simplified
//! to use iced `Mouse` events directly instead of DnD-based motion tracking
//! (the layer surface covers the full output, so cursor never leaves the
//! widget's bounds during a drag — no need for the DnD workaround).
//!
//! Drawing strategy:
//!  * Dim overlay drawn as 8 triangles surrounding the selection rect (mesh).
//!  * Selection border drawn as a `Quad` with width = `BORDER`.
//!  * Corner handles drawn as small rounded `Quad`s.
//! All paths go through the renderer → wgpu → compositor via dmabuf, so
//! there's no per-frame CPU pixel work and we hit the display's native rate
//! regardless of resolution.

use cosmic::iced::core::layout::Node;
use cosmic::iced::core::renderer::Quad;
use cosmic::iced::core::{
    Border, Color, Element, Length, Point, Rectangle, Renderer as _, Shadow, Size,
    layout,
    mouse,
    widget::{Tree, tree},
};
use cosmic::iced::Event;
use cosmic::widget::Widget;

/// Output-relative integer rect.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
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
/// in progress). `New` is the first drag from an empty selection.
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

/// Visual mode — Selecting shows the dim overlay + handles, Recording shows
/// only a thin border (no handles, no dim) for the active-capture indicator.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum Mode {
    #[default]
    Selecting,
    Recording,
}

const EDGE_GRAB: f32 = 8.0;
const CORNER_DIAM: f32 = 16.0;
const BORDER_W: f32 = 3.0;
// Half the corner box — produces a full circle.
const HANDLE_RADIUS: f32 = CORNER_DIAM / 2.0;

pub struct RectangleSelection<'a, Msg> {
    /// Output bounds in compositor-global coords.
    output_rect: Rect,
    /// Current selection in compositor-global coords (i.e. comparable across
    /// outputs). May be entirely or partially outside `output_rect`.
    selection: Rect,
    mode: Mode,
    on_change: Box<dyn Fn(Rect, DragKind) -> Msg + 'a>,
}

impl<'a, Msg> RectangleSelection<'a, Msg> {
    pub fn new(
        output_rect: Rect,
        selection: Rect,
        mode: Mode,
        on_change: impl Fn(Rect, DragKind) -> Msg + 'a,
    ) -> Self {
        Self {
            output_rect,
            selection,
            mode,
            on_change: Box::new(on_change),
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
        if cursor.is_over(make(r.x, r.y)) { return DragKind::NW; }
        if cursor.is_over(make(r.x + r.width, r.y)) { return DragKind::NE; }
        if cursor.is_over(make(r.x, r.y + r.height)) { return DragKind::SW; }
        if cursor.is_over(make(r.x + r.width, r.y + r.height)) { return DragKind::SE; }
        // Edges.
        let n = Rectangle::new(Point::new(r.x, r.y - EDGE_GRAB / 2.0), Size::new(r.width, EDGE_GRAB));
        if cursor.is_over(n) { return DragKind::N; }
        let s = Rectangle::new(
            Point::new(r.x, r.y + r.height - EDGE_GRAB / 2.0),
            Size::new(r.width, EDGE_GRAB),
        );
        if cursor.is_over(s) { return DragKind::S; }
        let w = Rectangle::new(Point::new(r.x - EDGE_GRAB / 2.0, r.y), Size::new(EDGE_GRAB, r.height));
        if cursor.is_over(w) { return DragKind::W; }
        let e = Rectangle::new(
            Point::new(r.x + r.width - EDGE_GRAB / 2.0, r.y),
            Size::new(EDGE_GRAB, r.height),
        );
        if cursor.is_over(e) { return DragKind::E; }
        // Inside → move.
        if cursor.is_over(r) { return DragKind::Move; }
        DragKind::None
    }
}

#[derive(Default)]
struct State {
    /// Active drag in progress (kind + per-drag anchor info).
    drag: Option<ActiveDrag>,
}

#[derive(Clone, Copy)]
struct ActiveDrag {
    kind: DragKind,
    /// For Move: cursor anchor at press, in global coords.
    anchor: (i32, i32),
    /// For Move: rect at press time.
    start_rect: Rect,
}

impl<'a, Msg: 'a + Clone> Widget<Msg, cosmic::Theme, cosmic::Renderer> for RectangleSelection<'a, Msg> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
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
        tree: &Tree,
        _layout: layout::Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        if matches!(self.mode, Mode::Recording) {
            return mouse::Interaction::default();
        }
        let state = tree.state.downcast_ref::<State>();
        if state.drag.is_some() {
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
        tree: &mut Tree,
        event: &Event,
        layout: layout::Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &cosmic::Renderer,
        _clipboard: &mut dyn cosmic::iced::core::Clipboard,
        shell: &mut cosmic::iced::core::Shell<'_, Msg>,
        _viewport: &Rectangle,
    ) {
        if matches!(self.mode, Mode::Recording) {
            return;
        }
        let state = tree.state.downcast_mut::<State>();
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if !cursor.is_over(layout.bounds()) {
                    return;
                }
                let kind = self.hit_test(cursor);
                let pos = cursor.position().unwrap_or_default();
                let gx = pos.x as i32 + self.output_rect.left;
                let gy = pos.y as i32 + self.output_rect.top;
                let new_kind = if matches!(kind, DragKind::None | DragKind::New) {
                    // Start fresh selection at the cursor.
                    DragKind::SE
                } else {
                    kind
                };
                let start_rect = match new_kind {
                    DragKind::SE if matches!(kind, DragKind::None | DragKind::New) => {
                        // Fresh draw: anchor NW at click, SE at click.
                        let r = Rect { left: gx, top: gy, right: gx + 1, bottom: gy + 1 };
                        shell.publish((self.on_change)(r, DragKind::SE));
                        r
                    }
                    _ => self.selection,
                };
                state.drag = Some(ActiveDrag { kind: new_kind, anchor: (gx, gy), start_rect });
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let Some(ad) = state.drag.as_mut() else { return };
                let pos = cursor.position().unwrap_or_default();
                let gx = pos.x as i32 + self.output_rect.left;
                let gy = pos.y as i32 + self.output_rect.top;
                let new_rect = apply_drag(ad, gx, gy);
                shell.publish((self.on_change)(new_rect, ad.kind));
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if state.drag.take().is_some() {
                    // Release does NOT commit — caller is responsible for
                    // initiating capture via a separate UI action. Just
                    // notify that the drag has ended so the parent can
                    // refresh its UI from `DragKind::None`.
                    shell.publish((self.on_change)(self.selection, DragKind::None));
                    shell.capture_event();
                }
            }
            _ => {}
        }
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
            // Standard "recording" red.
            Mode::Recording => Color::from_rgb(0.93, 0.20, 0.20),
        };

        // Selection border.
        if inner.width > 0.0 && inner.height > 0.0 {
            let clipped = inner.intersection(&output_rect).unwrap_or(inner);
            renderer.fill_quad(
                Quad {
                    bounds: clipped,
                    border: Border {
                        radius: 4.0.into(),
                        width: BORDER_W,
                        color: border_color,
                    },
                    shadow: Shadow::default(),
                    snap: true,
                },
                Color::TRANSPARENT,
            );
        }

        // Corner handles (Selecting only).
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

fn apply_drag(ad: &ActiveDrag, gx: i32, gy: i32) -> Rect {
    let s = ad.start_rect.normalize();
    let dx = gx - ad.anchor.0;
    let dy = gy - ad.anchor.1;
    match ad.kind {
        DragKind::Move => Rect {
            left: s.left + dx,
            top: s.top + dy,
            right: s.right + dx,
            bottom: s.bottom + dy,
        },
        DragKind::NW => Rect { left: gx, top: gy, right: s.right, bottom: s.bottom },
        DragKind::N => Rect { left: s.left, top: gy, right: s.right, bottom: s.bottom },
        DragKind::NE => Rect { left: s.left, top: gy, right: gx, bottom: s.bottom },
        DragKind::E => Rect { left: s.left, top: s.top, right: gx, bottom: s.bottom },
        DragKind::SE => Rect { left: s.left, top: s.top, right: gx, bottom: gy },
        DragKind::S => Rect { left: s.left, top: s.top, right: s.right, bottom: gy },
        DragKind::SW => Rect { left: gx, top: s.top, right: s.right, bottom: gy },
        DragKind::W => Rect { left: gx, top: s.top, right: s.right, bottom: s.bottom },
        DragKind::None | DragKind::New => s,
    }
}

impl<'a, Msg: 'a + Clone> From<RectangleSelection<'a, Msg>>
    for Element<'a, Msg, cosmic::Theme, cosmic::Renderer>
{
    fn from(w: RectangleSelection<'a, Msg>) -> Self {
        Element::new(w)
    }
}
