//! Drives the Claude Code CLI in bidirectional stream-json mode: one long-lived
//! `claude -p --input-format stream-json` process per session, each turn written
//! to its stdin as a JSONL user message, events parsed from its stdout.
//!
//! This is the seam that isolates CLI/stream-json specifics from the rest of the app.

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{ChildStderr, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use claudo_protocol::{ContextUsage, Profile, Quota, QuotaWindow, ToolCall};
use serde_json::{Value, json};
use uuid::Uuid;

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
    /// A chunk of the summary of Claude's reasoning.
    ThinkingDelta { text: String },
    /// Claude invoked a tool; the call has no result yet.
    ToolStarted(ToolCall),
    /// The subscription's usage windows, reported once per turn.
    Quota(Quota),
    /// A call matched `ASK_RULES`; the process waits for `Session::answer`.
    ApprovalNeeded { request_id: String, call: ToolCall },
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
    shared: Arc<Shared>,
}

/// State shared with the thread reading the process's stdout.
struct Shared {
    /// `None` once the session is dropped.
    stdin: Mutex<Option<ChildStdin>>,
    turn: Mutex<TurnSlot>,
    last_activity: Mutex<Instant>,
    /// Inputs of approvals awaiting an answer, keyed by request id; an allow echoes them back.
    approvals: Mutex<HashMap<String, Value>>,
}

#[derive(Default)]
struct TurnSlot {
    /// Where the running turn's events go; `None` between turns.
    sink: Option<Sender<EngineEvent>>,
    exited: bool,
}

const STDERR_TAIL_LINES: usize = 20;

/// Calls that wait for the user's answer under every permission profile, Full auto included.
const ASK_RULES: [&str; 4] = [
    "Bash(gh pr create:*)",
    "Bash(gh issue create:*)",
    "mcp__github__create_pull_request",
    "mcp__github__create_issue",
];

impl Session {
    pub fn spawn(config: SessionConfig, resume: Option<&str>) -> io::Result<Self> {
        let mut cmd = Command::new(claude_bin());
        cmd.current_dir(&config.cwd)
            .arg("-p")
            .args(["--input-format", "stream-json"])
            .args(["--output-format", "stream-json"])
            .arg("--include-partial-messages")
            // Hidden in --help; without it print mode sends thinking blocks empty.
            .args(["--thinking-display", "summarized"])
            .arg("--permission-mode")
            .arg(cli_mode(config.profile))
            .arg("--settings")
            .arg(json!({ "permissions": { "ask": ASK_RULES } }).to_string())
            // Without it the CLI denies whatever would prompt instead of asking over stdin.
            .args(["--permission-prompt-tool", "stdio"])
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
            stdin: Mutex::new(Some(stdin)),
            turn: Mutex::default(),
            last_activity: Mutex::new(Instant::now()),
            approvals: Mutex::default(),
        });

        // Drained continuously: a long-lived process would block once the pipe filled.
        let stderr = thread::spawn(move || stderr_tail(stderr));
        let reader = shared.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                reader.touch();
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if v.get("type").and_then(Value::as_str) == Some("control_cancel_request") {
                    if let Some(id) = v.get("request_id").and_then(Value::as_str) {
                        reader.approvals.lock().unwrap().remove(id);
                    }
                    continue;
                }
                let events = match permission_prompt(&v) {
                    Some(Prompt::Ask {
                        request_id,
                        call,
                        input,
                    }) => {
                        reader
                            .approvals
                            .lock()
                            .unwrap()
                            .insert(request_id.clone(), input);
                        vec![EngineEvent::ApprovalNeeded { request_id, call }]
                    }
                    Some(Prompt::Refuse { request_id, tool }) => {
                        let message = format!(
                            "Claude requested permissions to use {tool}, but you haven't granted it yet."
                        );
                        let _ = reader.write(&deny(&request_id, &message));
                        Vec::new()
                    }
                    None => decode(&v),
                };
                for ev in events {
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
        if let Err(e) = self.shared.write(&message) {
            self.shared.turn.lock().unwrap().sink = None;
            return Err(e);
        }
        Ok(rx)
    }

    /// Stop the running turn; it still ends with `Completed`, and the process
    /// stays up for the next one.
    pub fn interrupt(&self) -> io::Result<()> {
        self.shared.write(&json!({
            "type": "control_request",
            "request_id": Uuid::new_v4().to_string(),
            "request": { "subtype": "interrupt" },
        }))
    }

    /// Answer an `ApprovalNeeded`; the waiting call then runs or fails.
    pub fn answer(&self, request_id: &str, allow: bool) -> io::Result<()> {
        let input = self
            .shared
            .approvals
            .lock()
            .unwrap()
            .remove(request_id)
            .ok_or(io::ErrorKind::NotFound)?;
        let response = if allow {
            control_response(
                request_id,
                json!({ "behavior": "allow", "updatedInput": input }),
            )
        } else {
            deny(request_id, "The user denied this.")
        };
        self.shared.write(&response)
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

impl Drop for Session {
    fn drop(&mut self) {
        // The reader thread keeps `shared` alive, so stdin has to be closed here.
        self.shared.stdin.lock().unwrap().take();
    }
}

impl Shared {
    fn touch(&self) {
        *self.last_activity.lock().unwrap() = Instant::now();
    }

    fn write(&self, message: &Value) -> io::Result<()> {
        let mut stdin = self.stdin.lock().unwrap();
        let stdin = stdin.as_mut().ok_or(io::ErrorKind::BrokenPipe)?;
        writeln!(stdin, "{message}")?;
        stdin.flush()
    }
}

/// A `can_use_tool` request. The CLI sends one for every call that would prompt, but only
/// calls matching a rule are the user's to answer; the rest are refused as before.
enum Prompt {
    Ask {
        request_id: String,
        call: ToolCall,
        input: Value,
    },
    Refuse {
        request_id: String,
        tool: String,
    },
}

fn permission_prompt(v: &Value) -> Option<Prompt> {
    if v.get("type")?.as_str()? != "control_request" {
        return None;
    }
    let request_id = v.get("request_id")?.as_str()?.to_string();
    let request = v.get("request")?;
    if request.get("subtype")?.as_str()? != "can_use_tool" {
        return None;
    }
    let field = |key| request.get(key).and_then(Value::as_str);
    let tool = field("tool_name").unwrap_or("tool").to_string();
    if field("decision_reason_type") != Some("rule") {
        return Some(Prompt::Refuse { request_id, tool });
    }
    let input = request.get("input").cloned().unwrap_or(Value::Null);
    let call = ToolCall {
        call_id: field("tool_use_id").unwrap_or("").to_string(),
        name: tool,
        summary: summarize(&input),
        input: serde_json::to_string_pretty(&input).unwrap_or_default(),
        result: None,
        is_error: false,
    };
    Some(Prompt::Ask {
        request_id,
        call,
        input,
    })
}

fn control_response(request_id: &str, response: Value) -> Value {
    json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": request_id, "response": response },
    })
}

fn deny(request_id: &str, message: &str) -> Value {
    control_response(
        request_id,
        json!({ "behavior": "deny", "message": message }),
    )
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
fn decode(v: &Value) -> Vec<EngineEvent> {
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

        // Token-level text and thinking deltas. Tool input arrives here too (as
        // input_json_delta) but we render tools from the complete `assistant`
        // message below.
        "stream_event" => stream_delta(v).into_iter().collect(),

        // Completed assistant message: pull out tool_use blocks (text is already
        // streamed via deltas, so skip it to avoid duplication).
        "assistant" => content_blocks(v)
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
            .map(|b| EngineEvent::ToolStarted(tool_call(b)))
            .collect(),

        // Tool results are delivered as `user` messages by Claude Code.
        "user" => content_blocks(v)
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

        "rate_limit_event" => quota(v).map(EngineEvent::Quota).into_iter().collect(),

        "result" => vec![EngineEvent::Completed {
            session_id: v
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            // A turn the user stopped ends as an error (`aborted_streaming`, or
            // `aborted_tools` during a call or an approval); it is not one.
            is_error: v.get("is_error").and_then(Value::as_bool).unwrap_or(false)
                && !v
                    .get("terminal_reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| reason.starts_with("aborted_")),
            context: context_usage(v),
        }],

        _ => Vec::new(),
    }
}

/// A `tool_use` block, from stream-json or a transcript, before its result arrives.
pub(crate) fn tool_call(block: &Value) -> ToolCall {
    let field = |key| block.get(key).and_then(Value::as_str);
    let input = block.get("input");
    ToolCall {
        call_id: field("id").unwrap_or("").to_string(),
        name: field("name").unwrap_or("tool").to_string(),
        summary: input.map(summarize).unwrap_or_default(),
        input: input
            .map(|i| serde_json::to_string_pretty(i).unwrap_or_default())
            .unwrap_or_default(),
        result: None,
        is_error: false,
    }
}

/// Input fields that say what a Claude Code tool acts on, most telling first.
const SUMMARY_FIELDS: [&str; 9] = [
    "command",
    "file_path",
    "notebook_path",
    "pattern",
    "url",
    "query",
    "description",
    "skill",
    "prompt",
];

fn summarize(input: &Value) -> String {
    SUMMARY_FIELDS
        .iter()
        .find_map(|key| input.get(key).and_then(Value::as_str))
        .and_then(|s| s.lines().map(str::trim).find(|line| !line.is_empty()))
        .unwrap_or("")
        .to_string()
}

fn quota(v: &Value) -> Option<Quota> {
    let windows = v.pointer("/rate_limit_info/unifiedWindows")?;
    let window = |key: &str| {
        let w = windows.get(key)?;
        Some(QuotaWindow {
            used: w.get("utilization")?.as_f64()? as f32,
            resets_at: w.get("resetsAt")?.as_u64()?,
        })
    };
    Some(Quota {
        five_hour: window("five_hour"),
        seven_day: window("seven_day"),
    })
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

fn stream_delta(v: &Value) -> Option<EngineEvent> {
    let event = v.get("event")?;
    if event.get("type")?.as_str()? != "content_block_delta" {
        return None;
    }
    let delta = event.get("delta")?;
    let field = |key| Some(delta.get(key)?.as_str()?.to_string());
    match delta.get("type")?.as_str()? {
        "text_delta" => Some(EngineEvent::Delta {
            text: field("text")?,
        }),
        "thinking_delta" => Some(EngineEvent::ThinkingDelta {
            text: field("thinking")?,
        }),
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn line(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn text_and_thinking_deltas() {
        let text = line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}}"#,
        );
        assert!(matches!(&decode(&text)[..], [EngineEvent::Delta { text }] if text == "Hi"));

        let thinking = line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"Checking 391"}}}"#,
        );
        assert!(matches!(
            &decode(&thinking)[..],
            [EngineEvent::ThinkingDelta { text }] if text == "Checking 391"
        ));

        let signature = line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"signature_delta","signature":"x"}}}"#,
        );
        assert!(decode(&signature).is_empty());
    }

    #[test]
    fn tool_use_and_results() {
        let started = line(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"streamed already"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo build\nmore","description":"Build"}}]}}"#,
        );
        match &decode(&started)[..] {
            [EngineEvent::ToolStarted(call)] => {
                assert_eq!(call.call_id, "t1");
                assert_eq!(call.name, "Bash");
                assert_eq!(call.summary, "cargo build");
            }
            other => panic!("unexpected {other:?}"),
        }

        let result = line(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}]}}"#,
        );
        assert!(matches!(
            &decode(&result)[..],
            [EngineEvent::ToolResult { call_id, content, is_error: true }] if call_id == "t1" && content == "a\nb"
        ));
    }

    #[test]
    fn result_reports_context_and_stops_are_not_errors() {
        let done = line(
            r#"{"type":"result","is_error":false,"session_id":"s","usage":{"input_tokens":2,"cache_read_input_tokens":10,"cache_creation_input_tokens":20,"output_tokens":3},"modelUsage":{"main":{"contextWindow":1000000},"side":{"contextWindow":200000}}}"#,
        );
        match &decode(&done)[..] {
            [
                EngineEvent::Completed {
                    session_id,
                    is_error,
                    context,
                },
            ] => {
                assert_eq!(session_id.as_deref(), Some("s"));
                assert!(!is_error);
                assert_eq!(context.used, 35);
                assert_eq!(context.window, Some(1_000_000));
            }
            other => panic!("unexpected {other:?}"),
        }

        for (reason, expect_error) in [
            ("aborted_streaming", false),
            ("aborted_tools", false),
            ("max_turns", true),
        ] {
            let ended = line(&format!(
                r#"{{"type":"result","is_error":true,"terminal_reason":"{reason}"}}"#
            ));
            assert!(
                matches!(&decode(&ended)[..], [EngineEvent::Completed { is_error, .. }] if *is_error == expect_error),
                "{reason}"
            );
        }
    }

    #[test]
    fn rate_limit_event_becomes_quota() {
        let event = line(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","unifiedWindows":{"five_hour":{"utilization":0.09,"resetsAt":1792013400},"seven_day":{"utilization":0.19,"resetsAt":1792303200}}}}"#,
        );
        match &decode(&event)[..] {
            [EngineEvent::Quota(quota)] => {
                let five = quota.five_hour.unwrap();
                assert!((five.used - 0.09).abs() < 1e-6);
                assert_eq!(five.resets_at, 1792013400);
                assert_eq!(quota.seven_day.unwrap().resets_at, 1792303200);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn only_rule_prompts_reach_the_user() {
        let rule = line(
            r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"gh pr create --title x"},"decision_reason_type":"rule","tool_use_id":"t9"}}"#,
        );
        match permission_prompt(&rule) {
            Some(Prompt::Ask {
                request_id,
                call,
                input,
            }) => {
                assert_eq!(request_id, "r1");
                assert_eq!(call.call_id, "t9");
                assert_eq!(call.summary, "gh pr create --title x");
                assert_eq!(input["command"], "gh pr create --title x");
            }
            _ => panic!("rule prompt not offered to the user"),
        }

        let mode = line(
            r#"{"type":"control_request","request_id":"r2","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"a"}}}"#,
        );
        assert!(
            matches!(permission_prompt(&mode), Some(Prompt::Refuse { request_id, tool }) if request_id == "r2" && tool == "Write")
        );

        let other = line(
            r#"{"type":"control_request","request_id":"r3","request":{"subtype":"interrupt"}}"#,
        );
        assert!(permission_prompt(&other).is_none());
        assert!(permission_prompt(&line(r#"{"type":"result"}"#)).is_none());
    }

    #[test]
    fn summary_prefers_the_most_telling_field() {
        assert_eq!(
            summarize(&line(
                r#"{"description":"Build it","command":"cargo build"}"#
            )),
            "cargo build"
        );
        assert_eq!(summarize(&line(r#"{"file_path":"/a/b.rs"}"#)), "/a/b.rs");
        assert_eq!(
            summarize(&line(r#"{"command":"\n  ls -la\nwc"}"#)),
            "ls -la"
        );
        assert_eq!(summarize(&line(r#"{"todos":[]}"#)), "");
    }
}
