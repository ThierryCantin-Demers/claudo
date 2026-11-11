//! How full each Claude agent's context is, from its transcript's last request of the main
//! agent, when the person last wrote to it, and which of the pull requests it opened are
//! still open. A note on `session:<id>`, so only an agent another collector shows gets one.
//! Also the subagents each one runs now, as nodes of their own under it.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use claudo_protocol::{Node, NodeKind, NodeState, Provider, PullRequest};
use serde_json::Value;

use crate::room::Feed;

/// How often the transcripts are looked at. One is read again only once it changed.
const POLL: Duration = Duration::from_secs(10);

/// A transcript untouched for longer belongs to no running agent.
const RECENT: Duration = Duration::from_secs(12 * 60 * 60);

/// How much of a transcript's end is read for its last request, which a long tool result
/// or a long reply can push far from the end.
const TAIL: u64 = 1 << 20;

/// How often GitHub is asked which pull requests are still open.
const GITHUB_POLL: Duration = Duration::from_secs(3 * 60);

/// A subagent whose transcript has not changed for this long, with no end read, died with
/// the agent that ran it. One waiting on a command in the background, which writes nothing
/// meanwhile, is given until `RECENT`.
const SUBAGENT_STALE: Duration = Duration::from_secs(60 * 60);

/// Reads the transcripts into `feed`, a collector of notes, and the subagents running now
/// into `subagents`, for as long as the process runs.
pub fn spawn(feed: Feed, subagents: Feed) {
    let Some(projects) = crate::sessions::claude_projects_dir() else {
        return;
    };
    let open: Arc<Mutex<HashMap<String, PullRequest>>> = Arc::default();
    let asked = open.clone();
    std::thread::spawn(move || {
        loop {
            // A failed look keeps what the last one said.
            if let Some(now_open) = crate::github::open_pull_requests() {
                *asked.lock().unwrap() = now_open;
            }
            std::thread::sleep(GITHUB_POLL);
        }
    });
    std::thread::spawn(move || {
        let mut transcripts = Transcripts::default();
        loop {
            let open = open.lock().unwrap().clone();
            let (notes, running) = transcripts.scan(&projects, SystemTime::now(), &open);
            feed.replace(notes);
            subagents.replace(running);
            std::thread::sleep(POLL);
        }
    });
}

/// Each recent transcript as last read, with when it was written, and the same for each
/// recent subagent's.
#[derive(Default)]
struct Transcripts {
    read: HashMap<PathBuf, Known>,
    subagents: HashMap<PathBuf, Waiting>,
}

/// The commands a subagent left running in the background, read from the start of its
/// transcript on. Its turn can end while one runs, and it goes on once it ends.
#[derive(Default)]
struct Waiting {
    offset: u64,
    calls: HashSet<String>,
}

impl Waiting {
    fn read_on(&mut self, path: &Path) {
        let Ok(mut file) = std::fs::File::open(path) else {
            return;
        };
        let length = file.metadata().map_or(0, |meta| meta.len());
        if length < self.offset {
            *self = Waiting::default();
        }
        let mut bytes = Vec::new();
        if file.seek(SeekFrom::Start(self.offset)).is_err() || file.read_to_end(&mut bytes).is_err()
        {
            return;
        }
        let Some(end) = bytes.iter().rposition(|&byte| byte == b'\n') else {
            return;
        };
        self.offset += end as u64 + 1;
        for line in String::from_utf8_lossy(&bytes[..end]).lines() {
            if line.contains("Command running in background")
                || line.contains("Command was moved to the background")
            {
                let Ok(line) = serde_json::from_str(line) else {
                    continue;
                };
                for (id, output) in crate::engine::cli::call_results(&line) {
                    if crate::engine::cli::backgrounded(&output) {
                        self.calls.insert(id);
                    }
                }
            }
            if line.contains("<task-notification>") {
                let Ok(line) = serde_json::from_str(line) else {
                    continue;
                };
                for id in crate::engine::cli::notified_end(&line) {
                    self.calls.remove(&id);
                }
            }
        }
    }
}

#[derive(Default)]
struct Known {
    written: Option<SystemTime>,
    tail: Tail,
    opened: Opened,
}

/// The pull requests a session opened, and when its subagents ended, read from the start
/// of its transcript on.
#[derive(Default)]
struct Opened {
    /// How far the transcript has been read, to the end of its last whole line.
    offset: u64,
    /// `gh pr create` calls whose result has not been read yet.
    calls: HashSet<String>,
    links: BTreeSet<String>,
    /// Agent calls whose subagent has not ended yet.
    agents: HashSet<String>,
    /// When each subagent ended, in Unix seconds, by its Agent call and by its task id.
    ended: HashMap<String, u64>,
}

impl Opened {
    fn read_on(&mut self, path: &Path) {
        let Ok(mut file) = std::fs::File::open(path) else {
            return;
        };
        let length = file.metadata().map_or(0, |meta| meta.len());
        if length < self.offset {
            *self = Opened::default();
        }
        let mut bytes = Vec::new();
        if file.seek(SeekFrom::Start(self.offset)).is_err() || file.read_to_end(&mut bytes).is_err()
        {
            return;
        }
        let Some(end) = bytes.iter().rposition(|&byte| byte == b'\n') else {
            return;
        };
        self.offset += end as u64 + 1;
        for line in String::from_utf8_lossy(&bytes[..end]).lines() {
            // Parsing every line of a long transcript is slow; these are the only ones read.
            if line.contains("gh pr create") {
                let Ok(line) = serde_json::from_str(line) else {
                    continue;
                };
                for (id, command) in crate::engine::cli::shell_calls(&line) {
                    if runs_pr_create(&command) {
                        self.calls.insert(id);
                    }
                }
            }
            if line.contains(r#""name":"Agent""#) || line.contains(r#""name":"Task""#) {
                let Ok(line) = serde_json::from_str(line) else {
                    continue;
                };
                self.agents.extend(crate::engine::cli::agent_calls(&line));
            }
            if line.contains("<task-notification>") {
                let Ok(line) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                let at = seconds(&line);
                for id in crate::engine::cli::notified_end(&line) {
                    self.agents.remove(&id);
                    self.ended.extend(at.map(|at| (id, at)));
                }
            }
            let answers_agent = || self.agents.iter().any(|id| line.contains(id.as_str()));
            if line.contains("tool_result") && answers_agent() {
                let Ok(line) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                let at = seconds(&line);
                for (id, output) in crate::engine::cli::call_results(&line) {
                    // One in the background answers at once, and ends with a notification.
                    if self.agents.contains(&id) && !output.starts_with("Async agent launched") {
                        self.agents.remove(&id);
                        self.ended.extend(at.map(|at| (id, at)));
                    }
                }
            }
            if !self.calls.is_empty() && line.contains("/pull/") {
                let Ok(line) = serde_json::from_str(line) else {
                    continue;
                };
                for (id, output) in crate::engine::cli::call_results(&line) {
                    if self.calls.remove(&id) {
                        self.links.extend(opened(&output));
                    }
                }
            }
        }
    }
}

/// What a transcript's end says.
#[derive(Clone, Copy, Default)]
struct Tail {
    context: Option<u64>,
    answered: Option<u64>,
}

impl Transcripts {
    fn scan(
        &mut self,
        projects: &Path,
        now: SystemTime,
        open: &HashMap<String, PullRequest>,
    ) -> (Vec<Node>, Vec<Node>) {
        let mut read = HashMap::new();
        // A session that moved leaves a copy behind; the newest is the live one.
        let mut sessions: HashMap<String, (SystemTime, Tail, Vec<PullRequest>, PathBuf)> =
            HashMap::new();
        let files = std::fs::read_dir(projects)
            .into_iter()
            .flatten()
            .flatten()
            .flat_map(|project| {
                std::fs::read_dir(project.path())
                    .into_iter()
                    .flatten()
                    .flatten()
            });
        for file in files {
            let path = file.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            let Ok(written) = file.metadata().and_then(|meta| meta.modified()) else {
                continue;
            };
            if now.duration_since(written).unwrap_or_default() > RECENT {
                continue;
            }
            let mut known = self.read.remove(&path).unwrap_or_default();
            if known.written != Some(written) {
                // A long tool result can push the last request out of the end read; what
                // was known before still stands.
                let tail = read_tail(&path);
                known.tail = Tail {
                    context: tail.context.or(known.tail.context),
                    answered: tail.answered.or(known.tail.answered),
                };
                known.opened.read_on(&path);
                known.written = Some(written);
            }
            let tail = known.tail;
            let prs: Vec<PullRequest> = known
                .opened
                .links
                .iter()
                .filter_map(|link| open.get(link).cloned())
                .collect();
            read.insert(path.clone(), known);
            let Some(session) = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
            else {
                continue;
            };
            let newest = sessions
                .get(&session)
                .is_none_or(|(at, _, _, _)| *at < written);
            if newest {
                sessions.insert(session, (written, tail, prs, path));
            }
        }
        self.read = read;
        let mut running = Vec::new();
        let mut waiting = HashMap::new();
        for (session, (_, _, _, path)) in &sessions {
            let ended = &self.read[path].opened.ended;
            let folder = path.with_extension("");
            running.extend(subagents(
                &mut self.subagents,
                &folder,
                session,
                ended,
                &mut waiting,
                now,
            ));
        }
        self.subagents = waiting;
        let notes = sessions
            .into_iter()
            .filter(|(_, (_, tail, prs, _))| {
                tail.context.is_some() || tail.answered.is_some() || !prs.is_empty()
            })
            .map(|(session, (_, tail, prs, _))| Node {
                id: format!("session:{session}"),
                context: tail.context,
                answered: tail.answered,
                prs,
                ..Node::default()
            })
            .collect();
        (notes, running)
    }
}

/// The subagents of a session that run now, from `<session>/subagents`: each one's
/// `agent-<id>.jsonl`, and beside it the `.meta.json` naming its Agent call. One runs from
/// its call until the transcript says it ended, and again once its own transcript moves
/// on after that, as when it is sent a message, or while a command it started in the
/// background still runs.
fn subagents(
    read: &mut HashMap<PathBuf, Waiting>,
    folder: &Path,
    session: &str,
    ended: &HashMap<String, u64>,
    waiting: &mut HashMap<PathBuf, Waiting>,
    now: SystemTime,
) -> Vec<Node> {
    let Ok(entries) = std::fs::read_dir(folder.join("subagents")) else {
        return Vec::new();
    };
    let mut nodes = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(agent) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("agent-")?.strip_suffix(".jsonl"))
        else {
            continue;
        };
        let Ok(written) = entry.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        let quiet = now.duration_since(written).unwrap_or_default();
        if quiet > RECENT {
            continue;
        }
        let mut calls = read.remove(&path).unwrap_or_default();
        calls.read_on(&path);
        let background = !calls.calls.is_empty();
        waiting.insert(path.clone(), calls);
        if quiet > SUBAGENT_STALE && !background {
            continue;
        }
        let meta: Value = std::fs::read(path.with_extension("meta.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let field = |key: &str| meta.get(key).and_then(Value::as_str);
        let end = [Some(agent), field("toolUseId")]
            .into_iter()
            .flatten()
            .filter_map(|id| ended.get(id))
            .max();
        let written = written
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        // An end is timed to the second, and the subagent's last line comes just before it.
        let turn_over = end.is_some_and(|end| written <= end + 1);
        if turn_over && !background {
            continue;
        }
        let doing = match turn_over {
            true => Some("waiting on a command in the background".to_string()),
            false => field("agentType").map(str::to_string),
        };
        nodes.push(Node {
            id: format!("subagent:{agent}"),
            parent: Some(format!("session:{session}")),
            kind: Some(NodeKind::Subagent),
            name: field("description").map(str::to_string),
            doing,
            state: Some(NodeState::Working),
            provider: Some(Provider::Claude),
            ..Node::default()
        });
    }
    nodes
}

/// A transcript line's time, in Unix seconds.
fn seconds(line: &Value) -> Option<u64> {
    let at = line
        .get("timestamp")?
        .as_str()?
        .parse::<jiff::Timestamp>()
        .ok()?;
    u64::try_from(at.as_second()).ok()
}

/// The pull requests a `gh pr create` printed, leaving out the one it refused to open again
/// for the branch: `a pull request for branch "x" into branch "main" already exists:` and its
/// link on the next line.
fn opened(output: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut refused = false;
    for line in output.lines() {
        let names_existing =
            line.starts_with("a pull request for branch") && line.contains("already exists");
        if !names_existing && !refused {
            links.extend(crate::github::pull_links(line));
        }
        refused = names_existing;
    }
    links
}

/// Whether a shell command runs `gh pr create`, rather than only mentioning it, as a
/// script searching transcripts for it does.
fn runs_pr_create(command: &str) -> bool {
    let command = &unquoted(command);
    let assignment = |word: &str| {
        word.split_once('=').is_some_and(|(name, _)| {
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    };
    command.match_indices("gh pr create").any(|(at, found)| {
        let after = command[at + found.len()..].chars().next();
        if after.is_some_and(|c| !c.is_whitespace() && !";&|)`".contains(c)) {
            return false;
        }
        // A line break ends a command, so only spaces are trimmed.
        let blank = [' ', '\t'];
        let mut before = command[..at].trim_end_matches(blank);
        loop {
            let (rest, word) = before.rsplit_once(blank).unwrap_or(("", before));
            if word.is_empty() || !assignment(word) {
                break;
            }
            before = rest.trim_end_matches(blank);
        }
        before.is_empty() || before.ends_with([';', '&', '|', '(', '{', '`', '\n'])
    })
}

/// The command with its comments and what its quotes and here-documents hold blanked out,
/// since that is text it passes on rather than commands it runs.
fn unquoted(command: &str) -> String {
    let mut code = String::with_capacity(command.len());
    commands(&mut command.chars().peekable(), None, &mut code);
    code
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

/// Copies what runs into `code` up to `end`, the close of a substitution, outside quotes.
fn commands(chars: &mut Chars, end: Option<char>, code: &mut String) {
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let mut depth = 0usize;
    let mut word_start = true;
    while let Some(c) = chars.next() {
        if Some(c) == end && depth == 0 {
            code.push(c);
            return;
        }
        match c {
            '(' => {
                depth += 1;
                code.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                code.push(c);
            }
            '\\' => {
                code.push('_');
                if chars.next().is_some() {
                    code.push('_');
                }
            }
            '\'' => {
                code.push('_');
                for inside in chars.by_ref() {
                    code.push('_');
                    if inside == '\'' {
                        break;
                    }
                }
            }
            '"' => {
                code.push('_');
                quoted(chars, code);
            }
            '#' if word_start => {
                while chars.next_if(|c| *c != '\n').is_some() {
                    code.push('_');
                }
            }
            '<' if chars.peek() == Some(&'<') => {
                chars.next();
                code.push_str("<<");
                if chars.next_if_eq(&'<').is_some() {
                    code.push('<');
                    continue;
                }
                let tabs = chars.next_if_eq(&'-').is_some();
                while chars.next_if(|c| *c == ' ' || *c == '\t').is_some() {}
                let mut word = String::new();
                while let Some(c) = chars.next_if(|c| !c.is_whitespace() && !";&|<>()".contains(*c))
                {
                    if !matches!(c, '\'' | '"' | '\\') {
                        word.push(c);
                    }
                }
                code.push('_');
                if !word.is_empty() {
                    heredocs.push((word, tabs));
                }
            }
            '\n' if !heredocs.is_empty() => {
                code.push('\n');
                for (word, tabs) in heredocs.drain(..) {
                    loop {
                        let line: String = chars.by_ref().take_while(|c| *c != '\n').collect();
                        let line = if tabs {
                            line.trim_start_matches('\t')
                        } else {
                            &line
                        };
                        if line == word || chars.peek().is_none() {
                            break;
                        }
                    }
                }
                code.push('\n');
            }
            c => code.push(c),
        }
        word_start = c.is_whitespace() || ";&|()`".contains(c);
    }
}

/// Blanks a double-quoted string up to its close, keeping the substitutions it runs.
fn quoted(chars: &mut Chars, code: &mut String) {
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                code.push('_');
                return;
            }
            '\\' => {
                code.push('_');
                if chars.next().is_some() {
                    code.push('_');
                }
            }
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                code.push_str("$(");
                commands(chars, Some(')'), code);
            }
            '`' => {
                code.push('`');
                commands(chars, Some('`'), code);
            }
            _ => code.push('_'),
        }
    }
}

fn read_tail(path: &Path) -> Tail {
    tail_text(path).map_or_else(Tail::default, |text| Tail {
        context: text
            .lines()
            .rev()
            .filter_map(|line| serde_json::from_str(line).ok())
            .find_map(|line| crate::engine::cli::request_context(&line)),
        answered: text
            .lines()
            .rev()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(crate::engine::cli::from_person)
            .and_then(|line| seconds(&line)),
    })
}

/// The last whole lines of a transcript, up to `TAIL` bytes of them.
fn tail_text(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    // A read from the middle starts partway through a line.
    Some(if start > 0 {
        text.split_once('\n')
            .map_or(String::new(), |(_, rest)| rest.to_string())
    } else {
        text.into_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agents_context_is_its_last_main_request() {
        let projects =
            std::env::temp_dir().join(format!("claudo-transcripts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&projects);
        std::fs::create_dir_all(projects.join("-src-alpha")).unwrap();
        let usage = |tokens: u64| {
            format!(
                r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":{tokens},"cache_read_input_tokens":1000,"cache_creation_input_tokens":0,"output_tokens":0}}}}}}"#
            )
        };
        let transcript = [
            usage(5),
            usage(7),
            r#"{"type":"user","message":{"content":"next"}}"#.to_string(),
        ]
        .join("\n");
        std::fs::write(projects.join("-src-alpha/s1.jsonl"), transcript).unwrap();
        std::fs::write(
            projects.join("-src-alpha/s2.jsonl"),
            r#"{"type":"user","message":{"content":"first"}}"#,
        )
        .unwrap();
        let (nodes, _) = Transcripts::default().scan(&projects, SystemTime::now(), &HashMap::new());
        let shown: Vec<_> = nodes
            .iter()
            .map(|node| (node.id.as_str(), node.context))
            .collect();
        assert_eq!(
            shown,
            [("session:s1", Some(1007))],
            "a session yet to ask has none"
        );
        let later = SystemTime::now() + RECENT + Duration::from_secs(60);
        assert!(
            Transcripts::default()
                .scan(&projects, later, &HashMap::new())
                .0
                .is_empty()
        );
        std::fs::remove_dir_all(projects).unwrap();
    }

    #[test]
    fn a_request_pushed_out_of_the_end_read_keeps_its_context() {
        let projects = std::env::temp_dir().join(format!("claudo-tail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&projects);
        std::fs::create_dir_all(projects.join("-src-alpha")).unwrap();
        let path = projects.join("-src-alpha/s1.jsonl");
        let request = r#"{"type":"assistant","message":{"usage":{"input_tokens":7,"cache_read_input_tokens":1000}}}"#;
        std::fs::write(&path, format!("{request}\n")).unwrap();
        let mut transcripts = Transcripts::default();
        assert_eq!(
            transcripts
                .scan(&projects, SystemTime::now(), &HashMap::new())
                .0[0]
                .context,
            Some(1007)
        );
        let result = format!(
            r#"{{"type":"user","message":{{"content":"{}"}}}}"#,
            "x".repeat(TAIL as usize)
        );
        std::fs::write(&path, format!("{request}\n{result}\n")).unwrap();
        let later = SystemTime::now() + Duration::from_secs(1);
        assert_eq!(
            transcripts.scan(&projects, later, &HashMap::new()).0[0].context,
            Some(1007)
        );
        std::fs::remove_dir_all(projects).unwrap();
    }

    #[test]
    fn the_person_answered_at_their_last_message_and_no_one_elses() {
        let projects = std::env::temp_dir().join(format!("claudo-answered-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&projects);
        std::fs::create_dir_all(projects.join("-src-alpha")).unwrap();
        let transcript = [
            r#"{"type":"user","timestamp":"2026-11-11T10:00:00Z","origin":{"kind":"human"},"message":{"content":"go"}}"#,
            r#"{"type":"attachment","timestamp":"2026-11-11T10:05:00Z","attachment":{"type":"queued_command","prompt":"also this","origin":{"kind":"human"}}}"#,
            r#"{"type":"user","timestamp":"2026-11-11T11:00:00Z","origin":{"kind":"task-notification"},"message":{"content":"<task-notification>"}}"#,
            r#"{"type":"user","timestamp":"2026-11-11T11:30:00Z","origin":{"name":"builder"},"message":{"content":"from another agent"}}"#,
        ]
        .join("\n");
        std::fs::write(projects.join("-src-alpha/s1.jsonl"), transcript).unwrap();
        let (nodes, _) = Transcripts::default().scan(&projects, SystemTime::now(), &HashMap::new());
        let at = "2026-11-11T10:05:00Z".parse::<jiff::Timestamp>().unwrap();
        assert_eq!(nodes[0].answered, Some(at.as_second() as u64));
        std::fs::remove_dir_all(projects).unwrap();
    }

    #[test]
    fn an_agent_shows_the_open_pull_requests_it_opened() {
        let projects = std::env::temp_dir().join(format!("claudo-opened-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&projects);
        std::fs::create_dir_all(projects.join("-src-alpha")).unwrap();
        let call = |id: &str, command: &str| {
            serde_json::json!({"type": "assistant", "message": {"content": [
                {"type": "tool_use", "id": id, "name": "Bash", "input": {"command": command}}
            ]}})
            .to_string()
        };
        let answer = |id: &str, output: &str, failed: bool| {
            serde_json::json!({"type": "user", "message": {"content": [
                {"type": "tool_result", "tool_use_id": id, "content": output, "is_error": failed}
            ]}})
            .to_string()
        };
        let result = |id: &str, output: &str| answer(id, output, false);
        let transcript = [
            call("a", "gh pr create --title 'Fix' --body-file body.md"),
            result("a", "https://github.com/acme/widgets/pull/42\n"),
            call("b", "gh pr view 7"),
            result("b", "https://github.com/acme/widgets/pull/7"),
            call("c", "cd ../gadgets && gh pr create --fill"),
            result("c", "https://github.com/acme/gadgets/pull/9"),
            call("e", "gh pr create --fill && git checkout main"),
            answer(
                "e",
                "https://github.com/acme/widgets/pull/43\nerror: pathspec 'main' did not match",
                true,
            ),
            call("f", "git checkout -b feature; gh pr create --fill"),
            result(
                "f",
                "fatal: a branch named 'feature' already exists\nhttps://github.com/acme/widgets/pull/44",
            ),
            call("d", "gh pr create --fill"),
            answer(
                "d",
                "a pull request for branch \"fix\" already exists:\nhttps://github.com/acme/widgets/pull/7",
                true,
            ),
        ]
        .join("\n")
            + "\n";
        std::fs::write(projects.join("-src-alpha/s1.jsonl"), transcript).unwrap();
        let pr = |repo: &str, number: u64| PullRequest {
            url: format!("https://github.com/{repo}/pull/{number}"),
            number,
            title: "Fix".into(),
            repo: repo.into(),
            draft: false,
        };
        let open: HashMap<_, _> = [
            pr("acme/widgets", 42),
            pr("acme/widgets", 43),
            pr("acme/widgets", 44),
            pr("acme/widgets", 7),
        ]
        .into_iter()
        .map(|pr| (pr.url.clone(), pr))
        .collect();
        let (nodes, _) = Transcripts::default().scan(&projects, SystemTime::now(), &open);
        let shown: Vec<_> = nodes[0].prs.iter().map(|pr| pr.number).collect();
        assert_eq!(
            shown,
            [42, 43, 44],
            "7 it only looked at or found already open, 9 it opened but is merged"
        );
        std::fs::remove_dir_all(projects).unwrap();
    }

    #[test]
    fn a_subagent_shows_from_its_call_until_its_end() {
        let projects =
            std::env::temp_dir().join(format!("claudo-subagents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&projects);
        let folder = projects.join("-src-alpha/s1/subagents");
        std::fs::create_dir_all(&folder).unwrap();
        let call = |id: &str| {
            serde_json::json!({"type": "assistant", "message": {"content": [
                {"type": "tool_use", "id": id, "name": "Agent", "input": {"prompt": "look"}}
            ]}})
            .to_string()
        };
        let result = |id: &str, output: &str, at: &str| {
            serde_json::json!({"type": "user", "timestamp": at, "message": {"content": [
                {"type": "tool_result", "tool_use_id": id, "content": output}
            ]}})
            .to_string()
        };
        // Its end is timed after the files below are written, so it counts as the last word.
        let later = "2099-01-01T00:00:00Z";
        let notification = serde_json::json!({
            "type": "user",
            "timestamp": later,
            "origin": {"kind": "task-notification"},
            "message": {"content": "<task-notification>\n<task-id>quiet</task-id>\n<tool-use-id>c-quiet</tool-use-id>\n<status>completed</status>\n</task-notification>"},
        })
        .to_string();
        let transcript = [
            call("c-front"),
            result("c-front", "It found three callers.", later),
            call("c-back"),
            result("c-back", "Async agent launched successfully.", later),
            call("c-quiet"),
            result("c-quiet", "Async agent launched successfully.", later),
            notification,
            call("c-again"),
            result("c-again", "It answered.", "2020-01-01T00:00:00Z"),
            call("c-batch"),
            result("c-batch", "It started the batch.", later),
            call("c-moved"),
            result("c-moved", "It went on while its long command ran.", later),
            call("c-told"),
            result(
                "c-told",
                "Its command ended, and it was told after a preamble.",
                later,
            ),
        ]
        .join("\n")
            + "\n";
        std::fs::write(projects.join("-src-alpha/s1.jsonl"), transcript).unwrap();
        for (agent, call) in [
            ("front", "c-front"),
            ("back", "c-back"),
            ("quiet", "c-quiet"),
            ("again", "c-again"),
            ("batch", "c-batch"),
            ("told", "c-told"),
            ("moved", "c-moved"),
        ] {
            let own = match agent {
                "batch" => format!(
                    "{}\n",
                    result(
                        "b-run",
                        "Command running in background with ID: b-run.",
                        later
                    )
                ),
                "told" => format!(
                    "{}\n{}\n",
                    result(
                        "b-told",
                        "Command running in background with ID: b-told.",
                        later
                    ),
                    serde_json::json!({"type": "user", "timestamp": later, "message": {"content":
                        "[SYSTEM NOTIFICATION - NOT USER INPUT]\n<task-notification>\n<task-id>b-told</task-id>\n<tool-use-id>b-told</tool-use-id>\n<status>completed</status>\n</task-notification>"}}),
                ),
                "moved" => format!(
                    "{}\n",
                    result(
                        "b-moved",
                        "Command was moved to the background (ID: b-moved) after running for 180s.",
                        later
                    )
                ),
                _ => "{}\n".to_string(),
            };
            std::fs::write(folder.join(format!("agent-{agent}.jsonl")), own).unwrap();
            let meta = serde_json::json!({
                "agentType": "general-purpose",
                "description": format!("Look {agent}"),
                "toolUseId": call,
            });
            std::fs::write(
                folder.join(format!("agent-{agent}.meta.json")),
                meta.to_string(),
            )
            .unwrap();
        }
        let (_, running) =
            Transcripts::default().scan(&projects, SystemTime::now(), &HashMap::new());
        let mut shown: Vec<_> = running
            .iter()
            .map(|node| (node.id.as_str(), node.name.as_deref().unwrap()))
            .collect();
        shown.sort();
        assert_eq!(
            shown,
            [
                ("subagent:again", "Look again"),
                ("subagent:back", "Look back"),
                ("subagent:batch", "Look batch"),
                ("subagent:moved", "Look moved"),
            ],
            "front answered and quiet was notified; again moved on after it answered, and \
             batch waits on a command it left running"
        );
        assert!(
            running
                .iter()
                .all(|node| node.parent.as_deref() == Some("session:s1"))
        );
        std::fs::remove_dir_all(projects).unwrap();
    }

    #[test]
    fn only_a_command_that_runs_gh_pr_create_opens_one() {
        assert!(runs_pr_create("gh pr create --fill"));
        assert!(runs_pr_create(
            "cd ../alpha && git push -u origin HEAD && gh pr create --draft"
        ));
        assert!(runs_pr_create(
            "PR_BODY_FREEFORM=1 gh pr create --body-file b.md"
        ));
        assert!(runs_pr_create("git push\ngh pr create"));
        assert!(!runs_pr_create(
            r#"grep -c "gh pr create" transcript.jsonl"#
        ));
        assert!(!runs_pr_create(
            "python3 -c 'if \"gh pr create\" in line: pass'"
        ));
        assert!(!runs_pr_create("echo gh pr creates"));
        assert!(!runs_pr_create(
            "cat <<'BODY'\ngh pr create --fill\nhttps://github.com/acme/widgets/pull/42\nBODY"
        ));
        assert!(runs_pr_create(
            "cat > b.md <<-EOF\n\tgh pr create\n\tEOF\ngh pr create --body-file b.md"
        ));
        assert!(!runs_pr_create("python3 -c '\ngh pr create\n'"));
        assert!(runs_pr_create("echo \"it's \\\"done\\\"\"; gh pr create"));
        assert!(!runs_pr_create(
            "printf '%s\\n' https://github.com/acme/widgets/pull/42 # ; gh pr create --fill"
        ));
        assert!(runs_pr_create("echo a#b; gh pr create"));
        assert!(runs_pr_create(
            "url=\"$(gh pr create --fill)\"; echo \"$url\""
        ));
        assert!(runs_pr_create("echo \"`gh pr create`\""));
        assert!(!runs_pr_create("echo \"$(echo 'gh pr create')\""));
    }
}
