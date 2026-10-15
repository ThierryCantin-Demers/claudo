//! The `/` menu of the message box: the CLI's commands as one is typed, and the few claudo
//! runs itself because they change what it keeps for a conversation.

use super::*;
use claudo_protocol::CliCommand;

/// Commands for Claude Code's terminal interface, which do nothing useful here.
const HIDDEN: [&str; 8] = [
    "fast",
    "focus",
    "color",
    "heapdump",
    "agents",
    "extra-usage",
    "__remote-workflow",
    "workflow-launch-exec",
];

/// How many commands the menu lists at once.
const SHOWN: usize = 8;

/// A command that cannot run without what follows its name; `<optional …>` and `[…]` can.
fn needs_arguments(command: &CliCommand) -> bool {
    let hint = &command.argument_hint;
    hint.starts_with('<') && !hint.starts_with("<optional")
}

impl ConversationView {
    /// What the draft has typed of a command's name, while it is only that.
    fn command_query(&self) -> Option<String> {
        if self.menu_closed {
            return None;
        }
        let text = self.draft.text();
        let query = text.strip_prefix('/')?;
        (!query.contains(char::is_whitespace)).then(|| query.to_lowercase())
    }
}

impl ClaudoApp {
    /// The commands whose name has what the draft typed, those starting with it first.
    pub(super) fn command_matches(&self, view: &ConversationView) -> Vec<&CliCommand> {
        let Some(query) = view.command_query() else {
            return Vec::new();
        };
        let (mut first, mut then) = (Vec::new(), Vec::new());
        for command in &self.cli_commands {
            let name = command.name.to_lowercase();
            if HIDDEN.contains(&command.name.as_str()) {
                continue;
            }
            if name.starts_with(&query) {
                first.push(command);
            } else if name.contains(&query) {
                then.push(command);
            }
        }
        first.extend(then);
        first.truncate(SHOWN);
        first
    }

    /// Above the message box while a command's name is being typed.
    pub(super) fn command_menu<'a>(
        &'a self,
        id: &'a ConversationId,
        view: &'a ConversationView,
    ) -> Option<Element<'a, Message>> {
        let matches = self.command_matches(view);
        if matches.is_empty() {
            return None;
        }
        let size = 13.0 * self.zoom;
        let radius = cosmic::theme::active().cosmic().corner_radii.radius_s;
        let highlighted = view.menu.min(matches.len() - 1);
        let rows: Vec<Element<Message>> = matches
            .iter()
            .enumerate()
            .map(|(i, command)| {
                let mut line: Vec<Element<Message>> = vec![
                    widget::text(format!("/{}", command.name))
                        .size(size)
                        .font(cosmic::font::semibold())
                        .wrapping(text::Wrapping::None)
                        .into(),
                ];
                if !command.argument_hint.is_empty() {
                    line.push(
                        widget::text(truncate(&command.argument_hint, 40))
                            .size(size * 0.9)
                            .class(dim())
                            .wrapping(text::Wrapping::None)
                            .into(),
                    );
                }
                line.push(
                    widget::text(command.description.as_str())
                        .size(size * 0.9)
                        .class(dim())
                        .wrapping(text::Wrapping::None)
                        .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
                        .width(Length::Fill)
                        .into(),
                );
                widget::button::custom(
                    widget::Row::with_children(line)
                        .spacing(10)
                        .align_y(Alignment::Center),
                )
                .class(conversation_style(radius, i == highlighted))
                .selected(i == highlighted)
                .on_press(Message::MenuPick {
                    conversation: id.clone(),
                    at: Some(i),
                    run: false,
                })
                .padding([4, 10])
                .width(Length::Fill)
                .into()
            })
            .collect();
        let card = widget::container(widget::Column::with_children(rows).spacing(2))
            .padding(6)
            .width(Length::Fill)
            .class(cosmic::theme::Container::Card);
        Some(
            widget::container(widget::container(card).max_width(COLUMN_WIDTH * self.zoom))
                .center_x(Length::Fill)
                .padding([0, 16])
                .into(),
        )
    }

    /// Put a listed command in the draft. With `run`, one that needs nothing more is sent.
    pub(super) fn pick_command(&mut self, id: &ConversationId, at: Option<usize>, run: bool) {
        let Some(view) = self.views.get(id) else {
            return;
        };
        let matches = self.command_matches(view);
        let Some(command) =
            matches.get(at.unwrap_or(view.menu).min(matches.len().saturating_sub(1)))
        else {
            return;
        };
        let (name, needs) = (command.name.clone(), needs_arguments(command));
        let Some(view) = self.views.get_mut(id) else {
            return;
        };
        view.draft = text_editor::Content::with_text(&format!("/{name} "));
        view.draft
            .perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
        view.menu = 0;
        if run && !needs {
            self.send(id.clone());
        }
        self.focus_composer = Some(id.clone());
    }

    /// Run a command that changes what claudo keeps for the conversation through claudo,
    /// rather than the CLI, which would change it behind claudo's back. False for any other.
    pub(super) fn local_command(&mut self, id: &ConversationId, text: &str) -> bool {
        let Some(rest) = text.strip_prefix('/') else {
            return false;
        };
        let (name, args) = rest
            .split_once(char::is_whitespace)
            .map_or((rest, ""), |(name, args)| (name, args.trim()));
        match name {
            // A new session with an empty context: a new chat in its place.
            "clear" => {
                self.focus(id);
                self.new_conversation(Placement::Focused);
            }
            "rename" if !args.is_empty() => self.rename(id.clone(), args.to_string()),
            "rename" => {
                let task = self.update(Message::StartRename(id.clone(), TitleAt::Header));
                self.tasks.push(task);
            }
            "model" if !args.is_empty() => {
                let model = self
                    .models
                    .iter()
                    .find(|m| {
                        m.value.eq_ignore_ascii_case(args) || m.label.eq_ignore_ascii_case(args)
                    })
                    .map_or_else(|| args.to_string(), |m| m.value.clone());
                self.set_override(id.clone(), |o| o.model = Some(model));
            }
            "effort" if !args.is_empty() => {
                let effort = Effort::ALL.iter().copied().find(|e| {
                    e.label().eq_ignore_ascii_case(args)
                        || e.cli().is_some_and(|cli| cli.eq_ignore_ascii_case(args))
                        || (*e == Effort::Default && matches!(args, "auto" | "default"))
                });
                match effort {
                    Some(effort) => self.set_override(id.clone(), |o| o.effort = Some(effort)),
                    None => {
                        if let Some(view) = self.views.get_mut(id) {
                            view.error = Some(format!(
                                "No thinking level is called \u{201c}{args}\u{201d}."
                            ));
                        }
                    }
                }
            }
            _ => return false,
        }
        true
    }
}
