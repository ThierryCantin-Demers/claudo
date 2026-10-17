//! Versioned message types shared across the backend and every frontend.
//! Frontends render these; they never contain logic. Also the wire format for
//! the WebSocket transport, so everything here is `serde`-serializable.

use serde::{Deserialize, Serialize};

/// Bumped on any breaking change so the web client and backend can negotiate.
pub const PROTOCOL_VERSION: u32 = 0;

pub type ConversationId = String;
pub type SessionId = String;
pub type CallId = String;

/// Result of checking the local Claude Code CLI on startup.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemStatus {
    pub claude_installed: bool,
    pub claude_version: Option<String>,
    pub auth: AuthStatus,
}

/// Mirror of `claude auth status --json`. The app never stores tokens; this is
/// only a read of the CLI's own reported state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthStatus {
    pub logged_in: bool,
    pub method: Option<String>,
    pub api_provider: Option<String>,
    pub email: Option<String>,
    pub subscription_type: Option<String>,
}

impl SystemStatus {
    /// The one hard requirement to use the app: CLI present and logged in.
    pub fn ready(&self) -> bool {
        self.claude_installed && self.auth.logged_in
    }
}

/// How much tool autonomy a turn is granted (maps to `--permission-mode`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Profile {
    #[default]
    Safe,
    AcceptEdits,
    FullAuto,
}

impl Profile {
    pub fn label(self) -> &'static str {
        match self {
            Profile::Safe => "Safe",
            Profile::AcceptEdits => "Accept edits",
            Profile::FullAuto => "Full auto",
        }
    }

    pub fn tooltip(self) -> &'static str {
        match self {
            Profile::Safe => "Tools that need approval are denied. Read-only tools still work.",
            Profile::AcceptEdits => {
                "File edits are auto-approved. Shell and other tools are still denied."
            }
            Profile::FullAuto => {
                "Every tool is auto-approved. Claude can run anything in the working directory."
            }
        }
    }
}

/// A model family the CLI offers. Its value is an alias, so choosing "Opus 5.5" keeps
/// running the newest Opus once a later version ships.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOption {
    /// The `--model` value, such as `opus`.
    pub value: String,
    /// The version the alias runs today, such as "Opus 5.5".
    pub label: String,
}

/// A slash command the CLI runs when a message starts with it: built in, or one of the
/// user's skills, commands or plugins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliCommand {
    /// Without its slash, such as `compact`.
    pub name: String,
    pub description: String,
    /// What it takes after its name, such as `<optional custom summarization instructions>`.
    #[serde(default)]
    pub argument_hint: String,
}

/// The conversation was summarized to free up context, by the user or automatically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Compaction {
    pub auto: bool,
    /// Tokens of context before and after.
    pub before: u64,
    pub after: u64,
    /// What the summary that replaced the conversation says.
    #[serde(default)]
    pub summary: String,
}

/// Model used when the user hasn't chosen one. A `--model` value is an alias
/// (`opus`) or a specific version (`claude-sonnet-5-5`); empty means the CLI's own.
pub const DEFAULT_MODEL: &str = "opus";

/// Subscription usage the CLI reports during each turn; account-wide, not per conversation.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Quota {
    pub five_hour: Option<QuotaWindow>,
    pub seven_day: Option<QuotaWindow>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct QuotaWindow {
    /// Fraction of the window used, from 0.0 to 1.0.
    pub used: f32,
    /// When the window resets, in Unix seconds.
    pub resets_at: u64,
}

/// Context usage for the last turn of a conversation, reported by the CLI.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ContextUsage {
    /// Tokens in the last request's context (input + cache read/creation).
    pub used: u64,
    /// The model's context window, if the CLI reported one.
    pub window: Option<u64>,
}

impl ContextUsage {
    pub fn percent(&self) -> Option<f32> {
        match self.window {
            Some(w) if w > 0 => Some((self.used as f32 / w as f32) * 100.0),
            _ => None,
        }
    }
}

/// When a turn started and ended, in Unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnTime {
    pub started: u64,
    pub ended: u64,
}

/// Thinking / effort level. `Default` leaves the CLI's default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effort {
    #[default]
    Default,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Effort {
    pub fn label(self) -> &'static str {
        match self {
            Effort::Default => "Default thinking",
            Effort::Low => "Low",
            Effort::Medium => "Medium",
            Effort::High => "High",
            Effort::XHigh => "X-High",
            Effort::Max => "Max",
        }
    }

    /// CLI `--effort` value, or `None` to omit the flag.
    pub fn cli(self) -> Option<&'static str> {
        match self {
            Effort::Default => None,
            Effort::Low => Some("low"),
            Effort::Medium => Some("medium"),
            Effort::High => Some("high"),
            Effort::XHigh => Some("xhigh"),
            Effort::Max => Some("max"),
        }
    }

    pub const ALL: [Effort; 6] = [
        Effort::Default,
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::XHigh,
        Effort::Max,
    ];
}

/// One rendered element of a conversation. Text and tool calls interleave in the
/// order Claude produces them. Shared by the transcript reader and every frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Item {
    User(String),
    /// The summary of Claude's reasoning before a reply or tool call.
    Thinking(String),
    Assistant(String),
    Tool(ToolCall),
    /// A message another agent sent; it starts a turn like a user's message does.
    Incoming {
        from: String,
        text: String,
    },
    /// What started a turn Claude Code began by itself, such as a background command ending.
    Notice(String),
    Compacted(Compaction),
    /// The turn before it ended.
    Ended(TurnTime),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: CallId,
    pub name: String,
    /// One line naming what the call acts on (a command, a path, a pattern), or empty.
    #[serde(default)]
    pub summary: String,
    /// What the call says it does, when its tool asks for that (Bash does).
    #[serde(default)]
    pub description: String,
    /// The whole shell command, for a tool that runs one.
    #[serde(default)]
    pub command: Option<String>,
    /// The tool's input as pretty-printed JSON.
    pub input: String,
    pub result: Option<String>,
    pub is_error: bool,
    /// Set when the call runs in the background: its result only says it started.
    #[serde(default)]
    pub background: Option<Background>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Background {
    pub status: TaskStatus,
    /// What it printed, once it ended.
    pub output: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
    Stopped,
    /// No end was recorded: it ended with the process that ran it.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    Deny,
    AllowOnce,
    /// Allow, and stop asking about the request's scope in this conversation.
    AllowAlways,
}

/// A conversation's own model, thinking and permissions, copied from `Settings` when it is
/// created, so changing the defaults leaves it as it is. A `None` field follows `Settings`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Overrides {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<Effort>,
    #[serde(default)]
    pub profile: Option<Profile>,
}

impl Overrides {
    pub fn is_empty(&self) -> bool {
        self.model.is_none() && self.effort.is_none() && self.profile.is_none()
    }
}

/// Sidebar entry for one conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationInfo {
    pub id: ConversationId,
    pub title: String,
    pub cwd: String,
    /// Its working directory is a project folder of the user's, not one claudo made.
    #[serde(default)]
    pub attached: bool,
    /// Put away by the user: listed apart, but kept whole.
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub overrides: Overrides,
    /// Context usage from this conversation's most recent turn.
    #[serde(default)]
    pub context: ContextUsage,
}

/// A Claude Code session claudo has no conversation for, such as one Claude Desktop started.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: SessionId,
    pub title: String,
    /// The folder it last ran in, where claudo continues it.
    pub cwd: String,
    /// The app that started it, such as "Claude Desktop".
    pub source: String,
    /// When it was last written, in Unix seconds.
    pub updated: u64,
}

/// The model, thinking and permissions new conversations start with. These are concrete
/// values, so the user isn't guessing what "Default" means.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// `--model` value; empty = the CLI's own configured model.
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub effort: Effort,
    #[serde(default)]
    pub profile: Profile,
}

fn default_model() -> String {
    DEFAULT_MODEL.to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            model: default_model(),
            effort: Effort::default(),
            profile: Profile::default(),
        }
    }
}

/// Frontend -> backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    GetStatus,
    GetSettings,
    /// Answered by `Models` and `Commands`.
    GetCapabilities,
    /// The subscription's usage windows now, answered with `Quota`. Turns report them too.
    GetUsage,
    UpdateSettings {
        settings: Settings,
    },
    ListConversations,
    OpenConversation {
        id: ConversationId,
    },
    SetOverrides {
        id: ConversationId,
        overrides: Overrides,
    },
    /// Delete a conversation, its scratch folder and its Claude Code transcript, stopping
    /// its agent first.
    Delete {
        id: ConversationId,
    },
    /// Put a conversation away, or bring it back; nothing of it is deleted.
    Archive {
        id: ConversationId,
        archived: bool,
    },
    /// Give a conversation a title of the user's own; its agent takes it as its name.
    Rename {
        id: ConversationId,
        title: String,
    },
    /// `request` is echoed in the `Created` that answers it, so the client knows where it goes.
    NewScratch {
        request: u64,
    },
    NewAttached {
        path: String,
        request: u64,
    },
    /// Answered by `Sessions`.
    ListSessions,
    /// Continue a session from `Sessions` as a conversation, answered by `Created`. Its first
    /// turn forks the session, so the app that started it keeps it as it was.
    Import {
        session_id: SessionId,
        request: u64,
    },
    SendMessage {
        conversation_id: ConversationId,
        text: String,
        profile: Profile,
        /// `--model` value; empty means use the CLI's configured model.
        model: String,
        effort: Effort,
    },
    /// Stop the conversation's running turn. It still ends with `Completed`.
    Stop {
        conversation_id: ConversationId,
    },
    /// Answer an `ApprovalRequested`.
    AnswerApproval {
        conversation_id: ConversationId,
        request_id: String,
        decision: Decision,
    },
}

/// Backend -> frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerEvent {
    Status(SystemStatus),
    Settings(Settings),
    Models(Vec<ModelOption>),
    Commands(Vec<CliCommand>),
    /// Full conversation list.
    Conversations {
        list: Vec<ConversationInfo>,
    },
    /// Answers `NewScratch`, `NewAttached` or `Import`, after the list that includes it.
    Created {
        conversation_id: ConversationId,
        request: u64,
        /// It continues a session that has history: open it like a listed conversation, not
        /// as a new chat.
        #[serde(default)]
        imported: bool,
    },
    /// Answers `ListSessions`, newest first.
    Sessions {
        list: Vec<SessionInfo>,
    },
    /// Reconstructed history for a conversation the client opened.
    History {
        conversation_id: ConversationId,
        items: Vec<Item>,
    },
    Started {
        conversation_id: ConversationId,
        session_id: SessionId,
    },
    /// Another agent's message started a turn the client did not send.
    Incoming {
        conversation_id: ConversationId,
        from: String,
        text: String,
    },
    Delta {
        conversation_id: ConversationId,
        text: String,
    },
    ThinkingDelta {
        conversation_id: ConversationId,
        text: String,
    },
    ToolStarted {
        conversation_id: ConversationId,
        call: ToolCall,
    },
    ToolResult {
        conversation_id: ConversationId,
        call_id: CallId,
        content: String,
        is_error: bool,
    },
    /// Claude wants to make a call that always needs the user's approval, such as
    /// opening an issue or a pull request. The turn waits for `AnswerApproval`.
    ApprovalRequested {
        conversation_id: ConversationId,
        request_id: String,
        call: ToolCall,
        /// What `Decision::AllowAlways` stops asking about, such as `gh issue create`.
        scope: String,
    },
    /// A turn Claude Code began by itself starts, for this reason.
    Notice {
        conversation_id: ConversationId,
        text: String,
    },
    /// The conversation is being summarized.
    Compacting {
        conversation_id: ConversationId,
    },
    /// A request to the API failed and the CLI is waiting to try it again.
    Retrying {
        conversation_id: ConversationId,
        attempt: u32,
        max_retries: u32,
        /// Such as "overloaded (529)".
        error: String,
    },
    Compacted {
        conversation_id: ConversationId,
        compaction: Compaction,
    },
    /// The summary of the compaction just reported.
    CompactSummary {
        conversation_id: ConversationId,
        text: String,
    },
    /// A background call started or ended, possibly between turns.
    Background {
        conversation_id: ConversationId,
        call_id: CallId,
        background: Background,
    },
    Completed {
        conversation_id: ConversationId,
        is_error: bool,
        #[serde(default)]
        context: ContextUsage,
        #[serde(default)]
        time: Option<TurnTime>,
    },
    Quota(Quota),
    Error {
        /// The conversation it concerns, or `None` for the connection or the app.
        #[serde(default)]
        conversation_id: Option<ConversationId>,
        detail: String,
    },
}
