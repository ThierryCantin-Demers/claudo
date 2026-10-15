//! Conversations: a small claudo-owned index (title, cwd, session id) plus a
//! reader for message history sourced from Claude Code's own transcripts at
//! `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`.
//!
//! We locate a transcript by its `<session-id>.jsonl` filename, so we never have
//! to reverse Claude Code's directory-name encoding.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use claudo_protocol::{Background, ContextUsage, Item, Overrides, TaskStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::engine::cli::{backgrounded, incoming, task_notification, tool_call};

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
    /// The user chose the title, so a generated one never replaces it.
    #[serde(default)]
    pub renamed: bool,
    #[serde(default)]
    pub archived: bool,
    /// Approval scopes the user chose to allow always in this conversation.
    #[serde(default)]
    pub always_allowed: Vec<String>,
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
        let mut store: Self = match fs::read_to_string(Self::path()) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        };
        for c in &mut store.conversations {
            if let Some(model) = &mut c.overrides.model {
                crate::settings::follow_latest(model);
            }
        }
        store
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
            renamed: false,
            archived: false,
            always_allowed: Vec::new(),
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

    /// Record a turn: bump `updated` and store the session id.
    pub fn record_turn(&mut self, id: &str, session_id: Option<&str>) {
        if let Some(c) = self.get_mut(id) {
            c.updated = now_secs();
            if let Some(sid) = session_id {
                c.session_id = Some(sid.to_string());
            }
        }
        self.save();
    }

    pub fn remove(&mut self, id: &str) -> Option<ConversationMeta> {
        let index = self.conversations.iter().position(|c| c.id == id)?;
        let removed = self.conversations.remove(index);
        self.save();
        Some(removed)
    }

    pub fn set_archived(&mut self, id: &str, archived: bool) {
        if let Some(c) = self.get_mut(id)
            && c.archived != archived
        {
            c.archived = archived;
            self.save();
        }
    }

    pub fn allow_always(&mut self, id: &str, scope: String) {
        if let Some(c) = self.get_mut(id)
            && !c.always_allowed.contains(&scope)
        {
            c.always_allowed.push(scope);
            self.save();
        }
    }

    pub fn rename(&mut self, id: &str, title: String) {
        if let Some(c) = self.get_mut(id) {
            c.title = title;
            c.renamed = true;
        }
        self.save();
    }

    /// The conversation's title, first taken from `prompt` if it is still the placeholder.
    pub fn title_for(&mut self, id: &str, prompt: &str) -> Option<String> {
        let c = self.get_mut(id)?;
        if c.title == "New conversation" {
            c.title = title_from_prompt(prompt);
        }
        let title = c.title.clone();
        self.save();
        Some(title)
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

/// Remove what a deleted conversation leaves on disk: its Claude Code transcript, and its
/// scratch folder, which claudo owns. An attached project folder is never touched.
pub fn delete_files(meta: &ConversationMeta) {
    if let Some(session_id) = &meta.session_id
        && let Some(transcript) = find_transcript(session_id)
    {
        let _ = fs::remove_file(&transcript);
        if let Some(project) = transcript.parent() {
            // The session's own folder holds its subagents' transcripts.
            let _ = fs::remove_dir_all(project.join(session_id));
            // A scratch conversation's project holds only its sessions; this fails if not empty.
            if !meta.attached {
                let _ = fs::remove_dir(project);
            }
        }
    }
    let scratch = data_root().join("conversations");
    if !meta.attached && meta.cwd.starts_with(&scratch) && meta.cwd != scratch {
        let _ = fs::remove_dir_all(&meta.cwd);
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
            Some("user") if let Some((from, text)) = incoming(&v) => {
                items.push(Item::Incoming { from, text });
            }
            Some("user") => match v.pointer("/message/content") {
                Some(Value::String(s)) => match task_notification(s) {
                    Some(end) => {
                        if let Some(call) = call_mut(&mut items, &end.call_id) {
                            call.background = Some(end.background);
                        }
                        items.push(Item::Notice(end.summary));
                    }
                    None => items.push(Item::User(s.clone())),
                },
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
                            Some("thinking") => {
                                if let Some(t) = b.get("thinking").and_then(Value::as_str)
                                    && !t.trim().is_empty()
                                {
                                    items.push(Item::Thinking(t.trim_end().to_string()));
                                }
                            }
                            Some("tool_use") => items.push(Item::Tool(tool_call(b))),
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }

    (items, ai_title)
}

fn call_mut<'a>(items: &'a mut [Item], call_id: &str) -> Option<&'a mut claudo_protocol::ToolCall> {
    items.iter_mut().rev().find_map(|item| match item {
        Item::Tool(call) if call.call_id == call_id => Some(call),
        _ => None,
    })
}

fn attach_result(items: &mut [Item], call_id: &str, result: String, is_error: bool) {
    if let Some(tc) = call_mut(items, call_id) {
        if backgrounded(&result) {
            tc.background = Some(Background {
                status: TaskStatus::Unknown,
                output: None,
            });
        }
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
