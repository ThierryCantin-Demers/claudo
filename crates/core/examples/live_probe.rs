//! Manual probe for the persistent `claude` process: reuse across turns, respawn
//! when the flags change, close after idling, and resume afterwards.
//! Usage: cargo run -p claudo-core --example live_probe

use std::path::Path;
use std::thread;
use std::time::Duration;

use claudo_core::engine::{EngineEvent, SessionConfig};
use claudo_core::live::LiveSessions;
use claudo_protocol::Profile;

const CONVERSATION: &str = "probe";
const IDLE: Duration = Duration::from_secs(5);

fn main() {
    let cwd = std::env::temp_dir().join("claudo_live_probe");
    std::fs::create_dir_all(&cwd).unwrap();
    let live = LiveSessions::new(IDLE);
    let config = |effort: Option<&str>| SessionConfig {
        cwd: cwd.clone(),
        profile: Profile::Safe,
        model: Some("haiku".to_string()),
        effort: effort.map(str::to_owned),
    };
    let mut failed = false;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        failed |= !ok;
    };

    let (_, sid) = turn(
        &live,
        config(None),
        None,
        "Remember the word pineapple. Reply with just OK.",
    );
    let first = live.pid(CONVERSATION).unwrap();

    turn(
        &live,
        config(None),
        Some(&sid),
        "Reply with just the word two.",
    );
    check(
        live.pid(CONVERSATION) == Some(first),
        "same flags reuse the process",
    );

    let (text, resumed) = turn(
        &live,
        config(Some("low")),
        Some(&sid),
        "What word did I ask you to remember? Reply with just that word.",
    );
    let second = live.pid(CONVERSATION).unwrap();
    check(second != first, "changed effort respawns the process");
    check(resumed == sid, "respawn keeps the session id");
    check(
        text.to_lowercase().contains("pineapple"),
        "respawn keeps the history",
    );

    thread::sleep(IDLE * 2 + Duration::from_secs(2));
    check(live.pid(CONVERSATION).is_none(), "idle process is dropped");
    check(
        !Path::new(&format!("/proc/{second}")).exists(),
        "idle process has exited",
    );

    let (text, _) = turn(
        &live,
        config(Some("low")),
        Some(&sid),
        "Say the remembered word once more, just the word.",
    );
    check(
        live.pid(CONVERSATION).is_some_and(|p| p != second),
        "next turn respawns",
    );
    check(
        text.to_lowercase().contains("pineapple"),
        "resumed after idle",
    );

    std::process::exit(i32::from(failed));
}

fn turn(
    live: &LiveSessions,
    config: SessionConfig,
    resume: Option<&str>,
    prompt: &str,
) -> (String, String) {
    println!("> {prompt}");
    let mut text = String::new();
    let mut session_id = String::new();
    let events = live
        .send_turn(CONVERSATION, config, resume, prompt)
        .expect("send turn");
    for ev in events {
        match ev {
            EngineEvent::Started { session_id: s } => session_id = s,
            EngineEvent::Delta { text: t } => text.push_str(&t),
            EngineEvent::Error { detail } => panic!("turn failed: {detail}"),
            _ => {}
        }
    }
    println!("< {} (pid {:?})", text.trim(), live.pid(CONVERSATION));
    (text, session_id)
}
