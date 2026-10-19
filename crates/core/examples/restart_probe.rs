//! Manual probe for `Command::Restart` through the backend, as a frontend sends it: a turn
//! running a long command when the app restarts goes on in the new process, which says the
//! conversation is working and ends the turn with its time from before the restart.
//! Usage: cargo run -p claudo-core --example restart_probe

use std::time::Instant;

use claudo_core::backend::Backend;
use claudo_protocol::{Command, Effort, Profile, ServerEvent};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

fn main() {
    let tmp = std::env::temp_dir().join("claudo_restart_probe");
    let marker = tmp.join("marker");
    if std::env::var_os(claudo_core::handover::FILE_VAR).is_none() {
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // SAFETY: no other thread exists yet. The exec'd process inherits it.
        unsafe { std::env::set_var("XDG_DATA_HOME", &tmp) };
        before(&marker);
    } else {
        after(&marker);
    }
}

fn before(marker: &std::path::Path) {
    let (tx, mut rx) = unbounded_channel::<ServerEvent>();
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
        conversation_id: id,
        text: format!(
            "Run this exact bash command with the Bash tool, not in the background, then \
             reply with the single word finished: yes | head -c 8G | wc -c; date > {}",
            marker.display()
        ),
        profile: Profile::FullAuto,
        model: "haiku".to_string(),
        effort: Effort::Default,
        attachments: Vec::new(),
    });
    wait_for(
        &mut rx,
        |ev| matches!(ev, ServerEvent::ToolStarted { call, .. } if call.name == "Bash"),
    );
    println!("the command started; restarting");
    backend.handle(Command::Restart);
    let failed = wait_for(&mut rx, |ev| matches!(ev, ServerEvent::Error { .. }));
    println!("FAIL the restart failed: {failed:?}");
    std::process::exit(1);
}

fn after(marker: &std::path::Path) {
    let started = Instant::now();
    let (tx, mut rx) = unbounded_channel::<ServerEvent>();
    let _backend = Backend::new(tx);
    let mut failed = false;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        failed |= !ok;
    };
    let first = rx.blocking_recv();
    check(
        matches!(first, Some(ServerEvent::Working { .. })),
        "the new process says the conversation is working",
    );
    let mut reply = String::new();
    let ended = loop {
        match rx.blocking_recv() {
            Some(ServerEvent::Delta { text, .. }) => reply.push_str(&text),
            Some(ServerEvent::Completed { time, is_error, .. }) => break Some((time, is_error)),
            Some(ServerEvent::Error { detail, .. }) => {
                println!("  error: {detail}");
                break None;
            }
            Some(_) => {}
            None => break None,
        }
    };
    println!(
        "  ended {:?} after the restart; replied {reply:?}",
        started.elapsed()
    );
    check(
        matches!(ended, Some((_, false))),
        "the turn completed after the restart",
    );
    check(
        ended
            .and_then(|(time, _)| time)
            .is_some_and(|time| time.started < time.ended),
        "its time counts from before the restart",
    );
    check(marker.exists(), "the command ran to its end");
    check(
        reply.to_lowercase().contains("finished"),
        "the reply came through",
    );
    std::process::exit(i32::from(failed));
}

fn wait_for(
    rx: &mut UnboundedReceiver<ServerEvent>,
    done: impl Fn(&ServerEvent) -> bool,
) -> Option<ServerEvent> {
    while let Some(ev) = rx.blocking_recv() {
        if done(&ev) {
            return Some(ev);
        }
    }
    None
}
