//! Conversations: a small claudo-owned index (title, cwd, session id) plus a
//! reader for message history sourced from Claude Code's own transcripts at
//! `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`.
//!
//! We locate a transcript by its `<session-id>.jsonl` filename, so we never have
//! to reverse Claude Code's directory-name encoding.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use claudo_protocol::{ContextUsage, Item, Overrides, ToolCall};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// claudo's data root, e.g. `~/.local/share/claudo`.
pub fn data_root() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("claudo")
}

fn claude_projects_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude").join("projects"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One conversation in claudo's index. Message history is not stored here; it is
/// read from Claude Code's transcript on demand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMeta {
    pub id: String,
    pub title: String,
    pub cwd: PathBuf,
    /// True once a project folder was attached (vs a scratch dir we own).
    #[serde(default)]
    pub attached: bool,
    pub session_id: Option<String>,
    #[serde(default)]
    pub overrides: Overrides,
    /// Context usage from the most recent turn.
    #[serde(default)]
    pub context: ContextUsage,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub updated: u64,
}

/// The persisted list of conversations (`~/.local/share/claudo/conversations.json`).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    pub conversations: Vec<ConversationMeta>,
}

impl Store {
    fn path() -> PathBuf {
        data_root().join("conversations.json")
    }

    pub fn load() -> Self {
        match fs::read_to_string(Self::path()) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        if let Some(parent) = Self::path().parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write(Self::path(), json);
        }
    }

    pub fn get(&self, id: &str) -> Option<&ConversationMeta> {
        self.conversations.iter().find(|c| c.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut ConversationMeta> {
        self.conversations.iter_mut().find(|c| c.id == id)
    }

    /// Create a scratch conversation with a dedicated working directory we own.
    pub fn create_scratch(&mut self) -> String {
        let id = Uuid::new_v4().to_string();
        let cwd = data_root().join("conversations").join(&id);
        let _ = fs::create_dir_all(&cwd);
        self.push_new(id.clone(), cwd, false, "New conversation".to_string());
        id
    }

    /// Create a conversation whose working directory is an existing project folder.
    pub fn create_attached(&mut self, folder: PathBuf) -> String {
        let id = Uuid::new_v4().to_string();
        let title = folder
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| format!("📁 {n}"))
            .unwrap_or_else(|| "New conversation".to_string());
        self.push_new(id.clone(), folder, true, title);
        id
    }

    fn push_new(&mut self, id: String, cwd: PathBuf, attached: bool, title: String) {
        let now = now_secs();
        self.conversations.push(ConversationMeta {
            id,
            title,
            cwd,
            attached,
            session_id: None,
            overrides: Overrides::default(),
            context: ContextUsage::default(),
            created: now,
            updated: now,
        });
        self.save();
    }

    pub fn set_context(&mut self, id: &str, context: ContextUsage) {
        if let Some(c) = self.get_mut(id) {
            c.context = context;
        }
        self.save();
    }

    pub fn set_overrides(&mut self, id: &str, overrides: Overrides) {
        if let Some(c) = self.get_mut(id) {
            c.overrides = overrides;
        }
        self.save();
    }

    /// Record a completed turn: bump `updated`, store the session id, and set a
    /// title from the first prompt if it is still the placeholder.
    pub fn record_turn(&mut self, id: &str, session_id: Option<&str>, first_prompt: &str) {
        if let Some(c) = self.get_mut(id) {
            c.updated = now_secs();
            if let Some(sid) = session_id {
                c.session_id = Some(sid.to_string());
            }
            if c.title == "New conversation" {
                c.title = title_from_prompt(first_prompt);
            }
        }
        self.save();
    }

    /// Conversations sorted most-recently-updated first (for the sidebar).
    pub fn ordered(&self) -> Vec<ConversationMeta> {
        let mut v = self.conversations.clone();
        v.sort_by_key(|c| std::cmp::Reverse(c.updated));
        v
    }
}

fn title_from_prompt(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or("").trim();
    let truncated: String = line.chars().take(48).collect();
    if truncated.is_empty() {
        "New conversation".to_string()
    } else if line.chars().count() > 48 {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// Load a conversation's history (as shared `protocol::Item`s) and Claude Code's
/// AI-generated title (if any) from its transcript. Empty if the session has no
/// transcript yet.
pub fn load_transcript(session_id: &str) -> (Vec<Item>, Option<String>) {
    let Some(path) = find_transcript(session_id) else {
        return (Vec::new(), None);
    };
    parse_transcript(&path)
}

fn find_transcript(session_id: &str) -> Option<PathBuf> {
    let projects = claude_projects_dir()?;
    let target = format!("{session_id}.jsonl");
    for entry in fs::read_dir(&projects).ok()?.flatten() {
        let dir = entry.path();
        if dir.is_dir() {
            let file = dir.join(&target);
            if file.is_file() {
                return Some(file);
            }
        }
    }
    None
}

fn parse_transcript(path: &Path) -> (Vec<Item>, Option<String>) {
    let Ok(content) = fs::read_to_string(path) else {
        return (Vec::new(), None);
    };
    let mut items: Vec<Item> = Vec::new();
    let mut ai_title = None;

    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("ai-title") => {
                ai_title = v.get("aiTitle").and_then(Value::as_str).map(str::to_owned);
            }
            Some("user") => match v.pointer("/message/content") {
                Some(Value::String(s)) => items.push(Item::User(s.clone())),
                Some(Value::Array(arr)) => {
                    for b in arr {
                        match b.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(t) = b.get("text").and_then(Value::as_str) {
                                    items.push(Item::User(t.to_string()));
                                }
                            }
                            Some("tool_result") => attach_result(
                                &mut items,
                                b.get("tool_use_id").and_then(Value::as_str).unwrap_or(""),
                                tool_result_text(b.get("content")),
                                b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                            ),
                            _ => {}
                        }
                    }
                }
                _ => {}
            },
            Some("assistant") => {
                if let Some(Value::Array(arr)) = v.pointer("/message/content") {
                    for b in arr {
                        match b.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(t) = b.get("text").and_then(Value::as_str)
                                    && !t.is_empty()
                                {
                                    items.push(Item::Assistant(t.to_string()));
                                }
                            }
                            Some("tool_use") => items.push(Item::Tool(ToolCall {
                                call_id: b
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                name: b
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("tool")
                                    .to_string(),
                                input: b
                                    .get("input")
                                    .map(|i| serde_json::to_string_pretty(i).unwrap_or_default())
                                    .unwrap_or_default(),
                                result: None,
                                is_error: false,
                            })),
                            _ => {} // skip thinking
                        }
                    }
                }
            }
            _ => {}
        }
    }

    (items, ai_title)
}

fn attach_result(items: &mut [Item], call_id: &str, result: String, is_error: bool) {
    if let Some(Item::Tool(tc)) = items
        .iter_mut()
        .rev()
        .find(|it| matches!(it, Item::Tool(t) if t.call_id == call_id))
    {
        tc.result = Some(result);
        tc.is_error = is_error;
    }
}

/// `tool_result.content` is either a string or an array of text blocks.
fn tool_result_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}
