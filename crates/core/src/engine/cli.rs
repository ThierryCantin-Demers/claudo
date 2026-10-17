//! Drives the Claude Code CLI in bidirectional stream-json mode: one long-lived
//! `claude -p --input-format stream-json` process per session, each turn written
//! to its stdin as a JSONL user message, events parsed from its stdout.
//!
//! This is the seam that isolates CLI/stream-json specifics from the rest of the app.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{ChildStderr, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use claudo_protocol::{
    Background, CliCommand, Compaction, ContextUsage, Decision, ModelOption, Profile, Quota,
    QuotaWindow, TaskStatus, ToolCall,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::claude_bin;

/// The answer to one control request, from a short-lived process: no turn, no tokens.
fn ask(subtype: &str) -> io::Result<Value> {
    let mut child = Command::new(claude_bin())
        .current_dir(std::env::temp_dir())
        .arg("-p")
        .args(["--input-format", "stream-json"])
        .args(["--output-format", "stream-json"])
        .arg("--verbose")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("stdin piped");
    for request in ["initialize", subtype] {
        writeln!(
            stdin,
            "{}",
            json!({ "type": "control_request", "request_id": request, "request": { "subtype": request } })
        )?;
    }
    let stdout = child.stdout.take().expect("stdout piped");
    let response = BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .find(|v| {
            v.get("type").and_then(Value::as_str) == Some("control_response")
                && v.pointer("/response/request_id").and_then(Value::as_str) == Some(subtype)
        });
    drop(stdin);
    let _ = child.wait();
    response
        .and_then(|v| v.pointer("/response/response").cloned())
        .ok_or_else(|| io::ErrorKind::InvalidData.into())
}

/// The model families and slash commands the CLI offers, as its `initialize` response
/// lists them. A family is offered by the alias that runs its newest version; pinned
/// older versions and `default` are left out.
pub fn capabilities() -> io::Result<(Vec<ModelOption>, Vec<CliCommand>)> {
    let response = ask("initialize")?;
    let list = |key: &str| {
        response
            .get(key)
            .and_then(Value::as_array)
            .ok_or(io::ErrorKind::InvalidData)
    };
    let models = list("models")?
        .iter()
        .filter_map(|m| {
            let value = m.get("value")?.as_str()?;
            let label = m.get("displayName")?.as_str()?;
            (value != "default" && !value.starts_with("claude-")).then(|| ModelOption {
                value: value.to_string(),
                label: label.to_string(),
            })
        })
        .collect();
    let commands = list("commands")?
        .iter()
        .filter_map(|c| {
            let field = |key| c.get(key).and_then(Value::as_str).unwrap_or("").to_string();
            Some(CliCommand {
                name: c.get("name")?.as_str()?.to_string(),
                description: field("description"),
                argument_hint: field("argumentHint"),
            })
        })
        .collect();
    Ok((models, commands))
}

/// The subscription's usage windows, without waiting for a turn to report them.
pub fn usage() -> io::Result<Quota> {
    let response = ask("get_usage")?;
    let limits = response
        .get("rate_limits")
        .ok_or(io::ErrorKind::InvalidData)?;
    // Percents here, where a turn's `rate_limit_event` gives fractions.
    let window = |key: &str| {
        let window = limits.get(key)?;
        Some(QuotaWindow {
            used: window.get("utilization")?.as_f64()? as f32 / 100.0,
            resets_at: unix_seconds(window.get("resets_at")?.as_str()?)?,
        })
    };
    Ok(Quota {
        five_hour: window("five_hour"),
        seven_day: window("seven_day"),
    })
}

/// Unix seconds of an RFC 3339 time, such as `2026-10-15T21:30:00.476926+00:00`.
pub(crate) fn unix_seconds(time: &str) -> Option<u64> {
    let (date, rest) = time.split_once('T')?;
    let mut date = date.splitn(3, '-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let (clock, zone) = rest.split_at(rest.find(['Z', '+', '-']).unwrap_or(rest.len()));
    let mut clock = clock.splitn(3, ':');
    let hour = clock.next()?.parse::<i64>().ok()?;
    let minute = clock.next()?.parse::<i64>().ok()?;
    let second = clock.next()?.split('.').next()?.parse::<i64>().ok()?;
    let offset = match zone.chars().next() {
        None | Some('Z') => 0,
        Some(sign) => {
            let (h, m) = zone[1..].split_once(':')?;
            let offset = h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60;
            if sign == '-' { -offset } else { offset }
        }
    };
    // Days since 1970-01-01 in the proleptic Gregorian calendar (Howard Hinnant's days_from_civil).
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second - offset).ok()
}

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
    Started {
        session_id: String,
    },
    /// A message from someone other than the client, such as another agent, started this turn.
    Incoming {
        from: String,
        text: String,
    },
    /// A chunk of assistant text.
    Delta {
        text: String,
    },
    /// A chunk of the summary of Claude's reasoning.
    ThinkingDelta {
        text: String,
    },
    /// Claude invoked a tool; the call has no result yet.
    ToolStarted(ToolCall),
    /// The subscription's usage windows, reported once per turn.
    Quota(Quota),
    /// A call matched `ASK_RULES`; the process waits for `Session::answer`.
    ApprovalNeeded {
        request_id: String,
        call: ToolCall,
        scope: String,
    },
    /// A turn the CLI began by itself starts, for this reason.
    Notice {
        text: String,
    },
    /// A call running in the background started or ended. Its end can come between turns.
    Background {
        call_id: String,
        background: Background,
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
        /// The turn's final text; a local command such as `/context` streams none, so
        /// this is its only output.
        text: Option<String>,
    },
    /// The conversation is being summarized to free up context.
    Compacting,
    /// A request to the API failed and the CLI is waiting to try it again.
    Retrying {
        attempt: u32,
        max_retries: u32,
        error: String,
    },
    Compacted(Compaction),
    /// The summary a compaction left in place of the conversation.
    CompactSummary(String),
    /// The turn could not run (spawn failure, crash, or non-completing exit).
    Error {
        detail: String,
    },
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

/// The session a new process continues.
#[derive(Debug, Clone, Copy)]
pub struct Resume<'a> {
    pub session_id: &'a str,
    /// Continue it as a new session, leaving this one as it was for the app it belongs to.
    pub fork: bool,
}

/// Where the events of turns the client did not start go, such as one another agent's
/// message started.
pub type Incoming = Box<dyn Fn(EngineEvent) + Send + Sync>;

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
    turns: Mutex<Turns>,
    /// Signalled once the process has exited.
    exited: Condvar,
    incoming: Option<Incoming>,
    last_activity: Mutex<Instant>,
    /// Approvals awaiting an answer, keyed by request id; an allow echoes the input back.
    approvals: Mutex<HashMap<String, Pending>>,
    /// Scopes the user allowed always; their calls are allowed without asking.
    always: Mutex<HashSet<String>>,
}

struct Pending {
    input: Value,
    scope: String,
}

/// Turns are told apart by the ids the CLI reports as it starts each message, since it
/// also starts turns of its own, and queues the client's messages behind them.
#[derive(Default)]
struct Turns {
    /// Messages written but not started yet, oldest first, by id.
    queued: VecDeque<(String, Sender<EngineEvent>)>,
    /// Where the running turn's events go; `None` between turns.
    running: Option<Route>,
    exited: bool,
    /// The CLI reports which message each turn is for, so a turn starting without one is
    /// its own, not the next queued message's.
    reports_ids: bool,
}

enum Route {
    Client(Sender<EngineEvent>),
    Incoming,
}

impl Turns {
    fn start(&mut self, message: Option<&str>) {
        let client = self
            .queued
            .iter()
            .position(|(id, _)| Some(id.as_str()) == message)
            .and_then(|i| self.queued.remove(i));
        self.running = Some(match client {
            Some((_, sink)) => Route::Client(sink),
            None => Route::Incoming,
        });
    }
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
    /// `name` is how other agents find this one (`ListAgents`) and address it.
    pub fn spawn(
        config: SessionConfig,
        resume: Option<Resume>,
        name: &str,
        incoming: Option<Incoming>,
    ) -> io::Result<Self> {
        let mut cmd = Command::new(claude_bin());
        cmd.current_dir(&config.cwd)
            .env("CLAUDE_CODE_SESSION_NAME", name)
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
            // Echoes each message as its turn starts, with who sent it.
            .arg("--replay-user-messages")
            .arg("--verbose")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(resume) = resume {
            cmd.arg("--resume").arg(resume.session_id);
            if resume.fork {
                cmd.arg("--fork-session");
            }
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
            turns: Mutex::default(),
            exited: Condvar::new(),
            incoming,
            last_activity: Mutex::new(Instant::now()),
            approvals: Mutex::default(),
            always: Mutex::default(),
        });
        // Without it the process joins the other agents only once its first turn ends.
        shared.write(&json!({
            "type": "control_request",
            "request_id": Uuid::new_v4().to_string(),
            "request": { "subtype": "initialize" },
        }))?;

        // Drained continuously: a long-lived process would block once the pipe filled.
        let stderr = thread::spawn(move || stderr_tail(stderr));
        let reader = shared.clone();
        let bypass = config.profile == Profile::FullAuto;
        thread::spawn(move || {
            // The CLI starts a turn of its own for background commands that ended, and
            // echoes no message for it, so it opens with these.
            let mut notices: Vec<String> = Vec::new();
            // A turn's `result` sums the usage of every request it made; its context is the
            // last request's.
            let mut last_request = 0;
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                reader.touch();
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(context) = request_context(&v) {
                    last_request = context;
                }
                if let Some(message) = started_message(&v) {
                    let mut turns = reader.turns.lock().unwrap();
                    turns.reports_ids = true;
                    turns.start(message);
                    notices.clear();
                    continue;
                }
                notices.extend(ended_notice(&v));
                if v.get("type").and_then(Value::as_str) == Some("control_cancel_request") {
                    if let Some(id) = v.get("request_id").and_then(Value::as_str) {
                        reader.approvals.lock().unwrap().remove(id);
                    }
                    continue;
                }
                let events = match permission_prompt(&v, bypass) {
                    Some(Prompt::Ask {
                        request_id,
                        call,
                        input,
                        scope,
                    }) => {
                        if reader.always.lock().unwrap().contains(&scope) {
                            let _ = reader.write(&allow(&request_id, input));
                            Vec::new()
                        } else {
                            reader.approvals.lock().unwrap().insert(
                                request_id.clone(),
                                Pending {
                                    input,
                                    scope: scope.clone(),
                                },
                            );
                            vec![EngineEvent::ApprovalNeeded {
                                request_id,
                                call: *call,
                                scope,
                            }]
                        }
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
                for mut ev in events {
                    if let EngineEvent::Completed { context, .. } = &mut ev {
                        context.used = std::mem::take(&mut last_request);
                    }
                    let completed = matches!(ev, EngineEvent::Completed { .. });
                    let mut turns = reader.turns.lock().unwrap();
                    let incoming = matches!(ev, EngineEvent::Incoming { .. });
                    if turns.running.is_none()
                        && (incoming || matches!(ev, EngineEvent::Started { .. }))
                    {
                        // A CLI that reports no message ids: turns run in the order sent.
                        let next = turns
                            .queued
                            .front()
                            .map(|(id, _)| id.clone())
                            .filter(|_| !turns.reports_ids && !incoming);
                        turns.start(next.as_deref());
                        if !incoming
                            && matches!(turns.running, Some(Route::Incoming))
                            && !notices.is_empty()
                        {
                            let notice = EngineEvent::Notice {
                                text: notices.join("\n"),
                            };
                            reader.route(&turns, notice);
                        }
                        notices.clear();
                    }
                    reader.route(&turns, ev);
                    if completed {
                        turns.running = None;
                    }
                }
            }

            let status = child.wait();
            let err = stderr.join().unwrap_or_default();
            let mut turns = reader.turns.lock().unwrap();
            turns.exited = true;
            reader.exited.notify_all();
            let detail = if err.trim().is_empty() {
                match status {
                    Ok(s) => format!("claude exited without completing the turn ({s})"),
                    Err(e) => format!("waiting on claude failed: {e}"),
                }
            } else {
                err.trim().to_string()
            };
            if turns.running.is_some() {
                reader.route(
                    &turns,
                    EngineEvent::Error {
                        detail: detail.clone(),
                    },
                );
            }
            turns.running = None;
            for (_, sink) in std::mem::take(&mut turns.queued) {
                let _ = sink.send(EngineEvent::Error {
                    detail: detail.clone(),
                });
            }
        });

        Ok(Self {
            config,
            pid,
            shared,
        })
    }

    /// Start a turn, or queue it behind the running one. Its events arrive on the
    /// returned receiver, which closes after `Completed` or `Error`. Fails if the
    /// process has exited.
    pub fn send(&mut self, prompt: &str) -> io::Result<Receiver<EngineEvent>> {
        let (tx, rx) = mpsc::channel();
        let id = Uuid::new_v4().to_string();
        {
            let mut turns = self.shared.turns.lock().unwrap();
            if turns.exited {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            turns.queued.push_back((id.clone(), tx));
        }
        self.shared.touch();

        let message = json!({
            "type": "user",
            "uuid": id,
            "message": { "role": "user", "content": [{ "type": "text", "text": prompt }] },
        });
        if let Err(e) = self.shared.write(&message) {
            self.shared
                .turns
                .lock()
                .unwrap()
                .queued
                .retain(|(queued, _)| *queued != id);
            return Err(e);
        }
        Ok(rx)
    }

    /// Stop any turn, close stdin and wait up to `timeout` for the process to exit, so
    /// nothing it writes outlives files the caller removes next.
    pub fn shutdown(self, timeout: Duration) {
        if self.is_busy() {
            let _ = self.interrupt();
        }
        self.shared.stdin.lock().unwrap().take();
        let turns = self.shared.turns.lock().unwrap();
        let _ = self
            .shared
            .exited
            .wait_timeout_while(turns, timeout, |turns| !turns.exited);
    }

    /// Change the name other agents know this one by.
    pub fn rename(&self, name: &str) -> io::Result<()> {
        self.shared.write(&json!({
            "type": "control_request",
            "request_id": Uuid::new_v4().to_string(),
            "request": { "subtype": "rename_session", "title": name, "source": "host" },
        }))
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

    /// Answer an `ApprovalNeeded`; the waiting call then runs or fails. Returns its scope.
    pub fn answer(&self, request_id: &str, decision: Decision) -> io::Result<String> {
        let Pending { input, scope } = self
            .shared
            .approvals
            .lock()
            .unwrap()
            .remove(request_id)
            .ok_or(io::ErrorKind::NotFound)?;
        if decision == Decision::AllowAlways {
            self.allow_always(&scope);
        }
        let response = match decision {
            Decision::Deny => deny(request_id, "The user denied this."),
            Decision::AllowOnce | Decision::AllowAlways => allow(request_id, input),
        };
        self.shared.write(&response)?;
        Ok(scope)
    }

    /// Allow later calls in `scope` without an `ApprovalNeeded`.
    pub fn allow_always(&self, scope: &str) {
        self.shared.always.lock().unwrap().insert(scope.to_string());
    }

    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn is_alive(&self) -> bool {
        !self.shared.turns.lock().unwrap().exited
    }

    /// A turn is running or waiting to.
    pub fn is_busy(&self) -> bool {
        let turns = self.shared.turns.lock().unwrap();
        turns.running.is_some() || !turns.queued.is_empty()
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

    fn route(&self, turns: &Turns, ev: EngineEvent) {
        match &turns.running {
            Some(Route::Client(sink)) => {
                let _ = sink.send(ev);
            }
            Some(Route::Incoming) => {
                if let Some(incoming) = &self.incoming {
                    incoming(ev);
                }
            }
            None => {
                if let (EngineEvent::Background { .. }, Some(incoming)) = (&ev, &self.incoming) {
                    incoming(ev);
                }
            }
        }
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
        call: Box<ToolCall>,
        input: Value,
        /// What allowing it always covers: the rule it matched, or the call itself when
        /// something besides a rule may also have asked.
        scope: String,
    },
    Refuse {
        request_id: String,
        tool: String,
    },
}

/// `bypass`: the process runs in `bypassPermissions`, where only a rule asks.
fn permission_prompt(v: &Value, bypass: bool) -> Option<Prompt> {
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
    let reason = field("decision_reason_type");
    let command = request.pointer("/input/command").and_then(Value::as_str);
    let rule = command.and_then(ask_rule_in);
    let asked = match reason {
        Some("rule") => true,
        Some("subcommandResults") => rule.is_some(),
        _ => false,
    };
    if !asked {
        return Some(Prompt::Refuse { request_id, tool });
    }
    let input = request.get("input").cloned().unwrap_or(Value::Null);
    let scope = match (tool.as_str(), command) {
        ("Bash", Some(command)) => match rule {
            Some(rule) if bypass || reason == Some("rule") => rule.to_string(),
            _ => command.to_string(),
        },
        _ if ASK_RULES.contains(&tool.as_str()) => tool.clone(),
        _ => format!("{tool} {}", summarize(&input)),
    };
    let call = ToolCall {
        call_id: field("tool_use_id").unwrap_or("").to_string(),
        name: tool,
        summary: summarize(&input),
        description: String::new(),
        command: None,
        input: serde_json::to_string_pretty(&input).unwrap_or_default(),
        result: None,
        is_error: false,
        background: None,
    };
    Some(Prompt::Ask {
        request_id,
        call: Box::new(call),
        input,
        scope,
    })
}

/// The command prefix of the `Bash(...)` ask rule a part of `command` matches. The CLI
/// reports a compound command as `subcommandResults`, without naming the part that asked.
fn ask_rule_in(command: &str) -> Option<&'static str> {
    command
        .split(['&', '|', ';', '\n', '(', ')', '{', '}', '`'])
        .find_map(|part| {
            let words: Vec<&str> = part
                .split_whitespace()
                .skip_while(|word| word.contains('='))
                .collect();
            ASK_RULES
                .iter()
                .filter_map(|rule| rule.strip_prefix("Bash(")?.strip_suffix(":*)"))
                .find(|prefix| words.starts_with(&prefix.split_whitespace().collect::<Vec<_>>()))
        })
}

/// The message id of a `command_lifecycle` line reporting a message's turn starting.
/// The outer `Option` is whether the line is one; the id is missing for a turn the
/// CLI started without a message.
fn started_message(v: &Value) -> Option<Option<&str>> {
    if v.get("type")?.as_str()? != "command_lifecycle" || v.get("state")?.as_str()? != "started" {
        return None;
    }
    Some(v.get("command_uuid").and_then(Value::as_str))
}

/// Who sent a message the client did not, and what it says, from its stream-json
/// echo or its transcript line. The client's own messages carry no `origin`.
/// The CLI's one line about a background command that ended.
fn ended_notice(v: &Value) -> Option<String> {
    (v.get("subtype")?.as_str()? == "task_notification")
        .then(|| v.get("summary")?.as_str().map(str::to_owned))
        .flatten()
}

pub(crate) fn incoming(v: &Value) -> Option<(String, String)> {
    let origin = v.get("origin")?;
    let field = |key| origin.get(key).and_then(Value::as_str);
    if field("kind") == Some("task-notification") {
        return None;
    }
    let from = field("name").or(field("kind")).unwrap_or("another session");
    let text = match (field("body"), v.pointer("/message/content")) {
        (Some(body), _) => body.to_string(),
        (None, Some(Value::String(text))) => text.clone(),
        (None, _) => content_blocks(v)
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
    };
    Some((from.to_string(), text))
}

fn control_response(request_id: &str, response: Value) -> Value {
    json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": request_id, "response": response },
    })
}

fn allow(request_id: &str, input: Value) -> Value {
    control_response(
        request_id,
        json!({ "behavior": "allow", "updatedInput": input }),
    )
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

        "system" if v.get("subtype").and_then(Value::as_str) == Some("api_retry") => {
            let number = |key| v.get(key).and_then(Value::as_u64).unwrap_or(0) as u32;
            let error = match (
                v.get("error").and_then(Value::as_str),
                v.get("error_status").and_then(Value::as_u64),
            ) {
                (Some(error), Some(status)) => format!("{error} ({status})"),
                (Some(error), None) => error.to_string(),
                (None, Some(status)) => format!("HTTP {status}"),
                (None, None) => "an API error".to_string(),
            };
            vec![EngineEvent::Retrying {
                attempt: number("attempt"),
                max_retries: number("max_retries"),
                error,
            }]
        }

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

        // A message starting its turn, echoed by `--replay-user-messages`.
        // The summary a compaction leaves in place of the conversation.
        "user" if v.get("isSynthetic").and_then(Value::as_bool) == Some(true) => v
            .pointer("/message/content")
            .and_then(Value::as_str)
            .map(|text| EngineEvent::CompactSummary(text.to_string()))
            .into_iter()
            .collect(),

        "user" if v.get("isReplay").and_then(Value::as_bool) == Some(true) => incoming(v)
            .map(|(from, text)| EngineEvent::Incoming { from, text })
            .into_iter()
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

        "system" => match v.get("subtype").and_then(Value::as_str) {
            Some("status") if v.get("status").and_then(Value::as_str) == Some("compacting") => {
                vec![EngineEvent::Compacting]
            }
            Some("compact_boundary") => {
                compaction(v.get("compact_metadata"), "pre_tokens", "post_tokens")
                    .map(EngineEvent::Compacted)
                    .into_iter()
                    .collect()
            }
            _ => background_task(v).into_iter().collect(),
        },

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
            context: ContextUsage {
                used: 0,
                window: context_window(v),
            },
            text: v
                .get("result")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .map(str::to_owned),
        }],

        _ => Vec::new(),
    }
}

/// A compaction's trigger and sizes, from stream-json's `compact_metadata` or the
/// transcript's `compactMetadata`, which name the sizes differently.
pub(crate) fn compaction(
    metadata: Option<&Value>,
    before: &str,
    after: &str,
) -> Option<Compaction> {
    let metadata = metadata?;
    let tokens = |key| metadata.get(key).and_then(Value::as_u64).unwrap_or(0);
    Some(Compaction {
        auto: metadata.get("trigger").and_then(Value::as_str) == Some("auto"),
        before: tokens(before),
        after: tokens(after),
        summary: String::new(),
    })
}

/// A slash command as the transcript records it, `<command-name>/compact</command-name>`
/// and its arguments, back to what the user typed.
pub(crate) fn command_echo(text: &str) -> Option<String> {
    let name = xml_tag(text, "command-name")?;
    let args = xml_tag(text, "command-args").unwrap_or("").trim();
    Some(if args.is_empty() {
        name.to_string()
    } else {
        format!("{name} {args}")
    })
}

/// What a local command printed, as the transcript records it; `Some("")` for a line that
/// is one with nothing worth showing, such as compaction's "Compacted".
pub(crate) fn command_output(text: &str) -> Option<&str> {
    let output = xml_tag(text, "local-command-stdout")?.trim();
    Some(if output == "Compacted" { "" } else { output })
}

/// A background call starting (`task_started`) or ending (`task_notification`).
fn background_task(v: &Value) -> Option<EngineEvent> {
    let field = |key| v.get(key).and_then(Value::as_str);
    let background = match field("subtype")? {
        "task_started" if v.get("is_backgrounded").and_then(Value::as_bool) == Some(true) => {
            Background {
                status: TaskStatus::Running,
                output: None,
            }
        }
        "task_notification" => ended(field("status")?, field("output_file")),
        _ => return None,
    };
    Some(EngineEvent::Background {
        call_id: field("tool_use_id")?.to_string(),
        background,
    })
}

/// A background task's end, from the message the CLI gives the model when one ends, which
/// the transcript keeps as a user prompt.
pub(crate) struct TaskEnd {
    pub call_id: String,
    pub background: Background,
    pub summary: String,
}

pub(crate) fn task_notification(text: &str) -> Option<TaskEnd> {
    if !text.trim_start().starts_with("<task-notification>") {
        return None;
    }
    Some(TaskEnd {
        call_id: xml_tag(text, "tool-use-id")?.to_string(),
        background: ended(xml_tag(text, "status")?, xml_tag(text, "output-file")),
        summary: xml_tag(text, "summary")
            .unwrap_or("A background command ended")
            .to_string(),
    })
}

fn xml_tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let start = text.find(&format!("<{name}>"))? + name.len() + 2;
    let end = start + text[start..].find(&format!("</{name}>"))?;
    Some(&text[start..end])
}

/// Whether a tool result is the one a call moved to the background gets.
pub(crate) fn backgrounded(result: &str) -> bool {
    result.starts_with("Command running in background with ID:")
}

const TASK_OUTPUT_BYTES: u64 = 64 * 1024;

fn ended(status: &str, output_file: Option<&str>) -> Background {
    Background {
        status: match status {
            "completed" => TaskStatus::Completed,
            "killed" | "stopped" => TaskStatus::Stopped,
            _ => TaskStatus::Failed,
        },
        output: output_file.and_then(task_output),
    }
}

/// The end of a task's output file; the CLI's temporary files may be gone already.
fn task_output(path: &str) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TASK_OUTPUT_BYTES)))
        .ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// A `tool_use` block, from stream-json or a transcript, before its result arrives.
pub(crate) fn tool_call(block: &Value) -> ToolCall {
    let field = |key| block.get(key).and_then(Value::as_str);
    let input = block.get("input");
    let input_field = |key| input.and_then(|i| i.get(key)).and_then(Value::as_str);
    ToolCall {
        call_id: field("id").unwrap_or("").to_string(),
        name: field("name").unwrap_or("tool").to_string(),
        summary: input.map(summarize).unwrap_or_default(),
        description: input_field("description").unwrap_or("").to_string(),
        command: input_field("command").map(str::to_owned),
        input: input
            .map(|i| serde_json::to_string_pretty(i).unwrap_or_default())
            .unwrap_or_default(),
        result: None,
        is_error: false,
        background: None,
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

/// The tokens one request of the main agent had in context, from its `assistant` message;
/// a subagent's carry the call that started it.
fn request_context(v: &Value) -> Option<u64> {
    let main = v.get("parent_tool_use_id").is_none_or(Value::is_null);
    if v.get("type")?.as_str()? != "assistant" || !main {
        return None;
    }
    let usage = v.pointer("/message/usage")?;
    let n = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    Some(
        n("input_tokens")
            + n("cache_read_input_tokens")
            + n("cache_creation_input_tokens")
            + n("output_tokens"),
    )
}

/// The model's context window, from the `result` event's `modelUsage`.
fn context_window(v: &Value) -> Option<u64> {
    // Keyed by model name; side models such as the title generator appear too.
    v.get("modelUsage")?
        .as_object()?
        .values()
        .filter_map(|m| m.get("contextWindow").and_then(Value::as_u64))
        .max()
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
    fn a_retried_request_says_why() {
        let retry = line(
            r#"{"type":"system","subtype":"api_retry","attempt":2,"max_retries":10,"retry_delay_ms":4000,"error_status":529,"error":"overloaded"}"#,
        );
        assert!(matches!(
            &decode(&retry)[..],
            [EngineEvent::Retrying { attempt: 2, max_retries: 10, error }] if error == "overloaded (529)"
        ));
    }

    #[test]
    fn context_is_the_last_request_of_the_main_agent() {
        let usage = r#""usage":{"input_tokens":2,"cache_read_input_tokens":10,"cache_creation_input_tokens":20,"output_tokens":3}"#;
        let main = line(&format!(
            r#"{{"type":"assistant","parent_tool_use_id":null,"message":{{{usage}}}}}"#
        ));
        assert_eq!(request_context(&main), Some(35));
        let subagent = line(&format!(
            r#"{{"type":"assistant","parent_tool_use_id":"t1","message":{{{usage}}}}}"#
        ));
        assert_eq!(request_context(&subagent), None);
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
                    ..
                },
            ] => {
                assert_eq!(session_id.as_deref(), Some("s"));
                assert!(!is_error);
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
        match permission_prompt(&rule, false) {
            Some(Prompt::Ask {
                request_id,
                call,
                input,
                scope,
            }) => {
                assert_eq!(request_id, "r1");
                assert_eq!(call.call_id, "t9");
                assert_eq!(call.summary, "gh pr create --title x");
                assert_eq!(input["command"], "gh pr create --title x");
                assert_eq!(scope, "gh pr create");
            }
            _ => panic!("rule prompt not offered to the user"),
        }

        let mode = line(
            r#"{"type":"control_request","request_id":"r2","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"a"}}}"#,
        );
        assert!(
            matches!(permission_prompt(&mode, false), Some(Prompt::Refuse { request_id, tool }) if request_id == "r2" && tool == "Write")
        );

        let other = line(
            r#"{"type":"control_request","request_id":"r3","request":{"subtype":"interrupt"}}"#,
        );
        assert!(permission_prompt(&other, false).is_none());
        assert!(permission_prompt(&line(r#"{"type":"result"}"#), false).is_none());
    }

    #[test]
    fn compound_commands_ask_when_a_part_matches_a_rule() {
        let request = |command: &str| {
            line(&format!(
                r#"{{"type":"control_request","request_id":"r1","request":{{"subtype":"can_use_tool","tool_name":"Bash","input":{{"command":{}}},"decision_reason_type":"subcommandResults"}}}}"#,
                Value::from(command)
            ))
        };
        let scope = |command, bypass| match permission_prompt(&request(command), bypass) {
            Some(Prompt::Ask { scope, .. }) => Some(scope),
            _ => None,
        };
        let compound = "cd /x && gh issue create --help";
        assert_eq!(scope(compound, true).unwrap(), "gh issue create");
        // Outside bypassPermissions the cd may be what asked, so only the exact call is covered.
        assert_eq!(scope(compound, false).unwrap(), compound);
        assert_eq!(scope("cd /x && ls", true), None);

        assert_eq!(
            ask_rule_in("git push; GH_REPO=a/b gh pr create --fill"),
            Some("gh pr create")
        );
        assert!(ask_rule_in("echo $(gh  issue create -t x)").is_some());
        assert!(ask_rule_in("cd /x\ngh issue create | cat").is_some());
        assert!(ask_rule_in("cd /x && gh issue list").is_none());
        assert!(ask_rule_in("gh issue created").is_none());
    }

    #[test]
    fn background_tasks_start_and_end() {
        let started = line(
            r#"{"type":"system","subtype":"task_started","task_id":"b1","tool_use_id":"t1","is_backgrounded":true,"task_type":"local_bash"}"#,
        );
        match &decode(&started)[..] {
            [
                EngineEvent::Background {
                    call_id,
                    background,
                },
            ] => {
                assert_eq!(call_id, "t1");
                assert_eq!(background.status, TaskStatus::Running);
            }
            other => panic!("unexpected {other:?}"),
        }
        let ended = line(
            r#"{"type":"system","subtype":"task_notification","task_id":"b1","tool_use_id":"t1","status":"failed","output_file":"/nonexistent/b1.output"}"#,
        );
        match &decode(&ended)[..] {
            [EngineEvent::Background { background, .. }] => {
                assert_eq!(background.status, TaskStatus::Failed);
                assert_eq!(background.output, None);
            }
            other => panic!("unexpected {other:?}"),
        }

        let end = task_notification(
            "<task-notification>\n<task-id>b1</task-id>\n<tool-use-id>t1</tool-use-id>\n<output-file>/nonexistent</output-file>\n<status>completed</status>\n<summary>It ended</summary>\n</task-notification>",
        )
        .unwrap();
        assert_eq!(end.call_id, "t1");
        assert_eq!(end.background.status, TaskStatus::Completed);
        assert_eq!(end.summary, "It ended");
        assert!(task_notification("please read <task-notification> docs").is_none());
    }

    #[test]
    fn usage_reset_times_become_unix_seconds() {
        assert_eq!(
            unix_seconds("2026-10-15T21:30:00.476926+00:00"),
            Some(1_792_099_800)
        );
        assert_eq!(
            unix_seconds("2026-10-18T06:00:00-04:30"),
            Some(1_792_319_400)
        );
        assert_eq!(unix_seconds("2026-10-15T21:30:00Z"), Some(1_792_099_800));
        assert_eq!(unix_seconds("tomorrow"), None);
    }

    #[test]
    fn compaction_is_reported_and_read_back() {
        let status = line(r#"{"type":"system","subtype":"status","status":"compacting"}"#);
        assert!(matches!(&decode(&status)[..], [EngineEvent::Compacting]));
        let boundary = line(
            r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"manual","pre_tokens":35344,"post_tokens":1829}}"#,
        );
        match &decode(&boundary)[..] {
            [EngineEvent::Compacted(c)] => {
                assert!(!c.auto);
                assert_eq!((c.before, c.after), (35344, 1829));
            }
            other => panic!("unexpected {other:?}"),
        }
        let summary = line(
            r#"{"type":"user","isReplay":false,"isSynthetic":true,"message":{"role":"user","content":"This session is being continued..."}}"#,
        );
        assert!(matches!(
            &decode(&summary)[..],
            [EngineEvent::CompactSummary(_)]
        ));

        let local = line(r#"{"type":"result","is_error":false,"result":"Context Usage"}"#);
        assert!(
            matches!(&decode(&local)[..], [EngineEvent::Completed { text: Some(t), .. }] if t == "Context Usage")
        );

        assert_eq!(
            command_echo("<command-name>/compact</command-name>\n<command-message>compact</command-message>\n<command-args>keep it short</command-args>").as_deref(),
            Some("/compact keep it short")
        );
        assert_eq!(
            command_output("<local-command-stdout>Compacted </local-command-stdout>"),
            Some("")
        );
        assert_eq!(
            command_output("<local-command-stdout>42 tokens</local-command-stdout>"),
            Some("42 tokens")
        );
        assert_eq!(command_output("just text"), None);
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

    #[test]
    fn another_agents_message_is_incoming_and_the_clients_own_is_not() {
        let peer = line(
            r#"{"type":"user","isReplay":true,"uuid":"5b4e","message":{"role":"user","content":"Another Claude session sent a message: ..."},"origin":{"kind":"peer","name":"bravo","body":"ping"}}"#,
        );
        assert!(matches!(
            &decode(&peer)[..],
            [EngineEvent::Incoming { from, text }] if from == "bravo" && text == "ping"
        ));

        let own = line(
            r#"{"type":"user","isReplay":true,"uuid":"1111","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
        );
        assert!(decode(&own).is_empty());
    }

    #[test]
    fn turns_follow_the_ids_the_cli_starts() {
        let started = |id: &str| {
            line(&format!(
                r#"{{"type":"command_lifecycle","command_uuid":"{id}","state":"started"}}"#
            ))
        };
        assert_eq!(started_message(&started("a")), Some(Some("a")));
        assert_eq!(
            started_message(&line(
                r#"{"type":"command_lifecycle","command_uuid":"a","state":"queued"}"#
            )),
            None
        );

        let mut turns = Turns::default();
        let (first, _) = mpsc::channel();
        let (second, _) = mpsc::channel();
        turns.queued.push_back(("a".to_string(), first));
        turns.queued.push_back(("b".to_string(), second));

        turns.start(started_message(&started("peer")).unwrap());
        assert!(matches!(turns.running, Some(Route::Incoming)));
        turns.start(started_message(&started("b")).unwrap());
        assert!(matches!(turns.running, Some(Route::Client(_))));
        assert_eq!(turns.queued.len(), 1);
    }
}
