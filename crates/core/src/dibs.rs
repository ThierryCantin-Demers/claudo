//! The dibs machines and the jobs agents run on them, from `dibs --status --json --all`: each
//! machine's name on a line of its own, then, indented under it, its status as one JSON line
//! or what kept it from answering. The pages show every machine as dibstop does, and each job
//! hangs under the agent that asked, which the room finds by name. All of dibs's shapes stay
//! in this file.

use std::process::{Command, Stdio};
use std::time::Duration;

use claudo_protocol::{DibsMachine, Node, NodeKind, NodeState};
use dibstop_view::dibs_format::Mode;
use dibstop_view::dibs_format::status::Status;

use crate::room::Feed;

/// Each look reaches every machine over ssh; dibstop looks this often by default.
const POLL: Duration = Duration::from_secs(10);

/// Reads the machines and their jobs into `feed` for as long as the process runs. Without
/// dibs, there are none.
pub fn spawn(feed: Feed) {
    std::thread::spawn(move || {
        loop {
            let machines = look().map(|text| machines(&text)).unwrap_or_default();
            feed.replace(jobs(&machines));
            feed.dibs(machines);
            std::thread::sleep(POLL);
        }
    });
}

/// Both streams, in the order they were written: a machine's trouble goes to stderr, under
/// its name on stdout.
fn look() -> Option<String> {
    let output = Command::new("sh")
        .args(["-c", "exec dibs --status --json --all 2>&1"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
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
