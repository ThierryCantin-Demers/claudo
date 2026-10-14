//! Smoke test: per-conversation overrides persist and ride along in
//! ConversationInfo. Usage: cargo run -p claudo-core --example overrides_probe

use claudo_core::backend::Backend;
use claudo_protocol::{Command, Effort, Overrides, Profile, ServerEvent};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

fn main() {
    let tmp = std::env::temp_dir().join("claudo_ov_probe");
    let _ = std::fs::remove_dir_all(&tmp);
    // SAFETY: no other thread exists yet.
    unsafe { std::env::set_var("XDG_DATA_HOME", &tmp) };

    let (tx, mut rx) = unbounded_channel::<ServerEvent>();
    let backend = Backend::new(tx);

    backend.handle(Command::NewScratch);
    let id = new_id(&mut rx).expect("a new conversation id");

    backend.handle(Command::SetOverrides {
        id: id.clone(),
        overrides: Overrides {
            model: Some("claude-sonnet-5-5".to_string()),
            effort: Some(Effort::High),
            profile: Some(Profile::FullAuto),
        },
    });
    backend.handle(Command::ListConversations);
    print_overrides(&mut rx, "after SetOverrides");

    // Fresh backend loads it from disk.
    let (tx2, mut rx2) = unbounded_channel::<ServerEvent>();
    let backend2 = Backend::new(tx2);
    backend2.handle(Command::ListConversations);
    print_overrides(&mut rx2, "reloaded");

    let _ = std::fs::remove_dir_all(&tmp);
}

fn new_id(rx: &mut UnboundedReceiver<ServerEvent>) -> Option<String> {
    let mut id = None;
    while let Ok(ev) = rx.try_recv() {
        if let ServerEvent::Conversations {
            current: Some(c), ..
        } = ev
        {
            id = Some(c);
        }
    }
    id
}

fn print_overrides(rx: &mut UnboundedReceiver<ServerEvent>, label: &str) {
    while let Ok(ev) = rx.try_recv() {
        if let ServerEvent::Conversations { list, .. } = ev {
            for c in list {
                println!(
                    "{label}: model={:?} effort={:?} profile={:?}",
                    c.overrides.model, c.overrides.effort, c.overrides.profile
                );
            }
        }
    }
}
