//! Manual probe for deleting conversations: the store entry, a scratch conversation's
//! folder and its Claude Code transcript go; an attached project folder, and other
//! sessions in it, stay. Runs on a temporary home, so no CLI and no real data.
//! Usage: cargo run -p claudo-core --example delete_probe

use std::fs;
use std::path::Path;
use std::time::Duration;

use claudo_core::backend::Backend;
use claudo_core::sessions::Store;
use claudo_protocol::{Command, ServerEvent};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

fn main() {
    let tmp = std::env::temp_dir().join("claudo_delete_probe");
    let _ = fs::remove_dir_all(&tmp);
    // SAFETY: no other thread exists yet.
    unsafe {
        std::env::set_var("HOME", &tmp);
        std::env::set_var("XDG_DATA_HOME", tmp.join("data"));
    }
    let mut failed = false;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        failed |= !ok;
    };

    let projects = tmp.join(".claude/projects");
    let folder = tmp.join("my-project");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("main.rs"), "fn main() {}").unwrap();

    let (scratch, attached) = {
        let mut store = Store::load();
        let scratch = store.create_scratch();
        let attached = store.create_attached(folder.clone());
        store.record_turn(&scratch, Some("scratch-session"));
        store.record_turn(&attached, Some("attached-session"));
        (scratch, attached)
    };
    let transcript = |project: &str, session: &str| {
        let path = projects.join(project).join(format!("{session}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{}\n").unwrap();
        path
    };
    let scratch_transcript = transcript("-scratch", "scratch-session");
    fs::create_dir_all(projects.join("-scratch/scratch-session/subagents")).unwrap();
    let attached_transcript = transcript("-my-project", "attached-session");
    let neighbour = transcript("-my-project", "another-session");
    let scratch_dir = tmp.join("data/claudo/conversations").join(&scratch);
    check(
        scratch_dir.is_dir(),
        "the scratch conversation has its folder",
    );

    let (tx, mut rx) = unbounded_channel();
    let backend = Backend::new(tx);
    backend.handle(Command::Delete {
        id: scratch.clone(),
    });
    let listed = remaining(&mut rx);
    check(
        !listed.contains(&scratch),
        "the deleted conversation leaves the list",
    );
    check(!scratch_dir.exists(), "its scratch folder is gone");
    check(!scratch_transcript.exists(), "its transcript is gone");
    check(
        !projects.join("-scratch").exists(),
        "its project folder, subagents included, is gone",
    );

    backend.handle(Command::Delete {
        id: attached.clone(),
    });
    let listed = remaining(&mut rx);
    check(
        !listed.contains(&attached),
        "the attached conversation leaves the list",
    );
    check(!attached_transcript.exists(), "its transcript is gone");
    check(
        Path::new(&folder).join("main.rs").exists(),
        "the attached project folder is untouched",
    );
    check(neighbour.exists(), "other sessions of that project stay");

    let _ = fs::remove_dir_all(&tmp);
    std::process::exit(i32::from(failed));
}

/// The conversation ids in the next list the backend sends.
fn remaining(rx: &mut UnboundedReceiver<ServerEvent>) -> Vec<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match rx.try_recv() {
            Ok(ServerEvent::Conversations { list }) => {
                return list.into_iter().map(|c| c.id).collect();
            }
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    panic!("no conversation list after deleting");
}
