//! Codex reviews and explorations, and the other runs agents start, from their folders under
//! `agent-runs`. Each folder's `meta.json` names the session of the agent that started it, so
//! the run sits under that agent in the room. All of the folder's shapes stay in this file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use claudo_protocol::{Node, NodeKind, NodeState};
use serde_json::Value;

use crate::room::Feed;

/// How often the folders are looked at. A `meta.json` is read again only once it changed.
const POLL: Duration = Duration::from_secs(3);

/// How long a run stays in the room after it last changed, once it no longer runs.
const LINGER: Duration = Duration::from_secs(30 * 60);

/// A run still marked running after this long without a change died without saying so.
const STALE: Duration = Duration::from_secs(12 * 60 * 60);

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

/// Each run's `meta.json` as last read, with when it was written.
#[derive(Default)]
struct Runs {
    read: HashMap<PathBuf, (SystemTime, Option<Value>)>,
}

impl Runs {
    fn scan(&mut self, root: &Path, now: SystemTime) -> Vec<Node> {
        let Ok(folders) = std::fs::read_dir(root) else {
            self.read.clear();
            return Vec::new();
        };
        let mut read = HashMap::new();
        let mut nodes = Vec::new();
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
                let running = matches!(status(meta), "running" | "waiting-parent");
                if quiet < LINGER || (running && quiet < STALE) {
                    let name = folder.file_name();
                    nodes.extend(run(&name.to_string_lossy(), meta));
                }
            }
            read.insert(path, (written, meta));
        }
        self.read = read;
        nodes
    }
}

fn status(meta: &Value) -> &str {
    meta.get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// The run, then each Codex sub-agent it fanned out to under it.
fn run(folder: &str, meta: &Value) -> Vec<Node> {
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
        _ => rounds,
    };
    let machine = text("/parent/machine").map(str::to_string);
    let mut nodes = vec![Node {
        id: id.clone(),
        parent: text("/parent/session").map(|session| format!("session:{session}")),
        kind: text("/kind").map(|kind| NodeKind::Run(kind.to_string())),
        name: text("/title").map(str::to_string),
        doing,
        state: Some(state),
        repo: text("/repo").map(str::to_string),
        machine: machine.clone(),
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
        });
    }
    nodes
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert_eq!(run.kind, Some(NodeKind::Run("review".to_string())));
        assert_eq!(run.state, Some(NodeState::Working));
        assert_eq!(run.doing.as_deref(), Some("round 2 of 3"));
        assert_eq!(run.machine.as_deref(), Some("host-a"));
        assert_eq!(nodes[1].parent.as_deref(), Some("run:r1"));
        assert_eq!(nodes[1].name.as_deref(), Some("correctness"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_ended_run_leaves_after_a_while_and_a_running_one_stays() {
        let root = folder("linger");
        write(&root, "ended", review("done"));
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
        assert_eq!(now, ["run:ended", "run:waiting"]);
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
    fn a_folder_without_runs_shows_none() {
        let root = folder("missing");
        assert!(Runs::default().scan(&root, SystemTime::now()).is_empty());
        std::fs::create_dir_all(root.join("half")).unwrap();
        std::fs::write(root.join("half/meta.json"), "{\"kind\":").unwrap();
        assert!(Runs::default().scan(&root, SystemTime::now()).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
