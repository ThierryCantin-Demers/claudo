//! Manual probe for approvals: issue and PR creation waits for an answer under every
//! profile, alone or after a `cd`, until allowed always, while other calls Safe mode would prompt for are still refused.
//! `gh pr create --help` stands in for a real PR: it matches the rule but creates nothing.
//! Usage: cargo run -p claudo-core --example approval_probe

use claudo_core::backend::Backend;
use claudo_protocol::{Command, Decision, Effort, Profile, ServerEvent};
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
    backend.handle(Command::NewScratch { request: 0 });
    let id = loop {
        if let Some(ServerEvent::Created {
            conversation_id, ..
        }) = rx.blocking_recv()
        {
            break conversation_id;
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
        Decision::AllowOnce,
    );
    check(
        write.approvals.is_empty(),
        "Safe mode write is not offered for approval",
    );
    check(
        write.errors.iter().any(|e| e.contains("haven't granted")),
        "Safe mode write is refused",
    );

    let allowed = turn(
        &backend,
        &mut rx,
        &id,
        Profile::Safe,
        GH,
        Decision::AllowOnce,
    );
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

    let denied = turn(
        &backend,
        &mut rx,
        &id,
        Profile::FullAuto,
        GH,
        Decision::Deny,
    );
    check(
        !denied.approvals.is_empty(),
        "PR creation asks for approval in Full auto",
    );
    check(
        denied.errors.iter().any(|e| e.contains("denied")),
        "a denied call does not run",
    );

    let compound = format!(
        "Run exactly this command with the Bash tool, verbatim: cd {} && gh issue create --help. Then reply done.",
        std::env::temp_dir().display()
    );
    let compound = turn(
        &backend,
        &mut rx,
        &id,
        Profile::FullAuto,
        &compound,
        Decision::AllowAlways,
    );
    check(
        compound
            .approvals
            .iter()
            .any(|s| s.contains("&& gh issue create")),
        "issue creation after a cd asks for approval in Full auto",
    );

    let issue =
        "Run exactly this command with the Bash tool: gh issue create --help. Then reply done.";
    let again = turn(
        &backend,
        &mut rx,
        &id,
        Profile::FullAuto,
        issue,
        Decision::Deny,
    );
    check(
        again.approvals.is_empty() && again.outputs.iter().any(|o| o.contains("issue")),
        "once allowed always, issue creation runs without asking",
    );

    // A new backend spawns a new process, which must get the choice from the store.
    drop(backend);
    let (tx, mut rx) = unbounded_channel();
    let backend = Backend::new(tx);
    let restarted = turn(
        &backend,
        &mut rx,
        &id,
        Profile::FullAuto,
        issue,
        Decision::Deny,
    );
    check(
        restarted.approvals.is_empty() && restarted.outputs.iter().any(|o| o.contains("issue")),
        "a new process remembers it",
    );
    let pr = turn(
        &backend,
        &mut rx,
        &id,
        Profile::FullAuto,
        GH,
        Decision::Deny,
    );
    check(!pr.approvals.is_empty(), "PR creation still asks");

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
    decision: Decision,
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
                ..
            } => {
                println!("  approval requested: {} ({})", call.summary, call.name);
                seen.approvals.push(call.summary);
                backend.handle(Command::AnswerApproval {
                    conversation_id,
                    request_id,
                    decision,
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
            ServerEvent::Error { detail, .. } => {
                println!("  error: {detail}");
                break;
            }
            _ => {}
        }
    }
    seen
}
