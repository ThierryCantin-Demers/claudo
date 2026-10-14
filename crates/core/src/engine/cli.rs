//! Drives the Claude Code CLI in bidirectional stream-json mode: one long-lived
//! `claude -p --input-format stream-json` process per session, each turn written
//! to its stdin as a JSONL user message, events parsed from its stdout.
//!
//! This is the seam that isolates CLI/stream-json specifics from the rest of the app.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{ChildStderr, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use claudo_protocol::{ContextUsage, Profile};
use serde_json::{Value, json};

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

/// The flags a `claude` process starts with. They hold for its whole life, so a
/// turn asking for different ones needs a new process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfig {
    pub cwd: PathBuf,
    pub profile: Profile,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// One long-lived `claude -p --input-format stream-json` process. Turns are
/// written to its stdin; dropping the session closes stdin, which lets the
/// process finish a running turn and then exit.
pub struct Session {
    config: SessionConfig,
    pid: u32,
    stdin: ChildStdin,
    shared: Arc<Shared>,
}

/// State shared with the thread reading the process's stdout.
struct Shared {
    turn: Mutex<TurnSlot>,
    last_activity: Mutex<Instant>,
}

#[derive(Default)]
struct TurnSlot {
    /// Where the running turn's events go; `None` between turns.
    sink: Option<Sender<EngineEvent>>,
    exited: bool,
}

const STDERR_TAIL_LINES: usize = 20;

impl Session {
    pub fn spawn(config: SessionConfig, resume: Option<&str>) -> io::Result<Self> {
        let mut cmd = Command::new(claude_bin());
        cmd.current_dir(&config.cwd)
            .arg("-p")
            .args(["--input-format", "stream-json"])
            .args(["--output-format", "stream-json"])
            .arg("--include-partial-messages")
            .arg("--permission-mode")
            .arg(cli_mode(config.profile))
            .arg("--verbose")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(sid) = resume {
            cmd.arg("--resume").arg(sid);
        }
        if let Some(model) = &config.model {
            cmd.arg("--model").arg(model);
        }
        if let Some(effort) = &config.effort {
            cmd.arg("--effort").arg(effort);
        }

        let mut child = cmd.spawn()?;
        let pid = child.id();
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let shared = Arc::new(Shared {
            turn: Mutex::default(),
            last_activity: Mutex::new(Instant::now()),
        });

        // Drained continuously: a long-lived process would block once the pipe filled.
        let stderr = thread::spawn(move || stderr_tail(stderr));
        let reader = shared.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                reader.touch();
                for ev in parse_line(&line) {
                    let completed = matches!(ev, EngineEvent::Completed { .. });
                    let mut turn = reader.turn.lock().unwrap();
                    if let Some(sink) = &turn.sink {
                        let _ = sink.send(ev);
                    }
                    if completed {
                        turn.sink = None;
                    }
                }
            }

            let status = child.wait();
            let err = stderr.join().unwrap_or_default();
            let mut turn = reader.turn.lock().unwrap();
            turn.exited = true;
            if let Some(sink) = turn.sink.take() {
                let detail = if err.trim().is_empty() {
                    match status {
                        Ok(s) => format!("claude exited without completing the turn ({s})"),
                        Err(e) => format!("waiting on claude failed: {e}"),
                    }
                } else {
                    err.trim().to_string()
                };
                let _ = sink.send(EngineEvent::Error { detail });
            }
        });

        Ok(Self {
            config,
            pid,
            stdin,
            shared,
        })
    }

    /// Start a turn. Its events arrive on the returned receiver, which closes
    /// after `Completed` or `Error`. Fails if the process has exited.
    pub fn send(&mut self, prompt: &str) -> io::Result<Receiver<EngineEvent>> {
        let (tx, rx) = mpsc::channel();
        {
            let mut turn = self.shared.turn.lock().unwrap();
            if turn.exited {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            turn.sink = Some(tx);
        }
        self.shared.touch();

        let message = json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": prompt }] },
        });
        let written = writeln!(self.stdin, "{message}").and_then(|()| self.stdin.flush());
        if let Err(e) = written {
            self.shared.turn.lock().unwrap().sink = None;
            return Err(e);
        }
        Ok(rx)
    }

    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn is_alive(&self) -> bool {
        !self.shared.turn.lock().unwrap().exited
    }

    pub fn is_busy(&self) -> bool {
        self.shared.turn.lock().unwrap().sink.is_some()
    }

    /// Time since a turn was sent or the process last wrote anything.
    pub fn idle_for(&self) -> Duration {
        self.shared.last_activity.lock().unwrap().elapsed()
    }
}

impl Shared {
    fn touch(&self) {
        *self.last_activity.lock().unwrap() = Instant::now();
    }
}

fn stderr_tail(stderr: ChildStderr) -> String {
    let mut tail = VecDeque::with_capacity(STDERR_TAIL_LINES);
    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        if tail.len() == STDERR_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }
    Vec::from(tail).join("\n")
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
                call_id: b
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
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
