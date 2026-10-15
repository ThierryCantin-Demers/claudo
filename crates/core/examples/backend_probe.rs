//! Manual smoke test for M4a: drive the Backend through the protocol
//! (Commands in, ServerEvents out), exactly as a transport would. Uses a temp
//! data dir. Usage: cargo run -p claudo-core --example backend_probe

use std::time::{Duration, Instant};

use claudo_core::backend::Backend;
use claudo_protocol::{Command, Effort, Profile, ServerEvent};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

fn main() {
    let tmp = std::env::temp_dir().join("claudo_m4_probe");
    let _ = std::fs::remove_dir_all(&tmp);
    // SAFETY: no other thread exists yet.
    unsafe { std::env::set_var("XDG_DATA_HOME", &tmp) };

    let (tx, mut rx) = unbounded_channel::<ServerEvent>();
    let backend = Backend::new(tx);

    backend.handle(Command::GetStatus);
    backend.handle(Command::NewScratch { request: 0 });
    let conversation = drain(&mut rx); // Status, Conversations, History, Created

    let id = conversation.expect("NewScratch should select a conversation");
    println!("new conversation: {id}\nsending a message…\n");

    backend.handle(Command::SendMessage {
        conversation_id: id.clone(),
        text: "Reply with a short one-line greeting.".to_string(),
        profile: Profile::Safe,
        model: String::new(),
        effort: Effort::Default,
    });

    // Poll until the turn completes (worker thread streams events in).
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut assistant = String::new();
    loop {
        while let Ok(ev) = rx.try_recv() {
            match ev {
                ServerEvent::Delta { text, .. } => assistant.push_str(&text),
                ServerEvent::Quota(quota) => println!("quota: {quota:?}"),
                ServerEvent::Completed {
                    is_error, context, ..
                } => {
                    println!("assistant: {assistant}");
                    println!("completed (is_error={is_error})");
                    println!(
                        "context: used={} window={:?} pct={:?}",
                        context.used,
                        context.window,
                        context.percent()
                    );
                    return;
                }
                ServerEvent::Error { detail, .. } => {
                    println!("error: {detail}");
                    return;
                }
                _ => {}
            }
        }
        if Instant::now() > deadline {
            println!("timed out");
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Drain currently-queued events, printing each, and return the conversation id
/// the backend created.
fn drain(rx: &mut UnboundedReceiver<ServerEvent>) -> Option<String> {
    let mut selected = None;
    while let Ok(ev) = rx.try_recv() {
        match &ev {
            ServerEvent::Status(s) => println!("Status: ready={}", s.ready()),
            ServerEvent::Conversations { list } => {
                println!("Conversations: {} item(s)", list.len())
            }
            ServerEvent::Created {
                conversation_id, ..
            } => {
                println!("Created: {conversation_id}");
                selected = Some(conversation_id.clone());
            }
            ServerEvent::History { items, .. } => println!("History: {} item(s)", items.len()),
            other => println!("{other:?}"),
        }
    }
    selected
}
