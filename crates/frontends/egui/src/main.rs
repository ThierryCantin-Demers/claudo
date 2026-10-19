//! Default frontend: a native egui window. As of M4a it is transport-agnostic,
//! it sends `Command`s and renders `ServerEvent`s, with no direct dependency on
//! the backend. Native bundles the backend in-process; the web client (M4b)
//! speaks the same protocol over a WebSocket.

use std::time::Duration;

use claudo_protocol::{
    Attachment, Command, ConversationInfo, Decision, Effort, Item, ModelOption, Overrides, Profile,
    Review, ServerEvent, SessionInfo, Settings, SystemStatus, ToolCall,
};
use claudo_transport::{Transport, connect};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([900.0, 700.0]),
        ..Default::default()
    };
    eframe::run_native(
        "claudo",
        options,
        Box::new(|cc| {
            install_jp_font(&cc.egui_ctx);
            Ok(Box::new(ClaudoApp::new()))
        }),
    )
}

/// Web entry: eframe drives the `<canvas id="claudo_canvas">` in index.html.
#[cfg(target_arch = "wasm32")]
fn main() {
    use eframe::wasm_bindgen::JsCast as _;

    wasm_bindgen_futures::spawn_local(async {
        let canvas = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("claudo_canvas"))
            .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
            .expect("a <canvas id=\"claudo_canvas\"> element");
        eframe::WebRunner::new()
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(|cc| {
                    install_jp_font(&cc.egui_ctx);
                    Ok(Box::new(ClaudoApp::new()))
                }),
            )
            .await
            .expect("failed to start eframe");
    });
}

/// Deferred sidebar action, applied after panels are drawn to avoid borrow
/// conflicts with the panel closures.
enum Action {
    None,
    Open(String),
    NewScratch,
    ShowSessions,
    Import(String),
    /// Native only; the browser can't pick the daemon's server-side folders.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    PickFolder,
}

struct ClaudoApp {
    transport: Box<dyn Transport>,
    status: Option<SystemStatus>,
    conversations: Vec<ConversationInfo>,

    current: Option<String>,
    items: Vec<Item>,

    input: String,
    /// Files the next message sends.
    attachments: Vec<Attachment>,
    /// Persisted global defaults (model, thinking, permissions) used for turns.
    settings: Settings,
    /// The CLI's model families, with the versions their aliases run today.
    models: Vec<ModelOption>,
    /// Current conversation's per-field overrides (each falls back to `settings`).
    overrides: Overrides,
    show_settings: bool,
    show_sessions: bool,
    /// What the sessions window lists; `None` while they are being found.
    sessions: Option<Vec<SessionInfo>>,
    streaming: bool,
    /// Index of the assistant text item currently being filled by deltas.
    assistant_idx: Option<usize>,
    /// Characters of the streaming message revealed so far. The CLI delivers
    /// text in large chunks; revealing it gradually reads as smooth typing
    /// instead of sudden jumps.
    reveal: f32,
    error: Option<String>,
    /// Calls waiting for the user's answer: request id, call, and what allowing always covers.
    approvals: Vec<(String, ToolCall, String)>,
    md_cache: CommonMarkCache,
}

impl ClaudoApp {
    fn new() -> Self {
        let mut transport = connect();
        transport.send(Command::GetStatus);
        transport.send(Command::GetSettings);
        transport.send(Command::GetCapabilities);
        transport.send(Command::ListConversations);
        Self {
            transport,
            status: None,
            conversations: Vec::new(),
            current: None,
            items: Vec::new(),
            input: String::new(),
            attachments: Vec::new(),
            settings: Settings::default(),
            models: Vec::new(),
            overrides: Overrides::default(),
            show_settings: false,
            show_sessions: false,
            sessions: None,
            streaming: false,
            assistant_idx: None,
            reveal: 0.0,
            error: None,
            approvals: Vec::new(),
            md_cache: CommonMarkCache::default(),
        }
    }

    /// Ctrl+scroll (and trackpad pinch) zoom. egui reports the gesture but does
    /// not apply it; Ctrl+`+`/`-`/`0` is already handled by egui itself.
    fn handle_zoom(&self, ui: &egui::Ui) {
        let delta = ui.input(|i| i.zoom_delta());
        if delta != 1.0 {
            let ctx = ui.ctx();
            ctx.set_zoom_factor((ctx.zoom_factor() * delta).clamp(0.5, 3.0));
        }
    }

    /// Chars of the streaming message currently revealed.
    fn revealed(&self) -> usize {
        self.reveal as usize
    }

    /// Total chars of the streaming message, if one is in flight.
    fn stream_target(&self) -> Option<usize> {
        let i = self.assistant_idx?;
        match self.items.get(i) {
            Some(Item::Assistant(s)) => Some(s.chars().count()),
            _ => None,
        }
    }

    /// Advance the reveal animation. Returns true if more frames are needed.
    fn advance_reveal(&mut self, dt: f32) -> bool {
        let Some(target) = self.stream_target() else {
            return false;
        };
        if self.revealed() >= target {
            return false;
        }
        let backlog = target as f32 - self.reveal;
        // Drain the backlog over ~120ms, but never slower than a readable
        // baseline, so small chunks still animate rather than pop in.
        let per_sec = (backlog / 0.12).max(220.0);
        self.reveal = (self.reveal + per_sec * dt).min(target as f32);
        self.revealed() < target
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
        self.transport.send(Command::OpenConversation { id });
    }

    /// Load the current conversation's overrides from the sidebar list.
    fn sync_overrides(&mut self) {
        self.overrides = self
            .current
            .as_deref()
            .and_then(|id| self.conversations.iter().find(|c| c.id == id))
            .map(|c| c.overrides.clone())
            .unwrap_or_default();
    }

    /// Effective (model, effort, profile) for a turn: override, else global.
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

    fn send(&mut self) {
        let text = self.input.trim().to_string();
        let Some(current) = self.current.clone() else {
            return;
        };
        if (text.is_empty() && self.attachments.is_empty()) || self.streaming {
            return;
        }
        self.error = None;
        if !text.is_empty() {
            self.items.push(Item::User(text.clone()));
        }
        let attachments = std::mem::take(&mut self.attachments);
        self.items
            .extend(attachments.iter().cloned().map(Item::File));
        self.assistant_idx = None;
        self.streaming = true;
        let (model, effort, profile) = self.effective();
        self.transport.send(Command::SendMessage {
            conversation_id: current,
            text,
            profile,
            model,
            effort,
            attachments,
        });
        self.input.clear();
    }

    fn save_settings(&mut self) {
        self.transport.send(Command::UpdateSettings {
            settings: self.settings.clone(),
        });
    }

    fn push_assistant_text(&mut self, text: &str) {
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

    fn is_current(&self, conversation_id: &str) -> bool {
        self.current.as_deref() == Some(conversation_id)
    }

    fn drain_events(&mut self) {
        for ev in self.transport.poll() {
            match ev {
                ServerEvent::Status(s) => self.status = Some(s),

                ServerEvent::Settings(s) => self.settings = s,

                ServerEvent::Models(models) => self.models = models,
                ServerEvent::Commands(_)
                | ServerEvent::Waiting { .. }
                | ServerEvent::Queued { .. }
                | ServerEvent::Retrying { .. }
                | ServerEvent::Compacting { .. }
                | ServerEvent::CompactSummary { .. } => {}
                ServerEvent::Compacted {
                    conversation_id,
                    compaction,
                } => {
                    if self.is_current(&conversation_id) {
                        self.items.push(Item::Compacted(compaction));
                    }
                }

                ServerEvent::Conversations { list } => {
                    self.conversations = list;
                    if self.current.is_none() {
                        match self.conversations.first() {
                            Some(first) => {
                                let id = first.id.clone();
                                self.open(id);
                            }
                            None => self.transport.send(Command::NewScratch { request: 0 }),
                        }
                    }
                }

                ServerEvent::Created {
                    conversation_id,
                    imported: true,
                    ..
                } => self.open(conversation_id),

                ServerEvent::Created {
                    conversation_id, ..
                } => {
                    self.current = Some(conversation_id);
                    self.sync_overrides();
                    self.items.clear();
                    self.assistant_idx = None;
                    self.error = None;
                }

                ServerEvent::Sessions { list } => self.sessions = Some(list),
                ServerEvent::Folders { .. } | ServerEvent::Delivered { .. } => {}

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

                ServerEvent::Incoming {
                    conversation_id,
                    from,
                    text,
                } => {
                    if self.is_current(&conversation_id) {
                        self.assistant_idx = None;
                        self.streaming = true;
                        self.items.push(Item::Incoming { from, text });
                    }
                }

                ServerEvent::Typed {
                    conversation_id,
                    text,
                } => {
                    if self.is_current(&conversation_id) {
                        self.assistant_idx = None;
                        self.streaming = true;
                        self.items.push(Item::User(text));
                    }
                }

                ServerEvent::Working { conversation_id } => {
                    if self.is_current(&conversation_id) {
                        self.streaming = true;
                    }
                }

                ServerEvent::Notice {
                    conversation_id,
                    text,
                } => {
                    if self.is_current(&conversation_id) {
                        self.assistant_idx = None;
                        self.streaming = true;
                        self.items.push(Item::Notice(text));
                    }
                }

                ServerEvent::Delta {
                    conversation_id,
                    text,
                } => {
                    if self.is_current(&conversation_id) {
                        self.push_assistant_text(&text);
                    }
                }

                ServerEvent::Review {
                    conversation_id,
                    review,
                } => {
                    if self.is_current(&conversation_id) {
                        self.assistant_idx = None;
                        self.items.push(Item::Review(review));
                    }
                }

                ServerEvent::ThinkingDelta {
                    conversation_id,
                    text,
                } => {
                    if self.is_current(&conversation_id) {
                        if let Some(Item::Thinking(t)) = self.items.last_mut() {
                            t.push_str(&text);
                        } else {
                            self.items.push(Item::Thinking(text));
                            self.assistant_idx = None;
                        }
                    }
                }

                ServerEvent::ToolStarted {
                    conversation_id,
                    call,
                } => {
                    if self.is_current(&conversation_id) {
                        self.items.push(Item::Tool(call));
                        self.assistant_idx = None;
                    }
                }

                ServerEvent::Background {
                    conversation_id,
                    call_id,
                    background,
                    ..
                } => {
                    if self.is_current(&conversation_id)
                        && let Some(Item::Tool(tc)) = self
                            .items
                            .iter_mut()
                            .rev()
                            .find(|it| matches!(it, Item::Tool(t) if t.call_id == call_id))
                    {
                        tc.background = Some(background);
                    }
                }
                ServerEvent::ToolResult {
                    conversation_id,
                    call_id,
                    content,
                    is_error,
                    ..
                } => {
                    if self.is_current(&conversation_id)
                        && let Some(Item::Tool(tc)) = self
                            .items
                            .iter_mut()
                            .rev()
                            .find(|it| matches!(it, Item::Tool(t) if t.call_id == call_id))
                    {
                        tc.result = Some(content);
                        tc.is_error = is_error;
                    }
                }

                ServerEvent::ApprovalRequested {
                    conversation_id,
                    request_id,
                    call,
                    scope,
                } => {
                    if self.is_current(&conversation_id) {
                        self.approvals.push((request_id, call, scope));
                    }
                }

                ServerEvent::Completed {
                    conversation_id,
                    is_error,
                    time,
                    ..
                } => {
                    if self.is_current(&conversation_id) {
                        self.approvals.clear();
                        self.streaming = false;
                        self.assistant_idx = None;
                        self.items.extend(time.map(Item::Ended));
                        if is_error {
                            self.error =
                                Some("Claude reported an error for this turn.".to_string());
                        }
                    }
                }

                ServerEvent::Quota(_) | ServerEvent::QuotaUnavailable(_) => {}

                ServerEvent::Error {
                    conversation_id,
                    detail,
                } => {
                    if conversation_id.is_some() && conversation_id != self.current {
                        continue;
                    }
                    self.error = Some(detail);
                    self.approvals.clear();
                    self.streaming = false;
                }
            }
        }
    }
}

impl eframe::App for ClaudoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.handle_zoom(ui);
        let dt = ui.input(|i| i.stable_dt).clamp(0.0, 0.1);
        let animating = self.advance_reveal(dt);
        // Poll for backend events even when idle; animate at full rate while
        // text is still being revealed.
        if self.streaming || animating {
            ui.ctx().request_repaint();
        } else {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        }

        match &self.status {
            None => {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Connecting to backend…");
                    });
                });
                return;
            }
            Some(s) if !s.ready() => {
                self.not_ready_view(ui);
                return;
            }
            Some(_) => {}
        }

        let mut action = Action::None;
        self.sidebar(ui, &mut action);
        self.header(ui);
        self.composer(ui);
        self.transcript(ui);
        self.settings_window(ui);
        self.sessions_window(ui, &mut action);

        match action {
            Action::Open(id) => self.open(id),
            Action::NewScratch => self.transport.send(Command::NewScratch { request: 0 }),
            Action::ShowSessions => {
                self.sessions = None;
                self.show_sessions = true;
                self.transport.send(Command::ListSessions);
            }
            Action::Import(session_id) => {
                self.show_sessions = false;
                self.transport.send(Command::Import {
                    session_id,
                    request: 0,
                });
            }
            Action::PickFolder =>
            {
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                    self.transport.send(Command::NewAttached {
                        path: folder.display().to_string(),
                        request: 0,
                    });
                }
            }
            Action::None => {}
        }
    }
}

impl ClaudoApp {
    fn sidebar(&mut self, ui: &mut egui::Ui, action: &mut Action) {
        egui::Panel::left("conversations")
            .resizable(true)
            .default_size(230.0)
            .show(ui, |ui| {
                let streaming = self.streaming;
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!streaming, egui::Button::new("+ New"))
                        .clicked()
                    {
                        *action = Action::NewScratch;
                    }
                    // The browser can't pick a folder on the daemon's machine.
                    #[cfg(not(target_arch = "wasm32"))]
                    if ui
                        .add_enabled(!streaming, egui::Button::new("📁 Folder…"))
                        .on_hover_text("Start a conversation in an existing project folder")
                        .clicked()
                    {
                        *action = Action::PickFolder;
                    }
                    if ui
                        .add_enabled(!streaming, egui::Button::new("Sessions…"))
                        .on_hover_text("Continue a session started in Claude Desktop or a terminal")
                        .clicked()
                    {
                        *action = Action::ShowSessions;
                    }
                });
                ui.separator();

                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_enabled_ui(!streaming, |ui| {
                        for meta in &self.conversations {
                            let selected = self.current.as_deref() == Some(meta.id.as_str());
                            if ui.selectable_label(selected, &meta.title).clicked() {
                                *action = Action::Open(meta.id.clone());
                            }
                        }
                    });
                });
            });
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(4.0);
            let current = self
                .current
                .as_deref()
                .and_then(|id| self.conversations.iter().find(|c| c.id == id));
            match current {
                Some(meta) => {
                    ui.strong(&meta.title);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&meta.cwd).weak().small().monospace())
                            .on_hover_text(
                                "Working directory the Claude Code CLI runs in for this conversation",
                            );
                        if let Some(text) = context_label(&meta.context) {
                            ui.separator();
                            ui.label(egui::RichText::new(text).weak().small())
                                .on_hover_text(
                                    "Context used by the last turn vs the model's context window",
                                );
                        }
                    });
                }
                None => {
                    ui.strong("claudo");
                }
            }
            ui.add_space(4.0);
        });
    }

    fn sessions_window(&mut self, ui: &mut egui::Ui, action: &mut Action) {
        let mut open = self.show_sessions;
        egui::Window::new("Claude Code sessions")
            .open(&mut open)
            .collapsible(false)
            .default_width(480.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                ui.label(
                    egui::RichText::new(
                        "Started in Claude Desktop or a terminal. One continues here as a copy: \
                         its first message forks it, and the original stays as it was.",
                    )
                    .weak()
                    .small(),
                );
                ui.add_space(8.0);
                match &self.sessions {
                    None => {
                        ui.spinner();
                    }
                    Some(list) if list.is_empty() => {
                        ui.label("Every Claude Code session already has a conversation here.");
                    }
                    Some(list) => {
                        egui::ScrollArea::vertical()
                            .max_height(420.0)
                            .show(ui, |ui| {
                                for session in list {
                                    if ui.selectable_label(false, &session.title).clicked() {
                                        *action = Action::Import(session.session_id.clone());
                                    }
                                    let detail = format!("{} · {}", session.source, session.cwd);
                                    ui.label(egui::RichText::new(detail).weak().small());
                                    ui.add_space(4.0);
                                }
                            });
                    }
                }
            });
        self.show_sessions = open;
    }

    fn settings_window(&mut self, ui: &mut egui::Ui) {
        let mut open = self.show_settings;
        let mut changed = false;
        let mut overrides_changed = false;

        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                ui.label(
                    egui::RichText::new("Global defaults, applied to every message.")
                        .weak()
                        .small(),
                );
                ui.add_space(8.0);

                ui.strong("Model");
                let selected = if self.settings.model.is_empty() {
                    "Default (claude's configured model)".to_string()
                } else {
                    self.settings.model.clone()
                };
                egui::ComboBox::from_id_salt("settings_model")
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        for model in &self.models {
                            if ui
                                .selectable_label(self.settings.model == model.value, &model.label)
                                .clicked()
                            {
                                self.settings.model = model.value.clone();
                                changed = true;
                            }
                        }
                    });
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.settings.model)
                            .hint_text("or a version, e.g. claude-sonnet-5-5")
                            .desired_width(260.0),
                    )
                    .changed()
                {
                    changed = true;
                }
                ui.label(
                    egui::RichText::new("Empty = whatever `claude` is configured to use.")
                        .weak()
                        .small(),
                );
                ui.add_space(10.0);

                ui.strong("Thinking");
                egui::ComboBox::from_id_salt("settings_effort")
                    .selected_text(self.settings.effort.label())
                    .show_ui(ui, |ui| {
                        for effort in Effort::ALL {
                            if ui
                                .selectable_value(&mut self.settings.effort, effort, effort.label())
                                .clicked()
                            {
                                changed = true;
                            }
                        }
                    });
                ui.add_space(10.0);

                ui.strong("Permissions");
                for profile in [Profile::Safe, Profile::AcceptEdits, Profile::FullAuto] {
                    if ui
                        .selectable_value(&mut self.settings.profile, profile, profile.label())
                        .on_hover_text(profile.tooltip())
                        .clicked()
                    {
                        changed = true;
                    }
                }

                if self.current.is_some() {
                    ui.separator();
                    ui.strong("This conversation");
                    ui.label(
                        egui::RichText::new(
                            "Overrides the global defaults above, for this conversation only.",
                        )
                        .weak()
                        .small(),
                    );
                    ui.add_space(6.0);
                    overrides_changed = self.override_controls(ui);
                }
            });

        self.show_settings = open;
        if changed {
            self.save_settings();
        }
        if overrides_changed && let Some(id) = self.current.clone() {
            self.transport.send(Command::SetOverrides {
                id,
                overrides: self.overrides.clone(),
            });
        }
    }

    /// Per-conversation override combos (each can be "Use global"). Returns true
    /// if an override changed.
    fn override_controls(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;

        // Model
        let model_text = match &self.overrides.model {
            None => format!("Use global ({})", model_label(&self.settings.model)),
            Some(m) => model_label(m),
        };
        ui.label("Model");
        egui::ComboBox::from_id_salt("ov_model")
            .selected_text(model_text)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(self.overrides.model.is_none(), "Use global")
                    .clicked()
                {
                    self.overrides.model = None;
                    changed = true;
                }
                for model in &self.models {
                    if ui
                        .selectable_label(
                            self.overrides.model.as_deref() == Some(model.value.as_str()),
                            &model.label,
                        )
                        .clicked()
                    {
                        self.overrides.model = Some(model.value.clone());
                        changed = true;
                    }
                }
            });

        // Thinking
        let effort_text = match self.overrides.effort {
            None => format!("Use global ({})", self.settings.effort.label()),
            Some(e) => e.label().to_string(),
        };
        ui.label("Thinking");
        egui::ComboBox::from_id_salt("ov_effort")
            .selected_text(effort_text)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(self.overrides.effort.is_none(), "Use global")
                    .clicked()
                {
                    self.overrides.effort = None;
                    changed = true;
                }
                for effort in Effort::ALL {
                    if ui
                        .selectable_label(self.overrides.effort == Some(effort), effort.label())
                        .clicked()
                    {
                        self.overrides.effort = Some(effort);
                        changed = true;
                    }
                }
            });

        // Permissions
        let profile_text = match self.overrides.profile {
            None => format!("Use global ({})", self.settings.profile.label()),
            Some(p) => p.label().to_string(),
        };
        ui.label("Permissions");
        egui::ComboBox::from_id_salt("ov_profile")
            .selected_text(profile_text)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(self.overrides.profile.is_none(), "Use global")
                    .clicked()
                {
                    self.overrides.profile = None;
                    changed = true;
                }
                for profile in [Profile::Safe, Profile::AcceptEdits, Profile::FullAuto] {
                    if ui
                        .selectable_label(self.overrides.profile == Some(profile), profile.label())
                        .on_hover_text(profile.tooltip())
                        .clicked()
                    {
                        self.overrides.profile = Some(profile);
                        changed = true;
                    }
                }
            });

        if !self.overrides.is_empty() {
            ui.add_space(4.0);
            if ui.button("Reset to global").clicked() {
                self.overrides = Overrides::default();
                changed = true;
            }
        }

        changed
    }

    fn composer(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("composer")
            .resizable(false)
            .show(ui, |ui| {
                ui.add_space(6.0);
                let streaming = self.streaming;

                let mut answer = None;
                for (request_id, call, scope) in &self.approvals {
                    ui.horizontal(|ui| {
                        ui.strong("Claude wants to run:");
                        ui.monospace(&call.summary);
                        if ui.button("Allow once").clicked() {
                            answer = Some((request_id.clone(), Decision::AllowOnce));
                        }
                        if ui
                            .button("Allow always")
                            .on_hover_text(format!(
                                "Don't ask again for {scope} in this conversation"
                            ))
                            .clicked()
                        {
                            answer = Some((request_id.clone(), Decision::AllowAlways));
                        }
                        if ui.button("Deny").clicked() {
                            answer = Some((request_id.clone(), Decision::Deny));
                        }
                    });
                }
                if let (Some((request_id, decision)), Some(conversation_id)) =
                    (answer, self.current.clone())
                {
                    self.approvals.retain(|(id, ..)| *id != request_id);
                    self.transport.send(Command::AnswerApproval {
                        conversation_id,
                        request_id,
                        decision,
                    });
                }

                // Compact readout of the effective settings for this turn.
                ui.horizontal(|ui| {
                    if ui.button("⚙ Settings").clicked() {
                        self.show_settings = true;
                    }
                    ui.separator();
                    let (model, effort, profile) = self.effective();
                    let model = if model.is_empty() {
                        "default model".to_string()
                    } else {
                        model
                    };
                    ui.weak(format!(
                        "{} · {} · {}",
                        model,
                        effort.label(),
                        profile.label()
                    ));
                    if !self.overrides.is_empty() {
                        ui.label(egui::RichText::new("(override)").small().color(AMBER))
                            .on_hover_text("This conversation overrides some global settings.");
                    }
                });

                ui.horizontal(|ui| {
                    let hint = if streaming {
                        "Waiting for Claude..."
                    } else {
                        "Message Claude   (Enter to send, Shift+Enter for newline)"
                    };
                    // Shift+Enter is the multiline newline; plain Enter falls
                    // through unhandled by the editor so we can treat it as send.
                    let editor = egui::TextEdit::multiline(&mut self.input)
                        .desired_rows(2)
                        .hint_text(hint)
                        .desired_width(f32::INFINITY)
                        .return_key(egui::KeyboardShortcut::new(
                            egui::Modifiers::SHIFT,
                            egui::Key::Enter,
                        ));
                    let resp = ui.add_enabled(!streaming, editor);
                    let submit = resp.has_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
                    if submit {
                        self.send();
                    }
                });

                let mut detached = None;
                ui.horizontal_wrapped(|ui| {
                    for (index, file) in self.attachments.iter().enumerate() {
                        ui.weak(format!("📎 {}", file.name));
                        if ui.small_button("✕").clicked() {
                            detached = Some(index);
                        }
                    }
                });
                if let Some(index) = detached {
                    self.attachments.remove(index);
                }

                ui.horizontal(|ui| {
                    let can_send = !streaming
                        && !(self.input.trim().is_empty() && self.attachments.is_empty());
                    // rfd's blocking dialog does not exist in the browser.
                    #[cfg(not(target_arch = "wasm32"))]
                    if ui.button("📎 Attach…").clicked()
                        && let Some(paths) = rfd::FileDialog::new().pick_files()
                    {
                        for path in paths {
                            let name = path
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let file = std::fs::read(&path)
                                .map_err(|e| format!("cannot read {name}: {e}"))
                                .and_then(|bytes| Attachment::file(name, &bytes));
                            match file {
                                Ok(file) => self.attachments.push(file),
                                Err(error) => self.error = Some(error),
                            }
                        }
                    }
                    if ui
                        .add_enabled(can_send, egui::Button::new("Send"))
                        .clicked()
                    {
                        self.send();
                    }
                    if streaming {
                        ui.spinner();
                        ui.label("streaming...");
                        if ui.button("Stop").clicked()
                            && let Some(conversation_id) = self.current.clone()
                        {
                            self.transport.send(Command::Stop { conversation_id });
                        }
                    }
                });
                ui.add_space(6.0);
            });
    }

    fn transcript(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if self.items.is_empty() {
                        ui.add_space(16.0);
                        ui.weak(
                            "Start a conversation. This runs your Claude Code CLI under the hood.",
                        );
                    }
                    // Clone each item so the transcript borrow ends before the
                    // markdown viewer needs `&mut self.md_cache`.
                    for idx in 0..self.items.len() {
                        let item = self.items[idx].clone();
                        ui.add_space(8.0);
                        match item {
                            Item::User(text) => {
                                ui.strong("You");
                                ui.label(text);
                                ui.separator();
                            }
                            Item::Incoming { from, text } => {
                                ui.strong(format!("From {from}"));
                                ui.label(text);
                                ui.separator();
                            }
                            Item::Assistant(text) => {
                                ui.strong("Claude");
                                if Some(idx) == self.assistant_idx {
                                    // Streaming: fade words in at the leading
                                    // edge. Markdown is applied once complete.
                                    if self.reveal < 1.0 {
                                        ui.weak("...");
                                    } else {
                                        faded_text(ui, &text, self.reveal);
                                    }
                                } else if text.is_empty() {
                                    ui.weak("...");
                                } else {
                                    CommonMarkViewer::new().show(ui, &mut self.md_cache, &text);
                                }
                                ui.separator();
                            }
                            Item::Review(review) => {
                                ui.strong("Claude");
                                review_card(ui, &mut self.md_cache, idx, &review);
                                ui.separator();
                            }
                            Item::Thinking(text) => {
                                egui::CollapsingHeader::new(egui::RichText::new("Thinking").weak())
                                    .id_salt(("thinking", idx))
                                    .show(ui, |ui| {
                                        ui.label(egui::RichText::new(text).weak());
                                    });
                            }
                            Item::Tool(tc) => tool_card(ui, &tc),
                            Item::Notice(text) => {
                                ui.weak(text);
                            }
                            Item::Compacted(c) => {
                                ui.weak(format!(
                                    "Conversation compacted: {} → {} tokens",
                                    c.before, c.after
                                ));
                            }
                            Item::File(file) => {
                                ui.weak(format!("📎 {}", file.name));
                            }
                            Item::Ended(time) => {
                                let seconds = time.ended.saturating_sub(time.started);
                                ui.weak(format!("Took {} min {} s", seconds / 60, seconds % 60));
                            }
                        }
                    }
                    if let Some(err) = &self.error {
                        ui.add_space(8.0);
                        ui.colored_label(RED, err);
                    }
                });
        });
    }

    fn not_ready_view(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("claudo");
            ui.separator();
            let installed = self
                .status
                .as_ref()
                .map(|s| s.claude_installed)
                .unwrap_or(false);
            if !installed {
                ui.colored_label(RED, "Claude Code CLI not found on PATH.");
                ui.label("Install it and make sure `claude` is on your PATH, then re-check.");
            } else {
                ui.colored_label(AMBER, "CLI found, but not logged in.");
                ui.label("Run `claude auth login` in a terminal, then re-check.");
            }
            ui.add_space(8.0);
            if ui.button("Re-check").clicked() {
                self.transport.send(Command::GetStatus);
            }
        });
    }
}

/// egui only bundles Latin glyphs, so Japanese renders as blank boxes. The font
/// is embedded in the binary rather than read from the system, so it works
/// identically on any machine and in the browser (no fontconfig, no download).
/// BIZ UDPGothic, SIL OFL: see assets/fonts/OFL.txt.
const JP_FONT: &[u8] = include_bytes!("../../../../assets/fonts/BIZUDPGothic-Regular.ttf");

fn install_jp_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "jp".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(JP_FONT)),
    );
    // Fallback (appended, not prepended) so Latin keeps egui's default face.
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("jp".to_owned());
    }
    ctx.set_fonts(fonts);
}

/// Chars over which a word ramps from invisible to fully opaque.
const FADE_CHARS: f32 = 55.0;

/// Render streaming text with a soft leading edge: each word fades in as the
/// reveal cursor passes it, instead of snapping into place. Plain text only,
/// the finished message is re-rendered as markdown once the turn completes.
fn faded_text(ui: &mut egui::Ui, text: &str, reveal: f32) {
    let base = ui.visuals().text_color();
    let mut pos = 0usize; // chars consumed so far
    ui.vertical(|ui| {
        for line in text.split('\n') {
            let line_chars = line.chars().count();
            if pos as f32 >= reveal {
                break;
            }
            if line.trim().is_empty() {
                ui.add_space(6.0);
                pos += line_chars + 1;
                continue;
            }

            // Split into fade units up front so the closure can borrow freely.
            // Long runs are chunked so scripts without spaces (CJK) still fade
            // gradually rather than as one block.
            let mut units: Vec<(String, usize)> = Vec::new();
            let mut at = pos;
            for word in line.split_inclusive(' ') {
                let chars: Vec<char> = word.chars().collect();
                for chunk in chars.chunks(6) {
                    let s: String = chunk.iter().collect();
                    let n = s.chars().count();
                    units.push((s, at));
                    at += n;
                }
            }

            ui.horizontal_wrapped(|ui| {
                // Units keep their own trailing spaces, so no extra gap.
                ui.spacing_mut().item_spacing.x = 0.0;
                for (unit, start) in &units {
                    if *start as f32 >= reveal {
                        break;
                    }
                    let alpha = ((reveal - *start as f32) / FADE_CHARS).clamp(0.0, 1.0);
                    ui.label(egui::RichText::new(unit).color(base.gamma_multiply(alpha)));
                }
            });
            pos += line_chars + 1; // +1 for the newline
        }
    });
}

/// "12.3k / 1.0M ctx (1%)" for the header, or None before the first turn.
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

fn model_label(model: &str) -> String {
    if model.is_empty() {
        "default model".to_string()
    } else {
        model.to_string()
    }
}

const RED: egui::Color32 = egui::Color32::from_rgb(0xd0, 0x4a, 0x4a);
const AMBER: egui::Color32 = egui::Color32::from_rgb(0xe0, 0x9a, 0x2a);
const GREEN: egui::Color32 = egui::Color32::from_rgb(0x3f, 0xb9, 0x50);

fn review_card(ui: &mut egui::Ui, cache: &mut CommonMarkCache, idx: usize, review: &Review) {
    if !review.preface.is_empty() {
        CommonMarkViewer::new().show(ui, cache, &review.preface);
    }
    let count = review.findings.len();
    ui.weak(match count {
        1 => "1 finding".to_string(),
        n => format!("{n} findings, most severe first"),
    });
    for (n, finding) in review.findings.iter().enumerate() {
        ui.add_space(6.0);
        let place = match &finding.line {
            Some(line) => format!("{}:{line}", finding.file),
            None => finding.file.clone(),
        };
        ui.horizontal(|ui| {
            ui.strong(format!("{}.", n + 1));
            ui.label(egui::RichText::new(place).monospace().weak());
        });
        CommonMarkViewer::new().show(ui, cache, &finding.summary);
        if let Some(scenario) = &finding.failure_scenario {
            egui::CollapsingHeader::new(egui::RichText::new("Failure scenario").weak())
                .id_salt(("finding", idx, n))
                .show(ui, |ui| {
                    CommonMarkViewer::new().show(ui, cache, scenario);
                });
        }
    }
    if !review.afterword.is_empty() {
        ui.add_space(6.0);
        CommonMarkViewer::new().show(ui, cache, &review.afterword);
    }
}

fn tool_card(ui: &mut egui::Ui, tc: &ToolCall) {
    let color = match (&tc.result, tc.is_error) {
        (None, _) => AMBER,        // running
        (Some(_), false) => GREEN, // ok
        (Some(_), true) => RED,    // error / denied
    };
    ui.horizontal(|ui| {
        // Painted dot instead of a font glyph, so it renders on any system.
        let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, color);

        ui.vertical(|ui| {
            let header = egui::RichText::new(&tc.name).color(color);
            egui::CollapsingHeader::new(header)
                .id_salt(&tc.call_id)
                .show(ui, |ui| {
                    if !tc.input.is_empty() {
                        ui.label(egui::RichText::new(&tc.input).monospace().small());
                    }
                    if let Some(result) = &tc.result {
                        ui.separator();
                        let c = if tc.is_error {
                            RED
                        } else {
                            ui.visuals().text_color()
                        };
                        ui.label(egui::RichText::new(result).monospace().small().color(c));
                    }
                });
        });
    });
    ui.separator();
}
