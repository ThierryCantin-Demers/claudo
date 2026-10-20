//! Manual probe for next-message suggestions: a turn in a conversation on screen ends with
//! one, and a turn in one off screen with none.
//! Usage: cargo run -p claudo-core --example suggestion_probe

use std::time::Duration;

use claudo_core::backend::Backend;
use claudo_protocol::{Command, Effort, Profile, ServerEvent};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

const QUIET: Duration = Duration::from_secs(20);

fn main() {
    let tmp = std::env::temp_dir().join("claudo_suggestion_probe");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    // SAFETY: no other thread exists yet.
    unsafe { std::env::set_var("XDG_DATA_HOME", &tmp) };
    let mut failed = false;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        failed |= !ok;
    };

    let (tx, mut rx) = unbounded_channel();
    let backend = Backend::new(tx.clone());
    let mut created = || {
        backend.handle(Command::NewScratch { request: 0 });
        loop {
            if let Some(ServerEvent::Created {
                conversation_id, ..
            }) = rx.blocking_recv()
            {
                break conversation_id;
            }
        }
    };
    let (shown, hidden) = (created(), created());
    backend.handle(Command::OnScreen {
        conversation_ids: vec![shown.clone()],
    });

    for (id, on_screen) in [(&shown, true), (&hidden, false)] {
        backend.handle(send(
            id,
            "Write hello.py that prints hi, but do not run it yet.",
        ));
        let suggested = after_turn(&mut rx, &tx);
        println!("  suggested {suggested:?}");
        if on_screen {
            check(
                suggested.is_some(),
                "a turn on screen ends with a suggestion",
            );
        } else {
            check(suggested.is_none(), "one off screen ends with none");
        }
    }
    std::process::exit(i32::from(failed));
}

/// The suggestion that follows the turn running, if one comes within `QUIET` of its end.
fn after_turn(
    rx: &mut UnboundedReceiver<ServerEvent>,
    tx: &UnboundedSender<ServerEvent>,
) -> Option<String> {
    while let Some(ev) = rx.blocking_recv() {
        match ev {
            ServerEvent::Completed { .. } => break,
            ServerEvent::Error { detail, .. } => println!("  error: {detail}"),
            _ => {}
        }
    }
    let quiet = tx.clone();
    std::thread::spawn(move || {
        std::thread::sleep(QUIET);
        let _ = quiet.send(ServerEvent::QuotaUnavailable("quiet".to_string()));
    });
    while let Some(ev) = rx.blocking_recv() {
        match ev {
            ServerEvent::Suggested { text, .. } => return Some(text),
            ServerEvent::QuotaUnavailable(why) if why == "quiet" => return None,
            _ => {}
        }
    }
    None
}

fn send(id: &str, text: &str) -> Command {
    Command::SendMessage {
        conversation_id: id.to_string(),
        text: text.to_string(),
        profile: Profile::FullAuto,
        model: "haiku".to_string(),
        effort: Effort::Default,
        attachments: Vec::new(),
    }
}
