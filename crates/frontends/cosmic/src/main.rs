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
        Command, ConversationInfo, Effort, Item, Overrides, Profile, ServerEvent, Settings,
        SystemStatus, ToolCall, MODEL_PRESETS,
    };
    use claudo_transport::{spawn_backend, Commands};

    use cosmic::app::{Core, Task};
    use cosmic::iced::futures::SinkExt;
    use cosmic::iced::{stream, Length, Subscription};
    use cosmic::widget;
    use cosmic::{Application, Element};

    const PROFILES: [Profile; 3] = [Profile::Safe, Profile::AcceptEdits, Profile::FullAuto];

    pub struct ClaudoApp {
        core: Core,
        commands: Option<Commands>,
        status: Option<SystemStatus>,
        settings: Settings,
        overrides: Overrides,
        conversations: Vec<ConversationInfo>,
        current: Option<String>,
        items: Vec<Item>,
        input: String,
        streaming: bool,
        assistant_idx: Option<usize>,
        /// Chars of the streaming message revealed so far. The CLI delivers text
        /// in large chunks; revealing gradually reads as smooth typing.
        reveal: f32,
        /// Text scale. libcosmic exposes no runtime window scale factor, so zoom
        /// is applied to the conversation text rather than the whole window.
        zoom: f32,
        /// Ctrl held, tracked separately because scroll events carry no modifiers.
        ctrl: bool,
        error: Option<String>,
    }

    #[derive(Debug, Clone)]
    pub enum Message {
        Connected(Commands),
        Events(Vec<ServerEvent>),
        /// Animation frame while text is being revealed.
        Frame,
        /// Ctrl pressed/released.
        Ctrl(bool),
        /// Multiply the text scale (Ctrl+scroll, Ctrl+`+`/`-`).
        Zoom(f32),
        /// Reset text scale (Ctrl+0).
        ZoomReset,
        /// Wheel movement; zooms only while Ctrl is held.
        Scroll(f32),
        Input(String),
        Send,
        Open(String),
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
            self.assistant_idx = None;
            self.error = None;
            self.cmd(Command::OpenConversation { id });
        }

        fn send(&mut self) {
            let text = self.input.trim().to_string();
            let Some(current) = self.current.clone() else {
                return;
            };
            if text.is_empty() || self.streaming {
                return;
            }
            self.error = None;
            self.items.push(Item::User(text.clone()));
            self.assistant_idx = None;
            self.streaming = true;
            let (model, effort, profile) = self.effective();
            self.cmd(Command::SendMessage {
                conversation_id: current,
                text,
                profile,
                model,
                effort,
            });
            self.input.clear();
        }

        fn push_assistant(&mut self, text: &str) {
            match self.assistant_idx {
                Some(i) => {
                    if let Item::Assistant(s) = &mut self.items[i] {
                        s.push_str(text);
                    }
                }
                None => {
                    self.items.push(Item::Assistant(text.to_string()));
                    self.assistant_idx = Some(self.items.len() - 1);
                    self.reveal = 0.0;
                }
            }
        }

        fn stream_target(&self) -> Option<usize> {
            let i = self.assistant_idx?;
            match self.items.get(i) {
                Some(Item::Assistant(s)) => Some(s.chars().count()),
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
                        self.items = items;
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
                ServerEvent::ToolStarted {
                    conversation_id,
                    call_id,
                    name,
                    input,
                } => {
                    if self.is_current(&conversation_id) {
                        self.items.push(Item::Tool(ToolCall {
                            call_id,
                            name,
                            input,
                            result: None,
                            is_error: false,
                        }));
                        self.assistant_idx = None;
                    }
                }
                ServerEvent::ToolResult {
                    conversation_id,
                    call_id,
                    content,
                    is_error,
                } => {
                    if self.is_current(&conversation_id) {
                        if let Some(Item::Tool(tc)) = self
                            .items
                            .iter_mut()
                            .rev()
                            .find(|it| matches!(it, Item::Tool(t) if t.call_id == call_id))
                        {
                            tc.result = Some(content);
                            tc.is_error = is_error;
                        }
                    }
                }
                ServerEvent::Completed {
                    conversation_id,
                    is_error,
                    ..
                } => {
                    if self.is_current(&conversation_id) {
                        self.streaming = false;
                        self.assistant_idx = None;
                        if is_error {
                            self.error = Some("Claude reported an error.".to_string());
                        }
                    }
                }
                ServerEvent::Error { detail } => {
                    self.error = Some(detail);
                    self.streaming = false;
                }
            }
        }

        fn settings_view(&self) -> Element<'_, Message> {
            let model_labels: Vec<&str> = MODEL_PRESETS.iter().map(|(l, _)| *l).collect();
            let model_sel = MODEL_PRESETS
                .iter()
                .position(|(_, v)| *v == self.settings.model);
            let effort_labels: Vec<&str> = Effort::ALL.iter().map(|e| e.label()).collect();
            let effort_sel = Effort::ALL.iter().position(|e| *e == self.settings.effort);
            let profile_labels: Vec<&str> = PROFILES.iter().map(|p| p.label()).collect();
            let profile_sel = PROFILES.iter().position(|p| *p == self.settings.profile);

            let global = widget::Row::with_children(vec![
                widget::text("Model").into(),
                widget::dropdown(model_labels, model_sel, Message::SetModel).into(),
                widget::text_input("version…", &self.settings.model)
                    .on_input(Message::ModelText)
                    .width(Length::Fixed(190.0))
                    .into(),
                widget::text("Thinking").into(),
                widget::dropdown(effort_labels, effort_sel, Message::SetEffort).into(),
                widget::text("Permissions").into(),
                widget::dropdown(profile_labels, profile_sel, Message::SetProfile).into(),
            ])
            .spacing(6);

            let mut rows: Vec<Element<Message>> = Vec::new();
            if let Some(ctx) = self
                .current
                .as_deref()
                .and_then(|id| self.conversations.iter().find(|c| c.id == id))
                .and_then(|c| context_label(&c.context))
            {
                rows.push(widget::text(ctx).size(12.0).into());
            }
            rows.push(widget::text::heading("Global defaults").into());
            rows.push(global.into());

            if self.current.is_some() {
                let mut model_opts = vec!["Use global"];
                model_opts.extend(MODEL_PRESETS.iter().map(|(l, _)| *l));
                let model_sel = match &self.overrides.model {
                    None => Some(0),
                    Some(m) => MODEL_PRESETS
                        .iter()
                        .position(|(_, v)| v == m)
                        .map(|i| i + 1),
                };
                let mut effort_opts = vec!["Use global"];
                effort_opts.extend(Effort::ALL.iter().map(|e| e.label()));
                let effort_sel = match self.overrides.effort {
                    None => Some(0),
                    Some(e) => Effort::ALL.iter().position(|x| *x == e).map(|i| i + 1),
                };
                let mut profile_opts = vec!["Use global"];
                profile_opts.extend(PROFILES.iter().map(|p| p.label()));
                let profile_sel = match self.overrides.profile {
                    None => Some(0),
                    Some(p) => PROFILES.iter().position(|x| *x == p).map(|i| i + 1),
                };

                let ov = widget::Row::with_children(vec![
                    widget::text("Model").into(),
                    widget::dropdown(model_opts, model_sel, Message::OvModel).into(),
                    widget::text("Thinking").into(),
                    widget::dropdown(effort_opts, effort_sel, Message::OvEffort).into(),
                    widget::text("Permissions").into(),
                    widget::dropdown(profile_opts, profile_sel, Message::OvProfile).into(),
                ])
                .spacing(6);
                rows.push(widget::text::heading("This conversation (overrides)").into());
                rows.push(ov.into());
            }

            widget::Column::with_children(rows).spacing(6).into()
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
                input: String::new(),
                streaming: false,
                assistant_idx: None,
                reveal: 0.0,
                zoom: 1.0,
                ctrl: false,
                error: None,
            };
            (app, Task::none())
        }

        fn subscription(&self) -> Subscription<Message> {
            let backend = Subscription::batch([Subscription::run(backend_worker), zoom_events()]);
            if self.revealing() {
                // Animation frames only while text is still being revealed;
                // idle stays event-driven.
                Subscription::batch([
                    backend,
                    cosmic::iced::time::every(std::time::Duration::from_millis(16))
                        .map(|_| Message::Frame),
                ])
            } else {
                backend
            }
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
                Message::Ctrl(down) => self.ctrl = down,
                Message::Zoom(factor) => {
                    self.zoom = (self.zoom * factor).clamp(0.5, 3.0);
                }
                Message::ZoomReset => self.zoom = 1.0,
                Message::Scroll(dy) => {
                    if self.ctrl && dy != 0.0 {
                        let factor = if dy > 0.0 { 1.1 } else { 1.0 / 1.1 };
                        self.zoom = (self.zoom * factor).clamp(0.5, 3.0);
                    }
                }
                Message::Input(s) => self.input = s,
                Message::Send => self.send(),
                Message::Open(id) => self.open(id),
                Message::New => self.cmd(Command::NewScratch),
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
                    self.overrides.model = i
                        .checked_sub(1)
                        .and_then(|j| MODEL_PRESETS.get(j))
                        .map(|(_, v)| (*v).to_string());
                    self.save_overrides();
                }
                Message::OvEffort(i) => {
                    self.overrides.effort = i.checked_sub(1).and_then(|j| Effort::ALL.get(j)).copied();
                    self.save_overrides();
                }
                Message::OvProfile(i) => {
                    self.overrides.profile = i.checked_sub(1).and_then(|j| PROFILES.get(j)).copied();
                    self.save_overrides();
                }
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

            // Sidebar.
            let mut convs: Vec<Element<Message>> = Vec::new();
            for c in &self.conversations {
                let title = truncate(&c.title, 26);
                let label = if self.is_current(&c.id) {
                    format!("▸ {title}")
                } else {
                    format!("  {title}")
                };
                // `wrapping(None)` keeps each entry on one line; wrapped labels
                // overflowed their button and overlapped the next row.
                let text = widget::text(label)
                    .size(14.0 * self.zoom)
                    .wrapping(cosmic::iced::widget::text::Wrapping::None)
                    .width(Length::Fill);
                convs.push(
                    widget::button::custom(text)
                        .on_press(Message::Open(c.id.clone()))
                        .width(Length::Fill)
                        .into(),
                );
            }
            let list = widget::scrollable(widget::Column::with_children(convs).spacing(2))
                .height(Length::Fill);
            let sidebar = widget::container(
                widget::Column::with_children(vec![
                    widget::button::standard("+ New")
                        .on_press(Message::New)
                        .width(Length::Fill)
                        .into(),
                    list.into(),
                ])
                .spacing(10),
            )
            .width(Length::Fixed(240.0))
            .height(Length::Fill)
            .padding(8);

            // Transcript.
            let body = 14.0 * self.zoom;
            let head = 16.0 * self.zoom;
            let small = 12.0 * self.zoom;
            let mut lines: Vec<Element<Message>> = Vec::new();
            for (idx, item) in self.items.iter().enumerate() {
                match item {
                    Item::User(t) => {
                        lines.push(widget::text("You").size(head).into());
                        lines.push(widget::text(t.clone()).size(body).into());
                    }
                    Item::Assistant(t) => {
                        lines.push(widget::text("Claude").size(head).into());
                        // Only the streaming bubble reveals gradually.
                        let shown = if Some(idx) == self.assistant_idx {
                            take_chars(t, self.reveal as usize).to_string()
                        } else {
                            t.clone()
                        };
                        lines.push(widget::text(shown).size(body).into());
                    }
                    Item::Tool(tc) => {
                        let status = match (&tc.result, tc.is_error) {
                            (None, _) => "running",
                            (Some(_), false) => "done",
                            (Some(_), true) => "error",
                        };
                        lines.push(
                            widget::text(format!("[tool] {}: {}", tc.name, status))
                                .size(body)
                                .into(),
                        );
                        if let Some(r) = &tc.result {
                            lines.push(widget::text(r.clone()).size(small).into());
                        }
                    }
                }
            }
            if let Some(err) = &self.error {
                lines.push(widget::text(err.clone()).into());
            }
            let transcript = widget::scrollable(widget::Column::with_children(lines).spacing(6))
                .height(Length::Fill);

            // Composer.
            let mut send_btn = widget::button::standard("Send");
            if !self.streaming && !self.input.trim().is_empty() {
                send_btn = send_btn.on_press(Message::Send);
            }
            let composer = widget::Row::with_children(vec![
                widget::text_input("Message Claude", &self.input)
                    .on_input(Message::Input)
                    .on_submit(|_| Message::Send)
                    .width(Length::Fill)
                    .into(),
                send_btn.into(),
            ])
            .spacing(8);

            let main = widget::Column::with_children(vec![
                transcript.into(),
                self.settings_view(),
                composer.into(),
            ])
            .spacing(8)
            .width(Length::Fill)
            .height(Length::Fill);

            widget::Row::with_children(vec![sidebar.into(), main.into()])
                .spacing(12)
                .padding(12)
                .into()
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

    /// First `n` chars of `s`, respecting char boundaries.
    fn take_chars(s: &str, n: usize) -> &str {
        match s.char_indices().nth(n) {
            Some((i, _)) => &s[..i],
            None => s,
        }
    }

    fn truncate(s: &str, max: usize) -> String {
        if s.chars().count() > max {
            format!("{}…", s.chars().take(max - 1).collect::<String>())
        } else {
            s.to_string()
        }
    }

    /// Ctrl+scroll and Ctrl+`+`/`-`/`0`. Scroll events carry no modifier state,
    /// so Ctrl is tracked from `ModifiersChanged`.
    fn zoom_events() -> Subscription<Message> {
        use cosmic::iced::keyboard::{Event as Key, Key as K};
        use cosmic::iced::mouse::{Event as Mouse, ScrollDelta};
        use cosmic::iced::Event;

        cosmic::iced::event::listen_with(|event, _status, _id| match event {
            Event::Keyboard(Key::ModifiersChanged(m)) => Some(Message::Ctrl(m.control())),
            Event::Keyboard(Key::KeyPressed { key, modifiers, .. }) if modifiers.control() => {
                match key.as_ref() {
                    K::Character("+") | K::Character("=") => Some(Message::Zoom(1.1)),
                    K::Character("-") => Some(Message::Zoom(1.0 / 1.1)),
                    K::Character("0") => Some(Message::ZoomReset),
                    _ => None,
                }
            }
            Event::Mouse(Mouse::WheelScrolled { delta }) => {
                let dy = match delta {
                    ScrollDelta::Lines { y, .. } | ScrollDelta::Pixels { y, .. } => y,
                };
                Some(Message::Scroll(dy))
            }
            _ => None,
        })
    }

    /// Worker: owns the backend, streams events as messages. The first message
    /// hands the UI a command sender.
    fn backend_worker() -> impl cosmic::iced::futures::Stream<Item = Message> {
        stream::channel(64, |mut output: cosmic::iced::futures::channel::mpsc::Sender<Message>| async move {
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
        })
    }
}
