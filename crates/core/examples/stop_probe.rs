//! Manual probe for stopping a turn: mid-reply and while an approval waits. Either way
//! the turn ends without an error and the conversation carries on.
//! Usage: cargo run -p claudo-core --example stop_probe

use claudo_core::backend::Backend;
use claudo_protocol::{Command, Effort, Profile, ServerEvent};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

#[derive(Clone, Copy, PartialEq)]
enum StopOn {
    Text,
    Approval,
    Never,
}

fn main() {
    let tmp = std::env::temp_dir().join("claudo_stop_probe");
    let _ = std::fs::remove_dir_all(&tmp);
    // SAFETY: no other thread exists yet.
    unsafe { std::env::set_var("XDG_DATA_HOME", &tmp) };

    let (tx, mut rx) = unbounded_channel();
    let backend = Backend::new(tx);
    backend.handle(Command::NewScratch);
    let id = loop {
        if let Some(ServerEvent::Conversations {
            current: Some(id), ..
        }) = rx.blocking_recv()
        {
            break id;
        }
    };

    let mut failed = false;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        failed |= !ok;
    };

    let essay = turn(
        &backend,
        &mut rx,
        &id,
        "Write a 600 word essay about the history of bread.",
        StopOn::Text,
    );
    check(
        essay.completed && !essay.is_error,
        "stopping mid-reply ends the turn cleanly",
    );
    check(
        essay.text.split_whitespace().count() < 450,
        "the reply stops well short of the 600 words asked for",
    );

    let next = turn(
        &backend,
        &mut rx,
        &id,
        "Reply with just the word ok.",
        StopOn::Never,
    );
    check(
        next.text.to_lowercase().contains("ok"),
        "the next turn works",
    );

    let pending = turn(
        &backend,
        &mut rx,
        &id,
        "Run exactly this command with the Bash tool: gh pr create --help",
        StopOn::Approval,
    );
    check(pending.approvals == 1, "the approval was requested");
    check(
        pending.completed && !pending.is_error,
        "stopping during an approval ends the turn cleanly",
    );
    check(!pending.ran, "the stopped call did not run");

    let after = turn(
        &backend,
        &mut rx,
        &id,
        "Reply with just the word ok.",
        StopOn::Never,
    );
    check(
        after.text.to_lowercase().contains("ok"),
        "the turn after that works",
    );

    std::process::exit(i32::from(failed));
}

#[derive(Default)]
struct Turn {
    text: String,
    approvals: usize,
    ran: bool,
    completed: bool,
    is_error: bool,
}

fn turn(
    backend: &Backend,
    rx: &mut UnboundedReceiver<ServerEvent>,
    id: &str,
    prompt: &str,
    stop_on: StopOn,
) -> Turn {
    println!("> {prompt}");
    backend.handle(Command::SendMessage {
        conversation_id: id.to_string(),
        text: prompt.to_string(),
        profile: Profile::Safe,
        model: "haiku".to_string(),
        effort: Effort::Default,
    });
    let stop = || {
        backend.handle(Command::Stop {
            conversation_id: id.to_string(),
        })
    };
    let mut seen = Turn::default();
    let mut stopped = false;
    while let Some(event) = rx.blocking_recv() {
        match event {
            ServerEvent::Delta { text, .. } => {
                seen.text.push_str(&text);
                if stop_on == StopOn::Text && !stopped {
                    stopped = true;
                    stop();
                }
            }
            ServerEvent::ApprovalRequested { .. } => {
                seen.approvals += 1;
                if stop_on == StopOn::Approval {
                    stop();
                }
            }
            ServerEvent::ToolResult { is_error, .. } => seen.ran |= !is_error,
            ServerEvent::Completed { is_error, .. } => {
                seen.completed = true;
                seen.is_error = is_error;
                break;
            }
            ServerEvent::Error { detail } => {
                println!("  error: {detail}");
                break;
            }
            _ => {}
        }
    }
    println!(
        "  < {} words, completed={}",
        seen.text.split_whitespace().count(),
        seen.completed
    );
    seen
}
