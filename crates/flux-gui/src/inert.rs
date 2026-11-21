//! An event-inert wrapper for views that sit underneath a modal overlay.
//!
//! Iced's [`Stack`](iced::widget::stack) forwards pointer events to lower
//! layers unless the upper layer captures them. A modal backdrop only
//! captures presses, so cursor movement would still reach covered Track rows
//! and change their hover styling. [`inert`] freezes a layer completely: it
//! draws its content, but receives no events and always paints with an
//! unavailable cursor, so nothing underneath a modal can react to the pointer.

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{Operation, Tree};
use iced::advanced::{Clipboard, Shell, Widget};
use iced::mouse;
use iced::{Element, Event, Length, Rectangle, Size};

/// Wraps `content` so it renders without responding to input.
pub fn inert<'a, Message, Theme, Renderer>(
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
) -> Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    Element::new(Inert {
        content: content.into(),
    })
}

struct Inert<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for Inert<'_, Message, Theme, Renderer>
where
    Renderer: iced::advanced::Renderer,
{
    fn tag(&self) -> iced::advanced::widget::tree::Tag {
        self.content.as_widget().tag()
    }

    fn state(&self) -> iced::advanced::widget::tree::State {
        self.content.as_widget().state()
    }

    fn children(&self) -> Vec<Tree> {
        self.content.as_widget().children()
    }

    fn diff(&self, tree: &mut Tree) {
        self.content.as_widget().diff(tree);
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &iced::advanced::renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        // An unavailable cursor means covered buttons cannot paint a hover or
        // pressed state, even though the layer stays visible behind a
        // translucent backdrop.
        self.content.as_widget().draw(
            tree,
            renderer,
            theme,
            style,
            layout,
            mouse::Cursor::Unavailable,
            viewport,
        );
    }

    fn operate(
        &mut self,
        _tree: &mut Tree,
        _layout: Layout<'_>,
        _renderer: &Renderer,
        _operation: &mut dyn Operation,
    ) {
        // Operations (focus, scroll access, accessibility) must not reach a
        // covered layer while a modal owns the interaction.
    }

    fn update(
        &mut self,
        _tree: &mut Tree,
        _event: &Event,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        _shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
    }

    fn mouse_interaction(
        &self,
        _state: &Tree,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        mouse::Interaction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::advanced::renderer::{Quad, Style};
    use iced::advanced::widget::tree;
    use iced::{Background, Point, Transformation};

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Msg {
        Clicked,
    }

    #[derive(Debug, Clone, Copy)]
    struct TestRenderer;

    // Only the handful of operations the probe and inert wrapper touch are
    // meaningful; everything else is a no-op recording sink.
    impl iced::advanced::Renderer for TestRenderer {
        fn start_layer(&mut self, _bounds: Rectangle) {}
        fn end_layer(&mut self) {}
        fn start_transformation(&mut self, _transformation: Transformation) {}
        fn end_transformation(&mut self) {}
        fn fill_quad(&mut self, _quad: Quad, _background: impl Into<Background>) {}
        fn reset(&mut self, _new_bounds: Rectangle) {}
        fn allocate_image(
            &mut self,
            _handle: &iced::advanced::image::Handle,
            _callback: impl FnOnce(
                Result<iced::advanced::image::Allocation, iced::advanced::image::Error>,
            ) + Send
            + 'static,
        ) {
        }
    }

    /// Minimal widget that reacts to every event, so a wrapper that fails to
    /// block input is immediately visible as a published message.
    struct Probe;

    impl Widget<Msg, (), TestRenderer> for Probe {
        fn size(&self) -> Size<Length> {
            Size::new(Length::Fixed(100.0), Length::Fixed(40.0))
        }

        fn layout(
            &mut self,
            _tree: &mut Tree,
            _renderer: &TestRenderer,
            limits: &layout::Limits,
        ) -> layout::Node {
            layout::Node::new(limits.resolve(
                Length::Fixed(100.0),
                Length::Fixed(40.0),
                Size::new(100.0, 40.0),
            ))
        }

        fn draw(
            &self,
            _tree: &Tree,
            _renderer: &mut TestRenderer,
            _theme: &(),
            _style: &Style,
            _layout: Layout<'_>,
            _cursor: mouse::Cursor,
            _viewport: &Rectangle,
        ) {
        }

        fn update(
            &mut self,
            _tree: &mut Tree,
            _event: &Event,
            _layout: Layout<'_>,
            _cursor: mouse::Cursor,
            _renderer: &TestRenderer,
            _clipboard: &mut dyn Clipboard,
            shell: &mut Shell<'_, Msg>,
            _viewport: &Rectangle,
        ) {
            shell.publish(Msg::Clicked);
        }

        fn mouse_interaction(
            &self,
            _state: &Tree,
            _layout: Layout<'_>,
            _cursor: mouse::Cursor,
            _viewport: &Rectangle,
            _renderer: &TestRenderer,
        ) -> mouse::Interaction {
            mouse::Interaction::Pointer
        }

        fn tag(&self) -> tree::Tag {
            tree::Tag::stateless()
        }

        fn state(&self) -> tree::State {
            tree::State::None
        }

        fn children(&self) -> Vec<Tree> {
            Vec::new()
        }

        fn diff(&self, _tree: &mut Tree) {}
    }

    #[test]
    fn inert_layer_swallows_pointer_events_and_hover() {
        let mut element: Element<'_, Msg, (), TestRenderer> = inert(Element::new(Probe));
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(400.0, 400.0));
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &TestRenderer, &limits);
        let layout = Layout::new(&node);
        let renderer = TestRenderer;
        let cursor = mouse::Cursor::Available(Point::new(10.0, 10.0));
        let viewport = Rectangle::INFINITE;

        for event in [
            Event::Mouse(mouse::Event::CursorMoved {
                position: Point::new(10.0, 10.0),
            }),
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        ] {
            let mut messages = Vec::new();
            let mut clipboard = iced::advanced::clipboard::Null;
            let mut shell = Shell::new(&mut messages);
            element.as_widget_mut().update(
                &mut tree,
                &event,
                layout,
                cursor,
                &renderer,
                &mut clipboard,
                &mut shell,
                &viewport,
            );
            let captured = shell.is_event_captured();
            assert!(
                messages.is_empty(),
                "covered layers must not react to {event:?}"
            );
            assert!(
                !captured,
                "the inert layer must not take ownership of modal routing"
            );
        }

        assert_eq!(
            Widget::<Msg, (), TestRenderer>::mouse_interaction(
                element.as_widget(),
                &tree,
                layout,
                cursor,
                &viewport,
                &renderer,
            ),
            mouse::Interaction::None,
            "a covered layer must not suggest pointer interactivity"
        );
    }
}
