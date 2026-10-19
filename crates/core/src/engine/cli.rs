//! Drives the Claude Code CLI in bidirectional stream-json mode: one long-lived
//! `claude -p --input-format stream-json` process per session, each turn written
//! to its stdin as a JSONL user message, events parsed from its stdout.
//!
//! This is the seam that isolates CLI/stream-json specifics from the rest of the app.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::prelude::{BASE64_STANDARD, Engine as _};
use claudo_protocol::{
    Attachment, Background, CliCommand, Compaction, ContextUsage, Decision, Finding, ModelOption,
    Profile, Quota, QuotaWindow, Review, TaskStatus, ToolCall,
};
use serde::{Deserialize, Serialize};
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
    unix_millis(time).map(|millis| millis / 1000)
}

/// Unix milliseconds of an RFC 3339 time.
pub(crate) fn unix_millis(time: &str) -> Option<u64> {
    let (date, rest) = time.split_once('T')?;
    let mut date = date.splitn(3, '-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let (clock, zone) = rest.split_at(rest.find(['Z', '+', '-']).unwrap_or(rest.len()));
    let mut clock = clock.splitn(3, ':');
    let hour = clock.next()?.parse::<i64>().ok()?;
    let minute = clock.next()?.parse::<i64>().ok()?;
    let mut seconds = clock.next()?.splitn(2, '.');
    let second = seconds.next()?.parse::<i64>().ok()?;
    let millis = format!("{:0<3}", seconds.next().unwrap_or(""))[..3]
        .parse::<i64>()
        .ok()?;
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
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
    u64::try_from(seconds * 1000 + millis).ok()
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
    /// The client's oldest message still waiting reached the model: it started this turn,
    /// or `joined` the running one, which the CLI hands a message sent mid-turn at its next
    /// step.
    Delivered {
        joined: bool,
    },
    /// The user's own message, typed elsewhere such as the Claude app through Remote
    /// Control, started this turn or joined it.
    Typed {
        text: String,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionConfig {
    pub cwd: PathBuf,
    pub profile: Profile,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// What a turn is sent: the user's text and the files that go with it.
#[derive(Debug, Clone, Copy)]
pub struct UserMessage<'a> {
    pub text: &'a str,
    pub attachments: &'a [Attachment],
}

impl<'a> From<&'a str> for UserMessage<'a> {
    fn from(text: &'a str) -> Self {
        Self {
            text,
            attachments: &[],
        }
    }
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
    shared: Arc<Shared>,
}

/// State shared with the thread reading the process's stdout.
struct Shared {
    pid: u32,
    /// `None` once the session is dropped.
    stdin: Mutex<Option<File>>,
    /// Its stdin, stdout and stderr, which a handover passes on.
    fds: [RawFd; 3],
    /// Full auto, where the CLI asks about nothing but what the ask rules cover.
    bypass: bool,
    turns: Mutex<Turns>,
    /// Signalled once the process has exited.
    exited: Condvar,
    incoming: Option<Incoming>,
    last_activity: Mutex<Instant>,
    /// Approvals awaiting an answer, keyed by request id; an allow echoes the input back.
    approvals: Mutex<HashMap<String, Pending>>,
    /// Scopes the user allowed always; their calls are allowed without asking.
    always: Mutex<HashSet<String>>,
    /// Calls of the running turn already shown, which a forked command's replay repeats.
    shown: Mutex<HashSet<String>>,
    /// Calls running in the background, which keep the process alive with stdin closed.
    background: Mutex<HashSet<String>>,
    /// The reader reads and handles output only while holding this, so a handover that
    /// holds it finds no line half handled.
    reading: Mutex<Reading>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Pending {
    input: Value,
    scope: String,
    /// What the approval asked about, for an app taking the session over to ask again.
    call: ToolCall,
}

/// What the reader keeps between the lines it reads.
#[derive(Default)]
struct Reading {
    /// Read, but not a whole line yet.
    unread: Vec<u8>,
    /// The CLI starts a turn of its own for background commands that ended, and echoes no
    /// message for it, so it opens with these.
    notices: Vec<String>,
    /// A turn's `result` sums the usage of every request it made; its context is the last
    /// request's.
    last_request: u64,
    /// Forked commands' subagents followed, by task: their session, and what stops it.
    following: HashMap<String, (String, Arc<AtomicBool>)>,
    /// The output ended, so the process is done and nothing is handed over.
    closed: bool,
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
        self.running = Some(match self.take(message) {
            Some(sink) => Route::Client(sink),
            None => Route::Incoming,
        });
    }

    /// `message` starts its turn, or joins the running one. `Some(joined)` when it is the
    /// client's.
    fn begin(&mut self, message: Option<&str>) -> Option<bool> {
        if self.running.is_some() {
            return self.take(message).map(|_| true);
        }
        self.start(message);
        matches!(self.running, Some(Route::Client(_))).then_some(false)
    }

    fn take(&mut self, message: Option<&str>) -> Option<Sender<EngineEvent>> {
        let i = self
            .queued
            .iter()
            .position(|(id, _)| Some(id.as_str()) == message)?;
        self.queued.remove(i).map(|(_, sink)| sink)
    }
}

/// A session as a handover passes it to the process replacing this one: everything but
/// its channels, which the new process makes anew.
#[derive(Serialize, Deserialize)]
pub struct SessionState {
    config: SessionConfig,
    pid: u32,
    fds: [RawFd; 3],
    /// Output read but not yet a whole line, in base64.
    unread: String,
    queued: Vec<String>,
    running: Option<Running>,
    reports_ids: bool,
    approvals: HashMap<String, Pending>,
    always: HashSet<String>,
    shown: HashSet<String>,
    background: HashSet<String>,
    notices: Vec<String>,
    last_request: u64,
    /// Subagents followed: their task, then their session.
    following: Vec<(String, String)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Running {
    Client,
    /// One the CLI began itself, such as for another agent's message.
    Incoming,
}

impl SessionState {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn fds(&self) -> [RawFd; 3] {
        self.fds
    }

    pub fn running(&self) -> Option<Running> {
        self.running
    }
}

/// What a session taken over from the process this one replaced was doing.
pub struct Adopted {
    /// The running turn's events, when the client started it.
    pub running: Option<Receiver<EngineEvent>>,
    /// The events of messages sent but not started yet, oldest first.
    pub queued: Vec<Receiver<EngineEvent>>,
    /// Approvals the running turn waits on: request id, call and scope.
    pub approvals: Vec<(String, ToolCall, String)>,
}

/// The reader's state held, so nothing more is read until the process is replaced or this
/// is dropped.
pub(crate) struct Paused<'a> {
    _reading: std::sync::MutexGuard<'a, Reading>,
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
            // What a handover tells the process replacing this one is not the agent's.
            .env_remove(crate::handover::FILE_VAR)
            .env_remove(crate::handover::KEPT_VAR)
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
        let pipe = |fd: Option<OwnedFd>| File::from(fd.expect("piped"));
        let files = [
            pipe(child.stdin.take().map(OwnedFd::from)),
            pipe(child.stdout.take().map(OwnedFd::from)),
            pipe(child.stderr.take().map(OwnedFd::from)),
        ];
        let state = SessionState {
            config,
            pid: child.id(),
            fds: files.each_ref().map(AsRawFd::as_raw_fd),
            unread: String::new(),
            queued: Vec::new(),
            running: None,
            reports_ids: false,
            approvals: HashMap::new(),
            always: HashSet::new(),
            shown: HashSet::new(),
            background: HashSet::new(),
            notices: Vec::new(),
            last_request: 0,
            following: Vec::new(),
        };
        let (session, ..) = Self::start(state, files, incoming);
        // Without it the process joins the other agents only once its first turn ends.
        session.shared.write(&json!({
            "type": "control_request",
            "request_id": Uuid::new_v4().to_string(),
            "request": { "subtype": "initialize" },
        }))?;
        Ok(session)
    }

    /// Take over a session the process this one replaced was running, its pipes inherited
    /// across the exec.
    pub fn adopt(state: SessionState, incoming: Option<Incoming>) -> (Self, Adopted) {
        // SAFETY: a handover passes these descriptors to this process alone, open and owned
        // by nothing else here.
        let files = state.fds.map(|fd| unsafe { File::from_raw_fd(fd) });
        for file in &files {
            set_cloexec(file.as_raw_fd(), true);
        }
        let (session, running, queued) = Self::start(state, files, incoming);
        let approvals = session
            .shared
            .approvals
            .lock()
            .unwrap()
            .iter()
            .map(|(id, pending)| (id.clone(), pending.call.clone(), pending.scope.clone()))
            .collect();
        let adopted = Adopted {
            running,
            queued,
            approvals,
        };
        (session, adopted)
    }

    /// Starts reading a process's output, from where `state` says it was. Returns the
    /// session with the events of its running client turn and of its queued messages.
    fn start(
        state: SessionState,
        [stdin, stdout, stderr]: [File; 3],
        incoming: Option<Incoming>,
    ) -> (
        Self,
        Option<Receiver<EngineEvent>>,
        Vec<Receiver<EngineEvent>>,
    ) {
        let mut receivers = Vec::new();
        let mut channel = || {
            let (tx, rx) = mpsc::channel();
            receivers.push(rx);
            tx
        };
        let running = state.running.map(|running| match running {
            Running::Client => Route::Client(channel()),
            Running::Incoming => Route::Incoming,
        });
        let queued = state.queued.into_iter().map(|id| (id, channel())).collect();
        let mut receivers = receivers.into_iter();
        let running_events = matches!(running, Some(Route::Client(_)))
            .then(|| receivers.next())
            .flatten();
        let following: HashMap<String, (String, Arc<AtomicBool>)> = state
            .following
            .into_iter()
            .map(|(task, session)| (task, (session, Arc::new(AtomicBool::new(false)))))
            .collect();
        let shared = Arc::new(Shared {
            pid: state.pid,
            stdin: Mutex::new(Some(stdin)),
            fds: state.fds,
            bypass: state.config.profile == Profile::FullAuto,
            turns: Mutex::new(Turns {
                queued,
                running,
                exited: false,
                reports_ids: state.reports_ids,
            }),
            exited: Condvar::new(),
            incoming,
            last_activity: Mutex::new(Instant::now()),
            approvals: Mutex::new(state.approvals),
            always: Mutex::new(state.always),
            shown: Mutex::new(state.shown),
            background: Mutex::new(state.background),
            reading: Mutex::new(Reading {
                unread: BASE64_STANDARD.decode(&state.unread).unwrap_or_default(),
                notices: state.notices,
                last_request: state.last_request,
                following: HashMap::new(),
                closed: false,
            }),
        });
        for (task, (session, stop)) in following {
            shared.follow(&task, &session, &stop);
            shared
                .reading
                .lock()
                .unwrap()
                .following
                .insert(task, (session, stop));
        }

        // Drained continuously: a long-lived process would block once the pipe filled.
        let stderr = thread::spawn(move || stderr_tail(stderr));
        let reader = shared.clone();
        thread::spawn(move || {
            reader.read(stdout);
            reader.ended(stderr.join().unwrap_or_default());
        });
        let session = Self {
            config: state.config,
            shared,
        };
        (session, running_events, receivers.collect())
    }

    /// Holds the reader still and returns the session as a handover passes it on, or
    /// `None` once its process is done.
    pub(crate) fn pause(&self) -> Option<(Paused<'_>, SessionState)> {
        let reading = self.shared.reading.lock().unwrap();
        if reading.closed {
            return None;
        }
        let turns = self.shared.turns.lock().unwrap();
        let state = SessionState {
            config: self.config.clone(),
            pid: self.shared.pid,
            fds: self.shared.fds,
            unread: BASE64_STANDARD.encode(&reading.unread),
            queued: turns.queued.iter().map(|(id, _)| id.clone()).collect(),
            running: turns.running.as_ref().map(|route| match route {
                Route::Client(_) => Running::Client,
                Route::Incoming => Running::Incoming,
            }),
            reports_ids: turns.reports_ids,
            approvals: self.shared.approvals.lock().unwrap().clone(),
            always: self.shared.always.lock().unwrap().clone(),
            shown: self.shared.shown.lock().unwrap().clone(),
            background: self.shared.background.lock().unwrap().clone(),
            notices: reading.notices.clone(),
            last_request: reading.last_request,
            following: reading
                .following
                .iter()
                .map(|(task, (session, _))| (task.clone(), session.clone()))
                .collect(),
        };
        drop(turns);
        Some((Paused { _reading: reading }, state))
    }

    /// Start a turn, or queue it behind the running one. Its events arrive on the
    /// returned receiver, which closes after `Completed` or `Error`. Fails if the
    /// process has exited.
    pub fn send(&mut self, message: UserMessage) -> io::Result<Receiver<EngineEvent>> {
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

        let text =
            (!message.text.is_empty()).then(|| json!({ "type": "text", "text": message.text }));
        let content: Vec<Value> = text
            .into_iter()
            .chain(message.attachments.iter().map(attachment_block))
            .collect();
        let message = json!({
            "type": "user",
            "uuid": id,
            "message": { "role": "user", "content": content },
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

    /// Make the session reachable from the Claude app and claude.ai/code under `name`
    /// through Remote Control, or no longer.
    pub fn remote_control(&self, enabled: bool, name: &str) -> io::Result<()> {
        self.shared.write(&json!({
            "type": "control_request",
            "request_id": Uuid::new_v4().to_string(),
            "request": { "subtype": "remote_control", "enabled": enabled, "name": name },
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
        let Pending { input, scope, .. } = self
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
        self.shared.pid
    }

    pub fn is_alive(&self) -> bool {
        !self.shared.turns.lock().unwrap().exited
    }

    /// A call runs in the background, such as a listener waiting for messages.
    pub fn has_background(&self) -> bool {
        !self.shared.background.lock().unwrap().is_empty()
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

    /// Whether a call is new to the running turn, marking it shown.
    fn first_shown(&self, call_id: &str) -> bool {
        self.shown.lock().unwrap().insert(call_id.to_string())
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
            // Between turns, only work still going in the background reports: a command
            // ending, or a subagent's calls.
            None => {
                let background = match &ev {
                    EngineEvent::Background { .. } | EngineEvent::ToolResult { .. } => true,
                    EngineEvent::ToolStarted(call) => call.parent.is_some(),
                    _ => false,
                };
                if let (true, Some(incoming)) = (background, &self.incoming) {
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

    fn follow(self: &Arc<Self>, task: &str, session: &str, stop: &Arc<AtomicBool>) {
        let (shared, task, session, stop) = (
            self.clone(),
            task.to_string(),
            session.to_string(),
            stop.clone(),
        );
        thread::spawn(move || follow_subagent(&shared, &session, &task, &stop));
    }

    /// Handles each line of the process's output as it arrives, until the output ends.
    fn read(self: &Arc<Self>, mut stdout: File) {
        let mut chunk = vec![0; 64 * 1024];
        while readable(stdout.as_raw_fd()) {
            let mut reading = self.reading.lock().unwrap();
            let n = match stdout.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            reading.unread.extend_from_slice(&chunk[..n]);
            while let Some(end) = reading.unread.iter().position(|&byte| byte == b'\n') {
                let line: Vec<u8> = reading.unread.drain(..=end).collect();
                self.touch();
                if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                    self.handle(&mut reading, &v);
                }
            }
        }
        self.reading.lock().unwrap().closed = true;
    }

    fn handle(self: &Arc<Self>, reading: &mut Reading, v: &Value) {
        if let Some((task, session)) = forked_task(v) {
            let stop = Arc::new(AtomicBool::new(false));
            self.follow(&task, &session, &stop);
            reading.following.insert(task, (session, stop));
        }
        if let Some((_, stop)) = ended_task(v).and_then(|task| reading.following.remove(task)) {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(context) = request_context(v) {
            reading.last_request = context;
        }
        if let Some(message) = started_message(v) {
            let mut turns = self.turns.lock().unwrap();
            turns.reports_ids = true;
            if let Some(joined) = turns.begin(message) {
                self.route(&turns, EngineEvent::Delivered { joined });
            }
            reading.notices.clear();
            return;
        }
        reading.notices.extend(ended_notice(v));
        if v.get("type").and_then(Value::as_str) == Some("control_cancel_request") {
            if let Some(id) = v.get("request_id").and_then(Value::as_str) {
                self.approvals.lock().unwrap().remove(id);
            }
            return;
        }
        let events = match permission_prompt(v, self.bypass) {
            Some(Prompt::Ask {
                request_id,
                call,
                input,
                scope,
            }) => {
                if self.always.lock().unwrap().contains(&scope) {
                    let _ = self.write(&allow(&request_id, input));
                    Vec::new()
                } else {
                    self.approvals.lock().unwrap().insert(
                        request_id.clone(),
                        Pending {
                            input,
                            scope: scope.clone(),
                            call: (*call).clone(),
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
                let _ = self.write(&deny(&request_id, &message));
                Vec::new()
            }
            None => decode(v),
        };
        for mut ev in events {
            if let EngineEvent::Background {
                call_id,
                background,
            } = &ev
            {
                let mut running = self.background.lock().unwrap();
                match background.status {
                    TaskStatus::Running => running.insert(call_id.clone()),
                    _ => running.remove(call_id),
                };
            }
            if let EngineEvent::ToolStarted(call) = &ev
                && !self.first_shown(&call.call_id)
            {
                continue;
            }
            if let EngineEvent::Completed { context, .. } = &mut ev {
                context.used = std::mem::take(&mut reading.last_request);
                self.shown.lock().unwrap().clear();
            }
            let completed = matches!(ev, EngineEvent::Completed { .. });
            let mut turns = self.turns.lock().unwrap();
            let incoming = matches!(ev, EngineEvent::Incoming { .. } | EngineEvent::Typed { .. });
            if turns.running.is_none() && (incoming || matches!(ev, EngineEvent::Started { .. })) {
                // A CLI that reports no message ids: turns run in the order sent.
                let next = turns
                    .queued
                    .front()
                    .map(|(id, _)| id.clone())
                    .filter(|_| !turns.reports_ids && !incoming);
                turns.start(next.as_deref());
                if !incoming
                    && matches!(turns.running, Some(Route::Incoming))
                    && !reading.notices.is_empty()
                {
                    let notice = EngineEvent::Notice {
                        text: reading.notices.join("\n"),
                    };
                    self.route(&turns, notice);
                }
                reading.notices.clear();
            }
            self.route(&turns, ev);
            if completed {
                turns.running = None;
            }
        }
    }

    /// Reaps the process once its output has ended, and fails whatever was waiting on it.
    fn ended(&self, stderr: String) {
        for (_, stop) in self.reading.lock().unwrap().following.values() {
            stop.store(true, Ordering::Relaxed);
        }
        let status = wait(self.pid);
        let mut turns = self.turns.lock().unwrap();
        turns.exited = true;
        self.exited.notify_all();
        let detail = if stderr.trim().is_empty() {
            match status {
                Ok(s) => format!("claude exited without completing the turn ({s})"),
                Err(e) => format!("waiting on claude failed: {e}"),
            }
        } else {
            stderr.trim().to_string()
        };
        if turns.running.is_some() {
            self.route(
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
    }
}

/// Waits until `fd` has something to read, or has closed.
fn readable(fd: RawFd) -> bool {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: one valid `pollfd`, for the duration of the call.
        match unsafe { libc::poll(&mut poll, 1, -1) } {
            1 => return true,
            -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
            _ => return false,
        }
    }
}

/// Reaps the process by pid, which `Child::wait` cannot do for one adopted from a handover.
pub(crate) fn wait(pid: u32) -> io::Result<ExitStatus> {
    let mut status = 0;
    loop {
        // SAFETY: `status` outlives the call.
        if unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) } != -1 {
            return Ok(ExitStatus::from_raw(status));
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

pub(crate) fn set_cloexec(fd: RawFd, cloexec: bool) {
    let flag = if cloexec { libc::FD_CLOEXEC } else { 0 };
    // SAFETY: changes only the descriptor's flags; a closed one makes it fail harmlessly.
    unsafe { libc::fcntl(fd, libc::F_SETFD, flag) };
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
        parent: None,
        agent: false,
        started: None,
        ended: None,
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

/// The Agent call whose subagent sent a message. A forked command's subagent has none, its
/// command being no call.
fn subagent_of(v: &Value) -> Option<String> {
    v.get("parent_tool_use_id")?
        .as_str()
        .filter(|parent| !parent.starts_with("forked-command-"))
        .map(str::to_owned)
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
    // The interactive CLI marks what the user typed as `human`.
    if matches!(field("kind"), Some("task-notification" | "human")) {
        return None;
    }
    let from = field("name").or(field("kind")).unwrap_or("another session");
    Some((from.to_string(), message_text(v)))
}

/// The user's own message, typed in the Claude app through Remote Control or in a terminal,
/// which the CLI marks `human`; the client's own carry no origin.
fn typed(v: &Value) -> Option<String> {
    (v.pointer("/origin/kind")?.as_str()? == "human").then(|| message_text(v))
}

/// What a user message says: another agent's `body`, else its text.
fn message_text(v: &Value) -> String {
    match (
        v.pointer("/origin/body").and_then(Value::as_str),
        v.pointer("/message/content"),
    ) {
        (Some(body), _) => body.to_string(),
        (None, Some(Value::String(text))) => text.clone(),
        (None, _) => content_blocks(v)
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
    }
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

fn stderr_tail(stderr: File) -> String {
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
            .map(|b| {
                let mut call = tool_call(b);
                call.parent = subagent_of(v);
                EngineEvent::ToolStarted(call)
            })
            .collect(),

        // A message starting its turn, echoed by `--replay-user-messages`.
        // The summary a compaction leaves in place of the conversation.
        "user" if v.get("isSynthetic").and_then(Value::as_bool) == Some(true) => v
            .pointer("/message/content")
            .and_then(Value::as_str)
            .map(|text| EngineEvent::CompactSummary(text.to_string()))
            .into_iter()
            .collect(),

        "user" if v.get("isReplay").and_then(Value::as_bool) == Some(true) => match typed(v) {
            Some(text) => vec![EngineEvent::Typed { text }],
            None => incoming(v)
                .map(|(from, text)| EngineEvent::Incoming { from, text })
                .into_iter()
                .collect(),
        },

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

/// An attachment as the API takes it: images and PDFs as they are, anything else as text.
fn attachment_block(attachment: &Attachment) -> Value {
    let Attachment {
        name,
        media_type,
        data,
    } = attachment;
    if attachment.is_image() {
        json!({ "type": "image", "source": { "type": "base64", "media_type": media_type, "data": data } })
    } else if media_type == "application/pdf" {
        json!({
            "type": "document",
            "title": name,
            "source": { "type": "base64", "media_type": media_type, "data": data },
        })
    } else {
        let text = attachment
            .bytes()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        json!({
            "type": "document",
            "title": name,
            "source": { "type": "text", "media_type": "text/plain", "data": text },
        })
    }
}

/// An image or document block of a transcript's user message, back to what was attached.
pub(crate) fn attachment(block: &Value) -> Option<Attachment> {
    let source = block.get("source")?;
    fn field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
        v.get(key).and_then(Value::as_str)
    }
    let media_type = field(source, "media_type").unwrap_or("text/plain");
    let name = match field(block, "type")? {
        "image" => "image",
        "document" => field(block, "title").unwrap_or("document"),
        _ => return None,
    };
    let data = match field(source, "type")? {
        "base64" => field(source, "data")?.to_string(),
        "text" => BASE64_STANDARD.encode(field(source, "data")?),
        _ => return None,
    };
    Some(Attachment {
        name: name.to_string(),
        media_type: media_type.to_string(),
        data,
    })
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

/// A code review's findings, from the list the CLI writes `/code-review`'s JSON as:
/// `13 findings, most severe first:`, then items like ``1. `src/a.rs:12`: summary``, each
/// with `Failure scenario: ...` indented under it. `None` for text with no such list.
pub(crate) fn review(text: &str) -> Option<Review> {
    let mut lines = text.split_inclusive('\n');
    let mut start = 0;
    let (count, mut end) = loop {
        let line = lines.next()?;
        if let Some(count) = findings_header(line.trim_end()) {
            break (count, start + line.len());
        }
        start += line.len();
    };

    let mut items: Vec<(usize, Vec<&str>)> = Vec::new();
    for line in lines {
        let bare = line.trim_end_matches(['\n', '\r']);
        let number = format!("{}. ", items.len() + 1);
        if let Some(first) = bare.strip_prefix(&number) {
            items.push((number.len(), vec![first]));
        } else if let Some((indent, body)) = items.last_mut()
            && (bare.is_empty() || bare.starts_with(&" ".repeat(*indent)))
        {
            body.push(bare.get(*indent..).unwrap_or(""));
        } else if !(bare.is_empty() && items.is_empty()) {
            break;
        }
        end += line.len();
    }
    if items.len() != count {
        return None;
    }
    let findings = items
        .iter()
        .map(|(_, body)| finding(body.join("\n").trim()))
        .collect::<Option<Vec<_>>>()?;
    Some(Review {
        preface: text[..start].trim().to_string(),
        findings,
        afterword: text[end..].trim().to_string(),
    })
}

/// How many findings a line such as `13 findings, most severe first:` announces.
fn findings_header(line: &str) -> Option<usize> {
    let (count, rest) = line.split_once(' ')?;
    let count = count.parse().ok()?;
    let rest = rest.strip_prefix(if count == 1 { "finding" } else { "findings" })?;
    matches!(rest, ":" | ", most severe first:").then_some(count)
}

fn finding(item: &str) -> Option<Finding> {
    let (place, rest) = item.strip_prefix('`')?.split_once("`: ")?;
    let (summary, failure_scenario) = match rest.split_once("\n\nFailure scenario: ") {
        Some((summary, scenario)) => (summary, Some(scenario.to_string())),
        None => (rest, None),
    };
    let (file, line) = match place.rsplit_once(':') {
        Some((file, line)) if line.starts_with(|c: char| c.is_ascii_digit()) => {
            (file, Some(line.to_string()))
        }
        _ => (place, None),
    };
    Some(Finding {
        file: file.to_string(),
        line,
        summary: summary.to_string(),
        failure_scenario,
    })
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

/// A forked slash command's subagent, by task and session id: the CLI sends its calls only
/// once it ends, so they are followed from its transcript until then.
fn forked_task(v: &Value) -> Option<(String, String)> {
    let field = |key| v.get(key).and_then(Value::as_str);
    let foreground = v.get("is_backgrounded").and_then(Value::as_bool) != Some(true);
    // An Agent call's subagent names the call; a forked command's has none.
    let command = field("tool_use_id").is_none();
    (field("subtype")? == "task_started"
        && field("task_type")? == "local_agent"
        && foreground
        && command)
        .then(|| {
            Some((
                field("task_id")?.to_string(),
                field("session_id")?.to_string(),
            ))
        })
        .flatten()
}

fn ended_task(v: &Value) -> Option<&str> {
    (v.get("subtype").and_then(Value::as_str)? == "task_notification")
        .then(|| v.get("task_id").and_then(Value::as_str))
        .flatten()
}

const FOLLOW_TICK: Duration = Duration::from_millis(250);

/// Sends a subagent's calls to the running turn as its transcript records them, until `stop`.
fn follow_subagent(shared: &Shared, session: &str, task: &str, stop: &AtomicBool) {
    let name = format!("{session}/subagents/agent-{task}.jsonl");
    let mut transcript: Option<BufReader<File>> = None;
    let mut line = String::new();
    loop {
        let stopping = stop.load(Ordering::Relaxed);
        if transcript.is_none() {
            transcript = subagent_transcript(&name)
                .and_then(|path| File::open(path).ok())
                .map(BufReader::new);
        }
        // A line still being written stays in `line` until its newline arrives.
        while let Some(reader) = &mut transcript
            && matches!(reader.read_line(&mut line), Ok(n) if n > 0)
            && line.ends_with('\n')
        {
            let events = serde_json::from_str::<Value>(&line)
                .map(|v| decode(&v))
                .unwrap_or_default();
            line.clear();
            for ev in events {
                let shown = match &ev {
                    EngineEvent::ToolStarted(call) => shared.first_shown(&call.call_id),
                    EngineEvent::ToolResult { .. } => true,
                    _ => false,
                };
                if shown {
                    let turns = shared.turns.lock().unwrap();
                    shared.route(&turns, ev);
                }
            }
        }
        if stopping {
            return;
        }
        thread::sleep(FOLLOW_TICK);
    }
}

fn subagent_transcript(name: &str) -> Option<PathBuf> {
    let projects = PathBuf::from(std::env::var_os("HOME")?).join(".claude/projects");
    std::fs::read_dir(projects)
        .ok()?
        .flatten()
        .map(|project| project.path().join(name))
        .find(|path| path.is_file())
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
    let name = field("name").unwrap_or("tool");
    ToolCall {
        call_id: field("id").unwrap_or("").to_string(),
        name: name.to_string(),
        // `Task` is the Agent tool's earlier name, still in older transcripts.
        agent: matches!(name, "Agent" | "Task"),
        summary: input.map(summarize).unwrap_or_default(),
        description: input_field("description").unwrap_or("").to_string(),
        command: input_field("command").map(str::to_owned),
        input: input
            .map(|i| serde_json::to_string_pretty(i).unwrap_or_default())
            .unwrap_or_default(),
        result: None,
        is_error: false,
        background: None,
        parent: None,
        started: None,
        ended: None,
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

    /// The CLI's own rendering of a review's JSON, as its bundle writes it.
    fn findings_list(findings: &[(&str, Option<&str>, &str, Option<&str>)]) -> String {
        let items: Vec<String> = findings
            .iter()
            .enumerate()
            .map(|(i, (file, line, summary, scenario))| {
                let place = line.map_or(file.to_string(), |line| format!("{file}:{line}"));
                let scenario =
                    scenario.map_or(String::new(), |s| format!("\n\nFailure scenario: {s}"));
                let number = format!("{}. ", i + 1);
                let indent = format!("\n{}", " ".repeat(number.len()));
                let item = format!("{number}`{place}`: {summary}{scenario}");
                let mut indented = String::new();
                let mut rest = item.as_str();
                while let Some(at) = rest.find('\n') {
                    indented.push_str(&rest[..at]);
                    let next = &rest[at + 1..];
                    let blank = next.is_empty() || next.starts_with('\n');
                    indented.push_str(if blank { "\n" } else { &indent });
                    rest = next;
                }
                indented + rest
            })
            .collect();
        let n = findings.len();
        let header = match n {
            1 => "1 finding:".to_string(),
            n => format!("{n} findings, most severe first:"),
        };
        format!("{header}\n\n{}", items.join("\n\n"))
    }

    #[test]
    fn a_reviews_findings_come_back_from_the_list_the_cli_writes() {
        let list = findings_list(&[
            (
                "crates/runtime/src/tune/tuner.rs",
                Some("480"),
                "A lost device tunes `k`\nagain on every launch.",
                Some("One GPU:\n\nevery key tunes again."),
            ),
            ("README.md", None, "A stale line.", None),
        ]);
        let text = format!("I reviewed PR #1848.\n\n{list}\n\nOne more thing outside it.");
        let review = review(&text).expect("a review");
        assert_eq!(review.preface, "I reviewed PR #1848.");
        assert_eq!(review.afterword, "One more thing outside it.");
        assert_eq!(
            review.findings,
            [
                Finding {
                    file: "crates/runtime/src/tune/tuner.rs".to_string(),
                    line: Some("480".to_string()),
                    summary: "A lost device tunes `k`\nagain on every launch.".to_string(),
                    failure_scenario: Some("One GPU:\n\nevery key tunes again.".to_string()),
                },
                Finding {
                    file: "README.md".to_string(),
                    line: None,
                    summary: "A stale line.".to_string(),
                    failure_scenario: None,
                },
            ]
        );

        let many: Vec<_> = (1..=12)
            .map(|_| ("a.rs", Some("1-3"), "Wrong.\nTwice.", Some("Always.")))
            .collect();
        let review = super::review(&findings_list(&many)).expect("twelve findings");
        assert_eq!(review.findings.len(), 12);
        assert_eq!(review.findings[11].summary, "Wrong.\nTwice.");
        assert_eq!(review.findings[11].line.as_deref(), Some("1-3"));

        let one = findings_list(&[("a.rs", Some("2"), "Off by one.", None)]);
        assert_eq!(super::review(&one).map(|r| r.findings.len()), Some(1));
        assert!(super::review("No findings.").is_none());
        assert!(super::review("14 findings").is_none());
        assert!(
            super::review("3 findings, most severe first:\n\n1. `a.rs:1`: Only one.").is_none()
        );
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
    fn attachments_go_out_as_the_api_takes_them_and_come_back_from_transcripts() {
        let image = Attachment::new("shot.png".to_string(), "image/png", &[1, 2, 3]);
        let notes = Attachment::new("notes.txt".to_string(), "text/plain", b"hello");
        let block = attachment_block(&image);
        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["data"], image.data);
        let block = attachment_block(&notes);
        assert_eq!(block["source"]["data"], "hello", "text goes as text");
        let back = attachment(&block).unwrap();
        assert_eq!(
            (back.name.as_str(), back.bytes()),
            ("notes.txt", Some(b"hello".to_vec()))
        );
        assert_eq!(
            attachment(&attachment_block(&image)).unwrap().media_type,
            "image/png"
        );
    }

    #[test]
    fn a_forked_commands_subagent_is_followed_and_an_agent_calls_is_not() {
        let forked = line(
            r#"{"type":"system","subtype":"task_started","task_id":"a86","session_id":"s1","task_type":"local_agent","is_backgrounded":false}"#,
        );
        assert_eq!(
            forked_task(&forked),
            Some(("a86".to_string(), "s1".to_string()))
        );
        let agent = line(
            r#"{"type":"system","subtype":"task_started","task_id":"a87","session_id":"s1","task_type":"local_agent","is_backgrounded":false,"tool_use_id":"t9"}"#,
        );
        assert_eq!(forked_task(&agent), None);
        let ended = line(r#"{"type":"system","subtype":"task_notification","task_id":"a86"}"#);
        assert_eq!(ended_task(&ended), Some("a86"));
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
        assert_eq!(
            unix_millis("2026-10-17T22:38:52.922Z"),
            Some(1_792_276_732_922)
        );
        assert_eq!(unix_millis("2026-10-15T21:30:00Z"), Some(1_792_099_800_000));
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

        let typed = line(
            r#"{"type":"user","origin":{"kind":"human"},"message":{"role":"user","content":"hi"}}"#,
        );
        assert!(
            incoming(&typed).is_none(),
            "the user's own, typed in a terminal"
        );
        let from_the_app = line(
            r#"{"type":"user","isReplay":true,"uuid":"2222","origin":{"kind":"human"},"message":{"role":"user","content":"hi from my phone"}}"#,
        );
        assert!(matches!(
            &decode(&from_the_app)[..],
            [EngineEvent::Typed { text }] if text == "hi from my phone"
        ));
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

    #[test]
    fn a_subagents_calls_name_the_call_that_started_it() {
        let parent = |parent: &str| {
            let v = line(&format!(
                r#"{{"type":"assistant","parent_tool_use_id":{parent},"message":{{"content":[{{"type":"tool_use","id":"t","name":"Bash","input":{{}}}}]}}}}"#
            ));
            match decode(&v).as_slice() {
                [EngineEvent::ToolStarted(call)] => call.parent.clone(),
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(parent(r#""toolu_a""#).as_deref(), Some("toolu_a"));
        assert_eq!(parent("null"), None);
        assert_eq!(parent(r#""forked-command-code-review""#), None);
    }

    #[test]
    fn a_message_started_mid_turn_joins_it() {
        let mut turns = Turns::default();
        let (first, first_events) = mpsc::channel();
        let (second, second_events) = mpsc::channel();
        turns.queued.push_back(("a".to_string(), first));
        turns.queued.push_back(("b".to_string(), second));

        assert_eq!(turns.begin(Some("a")), Some(false));
        assert_eq!(turns.begin(Some("peer")), None);
        assert_eq!(turns.begin(Some("b")), Some(true));
        assert!(turns.queued.is_empty());
        let Some(Route::Client(running)) = &turns.running else {
            panic!("the first message's turn goes on");
        };
        running.send(EngineEvent::Compacting).unwrap();
        assert!(first_events.try_recv().is_ok());
        assert!(
            second_events.recv().is_err(),
            "the joined message has no turn"
        );
    }
}
