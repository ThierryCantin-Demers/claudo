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
    let ui = app::UiState::load();
    let mut settings = cosmic::app::Settings::default();
    if let Some((width, height)) = ui.window {
        settings = settings.size(cosmic::iced::Size::new(width, height));
    }
    cosmic::app::run::<app::ClaudoApp>(settings, ui)
}

#[cfg(feature = "cosmic")]
mod app {
    use claudo_protocol::{
        Command, ConversationId, ConversationInfo, Decision, Effort, Item, ModelOption, Overrides,
        Profile, ServerEvent, Settings, SystemStatus, ToolCall,
    };
    use claudo_transport::{Commands, spawn_backend};

    use cosmic::app::{Core, Task};
    use std::collections::HashMap;
    use std::hash::Hash;

    use cosmic::cosmic_config::{Config, ConfigGet, ConfigSet};
    use serde::{Deserialize, Serialize};

    use cosmic::iced::advanced::subscription::{EventStream, Hasher, Recipe, from_recipe};
    use cosmic::iced::advanced::text::IntoFragment;
    use cosmic::iced::advanced::text::{Ellipsize, EllipsizeHeightLimit};
    use cosmic::iced::advanced::widget::{Operation, Tree, tree};
    use cosmic::iced::advanced::{Clipboard, Layout, Shell, Widget, layout, overlay, renderer};
    use cosmic::iced::futures::stream::BoxStream;
    use cosmic::iced::futures::{SinkExt, Stream, StreamExt};
    use cosmic::iced::widget::{container, text};
    use cosmic::iced::{
        Alignment, Border, Color, Length, Padding, Point, Subscription, keyboard, stream,
    };
    use cosmic::widget::{self, markdown, menu, text_editor};
    use cosmic::{Application, Element};
    use selectable::{Collect, Mark, Selectable, Selection};

    const PROFILES: [Profile; 3] = [Profile::Safe, Profile::AcceptEdits, Profile::FullAuto];
    const COLUMN_WIDTH: f32 = 760.0;
    const BUBBLE_WIDTH: f32 = 560.0;
    const TOOL_OUTPUT_LINES: usize = 40;
    const PREVIEW_CHARS: usize = 100;
    const SIDEBAR_WIDTH: f32 = 260.0;
    const SIDEBAR_MIN: f32 = 180.0;
    const SIDEBAR_MAX: f32 = 480.0;
    /// Dragged narrower than this, the sidebar collapses instead of stopping at its minimum.
    const SIDEBAR_COLLAPSE: f32 = 120.0;
    /// Tiles stop shrinking here and the row scrolls sideways instead.
    const TILE_MIN: f32 = 360.0;
    /// Narrower tiles give the message box's menus short labels and less padding: a
    /// squeezed dropdown wraps its label, and libcosmic's has no way to stop it.
    const COMPACT_TILE: f32 = 600.0;
    const HANDLE_WIDTH: f32 = 8.0;
    const COMMANDS_WIDTH: f32 = 420.0;
    const COMMANDS_MIN: f32 = 280.0;
    /// Dragging the Commands panel narrower than this closes it.
    const COMMANDS_COLLAPSE: f32 = 160.0;
    const PULSE_TICK: std::time::Duration = std::time::Duration::from_millis(50);

    /// A transcript row. Replies keep their parsed markdown so a frame only
    /// parses the text revealed since the last one.
    enum Entry {
        User(String),
        /// A message another agent sent.
        Incoming {
            from: String,
            text: String,
        },
        /// Why a turn Claude Code began by itself started.
        Notice(String),
        Thinking {
            text: String,
            open: bool,
        },
        Reply(Box<Reply>),
        Tool {
            call: ToolCall,
            open: bool,
        },
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
                Item::Incoming { from, text } => Entry::Incoming { from, text },
                Item::Notice(text) => Entry::Notice(text),
                Item::Assistant(text) => Entry::Reply(Box::new(Reply::complete(text))),
                Item::Thinking(text) => Entry::Thinking { text, open: false },
                Item::Tool(call) => Entry::Tool { call, open: false },
            }
        }
    }

    struct Approval {
        request_id: String,
        call: ToolCall,
        /// What allowing always covers, such as `gh issue create`.
        scope: String,
    }

    /// Everything shown for one conversation, whichever tile shows it. It outlives its
    /// tile while a turn runs, so a background agent's progress is there on return.
    struct ConversationView {
        items: Vec<Entry>,
        draft: text_editor::Content,
        /// Calls waiting for the user's answer.
        approvals: Vec<Approval>,
        streaming: bool,
        assistant_idx: Option<usize>,
        /// Chars of the streaming message revealed so far. The CLI delivers text
        /// in large chunks; revealing gradually reads as smooth typing.
        reveal: f32,
        error: Option<String>,
        /// Made for a message another agent sent, so it holds only that turn so far.
        partial: bool,
        /// How many prompts back the draft was recalled from; 0 when it is the user's own.
        recalled: usize,
        /// The user's own draft while a recalled prompt replaces it.
        stash: String,
        /// Items whose row in the Commands panel is expanded.
        commands_open: std::collections::HashSet<usize>,
        /// The Commands panel shows only this conversation's title.
        commands_collapsed: bool,
    }

    impl ConversationView {
        fn new(draft: &str) -> Self {
            Self {
                items: Vec::new(),
                draft: text_editor::Content::with_text(draft),
                approvals: Vec::new(),
                streaming: false,
                assistant_idx: None,
                reveal: 0.0,
                error: None,
                partial: false,
                recalled: 0,
                stash: String::new(),
                commands_open: Default::default(),
                commands_collapsed: false,
            }
        }

        /// The prompts sent here, newest first, each repeat once.
        fn prompts(&self) -> Vec<&str> {
            let mut prompts: Vec<&str> = self
                .items
                .iter()
                .rev()
                .filter_map(|entry| match entry {
                    Entry::User(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            prompts.dedup();
            prompts
        }

        /// Replace the draft with the prompt before or after the one shown, back to the
        /// user's own draft past the newest.
        fn recall(&mut self, older: bool) {
            let back = if older {
                self.recalled + 1
            } else {
                self.recalled.saturating_sub(1)
            };
            if back == self.recalled || back > self.prompts().len() {
                return;
            }
            if self.recalled == 0 {
                self.stash = self.draft.text();
            }
            let text = match back {
                0 => std::mem::take(&mut self.stash),
                _ => self.prompts()[back - 1].to_string(),
            };
            self.recalled = back;
            self.draft = text_editor::Content::with_text(&text);
            self.draft
                .perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
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

        fn fail(&mut self, detail: String) {
            self.error = Some(detail);
            self.streaming = false;
            self.approvals.clear();
            self.finish_reply();
        }

        fn apply(&mut self, ev: ServerEvent) {
            match ev {
                ServerEvent::History { items, .. } => {
                    self.items = items.into_iter().map(Entry::from).collect();
                    self.assistant_idx = None;
                    self.commands_open.clear();
                }
                ServerEvent::Background {
                    call_id,
                    background,
                    ..
                } => {
                    if let Some(Entry::Tool { call, .. }) = self.items.iter_mut().rev().find(
                        |entry| matches!(entry, Entry::Tool { call, .. } if call.call_id == call_id),
                    ) {
                        call.background = Some(background);
                    }
                }
                ServerEvent::Incoming { from, text, .. } => {
                    self.finish_reply();
                    self.error = None;
                    self.streaming = true;
                    self.items.push(Entry::Incoming { from, text });
                }
                ServerEvent::Notice { text, .. } => {
                    self.finish_reply();
                    self.error = None;
                    self.streaming = true;
                    self.items.push(Entry::Notice(text));
                }
                ServerEvent::Delta { text, .. } => self.push_assistant(&text),
                ServerEvent::ThinkingDelta { text, .. } => {
                    if let Some(Entry::Thinking { text: thought, .. }) = self.items.last_mut() {
                        thought.push_str(&text);
                    } else {
                        self.finish_reply();
                        self.items.push(Entry::Thinking { text, open: false });
                    }
                }
                ServerEvent::ToolStarted { call, .. } => {
                    self.finish_reply();
                    self.items.push(Entry::Tool { call, open: false });
                }
                ServerEvent::ToolResult {
                    call_id,
                    content,
                    is_error,
                    ..
                } => {
                    if let Some(Entry::Tool { call, .. }) = self.items.iter_mut().rev().find(
                        |entry| matches!(entry, Entry::Tool { call, .. } if call.call_id == call_id),
                    ) {
                        call.result = Some(content);
                        call.is_error = is_error;
                    }
                }
                ServerEvent::ApprovalRequested {
                    request_id,
                    call,
                    scope,
                    ..
                } => self.approvals.push(Approval {
                    request_id,
                    call,
                    scope,
                }),
                ServerEvent::Completed { is_error, .. } => {
                    self.streaming = false;
                    self.approvals.clear();
                    self.finish_reply();
                    if is_error {
                        self.error = Some("Claude reported an error.".to_string());
                    }
                }
                ServerEvent::Error { detail, .. } => self.fail(detail),
                _ => {}
            }
        }
    }

    /// One window's arrangement: the conversations it shows, left to right. Their
    /// content lives in `ClaudoApp::views`, so a tile only names its conversation.
    struct WindowLayout {
        tiles: Vec<Tile>,
        focused: usize,
        sidebar_open: bool,
        sidebar_width: f32,
        commands_open: bool,
        commands_width: f32,
        size: Option<(f32, f32)>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Tile {
        conversation: ConversationId,
        /// Its part of the row's width, relative to the other tiles' shares.
        #[serde(default = "one")]
        share: f32,
    }

    impl Tile {
        fn new(conversation: ConversationId) -> Self {
            Self {
                conversation,
                share: 1.0,
            }
        }
    }

    fn one() -> f32 {
        1.0
    }

    /// A drag in progress. Positions are window x coordinates; `origin` is where the
    /// cursor was at the drag's first move, since the press itself reports none.
    enum Drag {
        /// The sidebar's edge; `offset` is the cursor's distance from it.
        Sidebar { offset: Option<f32> },
        /// The Commands panel's edge; `offset` is the cursor's distance from it.
        Commands { offset: Option<f32> },
        /// The handle between tile `left` and the next, from their shares at the start.
        Divider {
            left: usize,
            origin: Option<f32>,
            start: (f32, f32),
        },
        /// A tile picked up by its title, or with Ctrl anywhere in it. Positions are in the
        /// tile row; `at` is set once the cursor has moved far enough to lift the tile, and
        /// `slot` is where it would land among the others.
        Tile {
            from: usize,
            origin: Option<Point>,
            at: Option<Point>,
            slot: usize,
        },
    }

    /// A conversation's title being edited, the text so far, and where its field is.
    #[derive(Debug, Clone, PartialEq)]
    struct Renaming {
        conversation: ConversationId,
        text: String,
        at: TitleAt,
    }

    /// Where a title is edited: its tile's header (or the window's, with one tile), or its
    /// row in the sidebar.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum TitleAt {
        Header,
        Sidebar,
    }

    /// How a tile is drawn.
    #[derive(Debug, Clone, Copy)]
    struct TileView {
        /// Beside other tiles, so it names its conversation in a header.
        several: bool,
        focused: bool,
        /// Narrower than `COMPACT_TILE`.
        compact: bool,
        /// Being dragged; it shows where it would land.
        lifted: bool,
        /// Its width, as far as the window's size tells.
        pixels: f32,
    }

    /// Where an opened or created conversation goes.
    #[derive(Debug, Clone, Copy)]
    enum Placement {
        /// Into the focused tile, in place of its conversation.
        Focused,
        /// Into a new tile at the right end.
        Right,
    }

    pub struct ClaudoApp {
        core: Core,
        commands: Option<Commands>,
        status: Option<SystemStatus>,
        settings: Settings,
        /// The CLI's model families, with the versions their aliases run today.
        models: Vec<ModelOption>,
        conversations: Vec<ConversationInfo>,
        /// A conversation appears in at most one tile, so one view each is enough.
        views: HashMap<ConversationId, ConversationView>,
        layout: WindowLayout,
        /// Creation requests not yet answered by `Created`, and where each one goes.
        pending: HashMap<u64, Placement>,
        next_request: u64,
        /// Seconds of the activity indicators' animation.
        pulse: f32,
        /// Text scale. libcosmic exposes no runtime window scale factor, so zoom
        /// is applied to the conversation text rather than the whole window.
        zoom: f32,
        page: Page,
        modifiers: keyboard::Modifiers,
        drag: Option<Drag>,
        /// Text selected in a transcript; one at a time, as in a browser.
        selection: Option<Selection>,
        renaming: Option<Renaming>,
        /// The conversation the delete confirmation is asking about.
        deleting: Option<ConversationId>,
        /// The sidebar lists archived conversations too.
        show_archived: bool,
        /// Unsent text of conversations that have no view.
        drafts: HashMap<ConversationId, String>,
        /// The tiles to reopen, with the focused one, once the conversation list arrives.
        restore: Option<(Vec<Tile>, usize)>,
        /// The conversation whose composer takes focus after this update.
        focus_composer: Option<ConversationId>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Page {
        Chat,
        Settings,
    }

    /// Where the window was left, restored on the next launch.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(default)]
    pub struct UiState {
        sidebar_open: bool,
        sidebar_width: f32,
        commands_open: bool,
        commands_width: f32,
        page: Page,
        tiles: Vec<Tile>,
        focused: usize,
        zoom: f32,
        pub window: Option<(f32, f32)>,
        show_archived: bool,
        /// Unsent text, per conversation.
        drafts: HashMap<ConversationId, String>,
    }

    impl Default for UiState {
        fn default() -> Self {
            Self {
                sidebar_open: true,
                sidebar_width: SIDEBAR_WIDTH,
                commands_open: false,
                commands_width: COMMANDS_WIDTH,
                page: Page::Chat,
                tiles: Vec::new(),
                focused: 0,
                zoom: 1.0,
                window: None,
                show_archived: false,
                drafts: HashMap::new(),
            }
        }
    }

    const UI_STATE: &str = "ui";

    impl UiState {
        pub fn load() -> Self {
            Self::store()
                .and_then(|store| store.get(UI_STATE).ok())
                .unwrap_or_default()
        }

        fn save(&self) {
            if let Some(store) = Self::store() {
                let _ = store.set(UI_STATE, self);
            }
        }

        fn store() -> Option<Config> {
            Config::new_state(ClaudoApp::APP_ID, 1).ok()
        }
    }

    #[derive(Debug, Clone)]
    pub enum Message {
        Connected(Commands),
        Events(Vec<ServerEvent>),
        /// Animation frame while text is being revealed.
        Frame,
        /// Animation frame for the activity indicators.
        Pulse,
        /// Multiply the text scale (Ctrl+scroll, Ctrl+`+`/`-`).
        Zoom(f32),
        /// Reset text scale (Ctrl+0).
        ZoomReset,
        Modifiers(keyboard::Modifiers),
        Edit(ConversationId, text_editor::Action),
        Send(ConversationId),
        Stop(ConversationId),
        Link(markdown::Uri),
        /// Expand or collapse a thinking or tool row.
        Toggle(ConversationId, usize),
        /// Open or close the Commands panel.
        ShowCommands(bool),
        StartCommandsDrag,
        /// Expand or collapse a command in the Commands panel.
        ToggleCommand(ConversationId, usize),
        /// Fold or unfold a conversation's commands in the Commands panel.
        ToggleCommandGroup(ConversationId),
        /// Up or Down at the message box's edge: an earlier or later prompt.
        Recall {
            conversation: ConversationId,
            older: bool,
        },
        /// A press in a transcript's text, starting a selection there.
        SelectStart(ConversationId, Mark),
        /// The cursor dragged the selection's end here.
        SelectTo(Mark),
        /// A double or triple click selected a word or a block.
        SelectRange(ConversationId, Mark, Mark),
        Copy,
        /// A click on a conversation's title, or Rename in its sidebar menu, to edit it.
        StartRename(ConversationId, TitleAt),
        RenameText(String),
        /// Enter, or a click elsewhere, keeps the edited title.
        CommitRename,
        /// Escape leaves the title as it was.
        CancelRename,
        /// Put a conversation away, or bring it back.
        Archive(ConversationId, bool),
        /// Show or hide the archived conversations in the sidebar.
        ToggleArchived,
        /// Delete in a conversation's sidebar menu; asks first.
        AskDelete(ConversationId),
        ConfirmDelete,
        CancelDelete,
        /// The selected text, for the clipboard; `primary` for the one middle click pastes.
        Copied {
            text: String,
            primary: bool,
        },
        /// A hot patch landed. Handling any message rebuilds and redraws the view,
        /// which iced's own reload leaves until the next input event.
        #[cfg(feature = "hot")]
        Hotpatched,
        /// A click on a conversation in the sidebar; with Shift it gets a new tile.
        Open(ConversationId),
        OpenTile(ConversationId),
        Focus(ConversationId),
        Close(ConversationId),
        CloseFocused,
        Answer {
            conversation_id: ConversationId,
            request_id: String,
            decision: Decision,
        },
        SetPage(Page),
        ToggleSidebar,
        StartSidebarDrag,
        StartResize(usize),
        StartMove(ConversationId),
        DragTo(f32),
        /// The cursor in the tile row while a tile is dragged.
        DragTile(Point),
        DragEnd,
        /// Move a tile this many places, negative to the left.
        Move(ConversationId, i32),
        MoveFocused(i32),
        FocusStep(i32),
        Resized(cosmic::iced::window::Id, cosmic::iced::Size),
        /// Opens and closes popups, such as context menus.
        Surface(cosmic::surface::Action),
        /// The main window is closing.
        Quit,
        /// "New chat"; with Shift it gets a new tile.
        New,
        SetModel(usize),
        ModelText(String),
        SetEffort(usize),
        SetProfile(usize),
        OvModel(ConversationId, usize),
        OvEffort(ConversationId, usize),
        OvProfile(ConversationId, usize),
    }

    impl ClaudoApp {
        fn cmd(&self, command: Command) {
            if let Some(c) = &self.commands {
                c.send(command);
            }
        }

        fn info(&self, id: &str) -> Option<&ConversationInfo> {
            self.conversations.iter().find(|c| c.id == id)
        }

        fn tile_of(&self, id: &str) -> Option<usize> {
            self.layout.tiles.iter().position(|t| t.conversation == id)
        }

        fn focused(&self) -> Option<&ConversationId> {
            self.layout
                .tiles
                .get(self.layout.focused)
                .map(|t| &t.conversation)
        }

        fn save_settings(&self) {
            self.cmd(Command::UpdateSettings {
                settings: self.settings.clone(),
            });
        }

        fn set_override(&mut self, id: ConversationId, change: impl FnOnce(&mut Overrides)) {
            let Some(info) = self.conversations.iter_mut().find(|c| c.id == id) else {
                return;
            };
            change(&mut info.overrides);
            let overrides = info.overrides.clone();
            self.cmd(Command::SetOverrides { id, overrides });
        }

        fn effective(&self, id: &str) -> (String, Effort, Profile) {
            let overrides = self.info(id).map(|c| &c.overrides);
            (
                overrides
                    .and_then(|o| o.model.clone())
                    .unwrap_or_else(|| self.settings.model.clone()),
                overrides
                    .and_then(|o| o.effort)
                    .unwrap_or(self.settings.effort),
                overrides
                    .and_then(|o| o.profile)
                    .unwrap_or(self.settings.profile),
            )
        }

        fn open(&mut self, id: ConversationId, placement: Placement) {
            if !self.views.contains_key(&id) {
                let draft = self.drafts.remove(&id).unwrap_or_default();
                self.views.insert(id.clone(), ConversationView::new(&draft));
                self.cmd(Command::OpenConversation { id: id.clone() });
            }
            self.place(id, placement);
        }

        /// Show a conversation that has a view; one already in a tile is focused there.
        fn place(&mut self, id: ConversationId, placement: Placement) {
            self.focus_composer = Some(id.clone());
            if let Some(i) = self.tile_of(&id) {
                self.layout.focused = i;
                return;
            }
            let focused = self.layout.focused;
            match placement {
                Placement::Focused if focused < self.layout.tiles.len() => {
                    let left = std::mem::replace(&mut self.layout.tiles[focused].conversation, id);
                    self.release(&left);
                }
                _ => {
                    self.layout.tiles.push(Tile::new(id));
                    self.layout.focused = self.layout.tiles.len() - 1;
                }
            }
            self.persist();
        }

        /// Drop a view no tile shows once its turn is over, keeping the unsent text.
        fn release(&mut self, id: &str) {
            if self.tile_of(id).is_some() || self.views.get(id).is_none_or(|v| v.streaming) {
                return;
            }
            if let Some(view) = self.views.remove(id) {
                let text = view.draft.text();
                if !text.trim().is_empty() {
                    self.drafts.insert(id.to_string(), text);
                }
            }
        }

        /// The last tile stays: a window always shows a conversation.
        fn close(&mut self, id: &str) {
            let Some(i) = self.tile_of(id) else {
                return;
            };
            if self.layout.tiles.len() == 1 {
                return;
            }
            self.layout.tiles.remove(i);
            if self.layout.focused > i {
                self.layout.focused -= 1;
            }
            self.layout.focused = self.layout.focused.min(self.layout.tiles.len() - 1);
            self.focus_composer = self.focused().cloned();
            self.release(id);
            self.persist();
        }

        fn focus(&mut self, id: &str) {
            if let Some(i) = self.tile_of(id) {
                self.layout.focused = i;
            }
        }

        /// Where the tiles start, and the width they share, or `None` once they
        /// would be narrower than `TILE_MIN` and the row scrolls instead.
        fn tile_room(&self) -> (f32, Option<f32>) {
            let start = if self.layout.sidebar_open {
                self.layout.sidebar_width + HANDLE_WIDTH
            } else {
                0.0
            };
            let count = self.layout.tiles.len();
            let room = self.layout.size.map_or(f32::INFINITY, |(w, _)| w)
                - start
                - self.commands_room()
                - count.saturating_sub(1) as f32 * HANDLE_WIDTH;
            (
                start,
                (room >= count as f32 * self.tile_min()).then_some(room),
            )
        }

        /// Zooming out never makes a tile narrower than its message box's controls.
        fn tile_min(&self) -> f32 {
            TILE_MIN * self.zoom.max(1.0)
        }

        fn total_share(&self) -> f32 {
            self.layout.tiles.iter().map(|t| t.share).sum()
        }

        fn drag_to(&mut self, x: f32) {
            let room = self.tile_room().1;
            let total = self.total_share();
            let min = self.tile_min();
            match &mut self.drag {
                Some(Drag::Sidebar { offset }) => {
                    let layout = &mut self.layout;
                    let edge = if layout.sidebar_open {
                        layout.sidebar_width
                    } else {
                        0.0
                    };
                    let width = x - *offset.get_or_insert(x - edge);
                    layout.sidebar_open = width >= SIDEBAR_COLLAPSE;
                    if layout.sidebar_open {
                        layout.sidebar_width = width.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
                    }
                }
                Some(Drag::Commands { offset }) => {
                    let layout = &mut self.layout;
                    let Some((window, _)) = layout.size else {
                        return;
                    };
                    let edge = window - layout.commands_width;
                    let width = window - (x - *offset.get_or_insert(x - edge));
                    layout.commands_open = width >= COMMANDS_COLLAPSE;
                    if layout.commands_open {
                        layout.commands_width = width.clamp(COMMANDS_MIN, window * 0.6);
                    }
                }
                Some(Drag::Divider {
                    left,
                    origin,
                    start: (a, b),
                }) => {
                    let Some(room) = room.filter(|r| r.is_finite()) else {
                        return;
                    };
                    let per_px = total / room;
                    let moved = (x - *origin.get_or_insert(x)) * per_px;
                    let left_share = (*a + moved).min(*a + *b - min * per_px).max(min * per_px);
                    let tiles = &mut self.layout.tiles;
                    if *left + 1 < tiles.len() {
                        tiles[*left].share = left_share;
                        tiles[*left + 1].share = *a + *b - left_share;
                    }
                }
                Some(Drag::Tile { .. }) | None => {}
            }
        }

        fn drag_tile(&mut self, point: Point) {
            let Some(Drag::Tile { from, .. }) = self.drag else {
                return;
            };
            let centers = self.centers_without(from);
            let Some(Drag::Tile {
                origin, at, slot, ..
            }) = &mut self.drag
            else {
                return;
            };
            let origin = *origin.get_or_insert(point);
            if at.is_none() && origin.distance(point) < 8.0 {
                return;
            }
            *at = Some(point);
            *slot = centers.iter().filter(|c| **c < point.x).count();
        }

        /// The centers, from the tile row's left edge, of every tile but `skip` in a row
        /// without it.
        fn centers_without(&self, skip: usize) -> Vec<f32> {
            let (_, room) = self.tile_room();
            let total = self.total_share() - self.layout.tiles.get(skip).map_or(0.0, |t| t.share);
            let room = room.map(|room| room + HANDLE_WIDTH);
            let mut edge = 0.0;
            self.layout
                .tiles
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != skip)
                .map(|(_, t)| {
                    let width = room.map_or(self.tile_min(), |room| t.share / total * room);
                    let center = edge + width / 2.0;
                    edge += width + HANDLE_WIDTH;
                    center
                })
                .collect()
        }

        fn start_move(&mut self, from: usize) {
            let Some(id) = self.layout.tiles.get(from).map(|t| t.conversation.clone()) else {
                return;
            };
            if self.page != Page::Chat || self.layout.tiles.len() < 2 {
                return;
            }
            self.layout.focused = from;
            self.focus_composer = Some(id);
            self.drag = Some(Drag::Tile {
                from,
                origin: None,
                at: None,
                slot: from,
            });
        }

        /// Move the tile at `from` to `to`, focusing it there.
        fn move_tile(&mut self, from: usize, to: usize) {
            let tiles = &mut self.layout.tiles;
            if from >= tiles.len() {
                return;
            }
            let to = to.min(tiles.len() - 1);
            let tile = tiles.remove(from);
            tiles.insert(to, tile);
            self.layout.focused = to;
            self.persist();
        }

        fn new_conversation(&mut self, placement: Placement) {
            let request = self.next_request;
            self.next_request += 1;
            self.pending.insert(request, placement);
            self.cmd(Command::NewScratch { request });
        }

        fn restore_tiles(&mut self) {
            let Some((tiles, focused)) = self.restore.take() else {
                return;
            };
            for tile in tiles {
                if self.info(&tile.conversation).is_some() {
                    self.open(tile.conversation, Placement::Right);
                }
            }
            if self.layout.tiles.is_empty() {
                let first = self.conversations.iter().find(|c| !c.archived);
                match first.map(|c| c.id.clone()) {
                    Some(id) => self.open(id, Placement::Right),
                    None => self.new_conversation(Placement::Right),
                }
            }
            self.layout.focused = focused.min(self.layout.tiles.len().saturating_sub(1));
            self.focus_composer = self.focused().cloned();
        }

        /// Takes a conversation out of the window. A window keeps at least one tile, so the
        /// last one takes another conversation, or a new one.
        fn untile(&mut self, id: &str) {
            if self.tile_of(id).is_none() {
                return;
            }
            if self.layout.tiles.len() > 1 {
                self.close(id);
                return;
            }
            let next = self
                .conversations
                .iter()
                .find(|c| c.id != id && !c.archived)
                .map(|c| c.id.clone());
            match next {
                Some(next) => self.open(next, Placement::Focused),
                None => self.new_conversation(Placement::Focused),
            }
        }

        /// Drops a deleted conversation everywhere it shows.
        fn forget(&mut self, id: &str) {
            self.conversations.retain(|c| c.id != id);
            self.untile(id);
            self.views.remove(id);
            self.drafts.remove(id);
            if self
                .selection
                .as_ref()
                .is_some_and(|s| s.conversation == id)
            {
                self.selection = None;
            }
            if self.renaming.as_ref().is_some_and(|r| r.conversation == id) {
                self.renaming = None;
            }
            self.persist();
        }

        /// The confirmation before deleting, saying what goes and what stays.
        fn delete_dialog(&self, id: &str) -> Element<'_, Message> {
            let info = self.info(id);
            let title = info.map_or("this conversation", |c| c.title.as_str());
            let mut body = "Its messages are deleted from claudo and from Claude Code's history, \
                            along with the folder claudo made for it. This cannot be undone."
                .to_string();
            if info.is_some_and(|c| c.attached) {
                body = "Its messages are deleted from claudo and from Claude Code's history. \
                        Its project folder is not touched. This cannot be undone."
                    .to_string();
            }
            if self.views.get(id).is_some_and(|v| v.streaming) {
                body.push_str(" Its agent is still working and will be stopped.");
            }
            widget::dialog()
                .title(format!("Delete \u{201c}{title}\u{201d}?"))
                .body(body)
                .primary_action(
                    widget::button::destructive("Delete").on_press(Message::ConfirmDelete),
                )
                .secondary_action(
                    widget::button::standard("Cancel").on_press(Message::CancelDelete),
                )
                .into()
        }

        fn rename(&mut self, id: ConversationId, title: String) {
            let Some(info) = self.conversations.iter_mut().find(|c| c.id == id) else {
                return;
            };
            if title.is_empty() || info.title == title {
                return;
            }
            info.title = title.clone();
            self.cmd(Command::Rename { id, title });
        }

        /// The title being edited, in place of the conversation's name.
        fn rename_input(&self, width: Length) -> Element<'_, Message> {
            let text = self.renaming.as_ref().map_or("", |r| r.text.as_str());
            Intercept {
                on_escape: Some(Message::CancelRename),
                ..Intercept::new(
                    widget::text_input("Conversation title", text)
                        .id(rename_id())
                        .on_input(Message::RenameText)
                        .on_submit(|_| Message::CommitRename)
                        .on_unfocus(Message::CommitRename)
                        .width(width),
                )
            }
            .into()
        }

        /// A conversation's title, which a click turns into a text field.
        fn title_button<'a>(
            &'a self,
            id: &'a ConversationId,
            title: &'a str,
        ) -> widget::Button<'a, Message> {
            widget::button::custom(
                widget::text::heading(title)
                    .wrapping(text::Wrapping::None)
                    .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1))),
            )
            .class(title_style())
            .padding([2, 6])
            .on_press(Message::StartRename(id.clone(), TitleAt::Header))
        }

        fn renaming(&self, id: &str, at: TitleAt) -> bool {
            self.renaming
                .as_ref()
                .is_some_and(|r| r.conversation == id && r.at == at)
        }

        /// Copies the selected transcript text, gathered from the blocks on screen.
        fn collect_selection(&self, primary: bool) -> Task<Message> {
            if self.selection.as_ref().is_none_or(Selection::is_empty) {
                return Task::none();
            }
            cosmic::iced::advanced::widget::operate(Collect::default())
                .map(move |text| cosmic::Action::App(Message::Copied { text, primary }))
        }

        fn send(&mut self, id: ConversationId) {
            let (model, effort, profile) = self.effective(&id);
            let Some(view) = self.views.get_mut(&id) else {
                return;
            };
            let text = view.draft.text().trim().to_string();
            if text.is_empty() || view.streaming {
                return;
            }
            view.error = None;
            view.finish_reply();
            view.items.push(Entry::User(text.clone()));
            view.streaming = true;
            view.draft = text_editor::Content::new();
            view.recalled = 0;
            self.cmd(Command::SendMessage {
                conversation_id: id.clone(),
                text,
                profile,
                model,
                effort,
            });
            self.focus_composer = Some(id);
        }

        fn persist(&self) {
            let mut drafts = self.drafts.clone();
            for (id, view) in &self.views {
                let text = view.draft.text();
                if !text.trim().is_empty() {
                    drafts.insert(id.clone(), text);
                }
            }
            UiState {
                sidebar_open: self.layout.sidebar_open,
                sidebar_width: self.layout.sidebar_width,
                commands_open: self.layout.commands_open,
                commands_width: self.layout.commands_width,
                page: self.page,
                tiles: self.layout.tiles.clone(),
                focused: self.layout.focused,
                zoom: self.zoom,
                window: self.layout.size,
                show_archived: self.show_archived,
                drafts,
            }
            .save();
        }

        fn revealing(&self) -> bool {
            self.views.values().any(ConversationView::revealing)
        }

        fn streaming(&self) -> bool {
            self.views.values().any(|v| v.streaming)
        }

        fn apply(&mut self, ev: ServerEvent) {
            match ev {
                ServerEvent::Status(s) => self.status = Some(s),
                ServerEvent::Settings(s) => self.settings = s,
                ServerEvent::Models(models) => self.models = models,
                ServerEvent::Quota(_) => {}
                ServerEvent::Conversations { list } => {
                    self.conversations = list;
                    self.restore_tiles();
                }
                ServerEvent::Created {
                    conversation_id,
                    request,
                } => {
                    let placement = self.pending.remove(&request).unwrap_or(Placement::Focused);
                    self.views
                        .insert(conversation_id.clone(), ConversationView::new(""));
                    self.place(conversation_id, placement);
                }
                ServerEvent::Error {
                    conversation_id: None,
                    detail,
                } => {
                    if let Some(view) = self
                        .focused()
                        .cloned()
                        .and_then(|id| self.views.get_mut(&id))
                    {
                        view.fail(detail);
                    }
                }
                ServerEvent::History {
                    ref conversation_id,
                    ..
                }
                | ServerEvent::Started {
                    ref conversation_id,
                    ..
                }
                | ServerEvent::Incoming {
                    ref conversation_id,
                    ..
                }
                | ServerEvent::Notice {
                    ref conversation_id,
                    ..
                }
                | ServerEvent::Delta {
                    ref conversation_id,
                    ..
                }
                | ServerEvent::ThinkingDelta {
                    ref conversation_id,
                    ..
                }
                | ServerEvent::ToolStarted {
                    ref conversation_id,
                    ..
                }
                | ServerEvent::ToolResult {
                    ref conversation_id,
                    ..
                }
                | ServerEvent::ApprovalRequested {
                    ref conversation_id,
                    ..
                }
                | ServerEvent::Background {
                    ref conversation_id,
                    ..
                }
                | ServerEvent::Completed {
                    ref conversation_id,
                    ..
                }
                | ServerEvent::Error {
                    conversation_id: Some(ref conversation_id),
                    ..
                } => {
                    let id = conversation_id.clone();
                    let over = matches!(
                        ev,
                        ServerEvent::Completed { .. } | ServerEvent::Error { .. }
                    );
                    // A turn in a conversation no tile shows still gets a view, which
                    // the sidebar marks as working.
                    if matches!(
                        ev,
                        ServerEvent::Incoming { .. } | ServerEvent::Notice { .. }
                    ) && !self.views.contains_key(&id)
                    {
                        let draft = self.drafts.remove(&id).unwrap_or_default();
                        let mut view = ConversationView::new(&draft);
                        view.partial = true;
                        self.views.insert(id.clone(), view);
                    }
                    if let Some(view) = self.views.get_mut(&id) {
                        view.apply(ev);
                    }
                    if over {
                        let shown = self.tile_of(&id).is_some();
                        if let Some(view) = self.views.get_mut(&id)
                            && view.partial
                            && shown
                        {
                            view.partial = false;
                            self.cmd(Command::OpenConversation { id: id.clone() });
                        }
                        self.release(&id);
                    }
                }
            }
        }

        /// A model menu's entries, and which one `model` is.
        fn model_menu(&self, model: &str) -> (Vec<String>, Option<usize>) {
            (
                self.models.iter().map(|m| m.label.clone()).collect(),
                self.models.iter().position(|m| m.value == model),
            )
        }

        fn settings_page(&self) -> Element<'_, Message> {
            let (models, model) = self.model_menu(&self.settings.model);
            let efforts: Vec<&str> = Effort::ALL.iter().map(|e| e.label()).collect();
            let profiles: Vec<&str> = PROFILES.iter().map(|p| p.label()).collect();
            let defaults = widget::settings::section()
                .title("Defaults")
                .add(widget::settings::item(
                    "Model",
                    widget::dropdown(models, model, Message::SetModel)
                        .placeholder(model_name(&self.settings.model)),
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
                widget::text(
                    "Every conversation uses these until you change its model, thinking or \
                     permissions from its message box.",
                )
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
            let (archived, active): (Vec<&ConversationInfo>, Vec<&ConversationInfo>) =
                self.conversations.iter().partition(|c| c.archived);
            let mut list: Vec<Element<Message>> =
                active.iter().map(|c| self.conversation_row(c)).collect();
            if !archived.is_empty() {
                let chevron = if self.show_archived {
                    "go-down-symbolic"
                } else {
                    "go-next-symbolic"
                };
                list.push(
                    widget::button::text(format!("Archived ({})", archived.len()))
                        .leading_icon(widget::icon::from_name(chevron))
                        .on_press(Message::ToggleArchived)
                        .into(),
                );
                if self.show_archived {
                    list.extend(archived.iter().map(|c| self.conversation_row(c)));
                }
            }

            widget::container(
                widget::Column::with_children(vec![
                    widget::tooltip(
                        widget::button::text(if self.modifiers.shift() {
                            "New tile"
                        } else {
                            "New chat"
                        })
                        .leading_icon(widget::icon::from_name("list-add-symbolic"))
                        .on_press(Message::New),
                        widget::text("Shift+click for a new tile"),
                        widget::tooltip::Position::Bottom,
                    )
                    .into(),
                    widget::text("Recents")
                        .size(12.0 * self.zoom)
                        .class(dim())
                        .into(),
                    widget::scrollable(widget::Column::with_children(list).spacing(2))
                        .height(Length::Fill)
                        .into(),
                    widget::button::text("Settings")
                        .leading_icon(widget::icon::from_name("emblem-system-symbolic"))
                        .on_press(Message::SetPage(Page::Settings))
                        .into(),
                ])
                .spacing(8),
            )
            .width(Length::Fixed(self.layout.sidebar_width))
            .clip(true)
            .height(Length::Fill)
            .padding(8)
            .into()
        }

        /// A conversation in the sidebar, with its right-click menu.
        fn conversation_row<'a>(&'a self, c: &'a ConversationInfo) -> Element<'a, Message> {
            if self.renaming(&c.id, TitleAt::Sidebar) {
                return widget::container(self.rename_input(Length::Fill))
                    .padding([2, 4])
                    .into();
            }
            let size = 14.0 * self.zoom;
            let radius = cosmic::theme::active().cosmic().corner_radii.radius_s;
            let mut title = widget::text(c.title.as_str())
                .size(size)
                .wrapping(text::Wrapping::None)
                .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
                .width(Length::Fill);
            if c.archived {
                title = title.class(dim());
            }
            let mut row = vec![title.into()];
            if let Some(badge) = self.views.get(&c.id).and_then(|v| self.badge(v, size)) {
                row.push(badge);
            }
            let shown = self.page == Page::Chat && self.tile_of(&c.id).is_some();
            let item = widget::button::custom(
                widget::Row::with_children(row)
                    .spacing(8)
                    .align_y(Alignment::Center),
            )
            .class(conversation_style(radius, shown))
            .selected(shown)
            .on_press(Message::Open(c.id.clone()))
            .padding([6, 10])
            .width(Length::Fill);
            let id = || c.id.clone();
            let menu = vec![
                menu_item("Open in new tile", Message::OpenTile(id())),
                menu_item("Rename", Message::StartRename(id(), TitleAt::Sidebar)),
                if c.archived {
                    menu_item("Unarchive", Message::Archive(id(), false))
                } else {
                    menu_item("Archive", Message::Archive(id(), true))
                },
                menu_item("Delete…", Message::AskDelete(id())),
            ];
            widget::context_menu(item, Some(menu))
                .on_surface_action(Message::Surface)
                .into()
        }

        /// Marks a conversation an agent is working in, so it shows wherever its tile is.
        fn badge(&self, view: &ConversationView, size: f32) -> Option<Element<'_, Message>> {
            if !view.approvals.is_empty() {
                return Some(pill("Approve", size * 0.8));
            }
            view.streaming.then(|| pulse_dots(self.pulse, size))
        }

        fn focused_info(&self) -> Option<&ConversationInfo> {
            self.info(self.focused()?)
        }

        /// The tiles side by side, sharing the width until each reaches `TILE_MIN`.
        fn tiles(&self) -> Element<'_, Message> {
            let count = self.layout.tiles.len();
            let several = count > 1;
            let room = self.tile_room().1;
            let overflow = room.is_none();
            let total = self.total_share();
            let lifted = match self.drag {
                Some(Drag::Tile {
                    from,
                    at: Some(at),
                    slot,
                    ..
                }) => Some((from, at, slot)),
                _ => None,
            };
            let width = |share: f32| {
                if overflow {
                    Length::Fixed(self.tile_min())
                } else {
                    Length::FillPortion((share * 1000.0).round().max(1.0) as u16)
                }
            };

            // A lifted tile sits where it would land, so the row previews the drop.
            let mut order: Vec<usize> = (0..count)
                .filter(|i| lifted.is_none_or(|(from, ..)| *i != from))
                .collect();
            if let Some((from, _, slot)) = lifted {
                order.insert(slot.min(order.len()), from);
            }

            let mut row: Vec<Element<Message>> = Vec::new();
            for (k, &i) in order.iter().enumerate() {
                if k > 0 {
                    row.push(if overflow || lifted.is_some() {
                        divider()
                    } else {
                        widget::mouse_area(divider())
                            .on_press(Message::StartResize(k - 1))
                            .interaction(cosmic::iced::mouse::Interaction::ResizingHorizontally)
                            .into()
                    });
                }
                let tile = &self.layout.tiles[i];
                let Some(view) = self.views.get(&tile.conversation) else {
                    continue;
                };
                let pixels = room.map_or(self.tile_min(), |room| tile.share / total * room);
                let tile_view = TileView {
                    several,
                    focused: several && i == self.layout.focused,
                    compact: pixels < COMPACT_TILE,
                    lifted: lifted.is_some_and(|(from, ..)| from == i),
                    pixels,
                };
                row.push(self.tile(&tile.conversation, view, tile_view, width(tile.share)));
            }
            let row = widget::Row::with_children(row).height(Length::Fill);
            let row: Element<Message> = if overflow {
                widget::scrollable::horizontal(row)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into()
            } else {
                row.width(Length::Fill).into()
            };

            // The layers and the mouse area stay while idle, so starting a drag does not
            // rebuild the tiles and lose their scroll positions.
            let card: Element<Message> = match lifted {
                Some((from, at, _)) => {
                    let pixels = room.map_or(self.tile_min(), |room| {
                        self.layout.tiles[from].share / total * room
                    });
                    let (w, h) = ((pixels * 0.55).clamp(220.0, 420.0), 180.0 * self.zoom);
                    widget::container(self.lifted_card(&self.layout.tiles[from].conversation, w, h))
                        .padding(Padding {
                            top: (at.y - h / 2.0).max(0.0),
                            left: (at.x - w / 2.0).max(0.0),
                            ..Padding::ZERO
                        })
                        .into()
                }
                None => widget::space::horizontal().width(Length::Fixed(0.0)).into(),
            };
            let mut area = widget::mouse_area(
                cosmic::iced::widget::Stack::with_children(vec![row, card])
                    .width(Length::Fill)
                    .height(Length::Fill),
            );
            if matches!(self.drag, Some(Drag::Tile { .. })) {
                area = area
                    .on_move(Message::DragTile)
                    .interaction(cosmic::iced::mouse::Interaction::Grabbing);
            }
            area.into()
        }

        /// A dragged tile, shrunk to its title, context and latest message.
        fn lifted_card<'a>(
            &'a self,
            id: &'a ConversationId,
            width: f32,
            height: f32,
        ) -> Element<'a, Message> {
            let size = 13.0 * self.zoom;
            let info = self.info(id);
            let latest = self.views.get(id).and_then(|view| {
                view.items.iter().rev().find_map(|entry| match entry {
                    Entry::User(text) | Entry::Incoming { text, .. } => Some(text.as_str()),
                    Entry::Reply(reply) => Some(reply.text.as_str()),
                    _ => None,
                })
            });
            let mut column = vec![
                widget::text::heading(info.map_or("New conversation", |c| c.title.as_str()))
                    .wrapping(text::Wrapping::None)
                    .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
                    .into(),
            ];
            if let Some(label) = info.and_then(|c| context_label(&c.context)) {
                column.push(widget::text(label).size(size * 0.9).class(dim()).into());
            }
            if let Some(latest) = latest {
                column.push(
                    widget::text(truncate(latest, 400))
                        .size(size)
                        .class(dim())
                        .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Height(height * 0.6)))
                        .into(),
                );
            }
            widget::container(widget::Column::with_children(column).spacing(6))
                .padding(14)
                .width(Length::Fixed(width))
                .height(Length::Fixed(height))
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
                        shadow: cosmic::iced::Shadow {
                            color: Color::from_rgba(0.0, 0.0, 0.0, 0.35),
                            offset: cosmic::iced::Vector::new(0.0, 6.0),
                            blur_radius: 18.0,
                        },
                        ..container::Style::default()
                    }
                }))
                .into()
        }

        /// A conversation with, beside others, a header naming it and an outline when focused.
        fn tile<'a>(
            &'a self,
            id: &'a ConversationId,
            view: &'a ConversationView,
            tile: TileView,
            width: Length,
        ) -> Element<'a, Message> {
            let body = self.conversation(id, view, tile.compact);
            let content: Element<Message> = if tile.several {
                widget::Column::with_children(vec![self.tile_header(id, tile.pixels), body]).into()
            } else {
                body
            };
            // Named for the conversation, so libcosmic gives the tile its own scroll and
            // editor state back wherever it moves. The name sits below the row's direct
            // child: between siblings, libcosmic finds a moved state by name but leaves it
            // at its old index.
            let named = Intercept {
                id: Some(widget::Id::new(format!("tile-{id}"))),
                on_ctrl_press: Some(Message::StartMove(id.clone())),
                ..Intercept::new(content)
            };
            let framed = widget::container(named)
                .width(width)
                .height(Length::Fill)
                .class(tile_style(tile.focused, tile.lifted));
            widget::mouse_area(framed)
                .on_press(Message::Focus(id.clone()))
                .into()
        }

        fn tile_header<'a>(&'a self, id: &'a ConversationId, width: f32) -> Element<'a, Message> {
            let info = self.info(id);
            let mut row =
                if self.renaming(id, TitleAt::Header) {
                    vec![self.rename_input(Length::Fill)]
                } else {
                    // The title stays as wide as its text, so the rest of the header can be
                    // dragged; past the room it has, it ellipsizes.
                    vec![
                        widget::container(self.title_button(
                            id,
                            info.map_or("New conversation", |c| c.title.as_str()),
                        ))
                        .max_width((width - 220.0).max(80.0))
                        .into(),
                        widget::space::horizontal().into(),
                    ]
                };
            if let Some(label) = info.and_then(|c| context_label(&c.context)) {
                row.push(widget::text(label).size(12.0).class(dim()).into());
            }
            row.push(
                widget::button::icon(widget::icon::from_name("window-close-symbolic"))
                    .on_press(Message::Close(id.clone()))
                    .into(),
            );
            let header = widget::mouse_area(
                widget::container(
                    widget::Row::with_children(row)
                        .spacing(8)
                        .align_y(Alignment::Center),
                )
                .padding([4, 8, 4, 16]),
            )
            .on_press(Message::StartMove(id.clone()))
            .interaction(cosmic::iced::mouse::Interaction::Grab);

            let i = self.tile_of(id).unwrap_or_default();
            let mut menu = Vec::new();
            if i > 0 {
                menu.push(menu_item("Move left", Message::Move(id.clone(), -1)));
            }
            if i + 1 < self.layout.tiles.len() {
                menu.push(menu_item("Move right", Message::Move(id.clone(), 1)));
            }
            menu.push(menu_item("Close", Message::Close(id.clone())));
            widget::context_menu(header, Some(menu))
                .on_surface_action(Message::Surface)
                .into()
        }

        /// A new conversation centers the composer under a greeting, as Claude Desktop does.
        fn conversation<'a>(
            &'a self,
            id: &'a ConversationId,
            view: &'a ConversationView,
            compact: bool,
        ) -> Element<'a, Message> {
            if view.items.is_empty() && !view.streaming {
                let greeting =
                    widget::text::title2("How can I help you today?").size(26.0 * self.zoom);
                return widget::Column::with_children(vec![
                    widget::space::vertical().into(),
                    widget::container(greeting).center_x(Length::Fill).into(),
                    self.composer(id, view, compact),
                    widget::space::vertical().into(),
                ])
                .spacing(24)
                .into();
            }
            let mut column = vec![self.transcript(id, view)];
            column.extend(
                view.approvals
                    .iter()
                    .map(|approval| self.approval_card(id, approval)),
            );
            column.push(self.composer(id, view, compact));
            widget::Column::with_children(column).spacing(8).into()
        }

        /// What Claude wants to run, and the answers. The turn waits on it.
        fn approval_card<'a>(
            &self,
            id: &ConversationId,
            approval: &'a Approval,
        ) -> Element<'a, Message> {
            let size = 14.0 * self.zoom;
            let call = &approval.call;
            let answer = |decision| Message::Answer {
                conversation_id: id.clone(),
                request_id: approval.request_id.clone(),
                decision,
            };
            let buttons = widget::Row::with_children(vec![
                widget::space::horizontal().into(),
                widget::button::destructive("Deny")
                    .on_press(answer(Decision::Deny))
                    .into(),
                widget::tooltip(
                    widget::button::standard("Allow always")
                        .on_press(answer(Decision::AllowAlways)),
                    widget::text(format!(
                        "Don't ask again for {} in this conversation",
                        approval.scope
                    )),
                    widget::tooltip::Position::Top,
                )
                .into(),
                widget::button::suggested("Allow once")
                    .on_press(answer(Decision::AllowOnce))
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
                        radius: cosmic.corner_radii.radius_m.into(),
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
        fn composer<'a>(
            &'a self,
            id: &'a ConversationId,
            view: &'a ConversationView,
            compact: bool,
        ) -> Element<'a, Message> {
            let size = 14.0 * self.zoom;
            let (model, effort, profile) = self.effective(id);
            let placeholder = if view.items.is_empty() {
                "Message Claude"
            } else {
                "Reply to Claude…"
            };
            let editor = widget::text_editor(&view.draft)
                .id(composer_id(id))
                .placeholder(placeholder)
                .on_action(move |action| Message::Edit(id.clone(), action))
                .key_binding(move |press| composer_keys(press, id, view))
                .size(size)
                .padding(0)
                .min_height(if compact { size * 1.3 } else { size * 2.6 })
                .max_height(240.0 * self.zoom)
                .class(cosmic::theme::iced::TextEditor::Custom(Box::new(
                    borderless_editor,
                )));

            let profiles: Vec<&str> = PROFILES
                .iter()
                .map(|p| match (compact, p) {
                    (true, Profile::AcceptEdits) => "Edits",
                    (true, Profile::FullAuto) => "Auto",
                    _ => p.label(),
                })
                .collect();
            let efforts: Vec<&str> = Effort::ALL
                .iter()
                .map(|e| match (compact, e) {
                    (true, Effort::Default) => "Default",
                    _ => e.label(),
                })
                .collect();
            let send = if view.streaming {
                widget::button::icon(widget::icon::from_name("media-playback-stop-symbolic"))
                    .class(cosmic::theme::Button::Standard)
                    .on_press(Message::Stop(id.clone()))
            } else {
                widget::button::icon(widget::icon::from_name("go-up-symbolic"))
                    .class(cosmic::theme::Button::Suggested)
                    .on_press_maybe(view.can_send().then(|| Message::Send(id.clone())))
            };
            let (on_profile, on_effort, on_model) = (id.clone(), id.clone(), id.clone());
            let profile = widget::dropdown(
                profiles,
                PROFILES.iter().position(|p| *p == profile),
                move |i| Message::OvProfile(on_profile.clone(), i),
            );
            let effort = widget::dropdown(
                efforts,
                Effort::ALL.iter().position(|e| *e == effort),
                move |i| Message::OvEffort(on_effort.clone(), i),
            );
            let (models, selected) = self.model_menu(&model);
            let model = widget::dropdown(models, selected, move |i| {
                Message::OvModel(on_model.clone(), i)
            })
            .placeholder(model_name(&model));
            let small = |dropdown: widget::Dropdown<'a, &'static str, Message, Message>| {
                dropdown.text_size(13.0 * self.zoom).padding([4, 8])
            };
            let controls = widget::Row::with_children(vec![
                small(profile).into(),
                small(effort).into(),
                widget::space::horizontal().into(),
                model.text_size(13.0 * self.zoom).padding([4, 8]).into(),
                send.into(),
            ])
            .spacing(if compact { 2 } else { 6 })
            .align_y(Alignment::Center);

            let boxed = widget::container(
                widget::Column::with_children(vec![editor.into(), controls.into()])
                    .spacing(if compact { 6 } else { 10 }),
            )
            .padding(if compact { [10, 10] } else { [14, 16] })
            .width(Length::Fill)
            .class(cosmic::theme::Container::custom(|theme| {
                let cosmic = theme.cosmic();
                container::Style {
                    background: Some(Color::from(cosmic.bg_component_color()).into()),
                    border: Border {
                        radius: cosmic.corner_radii.radius_m.into(),
                        width: 1.0,
                        color: cosmic.bg_divider().into(),
                    },
                    ..container::Style::default()
                }
            }));
            widget::container(widget::container(boxed).max_width(COLUMN_WIDTH * self.zoom))
                .center_x(Length::Fill)
                .padding(if compact {
                    [0, 8, 8, 8]
                } else {
                    [0, 16, 16, 16]
                })
                .into()
        }

        fn transcript<'a>(
            &'a self,
            id: &'a ConversationId,
            view: &'a ConversationView,
        ) -> Element<'a, Message> {
            let size = 14.0 * self.zoom;
            let settings = markdown_settings(size);
            let live = view.pulsing().then_some(self.pulse);
            let texts = Texts {
                conversation: id,
                selection: self.selection.as_ref(),
            };
            let mut rows: Vec<Element<Message>> = Vec::new();
            for (idx, entry) in view.items.iter().enumerate() {
                let toggle = || Message::Toggle(id.clone(), idx);
                rows.push(match entry {
                    Entry::User(text) => user_bubble(
                        texts.text(text.as_str(), size, (idx, 0)).shrink(),
                        self.zoom,
                    ),
                    Entry::Notice(text) => notice_row(text, size),
                    Entry::Incoming { from, text } => incoming_card(
                        from,
                        texts.text(text.as_str(), size, (idx, 0)),
                        size,
                        self.zoom,
                    ),
                    Entry::Reply(reply) => markdown::view_with(
                        reply.markdown.items(),
                        settings,
                        &selectable::Viewer {
                            conversation: id,
                            entry: idx,
                            selection: self.selection.as_ref(),
                            next: std::cell::Cell::new(0),
                        },
                    ),
                    Entry::Thinking { text, open } => {
                        let last = idx + 1 == view.items.len();
                        let body = open.then(|| {
                            texts
                                .text(text.as_str(), size * 0.95, (idx, 0))
                                .color(dim_color())
                        });
                        thinking_row(toggle(), text, body, live.filter(|_| last), size)
                    }
                    Entry::Tool { call, open } => tool_row(
                        toggle(),
                        call,
                        open.then_some((texts, idx)),
                        commands::call_status(call, view.streaming, self.pulse, size * 0.85),
                        size,
                    ),
                });
            }
            if let Some(label) = view.activity() {
                rows.push(activity_row(label, self.pulse, size));
            }
            if let Some(err) = &view.error {
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

    fn menu_item(label: &'static str, message: Message) -> menu::Tree<Message> {
        menu::Tree::from(Element::from(
            menu::menu_button(vec![widget::text(label).into()]).on_press(message),
        ))
    }

    fn pill(label: &str, size: f32) -> Element<'_, Message> {
        widget::container(widget::text(label).size(size))
            .padding([1, 8])
            .class(cosmic::theme::Container::custom(|theme| {
                let cosmic = theme.cosmic();
                container::Style {
                    background: Some(Color::from(cosmic.accent_color()).into()),
                    text_color: Some(cosmic.on_accent_color().into()),
                    border: Border {
                        radius: cosmic.corner_radii.radius_xl.into(),
                        ..Border::default()
                    },
                    ..container::Style::default()
                }
            }))
            .into()
    }

    fn divider<'a>() -> Element<'a, Message> {
        widget::container(widget::divider::vertical::default())
            .center_x(Length::Fixed(HANDLE_WIDTH))
            .height(Length::Fill)
            .into()
    }

    /// Takes some input before its content sees it: a left press made with Ctrl, so
    /// Ctrl+dragging over a button or the message box moves a tile instead of pressing
    /// them, and Escape, which a text input would take as losing focus. It can also carry
    /// an id: libcosmic matches a widget's state to it by the id it reports, and a
    /// container reports its content's, not its own.
    struct Intercept<'a> {
        id: Option<widget::Id>,
        content: Element<'a, Message>,
        on_ctrl_press: Option<Message>,
        on_escape: Option<Message>,
    }

    impl<'a> Intercept<'a> {
        fn new(content: impl Into<Element<'a, Message>>) -> Self {
            Self {
                id: None,
                content: content.into(),
                on_ctrl_press: None,
                on_escape: None,
            }
        }
    }

    #[derive(Default)]
    struct CtrlState {
        ctrl: bool,
    }

    impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Intercept<'_> {
        fn tag(&self) -> tree::Tag {
            tree::Tag::of::<CtrlState>()
        }

        fn state(&self) -> tree::State {
            tree::State::new(CtrlState::default())
        }

        fn children(&self) -> Vec<Tree> {
            vec![Tree::new(&self.content)]
        }

        fn diff(&mut self, tree: &mut Tree) {
            tree.diff_children(std::slice::from_mut(&mut self.content));
        }

        fn id(&self) -> Option<widget::Id> {
            self.id.clone()
        }

        fn set_id(&mut self, id: widget::Id) {
            self.id = Some(id);
        }

        fn size(&self) -> cosmic::iced::Size<Length> {
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
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                operation,
            );
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
            use cosmic::iced::Event;
            use cosmic::iced::mouse::{Button, Event as Mouse};
            let state = tree.state.downcast_mut::<CtrlState>();
            match event {
                Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                    state.ctrl = modifiers.control();
                }
                Event::Mouse(Mouse::ButtonPressed(Button::Left))
                    if state.ctrl && cursor.is_over(layout.bounds()) =>
                {
                    if let Some(message) = &self.on_ctrl_press {
                        shell.publish(message.clone());
                        shell.capture_event();
                        return;
                    }
                }
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Escape),
                    ..
                }) => {
                    if let Some(message) = &self.on_escape {
                        shell.publish(message.clone());
                        shell.capture_event();
                        return;
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
            viewport: &cosmic::iced::Rectangle,
            renderer: &cosmic::Renderer,
        ) -> cosmic::iced::mouse::Interaction {
            if self.on_ctrl_press.is_some()
                && tree.state.downcast_ref::<CtrlState>().ctrl
                && cursor.is_over(layout.bounds())
            {
                return cosmic::iced::mouse::Interaction::Grab;
            }
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

        fn drag_destinations(
            &self,
            tree: &Tree,
            layout: Layout<'_>,
            renderer: &cosmic::Renderer,
            rectangles: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
        ) {
            self.content.as_widget().drag_destinations(
                &tree.children[0],
                layout,
                renderer,
                rectangles,
            );
        }
    }

    impl<'a> From<Intercept<'a>> for Element<'a, Message> {
        fn from(intercept: Intercept<'a>) -> Self {
            Element::new(intercept)
        }
    }

    fn tile_style(focused: bool, lifted: bool) -> cosmic::theme::Container<'static> {
        cosmic::theme::Container::custom(move |theme| {
            let cosmic = theme.cosmic();
            let accent: Color = cosmic.accent_color().into();
            container::Style {
                background: lifted.then(|| Color { a: 0.08, ..accent }.into()),
                border: Border {
                    radius: cosmic.corner_radii.radius_s.into(),
                    width: match (lifted, focused) {
                        (true, _) => 2.0,
                        (false, true) => 1.0,
                        (false, false) => 0.0,
                    },
                    color: accent,
                },
                ..container::Style::default()
            }
        })
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

    /// What a transcript's selectable text needs: whose it is and what is selected.
    #[derive(Clone, Copy)]
    struct Texts<'a> {
        conversation: &'a ConversationId,
        selection: Option<&'a Selection>,
    }

    impl<'a> Texts<'a> {
        fn text(
            &self,
            text: impl IntoFragment<'a>,
            size: f32,
            block: (usize, usize),
        ) -> Selectable<'a> {
            Selectable::new(
                vec![text::Span::new(text)],
                size,
                self.conversation,
                block,
                self.selection,
            )
        }
    }

    fn user_bubble<'a>(text: Selectable<'a>, zoom: f32) -> Element<'a, Message> {
        let bubble = widget::container(text)
            .padding([10.0 * zoom, 14.0 * zoom])
            .max_width(BUBBLE_WIDTH * zoom)
            .class(cosmic::theme::Container::custom(|theme| {
                let cosmic = theme.cosmic();
                container::Style {
                    background: Some(Color::from(cosmic.bg_component_color()).into()),
                    text_color: Some(cosmic.on_bg_component_color().into()),
                    border: Border {
                        radius: cosmic.corner_radii.radius_m.into(),
                        ..Border::default()
                    },
                    ..container::Style::default()
                }
            }));
        widget::container(bubble).align_right(Length::Fill).into()
    }

    /// Another agent's message: an outlined card on Claude's side, so it never passes
    /// for one the user wrote.
    fn notice_row(text: &str, size: f32) -> Element<'_, Message> {
        widget::Row::with_children(vec![
            widget::icon::from_name("utilities-terminal-symbolic")
                .size(14)
                .into(),
            widget::text(text).size(size * 0.85).class(dim()).into(),
        ])
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
    }

    fn incoming_card<'a>(
        from: &'a str,
        text: Selectable<'a>,
        size: f32,
        zoom: f32,
    ) -> Element<'a, Message> {
        let accent: Color = cosmic::theme::active().cosmic().accent_text_color().into();
        let card = widget::container(
            widget::Column::with_children(vec![
                widget::text(format!("From {from}"))
                    .size(size * 0.85)
                    .font(cosmic::font::semibold())
                    .class(cosmic::theme::Text::Color(accent))
                    .into(),
                text.into(),
            ])
            .spacing(6.0 * zoom),
        )
        .padding([12.0 * zoom, 16.0 * zoom])
        .max_width(BUBBLE_WIDTH * zoom)
        .class(cosmic::theme::Container::custom(|theme| {
            let cosmic = theme.cosmic();
            container::Style {
                border: Border {
                    radius: cosmic.corner_radii.radius_m.into(),
                    width: 1.0,
                    color: Color {
                        a: 0.5,
                        ..cosmic.accent_color().into()
                    },
                },
                ..container::Style::default()
            }
        }));
        widget::container(card).align_left(Length::Fill).into()
    }

    /// A one-line row naming the tool and what it acted on; expands to its input and output.
    /// `open` carries what its input and output need to be selectable.
    fn tool_row<'a>(
        on_toggle: Message,
        call: &'a ToolCall,
        open: Option<(Texts<'a>, usize)>,
        status: Element<'a, Message>,
        size: f32,
    ) -> Element<'a, Message> {
        let small = size * 0.85;
        let chevron = if open.is_some() {
            "go-down-symbolic"
        } else {
            "go-next-symbolic"
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
                .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
                .width(Length::Fill)
                .into(),
            status,
        ])
        .spacing(8)
        .align_y(Alignment::Center);
        let header = widget::button::custom(header)
            .on_press(on_toggle)
            .class(cosmic::theme::Button::Text)
            .width(Length::Fill);
        let Some((texts, idx)) = open else {
            return header.into();
        };

        let mut details = vec![tool_text(texts, (idx, 0), "Input", &call.input, small)];
        if let Some(result) = &call.result {
            details.push(tool_text(texts, (idx, 1), "Output", result, small));
        }
        if let Some(output) = call.background.as_ref().and_then(|b| b.output.as_deref()) {
            details.push(tool_text(
                texts,
                (idx, 2),
                "Output when it ended",
                output,
                small,
            ));
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
    fn tool_text<'a>(
        texts: Texts<'a>,
        block: (usize, usize),
        label: &'a str,
        body: &'a str,
        size: f32,
    ) -> Element<'a, Message> {
        let total = body.lines().count();
        let shown = body
            .lines()
            .take(TOOL_OUTPUT_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        let mut rows: Vec<Element<Message>> = vec![
            widget::text(label).size(size * 0.9).class(dim()).into(),
            texts
                .text(shown, size, block)
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

    /// A text button in the plain text color, since the title is not a link.
    fn title_style() -> cosmic::theme::Button {
        use cosmic::widget::button::{Catalog, Style};
        let plain = |mut style: Style, theme: &cosmic::Theme| {
            style.text_color = Some(theme.cosmic().on_bg_color().into());
            style
        };
        let text = || cosmic::theme::Button::Text;
        cosmic::theme::Button::Custom {
            active: Box::new(move |focused, theme| {
                plain(theme.active(focused, false, &text()), theme)
            }),
            disabled: Box::new(move |theme| theme.disabled(&text())),
            hovered: Box::new(move |focused, theme| {
                plain(theme.hovered(focused, false, &text()), theme)
            }),
            pressed: Box::new(move |focused, theme| {
                plain(theme.pressed(focused, false, &text()), theme)
            }),
        }
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
    /// `body` is the whole summary, when the row is open.
    fn thinking_row<'a>(
        on_toggle: Message,
        text: &'a str,
        body: Option<Selectable<'a>>,
        live: Option<f32>,
        size: f32,
    ) -> Element<'a, Message> {
        let chevron = if body.is_some() {
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
                .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
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
        .on_press(on_toggle)
        .class(cosmic::theme::Button::Text)
        .width(Length::Fill);
        let Some(body) = body else {
            return header.into();
        };
        let body = widget::container(body).padding([4, 12, 8, 32]);
        widget::Column::with_children(vec![header.into(), body.into()])
            .spacing(2)
            .into()
    }

    fn dim() -> cosmic::theme::Text {
        cosmic::theme::Text::Color(dim_color())
    }

    fn dim_color() -> Color {
        let color: Color = cosmic::theme::active().cosmic().on_bg_color().into();
        Color { a: 0.6, ..color }
    }

    fn destructive() -> cosmic::theme::Text {
        cosmic::theme::Text::Color(cosmic::theme::active().cosmic().destructive_color().into())
    }

    fn rename_id() -> widget::Id {
        widget::Id::new("rename")
    }

    fn composer_id(conversation: &str) -> widget::Id {
        widget::Id::new(format!("composer-{conversation}"))
    }

    /// Enter sends, and Up and Down past the first or last line recall prompts; Shift+Enter
    /// and everything else keep the editor's own bindings.
    fn composer_keys(
        press: text_editor::KeyPress,
        conversation: &ConversationId,
        view: &ConversationView,
    ) -> Option<text_editor::Binding<Message>> {
        let draft = &view.draft;
        use keyboard::key::Named;
        use text_editor::{Binding, Motion};
        let enter = matches!(press.key, keyboard::Key::Named(Named::Enter));
        let focused = matches!(press.status, text_editor::Status::Focused { .. });
        let modifiers = press.modifiers;
        let arrow = matches!(
            press.key,
            keyboard::Key::Named(Named::ArrowLeft | Named::ArrowRight)
        );
        // Alt+arrows switch and move tiles.
        if arrow && modifiers.alt() {
            return None;
        }
        if enter && focused && !modifiers.shift() {
            return Some(Binding::Custom(Message::Send(conversation.clone())));
        }
        let line = draft.cursor().position.line;
        let older = match press.key {
            keyboard::Key::Named(Named::ArrowUp)
                if line == 0 && view.recalled < view.prompts().len() =>
            {
                Some(true)
            }
            keyboard::Key::Named(Named::ArrowDown)
                if line + 1 == draft.line_count() && view.recalled > 0 =>
            {
                Some(false)
            }
            _ => None,
        };
        if let Some(older) = older
            && focused
            && modifiers.is_empty()
        {
            return Some(Binding::Custom(Message::Recall {
                conversation: conversation.clone(),
                older,
            }));
        }
        // iced's editor deletes one character whatever the modifiers; with Ctrl it is a word,
        // or the selection when there is one.
        let word = match press.key {
            keyboard::Key::Named(Named::Backspace) => Some((Motion::WordLeft, Binding::Backspace)),
            keyboard::Key::Named(Named::Delete) => Some((Motion::WordRight, Binding::Delete)),
            _ => None,
        };
        if let Some((motion, delete)) = word
            && focused
            && modifiers.control()
        {
            return Some(match draft.selection() {
                Some(_) => delete,
                None => Binding::Sequence(vec![Binding::Select(motion), delete]),
            });
        }
        // Ctrl with `-`, `=` or `0` still reports the plain character, which would be typed
        // along with the zoom.
        match Binding::from_key_press(press)? {
            Binding::Insert(_) if modifiers.control() && !modifiers.alt() => None,
            binding => Some(binding),
        }
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
        type Flags = UiState;
        type Message = Message;
        const APP_ID: &'static str = "com.claudo.Cosmic";

        fn core(&self) -> &Core {
            &self.core
        }

        fn core_mut(&mut self) -> &mut Core {
            &mut self.core
        }

        fn init(core: Core, ui: UiState) -> (Self, Task<Message>) {
            let app = ClaudoApp {
                core,
                commands: None,
                status: None,
                settings: Settings::default(),
                models: Vec::new(),
                conversations: Vec::new(),
                views: HashMap::new(),
                layout: WindowLayout {
                    tiles: Vec::new(),
                    focused: 0,
                    sidebar_open: ui.sidebar_open,
                    sidebar_width: ui.sidebar_width,
                    commands_open: ui.commands_open,
                    commands_width: ui.commands_width,
                    size: ui.window,
                },
                pending: HashMap::new(),
                next_request: 0,
                pulse: 0.0,
                zoom: ui.zoom,
                page: ui.page,
                modifiers: keyboard::Modifiers::empty(),
                drag: None,
                selection: None,
                renaming: None,
                deleting: None,
                show_archived: ui.show_archived,
                drafts: ui.drafts,
                restore: Some((ui.tiles, ui.focused)),
                focus_composer: None,
            };
            (app, Task::none())
        }

        fn subscription(&self) -> Subscription<Message> {
            let mut subscriptions = vec![named("backend", backend_worker), window_events()];
            #[cfg(feature = "hot")]
            subscriptions.push(named("hot-patches", hot_patches));
            if matches!(self.drag, Some(Drag::Tile { .. }))
                || self.selection.as_ref().is_some_and(|s| s.dragging)
            {
                subscriptions.push(cosmic::iced::event::listen_with(release_events));
            } else if self.drag.is_some() {
                subscriptions.push(cosmic::iced::event::listen_with(drag_events));
            }
            // Timers run only while something animates; idle stays event-driven.
            if self.revealing() {
                subscriptions.push(
                    cosmic::iced::time::every(std::time::Duration::from_millis(16))
                        .map(|_| Message::Frame),
                );
            } else if self.streaming() {
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
                Message::Frame => {
                    for view in self.views.values_mut() {
                        view.advance_reveal(1.0 / 60.0);
                    }
                    self.pulse += 1.0 / 60.0;
                }
                Message::Pulse => self.pulse += PULSE_TICK.as_secs_f32(),
                Message::Zoom(factor) => {
                    self.zoom = (self.zoom * factor).clamp(0.5, 3.0);
                    self.persist();
                }
                Message::ZoomReset => {
                    self.zoom = 1.0;
                    self.persist();
                }
                Message::Modifiers(modifiers) => self.modifiers = modifiers,
                Message::Edit(id, action) => {
                    if matches!(action, text_editor::Action::Click(_)) {
                        self.selection = None;
                    }
                    // An editor losing focus clears its selection through this message too.
                    if !matches!(
                        action,
                        text_editor::Action::ClearSelection | text_editor::Action::Scroll { .. }
                    ) {
                        self.focus(&id);
                    }
                    if let Some(view) = self.views.get_mut(&id) {
                        // An edited recall is the user's own draft now, kept when recalling again.
                        if matches!(action, text_editor::Action::Edit(_)) {
                            view.recalled = 0;
                        }
                        view.draft.perform(action);
                    }
                }
                Message::Send(id) => {
                    self.focus(&id);
                    self.send(id);
                }
                Message::ShowCommands(show) => {
                    self.layout.commands_open = show;
                    self.persist();
                }
                Message::StartCommandsDrag => self.drag = Some(Drag::Commands { offset: None }),
                Message::ToggleCommandGroup(id) => {
                    if let Some(view) = self.views.get_mut(&id) {
                        view.commands_collapsed = !view.commands_collapsed;
                    }
                }
                Message::ToggleCommand(id, idx) => {
                    if let Some(view) = self.views.get_mut(&id)
                        && !view.commands_open.remove(&idx)
                    {
                        view.commands_open.insert(idx);
                    }
                }
                Message::Recall {
                    conversation,
                    older,
                } => {
                    if let Some(view) = self.views.get_mut(&conversation) {
                        view.recall(older);
                    }
                }
                Message::Stop(conversation_id) => self.cmd(Command::Stop { conversation_id }),
                Message::Link(url) => {
                    std::thread::spawn(move || {
                        let _ = std::process::Command::new("xdg-open").arg(url).status();
                    });
                }
                #[cfg(feature = "hot")]
                Message::Hotpatched => {}
                Message::Toggle(id, idx) => {
                    if let Some(Entry::Tool { open, .. } | Entry::Thinking { open, .. }) = self
                        .views
                        .get_mut(&id)
                        .and_then(|view| view.items.get_mut(idx))
                    {
                        *open = !*open;
                    }
                }
                Message::Open(id) => {
                    self.page = Page::Chat;
                    let placement = if self.modifiers.shift() {
                        Placement::Right
                    } else {
                        Placement::Focused
                    };
                    self.open(id, placement);
                }
                Message::OpenTile(id) => {
                    self.page = Page::Chat;
                    self.open(id, Placement::Right);
                }
                Message::Focus(id) => {
                    self.selection = None;
                    self.focus(&id);
                    self.focus_composer = Some(id);
                }
                Message::SelectStart(conversation, mark) => {
                    self.focus(&conversation);
                    self.selection = Some(Selection {
                        conversation,
                        anchor: mark,
                        focus: mark,
                        dragging: true,
                    });
                }
                Message::SelectTo(mark) => {
                    if let Some(selection) = &mut self.selection
                        && selection.dragging
                    {
                        selection.focus = mark;
                    }
                }
                Message::SelectRange(conversation, anchor, focus) => {
                    self.focus(&conversation);
                    self.selection = Some(Selection {
                        conversation,
                        anchor,
                        focus,
                        dragging: false,
                    });
                    return self.collect_selection(true);
                }
                Message::Copy => return self.collect_selection(false),
                Message::StartRename(conversation, at) => {
                    let text = self
                        .info(&conversation)
                        .map(|c| c.title.clone())
                        .unwrap_or_default();
                    self.renaming = Some(Renaming {
                        conversation,
                        text,
                        at,
                    });
                    return Task::batch([
                        widget::text_input::focus(rename_id()),
                        widget::text_input::select_all(rename_id()),
                    ]);
                }
                Message::RenameText(text) => {
                    if let Some(renaming) = &mut self.renaming {
                        renaming.text = text;
                    }
                }
                Message::CommitRename => {
                    if let Some(renaming) = self.renaming.take() {
                        self.rename(renaming.conversation, renaming.text.trim().to_string());
                    }
                }
                Message::CancelRename => self.renaming = None,
                Message::AskDelete(id) => self.deleting = Some(id),
                Message::Archive(id, archived) => {
                    if let Some(info) = self.conversations.iter_mut().find(|c| c.id == id) {
                        info.archived = archived;
                    }
                    if archived {
                        self.untile(&id);
                    }
                    self.cmd(Command::Archive { id, archived });
                }
                Message::ToggleArchived => {
                    self.show_archived = !self.show_archived;
                    self.persist();
                }
                Message::CancelDelete => self.deleting = None,
                Message::ConfirmDelete => {
                    if let Some(id) = self.deleting.take() {
                        self.forget(&id);
                        self.cmd(Command::Delete { id });
                    }
                }
                Message::Copied { text, primary } => {
                    return if primary {
                        cosmic::iced::clipboard::write_primary(text)
                    } else {
                        cosmic::iced::clipboard::write(text)
                    };
                }
                Message::Close(id) => self.close(&id),
                Message::CloseFocused => {
                    if self.page == Page::Chat
                        && let Some(id) = self.focused().cloned()
                    {
                        self.close(&id);
                    }
                }
                Message::New => {
                    self.page = Page::Chat;
                    let placement = if self.modifiers.shift() {
                        Placement::Right
                    } else {
                        Placement::Focused
                    };
                    self.new_conversation(placement);
                }
                Message::SetPage(page) => {
                    self.page = page;
                    self.persist();
                }
                Message::ToggleSidebar => {
                    self.layout.sidebar_open = !self.layout.sidebar_open;
                    self.persist();
                }
                Message::StartSidebarDrag => self.drag = Some(Drag::Sidebar { offset: None }),
                Message::StartResize(left) => {
                    let tiles = &self.layout.tiles;
                    if left + 1 < tiles.len() {
                        self.drag = Some(Drag::Divider {
                            left,
                            origin: None,
                            start: (tiles[left].share, tiles[left + 1].share),
                        });
                    }
                }
                Message::StartMove(id) => {
                    if let Some(from) = self.tile_of(&id) {
                        self.start_move(from);
                    }
                }
                Message::DragTo(x) => self.drag_to(x),
                Message::DragTile(point) => self.drag_tile(point),
                Message::DragEnd if self.selection.as_ref().is_some_and(|s| s.dragging) => {
                    if let Some(selection) = &mut self.selection {
                        selection.dragging = false;
                    }
                    return self.collect_selection(true);
                }
                Message::DragEnd => {
                    if let Some(Drag::Tile {
                        from,
                        at: Some(_),
                        slot,
                        ..
                    }) = self.drag.take()
                    {
                        self.move_tile(from, slot);
                        self.focus_composer = self.focused().cloned();
                    }
                    self.persist();
                }
                Message::Move(id, step) => {
                    if let Some(i) = self.tile_of(&id) {
                        self.move_tile(i, i.saturating_add_signed(step as isize));
                    }
                }
                Message::MoveFocused(step) => {
                    let i = self.layout.focused;
                    self.move_tile(i, i.saturating_add_signed(step as isize));
                }
                Message::FocusStep(step) => {
                    let last = self.layout.tiles.len().saturating_sub(1);
                    self.layout.focused = self
                        .layout
                        .focused
                        .saturating_add_signed(step as isize)
                        .min(last);
                    self.focus_composer = self.focused().cloned();
                }
                // Popups are windows too; only the main one's size is worth restoring.
                Message::Resized(id, size) => {
                    if self.core.main_window_id() == Some(id) {
                        self.layout.size = Some((size.width, size.height));
                    }
                }
                Message::Surface(action) => {
                    return cosmic::task::message(cosmic::Action::Cosmic(
                        cosmic::app::Action::Surface(action),
                    ));
                }
                Message::Answer {
                    conversation_id,
                    request_id,
                    decision,
                } => {
                    if let Some(view) = self.views.get_mut(&conversation_id) {
                        view.approvals.retain(|a| a.request_id != request_id);
                    }
                    self.cmd(Command::AnswerApproval {
                        conversation_id,
                        request_id,
                        decision,
                    });
                }
                // libcosmic's Wayland teardown races between two threads and segfaults.
                // Nothing here needs tearing down: state is saved as it changes.
                Message::Quit => {
                    self.persist();
                    std::process::exit(0)
                }
                Message::SetModel(i) => {
                    if let Some(model) = self.models.get(i) {
                        self.settings.model = model.value.clone();
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
                Message::OvModel(id, i) => {
                    if let Some(model) = self.models.get(i).map(|m| m.value.clone()) {
                        self.set_override(id, |o| o.model = Some(model));
                    }
                }
                Message::OvEffort(id, i) => {
                    if let Some(effort) = Effort::ALL.get(i) {
                        self.set_override(id, |o| o.effort = Some(*effort));
                    }
                }
                Message::OvProfile(id, i) => {
                    if let Some(profile) = PROFILES.get(i) {
                        self.set_override(id, |o| o.profile = Some(*profile));
                    }
                }
            }
            if let Some(id) = self.focus_composer.take() {
                return widget::text_input::focus(composer_id(&id));
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
                Page::Chat => self.tiles(),
                Page::Settings => self.settings_page(),
            };
            let mut columns = Vec::new();
            if self.layout.sidebar_open {
                columns.push(self.sidebar());
                columns.push(
                    widget::mouse_area(
                        widget::container(widget::divider::vertical::default())
                            .center_x(Length::Fixed(HANDLE_WIDTH))
                            .height(Length::Fill),
                    )
                    .on_press(Message::StartSidebarDrag)
                    .interaction(cosmic::iced::mouse::Interaction::ResizingHorizontally)
                    .into(),
                );
            }
            columns.push(main);
            if let Some(panel) = self.commands_panel() {
                columns.push(
                    widget::mouse_area(
                        widget::container(widget::divider::vertical::default())
                            .center_x(Length::Fixed(HANDLE_WIDTH))
                            .height(Length::Fill),
                    )
                    .on_press(Message::StartCommandsDrag)
                    .interaction(cosmic::iced::mouse::Interaction::ResizingHorizontally)
                    .into(),
                );
                columns.push(panel);
            }
            widget::Row::with_children(columns)
                .height(Length::Fill)
                .into()
        }

        fn header_start(&self) -> Vec<Element<'_, Message>> {
            vec![
                widget::nav_bar_toggle()
                    .active(self.layout.sidebar_open)
                    .on_toggle(Message::ToggleSidebar)
                    .into(),
            ]
        }

        /// With several tiles each names its own conversation, so the window names none.
        fn header_center(&self) -> Vec<Element<'_, Message>> {
            let title: Option<Element<Message>> = match self.page {
                Page::Settings => Some(widget::text::heading("Settings").into()),
                Page::Chat if self.layout.tiles.len() == 1 => self.focused().map(|id| {
                    if self.renaming(id, TitleAt::Header) {
                        self.rename_input(Length::Fixed(360.0))
                    } else {
                        let title = self
                            .info(id)
                            .map_or("New conversation", |c| c.title.as_str());
                        widget::container(self.title_button(id, title))
                            .max_width(480.0)
                            .into()
                    }
                }),
                Page::Chat => None,
            };
            title.into_iter().collect()
        }

        fn header_end(&self) -> Vec<Element<'_, Message>> {
            if self.page != Page::Chat {
                return Vec::new();
            }
            let context = self
                .focused_info()
                .filter(|_| self.layout.tiles.len() == 1)
                .and_then(|c| context_label(&c.context))
                .map(|label| widget::text(label).size(12.0).class(dim()).into());
            context
                .into_iter()
                .chain([self.commands_button()])
                .collect()
        }

        fn dialog(&self) -> Option<Element<'_, Message>> {
            self.deleting.as_deref().map(|id| self.delete_dialog(id))
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

    /// How a model menu names a `--model` value that is not one of its entries.
    fn model_name(model: &str) -> String {
        if model.is_empty() {
            "CLI default".to_string()
        } else {
            model.to_string()
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

    /// Window resizes, modifiers, shortcuts and Ctrl+scroll zoom. Scroll events carry no
    /// modifier state, so Ctrl is tracked here; a plain scroll yields no message, as each
    /// one rebuilds the view.
    fn window_events() -> Subscription<Message> {
        use std::sync::atomic::{AtomicBool, Ordering};

        use cosmic::iced::Event;
        use cosmic::iced::keyboard::{Event as Key, Key as K, key::Named};
        use cosmic::iced::mouse::{Event as Mouse, ScrollDelta};

        static CTRL: AtomicBool = AtomicBool::new(false);

        cosmic::iced::event::listen_with(|event, _status, id| match event {
            Event::Keyboard(Key::ModifiersChanged(m)) => {
                CTRL.store(m.control(), Ordering::Relaxed);
                Some(Message::Modifiers(m))
            }
            Event::Window(cosmic::iced::window::Event::Resized(size)) => {
                Some(Message::Resized(id, size))
            }
            Event::Keyboard(Key::KeyPressed { key, modifiers, .. }) if modifiers.control() => {
                match key.as_ref() {
                    K::Character("+") | K::Character("=") => Some(Message::Zoom(1.1)),
                    K::Character("-") => Some(Message::Zoom(1.0 / 1.1)),
                    K::Character("0") => Some(Message::ZoomReset),
                    K::Character("w" | "W") => Some(Message::CloseFocused),
                    K::Character("c" | "C") => Some(Message::Copy),
                    K::Character("n" | "N") => Some(Message::New),
                    _ => None,
                }
            }
            Event::Keyboard(Key::KeyPressed {
                key: K::Named(Named::Escape),
                ..
            }) => Some(Message::CancelDelete),
            Event::Keyboard(Key::KeyPressed { key, modifiers, .. })
                if modifiers.alt() && !modifiers.control() =>
            {
                let step = match key.as_ref() {
                    K::Named(Named::ArrowLeft) => -1,
                    K::Named(Named::ArrowRight) => 1,
                    _ => return None,
                };
                Some(if modifiers.shift() {
                    Message::MoveFocused(step)
                } else {
                    Message::FocusStep(step)
                })
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

    /// Cursor moves and the release ending a drag; subscribed only during one, so
    /// ordinary mouse movement produces no messages.
    fn drag_events(
        event: cosmic::iced::Event,
        _status: cosmic::iced::event::Status,
        _id: cosmic::iced::window::Id,
    ) -> Option<Message> {
        use cosmic::iced::mouse::{Button, Event as Mouse};
        match event {
            cosmic::iced::Event::Mouse(Mouse::CursorMoved { position }) => {
                Some(Message::DragTo(position.x))
            }
            cosmic::iced::Event::Mouse(Mouse::ButtonReleased(Button::Left)) => {
                Some(Message::DragEnd)
            }
            _ => None,
        }
    }

    /// The release ending a tile drag; the tile row reports the cursor itself.
    fn release_events(
        event: cosmic::iced::Event,
        _status: cosmic::iced::event::Status,
        _id: cosmic::iced::window::Id,
    ) -> Option<Message> {
        use cosmic::iced::mouse::{Button, Event as Mouse};
        matches!(
            event,
            cosmic::iced::Event::Mouse(Mouse::ButtonReleased(Button::Left))
        )
        .then_some(Message::DragEnd)
    }

    mod commands;
    mod selectable;
    #[cfg(test)]
    mod tests;

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
                commands.send(Command::GetModels);
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
