//! Manual probe for agents running side by side: two conversations stream at once,
//! every event names its own conversation, and creation requests come back by id.
//! Usage: cargo run -p claudo-core --example concurrent_probe

use std::collections::HashMap;

use claudo_core::backend::Backend;
use claudo_protocol::{Command, Effort, Profile, ServerEvent};
use tokio::sync::mpsc::unbounded_channel;

const PROMPTS: [(u64, &str, &str); 2] = [
    (
        7,
        "apple",
        "Write the word apple 40 times, separated by spaces, and nothing else.",
    ),
    (
        9,
        "banana",
        "Write the word banana 40 times, separated by spaces, and nothing else.",
    ),
];

#[derive(Default)]
struct Seen {
    text: String,
    started: bool,
    completed: bool,
    is_error: bool,
}

fn main() {
    let tmp = std::env::temp_dir().join("claudo_concurrent_probe");
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

    let mut ids = HashMap::new();
    for (request, ..) in PROMPTS {
        backend.handle(Command::NewScratch { request });
    }
    while ids.len() < PROMPTS.len() {
        if let Some(ServerEvent::Created {
            conversation_id,
            request,
            ..
        }) = rx.blocking_recv()
        {
            ids.insert(request, conversation_id);
        }
    }
    check(
        PROMPTS
            .iter()
            .all(|(request, ..)| ids.contains_key(request))
            && ids[&7] != ids[&9],
        "each creation request comes back with its own conversation",
    );

    backend.handle(Command::SendMessage {
        conversation_id: "no-such-conversation".to_string(),
        text: "hi".to_string(),
        profile: Profile::Safe,
        model: "haiku".to_string(),
        effort: Effort::Default,
        attachments: Vec::new(),
    });
    let refused = loop {
        if let Some(ServerEvent::Error {
            conversation_id, ..
        }) = rx.blocking_recv()
        {
            break conversation_id;
        }
    };
    check(
        refused.as_deref() == Some("no-such-conversation"),
        "an error names the conversation it concerns",
    );

    for (request, _, prompt) in PROMPTS {
        backend.handle(Command::SendMessage {
            conversation_id: ids[&request].clone(),
            text: prompt.to_string(),
            profile: Profile::Safe,
            model: "haiku".to_string(),
            effort: Effort::Default,
            attachments: Vec::new(),
        });
    }

    let mut seen: HashMap<String, Seen> = HashMap::new();
    let mut overlapped = false;
    let mut stray = false;
    while seen.values().filter(|s| s.completed).count() < PROMPTS.len() {
        let Some(event) = rx.blocking_recv() else {
            break;
        };
        let streaming = seen.values().filter(|s| s.started && !s.completed).count();
        overlapped |= streaming == PROMPTS.len();
        match event {
            ServerEvent::Started {
                conversation_id, ..
            } => seen.entry(conversation_id).or_default().started = true,
            ServerEvent::Delta {
                conversation_id,
                text,
            } => seen
                .entry(conversation_id)
                .or_default()
                .text
                .push_str(&text),
            ServerEvent::Completed {
                conversation_id,
                is_error,
                ..
            } => {
                let s = seen.entry(conversation_id).or_default();
                s.completed = true;
                s.is_error = is_error;
            }
            ServerEvent::Error {
                conversation_id,
                detail,
            } => {
                println!("  error in {conversation_id:?}: {detail}");
                stray = true;
                break;
            }
            _ => {}
        }
    }

    check(!stray, "no turn failed");
    check(overlapped, "both turns were running at the same time");
    for (request, word, _) in PROMPTS {
        let s = seen.remove(&ids[&request]).unwrap_or_default();
        let other = if word == "apple" { "banana" } else { "apple" };
        println!("  {word}: {} words", s.text.split_whitespace().count());
        check(
            s.completed && !s.is_error,
            &format!("the {word} turn completed"),
        );
        check(
            s.text.contains(word) && !s.text.contains(other),
            &format!("the {word} reply holds only its own text"),
        );
    }

    let _ = std::fs::remove_dir_all(&tmp);
    std::process::exit(i32::from(failed));
}
