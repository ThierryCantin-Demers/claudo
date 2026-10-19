//! Moving through a transcript without the wheel: a page at a time, to either end, back
//! to the bottom from a button, or carried along by the middle button.

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
        if self.done {
            return;
        }
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
    // The transcript is the first scrollable reached; what it holds need not be visited.
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        if !self.done {
            operate(self);
        }
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

/// Whether the scrollable it holds has left its bottom, past a scroll too small to mean it.
#[derive(Default)]
struct FarUp(Option<bool>);

impl Operation for FarUp {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        if self.0.is_none() {
            operate(self);
        }
    }

    fn scrollable(
        &mut self,
        _id: Option<&widget::Id>,
        bounds: Rectangle,
        content: Rectangle,
        translation: Vector,
        _state: &mut dyn Scrollable,
    ) {
        let from_bottom = content.height - bounds.height - translation.y;
        self.0.get_or_insert(from_bottom > LEFT_BOTTOM);
    }
}

/// Holds a transcript, and over it a button back to the bottom once it is scrolled up.
/// A middle click starts it scrolling toward the pointer, faster the farther the pointer
/// moves from where it was clicked; another click stops it, and so does letting go of the
/// button after moving with it held.
pub struct Scroller<'a> {
    /// The transcript, then the button.
    children: [Element<'a, Message>; 2],
}

impl<'a> Scroller<'a> {
    pub fn new(
        transcript: impl Into<Element<'a, Message>>,
        back: impl Into<Element<'a, Message>>,
    ) -> Self {
        Self {
            children: [transcript.into(), back.into()],
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
    /// The button back to the bottom shows.
    far_up: bool,
}

/// Pixels a second for a pointer `dy` below the anchor, toward the top when negative.
fn speed(dy: f32) -> f32 {
    let d = (dy.abs() - DEAD_ZONE).max(0.0);
    (6.0 * d + 0.04 * d * d).copysign(dy)
}

/// How far up from the bottom a transcript is before the button back shows: a line or two.
const LEFT_BOTTOM: f32 = 40.0;

/// Room between the button and the transcript's bottom edge.
const BUTTON_MARGIN: f32 = 16.0;

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Scroller<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut self.children);
    }

    fn size(&self) -> Size<Length> {
        self.children[0].as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let [transcript, back] = &mut self.children;
        let transcript = transcript
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        let size = transcript.size();
        let back = back.as_widget_mut().layout(
            &mut tree.children[1],
            renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        let at = Point::new(
            (size.width - back.size().width) / 2.0,
            size.height - back.size().height - BUTTON_MARGIN,
        );
        layout::Node::with_children(size, vec![transcript, back.move_to(at)])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.children[0].as_widget_mut().operate(
            &mut tree.children[0],
            layout.child(0),
            renderer,
            operation,
        );
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
        let [transcript, back] = &mut self.children;
        let (transcript_layout, back_layout) = (layout.child(0), layout.child(1));
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
                        far_up: state.far_up,
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
                transcript.as_widget_mut().operate(
                    &mut tree.children[0],
                    transcript_layout,
                    renderer,
                    &mut once,
                );
                shell.request_redraw();
            }
            _ => {}
        }
        let taken_before = shell.is_event_captured();
        if state.far_up {
            back.as_widget_mut().update(
                &mut tree.children[1],
                event,
                back_layout,
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
        }
        // A press on the button is not one on the text under it.
        if taken_before || !shell.is_event_captured() {
            transcript.as_widget_mut().update(
                &mut tree.children[0],
                event,
                transcript_layout,
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
        }
        let mut far_up = FarUp::default();
        transcript.as_widget_mut().operate(
            &mut tree.children[0],
            transcript_layout,
            renderer,
            &mut far_up,
        );
        let state = tree.state.downcast_mut::<State>();
        let far_up = far_up.0.unwrap_or(false);
        if far_up != state.far_up {
            state.far_up = far_up;
            shell.request_redraw();
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
        let state = tree.state.downcast_ref::<State>();
        if state.anchor.is_some() {
            return mouse::Interaction::ResizingVertically;
        }
        let [transcript, back] = &self.children;
        if state.far_up && cursor.is_over(layout.child(1).bounds()) {
            return back.as_widget().mouse_interaction(
                &tree.children[1],
                layout.child(1),
                cursor,
                viewport,
                renderer,
            );
        }
        transcript.as_widget().mouse_interaction(
            &tree.children[0],
            layout.child(0),
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
        let [transcript, back] = &self.children;
        transcript.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout.child(0),
            cursor,
            viewport,
        );
        let state = tree.state.downcast_ref::<State>();
        if state.far_up {
            renderer.with_layer(layout.bounds(), |renderer| {
                back.as_widget().draw(
                    &tree.children[1],
                    renderer,
                    theme,
                    style,
                    layout.child(1),
                    cursor,
                    viewport,
                );
            });
        }
        let Some(anchor) = state.anchor else {
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
        let far_up = tree.state.downcast_ref::<State>().far_up;
        let [transcript, back] = &mut self.children;
        let [transcript_tree, back_tree] = &mut tree.children[..] else {
            return None;
        };
        let mut overlays: Vec<_> = transcript
            .as_widget_mut()
            .overlay(
                transcript_tree,
                layout.child(0),
                renderer,
                viewport,
                translation,
            )
            .into_iter()
            .collect();
        // A tooltip still open when the button went would float over nothing.
        if far_up {
            overlays.extend(back.as_widget_mut().overlay(
                back_tree,
                layout.child(1),
                renderer,
                viewport,
                translation,
            ));
        }
        match overlays.len() {
            0 => None,
            1 => overlays.pop(),
            _ => Some(overlay::Group::with_children(overlays).overlay()),
        }
    }
}

impl<'a> From<Scroller<'a>> for Element<'a, Message> {
    fn from(scroller: Scroller<'a>) -> Self {
        Element::new(scroller)
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
