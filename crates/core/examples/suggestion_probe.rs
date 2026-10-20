//! Manual probe for next-message suggestions: a turn in a conversation on screen ends with
//! one, and a turn in one off screen with none, until it is on screen too.
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
    unsafe {
        std::env::set_var("XDG_DATA_HOME", &tmp);
        std::env::set_var("XDG_STATE_HOME", &tmp);
    }
    let mut failed = false;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        failed |= !ok;
    };

    let (tx, mut rx) = unbounded_channel();
    let backend = Backend::new(tx.clone());
    claudo_core::debug::set(true);
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

    backend.handle(Command::OnScreen {
        conversation_ids: vec![shown, hidden.clone()],
    });
    let before = children();
    backend.handle(send(
        &hidden,
        "Also write bye.py that prints bye, but do not run it yet.",
    ));
    let suggested = after_turn(&mut rx, &tx);
    let replaced = children()
        .into_iter()
        .any(|(pid, args)| !before.iter().any(|(old, _)| *old == pid) && args.contains("--resume"));
    check(
        replaced,
        "brought on screen, its next message starts an agent that suggests",
    );
    // The CLI drops a guess it finds too long or unsure, so none is no failure here.
    println!("  suggested {suggested:?}");

    let dir = claudo_core::debug::dir();
    let log = std::fs::read_to_string(dir.join("claudo.log")).unwrap_or_default();
    println!("{log}");
    let asked = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|file| file.file_name().to_string_lossy().starts_with("cli-"))
        .filter(|file| {
            std::fs::read_to_string(file.path())
                .is_ok_and(|text| text.contains("source=prompt_suggestion"))
        })
        .count();
    check(
        log.contains("suggested \"") && asked > 0,
        "the debug log has what claudo did, and the CLI's own logs its suggestion requests",
    );
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

/// This process's children, with their command lines.
fn children() -> Vec<(u32, String)> {
    let tasks = std::fs::read_dir("/proc/self/task")
        .into_iter()
        .flatten()
        .flatten();
    tasks
        .filter_map(|task| std::fs::read_to_string(task.path().join("children")).ok())
        .flat_map(|pids| {
            pids.split_whitespace()
                .filter_map(|pid| pid.parse().ok())
                .collect::<Vec<u32>>()
        })
        .map(|pid| {
            let args = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            (pid, String::from_utf8_lossy(&args).replace('\0', " "))
        })
        .collect()
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
