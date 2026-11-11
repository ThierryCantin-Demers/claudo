//! The jobs agents run on dibs machines, from `dibs --status --json`: each machine's name on
//! a line of its own, then its status as one indented JSON line, whose `holders` and `queue`
//! name the agent that asked. A job hangs under that agent, which the room finds by name.
//! All of dibs's shapes stay in this file.

use std::process::{Command, Stdio};
use std::time::Duration;

use claudo_protocol::{Node, NodeKind, NodeState};
use serde_json::Value;

use crate::room::Feed;

/// Each look reaches every machine over ssh, so it is not done often.
const POLL: Duration = Duration::from_secs(30);

/// Reads the jobs into `feed` for as long as the process runs. Without dibs, there are none.
pub fn spawn(feed: Feed) {
    std::thread::spawn(move || {
        loop {
            feed.replace(status().map(|text| jobs(&text)).unwrap_or_default());
            std::thread::sleep(POLL);
        }
    });
}

fn status() -> Option<String> {
    let output = Command::new("dibs")
        .args(["--status", "--json"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn jobs(text: &str) -> Vec<Node> {
    let mut machine = "";
    let mut nodes = Vec::new();
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) && !line.trim().is_empty() {
            machine = line.trim();
            continue;
        }
        let Ok(status) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        for (list, queued) in [("holders", false), ("queue", true)] {
            let listed = status.get(list).and_then(Value::as_array);
            nodes.extend(
                listed
                    .into_iter()
                    .flatten()
                    .filter_map(|job| node(job, machine, queued)),
            );
        }
    }
    nodes
}

fn node(job: &Value, machine: &str, queued: bool) -> Option<Node> {
    let text = |key: &str| {
        job.get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    };
    let agent = text("agent")?;
    let pid = job.get("pid").and_then(Value::as_u64)?;
    let seconds = job
        .get(if queued { "waiting" } else { "elapsed" })
        .and_then(Value::as_u64);
    let mut doing = match (queued, text("mode")) {
        (true, _) => format!("queued on {machine}"),
        (false, Some("bench")) => format!("measuring on {machine}"),
        (false, _) => format!("running on {machine}"),
    };
    if let Some(seconds) = seconds {
        doing.push_str(&format!(" for {}", took(seconds)));
    }
    Some(Node {
        id: format!("dibs:{machine}:{pid}"),
        parent: Some(format!("name:{agent}")),
        kind: Some(NodeKind::Run("dibs".to_string())),
        name: Some(text("label").unwrap_or("job").to_string()),
        doing: Some(doing),
        state: Some(if queued {
            NodeState::Idle
        } else {
            NodeState::Working
        }),
        machine: Some(machine.to_string()),
        ..Node::default()
    })
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

    #[test]
    fn each_job_hangs_under_the_agent_that_asked() {
        let text = concat!(
            "host-a\n",
            r#"  {"state":"shared","holders":[{"mode":"shared","pid":41,"label":"widgets-test","agent":"builder","elapsed":125}],"queue":[{"mode":"bench","pid":42,"label":"widgets-bench","agent":"tuner","waiting":30}]}"#,
            "\n\nhost-b\n",
            "  ssh: Could not resolve hostname host-b\n",
            "host-c\n",
            r#"  {"state":"idle","holders":[],"queue":[]}"#,
            "\n",
        );
        let nodes = jobs(text);
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
                    "name:tuner",
                    "widgets-bench",
                    "queued on host-a for 30s",
                    NodeState::Idle
                ),
            ]
        );
    }
}
