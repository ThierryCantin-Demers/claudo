//! Text that can be selected and copied, which iced's own text widgets cannot be. Each
//! block of a transcript reports presses and drags as positions in the transcript, and
//! draws whatever part of the transcript's selection falls inside it.

use std::any::Any;
use std::cell::Cell;
use std::sync::Arc;

use claudo_protocol::ConversationId;
use cosmic::iced::advanced::graphics::text::{Paragraph, cosmic_text};
use cosmic::iced::advanced::mouse::{Click, click};
use cosmic::iced::advanced::text::{
    Alignment, Difference, Ellipsize, LineHeight, Paragraph as _, Renderer as _, Shaping, Span,
    Text, Wrapping,
};
use cosmic::iced::advanced::widget::operation::Outcome;
use cosmic::iced::advanced::widget::{Operation, Tree, tree};
use cosmic::iced::advanced::{Clipboard, Layout, Renderer as _, Shell, Widget, layout, renderer};
use cosmic::iced::{Color, Event, Font, Length, Pixels, Point, Rectangle, Size, alignment, mouse};
use cosmic::widget::{self, markdown};
use cosmic::{Element, Theme};

use super::Message;
use super::furigana::{self, Ruby};

/// A place in a transcript: a block of text, as its entry and its position within the
/// entry, and a byte offset into the block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Mark {
    pub block: (usize, usize),
    pub offset: usize,
}

/// Text selected in one conversation's transcript, from where the press began to where
/// the cursor is or was let go.
#[derive(Debug, Clone)]
pub struct Selection {
    pub conversation: ConversationId,
    pub anchor: Mark,
    pub focus: Mark,
    pub dragging: bool,
}

impl Selection {
    pub fn is_empty(&self) -> bool {
        self.anchor == self.focus
    }

    /// The bytes of `block` it covers; an end past the block's text means all of it.
    fn within(&self, conversation: &str, block: (usize, usize)) -> Option<(usize, usize)> {
        if self.conversation != conversation || self.is_empty() {
            return None;
        }
        let (start, end) = if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        };
        if block < start.block || block > end.block {
            return None;
        }
        let from = if block == start.block {
            start.offset
        } else {
            0
        };
        let to = if block == end.block {
            end.offset
        } else {
            usize::MAX
        };
        Some((from, to))
    }
}

/// A block's text: its own, or a parsed reply's, shared rather than copied every frame.
pub enum Spans<'a> {
    Own(Vec<Span<'a, markdown::Uri>>),
    Shared(Arc<[Span<'static, markdown::Uri>]>),
}

impl<'a> Spans<'a> {
    fn as_slice(&self) -> &[Span<'a, markdown::Uri>] {
        match self {
            Spans::Own(spans) => spans,
            Spans::Shared(spans) => spans,
        }
    }
}

impl<'a> From<Vec<Span<'a, markdown::Uri>>> for Spans<'a> {
    fn from(spans: Vec<Span<'a, markdown::Uri>>) -> Self {
        Spans::Own(spans)
    }
}

impl From<Arc<[Span<'static, markdown::Uri>]>> for Spans<'_> {
    fn from(spans: Arc<[Span<'static, markdown::Uri>]>) -> Self {
        Spans::Shared(spans)
    }
}

/// A block of rich text in a transcript.
pub struct Selectable<'a> {
    spans: Spans<'a>,
    size: f32,
    font: Option<Font>,
    color: Option<Color>,
    width: Length,
    wrapping: Wrapping,
    /// Put between this block and the one before it when a selection is copied.
    joiner: &'static str,
    conversation: &'a ConversationId,
    block: (usize, usize),
    selected: Option<(usize, usize)>,
    dragging: bool,
}

impl<'a> Selectable<'a> {
    pub fn new(
        spans: impl Into<Spans<'a>>,
        size: f32,
        conversation: &'a ConversationId,
        block: (usize, usize),
        selection: Option<&Selection>,
    ) -> Self {
        Self {
            spans: spans.into(),
            size,
            font: None,
            color: None,
            width: Length::Fill,
            wrapping: Wrapping::Word,
            joiner: "\n\n",
            conversation,
            block,
            selected: selection.and_then(|s| s.within(conversation, block)),
            dragging: selection.is_some_and(|s| s.dragging && s.conversation == *conversation),
        }
    }

    pub fn plain(
        text: &'a str,
        size: f32,
        conversation: &'a ConversationId,
        block: (usize, usize),
        selection: Option<&Selection>,
    ) -> Self {
        Self::new(vec![Span::new(text)], size, conversation, block, selection)
    }

    pub fn font(mut self, font: Font) -> Self {
        self.font = Some(font);
        self
    }

    pub fn color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }

    pub fn shrink(mut self) -> Self {
        self.width = Length::Shrink;
        self
    }

    fn joiner(mut self, joiner: &'static str) -> Self {
        self.joiner = joiner;
        self
    }

    fn line(mut self) -> Self {
        self.width = Length::Shrink;
        self.wrapping = Wrapping::None;
        self.joiner = "\n";
        self
    }
}

struct State {
    spans: Vec<Span<'static, markdown::Uri>>,
    paragraph: Paragraph,
    /// Readings to draw over the paragraph's words, which it lays out without them.
    rubies: Vec<Ruby>,
    last_click: Option<Click>,
    pressed_link: Option<usize>,
    hovered_link: Option<usize>,
    /// The offset last reported while dragging, so moves within a glyph send nothing.
    last_offset: Option<usize>,
}

/// The selected part of one block, handed to `Collect`.
struct SelectedText {
    text: String,
    joiner: &'static str,
}

impl Widget<Message, Theme, cosmic::Renderer> for Selectable<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State {
            spans: Vec::new(),
            paragraph: Paragraph::default(),
            rubies: Vec::new(),
            last_click: None,
            pressed_link: None,
            hovered_link: None,
            last_offset: None,
        })
    }

    fn size(&self) -> Size<Length> {
        Size::new(self.width, Length::Shrink)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let state = tree.state.downcast_mut::<State>();
        let font = self.font.unwrap_or_else(|| renderer.default_font());
        let size = Pixels(self.size);
        let wrapping = self.wrapping;
        let spans = self.spans.as_slice();
        layout::sized(limits, self.width, Length::Shrink, |limits| {
            let bounds = limits.max();
            if state.spans != spans {
                let (shown, rubies) = furigana::split(spans);
                state.rubies = rubies;
                let line_height = line_height(&state.rubies);
                state.paragraph = Paragraph::with_spans(text(
                    &shown[..],
                    bounds,
                    size,
                    line_height,
                    font,
                    wrapping,
                ));
                state.spans = spans.iter().cloned().map(Span::to_static).collect();
            } else {
                let line_height = line_height(&state.rubies);
                match state
                    .paragraph
                    .compare(text((), bounds, size, line_height, font, wrapping))
                {
                    Difference::None => {}
                    Difference::Bounds => state.paragraph.resize(bounds),
                    Difference::Shape => {
                        let (shown, _) = furigana::split(spans);
                        state.paragraph = Paragraph::with_spans(text(
                            &shown[..],
                            bounds,
                            size,
                            line_height,
                            font,
                            wrapping,
                        ));
                    }
                }
            }
            let lift = lift(&state.rubies, self.size);
            let laid = state.paragraph.min_bounds();
            Size::new(laid.width, laid.height - 2.0 * lift)
        })
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut cosmic::Renderer,
        theme: &Theme,
        defaults: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let state = tree.state.downcast_ref::<State>();
        let lift = lift(&state.rubies, self.size);
        let painted = Rectangle {
            y: bounds.y - 2.0 * lift,
            height: bounds.height + 2.0 * lift,
            ..bounds
        };
        if !painted.intersects(viewport) {
            return;
        }
        let buffer = state.paragraph.buffer();
        // Where the paragraph's own top is: its first line where it would be without
        // readings, which hang over the space above.
        let origin = Point::new(bounds.x, bounds.y - lift);

        if let Some((from, to)) = self.selected {
            let len = text_len(buffer);
            let (from, to) = (from.min(len), to.min(len));
            if from < to {
                let accent: Color = theme.cosmic().accent_color().into();
                let (start, end) = (cursor_at(buffer, from), cursor_at(buffer, to));
                for run in buffer.layout_runs() {
                    for (x, width) in run.highlight(start, end) {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds: Rectangle::new(
                                    Point::new(origin.x + x, origin.y + run.line_top),
                                    Size::new(width, run.line_height),
                                ),
                                ..renderer::Quad::default()
                            },
                            Color { a: 0.35, ..accent },
                        );
                    }
                }
            }
        }

        let color = self.color.unwrap_or(defaults.text_color);
        if let Some(link) = state.hovered_link {
            for region in state.paragraph.span_bounds(link) {
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle::new(
                            Point::new(
                                origin.x + region.x,
                                origin.y + region.y + region.height - 2.0,
                            ),
                            Size::new(region.width, 1.0),
                        ),
                        ..renderer::Quad::default()
                    },
                    color,
                );
            }
        }
        renderer.fill_paragraph(&state.paragraph, origin, color, *viewport);
        let font = self.font.unwrap_or_else(|| renderer.default_font());
        for ruby in &state.rubies {
            let (start, end) = (
                cursor_at(buffer, ruby.base.start),
                cursor_at(buffer, ruby.base.end),
            );
            // A word broken over two lines gets its reading over the first part.
            let Some((run, (x, width))) = buffer
                .layout_runs()
                .find_map(|run| Some((run.line_y, run.highlight(start, end).next()?)))
            else {
                continue;
            };
            let size = self.size * READING;
            renderer.fill_text(
                Text {
                    content: ruby.reading.clone(),
                    // Unbounded, cosmic-text wraps after every character.
                    bounds: Size::new(
                        size * 2.0 * ruby.reading.chars().count() as f32 + size,
                        size * 2.0,
                    ),
                    size: Pixels(size),
                    line_height: LineHeight::default(),
                    font,
                    align_x: Alignment::Center,
                    align_y: alignment::Vertical::Bottom,
                    shaping: Shaping::Advanced,
                    wrapping: Wrapping::None,
                    ellipsize: Ellipsize::None,
                },
                Point::new(
                    origin.x + x + width / 2.0,
                    origin.y + run - self.size * 0.95,
                ),
                color,
                *viewport,
            );
        }
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &cosmic::Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State>();
        let lift = lift(&state.rubies, self.size);
        let position = cursor
            .position_in(layout.bounds())
            .map(|point| Point::new(point.x, point.y + lift));
        let link_at = |state: &State, point: Point| {
            state.paragraph.hit_span(point).filter(|&span| {
                self.spans
                    .as_slice()
                    .get(span)
                    .is_some_and(|s| s.link.is_some())
            })
        };
        let mark = |offset| Mark {
            block: self.block,
            offset,
        };

        match event {
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let hovered = position.and_then(|point| link_at(state, point));
                if hovered != state.hovered_link {
                    state.hovered_link = hovered;
                    shell.request_redraw();
                }
                if self.dragging
                    && let Some(point) = position
                {
                    let offset = offset_at(&state.paragraph, point);
                    if state.last_offset != Some(offset) {
                        state.last_offset = Some(offset);
                        shell.publish(Message::SelectTo(mark(offset)));
                    }
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(point) = position else {
                    return;
                };
                if let Some(link) = link_at(state, point) {
                    state.pressed_link = Some(link);
                    shell.capture_event();
                    return;
                }
                let offset = offset_at(&state.paragraph, point);
                let click = Click::new(point, mouse::Button::Left, state.last_click);
                state.last_click = Some(click);
                state.last_offset = Some(offset);
                let conversation = self.conversation.clone();
                shell.publish(match click.kind() {
                    click::Kind::Single => Message::SelectStart(conversation, mark(offset)),
                    click::Kind::Double => {
                        let (from, to) = word_at(&full_text(state.paragraph.buffer()), offset);
                        Message::SelectRange(conversation, mark(from), mark(to))
                    }
                    click::Kind::Triple => Message::SelectRange(
                        conversation,
                        mark(0),
                        mark(text_len(state.paragraph.buffer())),
                    ),
                });
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if let Some(link) = state.pressed_link.take()
                    && position.and_then(|point| link_at(state, point)) == Some(link)
                    && let Some(url) = self.spans.as_slice()[link].link.clone()
                {
                    shell.publish(Message::Link(url));
                }
            }
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        if tree.state.downcast_ref::<State>().hovered_link.is_some() {
            mouse::Interaction::Pointer
        } else if cursor.is_over(layout.bounds()) {
            mouse::Interaction::Text
        } else {
            mouse::Interaction::None
        }
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        _renderer: &cosmic::Renderer,
        operation: &mut dyn Operation,
    ) {
        let Some((from, to)) = self.selected else {
            return;
        };
        let text = full_text(tree.state.downcast_ref::<State>().paragraph.buffer());
        let (from, to) = (from.min(text.len()), to.min(text.len()));
        if from < to {
            operation.custom(
                None,
                layout.bounds(),
                &mut SelectedText {
                    text: text[from..to].to_string(),
                    joiner: self.joiner,
                },
            );
        }
    }
}

impl<'a> From<Selectable<'a>> for Element<'a, Message> {
    fn from(text: Selectable<'a>) -> Self {
        Element::new(text)
    }
}

/// Gathers the selected text of every block, in the order they are shown.
#[derive(Default)]
pub struct Collect {
    text: String,
}

impl Operation<String> for Collect {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<String>)) {
        operate(self);
    }

    fn custom(&mut self, _id: Option<&widget::Id>, _bounds: Rectangle, state: &mut dyn Any) {
        if let Some(part) = state.downcast_mut::<SelectedText>() {
            if !self.text.is_empty() {
                self.text.push_str(part.joiner);
            }
            self.text.push_str(&part.text);
        }
    }

    fn finish(&self) -> Outcome<String> {
        Outcome::Some(self.text.clone())
    }
}

/// Renders a reply's markdown with selectable blocks, numbered in the order shown.
pub struct Viewer<'a> {
    pub conversation: &'a ConversationId,
    pub entry: usize,
    pub selection: Option<&'a Selection>,
    pub next: Cell<usize>,
}

impl<'a> Viewer<'a> {
    fn block(&self, spans: impl Into<Spans<'a>>, size: Pixels) -> Selectable<'a> {
        let index = self.next.get();
        self.next.set(index + 1);
        Selectable::new(
            spans,
            size.0,
            self.conversation,
            (self.entry, index),
            self.selection,
        )
    }
}

impl<'a> markdown::Viewer<'a, Message, Theme, cosmic::Renderer> for Viewer<'a> {
    fn on_link_click(url: markdown::Uri) -> Message {
        Message::Link(url)
    }

    fn paragraph(
        &self,
        settings: markdown::Settings,
        text: &markdown::Text,
    ) -> Element<'a, Message> {
        self.block(text.spans(settings.style), settings.text_size)
            .into()
    }

    fn heading(
        &self,
        settings: markdown::Settings,
        level: &'a markdown::HeadingLevel,
        text: &'a markdown::Text,
        _index: usize,
    ) -> Element<'a, Message> {
        use markdown::HeadingLevel::*;
        let size = match level {
            H1 => settings.h1_size,
            H2 => settings.h2_size,
            H3 => settings.h3_size,
            H4 => settings.h4_size,
            H5 => settings.h5_size,
            H6 => settings.h6_size,
        };
        self.block(text.spans(settings.style), size).into()
    }

    fn code_block(
        &self,
        settings: markdown::Settings,
        _language: Option<&'a str>,
        code: &'a str,
        lines: &'a [markdown::Text],
    ) -> Element<'a, Message> {
        let lines: Vec<Element<'a, Message>> = lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let line = self
                    .block(line.spans(settings.style), settings.code_size)
                    .font(settings.style.code_block_font)
                    .line();
                // The block's first line starts a paragraph; the rest continue it.
                if i == 0 { line.joiner("\n\n") } else { line }.into()
            })
            .collect();
        // A line wider than the block scrolls, and a drag selection does not follow it.
        let copy = widget::tooltip(
            widget::button::icon(widget::icon::from_name("edit-copy-symbolic")).on_press(
                Message::Copied {
                    text: code.trim_end_matches('\n').to_string(),
                    primary: false,
                },
            ),
            widget::text("Copy"),
            widget::tooltip::Position::Top,
        );
        let scrolled = widget::scrollable::horizontal(
            widget::container(widget::Column::with_children(lines)).padding(settings.code_size.0),
        )
        .width(Length::Fill);
        widget::container(widget::Row::with_children(vec![
            scrolled.into(),
            copy.into(),
        ]))
        .width(Length::Fill)
        .padding(settings.code_size.0 / 4.0)
        .class(<Theme as markdown::Catalog>::code_block())
        .into()
    }

    /// Fits the reply's width, its cells wrapping their text: blocks fill the width they are
    /// given, which iced's table, sized by its cells, leaves at nothing.
    fn table(
        &self,
        settings: markdown::Settings,
        columns: &'a [markdown::Column],
        rows: &'a [markdown::Row],
    ) -> Element<'a, Message> {
        use cosmic::iced::widget::table;
        let chars = |cell: &[markdown::Item]| -> usize {
            cell.iter()
                .map(|item| match item {
                    markdown::Item::Paragraph(text) | markdown::Item::Heading(_, text) => text
                        .spans(settings.style)
                        .iter()
                        .map(|span| span.text.chars().count())
                        .sum(),
                    _ => 0,
                })
                .sum()
        };
        let columns = columns.iter().enumerate().map(|(i, column)| {
            let longest = rows
                .iter()
                .filter_map(|row| row.cells.get(i))
                .chain([&column.header])
                .map(|cell| chars(cell))
                .max()
                .unwrap_or(0);
            table::column(
                markdown::items(self, settings, &column.header),
                move |row: &'a markdown::Row| match row.cells.get(i) {
                    Some(cell) => markdown::items(self, settings, cell),
                    None => widget::text("").into(),
                },
            )
            // Capped, so a column of prose leaves a column of labels room for its words.
            .width(Length::FillPortion(longest.clamp(4, 40) as u16))
        });
        table(columns, rows)
            .padding_x(settings.spacing.0)
            .padding_y(settings.spacing.0 / 2.0)
            .separator_x(0)
            .into()
    }
}

/// A reading's size, relative to its words'.
const READING: f32 = 0.6;

/// Lines tall enough for readings over them, in a block that has any.
fn line_height(rubies: &[Ruby]) -> LineHeight {
    match rubies.is_empty() {
        true => LineHeight::default(),
        false => LineHeight::Relative(2.3),
    }
}

/// How much taller lines with readings make the paragraph at each end: its text is drawn
/// that much higher, so a first line with readings sits where one without would, level with
/// a list item's number.
fn lift(rubies: &[Ruby], size: f32) -> f32 {
    let size = Pixels(size);
    let line = |height: LineHeight| height.to_absolute(size).0;
    (line(line_height(rubies)) - line(LineHeight::default())) / 2.0
}

fn text<C>(
    content: C,
    bounds: Size,
    size: Pixels,
    line_height: LineHeight,
    font: Font,
    wrapping: Wrapping,
) -> Text<C, Font> {
    Text {
        content,
        bounds,
        size,
        line_height,
        font,
        align_x: Alignment::Default,
        align_y: alignment::Vertical::Top,
        shaping: Shaping::Advanced,
        wrapping,
        ellipsize: Ellipsize::None,
    }
}

fn text_len(buffer: &cosmic_text::Buffer) -> usize {
    buffer
        .lines
        .iter()
        .map(|line| line.text().len() + 1)
        .sum::<usize>()
        .saturating_sub(1)
}

fn full_text(buffer: &cosmic_text::Buffer) -> String {
    buffer
        .lines
        .iter()
        .map(|line| line.text())
        .collect::<Vec<_>>()
        .join("\n")
}

fn cursor_at(buffer: &cosmic_text::Buffer, mut offset: usize) -> cosmic_text::Cursor {
    for (i, line) in buffer.lines.iter().enumerate() {
        let len = line.text().len();
        if offset <= len {
            return cosmic_text::Cursor::new(i, offset);
        }
        offset -= len + 1;
    }
    let last = buffer.lines.len().saturating_sub(1);
    let len = buffer.lines.last().map_or(0, |line| line.text().len());
    cosmic_text::Cursor::new(last, len)
}

/// The byte offset under `point`; past the last line it is the end of the text.
fn offset_at(paragraph: &Paragraph, point: Point) -> usize {
    let buffer = paragraph.buffer();
    match buffer.hit(point.x, point.y) {
        Some(cursor) => {
            buffer
                .lines
                .iter()
                .take(cursor.line)
                .map(|line| line.text().len() + 1)
                .sum::<usize>()
                + cursor.index
        }
        None if point.y > paragraph.min_height() / 2.0 => text_len(buffer),
        None => 0,
    }
}

fn word_at(text: &str, offset: usize) -> (usize, usize) {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let offset = offset.min(text.len());
    let start = text[..offset]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(offset, |(i, _)| i);
    let end = text[offset..]
        .char_indices()
        .find(|(_, c)| !is_word(*c))
        .map_or(text.len(), |(i, _)| offset + i);
    (start, end)
}
