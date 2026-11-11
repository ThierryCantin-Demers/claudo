//! The agents herdr runs, from its socket: one node per pane with an agent in it. All of
//! herdr's request and reply shapes stay in this file.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use claudo_protocol::{Action, Node, NodeKind, NodeState, Provider};
use serde_json::{Value, json};

use crate::room::Feed;

/// herdr's events have no cursor to resume from, so the snapshot is read again after this
/// long without one.
const FALLBACK: Duration = Duration::from_secs(15);

/// How long to wait before reaching herdr again once it is gone.
const RETRY: Duration = Duration::from_secs(5);

/// Panes coming and going; each agent pane's status is subscribed to by its id.
const LIFECYCLE: [&str; 6] = [
    "pane.created",
    "pane.closed",
    "pane.exited",
    "pane.moved",
    "pane.updated",
    "pane.agent_detected",
];

/// Reads herdr's agents into `feed` for as long as the process runs. While herdr is not
/// running, the room shows none.
pub fn spawn(feed: Feed) {
    std::thread::spawn(move || {
        let mut watcher = Watcher {
            feed,
            machine: hostname(),
            repos: Repos::default(),
        };
        loop {
            if watcher.watch().is_err() {
                watcher.feed.replace(Vec::new());
                std::thread::sleep(RETRY);
            }
        }
    });
}

struct Watcher {
    feed: Feed,
    machine: String,
    repos: Repos,
}

impl Watcher {
    /// Agent names come only from the snapshot, so every event reads it again, until the
    /// agent panes change and need subscribing to.
    fn watch(&mut self) -> io::Result<()> {
        let panes = self.refresh()?;
        let mut events = subscribe(&panes)?;
        loop {
            let mut line = String::new();
            match events.read_line(&mut line) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => return Err(error),
            }
            if self.refresh()? != panes {
                return Ok(());
            }
        }
    }

    /// Gives the room what herdr runs now, and says which panes have agents.
    fn refresh(&mut self) -> io::Result<Vec<String>> {
        let snapshot = snapshot()?;
        let mut panes: Vec<String> = agents(&snapshot)
            .filter_map(|agent| agent.get("pane_id")?.as_str().map(str::to_string))
            .collect();
        panes.sort();
        self.feed
            .replace(nodes(&snapshot, &self.machine, &mut self.repos));
        Ok(panes)
    }
}

/// A second subscription on one connection makes herdr drop it, so this asks for
/// everything at once.
fn subscribe(panes: &[String]) -> io::Result<BufReader<UnixStream>> {
    let mut stream = UnixStream::connect(socket())?;
    stream.set_read_timeout(Some(FALLBACK))?;
    let lifecycle = LIFECYCLE.map(|kind| json!({ "type": kind }));
    let statuses = panes
        .iter()
        .map(|pane| json!({ "type": "pane.agent_status_changed", "pane_id": pane }));
    let subscriptions: Vec<Value> = lifecycle.into_iter().chain(statuses).collect();
    let request = json!({
        "id": "claudo",
        "method": "events.subscribe",
        "params": { "subscriptions": subscriptions },
    });
    writeln!(stream, "{request}")?;
    Ok(BufReader::new(stream))
}

/// `HERDR_SOCKET_PATH`, which herdr sets in its panes, else its default place.
fn socket() -> PathBuf {
    if let Some(path) = std::env::var_os("HERDR_SOCKET_PATH") {
        return path.into();
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))
        .unwrap_or_default();
    config.join("herdr/herdr.sock")
}

/// Does `action` to the agent the node `node` stands for, in its pane on this machine.
pub fn act(node: &str, action: &Action) -> io::Result<()> {
    let target = pane(&snapshot()?, node, &hostname())
        .ok_or_else(|| io::Error::other(format!("no agent pane here for {node}")))?;
    match action {
        Action::Focus => request("agent.focus", json!({ "target": target })),
        Action::Prompt(text) => request("agent.prompt", json!({ "target": target, "text": text })),
    }
    .map(drop)
}

/// The pane an agent node's id names: by its session, or by the pane itself on `machine`.
fn pane(snapshot: &Value, node: &str, machine: &str) -> Option<String> {
    if let Some(pane) = node
        .strip_prefix("pane:")
        .and_then(|rest| rest.strip_prefix(machine)?.strip_prefix(':'))
    {
        return Some(pane.to_string());
    }
    let session = node.strip_prefix("session:")?;
    agents(snapshot)
        .find(|agent| {
            agent
                .pointer("/agent_session/value")
                .and_then(Value::as_str)
                == Some(session)
        })?
        .get("pane_id")?
        .as_str()
        .map(str::to_string)
}

fn snapshot() -> io::Result<Value> {
    request("session.snapshot", json!({}))?
        .get("snapshot")
        .cloned()
        .ok_or_else(|| io::ErrorKind::InvalidData.into())
}

/// herdr answers one request per connection, with its `result` or an `error`.
fn request(method: &str, params: Value) -> io::Result<Value> {
    let mut stream = UnixStream::connect(socket())?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let request = json!({ "id": "claudo", "method": method, "params": params });
    writeln!(stream, "{request}")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let mut reply: Value = serde_json::from_str(&line)?;
    if let Some(error) = reply.get("error") {
        let message = error.get("message").and_then(Value::as_str);
        return Err(io::Error::other(
            message.map_or_else(|| error.to_string(), str::to_string),
        ));
    }
    reply
        .get_mut("result")
        .map(Value::take)
        .ok_or_else(|| io::ErrorKind::InvalidData.into())
}

fn agents(snapshot: &Value) -> impl Iterator<Item = &Value> {
    snapshot
        .get("agents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn nodes(snapshot: &Value, machine: &str, repos: &mut Repos) -> Vec<Node> {
    agents(snapshot)
        .filter_map(|agent| {
            let text = |key: &str| {
                agent
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
            };
            let pane = text("pane_id")?;
            // A session id is the same in the transcript, the board and run folders.
            let id = match agent
                .pointer("/agent_session/value")
                .and_then(Value::as_str)
            {
                Some(session) => format!("session:{session}"),
                None => format!("pane:{machine}:{pane}"),
            };
            Some(Node {
                id,
                kind: text("agent").map(|agent| NodeKind::Agent(provider(agent))),
                name: text("name").map(str::to_string),
                doing: text("terminal_title_stripped").map(str::to_string),
                state: Some(state(text("agent_status").unwrap_or_default())),
                repo: text("cwd").and_then(|cwd| repos.root(cwd)),
                machine: Some(machine.to_string()),
                parent: None,
                note: None,
                context: None,
                answered: None,
                provider: None,
                prs: Vec::new(),
            })
        })
        .collect()
}

fn provider(agent: &str) -> Provider {
    match agent {
        "claude" => Provider::Claude,
        "codex" => Provider::Codex,
        other => Provider::Other(other.to_string()),
    }
}

fn state(status: &str) -> NodeState {
    match status {
        "idle" => NodeState::Idle,
        "working" => NodeState::Working,
        "blocked" => NodeState::Blocked,
        "done" => NodeState::Done,
        _ => NodeState::Unknown,
    }
}

/// The repository each folder is in, looked up once per folder.
#[derive(Default)]
struct Repos(HashMap<String, Option<String>>);

impl Repos {
    fn root(&mut self, cwd: &str) -> Option<String> {
        self.0
            .entry(cwd.to_string())
            .or_insert_with(|| {
                let found = Path::new(cwd)
                    .ancestors()
                    .find(|dir| dir.join(".git").exists());
                found.map(|root| root.to_string_lossy().into_owned())
            })
            .clone()
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_agent_pane_becomes_a_node_in_its_repo() {
        let repo = std::env::temp_dir().join(format!("claudo-herdr-{}", std::process::id()));
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("crates/core")).unwrap();
        let snapshot = json!({ "agents": [
            {
                "pane_id": "w0:p1", "agent": "claude", "name": "builder_hub",
                "agent_status": "blocked", "terminal_title_stripped": "Hub for the daemon",
                "cwd": repo.join("crates/core"),
                "agent_session": { "kind": "id", "value": "0a1b2c3d" },
            },
            { "pane_id": "w1:p2", "agent": "codex", "name": "", "agent_status": "working", "cwd": "/" },
            { "agent": "claude" },
        ]});
        let nodes = nodes(&snapshot, "host-a", &mut Repos::default());
        let _ = std::fs::remove_dir_all(&repo);

        assert_eq!(nodes.len(), 2, "a pane id is what makes an agent");
        let claude = &nodes[0];
        assert_eq!(claude.id, "session:0a1b2c3d");
        assert_eq!(claude.kind, Some(NodeKind::Agent(Provider::Claude)));
        assert_eq!(claude.state, Some(NodeState::Blocked));
        assert_eq!(claude.repo.as_deref(), Some(repo.to_str().unwrap()));
        assert_eq!(claude.doing.as_deref(), Some("Hub for the daemon"));
        let codex = &nodes[1];
        assert_eq!(
            codex.id, "pane:host-a:w1:p2",
            "no session yet: known by its pane"
        );
        assert_eq!(codex.name, None);
        assert_eq!(codex.kind, Some(NodeKind::Agent(Provider::Codex)));
    }

    #[test]
    fn an_action_finds_its_agent_by_session_or_by_pane_on_this_machine() {
        let snapshot = json!({ "agents": [
            { "pane_id": "w0:p1", "agent_session": { "kind": "id", "value": "0a1b2c3d" } },
            { "pane_id": "w1:p2" },
        ]});
        assert_eq!(
            pane(&snapshot, "session:0a1b2c3d", "host-a").as_deref(),
            Some("w0:p1")
        );
        assert_eq!(
            pane(&snapshot, "pane:host-a:w1:p2", "host-a").as_deref(),
            Some("w1:p2")
        );
        assert_eq!(
            pane(&snapshot, "pane:host-b:w1:p2", "host-a"),
            None,
            "another machine's"
        );
        assert_eq!(pane(&snapshot, "session:gone", "host-a"), None);
        assert_eq!(pane(&snapshot, "run:r1", "host-a"), None);
    }
}
