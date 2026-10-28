//! Conversations: a small claudo-owned index (title, cwd, session id) plus a
//! reader for message history sourced from Claude Code's own transcripts at
//! `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`.
//!
//! We locate a transcript by its `<session-id>.jsonl` filename, so we never have
//! to reverse Claude Code's directory-name encoding.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use claudo_protocol::{
    Background, ContextUsage, Item, Overrides, SessionInfo, Settings, TaskStatus, ToolCall,
    TurnTime,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::engine::cli::{
    agent_call, attachment, backgrounded, command_echo, command_output, compaction, incoming,
    reported_review, review, task_notification, tool_call, unix_millis,
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
    now_millis() / 1000
}

pub(crate) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One conversation in claudo's index. Message history is not stored here; it is
/// read from Claude Code's transcript on demand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Conversations deleted here, which a save must not take back from the file.
    #[serde(skip)]
    removed: HashSet<String>,
    /// The list as this claudo last read or wrote it, which tells its own changes from
    /// another's.
    #[serde(skip)]
    synced: Vec<ConversationMeta>,
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
            Ok(s) => match serde_json::from_str::<Self>(&s) {
                Ok(mut store) => {
                    store.synced = store.conversations.clone();
                    (store, true)
                }
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
        let scratch = data_root().join("conversations");
        if readable
            && let Some(projects) = claude_projects_dir()
            && store.recover(&projects, &scratch)
        {
            store.save();
        }
        store
    }

    /// Take back the scratch conversations missing from the list whose folder and transcript
    /// are still there. Returns whether there were any.
    fn recover(&mut self, projects: &Path, scratch: &Path) -> bool {
        let mut recovered = false;
        for project in fs::read_dir(projects).into_iter().flatten().flatten() {
            let Some((modified, transcript)) = newest_transcript(&project.path()) else {
                continue;
            };
            let Some(cwd) = transcript_cwd(&transcript) else {
                continue;
            };
            let id = match cwd.strip_prefix(scratch).ok().and_then(Path::to_str) {
                Some(id) if !id.is_empty() && !id.contains('/') => id.to_string(),
                _ => continue,
            };
            if self.get(&id).is_some() || self.removed.contains(&id) || !cwd.is_dir() {
                continue;
            }
            let Some(session_id) = transcript.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let (items, ai_title) = parse_transcript(&transcript);
            let prompt = items.iter().find_map(|item| match item {
                Item::User(text) => Some(title_from_prompt(text)),
                _ => None,
            });
            let updated = modified
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let mut overrides = Overrides::default();
            fill(&mut overrides, &crate::settings::load());
            self.conversations.push(ConversationMeta {
                id,
                title: ai_title
                    .or(prompt)
                    .unwrap_or_else(|| "New conversation".to_string()),
                cwd,
                attached: false,
                renamed: false,
                archived: false,
                always_allowed: Vec::new(),
                session_id: Some(session_id.to_string()),
                imported_from: None,
                overrides,
                context: ContextUsage::default(),
                created: updated,
                updated,
            });
            recovered = true;
        }
        recovered
    }

    /// Give every conversation the defaults it follows today as its own.
    fn keep_defaults(&mut self, settings: &Settings) {
        for c in &mut self.conversations {
            fill(&mut c.overrides, settings);
        }
    }

    /// Write the list, keeping what another claudo wrote to the file since: each writes the
    /// whole list, so neither may drop or undo the other's changes.
    pub fn save(&mut self) {
        let path = Self::path();
        if let Some(on_disk) = fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Store>(&s).ok())
        {
            self.adopt(on_disk.conversations);
        }
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self)
            && fs::write(path, json).is_ok()
        {
            self.synced = self.conversations.clone();
        }
    }

    /// Take in the conversations another claudo added, and its version of those this one
    /// left as they were synced.
    fn adopt(&mut self, conversations: Vec<ConversationMeta>) {
        for c in conversations {
            if self.removed.contains(&c.id) {
                continue;
            }
            let synced = self.synced.iter().find(|s| s.id == c.id);
            match self.conversations.iter_mut().find(|mine| mine.id == c.id) {
                None => self.conversations.push(c),
                Some(mine) if synced == Some(&*mine) => *mine = c,
                Some(_) => {}
            }
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
        self.removed.insert(removed.id.clone());
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
    let (items, title) = parse_transcript(&path);
    let subagents = path.with_extension("").join("subagents");
    (with_subagents(with_reviews(items), &subagents), title)
}

/// Code reviews reported with a call in place of the call, without those the CLI refused.
fn with_reviews(items: Vec<Item>) -> Vec<Item> {
    items
        .into_iter()
        .filter_map(|item| match item {
            Item::Tool(call) => match reported_review(&call) {
                Some(review) => {
                    (call.result.is_some() && !call.is_error).then_some(Item::Review(review))
                }
                None => Some(Item::Tool(call)),
            },
            item => Some(item),
        })
        .collect()
}

/// The calls subagents made, read from their own transcripts in `dir`, each after the call
/// that started its subagent.
fn with_subagents(items: Vec<Item>, dir: &Path) -> Vec<Item> {
    let mut started: HashMap<String, Vec<ToolCall>> = HashMap::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let meta = entry.path();
        let Some(agent) = meta
            .file_name()
            .and_then(|name| name.to_str()?.strip_suffix(".meta.json"))
        else {
            continue;
        };
        let Some(parent) = agent_call(&meta) else {
            continue;
        };
        let (subagent, _) = parse_transcript(&dir.join(format!("{agent}.jsonl")));
        let calls = subagent.into_iter().filter_map(|item| match item {
            Item::Tool(call) => Some(ToolCall {
                parent: Some(parent.clone()),
                ..call
            }),
            _ => None,
        });
        started.entry(parent.clone()).or_default().extend(calls);
    }

    fn place(call: ToolCall, started: &mut HashMap<String, Vec<ToolCall>>, out: &mut Vec<Item>) {
        let made = started.remove(&call.call_id).unwrap_or_default();
        out.push(Item::Tool(call));
        for call in made {
            place(call, started, out);
        }
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Item::Tool(call) => place(call, &mut started, &mut out),
            item => out.push(item),
        }
    }
    out
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
        let (modified, newest) = newest_transcript(&project.path())?;
        let seconds = modified.duration_since(UNIX_EPOCH).ok()?.as_secs();
        Some((seconds, transcript_cwd(&newest)?))
    }));
    rank_folders(known, &[data_root(), std::env::temp_dir()])
}

/// A project's most recently written transcript.
fn newest_transcript(project: &Path) -> Option<(SystemTime, PathBuf)> {
    fs::read_dir(project)
        .ok()?
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "jsonl"))
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .max()
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

/// What a command printed, as a reply: a code review's findings, or its text.
fn reply(output: &str) -> Item {
    review(output).map_or_else(|| Item::Assistant(output.to_string()), Item::Review)
}

fn parse_transcript(path: &Path) -> (Vec<Item>, Option<String>) {
    let Ok(content) = fs::read_to_string(path) else {
        return (Vec::new(), None);
    };
    let mut items: Vec<Item> = Vec::new();
    let mut ai_title = None;
    let mut turn: Option<TurnTime> = None;
    let subagents = path.with_extension("").join("subagents");

    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let joined = joined(&v);
        let mid_turn = joined.is_some();
        let v = joined.unwrap_or(v);
        let pushed = items.len();
        let kind = v.get("type").and_then(Value::as_str);
        let at = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(unix_millis);
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
                    items.push(reply(output));
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
                        items.push(reply(output));
                    }
                }
                Some(Value::String(s)) => match task_notification(s) {
                    Some(end) => {
                        // A resumed subagent's end names the call that resumed it.
                        let call_id = end
                            .task
                            .and_then(|task| {
                                agent_call(&subagents.join(format!("agent-{task}.meta.json")))
                            })
                            .unwrap_or(end.call_id);
                        if let Some(call) = call_mut(&mut items, &call_id) {
                            call.background = Some(end.background);
                            call.ended = at.or(call.ended);
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
                                at,
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
                            Some("tool_use") => items.push(Item::Tool(ToolCall {
                                started: at,
                                ..tool_call(b)
                            })),
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
        let Some(at) = at.map(|millis| millis / 1000) else {
            continue;
        };
        let starts_turn = !mid_turn
            && items[pushed..].iter().any(|item| {
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
                || items[pushed..]
                    .iter()
                    .any(|item| matches!(item, Item::Assistant(_) | Item::Review(_))))
        {
            turn.ended = at;
        }
    }
    items.extend(turn.map(Item::Ended));

    (items, ai_title)
}

/// A message sent while a turn ran, which joined it at its next step: the user's, another
/// agent's or a background task's end. Claude Code writes it as an attachment rather than a
/// user line; this is the user line it reads as.
fn joined(v: &Value) -> Option<Value> {
    let attachment = v.get("attachment")?;
    if v.get("type")?.as_str()? != "attachment"
        || attachment.get("type")?.as_str()? != "queued_command"
        || flag(attachment, "isMeta")
    {
        return None;
    }
    let mut user = serde_json::json!({
        "type": "user",
        "timestamp": v.get("timestamp"),
        "message": { "content": attachment.get("prompt")? },
    });
    if let Some(origin) = attachment.get("origin").filter(|origin| !origin.is_null()) {
        user["origin"] = origin.clone();
    }
    Some(user)
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

fn attach_result(
    items: &mut [Item],
    call_id: &str,
    result: String,
    is_error: bool,
    at: Option<u64>,
) {
    if let Some(tc) = call_mut(items, call_id) {
        tc.ended = at;
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
            ..Store::default()
        };
        let settings = Settings {
            model: "opus".to_string(),
            effort: Effort::High,
            profile: Profile::FullAuto,
            remote_control: false,
            ..Settings::default()
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
            "content": "<local-command-stdout>1 finding:\n\n1. `a.rs:2`: Off by one.</local-command-stdout>",
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
        let base = unix_millis(&at(0)).unwrap() / 1000;
        assert_eq!(ended, [(base, base + 70), (base + 800, base + 900)]);
        assert!(
            items
                .iter()
                .any(|i| matches!(i, Item::Review(r) if r.findings[0].summary == "Off by one.")),
            "a forked review's result is its findings"
        );
        assert!(
            matches!(items[3], Item::Ended(_)),
            "between the first turn and the second"
        );
    }

    #[test]
    fn a_reported_review_takes_its_calls_place_and_a_refused_one_is_dropped() {
        let report = |id: &str| {
            serde_json::json!({
                "type": "assistant",
                "message": { "content": [{ "type": "tool_use", "id": id, "name": "ReportFindings", "input": {
                    "findings": [{ "file": "a.rs", "line": 3, "summary": "Off by one.", "short_summary": "Off by one", "failure_scenario": "Always.", "category": "correctness" }],
                } }] },
            })
        };
        let result = |id: &str, is_error: bool| {
            serde_json::json!({
                "type": "user",
                "message": { "content": [{ "type": "tool_result", "tool_use_id": id, "is_error": is_error, "content": "1 finding reported." }] },
            })
        };
        let transcript = [
            report("r1"),
            result("r1", true),
            report("r2"),
            result("r2", false),
        ]
        .map(line)
        .concat();
        let path = std::env::temp_dir().join(format!("claudo-reported-{}.jsonl", Uuid::new_v4()));
        fs::write(&path, transcript).unwrap();
        let (items, _) = parse_transcript(&path);
        let _ = fs::remove_file(&path);
        let items = with_reviews(items);
        let [Item::Review(review)] = &items[..] else {
            panic!("{items:?}");
        };
        assert_eq!(review.findings[0].title.as_deref(), Some("Off by one"));
        assert!(review.findings[0].bug);
    }

    #[test]
    fn a_resumed_subagents_end_goes_to_the_agent_call_that_started_it() {
        let call = |id: &str, name: &str| {
            serde_json::json!({
                "type": "assistant",
                "message": { "content": [{ "type": "tool_use", "id": id, "name": name, "input": {} }] },
            })
        };
        let ended = |call: &str, status: &str| {
            serde_json::json!({
                "type": "user",
                "message": { "content": format!("<task-notification>\n<task-id>a1</task-id>\n<tool-use-id>{call}</tool-use-id>\n<status>{status}</status>\n<summary>It {status}</summary>\n</task-notification>") },
            })
        };
        let transcript = [
            call("agent", "Agent"),
            ended("agent", "failed"),
            call("send", "SendMessage"),
            ended("send", "completed"),
        ]
        .map(line)
        .concat();
        let path = std::env::temp_dir().join(format!("claudo-resumed-{}.jsonl", Uuid::new_v4()));
        let subagents = path.with_extension("").join("subagents");
        fs::create_dir_all(&subagents).unwrap();
        fs::write(subagents.join("agent-a1.meta.json"), r#"{"toolUseId":"agent"}"#).unwrap();
        fs::write(&path, transcript).unwrap();
        let (items, _) = parse_transcript(&path);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir_all(path.with_extension(""));
        let status: Vec<(&str, Option<TaskStatus>)> = items
            .iter()
            .filter_map(|item| match item {
                Item::Tool(call) => Some((
                    call.call_id.as_str(),
                    call.background.as_ref().map(|b| b.status),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            status,
            [("agent", Some(TaskStatus::Completed)), ("send", None)]
        );
    }

    #[test]
    fn messages_that_joined_a_running_turn_are_kept_where_they_joined() {
        let at = |s: u32| format!("2026-10-21T18:20:{s:02}.000Z");
        let queued = |s: u32, prompt: serde_json::Value, origin: serde_json::Value| {
            serde_json::json!({
                "type": "attachment", "timestamp": at(s),
                "attachment": { "type": "queued_command", "prompt": prompt, "commandMode": "prompt", "origin": origin },
            })
        };
        let transcript = [
            serde_json::json!({ "type": "user", "timestamp": at(1), "message": { "content": "fix it" } }),
            serde_json::json!({ "type": "assistant", "timestamp": at(2), "message": { "content": [{ "type": "text", "text": "On it." }] } }),
            queued(3, "also the docs".into(), serde_json::Value::Null),
            queued(4, serde_json::json!([{ "type": "text", "text": "and push" }]), serde_json::json!({ "kind": "human" })),
            queued(5, "the build is red".into(), serde_json::json!({ "kind": "peer", "name": "ci" })),
            serde_json::json!({ "type": "attachment", "timestamp": at(6), "attachment": { "type": "queued_command", "prompt": "reminder", "isMeta": true } }),
            serde_json::json!({ "type": "assistant", "timestamp": at(7), "message": { "content": [{ "type": "text", "text": "Done." }] } }),
        ]
        .map(line)
        .concat();
        let path = std::env::temp_dir().join(format!("claudo-joined-{}.jsonl", Uuid::new_v4()));
        fs::write(&path, transcript).unwrap();
        let (items, _) = parse_transcript(&path);
        let _ = fs::remove_file(&path);
        let shown: Vec<String> = items
            .iter()
            .map(|item| match item {
                Item::User(text) => format!("user {text}"),
                Item::Assistant(text) => format!("claude {text}"),
                Item::Incoming { from, text } => format!("{from} {text}"),
                Item::Ended(_) => "ended".to_string(),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            shown,
            [
                "user fix it",
                "claude On it.",
                "user also the docs",
                "user and push",
                "ci the build is red",
                "claude Done.",
                "ended",
            ],
            "one turn, with what joined it"
        );
    }

    #[test]
    fn a_call_runs_from_its_use_to_its_result() {
        let transcript = [
            serde_json::json!({
                "type": "assistant", "timestamp": "2026-10-17T22:38:52.000Z",
                "message": { "content": [
                    { "type": "tool_use", "id": "t1", "name": "Bash", "input": { "command": "ls" } },
                ] },
            }),
            serde_json::json!({
                "type": "user", "timestamp": "2026-10-17T22:38:53.250Z",
                "message": { "content": [
                    { "type": "tool_result", "tool_use_id": "t1", "content": "ok" },
                ] },
            }),
        ]
        .map(|line| format!("{line}\n"))
        .concat();
        let path = std::env::temp_dir().join(format!("claudo-call-{}.jsonl", Uuid::new_v4()));
        fs::write(&path, transcript).unwrap();
        let (items, _) = parse_transcript(&path);
        let _ = fs::remove_file(&path);
        let base = unix_millis("2026-10-17T22:38:52.000Z").unwrap();
        let times = items.iter().find_map(|item| match item {
            Item::Tool(call) => Some((call.started, call.ended)),
            _ => None,
        });
        assert_eq!(times, Some((Some(base), Some(base + 1250))));
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
            ..Store::default()
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

    fn scratch_conversation(id: &str) -> ConversationMeta {
        ConversationMeta {
            id: id.to_string(),
            title: id.to_string(),
            cwd: PathBuf::new(),
            attached: false,
            renamed: false,
            archived: false,
            always_allowed: Vec::new(),
            session_id: None,
            imported_from: None,
            overrides: Overrides::default(),
            context: ContextUsage::default(),
            created: 0,
            updated: 0,
        }
    }

    #[test]
    fn a_save_keeps_the_conversations_another_claudo_added() {
        let mut store = Store {
            conversations: vec![scratch_conversation("mine")],
            ..Store::default()
        };
        store.removed.insert("deleted here".to_string());
        store.adopt(
            ["mine", "theirs", "deleted here"]
                .map(scratch_conversation)
                .to_vec(),
        );
        let ids: Vec<&str> = store.conversations.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["mine", "theirs"]);
    }

    #[test]
    fn a_save_takes_in_what_another_claudo_changed_and_keeps_what_this_one_did() {
        let mut store = Store {
            conversations: ["compacted here", "untouched here"]
                .map(scratch_conversation)
                .to_vec(),
            ..Store::default()
        };
        store.synced = store.conversations.clone();
        store.conversations[0].context.used = 8_690;
        let mut theirs = store.synced.clone();
        for c in &mut theirs {
            c.context.used = 544_848;
        }
        store.adopt(theirs);
        let used: Vec<u64> = store.conversations.iter().map(|c| c.context.used).collect();
        assert_eq!(used, [8_690, 544_848]);
    }

    #[test]
    fn a_scratch_conversation_missing_from_the_list_comes_back() {
        let root = std::env::temp_dir().join(format!("claudo-recover-{}", Uuid::new_v4()));
        let (projects, scratch) = (root.join("projects"), root.join("conversations"));
        let transcript = |project: &str, cwd: &Path| {
            let dir = projects.join(project);
            fs::create_dir_all(&dir).unwrap();
            let line = serde_json::json!({
                "type": "user", "cwd": cwd, "timestamp": "2026-10-17T21:31:33.835Z",
                "message": {"content": "summarize the build logs"},
            });
            fs::write(dir.join("s1.jsonl"), line.to_string()).unwrap();
        };
        for id in ["lost", "listed", "deleted"] {
            transcript(id, &scratch.join(id));
        }
        transcript("project", &root);
        fs::create_dir_all(scratch.join("lost")).unwrap();
        fs::create_dir_all(scratch.join("listed")).unwrap();
        let mut store = Store {
            conversations: vec![scratch_conversation("listed")],
            ..Store::default()
        };

        let recovered = store.recover(&projects, &scratch);
        let _ = fs::remove_dir_all(&root);
        assert!(recovered);
        let ids: Vec<&str> = store.conversations.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["listed", "lost"], "a deleted one stays deleted");
        let lost = &store.conversations[1];
        assert_eq!(lost.session_id.as_deref(), Some("s1"));
        assert_eq!(lost.title, "summarize the build logs");
    }

    #[test]
    fn a_subagents_calls_follow_the_call_that_started_it() {
        let dir = std::env::temp_dir().join(format!("claudo-subagents-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let use_line = |id: &str, name: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{{"command":"ls"}}}}]}}}}"#
            )
        };
        let result = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"b","content":"ok"}]}}"#;
        for (agent, started_by, lines) in [
            (
                "one",
                "a",
                vec![
                    use_line("b", "Bash"),
                    result.to_string(),
                    use_line("c", "Agent"),
                ],
            ),
            ("two", "c", vec![use_line("d", "Bash")]),
        ] {
            fs::write(dir.join(format!("agent-{agent}.jsonl")), lines.join("\n")).unwrap();
            let meta = format!(r#"{{"agentType":"general-purpose","toolUseId":"{started_by}"}}"#);
            fs::write(dir.join(format!("agent-{agent}.meta.json")), meta).unwrap();
        }
        let mut agent = tool_call(&serde_json::json!({"id": "a", "name": "Agent"}));
        agent.result = Some("launched".to_string());
        let items = vec![
            Item::User("review".to_string()),
            Item::Tool(agent),
            Item::Assistant("started".to_string()),
        ];
        let items = with_subagents(items, &dir);
        let _ = fs::remove_dir_all(&dir);

        let calls: Vec<(&str, Option<&str>)> = items
            .iter()
            .filter_map(|item| match item {
                Item::Tool(call) => Some((call.call_id.as_str(), call.parent.as_deref())),
                _ => None,
            })
            .collect();
        assert_eq!(
            calls,
            [
                ("a", None),
                ("b", Some("a")),
                ("c", Some("a")),
                ("d", Some("c"))
            ]
        );
        assert!(matches!(&items[2], Item::Tool(b) if b.result.as_deref() == Some("ok")));
        assert!(matches!(items.last(), Some(Item::Assistant(_))));
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
