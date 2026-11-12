//! The dibs machines and the jobs agents run on them, from `dibs --status --json --all`: each
//! machine's name on a line of its own, then, indented under it, its status as one JSON line
//! or what kept it from answering. The pages show every machine as dibstop does, and each job
//! hangs under the agent that asked, which the room finds by name. All of dibs's shapes stay
//! in this file.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use claudo_protocol::{DIBS_POLL, DibsMachine, DibsWatch, Node, NodeKind, NodeState};
use dibstop_view::dibs_format::Mode;
use dibstop_view::dibs_format::status::Status;

use crate::room::Feed;

/// How the machines are watched, which a page may change while the collector waits.
static WATCH: Mutex<Option<DibsWatch>> = Mutex::new(None);
static CHANGED: Condvar = Condvar::new();

/// Reads the machines and their jobs into `feed` for as long as the process runs. Without
/// dibs, there are none.
pub fn spawn(feed: Feed) {
    std::thread::spawn(move || {
        let mut watch = WATCH.lock().unwrap().get_or_insert_with(saved).clone();
        let mut names = Vec::new();
        loop {
            feed.dibs_watch(watch.clone());
            let machines = look(&metrics(&watch, &names))
                .map(|text| machines(&text))
                .unwrap_or_default();
            names = machines
                .iter()
                .map(|machine| machine.name.clone())
                .collect();
            feed.replace(jobs(&machines));
            feed.dibs(machines);
            let now = WATCH.lock().unwrap();
            let (now, _) = CHANGED
                .wait_timeout_while(now, Duration::from_secs(watch.poll), |now| {
                    now.as_ref() == Some(&watch)
                })
                .unwrap();
            watch = now.clone().unwrap_or(watch);
        }
    });
}

/// The machines named in `machines`, or every one without names, with their metrics, for
/// a page anyone may look at: no jobs under agents, and a fixed poll nobody can change.
pub fn spawn_team(feed: Feed, machines: Vec<String>) {
    std::thread::spawn(move || {
        let metrics = match machines.is_empty() {
            true => "all".to_string(),
            false => machines.join(","),
        };
        loop {
            let found = look(&Some(metrics.clone()))
                .map(|text| self::machines(&text))
                .unwrap_or_default();
            let shown = found
                .into_iter()
                .filter(|machine| machines.is_empty() || machines.contains(&machine.name))
                .collect();
            feed.dibs(shown);
            std::thread::sleep(Duration::from_secs(DibsWatch::default().poll));
        }
    });
}

/// Watches as `watch` says from now on, looking again at once, every `poll` seconds kept
/// within `DIBS_POLL`.
pub fn set_watch(mut watch: DibsWatch) {
    watch.poll = watch.poll.clamp(*DIBS_POLL.start(), *DIBS_POLL.end());
    let text = serde_json::to_string(&watch).expect("plain data");
    *WATCH.lock().unwrap() = Some(watch);
    CHANGED.notify_all();
    let path = watch_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(error) = std::fs::write(&path, text) {
        eprintln!("claudo: cannot keep how dibs is watched: {error}");
    }
}

fn saved() -> DibsWatch {
    std::fs::read_to_string(watch_path())
        .ok()
        .and_then(|text| serde_json::from_str::<DibsWatch>(&text).ok())
        .filter(|watch| DIBS_POLL.contains(&watch.poll))
        .unwrap_or_default()
}

fn watch_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_default()
        .join("claudo/dibs-watch.json")
}

/// The `--metrics` argument: every machine, or the ones last seen whose metrics are wanted.
fn metrics(watch: &DibsWatch, names: &[String]) -> Option<String> {
    if watch.no_metrics.is_empty() {
        return Some("all".into());
    }
    let wanted: Vec<&str> = names
        .iter()
        .filter(|name| !watch.no_metrics.contains(name))
        .map(String::as_str)
        .collect();
    (!wanted.is_empty()).then(|| wanted.join(","))
}

/// Both streams, in the order they were written: a machine's trouble goes to stderr, under
/// its name on stdout.
fn look(metrics: &Option<String>) -> Option<String> {
    let mut command = Command::new("sh");
    command.args(["-c", r#"exec dibs --status --json --all "$@" 2>&1"#, "dibs"]);
    if let Some(metrics) = metrics {
        command.args(["--metrics", metrics]);
    }
    let output = command.stdin(Stdio::null()).output().ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn machines(text: &str) -> Vec<DibsMachine> {
    let mut machines: Vec<DibsMachine> = Vec::new();
    let mut lines = text.lines().filter(|raw| !raw.trim().is_empty()).peekable();
    while let Some(raw) = lines.next() {
        let line = raw.trim();
        let named = raw == line && !line.starts_with('{');
        // A machine's name has its status or trouble indented under it; dibs's own notices,
        // such as what changed since it last ran, have nothing.
        if named
            && !lines
                .peek()
                .is_some_and(|next| next.starts_with(char::is_whitespace))
        {
            continue;
        }
        // Without `--all`, as with `DIBS_ON` set, the one machine's status comes alone.
        if machines.is_empty() || named {
            let name = match line.starts_with('{') {
                true => "this machine",
                false => line,
            };
            machines.push(DibsMachine {
                name: name.to_string(),
                status: None,
                trouble: None,
            });
            if !line.starts_with('{') {
                continue;
            }
        }
        let machine = machines.last_mut().expect("pushed above");
        match serde_json::from_str::<Status>(line) {
            Ok(status) => machine.status = Some(status),
            Err(_) => {
                let trouble = machine.trouble.get_or_insert_default();
                if !trouble.is_empty() {
                    trouble.push('\n');
                }
                trouble.push_str(line);
            }
        }
    }
    machines
}

fn jobs(machines: &[DibsMachine]) -> Vec<Node> {
    let mut nodes = Vec::new();
    for machine in machines {
        let Some(status) = &machine.status else {
            continue;
        };
        let name = machine.name.as_str();
        for holder in &status.holders {
            let doing = match holder.mode {
                Mode::Bench => "measuring",
                _ => "running",
            };
            nodes.push(job(
                name,
                parent(&holder.agent, holder.subagent.as_deref()),
                holder.pid,
                holder.label.as_str(),
                false,
                doing,
                holder.elapsed,
            ));
        }
        for waiter in &status.queue {
            nodes.push(job(
                name,
                parent(&waiter.agent, waiter.subagent.as_deref()),
                waiter.pid,
                waiter.label.as_str(),
                true,
                "queued",
                waiter.waiting,
            ));
        }
    }
    nodes
}

/// Under the subagent that started it, which the room falls back from to its agent once
/// that subagent leaves.
fn parent(agent: &str, subagent: Option<&str>) -> String {
    match subagent.filter(|subagent| !subagent.is_empty()) {
        Some(subagent) => format!("subagent:{subagent}|name:{agent}"),
        None => format!("name:{agent}"),
    }
}

fn job(
    machine: &str,
    parent: String,
    pid: u32,
    label: &str,
    queued: bool,
    doing: &str,
    seconds: u64,
) -> Node {
    Node {
        id: format!("dibs:{machine}:{pid}"),
        parent: Some(parent),
        kind: Some(NodeKind::Run("dibs".to_string())),
        name: Some(match label {
            "" => "job".to_string(),
            label => label.to_string(),
        }),
        doing: Some(format!("{doing} on {machine} for {}", took(seconds))),
        state: Some(match queued {
            true => NodeState::Idle,
            false => NodeState::Working,
        }),
        machine: Some(machine.to_string()),
        ..Node::default()
    }
}

fn took(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        _ => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(mode: &str, pid: u32, label: &str, agent: &str, elapsed: u64) -> String {
        format!(
            r#"{{"mode":"{mode}","pid":{pid},"label":"{label}","agent":"{agent}","cmd":"cargo test","started":0,"elapsed":{elapsed},"cpu":0}}"#
        )
    }

    fn status(holders: &[String], queue: &[String]) -> String {
        format!(
            r#"{{"t":0,"state":"shared","cores":8,"load":0,"caches":[],"clones":[],"holders":[{}],"queue":[{}]}}"#,
            holders.join(","),
            queue.join(",")
        )
    }

    #[test]
    fn each_machine_says_its_status_or_why_it_could_not() {
        let waiter = r#"{"position":1,"mode":"bench","pid":42,"label":"widgets-bench","agent":"tuner","subagent":"a7","cmd":"cargo bench","arrived":0,"waiting":30}"#;
        let text = format!(
            "dibs changed since this session last ran it: 1111111 -> 2222222\nabc1234 A fix\nhost-a\n  {}\n\nhost-b\n  ssh: Could not resolve hostname host-b\n  dibs: cannot reach it\nhost-c\n  {}\n",
            status(
                &[holder("shared", 41, "widgets-test", "builder", 125)],
                &[waiter.to_string()]
            ),
            status(&[], &[]),
        );
        let machines = machines(&text);
        let names: Vec<_> = machines.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["host-a", "host-b", "host-c"]);
        assert!(machines[1].status.is_none());
        assert_eq!(
            machines[1].trouble.as_deref(),
            Some("ssh: Could not resolve hostname host-b\ndibs: cannot reach it")
        );
        let nodes = jobs(&machines);
        let shown: Vec<_> = nodes
            .iter()
            .map(|node| {
                (
                    node.id.as_str(),
                    node.parent.as_deref().unwrap(),
                    node.name.as_deref().unwrap(),
                    node.doing.as_deref().unwrap(),
                    node.state.unwrap(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                (
                    "dibs:host-a:41",
                    "name:builder",
                    "widgets-test",
                    "running on host-a for 2m",
                    NodeState::Working
                ),
                (
                    "dibs:host-a:42",
                    "subagent:a7|name:tuner",
                    "widgets-bench",
                    "queued on host-a for 30s",
                    NodeState::Idle
                ),
            ]
        );
    }

    #[test]
    fn metrics_are_asked_of_every_machine_but_the_ones_turned_off() {
        let names = ["alpha".to_string(), "beta".into(), "gamma".into()];
        let watch = DibsWatch::default();
        assert_eq!(metrics(&watch, &names).as_deref(), Some("all"));
        let watch = DibsWatch {
            no_metrics: vec!["beta".into()],
            ..watch
        };
        assert_eq!(metrics(&watch, &names).as_deref(), Some("alpha,gamma"));
        assert_eq!(metrics(&watch, &[]), None);
    }

    #[test]
    fn one_machine_alone_is_read_too() {
        let text = status(&[holder("bench", 7, "widgets-bench", "tuner", 5)], &[]);
        let machines = machines(&text);
        assert_eq!(machines[0].name, "this machine");
        let nodes = jobs(&machines);
        assert_eq!(
            nodes[0].doing.as_deref(),
            Some("measuring on this machine for 5s")
        );
    }
}
