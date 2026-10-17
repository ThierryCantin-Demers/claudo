//! A background subagent's calls reach the client after the turn that started it ended,
//! and reopening the conversation loads them under the Agent call.
//! Usage: cargo run -p claudo-core --example subagent_probe

use claudo_core::backend::Backend;
use claudo_protocol::{Command, Effort, Item, Profile, ServerEvent};
use tokio::sync::mpsc::unbounded_channel;

fn main() {
    let tmp = std::env::temp_dir().join("claudo_subagent_probe");
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
    backend.handle(Command::SendMessage {
        conversation_id: id.clone(),
        text: "Start one subagent with the Agent tool, run_in_background: true, whose job is to \
               run `ls /`, then `ls /usr`, then `ls /etc`, each in its own Bash call, and report. \
               Do not wait for it: right after starting it, reply with only the word: started"
            .to_string(),
        profile: Profile::FullAuto,
        model: "haiku".to_string(),
        effort: Effort::Default,
        attachments: Vec::new(),
    });

    let mut turns = 0;
    while let Some(ev) = rx.blocking_recv() {
        match ev {
            ServerEvent::ToolStarted { call, .. } => println!(
                "turns ended {turns}: {} {} (subagent: {})",
                call.name,
                call.summary,
                call.parent.is_some()
            ),
            ServerEvent::Notice { text, .. } => {
                println!("notice: {}", text.lines().next().unwrap_or_default())
            }
            ServerEvent::Completed { .. } => {
                turns += 1;
                println!("turn {turns} completed");
                // The subagent's end starts a turn of its own, after which it is done.
                if turns == 2 {
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

    backend.handle(Command::OpenConversation { id: id.clone() });
    while let Some(ev) = rx.blocking_recv() {
        if let ServerEvent::History { items, .. } = ev {
            let calls: Vec<String> = items
                .iter()
                .filter_map(|item| match item {
                    Item::Tool(call) => Some(format!(
                        "{} {}{}",
                        call.name,
                        call.summary,
                        if call.parent.is_some() {
                            " (subagent)"
                        } else {
                            ""
                        }
                    )),
                    _ => None,
                })
                .collect();
            println!("reloaded calls, in order:\n  {}", calls.join("\n  "));
            break;
        }
    }

    backend.handle(Command::Delete { id });
    // Deleting finishes on a thread of its own, which reports the list without it.
    while let Some(ev) = rx.blocking_recv() {
        if matches!(ev, ServerEvent::Conversations { list } if list.is_empty()) {
            break;
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
