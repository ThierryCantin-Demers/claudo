//! Manual probe for background commands: their start and end reach the frontend, the end
//! even after its turn, and a reload shows the call ended rather than the CLI's
//! notification as a prompt. Usage: cargo run -p claudo-core --example background_probe

use std::time::{Duration, Instant};

use claudo_core::backend::Backend;
use claudo_protocol::{Command, Effort, Item, Profile, ServerEvent, TaskStatus};
use tokio::sync::mpsc::unbounded_channel;

fn main() {
    let tmp = std::env::temp_dir().join("claudo_background_probe");
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
    backend.handle(Command::SendMessage {
        conversation_id: id.clone(),
        text: "Use the Bash tool with run_in_background set to true to run exactly: \
               seq 1 100000000 | tail -1. Then reply only: started"
            .to_string(),
        profile: Profile::FullAuto,
        model: "haiku".to_string(),
        effort: Effort::Default,
        attachments: Vec::new(),
    });

    let (mut started, mut ended, mut completed_first) = (false, None, false);
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline && ended.is_none() {
        match rx.blocking_recv() {
            Some(ServerEvent::Background { background, .. }) => {
                println!("  background: {:?}", background.status);
                match background.status {
                    TaskStatus::Running => started = true,
                    _ => ended = Some(background),
                }
            }
            Some(ServerEvent::Completed { .. }) if ended.is_none() => completed_first = true,
            None => break,
            _ => {}
        }
    }
    check(started, "the command reports starting in the background");
    let output = ended
        .as_ref()
        .and_then(|b| b.output.clone())
        .unwrap_or_default();
    check(
        ended
            .as_ref()
            .is_some_and(|b| b.status == TaskStatus::Completed),
        "it reports completing",
    );
    check(
        output.contains("100000000"),
        "its output comes with the end",
    );
    println!(
        "  (its turn had {}ended before)",
        if completed_first { "" } else { "not " }
    );

    // The CLI tells the model in a turn of its own, after the running one.
    let mut notice = None;
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let Ok(event) = rx.try_recv() else {
            std::thread::park_timeout(Duration::from_millis(50));
            continue;
        };
        match event {
            ServerEvent::Notice { text, .. } => notice = Some(text),
            ServerEvent::Completed { .. } if notice.is_some() => break,
            _ => {}
        }
    }
    println!("  notice: {}", notice.as_deref().unwrap_or("none"));
    check(
        notice.is_some_and(|n| n.starts_with("Background command ") && !n.contains('<')),
        "the turn for its end opens with a short notice",
    );
    backend.handle(Command::OpenConversation { id });
    let items = loop {
        if let Some(ServerEvent::History { items, .. }) = rx.blocking_recv() {
            break items;
        }
    };
    check(
        items.iter().any(|item| {
            matches!(item, Item::Tool(call)
            if call.background.as_ref().is_some_and(|b| b.status == TaskStatus::Completed))
        }),
        "the reloaded transcript has the call completed",
    );
    check(
        !items.iter().any(|item| {
            matches!(item,
            Item::User(text) | Item::Incoming { text, .. } if text.contains("<task-notification>"))
        }) && items.iter().any(|item| matches!(item, Item::Notice(_))),
        "and the notification shows as its summary, not as markup",
    );
    std::process::exit(i32::from(failed));
}
