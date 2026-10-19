//! Manual probe for the agent lock: three backends in one process stand in for three
//! claudos sharing a conversation. A message to a conversation another one's agent holds
//! waits for that agent to end, never starting a second, and a stop ends it.
//! Usage: cargo run -p claudo-core --example stray_probe

use std::time::Duration;

use claudo_core::backend::Backend;
use claudo_protocol::{Command, Effort, Profile, ServerEvent};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

fn main() {
    let tmp = std::env::temp_dir().join("claudo_stray_probe");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    // SAFETY: no other thread exists yet.
    unsafe { std::env::set_var("XDG_DATA_HOME", &tmp) };
    let mut failed = false;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        failed |= !ok;
    };

    let (a, mut a_events) = backend();
    a.handle(Command::NewScratch { request: 0 });
    let id = loop {
        if let Some(ServerEvent::Created {
            conversation_id, ..
        }) = a_events.blocking_recv()
        {
            break conversation_id;
        }
    };
    a.handle(send(
        &id,
        "Run this exact bash command with the Bash tool, not in the background, then reply \
         with the single word first: yes | head -c 16G | wc -c",
    ));
    until(
        &mut a_events,
        |ev| matches!(ev, ServerEvent::ToolStarted { call, .. } if call.name == "Bash"),
    );

    let (b, mut b_events) = backend();
    b.handle(send(&id, "Reply with the single word second."));
    let waited = until(&mut b_events, |ev| {
        matches!(ev, ServerEvent::Waiting { .. })
    });
    let Some(ServerEvent::Waiting { pid, .. }) = waited else {
        check(false, "a second claudo waits for the first one's agent");
        std::process::exit(1);
    };
    let alive = |pid: u32| std::path::Path::new(&format!("/proc/{pid}")).exists();
    check(
        alive(pid),
        "a second claudo waits for the first one's agent",
    );

    // The first claudo goes, as a killed one would: its agent finishes its turn alone.
    drop(a);
    let second = reply(&mut b_events);
    check(!alive(pid), "the first agent finished and exited");
    check(
        second.contains("second"),
        "then the waiting message went through",
    );

    let (c, mut c_events) = backend();
    c.handle(send(&id, "Reply with the single word third."));
    let waited = until(&mut c_events, |ev| {
        matches!(ev, ServerEvent::Waiting { .. })
    });
    let Some(ServerEvent::Waiting { pid, .. }) = waited else {
        check(
            false,
            "an idle agent of another claudo holds the conversation too",
        );
        std::process::exit(1);
    };
    check(
        true,
        "an idle agent of another claudo holds the conversation too",
    );
    c.handle(Command::StopStray {
        conversation_id: id.clone(),
    });
    let third = reply(&mut c_events);
    check(!alive(pid), "stopping it ended that agent");
    check(third.contains("third"), "and the message went through");
    drop(b);
    std::process::exit(i32::from(failed));
}

fn backend() -> (Backend, UnboundedReceiver<ServerEvent>) {
    let (tx, rx) = unbounded_channel();
    (Backend::new(tx), rx)
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

/// The text of the turn that ends next, lowercased.
fn reply(events: &mut UnboundedReceiver<ServerEvent>) -> String {
    let mut text = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while std::time::Instant::now() < deadline {
        match events.blocking_recv() {
            Some(ServerEvent::Delta { text: t, .. }) => text.push_str(&t),
            Some(ServerEvent::Completed { .. }) => break,
            Some(ServerEvent::Error { detail, .. }) => println!("  error: {detail}"),
            Some(_) => {}
            None => break,
        }
    }
    println!("  replied {text:?}");
    text.to_lowercase()
}

fn until(
    events: &mut UnboundedReceiver<ServerEvent>,
    done: impl Fn(&ServerEvent) -> bool,
) -> Option<ServerEvent> {
    while let Some(ev) = events.blocking_recv() {
        if done(&ev) {
            return Some(ev);
        }
    }
    None
}
