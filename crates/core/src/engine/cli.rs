//! Runs one chat turn by spawning `claude -p --output-format stream-json
//! --include-partial-messages`, reading its JSONL event stream on a background
//! thread, and forwarding parsed events over a channel the UI drains.
//!
//! Spawn-per-turn (PLAN.md decision): turn 1 creates a session, later turns pass
//! `--resume <session-id>`. This is the seam that isolates CLI/stream-json
//! specifics from the rest of the app.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use claudo_protocol::{ContextUsage, Profile};
use serde_json::Value;

use crate::claude_bin;

/// Map the shared `Profile` to the CLI's `--permission-mode` value. True
/// per-call approve/deny prompting needs the CLI's undocumented control protocol
/// (M3b spike); these documented modes are the robust M3a lever.
fn cli_mode(profile: Profile) -> &'static str {
    match profile {
        Profile::Safe => "default",
        Profile::AcceptEdits => "acceptEdits",
        Profile::FullAuto => "bypassPermissions",
    }
}

/// Engine-level events, already decoded from the CLI's stream-json. UI-agnostic.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// Session initialised; carries the id used to resume later turns.
    Started { session_id: String },
    /// A chunk of assistant text.
    Delta { text: String },
    /// Claude invoked a tool. `input` is pretty-printed JSON for display.
    ToolStarted {
        call_id: String,
        name: String,
        input: String,
    },
    /// A tool returned. In `Safe`/`AcceptEdits` a denial arrives here as an error.
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
    /// Turn finished. `session_id` is the authoritative id to resume with.
    Completed {
        session_id: Option<String>,
        is_error: bool,
        context: ContextUsage,
    },
    /// The turn could not run (spawn failure, crash, or non-completing exit).
    Error { detail: String },
}

/// Start a turn. Returns immediately with a receiver of events; the CLI runs on
/// a background thread. Dropping the receiver kills the child on the next event.
pub fn run_turn(
    cwd: &Path,
    resume_session: Option<&str>,
    prompt: &str,
    profile: Profile,
    model: Option<&str>,
    effort: Option<&str>,
) -> Receiver<EngineEvent> {
    let (tx, rx) = mpsc::channel();
    let cwd = cwd.to_path_buf();
    let resume = resume_session.map(str::to_owned);
    let prompt = prompt.to_owned();
    let model = model.map(str::to_owned);
    let effort = effort.map(str::to_owned);

    thread::spawn(move || {
        let mut cmd = Command::new(claude_bin());
        cmd.current_dir(&cwd)
            .arg("-p")
            .arg(&prompt)
            .arg("--output-format")
            .arg("stream-json")
            .arg("--include-partial-messages")
            .arg("--permission-mode")
            .arg(cli_mode(profile))
            .arg("--verbose")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(sid) = &resume {
            cmd.arg("--resume").arg(sid);
        }
        if let Some(model) = &model {
            cmd.arg("--model").arg(model);
        }
        if let Some(effort) = &effort {
            cmd.arg("--effort").arg(effort);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(EngineEvent::Error {
                    detail: format!("failed to launch `{}`: {e}", claude_bin()),
                });
                return;
            }
        };

        let stdout = child.stdout.take().expect("stdout piped");
        let mut saw_completed = false;
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            for ev in parse_line(&line) {
                if matches!(ev, EngineEvent::Completed { .. }) {
                    saw_completed = true;
                }
                if tx.send(ev).is_err() {
                    let _ = child.kill();
                    return;
                }
            }
        }

        let status = child.wait();
        if !saw_completed {
            let mut err = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.read_to_string(&mut err);
            }
            let detail = if err.trim().is_empty() {
                match status {
                    Ok(s) => format!("claude exited without completing the turn ({s})"),
                    Err(e) => format!("waiting on claude failed: {e}"),
                }
            } else {
                err.trim().to_string()
            };
            let _ = tx.send(EngineEvent::Error { detail });
        }
    });

    rx
}

/// Decode one JSONL line into zero or more `EngineEvent`s. Parses loosely so
/// unrelated schema changes don't break chat. A single `assistant`/`user`
/// message can carry several tool blocks, hence the vec.
fn parse_line(line: &str) -> Vec<EngineEvent> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let Some(kind) = v.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };

    match kind {
        "system" if v.get("subtype").and_then(Value::as_str) == Some("init") => v
            .get("session_id")
            .and_then(Value::as_str)
            .map(|sid| EngineEvent::Started {
                session_id: sid.to_string(),
            })
            .into_iter()
            .collect(),

        // Token-level text deltas. Tool input arrives here too (as
        // input_json_delta) but we render tools from the complete `assistant`
        // message below, so only text deltas matter here.
        "stream_event" => text_delta(&v).into_iter().collect(),

        // Completed assistant message: pull out tool_use blocks (text is already
        // streamed via deltas, so skip it to avoid duplication).
        "assistant" => content_blocks(&v)
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
            .map(|b| EngineEvent::ToolStarted {
                call_id: b.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                name: b
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string(),
                input: b
                    .get("input")
                    .map(|i| serde_json::to_string_pretty(i).unwrap_or_default())
                    .unwrap_or_default(),
            })
            .collect(),

        // Tool results are delivered as `user` messages by Claude Code.
        "user" => content_blocks(&v)
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            .map(|b| EngineEvent::ToolResult {
                call_id: b
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                content: tool_result_text(b.get("content")),
                is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            })
            .collect(),

        "result" => vec![EngineEvent::Completed {
            session_id: v
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            is_error: v.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            context: context_usage(&v),
        }],

        _ => Vec::new(),
    }
}

/// Context size for the turn, from the `result` event: everything that made up
/// the request's context, plus the model's window from `modelUsage`.
fn context_usage(v: &Value) -> ContextUsage {
    let n = |key: &str| {
        v.pointer(&format!("/usage/{key}"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let used = n("input_tokens")
        + n("cache_read_input_tokens")
        + n("cache_creation_input_tokens")
        + n("output_tokens");

    // `modelUsage` is keyed by model name; take the largest window reported
    // (side-models like the title generator also appear here).
    let window = v
        .get("modelUsage")
        .and_then(Value::as_object)
        .and_then(|models| {
            models
                .values()
                .filter_map(|m| m.get("contextWindow").and_then(Value::as_u64))
                .max()
        });

    ContextUsage { used, window }
}

fn text_delta(v: &Value) -> Option<EngineEvent> {
    let event = v.get("event")?;
    if event.get("type")?.as_str()? != "content_block_delta" {
        return None;
    }
    let delta = event.get("delta")?;
    if delta.get("type")?.as_str()? != "text_delta" {
        return None;
    }
    Some(EngineEvent::Delta {
        text: delta.get("text")?.as_str()?.to_string(),
    })
}

fn content_blocks(v: &Value) -> Vec<Value> {
    v.pointer("/message/content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// `tool_result.content` is either a string or an array of text blocks.
fn tool_result_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}
