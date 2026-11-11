//! Codex reviews and explorations, and the other runs agents start, from their folders under
//! `agent-runs`. Each folder's `meta.json` names the session of the agent that started it, so
//! the run sits under that agent in the room. All of the folder's shapes stay in this file.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use claudo_protocol::{Node, NodeKind, NodeState, Provider};
use serde_json::Value;

use crate::room::Feed;

/// How often the folders are looked at. A `meta.json` is read again only once it changed.
const POLL: Duration = Duration::from_secs(3);

/// How long a failed run stays in the room after it last changed. One that ended well
/// leaves at once: still drawn under its agent, it reads as still going.
const LINGER: Duration = Duration::from_secs(30 * 60);

/// A run that keeps no lock, still marked running after this long without a change, died
/// without saying so.
const STALE: Duration = Duration::from_secs(12 * 60 * 60);

/// How long a finished review stays for its digest to be read.
const KEPT: Duration = Duration::from_secs(12 * 60 * 60);

/// Reads the runs in `$CLAUDO_AGENT_RUNS`, or else `agent-runs` in the XDG state directory,
/// into `feed` for as long as the process runs.
pub fn spawn(feed: Feed) {
    let Some(root) = root() else {
        return;
    };
    std::thread::spawn(move || {
        let mut runs = Runs::default();
        loop {
            feed.replace(runs.scan(&root, SystemTime::now()));
            std::thread::sleep(POLL);
        }
    });
}

fn root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDO_AGENT_RUNS") {
        return Some(dir.into());
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/state")))?;
    Some(state.join("agent-runs"))
}

/// Each run's `meta.json` as last read, with when it was written, and the same for each
/// finished review's `digest.md`.
#[derive(Default)]
struct Runs {
    read: HashMap<PathBuf, (SystemTime, Option<Value>)>,
    digests: HashMap<PathBuf, (SystemTime, String)>,
}

impl Runs {
    fn scan(&mut self, root: &Path, now: SystemTime) -> Vec<Node> {
        let Ok(folders) = std::fs::read_dir(root) else {
            self.read.clear();
            return Vec::new();
        };
        let mut read = HashMap::new();
        let mut digests = HashMap::new();
        let mut nodes = Vec::new();
        let locked = locked_inodes();
        for folder in folders.flatten() {
            let path = folder.path().join("meta.json");
            let Ok(written) = std::fs::metadata(&path).and_then(|file| file.modified()) else {
                continue;
            };
            // A file read mid-write fails to parse; the write that finishes it changes the time.
            let meta = match self.read.remove(&path) {
                Some((at, meta)) if at == written => meta,
                _ => std::fs::read(&path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok()),
            };
            let quiet = now.duration_since(written).unwrap_or_default();
            if let Some(meta) = &meta {
                let digest = match status(meta) {
                    "done" if quiet < KEPT => {
                        self.digest(&folder.path().join("digest.md"), &mut digests)
                    }
                    _ => None,
                };
                let shown = match status(meta) {
                    "running" => match held(&folder.path(), &locked) {
                        Some(held) => held,
                        None => quiet < STALE,
                    },
                    "waiting-parent" => quiet < STALE,
                    "failed" => quiet < LINGER,
                    "done" => digest.is_some() && quiet < KEPT,
                    _ => false,
                };
                if shown {
                    let name = folder.file_name();
                    nodes.extend(run(&name.to_string_lossy(), meta, digest));
                }
            }
            read.insert(path, (written, meta));
        }
        self.read = read;
        self.digests = digests;
        nodes
    }

    /// The digest at `path`, read again only once it changed, kept in `digests`.
    fn digest(
        &mut self,
        path: &Path,
        digests: &mut HashMap<PathBuf, (SystemTime, String)>,
    ) -> Option<String> {
        let written = std::fs::metadata(path)
            .and_then(|file| file.modified())
            .ok()?;
        let text = match self.digests.remove(path) {
            Some((at, text)) if at == written => text,
            _ => tidy(&std::fs::read_to_string(path).ok()?),
        };
        digests.insert(path.to_path_buf(), (written, text.clone()));
        Some(text)
    }
}

/// Whether the run's driver still holds the lock it keeps on `.lock` while it runs, which
/// one killed does not let go of in its `meta.json`. None for a run that keeps no lock.
fn held(folder: &Path, locked: &HashSet<u64>) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;
    let lock = std::fs::metadata(folder.join(".lock")).ok()?;
    Some(locked.contains(&lock.ino()))
}

/// The inodes of the files locked now, from `/proc/locks`, read rather than tried: taking
/// the lock even for a moment could turn away a run starting then.
fn locked_inodes() -> HashSet<u64> {
    let locks = std::fs::read_to_string("/proc/locks").unwrap_or_default();
    locks
        .lines()
        .filter_map(|line| {
            let file = line
                .split_whitespace()
                .find(|word| word.matches(':').count() == 2)?;
            file.rsplit(':').next()?.parse().ok()
        })
        .collect()
}

fn status(meta: &Value) -> &str {
    meta.get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// The run, then each Codex sub-agent it fanned out to under it.
fn run(folder: &str, meta: &Value, digest: Option<String>) -> Vec<Node> {
    let text = |pointer: &str| {
        meta.pointer(pointer)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    };
    let id = format!("run:{folder}");
    let status = status(meta);
    let state = match status {
        "running" => NodeState::Working,
        "waiting-parent" => NodeState::Idle,
        "done" | "cancelled" => NodeState::Done,
        "failed" => NodeState::Failed,
        _ => NodeState::Unknown,
    };
    let rounds = match (
        meta.get("round").and_then(Value::as_u64),
        meta.get("max_rounds").and_then(Value::as_u64),
    ) {
        (Some(round), Some(most)) => Some(format!("round {round} of {most}")),
        _ => None,
    };
    let rounds = [rounds, pace(meta)]
        .into_iter()
        .flatten()
        .reduce(|rounds, pace| format!("{rounds}, {pace}"));
    let doing = match status {
        "waiting-parent" => Some(match &rounds {
            Some(rounds) => format!("{rounds}, waiting on its agent"),
            None => "waiting on its agent".to_string(),
        }),
        "failed" => text("/error")
            .and_then(|error| error.lines().next())
            .map(str::to_string)
            .or(rounds),
        "cancelled" => Some("cancelled".to_string()),
        "done" => meta
            .get("round")
            .and_then(Value::as_u64)
            .map(|round| match round {
                1 => "done after 1 round".to_string(),
                _ => format!("done after {round} rounds"),
            }),
        _ => rounds,
    };
    let machine = text("/parent/machine").map(str::to_string);
    let provider = text("/provider").map(|provider| match provider {
        "codex" => Provider::Codex,
        "claude" => Provider::Claude,
        other => Provider::Other(other.to_string()),
    });
    let mut nodes = vec![Node {
        id: id.clone(),
        parent: text("/parent/session").map(|session| match text("/parent/subagent") {
            Some(subagent) => format!("subagent:{subagent}|session:{session}"),
            None => format!("session:{session}"),
        }),
        kind: text("/kind").map(|kind| NodeKind::Run(kind.to_string())),
        name: text("/title").map(str::to_string),
        doing,
        state: Some(state),
        repo: text("/repo").map(str::to_string),
        machine: machine.clone(),
        note: None,
        context: None,
        answered: None,
        provider: provider.clone(),
        prs: Vec::new(),
        digest,
    }];
    let subagents = meta.get("subagents").and_then(Value::as_array);
    for (i, subagent) in subagents.into_iter().flatten().enumerate() {
        let role = subagent
            .get("path")
            .and_then(Value::as_str)
            .and_then(|path| path.rsplit('/').next())
            .filter(|role| !role.is_empty());
        nodes.push(Node {
            id: format!("{id}/{i}"),
            parent: Some(id.clone()),
            kind: Some(NodeKind::Subagent),
            name: role.map(str::to_string),
            doing: None,
            state: Some(state),
            repo: None,
            machine: machine.clone(),
            note: None,
            context: None,
            answered: None,
            provider: provider.clone(),
            prs: Vec::new(),
            digest: None,
        });
    }
    nodes
}

/// A digest without the sections that hold nothing, which an older `codex-review` wrote as
/// bare headings.
fn tidy(digest: &str) -> String {
    let lines: Vec<&str> = digest.lines().collect();
    let mut kept = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("## ") {
            let next = lines[i + 1..].iter().find(|line| !line.trim().is_empty());
            if next.is_none_or(|next| next.starts_with("## ")) {
                continue;
            }
        }
        kept.push(*line);
    }
    let mut tidied = kept.join("\n");
    tidied.push('\n');
    tidied
}

/// How a review stands against the Codex week: a deep round run quick because the week was
/// past its pace, and the share of the week used against the share allowed so far.
fn pace(meta: &Value) -> Option<String> {
    let percent = |pointer: &str| meta.pointer(pointer).and_then(Value::as_f64);
    let budget = percent("/budget/used")
        .zip(percent("/budget/allowed"))
        .map(|(used, allowed)| format!("Codex week at {used:.0}% of {allowed:.0}%"));
    let quick = meta.get("round_depth").and_then(Value::as_str) == Some("quick");
    match (quick, budget) {
        (true, Some(budget)) => Some(format!("ran quick, {budget}")),
        (true, None) => Some("ran quick, past the Codex week's pace".to_string()),
        (false, budget) => budget,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn folder(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("claudo-runs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    fn write(root: &Path, run: &str, meta: Value) {
        std::fs::create_dir_all(root.join(run)).unwrap();
        std::fs::write(root.join(run).join("meta.json"), meta.to_string()).unwrap();
    }

    fn review(status: &str) -> Value {
        json!({
            "kind": "review",
            "provider": "codex",
            "title": "Review alpha",
            "parent": {"agent": "builder", "session": "s1", "machine": "host-a"},
            "repo": "/src/alpha",
            "round": 2,
            "max_rounds": 3,
            "status": status,
            "subagents": [
                {"path": "/root/correctness"},
                {"path": "/root/tests"},
            ],
        })
    }

    #[test]
    fn a_review_sits_under_its_agent_with_its_reviewers_under_it() {
        let root = folder("tree");
        write(&root, "r1", review("running"));
        let nodes = Runs::default().scan(&root, SystemTime::now());
        let ids: Vec<_> = nodes.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(ids, ["run:r1", "run:r1/0", "run:r1/1"]);
        let run = &nodes[0];
        assert_eq!(run.parent.as_deref(), Some("session:s1"));
        let mut started = review("running");
        started["parent"]["subagent"] = json!("a1");
        let from_subagent = super::run("r2", &started, None);
        assert_eq!(
            from_subagent[0].parent.as_deref(),
            Some("subagent:a1|session:s1"),
            "one a subagent started"
        );
        assert_eq!(run.kind, Some(NodeKind::Run("review".to_string())));
        assert_eq!(run.state, Some(NodeState::Working));
        assert_eq!(run.doing.as_deref(), Some("round 2 of 3"));
        assert_eq!(run.machine.as_deref(), Some("host-a"));
        assert_eq!(nodes[1].parent.as_deref(), Some("run:r1"));
        assert_eq!(run.provider, Some(Provider::Codex));
        assert_eq!(
            nodes[1].provider,
            Some(Provider::Codex),
            "a reviewer it fanned out to"
        );
        assert_eq!(nodes[1].name.as_deref(), Some("correctness"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_run_shows_while_it_runs_and_a_failed_one_for_a_while() {
        let root = folder("linger");
        write(&root, "ended", review("done"));
        write(&root, "failed", review("failed"));
        write(&root, "waiting", review("waiting-parent"));
        let mut runs = Runs::default();
        let shown = |nodes: Vec<Node>| -> Vec<String> {
            nodes
                .into_iter()
                .filter(|node| node.parent.as_deref() == Some("session:s1"))
                .map(|node| node.id)
                .collect()
        };
        let mut now = shown(runs.scan(&root, SystemTime::now()));
        now.sort();
        assert_eq!(
            now,
            ["run:failed", "run:waiting"],
            "one that ended well is gone"
        );
        let later = SystemTime::now() + LINGER + Duration::from_secs(60);
        let nodes = runs.scan(&root, later);
        assert_eq!(
            nodes[0].doing.as_deref(),
            Some("round 2 of 3, waiting on its agent")
        );
        assert_eq!(shown(nodes), ["run:waiting"]);
        assert!(
            runs.scan(&root, later + STALE).is_empty(),
            "a run that died quietly"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_finished_review_stays_while_it_has_a_digest() {
        let root = folder("digest");
        write(&root, "read", review("done"));
        write(&root, "unread", review("done"));
        std::fs::write(root.join("read/digest.md"), "# Review of alpha, main\n").unwrap();
        let mut runs = Runs::default();
        let nodes = runs.scan(&root, SystemTime::now());
        let ids: Vec<_> = nodes.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(ids, ["run:read", "run:read/0", "run:read/1"]);
        assert_eq!(nodes[0].state, Some(NodeState::Done));
        assert_eq!(nodes[0].doing.as_deref(), Some("done after 2 rounds"));
        assert_eq!(
            nodes[0].digest.as_deref(),
            Some("# Review of alpha, main\n")
        );
        assert!(runs.scan(&root, SystemTime::now() + KEPT).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_digest_leaves_out_its_empty_sections() {
        let digest = "# Review of alpha, main\n\nNo findings.\n## Fixed\n\n- F1 (fixed): a.\n\n## Disputed\n\n\n## Left open\n";
        assert_eq!(
            tidy(digest),
            "# Review of alpha, main\n\nNo findings.\n## Fixed\n\n- F1 (fixed): a.\n\n\n\n"
        );
    }

    #[test]
    fn a_review_says_how_it_stands_against_the_codex_week() {
        let root = folder("pace");
        let mut quick = review("running");
        quick["round_depth"] = json!("quick");
        quick["budget"] = json!({"used": 34.4, "allowed": 40});
        write(&root, "quick", quick);
        let mut deep = review("waiting-parent");
        deep["budget"] = json!({"used": 12, "allowed": 40});
        write(&root, "deep", deep);
        let doing: BTreeMap<_, _> = Runs::default()
            .scan(&root, SystemTime::now())
            .into_iter()
            .filter_map(|node| Some((node.id, node.doing?)))
            .collect();
        assert_eq!(
            doing["run:quick"],
            "round 2 of 3, ran quick, Codex week at 34% of 40%"
        );
        assert_eq!(
            doing["run:deep"],
            "round 2 of 3, Codex week at 12% of 40%, waiting on its agent"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_folder_without_runs_shows_none() {
        let root = folder("missing");
        assert!(Runs::default().scan(&root, SystemTime::now()).is_empty());
        std::fs::create_dir_all(root.join("half")).unwrap();
        std::fs::write(root.join("half/meta.json"), "{\"kind\":").unwrap();
        assert!(Runs::default().scan(&root, SystemTime::now()).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_run_marked_running_shows_only_while_its_lock_is_held() {
        use std::os::unix::io::AsRawFd;
        let root = folder("held");
        write(&root, "live", review("running"));
        write(&root, "killed", review("running"));
        let lock = std::fs::File::create(root.join("live/.lock")).unwrap();
        std::fs::File::create(root.join("killed/.lock")).unwrap();
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
        let runs: Vec<_> = Runs::default()
            .scan(&root, SystemTime::now())
            .into_iter()
            .filter(|node| node.parent.as_deref() == Some("session:s1"))
            .map(|node| node.id)
            .collect();
        assert_eq!(runs, ["run:live"]);
        drop(lock);
        std::fs::remove_dir_all(root).unwrap();
    }
}
