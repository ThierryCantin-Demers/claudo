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

/// A `--model` value: an alias (`opus`) or a specific version
/// (`claude-sonnet-5-5`). Empty string means "use the CLI's configured model".
/// The frontend offers presets plus a free-text field so any version works.
pub const MODEL_PRESETS: &[(&str, &str)] = &[
    ("Opus 5.5 (default)", DEFAULT_MODEL),
    ("Sonnet 5.5", "claude-sonnet-5-5"),
    ("Haiku 5.5", "claude-haiku-5-5"),
    ("Fable 5.1", "claude-fable-5-1"),
    ("Opus (latest alias)", "opus"),
    ("Sonnet (latest alias)", "sonnet"),
    ("Haiku (latest alias)", "haiku"),
    ("Fable (latest alias)", "fable"),
    ("CLI default (unset)", ""),
];

/// Model used when the user hasn't chosen one.
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";

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
    Assistant(String),
    Tool(ToolCall),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: CallId,
    pub name: String,
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
    NewScratch,
    NewAttached {
        path: String,
    },
    SendMessage {
        conversation_id: ConversationId,
        text: String,
        profile: Profile,
        /// `--model` value; empty means use the CLI's configured model.
        model: String,
        effort: Effort,
    },
}

/// Backend -> frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerEvent {
    Status(SystemStatus),
    Settings(Settings),
    /// Full conversation list. `current` is set only when the backend just
    /// created/selected one (e.g. after `NewScratch`).
    Conversations {
        list: Vec<ConversationInfo>,
        current: Option<ConversationId>,
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
    Delta {
        conversation_id: ConversationId,
        text: String,
    },
    ToolStarted {
        conversation_id: ConversationId,
        call_id: CallId,
        name: String,
        input: String,
    },
    ToolResult {
        conversation_id: ConversationId,
        call_id: CallId,
        content: String,
        is_error: bool,
    },
    Completed {
        conversation_id: ConversationId,
        is_error: bool,
        #[serde(default)]
        context: ContextUsage,
    },
    Error {
        detail: String,
    },
}
