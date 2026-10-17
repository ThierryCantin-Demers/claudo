//! A transcript entry that skips its diff and layout while nothing it shows has changed.
//! Every frame of a streaming reply rebuilds the whole view, so without it the cost of a
//! frame grows with the length of every open conversation.

use super::*;

/// Wraps content whose widget tree and size depend only on `key`. Anything that changes
/// either must change the key; what only changes how it is drawn need not.
pub struct Frozen<'a> {
    key: u64,
    content: Element<'a, Message>,
}

impl<'a> Frozen<'a> {
    pub fn new(key: u64, content: impl Into<Element<'a, Message>>) -> Self {
        Self {
            key,
            content: content.into(),
        }
    }
}

#[derive(Default)]
struct State {
    key: Option<u64>,
    laid_out: Option<(layout::Limits, layout::Node)>,
}

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Frozen<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    // Holds no named widget: libcosmic only hands a named state back during a diff.
    fn diff(&mut self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State>();
        if state.key == Some(self.key) {
            return;
        }
        state.key = Some(self.key);
        state.laid_out = None;
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
        let state = tree.state.downcast_mut::<State>();
        if let Some((laid_for, node)) = &state.laid_out
            && laid_for == limits
        {
            return node.clone();
        }
        let node = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        tree.state.downcast_mut::<State>().laid_out = Some((*limits, node.clone()));
        node
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
        viewport: &cosmic::iced::Rectangle,
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
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: cosmic::iced::mouse::Cursor,
        viewport: &cosmic::iced::Rectangle,
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
        viewport: &cosmic::iced::Rectangle,
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
        viewport: &cosmic::iced::Rectangle,
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

impl<'a> From<Frozen<'a>> for Element<'a, Message> {
    fn from(frozen: Frozen<'a>) -> Self {
        Element::new(frozen)
    }
}

/// What decides an entry's widgets and size; `live` is an animated thinking row.
pub fn key(view: &ConversationView, idx: usize, entry: &Entry, live: bool, zoom: f32) -> u64 {
    use std::hash::{DefaultHasher, Hasher};
    let mut h = DefaultHasher::new();
    (view.generation, idx, zoom.to_bits()).hash(&mut h);
    std::mem::discriminant(entry).hash(&mut h);
    match entry {
        Entry::User(text) | Entry::Notice(text) => text.len().hash(&mut h),
        Entry::Incoming { from, text } => (from.len(), text.len()).hash(&mut h),
        Entry::Compacted { compaction, open } => (compaction.summary.len(), open).hash(&mut h),
        Entry::Reply(reply) => reply.parsed.hash(&mut h),
        Entry::Ended(time) => time.hash(&mut h),
        Entry::File(file) => (file.attachment.name.len(), file.attachment.data.len()).hash(&mut h),
        Entry::Thinking { text, open } => (text.len(), open, live).hash(&mut h),
        Entry::Tool { call, open } => {
            let background = call
                .background
                .as_ref()
                .map(|b| (b.status as u8, b.output.as_ref().map(String::len)));
            (
                open,
                call.result.as_ref().map(String::len),
                call.is_error,
                background,
                commands::running(call, view.streaming),
            )
                .hash(&mut h)
        }
    }
    h.finish()
}
