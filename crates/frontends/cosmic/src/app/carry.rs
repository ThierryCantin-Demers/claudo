//! A tile moving to another window keeps its transcript scrolled where it was. Widget
//! state lives in each window's own tree, so the new window would start it over.

use super::*;
use cosmic::iced::Rectangle;
use cosmic::iced::advanced::widget::operate;
use cosmic::iced::advanced::widget::operation::{Outcome, Scrollable};
use cosmic::iced::widget::scrollable::AbsoluteOffset;

/// Where a conversation's tile goes.
#[derive(Debug, Clone, Copy)]
pub enum Destination {
    NewWindow,
    Window(window::Id),
    /// Dropped on a window's tile row, `x` from the window's left.
    Drop {
        window: window::Id,
        x: f32,
    },
}

pub(super) fn tile_id(conversation: &str) -> widget::Id {
    widget::Id::new(format!("tile-{conversation}"))
}

/// Tracks whether an operation is inside one tile, which `Intercept` reports as a container.
struct InTile {
    tile: widget::Id,
    entering: bool,
    inside: bool,
}

impl InTile {
    fn new(conversation: &str) -> Self {
        Self {
            tile: tile_id(conversation),
            entering: false,
            inside: false,
        }
    }

    fn container(&mut self, id: Option<&widget::Id>) {
        self.entering = id == Some(&self.tile);
    }

    /// Going into a widget's children; returns whether the operation was inside before.
    fn enter(&mut self) -> bool {
        let outer = self.inside;
        self.inside |= std::mem::take(&mut self.entering);
        outer
    }
}

/// How far from the bottom a tile's transcript is scrolled, in whichever window shows it.
struct ReadScroll {
    tile: InTile,
    from_bottom: Option<f32>,
}

impl Operation<Option<f32>> for ReadScroll {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Option<f32>>)) {
        let outer = self.tile.enter();
        operate(self);
        self.tile.inside = outer;
    }

    fn container(&mut self, id: Option<&widget::Id>, _bounds: Rectangle) {
        self.tile.container(id);
    }

    // The transcript is the tile's outermost scrollable, reported before what it holds.
    fn scrollable(
        &mut self,
        _id: Option<&widget::Id>,
        bounds: Rectangle,
        content_bounds: Rectangle,
        translation: cosmic::iced::Vector,
        _state: &mut dyn Scrollable,
    ) {
        if self.tile.inside && self.from_bottom.is_none() {
            self.from_bottom =
                Some((content_bounds.height - bounds.height - translation.y).max(0.0));
        }
    }

    fn finish(&self) -> Outcome<Option<f32>> {
        Outcome::Some(self.from_bottom)
    }
}

/// Scrolls a tile's transcript to a distance from the bottom: transcripts are anchored
/// there, so an absolute offset counts from it.
struct RestoreScroll {
    tile: InTile,
    from_bottom: f32,
    done: bool,
}

impl Operation<bool> for RestoreScroll {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<bool>)) {
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
        _bounds: Rectangle,
        _content_bounds: Rectangle,
        _translation: cosmic::iced::Vector,
        state: &mut dyn Scrollable,
    ) {
        if self.tile.inside && !self.done {
            state.scroll_to(AbsoluteOffset {
                x: None,
                y: Some(self.from_bottom),
            });
            self.done = true;
        }
    }

    fn finish(&self) -> Outcome<bool> {
        Outcome::Some(self.done)
    }
}

impl ClaudoApp {
    /// Move a tile to another window once its transcript's scroll is read.
    pub(super) fn carry(&self, id: ConversationId, to: Destination) -> Task<Message> {
        operate(ReadScroll {
            tile: InTile::new(&id),
            from_bottom: None,
        })
        .map(move |from_bottom| cosmic::Action::App(Message::Carried(id.clone(), to, from_bottom)))
    }

    /// Scroll a moved tile's transcript back, once its window has drawn it.
    pub(super) fn restore_scroll(&self, id: &ConversationId) -> Task<Message> {
        let Some(&from_bottom) = self.carried_scroll.get(id) else {
            return Task::none();
        };
        let id = id.clone();
        operate(RestoreScroll {
            tile: InTile::new(&id),
            from_bottom,
            done: false,
        })
        .map(move |done| cosmic::Action::App(Message::ScrollRestored(id.clone(), done)))
    }

    pub(super) fn arrive(&mut self, id: ConversationId, to: Destination) {
        match to {
            Destination::NewWindow => self.open_in_new_window(id),
            Destination::Window(w) => self.move_to_window(id, w),
            Destination::Drop { window, x } => {
                self.current = window;
                self.drop_tile(id, x);
            }
        }
    }
}

#[cfg(test)]
pub(super) fn read_scroll(conversation: &str) -> impl Operation<Option<f32>> {
    ReadScroll {
        tile: InTile::new(conversation),
        from_bottom: None,
    }
}

#[cfg(test)]
pub(super) fn scroll_back(conversation: &str, from_bottom: f32) -> impl Operation<bool> {
    RestoreScroll {
        tile: InTile::new(conversation),
        from_bottom,
        done: false,
    }
}
