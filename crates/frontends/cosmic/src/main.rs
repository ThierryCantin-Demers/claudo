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
        Command, ConversationId, ConversationInfo, Effort, Item, MODEL_PRESETS, Overrides, Profile,
        ServerEvent, Settings, SystemStatus, ToolCall,
    };
    use claudo_transport::{Commands, spawn_backend};

    use cosmic::app::{Core, Task};
    use std::collections::HashMap;
    use std::hash::Hash;

    use cosmic::cosmic_config::{Config, ConfigGet, ConfigSet};
    use serde::{Deserialize, Serialize};

    use cosmic::iced::advanced::subscription::{EventStream, Hasher, Recipe, from_recipe};
    use cosmic::iced::advanced::text::{Ellipsize, EllipsizeHeightLimit};
    use cosmic::iced::futures::stream::BoxStream;
    use cosmic::iced::futures::{SinkExt, Stream, StreamExt};
    use cosmic::iced::widget::{container, text};
    use cosmic::iced::{Alignment, Border, Color, Length, Padding, Subscription, keyboard, stream};
    use cosmic::widget::{self, markdown, menu, text_editor};
    use cosmic::{Application, Element};

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
    const HANDLE_WIDTH: f32 = 8.0;
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

    /// Everything shown for one conversation, whichever tile shows it. It outlives its
    /// tile while a turn runs, so a background agent's progress is there on return.
    struct ConversationView {
        items: Vec<Entry>,
        draft: text_editor::Content,
        /// Calls waiting for the user's answer, with their request ids.
        approvals: Vec<(String, ToolCall)>,
        streaming: bool,
        assistant_idx: Option<usize>,
        /// Chars of the streaming message revealed so far. The CLI delivers text
        /// in large chunks; revealing gradually reads as smooth typing.
        reveal: f32,
        error: Option<String>,
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
            }
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
                    request_id, call, ..
                } => self.approvals.push((request_id, call)),
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
        /// The handle between tile `left` and the next, from their shares at the start.
        Divider {
            left: usize,
            origin: Option<f32>,
            start: (f32, f32),
        },
        /// A tile by its header, with the gap it would drop into once it has moved.
        Tile {
            from: usize,
            origin: Option<f32>,
            slot: Option<usize>,
        },
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
        page: Page,
        tiles: Vec<Tile>,
        focused: usize,
        zoom: f32,
        pub window: Option<(f32, f32)>,
        /// Unsent text, per conversation.
        drafts: HashMap<ConversationId, String>,
    }

    impl Default for UiState {
        fn default() -> Self {
            Self {
                sidebar_open: true,
                sidebar_width: SIDEBAR_WIDTH,
                page: Page::Chat,
                tiles: Vec::new(),
                focused: 0,
                zoom: 1.0,
                window: None,
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
            allow: bool,
        },
        SetPage(Page),
        ToggleSidebar,
        StartSidebarDrag,
        StartResize(usize),
        StartMove(ConversationId),
        DragTo(f32),
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
                - count.saturating_sub(1) as f32 * HANDLE_WIDTH;
            (
                start,
                (room >= count as f32 * TILE_MIN * self.zoom).then_some(room),
            )
        }

        fn total_share(&self) -> f32 {
            self.layout.tiles.iter().map(|t| t.share).sum()
        }

        fn drag_to(&mut self, x: f32) {
            let (start, room) = self.tile_room();
            let total = self.total_share();
            let min = TILE_MIN * self.zoom;
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
                Some(Drag::Tile { origin, slot, .. }) => {
                    let origin = *origin.get_or_insert(x);
                    if slot.is_none() && (x - origin).abs() < 8.0 {
                        return;
                    }
                    let mut edge = start;
                    let centers = self.layout.tiles.iter().map(|t| {
                        let width = room.map_or(min, |room| t.share / total * room);
                        let center = edge + width / 2.0;
                        edge += width + HANDLE_WIDTH;
                        center
                    });
                    *slot = Some(centers.filter(|c| *c < x).count());
                }
                None => {}
            }
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
                match self.conversations.first().map(|c| c.id.clone()) {
                    Some(id) => self.open(id, Placement::Right),
                    None => self.new_conversation(Placement::Right),
                }
            }
            self.layout.focused = focused.min(self.layout.tiles.len().saturating_sub(1));
            self.focus_composer = self.focused().cloned();
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
                page: self.page,
                tiles: self.layout.tiles.clone(),
                focused: self.layout.focused,
                zoom: self.zoom,
                window: self.layout.size,
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
                    if let Some(view) = self.views.get_mut(&id) {
                        view.apply(ev);
                    }
                    if over {
                        self.release(&id);
                    }
                }
            }
        }

        fn settings_page(&self) -> Element<'_, Message> {
            let models: Vec<&str> = MODEL_PRESETS.iter().map(|(label, _)| *label).collect();
            let efforts: Vec<&str> = Effort::ALL.iter().map(|e| e.label()).collect();
            let profiles: Vec<&str> = PROFILES.iter().map(|p| p.label()).collect();
            let defaults = widget::settings::section()
                .title("Defaults")
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
            let size = 14.0 * self.zoom;
            let radius = cosmic::theme::active().cosmic().corner_radii.radius_s;
            let recents: Vec<Element<Message>> = self
                .conversations
                .iter()
                .map(|c| {
                    let mut row = vec![
                        widget::text(c.title.as_str())
                            .size(size)
                            .wrapping(text::Wrapping::None)
                            .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
                            .width(Length::Fill)
                            .into(),
                    ];
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
                    let menu = vec![menu_item(
                        "Open in new tile",
                        Message::OpenTile(c.id.clone()),
                    )];
                    widget::context_menu(item, Some(menu))
                        .on_surface_action(Message::Surface)
                        .into()
                })
                .collect();

            widget::container(
                widget::Column::with_children(vec![
                    widget::tooltip(
                        widget::button::text(if self.modifiers.shift() {
                            "New chat in new tile"
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
            .width(Length::Fixed(self.layout.sidebar_width))
            .clip(true)
            .height(Length::Fill)
            .padding(8)
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
            let overflow = self.tile_room().1.is_none();
            let slot = match self.drag {
                Some(Drag::Tile { from, slot, .. }) => {
                    slot.filter(|s| *s != from && *s != from + 1)
                }
                _ => None,
            };

            let mut row: Vec<Element<Message>> = Vec::new();
            for (i, tile) in self.layout.tiles.iter().enumerate() {
                let Some(view) = self.views.get(&tile.conversation) else {
                    continue;
                };
                if slot == Some(i) {
                    row.push(drop_marker());
                } else if i > 0 && overflow {
                    row.push(divider());
                } else if i > 0 {
                    row.push(
                        widget::mouse_area(divider())
                            .on_press(Message::StartResize(i - 1))
                            .interaction(cosmic::iced::mouse::Interaction::ResizingHorizontally)
                            .into(),
                    );
                }
                let width = if overflow {
                    Length::Fixed(TILE_MIN * self.zoom)
                } else {
                    Length::FillPortion((tile.share * 1000.0).round().max(1.0) as u16)
                };
                let focused = several && i == self.layout.focused;
                row.push(self.tile(&tile.conversation, view, several, focused, width));
            }
            if slot == Some(count) {
                row.push(drop_marker());
            }
            let row = widget::Row::with_children(row).height(Length::Fill);
            if overflow {
                widget::scrollable::horizontal(row)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into()
            } else {
                row.width(Length::Fill).into()
            }
        }

        /// A conversation with, beside others, a header naming it and an outline when focused.
        fn tile<'a>(
            &'a self,
            id: &'a ConversationId,
            view: &'a ConversationView,
            several: bool,
            focused: bool,
            width: Length,
        ) -> Element<'a, Message> {
            let body = self.conversation(id, view);
            let content: Element<Message> = if several {
                widget::Column::with_children(vec![self.tile_header(id), body]).into()
            } else {
                body
            };
            let framed = widget::container(content)
                .width(width)
                .height(Length::Fill)
                .class(tile_style(focused));
            widget::mouse_area(framed)
                .on_press(Message::Focus(id.clone()))
                .into()
        }

        fn tile_header<'a>(&'a self, id: &'a ConversationId) -> Element<'a, Message> {
            let info = self.info(id);
            let mut row = vec![
                widget::text::heading(info.map_or("New conversation", |c| c.title.as_str()))
                    .wrapping(text::Wrapping::None)
                    .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
                    .width(Length::Fill)
                    .into(),
            ];
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
        ) -> Element<'a, Message> {
            if view.items.is_empty() && !view.streaming {
                let greeting =
                    widget::text::title2("How can I help you today?").size(26.0 * self.zoom);
                return widget::Column::with_children(vec![
                    widget::space::vertical().into(),
                    widget::container(greeting).center_x(Length::Fill).into(),
                    self.composer(id, view),
                    widget::space::vertical().into(),
                ])
                .spacing(24)
                .into();
            }
            let mut column = vec![self.transcript(id, view)];
            column.extend(
                view.approvals
                    .iter()
                    .map(|(request, call)| self.approval_card(id, request, call)),
            );
            column.push(self.composer(id, view));
            widget::Column::with_children(column).spacing(8).into()
        }

        /// What Claude wants to run, and the two answers. The turn waits on it.
        fn approval_card<'a>(
            &self,
            id: &ConversationId,
            request_id: &str,
            call: &'a ToolCall,
        ) -> Element<'a, Message> {
            let size = 14.0 * self.zoom;
            let answer = |allow| Message::Answer {
                conversation_id: id.clone(),
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
        fn composer<'a>(
            &'a self,
            id: &'a ConversationId,
            view: &'a ConversationView,
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
                .key_binding(move |press| send_on_enter(press, id))
                .size(size)
                .padding(0)
                .max_height(240.0 * self.zoom)
                .class(cosmic::theme::iced::TextEditor::Custom(Box::new(
                    borderless_editor,
                )));

            let profiles: Vec<&str> = PROFILES.iter().map(|p| p.label()).collect();
            let efforts: Vec<&str> = Effort::ALL.iter().map(|e| e.label()).collect();
            let models: Vec<&str> = MODEL_PRESETS.iter().map(|(label, _)| *label).collect();
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
            let controls = widget::Row::with_children(vec![
                widget::dropdown(
                    profiles,
                    PROFILES.iter().position(|p| *p == profile),
                    move |i| Message::OvProfile(on_profile.clone(), i),
                )
                .into(),
                widget::dropdown(
                    efforts,
                    Effort::ALL.iter().position(|e| *e == effort),
                    move |i| Message::OvEffort(on_effort.clone(), i),
                )
                .into(),
                widget::space::horizontal().into(),
                widget::dropdown(
                    models,
                    MODEL_PRESETS.iter().position(|(_, m)| *m == model),
                    move |i| Message::OvModel(on_model.clone(), i),
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

        fn transcript<'a>(
            &'a self,
            id: &'a ConversationId,
            view: &'a ConversationView,
        ) -> Element<'a, Message> {
            let size = 14.0 * self.zoom;
            let settings = markdown_settings(size);
            let live = view.pulsing().then_some(self.pulse);
            let mut rows: Vec<Element<Message>> = Vec::new();
            for (idx, entry) in view.items.iter().enumerate() {
                let toggle = || Message::Toggle(id.clone(), idx);
                rows.push(match entry {
                    Entry::User(text) => user_bubble(text, size, self.zoom),
                    Entry::Reply(reply) => {
                        markdown::view(reply.markdown.items(), settings).map(Message::Link)
                    }
                    Entry::Thinking { text, open } => {
                        let last = idx + 1 == view.items.len();
                        thinking_row(toggle(), text, *open, live.filter(|_| last), size)
                    }
                    Entry::Tool { call, open } => tool_row(toggle(), call, *open, size),
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

    /// Where a dragged tile would land.
    fn drop_marker<'a>() -> Element<'a, Message> {
        widget::container(
            widget::container(widget::space::horizontal().width(Length::Fixed(3.0)))
                .height(Length::Fill)
                .class(cosmic::theme::Container::custom(|theme| container::Style {
                    background: Some(Color::from(theme.cosmic().accent_color()).into()),
                    ..container::Style::default()
                })),
        )
        .center_x(Length::Fixed(HANDLE_WIDTH))
        .height(Length::Fill)
        .into()
    }

    fn tile_style(focused: bool) -> cosmic::theme::Container<'static> {
        cosmic::theme::Container::custom(move |theme| {
            let cosmic = theme.cosmic();
            container::Style {
                border: Border {
                    radius: cosmic.corner_radii.radius_s.into(),
                    width: if focused { 1.0 } else { 0.0 },
                    color: cosmic.accent_color().into(),
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
    fn tool_row(
        on_toggle: Message,
        call: &ToolCall,
        open: bool,
        size: f32,
    ) -> Element<'_, Message> {
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
            .on_press(on_toggle)
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
        on_toggle: Message,
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
        .on_press(on_toggle)
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

    fn composer_id(conversation: &str) -> widget::Id {
        widget::Id::new(format!("composer-{conversation}"))
    }

    /// Enter sends; Shift+Enter and everything else keep the editor's own bindings.
    fn send_on_enter(
        press: text_editor::KeyPress,
        conversation: &ConversationId,
    ) -> Option<text_editor::Binding<Message>> {
        let enter = matches!(press.key, keyboard::Key::Named(keyboard::key::Named::Enter));
        let focused = matches!(press.status, text_editor::Status::Focused { .. });
        let modifiers = press.modifiers;
        if enter && focused && !modifiers.shift() {
            return Some(text_editor::Binding::Custom(Message::Send(
                conversation.clone(),
            )));
        }
        // Ctrl with `-`, `=` or `0` still reports the plain character, which would be typed
        // along with the zoom.
        match text_editor::Binding::from_key_press(press)? {
            text_editor::Binding::Insert(_) if modifiers.control() && !modifiers.alt() => None,
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
                conversations: Vec::new(),
                views: HashMap::new(),
                layout: WindowLayout {
                    tiles: Vec::new(),
                    focused: 0,
                    sidebar_open: ui.sidebar_open,
                    sidebar_width: ui.sidebar_width,
                    size: ui.window,
                },
                pending: HashMap::new(),
                next_request: 0,
                pulse: 0.0,
                zoom: ui.zoom,
                page: ui.page,
                modifiers: keyboard::Modifiers::empty(),
                drag: None,
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
            if self.drag.is_some() {
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
                    // An editor losing focus clears its selection through this message too.
                    if !matches!(
                        action,
                        text_editor::Action::ClearSelection | text_editor::Action::Scroll { .. }
                    ) {
                        self.focus(&id);
                    }
                    if let Some(view) = self.views.get_mut(&id) {
                        view.draft.perform(action);
                    }
                }
                Message::Send(id) => {
                    self.focus(&id);
                    self.send(id);
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
                    self.focus(&id);
                    self.focus_composer = Some(id);
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
                        self.layout.focused = from;
                        self.focus_composer = Some(id);
                        self.drag = Some(Drag::Tile {
                            from,
                            origin: None,
                            slot: None,
                        });
                    }
                }
                Message::DragTo(x) => self.drag_to(x),
                Message::DragEnd => {
                    if let Some(Drag::Tile {
                        from,
                        slot: Some(slot),
                        ..
                    }) = self.drag.take()
                    {
                        self.move_tile(from, if slot > from { slot - 1 } else { slot });
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
                    allow,
                } => {
                    if let Some(view) = self.views.get_mut(&conversation_id) {
                        view.approvals.retain(|(id, _)| *id != request_id);
                    }
                    self.cmd(Command::AnswerApproval {
                        conversation_id,
                        request_id,
                        allow,
                    });
                }
                // libcosmic's Wayland teardown races between two threads and segfaults.
                // Nothing here needs tearing down: state is saved as it changes.
                Message::Quit => {
                    self.persist();
                    std::process::exit(0)
                }
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
                Message::OvModel(id, i) => {
                    if let Some((_, model)) = MODEL_PRESETS.get(i) {
                        self.set_override(id, |o| o.model = Some((*model).to_string()));
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
            let title = match self.page {
                Page::Settings => Some("Settings".to_string()),
                Page::Chat if self.layout.tiles.len() == 1 => {
                    self.focused_info().map(|c| truncate(&c.title, 60))
                }
                Page::Chat => None,
            };
            title
                .map(|t| widget::text::heading(t).into())
                .into_iter()
                .collect()
        }

        fn header_end(&self) -> Vec<Element<'_, Message>> {
            if self.page != Page::Chat || self.layout.tiles.len() != 1 {
                return Vec::new();
            }
            self.focused_info()
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
                    K::Character("n" | "N") => Some(Message::New),
                    K::Named(Named::PageUp) if modifiers.shift() => Some(Message::MoveFocused(-1)),
                    K::Named(Named::PageDown) if modifiers.shift() => Some(Message::MoveFocused(1)),
                    K::Named(Named::PageUp) => Some(Message::FocusStep(-1)),
                    K::Named(Named::PageDown) => Some(Message::FocusStep(1)),
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
