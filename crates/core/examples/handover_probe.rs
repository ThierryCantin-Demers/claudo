//! Manual probe for a handover: a turn running a long command when the process execs a new
//! copy of itself finishes in the copy, which takes the agent over mid-command.
//! Usage: cargo run -p claudo-core --example handover_probe

use std::sync::mpsc;
use std::time::{Duration, Instant};

use claudo_core::engine::{EngineEvent, SessionConfig};
use claudo_core::handover::{self, Progress};
use claudo_core::live::LiveSessions;
use claudo_protocol::Profile;

fn main() {
    let cwd = std::env::temp_dir().join("claudo_handover_probe");
    let marker = cwd.join("marker");
    match handover::take() {
        None => before(&cwd, &marker),
        Some(handover) => after(handover, &marker),
    }
}

fn before(cwd: &std::path::Path, marker: &std::path::Path) {
    std::fs::create_dir_all(cwd).unwrap();
    let _ = std::fs::remove_file(marker);
    let (incoming, _) = mpsc::channel();
    let live = LiveSessions::new(Duration::from_secs(600), Some(incoming));
    let config = SessionConfig {
        cwd: cwd.to_path_buf(),
        profile: Profile::FullAuto,
        model: Some("haiku".to_string()),
        effort: None,
    };
    let prompt = format!(
        "Run this exact bash command with the Bash tool, not in the background, then reply \
         with the single word finished: yes | head -c 8G | wc -c; date > {}",
        marker.display()
    );
    let turn = live
        .send_turn(
            "probe",
            config,
            None,
            "claudo handover probe",
            &[],
            prompt.as_str().into(),
        )
        .expect("a turn");
    for event in turn {
        if let EngineEvent::ToolStarted(call) = event
            && call.name == "Bash"
        {
            break;
        }
    }
    println!("the command started; handing the agent over");
    let exe = std::env::current_exe().unwrap();
    let error = live.exec(&exe, |_| Progress::default(), Vec::new());
    println!("FAIL the exec failed: {error}");
    std::process::exit(1);
}

fn after(handover: handover::Handover, marker: &std::path::Path) {
    let started = Instant::now();
    let (incoming, _) = mpsc::channel();
    let live = LiveSessions::new(Duration::from_secs(600), Some(incoming));
    let mut failed = false;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        failed |= !ok;
    };
    check(handover.sessions.len() == 1, "one agent handed over");
    let Some(handed) = handover.sessions.into_iter().next() else {
        std::process::exit(1);
    };
    let pid = handed.state.pid();
    let adopted = live.adopt(&handed.conversation, handed.state);
    check(live.pid("probe") == Some(pid), "the same process, kept");
    let mut reply = String::new();
    let mut completed = false;
    for event in adopted.running.expect("the turn it was running") {
        match event {
            EngineEvent::Delta { text } => reply.push_str(&text),
            EngineEvent::Completed { .. } => completed = true,
            EngineEvent::Error { detail } => println!("  error: {detail}"),
            _ => {}
        }
    }
    println!(
        "  took {:?} after the exec; replied {reply:?}",
        started.elapsed()
    );
    check(completed, "the turn completed in the new process");
    check(marker.exists(), "the command ran to its end");
    check(
        reply.to_lowercase().contains("finished"),
        "the reply came through",
    );
    std::process::exit(i32::from(failed));
}
