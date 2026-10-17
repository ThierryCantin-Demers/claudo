//! Conversations: a small claudo-owned index (title, cwd, session id) plus a
//! reader for message history sourced from Claude Code's own transcripts at
//! `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`.
//!
//! We locate a transcript by its `<session-id>.jsonl` filename, so we never have
//! to reverse Claude Code's directory-name encoding.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use claudo_protocol::{
    Background, ContextUsage, Item, Overrides, SessionInfo, Settings, TaskStatus, TurnTime,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::engine::cli::{
    attachment, backgrounded, command_echo, command_output, compaction, incoming,
    task_notification, tool_call, unix_seconds,
};

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

pub(crate) fn now_secs() -> u64 {
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
    /// The other app's session it was imported from. Until a turn forks it, `session_id` is
    /// still that session, which claudo must not change or delete.
    #[serde(default)]
    pub imported_from: Option<String>,
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

impl ConversationMeta {
    /// Its session is still another app's, so the next process forks it instead of resuming it.
    pub fn borrowed(&self) -> bool {
        self.imported_from.is_some() && self.imported_from == self.session_id
    }
}

/// The persisted list of conversations (`~/.local/share/claudo/conversations.json`).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    pub conversations: Vec<ConversationMeta>,
    /// Which one-time migrations the file has had; see `STORE_VERSION`.
    #[serde(default)]
    pub version: u32,
}

/// 1: conversations that followed the defaults keep them as they were, now that the
/// defaults apply to new conversations only.
const STORE_VERSION: u32 = 1;

impl Store {
    fn path() -> PathBuf {
        data_root().join("conversations.json")
    }

    pub fn load() -> Self {
        // An unreadable file is left alone rather than migrated over.
        let (mut store, readable): (Self, bool) = match fs::read_to_string(Self::path()) {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(store) => (store, true),
                Err(_) => (Self::default(), false),
            },
            Err(_) => (Self::default(), true),
        };
        for c in &mut store.conversations {
            if let Some(model) = &mut c.overrides.model {
                crate::settings::follow_latest(model);
            }
        }
        if readable && store.version < STORE_VERSION {
            store.keep_defaults(&crate::settings::load());
            store.version = STORE_VERSION;
            store.save();
        }
        store
    }

    /// Give every conversation the defaults it follows today as its own.
    fn keep_defaults(&mut self, settings: &Settings) {
        for c in &mut self.conversations {
            fill(&mut c.overrides, settings);
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
        let cwd = scratch_dir(&id);
        let _ = fs::create_dir_all(&cwd);
        self.push_new(id.clone(), cwd, false, "New conversation".to_string());
        id
    }

    /// Create a conversation whose working directory is an existing project folder.
    pub fn create_attached(&mut self, folder: PathBuf) -> String {
        let id = Uuid::new_v4().to_string();
        self.push_new(id.clone(), folder, true, "New conversation".to_string());
        id
    }

    /// Move a conversation that has not started into `folder`, or back into its scratch one.
    pub fn set_folder(&mut self, id: &str, folder: Option<PathBuf>) -> Result<(), String> {
        let c = self.get_mut(id).ok_or("that conversation is gone")?;
        if c.session_id.is_some() {
            return Err("a conversation stays in the folder it started in".to_string());
        }
        let scratch = scratch_dir(id);
        match folder {
            Some(folder) => {
                if !c.attached {
                    let _ = fs::remove_dir(&scratch);
                }
                c.cwd = folder;
                c.attached = true;
            }
            None => {
                let _ = fs::create_dir_all(&scratch);
                c.cwd = scratch;
                c.attached = false;
            }
        }
        self.save();
        Ok(())
    }

    /// The project folders conversations ran in, with when each last did.
    pub fn folders(&self) -> Vec<(u64, PathBuf)> {
        self.conversations
            .iter()
            .filter(|c| c.attached)
            .map(|c| (c.updated, c.cwd.clone()))
            .collect()
    }

    fn push_new(&mut self, id: String, cwd: PathBuf, attached: bool, title: String) {
        let now = now_secs();
        let mut overrides = Overrides::default();
        fill(&mut overrides, &crate::settings::load());
        self.conversations.push(ConversationMeta {
            id,
            title,
            cwd,
            attached,
            renamed: false,
            archived: false,
            always_allowed: Vec::new(),
            session_id: None,
            imported_from: None,
            overrides,
            context: ContextUsage::default(),
            created: now,
            updated: now,
        });
        self.save();
    }

    /// Add a conversation continuing another app's session, or find the one that already does.
    pub fn import(&mut self, session_id: &str) -> Result<String, String> {
        if let Some(c) = self.conversations.iter().find(|c| {
            c.imported_from.as_deref() == Some(session_id)
                || c.session_id.as_deref() == Some(session_id)
        }) {
            return Ok(c.id.clone());
        }
        let session = find_transcript(session_id)
            .and_then(|path| summarize(&path))
            .ok_or_else(|| "that session can no longer be continued".to_string())?;
        let id = Uuid::new_v4().to_string();
        self.push_new(id.clone(), session.cwd.into(), true, session.title);
        if let Some(c) = self.get_mut(&id) {
            c.session_id = Some(session_id.to_string());
            c.imported_from = Some(session_id.to_string());
        }
        self.save();
        Ok(id)
    }

    /// Every session a conversation continues or was imported from.
    pub fn session_ids(&self) -> HashSet<String> {
        self.conversations
            .iter()
            .flat_map(|c| [c.session_id.clone(), c.imported_from.clone()])
            .flatten()
            .collect()
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

fn scratch_dir(id: &str) -> PathBuf {
    data_root().join("conversations").join(id)
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
        && !meta.borrowed()
        && let Some(transcript) = find_transcript(session_id)
    {
        let _ = fs::remove_file(&transcript);
        if let Some(project) = transcript.parent() {
            // The session's own folder holds its subagents' transcripts.
            let _ = fs::remove_dir_all(project.join(session_id));
            // A scratch conversation's project holds only its sessions and the CLI's memory
            // folder; neither removal happens unless the folder is empty.
            if !meta.attached {
                let _ = fs::remove_dir(project.join("memory"));
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

/// How many folders `recent_folders` lists.
const RECENT_FOLDERS: usize = 8;

/// Folders to start a conversation in, newest first: `known`, which conversations ran in,
/// and those Claude Code sessions ran in, whichever app started them.
pub fn recent_folders(mut known: Vec<(u64, PathBuf)>) -> Vec<String> {
    let projects = claude_projects_dir()
        .and_then(|dir| fs::read_dir(dir).ok())
        .into_iter()
        .flatten()
        .flatten();
    known.extend(projects.filter_map(|project| {
        let (modified, newest) = fs::read_dir(project.path())
            .ok()?
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|e| e == "jsonl"))
            .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
            .max()?;
        let seconds = modified.duration_since(UNIX_EPOCH).ok()?.as_secs();
        Some((seconds, transcript_cwd(&newest)?))
    }));
    rank_folders(known, &[data_root(), std::env::temp_dir()])
}

/// The folder a transcript's session started in, from the first line that names one.
fn transcript_cwd(path: &Path) -> Option<PathBuf> {
    use std::io::BufRead;
    std::io::BufReader::new(fs::File::open(path).ok()?)
        .lines()
        .map_while(Result::ok)
        .filter(|line| line.contains("\"cwd\""))
        .find_map(|line| {
            let v: Value = serde_json::from_str(&line).ok()?;
            Some(PathBuf::from(v.get("cwd")?.as_str()?))
        })
}

/// Existing folders newest first, once each, leaving out those under `excluded`: claudo's
/// scratch folders and temporary ones are nobody's project.
fn rank_folders(mut folders: Vec<(u64, PathBuf)>, excluded: &[PathBuf]) -> Vec<String> {
    folders.sort_by_key(|(when, _)| Reverse(*when));
    let mut seen = HashSet::new();
    folders
        .into_iter()
        .map(|(_, folder)| folder)
        .filter(|folder| !excluded.iter().any(|root| folder.starts_with(root)))
        .filter(|folder| folder.is_dir() && seen.insert(folder.clone()))
        .take(RECENT_FOLDERS)
        .map(|folder| folder.display().to_string())
        .collect()
}

/// How many sessions `discover` lists.
const DISCOVERED: usize = 50;

/// Claude Code sessions no conversation continues, newest first. Headless runs, claudo's own
/// among them, are left out.
pub fn discover(known: &HashSet<String>) -> Vec<SessionInfo> {
    let Some(projects) = claude_projects_dir() else {
        return Vec::new();
    };
    let mut transcripts: Vec<(SystemTime, PathBuf)> = fs::read_dir(projects)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|project| fs::read_dir(project.path()).ok())
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let unknown =
                path.extension()? == "jsonl" && !known.contains(path.file_stem()?.to_str()?);
            unknown.then_some((entry.metadata().ok()?.modified().ok()?, path))
        })
        .collect();
    transcripts.sort_by_key(|(modified, _)| Reverse(*modified));
    let mut listed = HashSet::new();
    transcripts
        .iter()
        .filter(|(_, path)| listed.insert(path.file_stem()))
        .filter_map(|(_, path)| summarize(path))
        .take(DISCOVERED)
        .collect()
}

fn summarize(path: &Path) -> Option<SessionInfo> {
    let transcript = fs::read_to_string(path).ok()?;
    let updated = fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    summary(path.file_stem()?.to_str()?, &transcript, updated)
}

/// What the session list shows of a transcript, or `None` for one not worth continuing: a
/// headless run, a subagent's, one with no prompt, or one whose folder is gone.
fn summary(session_id: &str, transcript: &str, updated: u64) -> Option<SessionInfo> {
    let parse = |line: &str| serde_json::from_str::<Value>(line).ok();
    let first = transcript
        .lines()
        .filter(|line| line.contains("\"entrypoint\""))
        .find_map(parse)?;
    let entrypoint = first.get("entrypoint")?.as_str()?;
    if entrypoint.starts_with("sdk") || flag(&first, "isSidechain") {
        return None;
    }
    let last = |kind: &str, key: &str| {
        transcript
            .lines()
            .rev()
            .filter(|line| line.contains(kind))
            .filter_map(parse)
            .find(|v| v.get("type").and_then(Value::as_str) == Some(kind))
            .and_then(|v| v.get(key)?.as_str().map(str::to_owned))
    };
    let title = last("custom-title", "customTitle")
        .or_else(|| last("ai-title", "aiTitle"))
        .or_else(|| first_prompt(transcript))?;
    // A session that entered a worktree records where it went, and continues there.
    let cwd = transcript
        .lines()
        .rev()
        .filter(|line| line.contains("\"cwd\"") || line.contains("\"relocatedCwd\""))
        .filter_map(parse)
        .find_map(|v| {
            let cwd = v.get("relocatedCwd").or_else(|| v.get("cwd"))?;
            cwd.as_str().map(str::to_owned)
        })?;
    if !Path::new(&cwd).is_dir() {
        return None;
    }
    Some(SessionInfo {
        session_id: session_id.to_string(),
        title,
        cwd,
        source: match entrypoint {
            "claude-desktop" => "Claude Desktop",
            "cli" => "Claude Code",
            other => other,
        }
        .to_string(),
        updated,
    })
}

/// The first thing the user typed, as a title.
fn first_prompt(transcript: &str) -> Option<String> {
    transcript
        .lines()
        .filter(|line| line.contains("\"user\""))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|v| v.get("type").and_then(Value::as_str) == Some("user") && !flag(v, "isMeta"))
        .find_map(|v| {
            let text = match v.pointer("/message/content")? {
                Value::String(text) => text.as_str(),
                Value::Array(blocks) => blocks.iter().find_map(|b| b.get("text")?.as_str())?,
                _ => return None,
            }
            .trim();
            // Command echoes and caveats are wrapped in tags; a prompt is not.
            (!text.is_empty() && !text.starts_with('<')).then(|| title_from_prompt(text))
        })
}

/// A session that moved into a worktree leaves a copy behind; the newest is the one in use.
fn find_transcript(session_id: &str) -> Option<PathBuf> {
    let projects = claude_projects_dir()?;
    let target = format!("{session_id}.jsonl");
    fs::read_dir(&projects)
        .ok()?
        .flatten()
        .map(|entry| entry.path().join(&target))
        .filter_map(|file| Some((fs::metadata(&file).ok()?.modified().ok()?, file)))
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, file)| file)
}

fn parse_transcript(path: &Path) -> (Vec<Item>, Option<String>) {
    let Ok(content) = fs::read_to_string(path) else {
        return (Vec::new(), None);
    };
    let mut items: Vec<Item> = Vec::new();
    let mut ai_title = None;
    let mut turn: Option<TurnTime> = None;

    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let pushed = items.len();
        let kind = v.get("type").and_then(Value::as_str);
        match kind {
            Some("ai-title") => {
                ai_title = v.get("aiTitle").and_then(Value::as_str).map(str::to_owned);
            }
            Some("system")
                if v.get("subtype").and_then(Value::as_str) == Some("compact_boundary") =>
            {
                items.extend(
                    compaction(v.get("compactMetadata"), "preTokens", "postTokens")
                        .map(Item::Compacted),
                );
            }
            // A local command and what it printed, a forked skill's result among them.
            Some("system") if v.get("subtype").and_then(Value::as_str) == Some("local_command") => {
                let text = v.get("content").and_then(Value::as_str).unwrap_or("");
                if let Some(command) = command_echo(text) {
                    items.push(Item::User(command));
                } else if let Some(output) = command_output(text)
                    && !output.is_empty()
                {
                    items.push(Item::Assistant(output.to_string()));
                }
            }
            // What Claude Code tells the model around a slash command, not the user's words.
            Some("user") if flag(&v, "isMeta") => {}
            Some("user") if flag(&v, "isCompactSummary") => {
                let summary = v.pointer("/message/content").and_then(Value::as_str);
                if let (Some(Item::Compacted(compacted)), Some(summary)) = (
                    items
                        .iter_mut()
                        .rev()
                        .find(|i| matches!(i, Item::Compacted(_))),
                    summary,
                ) {
                    compacted.summary = summary.to_string();
                }
            }
            Some("user") if let Some((from, text)) = incoming(&v) => {
                items.push(Item::Incoming { from, text });
            }
            Some("user") => match v.pointer("/message/content") {
                Some(Value::String(s)) if let Some(command) = command_echo(s) => {
                    items.push(Item::User(command));
                }
                Some(Value::String(s)) if let Some(output) = command_output(s) => {
                    if !output.is_empty() {
                        items.push(Item::Assistant(output.to_string()));
                    }
                }
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
                            Some("image" | "document") => {
                                items.extend(attachment(b).map(Item::File));
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
        let Some(at) = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(unix_seconds)
        else {
            continue;
        };
        let starts_turn = items[pushed..].iter().any(|item| {
            matches!(
                item,
                Item::User(_) | Item::Incoming { .. } | Item::Notice(_) | Item::File(_)
            )
        });
        if starts_turn {
            if let Some(ended) = turn.take() {
                items.insert(pushed, Item::Ended(ended));
            }
            turn = Some(TurnTime {
                started: at,
                ended: at,
            });
        } else if let Some(turn) = &mut turn
            // Only a reply moves a turn's end: the lines between turns, such as a queued
            // message's, are written when the next one starts.
            && (kind == Some("assistant")
                || items[pushed..].iter().any(|item| matches!(item, Item::Assistant(_))))
        {
            turn.ended = at;
        }
    }
    items.extend(turn.map(Item::Ended));

    (items, ai_title)
}

fn flag(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool) == Some(true)
}

/// The defaults, wherever a conversation has no value of its own.
fn fill(overrides: &mut Overrides, settings: &Settings) {
    overrides
        .model
        .get_or_insert_with(|| settings.model.clone());
    overrides.effort.get_or_insert(settings.effort);
    overrides.profile.get_or_insert(settings.profile);
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

#[cfg(test)]
mod tests {
    use super::*;
    use claudo_protocol::{Effort, Profile};

    #[test]
    fn conversations_keep_the_defaults_they_followed() {
        let conversation = |overrides| ConversationMeta {
            id: "c".to_string(),
            title: String::new(),
            cwd: PathBuf::new(),
            attached: false,
            renamed: false,
            archived: false,
            always_allowed: Vec::new(),
            session_id: None,
            imported_from: None,
            overrides,
            context: ContextUsage::default(),
            created: 0,
            updated: 0,
        };
        let mut store = Store {
            conversations: vec![
                conversation(Overrides::default()),
                conversation(Overrides {
                    model: Some("haiku".to_string()),
                    ..Overrides::default()
                }),
            ],
            version: 0,
        };
        let settings = Settings {
            model: "opus".to_string(),
            effort: Effort::High,
            profile: Profile::FullAuto,
        };
        store.keep_defaults(&settings);
        let [following, own] = &store.conversations[..] else {
            unreachable!()
        };
        assert_eq!(following.overrides.model.as_deref(), Some("opus"));
        assert_eq!(following.overrides.effort, Some(Effort::High));
        assert_eq!(
            own.overrides.model.as_deref(),
            Some("haiku"),
            "its own choice stays"
        );
        assert_eq!(own.overrides.profile, Some(Profile::FullAuto));
    }

    fn line(v: Value) -> String {
        v.to_string() + "\n"
    }

    fn prompt(cwd: &str, entrypoint: &str, content: Value) -> String {
        line(serde_json::json!({
            "type": "user", "cwd": cwd, "entrypoint": entrypoint, "isSidechain": false,
            "message": { "role": "user", "content": content },
        }))
    }

    #[test]
    fn a_session_continues_where_it_moved_under_the_title_it_was_given() {
        let (start, worktree) = (std::env::temp_dir(), env!("CARGO_MANIFEST_DIR"));
        let start = start.to_str().unwrap();
        let transcript = [
            prompt(start, "claude-desktop", "fix the build".into()),
            line(serde_json::json!({ "type": "ai-title", "aiTitle": "Build fix" })),
            line(serde_json::json!({ "type": "custom-title", "customTitle": "Old name" })),
            line(serde_json::json!({ "type": "custom-title", "customTitle": "CI repair" })),
            line(serde_json::json!({ "type": "relocated", "relocatedCwd": worktree })),
        ]
        .concat();
        let session = summary("s", &transcript, 7).expect("listed");
        assert_eq!(session.title, "CI repair");
        assert_eq!(session.cwd, worktree);
        assert_eq!(session.source, "Claude Desktop");
        assert_eq!(session.updated, 7);
    }

    #[test]
    fn an_untitled_session_is_named_after_what_was_typed() {
        let cwd = std::env::temp_dir();
        let cwd = cwd.to_str().unwrap();
        let transcript = [
            prompt(cwd, "cli", "<command-name>/model</command-name>".into()),
            prompt(
                cwd,
                "cli",
                serde_json::json!([{ "type": "text", "text": "why is CI red" }]),
            ),
        ]
        .concat();
        assert_eq!(summary("s", &transcript, 0).unwrap().title, "why is CI red");
    }

    #[test]
    fn headless_runs_and_sessions_whose_folder_is_gone_are_not_listed() {
        let cwd = std::env::temp_dir();
        let headless = prompt(cwd.to_str().unwrap(), "sdk-cli", "hi".into());
        assert!(summary("s", &headless, 0).is_none());
        let gone = prompt("/no/such/folder", "cli", "hi".into());
        assert!(summary("s", &gone, 0).is_none());
    }

    #[test]
    fn each_turn_ends_with_when_it_started_and_its_last_reply() {
        let at = |s: u32| format!("2026-10-17T15:{:02}:{:02}.000Z", s / 60, s % 60);
        let prompt = |s, text: &str| {
            line(
                serde_json::json!({ "type": "user", "timestamp": at(s), "message": { "content": text } }),
            )
        };
        let reply = |s| {
            line(
                serde_json::json!({ "type": "assistant", "timestamp": at(s), "message": { "content": [{ "type": "text", "text": "ok" }] } }),
            )
        };
        let queued = line(serde_json::json!({ "type": "queue-operation", "timestamp": at(700) }));
        let forked = line(serde_json::json!({
            "type": "system", "subtype": "local_command", "timestamp": at(900),
            "content": "<local-command-stdout>14 findings</local-command-stdout>",
        }));
        let transcript = [
            prompt(0, "one"),
            reply(5),
            reply(70),
            queued,
            prompt(800, "two"),
            reply(830),
            forked,
        ]
        .concat();
        let path = std::env::temp_dir().join(format!("claudo-turns-{}.jsonl", Uuid::new_v4()));
        fs::write(&path, transcript).unwrap();
        let (items, _) = parse_transcript(&path);
        let _ = fs::remove_file(&path);
        let ended: Vec<(u64, u64)> = items
            .iter()
            .filter_map(|item| match item {
                Item::Ended(t) => Some((t.started, t.ended)),
                _ => None,
            })
            .collect();
        let base = unix_seconds(&at(0)).unwrap();
        assert_eq!(ended, [(base, base + 70), (base + 800, base + 900)]);
        assert!(
            items
                .iter()
                .any(|i| matches!(i, Item::Assistant(t) if t == "14 findings")),
            "a forked skill's result is its reply"
        );
        assert!(
            matches!(items[3], Item::Ended(_)),
            "between the first turn and the second"
        );
    }

    #[test]
    fn an_imported_session_is_claudos_once_a_turn_forks_it() {
        let mut imported = ConversationMeta {
            id: "c".to_string(),
            title: String::new(),
            cwd: PathBuf::new(),
            attached: true,
            renamed: false,
            archived: false,
            always_allowed: Vec::new(),
            session_id: Some("desktop".to_string()),
            imported_from: Some("desktop".to_string()),
            overrides: Overrides::default(),
            context: ContextUsage::default(),
            created: 0,
            updated: 0,
        };
        assert!(imported.borrowed());
        imported.session_id = Some("fork".to_string());
        assert!(!imported.borrowed());
        let store = Store {
            conversations: vec![imported],
            version: STORE_VERSION,
        };
        assert!(store.session_ids().contains("desktop"), "not offered again");
    }

    #[test]
    fn recent_folders_are_existing_projects_newest_first_once_each() {
        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace = crate_dir.parent().unwrap().to_path_buf();
        let ranked = rank_folders(
            vec![
                (1, workspace.clone()),
                (5, crate_dir.join("src")),
                (3, crate_dir.clone()),
                (4, PathBuf::from("/no/such/folder")),
                (9, crate_dir.join("src")),
                (8, std::env::temp_dir()),
            ],
            &[std::env::temp_dir()],
        );
        let expected = [crate_dir.join("src"), crate_dir, workspace];
        let expected: Vec<String> = expected.iter().map(|f| f.display().to_string()).collect();
        assert_eq!(ranked, expected);
    }

    #[test]
    fn a_transcript_names_the_folder_its_session_started_in() {
        let path = std::env::temp_dir().join(format!("claudo-cwd-{}.jsonl", Uuid::new_v4()));
        let lines = [
            r#"{"type":"summary","summary":"x"}"#,
            r#"{"type":"user","cwd":"/home/me/project","message":{"content":"hi"}}"#,
            r#"{"type":"user","cwd":"/home/me/elsewhere"}"#,
        ];
        fs::write(&path, lines.join("\n")).unwrap();
        let cwd = transcript_cwd(&path);
        let _ = fs::remove_file(&path);
        assert_eq!(cwd, Some(PathBuf::from("/home/me/project")));
    }
}
