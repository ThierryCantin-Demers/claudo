//! Manual probe for approvals: issue and PR creation waits for an answer under every
//! profile, while other calls Safe mode would prompt for are still refused.
//! `gh pr create --help` stands in for a real PR: it matches the rule but creates nothing.
//! Usage: cargo run -p claudo-core --example approval_probe

use claudo_core::backend::Backend;
use claudo_protocol::{Command, Effort, Profile, ServerEvent};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

const GH: &str =
    "Run exactly this command with the Bash tool: gh pr create --help. Then reply done.";

fn main() {
    let tmp = std::env::temp_dir().join("claudo_approval_probe");
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

    let write = turn(
        &backend,
        &mut rx,
        &id,
        Profile::Safe,
        "Use the Write tool to create probe.txt containing hi. Then reply done.",
        true,
    );
    check(
        write.approvals.is_empty(),
        "Safe mode write is not offered for approval",
    );
    check(
        write.errors.iter().any(|e| e.contains("haven't granted")),
        "Safe mode write is refused",
    );

    let allowed = turn(&backend, &mut rx, &id, Profile::Safe, GH, true);
    check(
        allowed
            .approvals
            .iter()
            .any(|s| s.starts_with("gh pr create")),
        "PR creation asks for approval in Safe mode",
    );
    check(
        allowed.outputs.iter().any(|o| o.contains("pull request")),
        "an allowed call runs",
    );

    let denied = turn(&backend, &mut rx, &id, Profile::FullAuto, GH, false);
    check(
        !denied.approvals.is_empty(),
        "PR creation asks for approval in Full auto",
    );
    check(
        denied.errors.iter().any(|e| e.contains("denied")),
        "a denied call does not run",
    );

    std::process::exit(i32::from(failed));
}

#[derive(Default)]
struct Turn {
    approvals: Vec<String>,
    outputs: Vec<String>,
    errors: Vec<String>,
}

fn turn(
    backend: &Backend,
    rx: &mut UnboundedReceiver<ServerEvent>,
    id: &str,
    profile: Profile,
    prompt: &str,
    allow: bool,
) -> Turn {
    println!("> [{}] {prompt}", profile.label());
    backend.handle(Command::SendMessage {
        conversation_id: id.to_string(),
        text: prompt.to_string(),
        profile,
        model: "haiku".to_string(),
        effort: Effort::Default,
    });
    let mut seen = Turn::default();
    while let Some(event) = rx.blocking_recv() {
        match event {
            ServerEvent::ApprovalRequested {
                conversation_id,
                request_id,
                call,
            } => {
                println!("  approval requested: {} ({})", call.summary, call.name);
                seen.approvals.push(call.summary);
                backend.handle(Command::AnswerApproval {
                    conversation_id,
                    request_id,
                    allow,
                });
            }
            ServerEvent::ToolResult {
                content, is_error, ..
            } => {
                let line = content.lines().next().unwrap_or("").to_string();
                println!(
                    "  tool result{}: {line}",
                    if is_error { " (error)" } else { "" }
                );
                if is_error {
                    seen.errors.push(content);
                } else {
                    seen.outputs.push(content);
                }
            }
            ServerEvent::Completed { .. } => break,
            ServerEvent::Error { detail } => {
                println!("  error: {detail}");
                break;
            }
            _ => {}
        }
    }
    seen
}
