//! Captures keyboard events that bubble up past inner content.
//!
//! Direct port of `xdg-desktop-portal-cosmic`'s widget of the same name —
//! a thin wrapper widget that forwards content events through and, if the
//! inner content didn't capture the event, hands the keypress to a callback
//! that may turn it into a Message.

use cosmic::iced::core::event::Event;
use cosmic::iced::core::widget::{Operation, Tree};
use cosmic::iced::core::{
    Clipboard, Element, Layout, Length, Rectangle, Shell, Size, Widget, keyboard, layout, mouse,
    overlay, renderer,
};

#[allow(missing_debug_implementations)]
pub struct KeyboardWrapper<'a, Message> {
    content: Element<'a, Message, cosmic::Theme, cosmic::Renderer>,
    handler: fn(keyboard::Key, keyboard::Modifiers) -> Option<Message>,
}

impl<'a, Message> KeyboardWrapper<'a, Message> {
    pub fn new(
        content: impl Into<Element<'a, Message, cosmic::Theme, cosmic::Renderer>>,
        handler: fn(keyboard::Key, keyboard::Modifiers) -> Option<Message>,
    ) -> Self {
        Self { content: content.into(), handler }
    }
}

impl<'a, Message> Widget<Message, cosmic::Theme, cosmic::Renderer> for KeyboardWrapper<'a, Message>
where
    Message: Clone,
{
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation<()>,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &cosmic::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if shell.is_event_captured() {
            return;
        }

        if let Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = event {
            if let Some(message) = (self.handler)(key.clone(), *modifiers) {
                shell.publish(message.clone());
                shell.capture_event();
            }
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        renderer_style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            renderer_style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &cosmic::Renderer,
        viewport: &Rectangle,
        translation: cosmic::iced::Vector,
    ) -> Option<overlay::Element<'b, Message, cosmic::Theme, cosmic::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message> From<KeyboardWrapper<'a, Message>>
    for Element<'a, Message, cosmic::Theme, cosmic::Renderer>
where
    Message: 'a + Clone,
{
    fn from(w: KeyboardWrapper<'a, Message>) -> Self {
        Element::new(w)
    }
}
