//! How full each Claude agent's context is, from its transcript's last request of the main
//! agent, when the person last wrote to it, and which of the pull requests it opened are
//! still open. A note on `session:<id>`, so only an agent another collector shows gets one.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use claudo_protocol::{Node, PullRequest};

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

/// Reads the transcripts into `feed`, a collector of notes, for as long as the process runs.
pub fn spawn(feed: Feed) {
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
            feed.replace(transcripts.scan(&projects, SystemTime::now(), &open));
            std::thread::sleep(POLL);
        }
    });
}

/// Each recent transcript as last read, with when it was written.
#[derive(Default)]
struct Transcripts {
    read: HashMap<PathBuf, Known>,
}

#[derive(Default)]
struct Known {
    written: Option<SystemTime>,
    tail: Tail,
    opened: Opened,
}

/// The pull requests a session opened, read from the start of its transcript on.
#[derive(Default)]
struct Opened {
    /// How far the transcript has been read, to the end of its last whole line.
    offset: u64,
    /// `gh pr create` calls whose result has not been read yet.
    calls: HashSet<String>,
    links: BTreeSet<String>,
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
    ) -> Vec<Node> {
        let mut read = HashMap::new();
        // A session that moved leaves a copy behind; the newest is the live one.
        let mut sessions: HashMap<String, (SystemTime, Tail, Vec<PullRequest>)> = HashMap::new();
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
                .is_none_or(|(at, _, _)| *at < written);
            if newest {
                sessions.insert(session, (written, tail, prs));
            }
        }
        self.read = read;
        sessions
            .into_iter()
            .filter(|(_, (_, tail, prs))| {
                tail.context.is_some() || tail.answered.is_some() || !prs.is_empty()
            })
            .map(|(session, (_, tail, prs))| Node {
                id: format!("session:{session}"),
                context: tail.context,
                answered: tail.answered,
                prs,
                ..Node::default()
            })
            .collect()
    }
}

/// The pull requests a `gh pr create` printed, leaving out the one it refused to open again
/// for the branch: `a pull request for branch "x" into branch "main" already exists:` and its
/// link on the next line.
fn opened(output: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut refused = false;
    for line in output.lines() {
        let names_existing = line.contains("already exists");
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
            .and_then(|line| {
                let at = line
                    .get("timestamp")?
                    .as_str()?
                    .parse::<jiff::Timestamp>()
                    .ok()?;
                u64::try_from(at.as_second()).ok()
            }),
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
        let nodes = Transcripts::default().scan(&projects, SystemTime::now(), &HashMap::new());
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
            transcripts.scan(&projects, SystemTime::now(), &HashMap::new())[0].context,
            Some(1007)
        );
        let result = format!(
            r#"{{"type":"user","message":{{"content":"{}"}}}}"#,
            "x".repeat(TAIL as usize)
        );
        std::fs::write(&path, format!("{request}\n{result}\n")).unwrap();
        let later = SystemTime::now() + Duration::from_secs(1);
        assert_eq!(
            transcripts.scan(&projects, later, &HashMap::new())[0].context,
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
        let nodes = Transcripts::default().scan(&projects, SystemTime::now(), &HashMap::new());
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
            pr("acme/widgets", 7),
        ]
        .into_iter()
        .map(|pr| (pr.url.clone(), pr))
        .collect();
        let nodes = Transcripts::default().scan(&projects, SystemTime::now(), &open);
        let shown: Vec<_> = nodes[0].prs.iter().map(|pr| pr.number).collect();
        assert_eq!(
            shown,
            [42, 43],
            "7 it only looked at or found already open, 9 it opened but is merged"
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
