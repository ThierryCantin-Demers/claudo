//! Manual probe for code reviews reported with `ReportFindings`: `/code-review` in a
//! conversation shows its findings with their titles while the turn runs, never the call
//! itself, and the same review comes back from the transcript.
//! Usage: cargo run -p claudo-core --example review_probe

use std::process::Command as Process;

use claudo_core::backend::Backend;
use claudo_protocol::{Command, Effort, Item, Profile, Review, ServerEvent};
use tokio::sync::mpsc::unbounded_channel;

fn main() {
    let tmp = std::env::temp_dir().join("claudo_review_probe");
    let _ = std::fs::remove_dir_all(&tmp);
    let project = tmp.join("project");
    std::fs::create_dir_all(&project).unwrap();
    // SAFETY: no other thread exists yet.
    unsafe { std::env::set_var("XDG_DATA_HOME", &tmp) };
    let git = |args: &[&str]| {
        let done = Process::new("git")
            .args(["-c", "user.name=probe", "-c", "user.email=probe@localhost"])
            .args(args)
            .current_dir(&project)
            .status();
        assert!(done.is_ok_and(|s| s.success()), "git {args:?}");
    };
    git(&["init", "-q"]);
    std::fs::write(
        project.join("lib.rs"),
        "fn first(v: &[i32]) -> i32 {\n    v[0]\n}\n",
    )
    .unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    std::fs::write(
        project.join("lib.rs"),
        "fn first(v: &[i32]) -> i32 {\n    v[1]\n}\n\nfn last(v: &[i32]) -> i32 {\n    v[v.len()]\n}\n",
    )
    .unwrap();

    let mut failed = false;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        failed |= !ok;
    };

    let (tx, mut rx) = unbounded_channel();
    let backend = Backend::new(tx);
    backend.handle(Command::NewAttached {
        path: project.display().to_string(),
        request: 0,
    });
    let id = loop {
        if let Some(ServerEvent::Created {
            conversation_id, ..
        }) = rx.blocking_recv()
        {
            break conversation_id;
        }
    };
    backend.handle(Command::SendMessage {
        conversation_id: id.clone(),
        text: "/code-review low".to_string(),
        profile: Profile::FullAuto,
        model: "sonnet".to_string(),
        effort: Effort::Default,
        attachments: Vec::new(),
    });
    let mut live: Option<Review> = None;
    let mut called = false;
    while let Some(ev) = rx.blocking_recv() {
        match ev {
            ServerEvent::Review { review, .. } => live = Some(review),
            ServerEvent::ToolStarted { call, .. } => {
                println!("  call {} {}", call.name, call.summary);
                called |= call.name == "ReportFindings";
            }
            ServerEvent::Error { detail, .. } => println!("  error: {detail}"),
            ServerEvent::Completed { .. } => break,
            _ => {}
        }
    }
    if let Some(review) = &live {
        for finding in &review.findings {
            println!(
                "  {:?} {:?} bug={} confirmed={:?}",
                finding.title, finding.category, finding.bug, finding.confirmed
            );
        }
    }
    check(
        live.as_ref()
            .is_some_and(|review| review.findings.iter().any(|f| f.title.is_some())),
        "the review arrives during its turn, its findings titled",
    );
    check(!called, "the ReportFindings call itself is not shown");

    backend.handle(Command::OpenConversation { id: id.clone() });
    let loaded = loop {
        if let Some(ServerEvent::History { items, .. }) = rx.blocking_recv() {
            break items;
        }
    };
    let reviews: Vec<&Review> = loaded
        .iter()
        .filter_map(|item| match item {
            Item::Review(review) => Some(review),
            _ => None,
        })
        .collect();
    let calls = loaded
        .iter()
        .any(|item| matches!(item, Item::Tool(call) if call.name == "ReportFindings"));
    check(
        reviews.len() == 1 && Some(reviews[0]) == live.as_ref() && !calls,
        "reopened, the transcript shows the same review in place of the call",
    );
    std::process::exit(i32::from(failed));
}
