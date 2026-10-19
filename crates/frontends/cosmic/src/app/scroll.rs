//! Moving through a transcript without the wheel: a page at a time, to either end, or
//! carried along by the middle button.

use std::time::Instant;

use super::carry::InTile;
use super::*;
use cosmic::iced::advanced::widget::operate;
use cosmic::iced::advanced::widget::operation::Scrollable;
use cosmic::iced::widget::scrollable::AbsoluteOffset;
use cosmic::iced::{Event, Rectangle, Vector, mouse};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Jump {
    PageUp,
    PageDown,
    Top,
    Bottom,
}

/// The share of a transcript's height a page moves, keeping a few lines in view.
const PAGE: f32 = 0.85;

/// How far the pointer goes from where the middle button was pressed before anything scrolls.
const DEAD_ZONE: f32 = 12.0;

/// The jump a key asks for. Shift, Alt and Super belong to the editor's selection and to the
/// desktop, and Ctrl+PageUp to the tabs some apps have.
pub fn jump_key(key: &keyboard::Key, modifiers: keyboard::Modifiers) -> Option<Jump> {
    use keyboard::key::Named;
    if modifiers.shift() || modifiers.alt() || modifiers.logo() {
        return None;
    }
    match (key, modifiers.control()) {
        (keyboard::Key::Named(Named::PageUp), false) => Some(Jump::PageUp),
        (keyboard::Key::Named(Named::PageDown), false) => Some(Jump::PageDown),
        (keyboard::Key::Named(Named::Home), _) => Some(Jump::Top),
        (keyboard::Key::Named(Named::End), _) => Some(Jump::Bottom),
        _ => None,
    }
}

/// Scrolls the first scrollable an operation reaches by `y`, upward when positive:
/// transcripts are anchored at the bottom, so their offset counts from it.
fn scroll_by(state: &mut dyn Scrollable, y: f32, bounds: Rectangle, content: Rectangle) {
    state.scroll_by(AbsoluteOffset { x: 0.0, y }, bounds, content);
}

/// Moves one tile's transcript, found by the tile's id.
struct JumpIn {
    tile: InTile,
    jump: Jump,
    done: bool,
}

impl Operation for JumpIn {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        let outer = self.tile.enter();
        operate(self);
        self.tile.inside = outer;
    }

    fn container(&mut self, id: Option<&widget::Id>, _bounds: Rectangle) {
        self.tile.container(id);
    }

    fn scrollable(
        &mut self,
        _id: Option<&widget::Id>,
        bounds: Rectangle,
        content: Rectangle,
        _translation: Vector,
        state: &mut dyn Scrollable,
    ) {
        if self.tile.inside && !self.done {
            self.done = true;
            let y = match self.jump {
                Jump::PageUp => bounds.height * PAGE,
                Jump::PageDown => -bounds.height * PAGE,
                Jump::Top => content.height,
                Jump::Bottom => -content.height,
            };
            scroll_by(state, y, bounds, content);
        }
    }
}

pub(super) fn jump_in(conversation: &str, jump: Jump) -> impl Operation + use<> {
    JumpIn {
        tile: InTile::new(conversation),
        jump,
        done: false,
    }
}

impl ClaudoApp {
    pub(super) fn jump(&self, conversation: &str, jump: Jump) -> Task<Message> {
        operate(jump_in(conversation, jump)).discard()
    }
}

/// The scrollable it holds, scrolled by `y` once.
struct ScrollOnce {
    y: f32,
    done: bool,
}

impl Operation for ScrollOnce {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn scrollable(
        &mut self,
        _id: Option<&widget::Id>,
        bounds: Rectangle,
        content: Rectangle,
        _translation: Vector,
        state: &mut dyn Scrollable,
    ) {
        if !self.done {
            self.done = true;
            scroll_by(state, self.y, bounds, content);
        }
    }
}

/// Holds a transcript. A middle click starts it scrolling toward the pointer, faster the
/// farther the pointer moves from where it was clicked; another click stops it, and so does
/// letting go of the button after moving with it held.
pub struct Autoscroll<'a> {
    content: Element<'a, Message>,
}

impl<'a> Autoscroll<'a> {
    pub fn new(content: impl Into<Element<'a, Message>>) -> Self {
        Self {
            content: content.into(),
        }
    }
}

#[derive(Default)]
struct State {
    /// Where the middle click was, while it scrolls.
    anchor: Option<Point>,
    pointer: Point,
    held: bool,
    /// The pointer left the dead zone with the button held, so letting go stops.
    dragged: bool,
    last_frame: Option<Instant>,
}

/// Pixels a second for a pointer `dy` below the anchor, toward the top when negative.
fn speed(dy: f32) -> f32 {
    let d = (dy.abs() - DEAD_ZONE).max(0.0);
    (6.0 * d + 0.04 * d * d).copysign(dy)
}

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Autoscroll<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

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
        operation: &mut dyn Operation,
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
        let state = tree.state.downcast_mut::<State>();
        let stop = |state: &mut State, shell: &mut Shell<'_, Message>| {
            state.anchor = None;
            shell.request_redraw();
        };
        match (event, state.anchor) {
            (Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Middle)), None) => {
                if let Some(at) = cursor.position_over(layout.bounds()) {
                    *state = State {
                        anchor: Some(at),
                        pointer: at,
                        held: true,
                        ..State::default()
                    };
                    shell.capture_event();
                    shell.request_redraw();
                    return;
                }
            }
            (Event::Mouse(mouse::Event::ButtonPressed(_)), Some(_)) => {
                stop(state, shell);
                shell.capture_event();
                return;
            }
            (Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Middle)), Some(_)) => {
                state.held = false;
                if state.dragged {
                    stop(state, shell);
                }
                shell.capture_event();
                return;
            }
            (Event::Mouse(mouse::Event::CursorMoved { position }), Some(anchor)) => {
                state.pointer = *position;
                state.dragged |= state.held && (position.y - anchor.y).abs() > DEAD_ZONE;
            }
            (
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Escape),
                    ..
                }),
                Some(_),
            ) => {
                stop(state, shell);
                shell.capture_event();
                return;
            }
            (
                Event::Mouse(mouse::Event::WheelScrolled { .. })
                | Event::Window(window::Event::Unfocused),
                Some(_),
            ) => stop(state, shell),
            (Event::Window(window::Event::RedrawRequested(now)), Some(anchor)) => {
                let seconds = state
                    .last_frame
                    .map_or(0.0, |last| now.duration_since(last).as_secs_f32())
                    .min(0.1);
                state.last_frame = Some(*now);
                let mut once = ScrollOnce {
                    y: -speed(state.pointer.y - anchor.y) * seconds,
                    done: false,
                };
                self.content.as_widget_mut().operate(
                    &mut tree.children[0],
                    layout,
                    renderer,
                    &mut once,
                );
                shell.request_redraw();
            }
            _ => {}
        }
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
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        if tree.state.downcast_ref::<State>().anchor.is_some() {
            return mouse::Interaction::ResizingVertically;
        }
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
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        use cosmic::iced::advanced::Renderer as _;
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
        let Some(anchor) = tree.state.downcast_ref::<State>().anchor else {
            return;
        };
        let cosmic = theme.cosmic();
        let accent: Color = cosmic.accent_color().into();
        let mark = |radius: f32| Rectangle {
            x: anchor.x - radius,
            y: anchor.y - radius,
            width: 2.0 * radius,
            height: 2.0 * radius,
        };
        renderer.with_layer(layout.bounds(), |renderer| {
            renderer.fill_quad(
                renderer::Quad {
                    bounds: mark(14.0),
                    border: Border {
                        radius: 14.0.into(),
                        width: 1.5,
                        color: accent,
                    },
                    ..renderer::Quad::default()
                },
                Color::from(cosmic.bg_component_color()),
            );
            renderer.fill_quad(
                renderer::Quad {
                    bounds: mark(3.0),
                    border: Border {
                        radius: 3.0.into(),
                        ..Border::default()
                    },
                    ..renderer::Quad::default()
                },
                accent,
            );
        });
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &cosmic::Renderer,
        viewport: &Rectangle,
        translation: Vector,
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

impl<'a> From<Autoscroll<'a>> for Element<'a, Message> {
    fn from(autoscroll: Autoscroll<'a>) -> Self {
        Element::new(autoscroll)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pointer_scrolls_faster_the_farther_it_goes() {
        assert_eq!(speed(DEAD_ZONE), 0.0);
        assert_eq!(speed(-DEAD_ZONE / 2.0), 0.0);
        assert!(speed(60.0) > 0.0 && speed(-60.0) < 0.0);
        assert!(speed(400.0) > 10.0 * speed(60.0));
    }
}
