//! Manual probe for claudo on another computer: a second frontend joins a running backend
//! through the bridge, as one would over ssh, and the two share its conversations. Each
//! answer goes only to the client that asked, a message one sends shows on the other, and
//! a client arriving mid-turn hears what runs.
//! Usage: cargo run -p claudo-transport --example remote_probe (`PROBE_TRACE=1` prints every
//! event)

use std::process::Command as Process;
use std::time::{Duration, Instant};

use claudo_protocol::{Command, Effort, Profile, ServerEvent};
use claudo_transport::{Events, bridge, connect_with, spawn_backend};

fn main() -> std::process::ExitCode {
    if std::env::args().any(|arg| arg == "--bridge") {
        return bridge();
    }
    let tmp = std::env::temp_dir().join("claudo_remote_probe");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("run")).unwrap();
    // SAFETY: no other thread exists yet.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", tmp.join("data"));
        std::env::set_var("XDG_RUNTIME_DIR", tmp.join("run"));
    }
    let mut failed = false;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        failed |= !ok;
    };
    let through_bridge = || {
        let mut bridge = Process::new(std::env::current_exe().unwrap());
        bridge.arg("--bridge");
        bridge
    };

    let (_, mut nowhere) = connect_with(through_bridge);
    let refused = next(&mut nowhere, |ev| {
        matches!(ev, ServerEvent::Connection { .. })
    });
    check(
        matches!(&refused, Some(ServerEvent::Connection { connected: false, detail })
            if detail.starts_with("claudo is not running on")),
        "with no claudo there, the bridge says so",
    );

    let (local, mut local_events) = spawn_backend(true);
    let (remote, mut remote_events) = connect_with(through_bridge);
    check(
        next(&mut remote_events, |ev| {
            matches!(
                ev,
                ServerEvent::Connection {
                    connected: true,
                    ..
                }
            )
        })
        .is_some(),
        "the other computer's claudo connects through the bridge",
    );

    local.send(Command::NewScratch { request: 7 });
    let created = next(&mut local_events, |ev| {
        matches!(ev, ServerEvent::Created { .. })
    });
    let Some(ServerEvent::Created {
        conversation_id: id,
        request: 7,
        ..
    }) = created
    else {
        check(
            false,
            "the client that asked hears its conversation was made",
        );
        return std::process::ExitCode::FAILURE;
    };
    remote.send(Command::GetSettings);
    let seen = until(&mut remote_events, |ev| {
        matches!(ev, ServerEvent::Settings(_))
    });
    check(
        seen.iter()
            .any(|ev| matches!(ev, ServerEvent::Conversations { .. }))
            && !seen
                .iter()
                .any(|ev| matches!(ev, ServerEvent::Created { .. })),
        "the other client hears of the new conversation, but not the answer meant for the first",
    );

    let text = "Reply with the single word ok.".to_string();
    local.send(Command::SendMessage {
        conversation_id: id.clone(),
        text: text.clone(),
        profile: Profile::FullAuto,
        model: "haiku".to_string(),
        effort: Effort::Default,
        attachments: Vec::new(),
    });
    let (late, mut late_events) = connect_with(through_bridge);
    let remote_turn = until(&mut remote_events, |ev| {
        matches!(ev, ServerEvent::Completed { .. })
    });
    check(
        remote_turn
            .iter()
            .any(|ev| matches!(ev, ServerEvent::Queued { text: queued, .. } if *queued == text))
            && remote_turn
                .iter()
                .any(|ev| matches!(ev, ServerEvent::Delivered { .. }))
            && remote_turn
                .iter()
                .any(|ev| matches!(ev, ServerEvent::Delta { .. })),
        "the other client sees the message wait, reach Claude, and the reply stream",
    );
    let local_turn = until(&mut local_events, |ev| {
        matches!(ev, ServerEvent::Completed { .. })
    });
    check(
        !local_turn
            .iter()
            .any(|ev| matches!(ev, ServerEvent::Queued { .. })),
        "the client that sent it is not told it again",
    );
    let late_turn = until(&mut late_events, |ev| {
        matches!(ev, ServerEvent::Completed { .. })
    });
    check(
        late_turn
            .iter()
            .any(|ev| matches!(ev, ServerEvent::Working { .. } | ServerEvent::Queued { .. })),
        "a client arriving mid-turn hears what runs",
    );

    late.send(Command::OpenConversation { id: id.clone() });
    let history = next(&mut late_events, |ev| {
        matches!(ev, ServerEvent::History { .. })
    });
    remote.send(Command::GetStatus);
    let others = until(&mut remote_events, |ev| {
        matches!(ev, ServerEvent::Status(_))
    });
    check(
        history.is_some()
            && !others
                .iter()
                .any(|ev| matches!(ev, ServerEvent::History { .. })),
        "a conversation's history goes only to the client that opened it",
    );
    std::process::exit(i32::from(failed))
}

/// Events until the first that `stop` takes, which comes last, or all of them within a
/// minute.
fn until(events: &mut Events, stop: impl Fn(&ServerEvent) -> bool) -> Vec<ServerEvent> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        let Some(ev) = events.blocking_recv() else {
            break;
        };
        if std::env::var_os("PROBE_TRACE").is_some() {
            eprintln!("  {ev:?}");
        }
        let done = stop(&ev);
        seen.push(ev);
        if done {
            break;
        }
    }
    seen
}

fn next(events: &mut Events, find: impl Fn(&ServerEvent) -> bool) -> Option<ServerEvent> {
    until(events, &find).pop().filter(find)
}
