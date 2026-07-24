//! Manual smoke test for M2: create a conversation, run a turn, reload history
//! from Claude Code's transcript store. Uses a temp data dir so it does not
//! touch the real conversation list.
//! Usage: cargo run -p claudo-core --example sessions

use claudo_core::engine::{run_turn, EngineEvent};
use claudo_core::sessions::{load_transcript, Store};
use claudo_protocol::{Item, Profile};

fn main() {
    let tmp = std::env::temp_dir().join("claudo_m2_probe");
    let _ = std::fs::remove_dir_all(&tmp);
    std::env::set_var("XDG_DATA_HOME", &tmp);

    let prompt = "Reply with a short one-line greeting, then stop.";
    let mut store = Store::load();
    let id = store.create_scratch();
    let cwd = store.get(&id).unwrap().cwd.clone();
    println!("conversation {id}\ncwd {}\n", cwd.display());

    let mut session_id = None;
    for ev in run_turn(&cwd, None, prompt, Profile::Safe, None, None) {
        match ev {
            EngineEvent::Started { session_id: s } => session_id = Some(s),
            EngineEvent::Completed {
                session_id: Some(s),
                ..
            } => session_id = Some(s),
            _ => {}
        }
    }
    let session_id = session_id.expect("a session id");
    store.record_turn(&id, Some(&session_id), prompt);
    println!("session {session_id}");
    println!("title  -> {:?}", store.get(&id).unwrap().title);

    let (items, ai_title) = load_transcript(&session_id);
    println!("ai_title -> {ai_title:?}");
    println!("history items: {}", items.len());
    for it in &items {
        match it {
            Item::User(t) => println!("  USER: {}", clip(t)),
            Item::Assistant(t) => println!("  ASSISTANT: {}", clip(t)),
            Item::Tool(tc) => println!(
                "  TOOL {} err={} result={}",
                tc.name,
                tc.is_error,
                clip(tc.result.as_deref().unwrap_or(""))
            ),
        }
    }

    let reloaded = Store::load();
    println!(
        "\nstore reloaded from disk: {} conversation(s)",
        reloaded.conversations.len()
    );
}

fn clip(s: &str) -> String {
    s.chars().take(60).collect()
}
