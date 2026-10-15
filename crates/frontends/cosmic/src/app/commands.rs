//! The Commands panel: the shell commands a conversation's agent ran, running ones first.

use super::*;
use claudo_protocol::TaskStatus;

/// Selection blocks of a panel row, apart from the same call's row in the transcript.
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
    /// Its width and the handle's, when it shows.
    pub(super) fn commands_room(&self) -> f32 {
        if self.layout.commands_open && self.page == Page::Chat {
            self.layout.commands_width + HANDLE_WIDTH
        } else {
            0.0
        }
    }

    /// The header bar's button for the panel, with how many commands are running in all.
    pub(super) fn commands_button(&self) -> Element<'_, Message> {
        let open = self.layout.commands_open;
        let running: usize = self
            .views
            .values()
            .map(ConversationView::running_commands)
            .sum();
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

    /// The conversations the panel lists: the tiles left to right, then any other still
    /// running a command, since an agent keeps working after its tile closes.
    pub(super) fn command_groups(&self) -> Vec<(&ConversationId, &ConversationView)> {
        let mut ids: Vec<&ConversationId> =
            self.layout.tiles.iter().map(|t| &t.conversation).collect();
        let mut others: Vec<&ConversationId> = self
            .views
            .iter()
            .filter(|(id, view)| !ids.contains(id) && view.running_commands() > 0)
            .map(|(id, _)| id)
            .collect();
        others.sort();
        ids.extend(others);
        ids.into_iter()
            .filter_map(|id| Some((id, self.views.get(id)?)))
            .collect()
    }

    pub(super) fn commands_panel(&self) -> Option<Element<'_, Message>> {
        if self.commands_room() == 0.0 {
            return None;
        }
        let size = 13.0 * self.zoom;
        let groups = self.command_groups();
        let running: usize = groups.iter().map(|(_, v)| v.running_commands()).sum();
        let mut rows: Vec<Element<Message>> = Vec::new();
        for (id, view) in groups {
            rows.push(self.command_group(id, view, size));
        }
        let status = match running {
            0 => "Nothing running".to_string(),
            1 => "1 running".to_string(),
            n => format!("{n} running"),
        };
        let header = widget::Row::with_children(vec![
            widget::Column::with_children(vec![
                widget::text::heading("Commands").into(),
                widget::text(status).size(size * 0.9).class(dim()).into(),
            ])
            .width(Length::Fill)
            .into(),
            widget::button::icon(widget::icon::from_name("window-close-symbolic"))
                .on_press(Message::ShowCommands(false))
                .into(),
        ])
        .align_y(Alignment::Center);
        let list = widget::scrollable(
            widget::container(widget::Column::with_children(rows).spacing(16))
                .padding([0, 12, 12, 0]),
        )
        .height(Length::Fill);
        Some(
            widget::container(
                widget::Column::with_children(vec![header.into(), list.into()]).spacing(12),
            )
            .padding([8, 4, 0, 12])
            .width(Length::Fixed(self.layout.commands_width))
            .height(Length::Fill)
            .into(),
        )
    }

    /// One conversation's commands under its title, running ones first; the title folds them.
    fn command_group<'a>(
        &'a self,
        id: &'a ConversationId,
        view: &'a ConversationView,
        size: f32,
    ) -> Element<'a, Message> {
        let running_count = view.running_commands();
        let chevron = if view.commands_collapsed {
            "go-next-symbolic"
        } else {
            "go-down-symbolic"
        };
        let title = self
            .info(id)
            .map_or("New conversation", |c| c.title.as_str());
        let mut header = vec![
            widget::icon::from_name(chevron).size(16).into(),
            widget::text(title)
                .size(size)
                .font(cosmic::font::semibold())
                .wrapping(text::Wrapping::None)
                .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
                .width(Length::Fill)
                .into(),
        ];
        if running_count > 0 {
            header.push(
                widget::text(format!("{running_count} running"))
                    .size(size * 0.9)
                    .class(dim())
                    .into(),
            );
        }
        let header = widget::button::custom(
            widget::Row::with_children(header)
                .spacing(6)
                .align_y(Alignment::Center),
        )
        .on_press(Message::ToggleCommandGroup(id.clone()))
        .class(cosmic::theme::Button::Text)
        .width(Length::Fill);
        let mut rows: Vec<Element<Message>> = vec![header.into()];
        if view.commands_collapsed {
            return widget::Column::with_children(rows).into();
        }

        let texts = Texts {
            conversation: id,
            selection: self.selection.as_ref(),
        };
        let (running, done): (Vec<_>, Vec<_>) = view
            .commands()
            .partition(|(_, call)| running(call, view.streaming));
        if running.is_empty() && done.is_empty() {
            rows.push(
                widget::container(
                    widget::text("No commands yet")
                        .size(size * 0.9)
                        .class(dim()),
                )
                .padding([0, 0, 0, 26])
                .into(),
            );
        }
        rows.extend(
            running
                .into_iter()
                .chain(done)
                .map(|(idx, call)| self.command_row(id, view, idx, call, texts, size)),
        );
        widget::Column::with_children(rows).spacing(4).into()
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
