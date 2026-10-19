//! Manual probe for what a restart hands over besides a running command: an approval
//! waiting for its answer, a message queued behind a running call, a command in the
//! background and a forked command's subagent. Each held command reads a fifo, so the new
//! process decides when it ends.
//! Usage: cargo run -p claudo-core --example handover_cases_probe

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use claudo_core::backend::Backend;
use claudo_protocol::{Command, Decision, Effort, Profile, ServerEvent, TaskStatus};
use tokio::sync::mpsc::unbounded_channel;

const CASES: [&str; 4] = ["approval", "queued", "background", "forked"];

fn main() {
    let tmp = std::env::temp_dir().join("claudo_handover_cases_probe");
    // A probe stuck on a command that never started fails instead of hanging.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(300));
        println!("FAIL timed out");
        std::process::exit(1);
    });
    if std::env::var_os(claudo_core::handover::FILE_VAR).is_none() {
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // SAFETY: no thread reads the environment yet. The exec'd process inherits it.
        unsafe { std::env::set_var("XDG_DATA_HOME", &tmp) };
        before(&tmp);
    } else {
        after(&tmp);
    }
}

fn fifo(tmp: &Path, case: &str) -> PathBuf {
    tmp.join(format!("{case}.fifo"))
}

fn before(tmp: &Path) {
    for case in &CASES[1..] {
        let made = std::process::Command::new("mkfifo")
            .arg(fifo(tmp, case))
            .status();
        assert!(made.is_ok_and(|s| s.success()), "mkfifo");
    }
    let project = tmp.join("project");
    let skill = project.join(".claude/skills/probefork");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        format!(
            "---\nname: probefork\ndescription: Runs one command in a forked context, for a \
             claudo probe.\ncontext: fork\nmodel: haiku\n---\n\nRun this exact bash command \
             with the Bash tool, not in the background: cat {}\nThen reply with the single \
             word forked.\n",
            fifo(tmp, "forked").display()
        ),
    )
    .unwrap();

    let (tx, mut rx) = unbounded_channel::<ServerEvent>();
    let backend = Backend::new(tx);
    for request in 0..3 {
        backend.handle(Command::NewScratch { request });
    }
    backend.handle(Command::NewAttached {
        path: project.display().to_string(),
        request: 3,
    });
    let mut ids = HashMap::new();
    while ids.len() < CASES.len() {
        if let Some(ServerEvent::Created {
            conversation_id,
            request,
            ..
        }) = rx.blocking_recv()
        {
            ids.insert(CASES[request as usize], conversation_id);
        }
    }
    let send = |case: &str, text: String| backend.handle(send(&ids[case], &text));
    send(
        "approval",
        "Run exactly this command with the Bash tool: gh pr create --help. Then reply done."
            .to_string(),
    );
    send(
        "queued",
        format!(
            "Run this exact bash command with the Bash tool, not in the background, then reply \
             with the single word first: cat {}",
            fifo(tmp, "queued").display()
        ),
    );
    send(
        "background",
        format!(
            "Run this exact bash command with the Bash tool with run_in_background set to true, \
             then reply with the single word started: cat {}",
            fifo(tmp, "background").display()
        ),
    );
    send("forked", "/probefork".to_string());

    let case = |id: &str| ids.iter().find(|(_, c)| *c == id).map(|(case, _)| *case);
    let mut ready: Vec<&str> = Vec::new();
    while ready.len() < CASES.len() {
        let Some(ev) = rx.blocking_recv() else {
            break;
        };
        let now = match &ev {
            ServerEvent::ApprovalRequested {
                conversation_id, ..
            } => case(conversation_id).filter(|c| *c == "approval"),
            ServerEvent::ToolStarted {
                conversation_id,
                call,
            } if call.name == "Bash" => {
                case(conversation_id).filter(|c| matches!(*c, "queued" | "forked"))
            }
            ServerEvent::Completed {
                conversation_id, ..
            } => case(conversation_id).filter(|c| *c == "background"),
            ServerEvent::Error { detail, .. } => {
                println!("  error: {detail}");
                None
            }
            _ => None,
        };
        if let Some(now) = now.filter(|now| !ready.contains(now)) {
            println!("  {now} is ready");
            if now == "queued" {
                send("queued", "Reply with the single word second.".to_string());
            }
            ready.push(now);
        }
    }
    let saved: Vec<String> = CASES
        .iter()
        .map(|case| format!("{case} {}", ids[case]))
        .collect();
    std::fs::write(tmp.join("cases"), saved.join("\n")).unwrap();
    println!("every case is under way; restarting");
    backend.handle(Command::Restart);
    while let Some(ev) = rx.blocking_recv() {
        if let ServerEvent::Error { detail, .. } = ev {
            println!("FAIL the restart failed: {detail}");
            break;
        }
    }
    std::process::exit(1);
}

fn after(tmp: &Path) {
    let ids: HashMap<String, String> = std::fs::read_to_string(tmp.join("cases"))
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(case, id)| (id.to_string(), case.to_string()))
        .collect();
    let (tx, mut rx) = unbounded_channel::<ServerEvent>();
    let backend = Backend::new(tx);
    for case in &CASES[1..] {
        let fifo = fifo(tmp, case);
        std::thread::spawn(move || std::fs::write(fifo, "go\n"));
    }

    let mut seen: HashMap<String, Vec<String>> = HashMap::new();
    let mut replies: HashMap<String, String> = HashMap::new();
    let mut completed = Vec::new();
    while completed.len() < CASES.len() {
        let Some(ev) = rx.blocking_recv() else {
            break;
        };
        let Some(case) = conversation(&ev).and_then(|id| ids.get(id)).cloned() else {
            continue;
        };
        let what = match &ev {
            ServerEvent::ApprovalRequested { request_id, .. } => {
                backend.handle(Command::AnswerApproval {
                    conversation_id: conversation(&ev).unwrap().to_string(),
                    request_id: request_id.clone(),
                    decision: Decision::Deny,
                });
                "asked again".to_string()
            }
            ServerEvent::Queued { text, .. } => format!("queued {text:?}"),
            ServerEvent::Delivered { .. } => "delivered".to_string(),
            ServerEvent::Background { background, .. }
                if background.status != TaskStatus::Running =>
            {
                "background ended".to_string()
            }
            ServerEvent::Notice { .. } => "notice".to_string(),
            ServerEvent::ToolResult { .. } => "call ended".to_string(),
            ServerEvent::Delta { text, .. } => {
                replies.entry(case.clone()).or_default().push_str(text);
                continue;
            }
            ServerEvent::Completed { .. } => {
                completed.push(case.clone());
                "completed".to_string()
            }
            ServerEvent::Error { detail, .. } => format!("error {detail}"),
            _ => continue,
        };
        println!("  {case}: {what}");
        seen.entry(case).or_default().push(what);
    }

    let mut failed = false;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        failed |= !ok;
    };
    let saw = |case: &str, what: &str| {
        seen.get(case)
            .is_some_and(|events| events.iter().any(|e| e.starts_with(what)))
    };
    let replied = |case: &str| {
        replies
            .get(case)
            .cloned()
            .unwrap_or_default()
            .to_lowercase()
    };
    check(
        saw("approval", "asked again") && saw("approval", "completed"),
        "a pending approval is asked again, and its answer ends the turn",
    );
    check(
        saw("queued", "queued \"Reply with the single word second.\""),
        "a queued message is shown as queued again",
    );
    check(
        saw("queued", "delivered") && saw("queued", "completed"),
        "it reaches Claude once the running call ends",
    );
    check(
        saw("background", "background ended") && saw("background", "notice"),
        "a background command's end is heard, and starts a turn",
    );
    check(
        saw("forked", "call ended") && replied("forked").contains("forked"),
        "a forked command's subagent is still followed, and its result comes through",
    );
    std::process::exit(i32::from(failed));
}

fn conversation(ev: &ServerEvent) -> Option<&str> {
    match ev {
        ServerEvent::ApprovalRequested {
            conversation_id, ..
        }
        | ServerEvent::Queued {
            conversation_id, ..
        }
        | ServerEvent::Delivered { conversation_id }
        | ServerEvent::Background {
            conversation_id, ..
        }
        | ServerEvent::Notice {
            conversation_id, ..
        }
        | ServerEvent::ToolResult {
            conversation_id, ..
        }
        | ServerEvent::Delta {
            conversation_id, ..
        }
        | ServerEvent::Completed {
            conversation_id, ..
        } => Some(conversation_id),
        ServerEvent::Error {
            conversation_id, ..
        } => conversation_id.as_deref(),
        _ => None,
    }
}

fn send(id: &str, text: &str) -> Command {
    Command::SendMessage {
        conversation_id: id.to_string(),
        text: text.to_string(),
        profile: Profile::FullAuto,
        model: "haiku".to_string(),
        effort: Effort::Default,
        attachments: Vec::new(),
    }
}
