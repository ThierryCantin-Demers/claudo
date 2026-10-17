//! Manual probe for agents messaging each other: each conversation's agent is named
//! after its title, and another agent's message to it runs a turn whose events reach
//! the client, starting with `Incoming`.
//! Usage: cargo run -p claudo-core --example peer_probe

use std::collections::HashMap;
use std::time::{Duration, Instant};

use claudo_core::backend::Backend;
use claudo_protocol::{Command, Effort, Profile, ServerEvent};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

const ALPHA: &str = "alpha-7731";
const BRAVO: &str = "bravo-7731";

fn main() {
    let tmp = std::env::temp_dir().join("claudo_peer_probe");
    let _ = std::fs::remove_dir_all(&tmp);
    // SAFETY: no other thread exists yet.
    unsafe { std::env::set_var("XDG_DATA_HOME", &tmp) };

    let (tx, mut rx) = unbounded_channel();
    let backend = Backend::new(tx);
    let mut failed = false;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        failed |= !ok;
    };

    backend.handle(Command::NewScratch { request: 1 });
    backend.handle(Command::NewScratch { request: 2 });
    let mut ids = HashMap::new();
    while ids.len() < 2 {
        if let Some(ServerEvent::Created {
            conversation_id,
            request,
            ..
        }) = rx.blocking_recv()
        {
            ids.insert(request, conversation_id);
        }
    }
    let (alpha, bravo) = (ids[&1].clone(), ids[&2].clone());

    send(
        &backend,
        &alpha,
        &format!("{ALPHA}\nReply with just the word ready."),
    );
    let ready = wait(&mut rx, &alpha, |_| true);
    check(ready.completed, "alpha's first turn completes");

    send(
        &backend,
        &bravo,
        &format!(
            "{BRAVO}\nUse the SendMessage tool to send the message \"ping from bravo\" to the \
             agent named {ALPHA}. Then reply with just the word sent."
        ),
    );
    let incoming = wait(&mut rx, &alpha, |seen| {
        seen.from.is_some() && seen.completed
    });
    println!(
        "  alpha received: {:?} from {:?}",
        incoming.text, incoming.from
    );
    println!("  alpha replied: {}", incoming.reply.trim());
    check(
        incoming.from.as_deref() == Some(BRAVO),
        "the message names its sender by its conversation's title",
    );
    check(
        incoming.text.contains("ping from bravo"),
        "the message's text arrives",
    );
    check(
        incoming.completed && !incoming.is_error,
        "the turn it started reaches the client and completes",
    );

    let _ = std::fs::remove_dir_all(&tmp);
    std::process::exit(i32::from(failed));
}

fn send(backend: &Backend, id: &str, text: &str) {
    println!("> {}", text.replace('\n', " / "));
    backend.handle(Command::SendMessage {
        conversation_id: id.to_string(),
        text: text.to_string(),
        profile: Profile::FullAuto,
        model: "haiku".to_string(),
        effort: Effort::Default,
    });
}

#[derive(Default)]
struct Seen {
    from: Option<String>,
    text: String,
    reply: String,
    completed: bool,
    is_error: bool,
}

/// Collect `conversation`'s events until one of its turns completes with `done` holding,
/// or two minutes pass.
fn wait(
    rx: &mut UnboundedReceiver<ServerEvent>,
    conversation: &str,
    done: impl Fn(&Seen) -> bool,
) -> Seen {
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut seen = Seen::default();
    while Instant::now() < deadline {
        let Ok(event) = rx.try_recv() else {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };
        match event {
            ServerEvent::Incoming {
                conversation_id,
                from,
                text,
            } if conversation_id == conversation => {
                seen.from = Some(from);
                seen.text = text;
            }
            ServerEvent::Delta {
                conversation_id,
                text,
            } if conversation_id == conversation => seen.reply.push_str(&text),
            ServerEvent::Completed {
                conversation_id,
                is_error,
                ..
            } if conversation_id == conversation => {
                seen.completed = true;
                seen.is_error = is_error;
                if done(&seen) {
                    return seen;
                }
            }
            ServerEvent::Error {
                conversation_id,
                detail,
            } => println!("  error in {conversation_id:?}: {detail}"),
            _ => {}
        }
    }
    println!("  timed out");
    seen
}
