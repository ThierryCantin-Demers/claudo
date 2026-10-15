//! Smoke test: per-conversation overrides and titles persist and ride along in
//! ConversationInfo, and a conversation keeps the defaults it was created with when they
//! change. Usage: cargo run -p claudo-core --example overrides_probe

use claudo_core::backend::Backend;
use claudo_protocol::{Command, Effort, Overrides, Profile, ServerEvent, Settings};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

fn main() {
    let tmp = std::env::temp_dir().join("claudo_ov_probe");
    let _ = std::fs::remove_dir_all(&tmp);
    // SAFETY: no other thread exists yet.
    unsafe { std::env::set_var("XDG_DATA_HOME", &tmp) };

    let (tx, mut rx) = unbounded_channel::<ServerEvent>();
    let backend = Backend::new(tx);

    backend.handle(Command::UpdateSettings {
        settings: Settings {
            model: "opus".to_string(),
            effort: Effort::Low,
            profile: Profile::Safe,
        },
    });
    backend.handle(Command::NewScratch { request: 0 });
    let id = new_id(&mut rx).expect("a new conversation id");
    backend.handle(Command::UpdateSettings {
        settings: Settings {
            model: "haiku".to_string(),
            effort: Effort::Max,
            profile: Profile::FullAuto,
        },
    });
    backend.handle(Command::ListConversations);
    print_overrides(
        &mut rx,
        "created with opus/Low/Safe, then the defaults changed",
    );

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

    backend.handle(Command::Rename {
        id: id.clone(),
        title: "  Research agent ".to_string(),
    });
    backend.handle(Command::Rename {
        id: id.clone(),
        title: "   ".to_string(),
    });
    print_titles(&mut rx, "after renaming, then a blank rename");

    backend.handle(Command::Archive {
        id: id.clone(),
        archived: true,
    });
    print_archived(&mut rx, "after archiving");

    // Fresh backend loads it from disk.
    let (tx2, mut rx2) = unbounded_channel::<ServerEvent>();
    let backend2 = Backend::new(tx2);
    backend2.handle(Command::ListConversations);
    print_titles(&mut rx2, "reloaded");
    backend2.handle(Command::ListConversations);
    print_archived(&mut rx2, "reloaded");
    backend2.handle(Command::ListConversations);
    print_overrides(&mut rx2, "reloaded");

    let _ = std::fs::remove_dir_all(&tmp);
}

fn new_id(rx: &mut UnboundedReceiver<ServerEvent>) -> Option<String> {
    let mut id = None;
    while let Ok(ev) = rx.try_recv() {
        if let ServerEvent::Created {
            conversation_id, ..
        } = ev
        {
            id = Some(conversation_id);
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

fn print_titles(rx: &mut UnboundedReceiver<ServerEvent>, label: &str) {
    while let Ok(ev) = rx.try_recv() {
        if let ServerEvent::Conversations { list } = ev {
            for c in list {
                println!("{label}: title={:?}", c.title);
            }
        }
    }
}

fn print_archived(rx: &mut UnboundedReceiver<ServerEvent>, label: &str) {
    while let Ok(ev) = rx.try_recv() {
        if let ServerEvent::Conversations { list } = ev {
            for c in list {
                println!("{label}: archived={}", c.archived);
            }
        }
    }
}
