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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: CallId,
    pub name: String,
    /// One line naming what the call acts on (a command, a path, a pattern), or empty.
    #[serde(default)]
    pub summary: String,
    /// The tool's input as pretty-printed JSON.
    pub input: String,
    pub result: Option<String>,
    pub is_error: bool,
}

/// Per-conversation overrides. Each `None` field falls back to the global
/// `Settings` value; `Some` wins for that conversation only.
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
    #[serde(default)]
    pub overrides: Overrides,
    /// Context usage from this conversation's most recent turn.
    #[serde(default)]
    pub context: ContextUsage,
}

/// Persisted global preferences applied to every turn. These are the concrete
/// defaults the app runs with, so the user isn't guessing what "Default" means.
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
    /// Answered by `Models`.
    GetModels,
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
        allow: bool,
    },
}

/// Backend -> frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerEvent {
    Status(SystemStatus),
    Settings(Settings),
    Models(Vec<ModelOption>),
    /// Full conversation list.
    Conversations {
        list: Vec<ConversationInfo>,
    },
    /// Answers `NewScratch` or `NewAttached`, after the list that includes it.
    Created {
        conversation_id: ConversationId,
        request: u64,
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
    },
    Completed {
        conversation_id: ConversationId,
        is_error: bool,
        #[serde(default)]
        context: ContextUsage,
    },
    Quota(Quota),
    Error {
        /// The conversation it concerns, or `None` for the connection or the app.
        #[serde(default)]
        conversation_id: Option<ConversationId>,
        detail: String,
    },
}
