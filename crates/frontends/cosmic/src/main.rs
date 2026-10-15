//! Native COSMIC frontend (libcosmic / iced). Opt-in behind the `cosmic`
//! feature so a plain workspace build never pulls libcosmic. Speaks the same
//! `transport` + `protocol` as the egui and web frontends.
//!
//! Streaming is event-driven: a worker subscription owns the backend and pushes
//! each `ServerEvent` as a message, so updates render as they arrive (no polling).

#[cfg(not(feature = "cosmic"))]
fn main() {
    eprintln!("claudo-cosmic was built without the `cosmic` feature.");
    eprintln!("Build/run it with:  cargo run -p claudo-cosmic --features cosmic");
}

#[cfg(feature = "cosmic")]
fn main() -> cosmic::iced::Result {
    cosmic::app::run::<app::ClaudoApp>(cosmic::app::Settings::default(), ())
}

#[cfg(feature = "cosmic")]
mod app {
    use claudo_protocol::{
        Command, ConversationInfo, Effort, Item, MODEL_PRESETS, Overrides, Profile, ServerEvent,
        Settings, SystemStatus, ToolCall,
    };
    use claudo_transport::{Commands, spawn_backend};

    use cosmic::app::{Core, Task};
    use std::hash::Hash;

    use cosmic::iced::advanced::subscription::{EventStream, Hasher, Recipe, from_recipe};
    use cosmic::iced::futures::stream::BoxStream;
    use cosmic::iced::futures::{SinkExt, Stream, StreamExt};
    use cosmic::iced::widget::{container, text};
    use cosmic::iced::{Alignment, Border, Color, Length, Padding, Subscription, keyboard, stream};
    use cosmic::widget::{self, markdown, text_editor};
    use cosmic::{Application, Element};

    const PROFILES: [Profile; 3] = [Profile::Safe, Profile::AcceptEdits, Profile::FullAuto];
    const COLUMN_WIDTH: f32 = 760.0;
    const BUBBLE_WIDTH: f32 = 560.0;
    const TOOL_OUTPUT_LINES: usize = 40;
    const PREVIEW_CHARS: usize = 100;
    const PULSE_TICK: std::time::Duration = std::time::Duration::from_millis(50);

    /// A transcript row. Replies keep their parsed markdown so a frame only
    /// parses the text revealed since the last one.
    enum Entry {
        User(String),
        Thinking { text: String, open: bool },
        Reply(Box<Reply>),
        Tool { call: ToolCall, open: bool },
    }

    struct Reply {
        text: String,
        markdown: markdown::Content,
        /// Bytes of `text` already fed to `markdown`.
        parsed: usize,
    }

    impl Reply {
        fn streaming(text: &str) -> Self {
            Self {
                text: text.to_string(),
                markdown: markdown::Content::new(),
                parsed: 0,
            }
        }

        fn complete(text: String) -> Self {
            Self {
                markdown: markdown::Content::parse(&text),
                parsed: text.len(),
                text,
            }
        }

        fn reveal_to(&mut self, end: usize) {
            if end > self.parsed {
                self.markdown.push_str(&self.text[self.parsed..end]);
                self.parsed = end;
            }
        }

        fn finish(&mut self) {
            self.reveal_to(self.text.len());
        }
    }

    impl From<Item> for Entry {
        fn from(item: Item) -> Self {
            match item {
                Item::User(text) => Entry::User(text),
                Item::Assistant(text) => Entry::Reply(Box::new(Reply::complete(text))),
                Item::Thinking(text) => Entry::Thinking { text, open: false },
                Item::Tool(call) => Entry::Tool { call, open: false },
            }
        }
    }

    pub struct ClaudoApp {
        core: Core,
        commands: Option<Commands>,
        status: Option<SystemStatus>,
        settings: Settings,
        overrides: Overrides,
        conversations: Vec<ConversationInfo>,
        current: Option<String>,
        items: Vec<Entry>,
        draft: text_editor::Content,
        /// Set when the composer should take focus after this update.
        focus_composer: bool,
        /// Calls waiting for the user's answer, with their request ids.
        approvals: Vec<(String, ToolCall)>,
        streaming: bool,
        assistant_idx: Option<usize>,
        /// Chars of the streaming message revealed so far. The CLI delivers text
        /// in large chunks; revealing gradually reads as smooth typing.
        reveal: f32,
        /// Seconds of the activity indicator's animation.
        pulse: f32,
        /// Text scale. libcosmic exposes no runtime window scale factor, so zoom
        /// is applied to the conversation text rather than the whole window.
        zoom: f32,
        error: Option<String>,
        page: Page,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Page {
        Chat,
        Settings,
    }

    #[derive(Debug, Clone)]
    pub enum Message {
        Connected(Commands),
        Events(Vec<ServerEvent>),
        /// Animation frame while text is being revealed.
        Frame,
        /// Animation frame for the activity indicator.
        Pulse,
        /// Multiply the text scale (Ctrl+scroll, Ctrl+`+`/`-`).
        Zoom(f32),
        /// Reset text scale (Ctrl+0).
        ZoomReset,
        Edit(text_editor::Action),
        Send,
        Stop,
        Link(markdown::Uri),
        /// Expand or collapse a thinking or tool row.
        Toggle(usize),
        /// A hot patch landed. Handling any message rebuilds and redraws the view,
        /// which iced's own reload leaves until the next input event.
        #[cfg(feature = "hot")]
        Hotpatched,
        Open(String),
        Answer {
            request_id: String,
            allow: bool,
        },
        SetPage(Page),
        /// The main window is closing.
        Quit,
        New,
        SetModel(usize),
        ModelText(String),
        SetEffort(usize),
        SetProfile(usize),
        OvModel(usize),
        OvEffort(usize),
        OvProfile(usize),
    }

    impl ClaudoApp {
        fn cmd(&self, command: Command) {
            if let Some(c) = &self.commands {
                c.send(command);
            }
        }

        fn is_current(&self, id: &str) -> bool {
            self.current.as_deref() == Some(id)
        }

        fn save_settings(&self) {
            self.cmd(Command::UpdateSettings {
                settings: self.settings.clone(),
            });
        }

        fn save_overrides(&self) {
            if let Some(id) = self.current.clone() {
                self.cmd(Command::SetOverrides {
                    id,
                    overrides: self.overrides.clone(),
                });
            }
        }

        fn sync_overrides(&mut self) {
            self.overrides = self
                .current
                .as_deref()
                .and_then(|id| self.conversations.iter().find(|c| c.id == id))
                .map(|c| c.overrides.clone())
                .unwrap_or_default();
        }

        fn effective(&self) -> (String, Effort, Profile) {
            (
                self.overrides
                    .model
                    .clone()
                    .unwrap_or_else(|| self.settings.model.clone()),
                self.overrides.effort.unwrap_or(self.settings.effort),
                self.overrides.profile.unwrap_or(self.settings.profile),
            )
        }

        fn open(&mut self, id: String) {
            if self.streaming {
                return;
            }
            self.current = Some(id.clone());
            self.sync_overrides();
            self.items.clear();
            self.approvals.clear();
            self.assistant_idx = None;
            self.error = None;
            self.focus_composer = true;
            self.cmd(Command::OpenConversation { id });
        }

        /// True while a turn runs and no text is appearing, so something must show it is alive.
        fn pulsing(&self) -> bool {
            self.streaming && !self.revealing()
        }

        /// What the running turn is doing, unless a thinking row is already showing it.
        fn activity(&self) -> Option<String> {
            if !self.pulsing() {
                return None;
            }
            if !self.approvals.is_empty() {
                return Some("Waiting for your approval".to_string());
            }
            match self.items.last() {
                Some(Entry::Thinking { .. }) => None,
                Some(Entry::Tool { call, .. }) if call.result.is_none() => {
                    Some(format!("Running {}", call.name))
                }
                _ => Some("Thinking".to_string()),
            }
        }

        fn can_send(&self) -> bool {
            !self.streaming && !self.draft.text().trim().is_empty()
        }

        fn send(&mut self) {
            let text = self.draft.text().trim().to_string();
            let Some(current) = self.current.clone() else {
                return;
            };
            if text.is_empty() || self.streaming {
                return;
            }
            self.error = None;
            self.finish_reply();
            self.items.push(Entry::User(text.clone()));
            self.streaming = true;
            let (model, effort, profile) = self.effective();
            self.cmd(Command::SendMessage {
                conversation_id: current,
                text,
                profile,
                model,
                effort,
            });
            self.draft = text_editor::Content::new();
            self.focus_composer = true;
        }

        fn push_assistant(&mut self, text: &str) {
            match self.assistant_idx {
                Some(i) => {
                    if let Entry::Reply(reply) = &mut self.items[i] {
                        reply.text.push_str(text);
                    }
                }
                None => {
                    self.items
                        .push(Entry::Reply(Box::new(Reply::streaming(text))));
                    self.assistant_idx = Some(self.items.len() - 1);
                    self.reveal = 0.0;
                }
            }
        }

        /// Stop revealing the streaming reply and show all of it.
        fn finish_reply(&mut self) {
            if let Some(i) = self.assistant_idx.take()
                && let Some(Entry::Reply(reply)) = self.items.get_mut(i)
            {
                reply.finish();
            }
        }

        fn streaming_reply(&mut self) -> Option<&mut Reply> {
            match self.items.get_mut(self.assistant_idx?) {
                Some(Entry::Reply(reply)) => Some(reply),
                _ => None,
            }
        }

        fn stream_target(&self) -> Option<usize> {
            match self.items.get(self.assistant_idx?) {
                Some(Entry::Reply(reply)) => Some(reply.text.chars().count()),
                _ => None,
            }
        }

        /// True while text is still being revealed (drives the animation tick).
        fn revealing(&self) -> bool {
            self.stream_target()
                .is_some_and(|t| (self.reveal as usize) < t)
        }

        fn advance_reveal(&mut self, dt: f32) {
            let Some(target) = self.stream_target() else {
                return;
            };
            if (self.reveal as usize) >= target {
                return;
            }
            let backlog = target as f32 - self.reveal;
            let per_sec = (backlog / 0.12).max(220.0);
            self.reveal = (self.reveal + per_sec * dt).min(target as f32);
            let shown = self.reveal as usize;
            if let Some(reply) = self.streaming_reply() {
                let end = char_offset(&reply.text, shown);
                reply.reveal_to(end);
            }
        }

        fn apply(&mut self, ev: ServerEvent) {
            match ev {
                ServerEvent::Status(s) => self.status = Some(s),
                ServerEvent::Settings(s) => self.settings = s,
                ServerEvent::Conversations { list, current } => {
                    self.conversations = list;
                    if let Some(id) = current {
                        self.current = Some(id);
                        self.sync_overrides();
                        self.items.clear();
                        self.assistant_idx = None;
                        self.error = None;
                        self.focus_composer = true;
                    } else if self.current.is_none() {
                        match self.conversations.first() {
                            Some(first) => {
                                let id = first.id.clone();
                                self.open(id);
                            }
                            None => self.cmd(Command::NewScratch),
                        }
                    }
                }
                ServerEvent::History {
                    conversation_id,
                    items,
                } => {
                    if self.is_current(&conversation_id) {
                        self.items = items.into_iter().map(Entry::from).collect();
                        self.assistant_idx = None;
                    }
                }
                ServerEvent::Started { .. } => {}
                ServerEvent::Delta {
                    conversation_id,
                    text,
                } => {
                    if self.is_current(&conversation_id) {
                        self.push_assistant(&text);
                    }
                }
                ServerEvent::ThinkingDelta {
                    conversation_id,
                    text,
                } => {
                    if self.is_current(&conversation_id) {
                        if let Some(Entry::Thinking { text: thought, .. }) = self.items.last_mut() {
                            thought.push_str(&text);
                        } else {
                            self.finish_reply();
                            self.items.push(Entry::Thinking { text, open: false });
                        }
                    }
                }
                ServerEvent::ToolStarted {
                    conversation_id,
                    call,
                } => {
                    if self.is_current(&conversation_id) {
                        self.finish_reply();
                        self.items.push(Entry::Tool { call, open: false });
                    }
                }
                ServerEvent::ToolResult {
                    conversation_id,
                    call_id,
                    content,
                    is_error,
                } => {
                    if self.is_current(&conversation_id)
                        && let Some(Entry::Tool { call, .. }) = self.items.iter_mut().rev().find(
                            |entry| matches!(entry, Entry::Tool { call, .. } if call.call_id == call_id),
                        )
                    {
                        call.result = Some(content);
                        call.is_error = is_error;
                    }
                }
                ServerEvent::Completed {
                    conversation_id,
                    is_error,
                    ..
                } => {
                    if self.is_current(&conversation_id) {
                        self.streaming = false;
                        self.approvals.clear();
                        self.finish_reply();
                        if is_error {
                            self.error = Some("Claude reported an error.".to_string());
                        }
                    }
                }
                ServerEvent::ApprovalRequested {
                    conversation_id,
                    request_id,
                    call,
                } => {
                    if self.is_current(&conversation_id) {
                        self.approvals.push((request_id, call));
                    }
                }
                ServerEvent::Quota(_) => {}
                ServerEvent::Error { detail } => {
                    self.error = Some(detail);
                    self.streaming = false;
                    self.approvals.clear();
                    self.finish_reply();
                }
            }
        }

        fn settings_page(&self) -> Element<'_, Message> {
            let models: Vec<&str> = MODEL_PRESETS.iter().map(|(label, _)| *label).collect();
            let efforts: Vec<&str> = Effort::ALL.iter().map(|e| e.label()).collect();
            let profiles: Vec<&str> = PROFILES.iter().map(|p| p.label()).collect();
            let defaults = widget::settings::section()
                .title("Defaults for new conversations")
                .add(widget::settings::item(
                    "Model",
                    widget::dropdown(
                        models,
                        MODEL_PRESETS
                            .iter()
                            .position(|(_, m)| *m == self.settings.model),
                        Message::SetModel,
                    ),
                ))
                .add(widget::settings::item(
                    "Model version",
                    widget::text_input("claude-sonnet-5-5", &self.settings.model)
                        .on_input(Message::ModelText)
                        .width(Length::Fixed(220.0)),
                ))
                .add(widget::settings::item(
                    "Thinking",
                    widget::dropdown(
                        efforts,
                        Effort::ALL.iter().position(|e| *e == self.settings.effort),
                        Message::SetEffort,
                    ),
                ))
                .add(widget::settings::item(
                    "Permissions",
                    widget::dropdown(
                        profiles,
                        PROFILES.iter().position(|p| *p == self.settings.profile),
                        Message::SetProfile,
                    ),
                ));

            let page = widget::Column::with_children(vec![
                widget::button::text("Back to chat")
                    .leading_icon(widget::icon::from_name("go-previous-symbolic"))
                    .on_press(Message::SetPage(Page::Chat))
                    .into(),
                widget::text::title3("Settings").into(),
                defaults.into(),
                widget::text(self.settings.profile.tooltip())
                    .class(dim())
                    .into(),
                widget::text("Each conversation can change these from its message box.")
                    .class(dim())
                    .into(),
            ])
            .spacing(16)
            .padding([24, 16])
            .max_width(COLUMN_WIDTH * self.zoom);
            widget::scrollable(widget::container(page).center_x(Length::Fill))
                .height(Length::Fill)
                .into()
        }

        fn sidebar(&self) -> Element<'_, Message> {
            let size = 14.0 * self.zoom;
            let radius = cosmic::theme::active().cosmic().corner_radii.radius_s;
            let recents: Vec<Element<Message>> = self
                .conversations
                .iter()
                .map(|c| {
                    let title = widget::text(truncate(&c.title, 30))
                        .size(size)
                        .wrapping(text::Wrapping::None)
                        .width(Length::Fill);
                    let selected = self.page == Page::Chat && self.is_current(&c.id);
                    widget::button::custom(title)
                        .class(conversation_style(radius, selected))
                        .selected(selected)
                        .on_press(Message::Open(c.id.clone()))
                        .padding([6, 10])
                        .width(Length::Fill)
                        .into()
                })
                .collect();

            widget::container(
                widget::Column::with_children(vec![
                    widget::button::text("New chat")
                        .leading_icon(widget::icon::from_name("list-add-symbolic"))
                        .on_press(Message::New)
                        .into(),
                    widget::text("Recents")
                        .size(12.0 * self.zoom)
                        .class(dim())
                        .into(),
                    widget::scrollable(widget::Column::with_children(recents).spacing(2))
                        .height(Length::Fill)
                        .into(),
                    widget::button::text("Settings")
                        .leading_icon(widget::icon::from_name("emblem-system-symbolic"))
                        .on_press(Message::SetPage(Page::Settings))
                        .into(),
                ])
                .spacing(8),
            )
            .width(Length::Fixed(260.0))
            .height(Length::Fill)
            .padding(8)
            .into()
        }

        fn current_info(&self) -> Option<&ConversationInfo> {
            let id = self.current.as_deref()?;
            self.conversations.iter().find(|c| c.id == id)
        }
    }

    impl ClaudoApp {
        /// A new conversation centers the composer under a greeting, as Claude Desktop does.
        fn conversation(&self) -> Element<'_, Message> {
            if self.items.is_empty() && !self.streaming {
                let greeting =
                    widget::text::title2("How can I help you today?").size(26.0 * self.zoom);
                return widget::Column::with_children(vec![
                    widget::space::vertical().into(),
                    widget::container(greeting).center_x(Length::Fill).into(),
                    self.composer(),
                    widget::space::vertical().into(),
                ])
                .spacing(24)
                .into();
            }
            let mut column = vec![self.transcript()];
            column.extend(
                self.approvals
                    .iter()
                    .map(|(id, call)| self.approval_card(id, call)),
            );
            column.push(self.composer());
            widget::Column::with_children(column).spacing(8).into()
        }

        /// What Claude wants to run, and the two answers. The turn waits on it.
        fn approval_card<'a>(&self, request_id: &str, call: &'a ToolCall) -> Element<'a, Message> {
            let size = 14.0 * self.zoom;
            let answer = |allow| Message::Answer {
                request_id: request_id.to_string(),
                allow,
            };
            let buttons = widget::Row::with_children(vec![
                widget::space::horizontal().into(),
                widget::button::destructive("Deny")
                    .on_press(answer(false))
                    .into(),
                widget::button::suggested("Allow")
                    .on_press(answer(true))
                    .into(),
            ])
            .spacing(8);
            let card = widget::container(
                widget::Column::with_children(vec![
                    widget::text(format!("Claude wants to run {}", call.name))
                        .size(size)
                        .font(cosmic::font::semibold())
                        .into(),
                    widget::text(call.summary.as_str())
                        .size(size * 0.9)
                        .font(cosmic::font::mono())
                        .into(),
                    buttons.into(),
                ])
                .spacing(10),
            )
            .padding([12, 14])
            .width(Length::Fill)
            .class(cosmic::theme::Container::custom(|theme| {
                let cosmic = theme.cosmic();
                container::Style {
                    background: Some(Color::from(cosmic.bg_component_color()).into()),
                    border: Border {
                        radius: cosmic.corner_radii.radius_l.into(),
                        width: 1.0,
                        color: cosmic.accent_color().into(),
                    },
                    ..container::Style::default()
                }
            }));
            widget::container(widget::container(card).max_width(COLUMN_WIDTH * self.zoom))
                .center_x(Length::Fill)
                .padding([0, 16])
                .into()
        }

        /// The message box with this conversation's permissions, thinking and model.
        fn composer(&self) -> Element<'_, Message> {
            let size = 14.0 * self.zoom;
            let (model, effort, profile) = self.effective();
            let placeholder = if self.items.is_empty() {
                "Message Claude"
            } else {
                "Reply to Claude…"
            };
            let editor = widget::text_editor(&self.draft)
                .id(composer_id())
                .placeholder(placeholder)
                .on_action(Message::Edit)
                .key_binding(send_on_enter)
                .size(size)
                .padding(0)
                .max_height(240.0 * self.zoom)
                .class(cosmic::theme::iced::TextEditor::Custom(Box::new(
                    borderless_editor,
                )));

            let profiles: Vec<&str> = PROFILES.iter().map(|p| p.label()).collect();
            let efforts: Vec<&str> = Effort::ALL.iter().map(|e| e.label()).collect();
            let models: Vec<&str> = MODEL_PRESETS.iter().map(|(label, _)| *label).collect();
            let send = if self.streaming {
                widget::button::icon(widget::icon::from_name("media-playback-stop-symbolic"))
                    .class(cosmic::theme::Button::Standard)
                    .on_press(Message::Stop)
            } else {
                widget::button::icon(widget::icon::from_name("go-up-symbolic"))
                    .class(cosmic::theme::Button::Suggested)
                    .on_press_maybe(self.can_send().then_some(Message::Send))
            };
            let controls = widget::Row::with_children(vec![
                widget::dropdown(
                    profiles,
                    PROFILES.iter().position(|p| *p == profile),
                    Message::OvProfile,
                )
                .into(),
                widget::dropdown(
                    efforts,
                    Effort::ALL.iter().position(|e| *e == effort),
                    Message::OvEffort,
                )
                .into(),
                widget::space::horizontal().into(),
                widget::dropdown(
                    models,
                    MODEL_PRESETS.iter().position(|(_, m)| *m == model),
                    Message::OvModel,
                )
                .into(),
                send.into(),
            ])
            .spacing(8)
            .align_y(Alignment::Center);

            let boxed = widget::container(
                widget::Column::with_children(vec![editor.into(), controls.into()]).spacing(10),
            )
            .padding([12, 14])
            .width(Length::Fill)
            .class(cosmic::theme::Container::custom(|theme| {
                let cosmic = theme.cosmic();
                container::Style {
                    background: Some(Color::from(cosmic.bg_component_color()).into()),
                    border: Border {
                        radius: cosmic.corner_radii.radius_l.into(),
                        width: 1.0,
                        color: cosmic.bg_divider().into(),
                    },
                    ..container::Style::default()
                }
            }));
            widget::container(widget::container(boxed).max_width(COLUMN_WIDTH * self.zoom))
                .center_x(Length::Fill)
                .padding([0, 16, 16, 16])
                .into()
        }

        fn transcript(&self) -> Element<'_, Message> {
            let size = 14.0 * self.zoom;
            let settings = markdown_settings(size);
            let mut rows: Vec<Element<Message>> = Vec::new();
            for (idx, entry) in self.items.iter().enumerate() {
                rows.push(match entry {
                    Entry::User(text) => user_bubble(text, size, self.zoom),
                    Entry::Reply(reply) => {
                        markdown::view(reply.markdown.items(), settings).map(Message::Link)
                    }
                    Entry::Thinking { text, open } => {
                        let live = self.pulsing() && idx + 1 == self.items.len();
                        thinking_row(idx, text, *open, live.then_some(self.pulse), size)
                    }
                    Entry::Tool { call, open } => tool_row(idx, call, *open, size),
                });
            }
            if let Some(label) = self.activity() {
                rows.push(activity_row(label, self.pulse, size));
            }
            if let Some(err) = &self.error {
                rows.push(
                    widget::text(err.as_str())
                        .size(size)
                        .class(destructive())
                        .into(),
                );
            }

            let column = widget::Column::with_children(rows)
                .spacing(18.0 * self.zoom)
                .padding([24, 16])
                .max_width(COLUMN_WIDTH * self.zoom);
            widget::scrollable(widget::container(column).center_x(Length::Fill))
                .anchor_bottom()
                .height(Length::Fill)
                .into()
        }
    }

    fn markdown_settings(size: f32) -> markdown::Settings {
        let theme = cosmic::theme::active();
        let cosmic = theme.cosmic();
        let style = markdown::Style {
            font: cosmic::font::default(),
            inline_code_highlight: markdown::Highlight {
                background: Color::from(cosmic.bg_component_color()).into(),
                border: Border {
                    radius: 4.0.into(),
                    ..Border::default()
                },
            },
            inline_code_padding: Padding::from([0, 3]),
            inline_code_color: cosmic.on_bg_component_color().into(),
            inline_code_font: cosmic::font::mono(),
            code_block_font: cosmic::font::mono(),
            link_color: cosmic.accent_text_color().into(),
        };
        let mut settings = markdown::Settings::with_text_size(size, style);
        // iced's defaults (h1 at twice the body, code at three quarters) suit documents, not chat.
        settings.h1_size = (size * 1.4).into();
        settings.h2_size = (size * 1.25).into();
        settings.h3_size = (size * 1.1).into();
        settings.h4_size = size.into();
        settings.code_size = (size * 0.9).into();
        settings
    }

    fn user_bubble(text: &str, size: f32, zoom: f32) -> Element<'_, Message> {
        let bubble = widget::container(widget::text(text).size(size))
            .padding([10.0 * zoom, 14.0 * zoom])
            .max_width(BUBBLE_WIDTH * zoom)
            .class(cosmic::theme::Container::custom(|theme| {
                let cosmic = theme.cosmic();
                container::Style {
                    background: Some(Color::from(cosmic.bg_component_color()).into()),
                    text_color: Some(cosmic.on_bg_component_color().into()),
                    border: Border {
                        radius: cosmic.corner_radii.radius_l.into(),
                        ..Border::default()
                    },
                    ..container::Style::default()
                }
            }));
        widget::container(bubble).align_right(Length::Fill).into()
    }

    /// A one-line row naming the tool and what it acted on; expands to its input and output.
    fn tool_row(idx: usize, call: &ToolCall, open: bool, size: f32) -> Element<'_, Message> {
        let small = size * 0.85;
        let chevron = if open {
            "go-down-symbolic"
        } else {
            "go-next-symbolic"
        };
        let status: Element<Message> = match (&call.result, call.is_error) {
            (None, _) => widget::text("running").size(small).class(dim()).into(),
            (Some(_), true) => widget::text("error")
                .size(small)
                .class(destructive())
                .into(),
            (Some(_), false) => widget::icon::from_name("object-select-symbolic")
                .size(16)
                .into(),
        };
        let header = widget::Row::with_children(vec![
            widget::icon::from_name(chevron).size(16).into(),
            widget::text(call.name.as_str())
                .size(size)
                .font(cosmic::font::semibold())
                .into(),
            widget::text(truncate(&call.summary, PREVIEW_CHARS))
                .size(size)
                .class(dim())
                .wrapping(text::Wrapping::None)
                .width(Length::Fill)
                .into(),
            status,
        ])
        .spacing(8)
        .align_y(Alignment::Center);
        let header = widget::button::custom(header)
            .on_press(Message::Toggle(idx))
            .class(cosmic::theme::Button::Text)
            .width(Length::Fill);
        if !open {
            return header.into();
        }

        let mut details = vec![tool_text("Input", &call.input, small)];
        if let Some(result) = &call.result {
            details.push(tool_text("Output", result, small));
        }
        let details = widget::container(widget::Column::with_children(details).spacing(10))
            .padding(12)
            .width(Length::Fill)
            .class(cosmic::theme::Container::Card);
        widget::Column::with_children(vec![header.into(), details.into()])
            .spacing(4)
            .into()
    }

    /// Tool input or output in monospace, cut to its first lines: a build log
    /// can run to thousands, and the transcript lays out every one of them.
    fn tool_text<'a>(label: &'a str, body: &'a str, size: f32) -> Element<'a, Message> {
        let total = body.lines().count();
        let shown = body
            .lines()
            .take(TOOL_OUTPUT_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        let mut rows: Vec<Element<Message>> = vec![
            widget::text(label).size(size * 0.9).class(dim()).into(),
            widget::text(shown)
                .size(size)
                .font(cosmic::font::mono())
                .into(),
        ];
        if total > TOOL_OUTPUT_LINES {
            rows.push(
                widget::text(format!("{} more lines", total - TOOL_OUTPUT_LINES))
                    .size(size * 0.9)
                    .class(dim())
                    .into(),
            );
        }
        widget::Column::with_children(rows).spacing(4).into()
    }

    /// libcosmic's list item, with the title taking the accent color on hover. `Custom`
    /// closures are not told whether the button is selected, so it is captured here.
    fn conversation_style(radius: [f32; 4], selected: bool) -> cosmic::theme::Button {
        use cosmic::widget::button::Catalog;
        let list = move || cosmic::theme::Button::ListItem(radius);
        cosmic::theme::Button::Custom {
            active: Box::new(move |focused, theme| theme.active(focused, selected, &list())),
            disabled: Box::new(move |theme| theme.disabled(&list())),
            hovered: Box::new(move |focused, theme| {
                let mut style = theme.hovered(focused, selected, &list());
                style.text_color = Some(theme.cosmic().accent_text_color().into());
                style
            }),
            pressed: Box::new(move |focused, theme| theme.pressed(focused, selected, &list())),
        }
    }

    /// Three dots pulsing in turn.
    fn pulse_dots<'a>(pulse: f32, size: f32) -> Element<'a, Message> {
        let accent: Color = cosmic::theme::active().cosmic().accent_text_color().into();
        let dots: Vec<Element<Message>> = (0..3)
            .map(|i| {
                let wave = ((pulse * 5.0 - i as f32 * 0.9).sin() + 1.0) / 2.0;
                widget::text("●")
                    .size(size * 0.7)
                    .class(cosmic::theme::Text::Color(Color {
                        a: 0.25 + 0.75 * wave,
                        ..accent
                    }))
                    .into()
            })
            .collect();
        widget::Row::with_children(dots).spacing(4).into()
    }

    fn activity_row<'a>(label: String, pulse: f32, size: f32) -> Element<'a, Message> {
        widget::Row::with_children(vec![
            pulse_dots(pulse, size),
            widget::text(label).size(size).class(dim()).into(),
        ])
        .spacing(10)
        .align_y(Alignment::Center)
        .into()
    }

    /// Claude's reasoning summary as one line: its latest sentence while it is
    /// still arriving, its first once done. Expands to the whole summary.
    fn thinking_row(
        idx: usize,
        text: &str,
        open: bool,
        live: Option<f32>,
        size: f32,
    ) -> Element<'_, Message> {
        let chevron = if open {
            "go-down-symbolic"
        } else {
            "go-next-symbolic"
        };
        let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
        let preview = if live.is_some() {
            lines.next_back()
        } else {
            lines.next()
        };
        let mut header = vec![
            widget::icon::from_name(chevron).size(16).into(),
            widget::text("Thinking")
                .size(size)
                .font(cosmic::font::semibold())
                .into(),
            widget::text(truncate(preview.unwrap_or(""), PREVIEW_CHARS))
                .size(size)
                .class(dim())
                .wrapping(text::Wrapping::None)
                .width(Length::Fill)
                .into(),
        ];
        if let Some(pulse) = live {
            header.push(pulse_dots(pulse, size));
        }
        let header = widget::button::custom(
            widget::Row::with_children(header)
                .spacing(8)
                .align_y(Alignment::Center),
        )
        .on_press(Message::Toggle(idx))
        .class(cosmic::theme::Button::Text)
        .width(Length::Fill);
        if !open {
            return header.into();
        }
        let body = widget::container(widget::text(text).size(size * 0.95).class(dim()))
            .padding([4, 12, 8, 32]);
        widget::Column::with_children(vec![header.into(), body.into()])
            .spacing(2)
            .into()
    }

    fn dim() -> cosmic::theme::Text {
        let color: Color = cosmic::theme::active().cosmic().on_bg_color().into();
        cosmic::theme::Text::Color(Color { a: 0.6, ..color })
    }

    fn destructive() -> cosmic::theme::Text {
        cosmic::theme::Text::Color(cosmic::theme::active().cosmic().destructive_color().into())
    }

    fn composer_id() -> widget::Id {
        widget::Id::new("composer")
    }

    /// Enter sends; Shift+Enter and everything else keep the editor's own bindings.
    fn send_on_enter(press: text_editor::KeyPress) -> Option<text_editor::Binding<Message>> {
        let enter = matches!(press.key, keyboard::Key::Named(keyboard::key::Named::Enter));
        let focused = matches!(press.status, text_editor::Status::Focused { .. });
        if enter && focused && !press.modifiers.shift() {
            return Some(text_editor::Binding::Custom(Message::Send));
        }
        text_editor::Binding::from_key_press(press)
    }

    /// The editor without its own frame, since the composer box draws one.
    fn borderless_editor(theme: &cosmic::Theme, status: text_editor::Status) -> text_editor::Style {
        use text_editor::Catalog;
        text_editor::Style {
            background: Color::TRANSPARENT.into(),
            border: Border::default(),
            ..theme.style(&cosmic::theme::iced::TextEditor::Default, status)
        }
    }

    impl Application for ClaudoApp {
        type Executor = cosmic::executor::Default;
        type Flags = ();
        type Message = Message;
        const APP_ID: &'static str = "com.claudo.Cosmic";

        fn core(&self) -> &Core {
            &self.core
        }

        fn core_mut(&mut self) -> &mut Core {
            &mut self.core
        }

        fn init(core: Core, _flags: ()) -> (Self, Task<Message>) {
            let app = ClaudoApp {
                core,
                commands: None,
                status: None,
                settings: Settings::default(),
                overrides: Overrides::default(),
                conversations: Vec::new(),
                current: None,
                items: Vec::new(),
                draft: text_editor::Content::new(),
                focus_composer: false,
                approvals: Vec::new(),
                streaming: false,
                assistant_idx: None,
                reveal: 0.0,
                pulse: 0.0,
                zoom: 1.0,
                error: None,
                page: Page::Chat,
            };
            (app, Task::none())
        }

        fn subscription(&self) -> Subscription<Message> {
            let mut subscriptions = vec![named("backend", backend_worker), zoom_events()];
            #[cfg(feature = "hot")]
            subscriptions.push(named("hot-patches", hot_patches));
            // Timers run only while something animates; idle stays event-driven.
            if self.revealing() {
                subscriptions.push(
                    cosmic::iced::time::every(std::time::Duration::from_millis(16))
                        .map(|_| Message::Frame),
                );
            } else if self.pulsing() {
                subscriptions.push(cosmic::iced::time::every(PULSE_TICK).map(|_| Message::Pulse));
            }
            Subscription::batch(subscriptions)
        }

        fn update(&mut self, message: Message) -> Task<Message> {
            match message {
                Message::Connected(commands) => self.commands = Some(commands),
                Message::Events(evs) => {
                    for ev in evs {
                        self.apply(ev);
                    }
                }
                Message::Frame => self.advance_reveal(1.0 / 60.0),
                Message::Pulse => self.pulse += PULSE_TICK.as_secs_f32(),
                Message::Zoom(factor) => {
                    self.zoom = (self.zoom * factor).clamp(0.5, 3.0);
                }
                Message::ZoomReset => self.zoom = 1.0,
                Message::Edit(action) => self.draft.perform(action),
                Message::Send => self.send(),
                Message::Stop => {
                    if let Some(conversation_id) = self.current.clone() {
                        self.cmd(Command::Stop { conversation_id });
                    }
                }
                Message::Link(url) => {
                    std::thread::spawn(move || {
                        let _ = std::process::Command::new("xdg-open").arg(url).status();
                    });
                }
                #[cfg(feature = "hot")]
                Message::Hotpatched => {}
                Message::Toggle(idx) => {
                    if let Some(Entry::Tool { open, .. } | Entry::Thinking { open, .. }) =
                        self.items.get_mut(idx)
                    {
                        *open = !*open;
                    }
                }
                Message::Open(id) => {
                    self.page = Page::Chat;
                    self.open(id);
                }
                Message::New => {
                    self.page = Page::Chat;
                    self.cmd(Command::NewScratch);
                }
                Message::SetPage(page) => self.page = page,
                Message::Answer { request_id, allow } => {
                    self.approvals.retain(|(id, _)| *id != request_id);
                    if let Some(conversation_id) = self.current.clone() {
                        self.cmd(Command::AnswerApproval {
                            conversation_id,
                            request_id,
                            allow,
                        });
                    }
                }
                // libcosmic's Wayland teardown races between two threads and segfaults.
                // Nothing here needs tearing down: state is saved as it changes.
                Message::Quit => std::process::exit(0),
                Message::SetModel(i) => {
                    if let Some((_, v)) = MODEL_PRESETS.get(i) {
                        self.settings.model = (*v).to_string();
                        self.save_settings();
                    }
                }
                Message::ModelText(s) => {
                    self.settings.model = s;
                    self.save_settings();
                }
                Message::SetEffort(i) => {
                    if let Some(e) = Effort::ALL.get(i) {
                        self.settings.effort = *e;
                        self.save_settings();
                    }
                }
                Message::SetProfile(i) => {
                    if let Some(p) = PROFILES.get(i) {
                        self.settings.profile = *p;
                        self.save_settings();
                    }
                }
                Message::OvModel(i) => {
                    if let Some((_, model)) = MODEL_PRESETS.get(i) {
                        self.overrides.model = Some((*model).to_string());
                        self.save_overrides();
                    }
                }
                Message::OvEffort(i) => {
                    if let Some(effort) = Effort::ALL.get(i) {
                        self.overrides.effort = Some(*effort);
                        self.save_overrides();
                    }
                }
                Message::OvProfile(i) => {
                    if let Some(profile) = PROFILES.get(i) {
                        self.overrides.profile = Some(*profile);
                        self.save_overrides();
                    }
                }
            }
            if std::mem::take(&mut self.focus_composer) {
                return widget::text_input::focus(composer_id());
            }
            Task::none()
        }

        fn view(&self) -> Element<'_, Message> {
            match &self.status {
                None => return widget::text("Connecting to backend…").into(),
                Some(s) if !s.ready() => {
                    let msg = if !s.claude_installed {
                        "Claude Code CLI not found on PATH."
                    } else {
                        "CLI found, but not logged in. Run `claude auth login`."
                    };
                    return widget::text(msg).into();
                }
                Some(_) => {}
            }

            let main = match self.page {
                Page::Chat => self.conversation(),
                Page::Settings => self.settings_page(),
            };
            widget::Row::with_children(vec![
                self.sidebar(),
                widget::divider::vertical::default().into(),
                main,
            ])
            .height(Length::Fill)
            .into()
        }

        fn header_center(&self) -> Vec<Element<'_, Message>> {
            let title = match self.page {
                Page::Settings => Some("Settings".to_string()),
                Page::Chat => self.current_info().map(|c| truncate(&c.title, 60)),
            };
            title
                .map(|t| widget::text::heading(t).into())
                .into_iter()
                .collect()
        }

        fn header_end(&self) -> Vec<Element<'_, Message>> {
            if self.page != Page::Chat {
                return Vec::new();
            }
            self.current_info()
                .and_then(|c| context_label(&c.context))
                .map(|label| widget::text(label).size(12.0).class(dim()).into())
                .into_iter()
                .collect()
        }

        fn on_close_requested(&self, id: cosmic::iced::window::Id) -> Option<Message> {
            (self.core.main_window_id() == Some(id)).then_some(Message::Quit)
        }

        fn on_app_exit(&mut self) -> Option<Message> {
            Some(Message::Quit)
        }
    }

    /// "12.3k / 1.0M ctx (1%)", or None before the first turn.
    fn context_label(ctx: &claudo_protocol::ContextUsage) -> Option<String> {
        if ctx.used == 0 {
            return None;
        }
        let used = compact(ctx.used);
        match (ctx.window, ctx.percent()) {
            (Some(w), Some(p)) => Some(format!("{used} / {} ctx ({p:.0}%)", compact(w))),
            _ => Some(format!("{used} ctx")),
        }
    }

    fn compact(n: u64) -> String {
        match n {
            n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1_000_000.0),
            n if n >= 1_000 => format!("{:.1}k", n as f64 / 1_000.0),
            n => n.to_string(),
        }
    }

    /// Byte offset of the `n`th char of `s`, or its length.
    fn char_offset(s: &str, n: usize) -> usize {
        s.char_indices().nth(n).map_or(s.len(), |(i, _)| i)
    }

    fn truncate(s: &str, max: usize) -> String {
        if s.chars().count() > max {
            format!("{}…", s.chars().take(max - 1).collect::<String>())
        } else {
            s.to_string()
        }
    }

    /// Ctrl+scroll and Ctrl+`+`/`-`/`0`. Scroll events carry no modifier state, so
    /// Ctrl is tracked here; a plain scroll yields no message, as each one rebuilds the view.
    fn zoom_events() -> Subscription<Message> {
        use std::sync::atomic::{AtomicBool, Ordering};

        use cosmic::iced::Event;
        use cosmic::iced::keyboard::{Event as Key, Key as K};
        use cosmic::iced::mouse::{Event as Mouse, ScrollDelta};

        static CTRL: AtomicBool = AtomicBool::new(false);

        cosmic::iced::event::listen_with(|event, _status, _id| match event {
            Event::Keyboard(Key::ModifiersChanged(m)) => {
                CTRL.store(m.control(), Ordering::Relaxed);
                None
            }
            Event::Keyboard(Key::KeyPressed { key, modifiers, .. }) if modifiers.control() => {
                match key.as_ref() {
                    K::Character("+") | K::Character("=") => Some(Message::Zoom(1.1)),
                    K::Character("-") => Some(Message::Zoom(1.0 / 1.1)),
                    K::Character("0") => Some(Message::ZoomReset),
                    _ => None,
                }
            }
            Event::Mouse(Mouse::WheelScrolled { delta }) if CTRL.load(Ordering::Relaxed) => {
                let dy = match delta {
                    ScrollDelta::Lines { y, .. } | ScrollDelta::Pixels { y, .. } => y,
                };
                (dy != 0.0).then(|| Message::Zoom(if dy > 0.0 { 1.1 } else { 1.0 / 1.1 }))
            }
            _ => None,
        })
    }

    /// A subscription identified by a fixed name. `Subscription::run` hashes its
    /// function's address, which a hot patch changes, so the backend would restart.
    struct Named<S> {
        name: &'static str,
        stream: fn() -> S,
    }

    impl<S: Stream<Item = Message> + Send + 'static> Recipe for Named<S> {
        type Output = Message;

        fn hash(&self, state: &mut Hasher) {
            self.name.hash(state);
        }

        fn stream(self: Box<Self>, _input: EventStream) -> BoxStream<'static, Message> {
            (self.stream)().boxed()
        }
    }

    fn named<S: Stream<Item = Message> + Send + 'static>(
        name: &'static str,
        stream: fn() -> S,
    ) -> Subscription<Message> {
        from_recipe(Named { name, stream })
    }

    #[cfg(feature = "hot")]
    fn hot_patches() -> impl Stream<Item = Message> {
        stream::channel(
            1,
            |mut output: cosmic::iced::futures::channel::mpsc::Sender<Message>| async move {
                let (patched, mut patches) = cosmic::iced::futures::channel::mpsc::unbounded();
                iced_debug::on_hotpatch(move || {
                    let _ = patched.unbounded_send(());
                });
                while patches.next().await.is_some() {
                    let _ = output.send(Message::Hotpatched).await;
                }
            },
        )
    }

    /// Worker: owns the backend, streams events as messages. The first message
    /// hands the UI a command sender.
    fn backend_worker() -> impl cosmic::iced::futures::Stream<Item = Message> {
        stream::channel(
            64,
            |mut output: cosmic::iced::futures::channel::mpsc::Sender<Message>| async move {
                let (commands, mut events) = spawn_backend();
                commands.send(Command::GetStatus);
                commands.send(Command::GetSettings);
                commands.send(Command::ListConversations);
                let _ = output.send(Message::Connected(commands)).await;
                while let Some(ev) = events.recv().await {
                    // Coalesce any already-queued events into one render.
                    let mut batch = vec![ev];
                    while let Some(more) = events.try_recv() {
                        batch.push(more);
                        if batch.len() >= 256 {
                            break;
                        }
                    }
                    if output.send(Message::Events(batch)).await.is_err() {
                        break;
                    }
                }
            },
        )
    }
}
