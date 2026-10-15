//! Dragging a tile out of its window: past the window's edge the drag becomes a Wayland
//! drag and drop, since a window learns nothing of the cursor outside it, and any claudo
//! window's tile row takes the drop.

use std::borrow::Cow;
use std::hash::{Hash, Hasher};

use super::*;
use cosmic::iced::Rectangle;
use cosmic::iced::advanced::clipboard::{DndSource, IconSurface, start_dnd};
use cosmic::iced::clipboard::dnd::{DndAction, DndEvent, SourceEvent};
use cosmic::iced::clipboard::mime::AsMimeTypes;

pub(super) const TILE_MIME: &str = "application/x-claudo-tile";

/// The conversation a dragged tile shows.
struct TileData(ConversationId);

impl AsMimeTypes for TileData {
    fn available(&self) -> Cow<'static, [String]> {
        Cow::Owned(vec![TILE_MIME.to_string()])
    }

    fn as_bytes(&self, mime_type: &str) -> Option<Cow<'static, [u8]>> {
        (mime_type == TILE_MIME).then(|| Cow::Owned(self.0.clone().into_bytes()))
    }
}

/// A window's tile row as a drop target. The id stays the same from frame to frame,
/// since the compositor addresses the drag's events to it.
pub(super) fn drop_target_id(w: window::Id) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    w.hash(&mut hasher);
    hasher.finish()
}

/// What the cursor carries once a tile leaves its window.
pub(super) struct Carried {
    pub conversation: ConversationId,
    pub title: String,
    pub latest: Option<String>,
    pub size: Size,
}

/// A window's content, which hands a tile drag over to the compositor when the cursor
/// leaves the window, and reports how that drag ended.
pub(super) struct TearOff<'a> {
    pub content: Element<'a, Message>,
    pub window: window::Id,
    /// The window's size; the cursor past it has left.
    pub bounds: Size,
    pub carried: Option<Carried>,
}

#[derive(Default)]
struct Away(bool);

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for TearOff<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Away>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Away::default())
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
        event: &cosmic::iced::Event,
        layout: Layout<'_>,
        cursor: cosmic::iced::mouse::Cursor,
        renderer: &cosmic::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        use cosmic::iced::Event;
        use cosmic::iced::mouse::Event as Mouse;
        let away = &mut tree.state.downcast_mut::<Away>().0;
        match event {
            Event::Mouse(Mouse::CursorMoved { position })
                if !*away && !Rectangle::with_size(self.bounds).contains(*position) =>
            {
                if let Some(carried) = &self.carried {
                    let card = drag_icon(carried);
                    let offset = cosmic::iced::Vector::new(
                        -carried.size.width / 2.0,
                        -carried.size.height / 2.0,
                    );
                    start_dnd::<cosmic::Theme, cosmic::Renderer>(
                        clipboard,
                        false,
                        Some(DndSource::Surface(self.window)),
                        Some(IconSurface::new(card, tree::State::None, offset)),
                        Box::new(TileData(carried.conversation.clone())),
                        DndAction::Move,
                    );
                    *away = true;
                    shell.publish(Message::TileAway);
                    shell.capture_event();
                    return;
                }
            }
            Event::Dnd(DndEvent::Source(SourceEvent::Finished)) if *away => {
                *away = false;
                shell.publish(Message::TileLanded);
            }
            Event::Dnd(DndEvent::Source(SourceEvent::Cancelled)) if *away => {
                *away = false;
                if let Some(carried) = &self.carried {
                    shell.publish(Message::TileDroppedOutside(carried.conversation.clone()));
                }
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
        cursor: cosmic::iced::mouse::Cursor,
        viewport: &Rectangle,
        renderer: &cosmic::Renderer,
    ) -> cosmic::iced::mouse::Interaction {
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
        cursor: cosmic::iced::mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
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

    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        dnd_rectangles: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        self.content.as_widget().drag_destinations(
            &tree.children[0],
            layout,
            renderer,
            dnd_rectangles,
        );
    }
}

impl<'a> From<TearOff<'a>> for Element<'a, Message> {
    fn from(tear_off: TearOff<'a>) -> Self {
        Element::new(tear_off)
    }
}

/// The lifted card, drawn by the compositor under the cursor outside the window.
fn drag_icon(carried: &Carried) -> Element<'static, ()> {
    let mut column: Vec<Element<'static, ()>> = vec![
        widget::text::heading(carried.title.clone())
            .wrapping(text::Wrapping::None)
            .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
            .into(),
    ];
    if let Some(latest) = &carried.latest {
        column.push(
            widget::text(latest.clone())
                .size(13.0)
                .class(dim())
                .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Height(
                    carried.size.height * 0.6,
                )))
                .into(),
        );
    }
    widget::container(widget::Column::with_children(column).spacing(6))
        .padding(14)
        .width(Length::Fixed(carried.size.width))
        .height(Length::Fixed(carried.size.height))
        .clip(true)
        .class(cosmic::theme::Container::custom(|theme| {
            let cosmic = theme.cosmic();
            container::Style {
                background: Some(Color::from(cosmic.bg_component_color()).into()),
                text_color: Some(cosmic.on_bg_component_color().into()),
                border: Border {
                    radius: cosmic.corner_radii.radius_m.into(),
                    width: 1.5,
                    color: cosmic.accent_color().into(),
                },
                ..container::Style::default()
            }
        }))
        .into()
}
