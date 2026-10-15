//! Manual probe for compaction and slash commands: `/compact` reports its progress and
//! sizes and keeps the context readout, a local command such as `/context` answers in
//! text, and a reload shows the compaction rather than its raw records.
//! Usage: cargo run -p claudo-core --example compact_probe

use std::time::{Duration, Instant};

use claudo_core::backend::Backend;
use claudo_protocol::{Command, Effort, Item, Profile, ServerEvent};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

fn main() {
    let tmp = std::env::temp_dir().join("claudo_compact_probe");
    let _ = std::fs::remove_dir_all(&tmp);
    // SAFETY: no other thread exists yet.
    unsafe { std::env::set_var("XDG_DATA_HOME", &tmp) };
    let mut failed = false;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        failed |= !ok;
    };

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
            profile: Profile::Safe,
            model: "haiku".to_string(),
            effort: Effort::Default,
        })
    };

    send("Reply with exactly one word: hello");
    turn(&mut rx);
    send("/compact");
    let events = turn(&mut rx);
    check(
        events
            .iter()
            .any(|e| matches!(e, ServerEvent::Compacting { .. })),
        "compacting is reported while it runs",
    );
    let compacted = events.iter().find_map(|e| match e {
        ServerEvent::Compacted { compaction, .. } => Some(compaction.clone()),
        _ => None,
    });
    println!("  compacted: {compacted:?}");
    check(
        compacted
            .as_ref()
            .is_some_and(|c| !c.auto && c.before > c.after && c.after > 0),
        "it says how much it freed",
    );
    check(
        events
            .iter()
            .any(|e| matches!(e, ServerEvent::CompactSummary { text, .. } if !text.is_empty())),
        "the summary comes with it",
    );
    let context = events.iter().rev().find_map(|e| match e {
        ServerEvent::Conversations { list } => list.iter().find(|c| c.id == id).map(|c| c.context),
        _ => None,
    });
    check(
        context.is_some_and(|c| c.used > 0),
        "the context readout stays after it",
    );

    send("/context");
    let events = turn(&mut rx);
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ServerEvent::Delta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    check(
        text.contains("Context Usage"),
        "a local command's output shows as a reply",
    );

    backend.handle(Command::OpenConversation { id: id.clone() });
    let items = loop {
        if let Some(ServerEvent::History { items, .. }) = rx.blocking_recv() {
            break items;
        }
    };
    check(
        items
            .iter()
            .any(|i| matches!(i, Item::Compacted(c) if !c.summary.is_empty())),
        "the reload shows the compaction with its summary",
    );
    check(
        items
            .iter()
            .any(|i| matches!(i, Item::User(t) if t == "/compact")),
        "and the command as typed",
    );
    check(
        !items.iter().any(|i| matches!(i, Item::User(t) | Item::Assistant(t)
            if t.contains("<command-") || t.contains("<local-command") || t.starts_with("This session is being continued"))),
        "and none of the raw records",
    );
    std::process::exit(i32::from(failed));
}

/// The events of one turn, up to its end.
fn turn(rx: &mut UnboundedReceiver<ServerEvent>) -> Vec<ServerEvent> {
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut events = Vec::new();
    while Instant::now() < deadline {
        match rx.try_recv() {
            Ok(event) => {
                let done = matches!(
                    event,
                    ServerEvent::Completed { .. } | ServerEvent::Error { .. }
                );
                events.push(event);
                if done {
                    // The store's update follows the turn's end.
                    std::thread::park_timeout(Duration::from_millis(200));
                    while let Ok(event) = rx.try_recv() {
                        events.push(event);
                    }
                    break;
                }
            }
            Err(_) => std::thread::park_timeout(Duration::from_millis(50)),
        }
    }
    events
}
