//! A message sent while a turn runs reaches Claude at its next step: this sends one after
//! the turn's second call, then reports both deliveries and how many turns ended.
//! Usage: cargo run -p claudo-core --example queue_probe

use claudo_core::backend::Backend;
use claudo_protocol::{Command, Effort, Profile, ServerEvent};
use tokio::sync::mpsc::unbounded_channel;

fn main() {
    let tmp = std::env::temp_dir().join("claudo_queue_probe");
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
    let send = |text: &str| {
        backend.handle(Command::SendMessage {
            conversation_id: id.clone(),
            text: text.to_string(),
            profile: Profile::FullAuto,
            model: "haiku".to_string(),
            effort: Effort::Default,
            attachments: Vec::new(),
        })
    };
    send(
        "Run these Bash commands, each in its own tool call, one after another: `ls /`, \
         `ls /usr`, `ls /etc`, `ls /var`. Then reply with only the word: first",
    );

    let (mut returned, mut delivered, mut completed) = (0, 0, 0);
    while let Some(ev) = rx.blocking_recv() {
        match ev {
            ServerEvent::Delivered { .. } => {
                delivered += 1;
                println!("message {delivered} delivered");
            }
            ServerEvent::ToolResult { .. } => {
                returned += 1;
                println!("call {returned} returned");
                if returned == 2 {
                    println!("sending a second message");
                    send("Also reply with the word: second");
                }
            }
            ServerEvent::Delta { text, .. } => println!("text: {text}"),
            ServerEvent::Completed { is_error, .. } => {
                completed += 1;
                println!("turn completed (is_error={is_error})");
                if delivered == 2 {
                    break;
                }
            }
            ServerEvent::Error { detail, .. } => {
                println!("error: {detail}");
                break;
            }
            _ => {}
        }
    }
    println!("{delivered} messages delivered, {completed} turns completed");
    backend.handle(Command::Delete { id });
    // Deleting finishes on a thread of its own, which reports the list without it.
    while let Some(ev) = rx.blocking_recv() {
        if matches!(ev, ServerEvent::Conversations { list } if list.is_empty()) {
            break;
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
