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
    running: bool,
    pulse: f32,
    size: f32,
) -> Element<'a, Message> {
    if running {
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
    /// What the Commands panel lists, newest first, with their item's index: the agent's
    /// shell commands, and its subagents, whose own calls show under them.
    pub(super) fn commands(&self) -> impl Iterator<Item = (usize, &ToolCall)> {
        self.items
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(idx, entry)| match entry {
                Entry::Tool { call, .. }
                    if call.parent.is_none() && (call.command.is_some() || call.agent) =>
                {
                    Some((idx, call))
                }
                _ => None,
            })
    }

    /// The calls a subagent made, in order, with their item's index.
    pub(super) fn made_by<'a>(
        &'a self,
        agent: &'a str,
    ) -> impl Iterator<Item = (usize, &'a ToolCall)> {
        self.items
            .iter()
            .enumerate()
            .filter_map(move |(idx, entry)| match entry {
                Entry::Tool { call, .. } if call.parent.as_deref() == Some(agent) => {
                    Some((idx, call))
                }
                _ => None,
            })
    }

    /// What still works in the background, which nothing else shows once its turn ended.
    pub(super) fn in_background(&self) -> Option<String> {
        let (mut commands, mut subagents) = (0, 0);
        for (_, call) in self.commands() {
            if call.background.is_some() && self.running(call) {
                match call.agent {
                    true => subagents += 1,
                    false => commands += 1,
                }
            }
        }
        let count = |n: usize, what: &str| match n {
            0 => None,
            1 => Some(format!("1 {what}")),
            n => Some(format!("{n} {what}s")),
        };
        let running: Vec<String> = [count(commands, "command"), count(subagents, "subagent")]
            .into_iter()
            .flatten()
            .collect();
        (!running.is_empty())
            .then(|| format!("Running {} in the background", running.join(" and ")))
    }

    pub(super) fn running_commands(&self) -> usize {
        self.commands()
            .filter(|(_, call)| self.running(call))
            .count()
    }

    /// Whether a call is still going. A subagent's call goes on while its subagent does,
    /// which outlives the turn when it runs in the background.
    pub(super) fn running(&self, call: &ToolCall) -> bool {
        let started_by = call
            .parent
            .as_deref()
            .filter(|parent| *parent != call.call_id)
            .and_then(|parent| self.call(parent));
        match (&call.background, started_by) {
            (None, Some(agent)) if call.result.is_none() => self.running(agent),
            _ => running(call, self.streaming),
        }
    }

    fn call(&self, call_id: &str) -> Option<&ToolCall> {
        self.items.iter().rev().find_map(|entry| match entry {
            Entry::Tool { call, .. } if call.call_id == call_id => Some(call),
            _ => None,
        })
    }
}

impl ClaudoApp {
    /// Its width and the handle's, when it shows.
    pub(super) fn commands_room(&self, w: window::Id) -> f32 {
        let layout = self.window(w);
        if layout.commands_open && layout.page == Page::Chat {
            layout.commands_width + HANDLE_WIDTH
        } else {
            0.0
        }
    }

    /// The header bar's button for the panel, with how many commands are running in all.
    pub(super) fn commands_button(&self, w: window::Id) -> Element<'_, Message> {
        let open = self.window(w).commands_open;
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

    /// The conversations a window's panel lists: its tiles left to right, then any other
    /// still running a command, in another window or after its tile closed.
    pub(super) fn command_groups(
        &self,
        w: window::Id,
    ) -> Vec<(&ConversationId, &ConversationView)> {
        let mut ids: Vec<&ConversationId> = self
            .window(w)
            .tiles
            .iter()
            .map(|t| &t.conversation)
            .collect();
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

    pub(super) fn commands_panel(&self, w: window::Id) -> Option<Element<'_, Message>> {
        if self.commands_room(w) == 0.0 {
            return None;
        }
        let size = 13.0 * self.zoom;
        let groups = self.command_groups(w);
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
            .width(Length::Fixed(self.window(w).commands_width))
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
        let (running, done): (Vec<_>, Vec<_>) =
            view.commands().partition(|(_, call)| view.running(call));
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
        for (label, calls) in [("Running", running), ("Finished", done)] {
            if calls.is_empty() {
                continue;
            }
            rows.push(
                widget::container(widget::text(label).size(size * 0.85).class(dim()))
                    .padding([6, 0, 0, 26])
                    .into(),
            );
            rows.extend(
                calls
                    .into_iter()
                    .map(|(idx, call)| self.panel_row(id, view, idx, call, texts, size)),
            );
        }
        widget::Column::with_children(rows).spacing(4).into()
    }

    fn panel_row<'a>(
        &'a self,
        id: &'a ConversationId,
        view: &'a ConversationView,
        idx: usize,
        call: &'a ToolCall,
        texts: Texts<'a>,
        size: f32,
    ) -> Element<'a, Message> {
        if call.agent {
            self.agent_row(id, view, idx, call, texts, size)
        } else {
            self.command_row(id, view, idx, call, texts, size)
        }
    }

    /// A subagent, for as long as it works: what it does and how far it got; opened, its
    /// calls newest first and what it reported.
    fn agent_row<'a>(
        &'a self,
        id: &'a ConversationId,
        view: &'a ConversationView,
        idx: usize,
        agent: &'a ToolCall,
        texts: Texts<'a>,
        size: f32,
    ) -> Element<'a, Message> {
        let small = size * 0.9;
        let running = view.running(agent);
        let made: Vec<(usize, &ToolCall)> = view.made_by(&agent.call_id).collect();
        let mut about = vec!["Subagent".to_string()];
        if agent.background.is_some() {
            about.push("in the background".to_string());
        }
        about.push(match made.len() {
            1 => "1 call".to_string(),
            n => format!("{n} calls"),
        });
        if running && let Some((_, latest)) = made.last() {
            about.push(format!("{} {}", latest.name, latest.summary));
        }
        let lines = vec![
            widget::text(truncate(&agent.summary, PREVIEW_CHARS))
                .size(size)
                .wrapping(text::Wrapping::None)
                .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
                .width(Length::Fill)
                .into(),
            widget::text(about.join(" · "))
                .size(small)
                .class(dim())
                .wrapping(text::Wrapping::None)
                .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
                .into(),
        ];
        let header = widget::button::custom(
            widget::Row::with_children(vec![
                widget::Column::with_children(lines)
                    .spacing(2)
                    .width(Length::Fill)
                    .into(),
                call_status(agent, running, self.pulse, small),
            ])
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .on_press(Message::ToggleCommand(id.clone(), idx))
        .class(cosmic::theme::Button::Text)
        .width(Length::Fill);
        if !view.commands_open.contains(&idx) {
            return header.into();
        }

        let mut rows: Vec<Element<Message>> = made
            .iter()
            .rev()
            .map(|&(idx, call)| self.panel_row(id, view, idx, call, texts, size))
            .collect();
        let report = match &agent.background {
            Some(background) => background.output.as_deref(),
            None => agent.result.as_deref(),
        };
        if let Some(report) = report.filter(|r| !r.trim().is_empty()) {
            rows.push(
                widget::container(tool_text(
                    texts,
                    (idx, OUTPUT_BLOCK),
                    "Report",
                    report,
                    small,
                ))
                .padding(10)
                .width(Length::Fill)
                .class(cosmic::theme::Container::Card)
                .into(),
            );
        }
        widget::Column::with_children(vec![
            header.into(),
            widget::container(widget::Column::with_children(rows).spacing(4))
                .padding([0, 0, 0, 16])
                .into(),
        ])
        .spacing(4)
        .into()
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
        let title = match call.command {
            Some(_) => call.summary.clone(),
            None => format!("{} {}", call.name, call.summary),
        };
        let mut lines = vec![
            widget::text(truncate(&title, PREVIEW_CHARS))
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
            call_status(call, view.running(call), self.pulse, small),
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
        // A subagent's row lists its other calls too, which run no command.
        let (label, input) = match &call.command {
            Some(command) => ("Command", command),
            None => ("Input", &call.input),
        };
        let mut details = vec![tool_text(texts, (idx, COMMAND_BLOCK), label, input, small)];
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
