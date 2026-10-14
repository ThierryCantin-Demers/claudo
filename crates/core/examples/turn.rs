//! Manual smoke test for the M1 engine: run one real turn and print events.
//! Usage: cargo run -p claudo-core --example turn -- "your prompt"

use claudo_core::engine::{EngineEvent, Session, SessionConfig};
use claudo_protocol::Profile;
use std::path::Path;

fn main() {
    let cwd = std::env::temp_dir();
    let write = "Create a file named claudo_probe.txt containing 'hi' using the Write tool.";

    println!("== Safe: tool should be DENIED ==");
    let _ = std::fs::remove_file(cwd.join("claudo_probe.txt"));
    turn(&cwd, None, write, Profile::Safe);
    println!("file written? {}\n", cwd.join("claudo_probe.txt").exists());

    println!("== Full auto: tool should RUN ==");
    let _ = std::fs::remove_file(cwd.join("claudo_probe.txt"));
    turn(&cwd, None, write, Profile::FullAuto);
    println!("file written? {}", cwd.join("claudo_probe.txt").exists());
    let _ = std::fs::remove_file(cwd.join("claudo_probe.txt"));
}

/// Run one turn, printing deltas/tools live; return (full text, session id).
fn turn(
    cwd: &Path,
    resume: Option<&str>,
    prompt: &str,
    profile: Profile,
) -> (String, Option<String>) {
    println!("> {prompt}");
    let mut text = String::new();
    let mut session_id = resume.map(str::to_owned);
    let config = SessionConfig {
        cwd: cwd.to_path_buf(),
        profile,
        model: None,
        effort: None,
    };
    let mut session = Session::spawn(config, resume).expect("spawn claude");
    for ev in session.send(prompt).expect("send turn") {
        match ev {
            EngineEvent::Started { session_id: s } => session_id = Some(s),
            EngineEvent::Delta { text: t } => text.push_str(&t),
            EngineEvent::ToolStarted { name, .. } => println!("  [tool: {name}]"),
            EngineEvent::ToolResult {
                is_error, content, ..
            } => println!(
                "  [tool result{}: {}]",
                if is_error { " ERROR" } else { "" },
                content
            ),
            EngineEvent::Completed {
                session_id: s,
                is_error,
                ..
            } => {
                if let Some(s) = s {
                    session_id = Some(s);
                }
                if is_error {
                    text.push_str(" [error]");
                }
            }
            EngineEvent::Error { detail } => text = format!("[error] {detail}"),
        }
    }
    (text, session_id)
}
