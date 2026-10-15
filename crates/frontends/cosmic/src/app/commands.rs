//! The Commands drawer: the shell commands a conversation's agent ran, running ones first.

use super::*;
use claudo_protocol::TaskStatus;
use cosmic::app::context_drawer::{ContextDrawer, context_drawer};

/// Selection blocks of a drawer row, apart from the same call's row in the transcript.
const COMMAND_BLOCK: usize = 10;
const OUTPUT_BLOCK: usize = 11;

/// A call in the background runs until it reports its end; any other only while its turn does.
pub(super) fn running(call: &ToolCall, streaming: bool) -> bool {
    match &call.background {
        Some(background) => background.status == TaskStatus::Running,
        None => call.result.is_none() && streaming,
    }
}

/// How a call stands, for its row: a pulse while running, else a mark or a word.
pub(super) fn call_status<'a>(
    call: &ToolCall,
    streaming: bool,
    pulse: f32,
    size: f32,
) -> Element<'a, Message> {
    if running(call, streaming) {
        return pulse_dots(pulse, size);
    }
    let word = |label: &'a str| widget::text(label).size(size).class(dim()).into();
    match (&call.background, &call.result, call.is_error) {
        (Some(b), _, _) if b.status == TaskStatus::Failed => widget::text("failed")
            .size(size)
            .class(destructive())
            .into(),
        (Some(b), _, _) if b.status == TaskStatus::Stopped => word("stopped"),
        (Some(b), _, _) if b.status == TaskStatus::Unknown => word("ended"),
        (None, None, _) => word("stopped"),
        (_, Some(_), true) => widget::text("error").size(size).class(destructive()).into(),
        _ => widget::icon::from_name("object-select-symbolic")
            .size(16)
            .into(),
    }
}

impl ConversationView {
    /// Shell commands, newest first, with their item's index.
    pub(super) fn commands(&self) -> impl Iterator<Item = (usize, &ToolCall)> {
        self.items
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(idx, entry)| match entry {
                Entry::Tool { call, .. } if call.command.is_some() => Some((idx, call)),
                _ => None,
            })
    }

    pub(super) fn running_commands(&self) -> usize {
        self.commands()
            .filter(|(_, call)| running(call, self.streaming))
            .count()
    }
}

impl ClaudoApp {
    /// The header bar's button for the drawer, with how many commands are running.
    pub(super) fn commands_button(&self) -> Element<'_, Message> {
        let open = self.core.window.show_context;
        let running = self
            .focused()
            .and_then(|id| self.views.get(id))
            .map_or(0, ConversationView::running_commands);
        let mut content = vec![
            widget::icon::from_name("utilities-terminal-symbolic")
                .size(16)
                .into(),
        ];
        if running > 0 {
            content.push(widget::text(running.to_string()).size(12).into());
        }
        let button = widget::button::custom(
            widget::Row::with_children(content)
                .spacing(4)
                .align_y(Alignment::Center),
        )
        .padding([6, 8])
        .class(if open {
            cosmic::theme::Button::Suggested
        } else {
            cosmic::theme::Button::Text
        })
        .on_press(Message::ShowCommands(!open));
        widget::tooltip(
            button,
            widget::text("Commands"),
            widget::tooltip::Position::Bottom,
        )
        .into()
    }

    pub(super) fn commands_drawer(&self) -> Option<ContextDrawer<'_, Message>> {
        let id = self.focused()?;
        let view = self.views.get(id)?;
        let size = 13.0 * self.zoom;
        let texts = Texts {
            conversation: id,
            selection: self.selection.as_ref(),
        };
        let (running, done): (Vec<_>, Vec<_>) = view
            .commands()
            .partition(|(_, call)| running(call, view.streaming));
        let mut rows: Vec<Element<Message>> = Vec::new();
        if running.is_empty() && done.is_empty() {
            rows.push(
                widget::text("The commands Claude runs in this conversation show here.")
                    .size(size)
                    .class(dim())
                    .into(),
            );
        }
        for (heading, list) in [("Running", running), ("History", done)] {
            if list.is_empty() {
                continue;
            }
            rows.push(
                widget::text(heading)
                    .size(size * 0.9)
                    .font(cosmic::font::semibold())
                    .class(dim())
                    .into(),
            );
            rows.extend(
                list.into_iter()
                    .map(|(idx, call)| self.command_row(id, view, idx, call, texts, size)),
            );
        }
        let title = self.info(id).map_or("Commands".to_string(), |c| {
            format!("Commands · {}", c.title)
        });
        Some(
            context_drawer(
                widget::Column::with_children(rows).spacing(6),
                Message::ShowCommands(false),
            )
            .title(title),
        )
    }

    fn command_row<'a>(
        &'a self,
        id: &'a ConversationId,
        view: &'a ConversationView,
        idx: usize,
        call: &'a ToolCall,
        texts: Texts<'a>,
        size: f32,
    ) -> Element<'a, Message> {
        let small = size * 0.9;
        let open = view.commands_open.contains(&idx);
        let command = call.command.as_deref().unwrap_or_default();
        let mut lines = vec![
            widget::text(truncate(&call.summary, PREVIEW_CHARS))
                .size(size)
                .font(cosmic::font::mono())
                .wrapping(text::Wrapping::None)
                .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
                .width(Length::Fill)
                .into(),
        ];
        let mut about = call.description.clone();
        if call.background.is_some() {
            about = if about.is_empty() {
                "in the background".to_string()
            } else {
                format!("{about} · in the background")
            };
        }
        if !about.is_empty() {
            lines.push(
                widget::text(about)
                    .size(small)
                    .class(dim())
                    .wrapping(text::Wrapping::None)
                    .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
                    .into(),
            );
        }
        let header = widget::Row::with_children(vec![
            widget::Column::with_children(lines)
                .spacing(2)
                .width(Length::Fill)
                .into(),
            call_status(call, view.streaming, self.pulse, small),
        ])
        .spacing(8)
        .align_y(Alignment::Center);
        let header = widget::button::custom(header)
            .on_press(Message::ToggleCommand(id.clone(), idx))
            .class(cosmic::theme::Button::Text)
            .width(Length::Fill);
        if !open {
            return header.into();
        }

        let output = match &call.background {
            Some(background) => background.output.as_deref(),
            None => call.result.as_deref(),
        };
        let mut details = vec![tool_text(
            texts,
            (idx, COMMAND_BLOCK),
            "Command",
            command,
            small,
        )];
        if let Some(output) = output.filter(|o| !o.trim().is_empty()) {
            details.push(tool_text(
                texts,
                (idx, OUTPUT_BLOCK),
                "Output",
                output,
                small,
            ));
        }
        let details = widget::container(widget::Column::with_children(details).spacing(10))
            .padding(10)
            .width(Length::Fill)
            .class(cosmic::theme::Container::Card);
        widget::Column::with_children(vec![header.into(), details.into()])
            .spacing(4)
            .into()
    }
}
