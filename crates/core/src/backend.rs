//! The transport-agnostic backend: it owns the conversation store, drives the
//! engine, and emits `ServerEvent`s. A transport (in-process channel, or the
//! daemon's WebSocket server) feeds it `Command`s and forwards its events.
//!
//! Events go out on a tokio unbounded channel: the sender is `Send + Sync` and
//! usable from the worker threads forwarding each turn, while a consumer can either
//! `try_recv` (native, no runtime) or `recv().await` (the daemon).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use claudo_protocol::{
    Background, Command, ContextUsage, ConversationInfo, Decision, Item, ModelOption, ServerEvent,
    TaskStatus, ToolCall, TurnTime,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::engine::{EngineEvent, Resume, SessionConfig, UserMessage};
use crate::handover::{Handed, Handover, Progress, Waited};
use crate::live::{AgentLock, IDLE_TIMEOUT, LiveSessions, SendError};
use crate::sessions::{Store, load_transcript};

pub struct Backend {
    store: Arc<Mutex<Store>>,
    live: Arc<LiveSessions>,
    events: UnboundedSender<ServerEvent>,
    /// Each conversation's running turn, for a handover to pass on.
    progress: Running,
    /// Messages to conversations whose agent is a process this app does not run, oldest
    /// first, until it ends.
    waiting: Waiting,
}

type Running = Arc<Mutex<HashMap<String, Progress>>>;
type Waiting = Arc<Mutex<HashMap<String, VecDeque<Outgoing>>>>;

/// A message on its way to a conversation's agent.
#[derive(Clone, Serialize, Deserialize)]
pub struct Outgoing {
    text: String,
    profile: claudo_protocol::Profile,
    model: String,
    effort: claudo_protocol::Effort,
    attachments: Vec<claudo_protocol::Attachment>,
}

/// What sends messages to agents and passes their turns on, from the backend or from a
/// thread waiting for a conversation to be free.
#[derive(Clone)]
struct Agents {
    store: Arc<Mutex<Store>>,
    live: Arc<LiveSessions>,
    events: UnboundedSender<ServerEvent>,
    progress: Running,
    waiting: Waiting,
}

impl Backend {
    pub fn new(events: UnboundedSender<ServerEvent>) -> Self {
        let store = Arc::new(Mutex::new(Store::load()));
        let handover = crate::handover::take();
        let progress: Running = Arc::new(Mutex::new(
            handover
                .iter()
                .flat_map(|handover| &handover.sessions)
                .map(|handed| (handed.conversation.clone(), handed.progress))
                .collect(),
        ));
        let (incoming, turns) = std::sync::mpsc::channel();
        let (forwarded, kept, running) = (events.clone(), store.clone(), progress.clone());
        std::thread::spawn(move || {
            let mut forwards: HashMap<String, Forward> = HashMap::new();
            for (id, ev) in turns {
                forwards
                    .entry(id)
                    .or_insert_with_key(|id| {
                        Forward::resumed(forwarded.clone(), kept.clone(), id.clone(), &running)
                    })
                    .event(ev);
            }
        });
        let backend = Self {
            store,
            live: Arc::new(LiveSessions::new(IDLE_TIMEOUT, Some(incoming))),
            events,
            progress,
            waiting: Arc::default(),
        };
        let settings = crate::settings::load();
        crate::debug::set(settings.debug_log);
        if settings.remote_control {
            backend.live.set_remote_control(true, |_| String::new());
        }
        backend.live.set_suggestions(settings.prompt_suggestions);
        if let Some(handover) = handover {
            backend.adopt(handover);
        }
        backend
    }

    /// Take over the agents of the app this one replaced, where each one was.
    fn adopt(&self, handover: Handover) {
        for Handed {
            conversation,
            state,
            ..
        } in handover.sessions
        {
            let working = state.running().is_some();
            let background: Vec<String> = state.in_background().iter().cloned().collect();
            let adopted = self.live.adopt(&conversation, state);
            if working {
                self.emit(ServerEvent::Working {
                    conversation_id: conversation.clone(),
                });
            }
            for call_id in background {
                self.emit(ServerEvent::Background {
                    conversation_id: conversation.clone(),
                    call_id,
                    background: Background {
                        status: TaskStatus::Running,
                        output: None,
                    },
                    at: None,
                });
            }
            for (request_id, call, scope) in adopted.approvals {
                self.emit(ServerEvent::ApprovalRequested {
                    conversation_id: conversation.clone(),
                    request_id,
                    call,
                    scope,
                });
            }
            let agents = self.agents();
            if let Some(turn) = adopted.running {
                agents.forward(
                    turn,
                    Forward::resumed(
                        self.events.clone(),
                        self.store.clone(),
                        conversation.clone(),
                        &self.progress,
                    ),
                );
            }
            for (sent, turn) in adopted.queued {
                self.emit(ServerEvent::Queued {
                    conversation_id: conversation.clone(),
                    text: sent.text,
                    attachments: sent.attachments,
                });
                let latest = agents.session_id(&conversation);
                agents.forward(turn, agents.fresh(conversation.clone(), latest));
            }
        }
        crate::handover::reap(handover.reap);
        for Waited {
            conversation,
            messages,
        } in handover.waiting
        {
            for message in &messages {
                self.emit(ServerEvent::Queued {
                    conversation_id: conversation.clone(),
                    text: message.text.clone(),
                    attachments: message.attachments.clone(),
                });
            }
            self.waiting
                .lock()
                .unwrap()
                .insert(conversation.clone(), messages.into());
            self.agents().wait(conversation);
        }
    }

    /// Replace this app with the build installed where it was started from, handing it
    /// every agent.
    fn restart(&self) {
        let Some(exe) = crate::handover::installed() else {
            return self.emit(ServerEvent::Error {
                conversation_id: None,
                detail: "Could not find where claudo is installed".to_string(),
            });
        };
        let progress = self.progress.lock().unwrap().clone();
        // Held until the exec, so no message leaves the queue for an agent meanwhile.
        let waiting = self.waiting.lock().unwrap();
        let waited = waiting
            .iter()
            .map(|(conversation, messages)| Waited {
                conversation: conversation.clone(),
                messages: messages.iter().cloned().collect(),
            })
            .collect();
        let error = self.live.exec(
            &exe,
            |id| progress.get(id).copied().unwrap_or_default(),
            waited,
        );
        drop(waiting);
        self.emit(ServerEvent::Error {
            conversation_id: None,
            detail: format!("Could not restart claudo: {error}"),
        });
    }

    fn agents(&self) -> Agents {
        Agents {
            store: self.store.clone(),
            live: self.live.clone(),
            events: self.events.clone(),
            progress: self.progress.clone(),
            waiting: self.waiting.clone(),
        }
    }

    fn emit(&self, event: ServerEvent) {
        let _ = self.events.send(event);
    }

    /// Handle one command. Returns quickly; `SendMessage` hands the turn to the
    /// conversation's `claude` process and streams its events from a worker thread.
    pub fn handle(&self, cmd: Command) {
        match cmd {
            Command::GetStatus => self.emit(ServerEvent::Status(crate::detect_system())),

            Command::GetSettings => self.emit(ServerEvent::Settings(crate::settings::load())),

            Command::GetCapabilities => {
                let events = self.events.clone();
                std::thread::spawn(move || {
                    let (models, commands) =
                        crate::engine::cli::capabilities().unwrap_or_else(|_| {
                            let models = ["Opus", "Fable", "Sonnet", "Haiku"]
                                .map(|label| ModelOption {
                                    value: label.to_lowercase(),
                                    label: label.to_string(),
                                })
                                .to_vec();
                            (models, Vec::new())
                        });
                    let _ = events.send(ServerEvent::Models(models));
                    let _ = events.send(ServerEvent::Commands(commands));
                });
            }

            Command::GetUsage => {
                let events = self.events.clone();
                std::thread::spawn(move || {
                    let _ = events.send(match crate::engine::cli::usage() {
                        Ok(quota) => ServerEvent::Quota(quota),
                        Err(error) => ServerEvent::QuotaUnavailable(error.to_string()),
                    });
                });
            }

            Command::UpdateSettings { settings } => {
                if settings.remote_control != crate::settings::load().remote_control {
                    let store = self.store.lock().unwrap();
                    self.live.set_remote_control(settings.remote_control, |id| {
                        store.get(id).map(|c| c.title.clone()).unwrap_or_default()
                    });
                }
                crate::debug::set(settings.debug_log);
                self.live.set_suggestions(settings.prompt_suggestions);
                crate::settings::save(&settings);
                self.emit(ServerEvent::Settings(settings));
            }

            Command::ListConversations => {
                let event = conversations_event(&self.store);
                self.emit(event);
            }

            Command::NewScratch { request } => {
                let id = self.store.lock().unwrap().create_scratch();
                self.created(id, request);
            }

            Command::NewAttached { path, request } => {
                let id = self.store.lock().unwrap().create_attached(path.into());
                self.created(id, request);
            }

            Command::SetFolder { id, path } => {
                // Its first turn may still be waiting for the session id.
                let moved = if self.live.pid(&id).is_some() {
                    Err("a conversation stays in the folder it started in".to_string())
                } else {
                    let folder = path.map(PathBuf::from);
                    self.store.lock().unwrap().set_folder(&id, folder)
                };
                match moved {
                    Ok(()) => self.emit(conversations_event(&self.store)),
                    Err(detail) => self.error(id, detail),
                }
            }

            Command::ListFolders => {
                let known = self.store.lock().unwrap().folders();
                let events = self.events.clone();
                std::thread::spawn(move || {
                    let list = crate::sessions::recent_folders(known);
                    let _ = events.send(ServerEvent::Folders { list });
                });
            }

            Command::ListSessions => {
                let known = self.store.lock().unwrap().session_ids();
                let events = self.events.clone();
                std::thread::spawn(move || {
                    let list = crate::sessions::discover(&known);
                    let _ = events.send(ServerEvent::Sessions { list });
                });
            }

            Command::Import {
                session_id,
                request,
            } => {
                let imported = self.store.lock().unwrap().import(&session_id);
                match imported {
                    Ok(id) => {
                        self.emit(conversations_event(&self.store));
                        self.emit(ServerEvent::Created {
                            conversation_id: id,
                            request,
                            imported: true,
                        });
                    }
                    Err(detail) => self.emit(ServerEvent::Error {
                        conversation_id: None,
                        detail,
                    }),
                }
            }

            Command::OpenConversation { id } => self.open(id),

            Command::Delete { id } => {
                let session = self.live.remove(&id);
                let (events, store) = (self.events.clone(), self.store.clone());
                std::thread::spawn(move || {
                    if let Some(session) = session {
                        session.shutdown(std::time::Duration::from_secs(10));
                    }
                    let removed = store.lock().unwrap().remove(&id);
                    if let Some(meta) = removed {
                        crate::sessions::delete_files(&meta);
                    }
                    let _ = events.send(conversations_event(&store));
                });
            }

            Command::Archive { id, archived } => {
                self.store.lock().unwrap().set_archived(&id, archived);
                self.emit(conversations_event(&self.store));
            }

            Command::Rename { id, title } => {
                let title = title.trim().to_string();
                if !title.is_empty() {
                    self.store.lock().unwrap().rename(&id, title.clone());
                    self.live.rename(&id, &title);
                }
                self.emit(conversations_event(&self.store));
            }

            Command::SetOverrides { id, overrides } => {
                self.store.lock().unwrap().set_overrides(&id, overrides);
                self.emit(conversations_event(&self.store));
            }

            Command::SendMessage {
                conversation_id,
                text,
                profile,
                model,
                effort,
                attachments,
            } => self.agents().send(
                conversation_id,
                Outgoing {
                    text,
                    profile,
                    model,
                    effort,
                    attachments,
                },
            ),

            Command::Stop { conversation_id } => {
                if let Err(detail) = self.live.interrupt(&conversation_id) {
                    self.error(conversation_id, detail);
                }
            }

            Command::Restart => self.restart(),

            Command::OnScreen { conversation_ids } => self.live.set_on_screen(conversation_ids),

            Command::StopStray { conversation_id } => {
                if let Err(e) = crate::live::stop_stray(&conversation_id) {
                    self.error(conversation_id, format!("Could not stop that agent: {e}"));
                }
            }

            Command::AnswerApproval {
                conversation_id,
                request_id,
                decision,
            } => match self.live.answer(&conversation_id, &request_id, decision) {
                Ok(scope) if decision == Decision::AllowAlways => self
                    .store
                    .lock()
                    .unwrap()
                    .allow_always(&conversation_id, scope),
                Ok(_) => {}
                Err(detail) => self.error(conversation_id, detail),
            },
        }
    }

    fn created(&self, id: String, request: u64) {
        self.emit(conversations_event(&self.store));
        self.emit(ServerEvent::History {
            conversation_id: id.clone(),
            items: Vec::new(),
        });
        self.emit(ServerEvent::Created {
            conversation_id: id,
            request,
            imported: false,
        });
    }

    fn error(&self, conversation_id: String, detail: String) {
        self.emit(ServerEvent::Error {
            conversation_id: Some(conversation_id),
            detail,
        });
    }

    fn open(&self, id: String) {
        let session = {
            let store = self.store.lock().unwrap();
            match store.get(&id) {
                Some(meta) => meta.session_id.clone(),
                None => return,
            }
        };

        let mut items = match &session {
            Some(sid) => {
                let (items, ai_title) = load_transcript(sid);
                if let Some(title) = ai_title {
                    let mut store = self.store.lock().unwrap();
                    if let Some(meta) = store.get_mut(&id)
                        && meta.imported_from.is_none()
                        && !meta.renamed
                        && meta.title != title
                    {
                        meta.title = title.clone();
                        self.live.rename(&id, &title);
                    }
                    store.save();
                }
                items
            }
            None => Vec::new(),
        };

        // The transcript cannot tell whether a call in the background still runs; the
        // conversation's process can, and without one nothing here hears such a call end.
        let running = self.live.in_background(&id);
        let alive = self.live.pid(&id).is_some();
        for item in &mut items {
            let Item::Tool(call) = item else {
                continue;
            };
            if running.contains(&call.call_id) {
                call.background = Some(Background {
                    status: TaskStatus::Running,
                    output: None,
                });
                call.ended = None;
            } else if let Some(background) = &mut call.background
                && background.status == TaskStatus::Running
                && !alive
            {
                background.status = TaskStatus::Unknown;
            }
        }
        self.emit(ServerEvent::History {
            conversation_id: id,
            items,
        });
        // Title may have been upgraded from the transcript's ai-title.
        self.emit(conversations_event(&self.store));
    }
}

impl Agents {
    fn emit(&self, event: ServerEvent) {
        let _ = self.events.send(event);
    }

    fn session_id(&self, conversation_id: &str) -> Option<String> {
        let store = self.store.lock().unwrap();
        store.get(conversation_id)?.session_id.clone()
    }

    fn fresh(&self, conversation_id: String, latest: Option<String>) -> Forward {
        Forward::new(
            self.events.clone(),
            self.store.clone(),
            conversation_id,
            latest,
            self.progress.clone(),
        )
    }

    /// Passes a turn's events on from a thread of its own.
    fn forward(&self, turn: std::sync::mpsc::Receiver<EngineEvent>, mut forward: Forward) {
        std::thread::spawn(move || {
            for ev in turn {
                forward.event(ev);
            }
        });
    }

    /// Sends a message, after any still waiting for the conversation to be free.
    fn send(&self, conversation_id: String, message: Outgoing) {
        if let Some(waiting) = self.waiting.lock().unwrap().get_mut(&conversation_id) {
            waiting.push_back(message);
            return;
        }
        if let Err(message) = self.deliver(&conversation_id, message) {
            self.waiting
                .lock()
                .unwrap()
                .insert(conversation_id.clone(), VecDeque::from([message]));
            self.wait(conversation_id);
        }
    }

    /// Waits for the process holding the conversation to end, then sends what waited, in
    /// order, including what arrives meanwhile.
    fn wait(&self, conversation_id: String) {
        let agents = self.clone();
        std::thread::spawn(move || {
            AgentLock::wait(&conversation_id, |pid, idle| {
                agents.emit(ServerEvent::Waiting {
                    conversation_id: conversation_id.clone(),
                    pid,
                    idle,
                });
            });
            // Held while each message goes, so a restart finds it either sent or still here.
            let mut waiting = agents.waiting.lock().unwrap();
            while let Some(message) = waiting
                .get_mut(&conversation_id)
                .and_then(VecDeque::pop_front)
            {
                if let Err(message) = agents.deliver(&conversation_id, message) {
                    waiting
                        .entry(conversation_id.clone())
                        .or_default()
                        .push_front(message);
                    drop(waiting);
                    return agents.wait(conversation_id);
                }
            }
            waiting.remove(&conversation_id);
        });
    }

    /// Hands a message to the conversation's agent, starting one if needed. Gives it back
    /// when a process this app does not run holds the conversation.
    fn deliver(&self, conversation_id: &str, message: Outgoing) -> Result<(), Outgoing> {
        let Outgoing {
            text,
            profile,
            model,
            effort,
            attachments,
        } = &message;
        let (cwd, session_id, borrowed, name, always) = {
            let mut store = self.store.lock().unwrap();
            // A message to an archived conversation means it is in use again.
            store.set_archived(conversation_id, false);
            let Some(name) = store.title_for(conversation_id, text) else {
                drop(store);
                self.emit(ServerEvent::Error {
                    conversation_id: Some(conversation_id.to_string()),
                    detail: "unknown conversation".to_string(),
                });
                return Ok(());
            };
            let meta = store.get(conversation_id).expect("titled above");
            (
                meta.cwd.clone(),
                meta.session_id.clone(),
                meta.borrowed(),
                name,
                meta.always_allowed.clone(),
            )
        };
        self.emit(conversations_event(&self.store));

        let config = SessionConfig {
            cwd,
            profile: *profile,
            model: (!model.trim().is_empty()).then(|| model.clone()),
            effort: effort.cli().map(str::to_owned),
        };
        let resume = session_id.as_deref().map(|session_id| Resume {
            session_id,
            fork: borrowed,
        });
        let turn = match self.live.send_turn(
            conversation_id,
            config,
            resume,
            &name,
            &always,
            UserMessage { text, attachments },
        ) {
            Ok(turn) => turn,
            Err(SendError::Occupied) => return Err(message),
            Err(SendError::Failed(detail)) => {
                self.emit(ServerEvent::Error {
                    conversation_id: Some(conversation_id.to_string()),
                    detail,
                });
                return Ok(());
            }
        };

        let mut forward = self.fresh(conversation_id.to_string(), session_id);
        // A forked command's `init` comes only once it ends, so the turn starts when it is sent.
        forward.started = Some(crate::sessions::now_secs());
        self.forward(turn, forward);
        Ok(())
    }
}

/// Turns a conversation's engine events into `ServerEvent`s, keeping the store's
/// session id and recency up to date along the way.
struct Forward {
    events: UnboundedSender<ServerEvent>,
    store: Arc<Mutex<Store>>,
    conversation_id: String,
    /// The session id to resume with, as last reported.
    latest: Option<String>,
    /// The running turn has streamed text.
    streamed: bool,
    /// When the running turn started, in Unix seconds.
    started: Option<u64>,
    /// Where the running turn's progress is kept for a handover.
    progress: Running,
}

impl Forward {
    fn new(
        events: UnboundedSender<ServerEvent>,
        store: Arc<Mutex<Store>>,
        conversation_id: String,
        latest: Option<String>,
        progress: Running,
    ) -> Self {
        Self {
            events,
            store,
            conversation_id,
            latest,
            streamed: false,
            started: None,
            progress,
        }
    }

    /// Picks up a turn a handover passed on where it was, or starts like `new`.
    fn resumed(
        events: UnboundedSender<ServerEvent>,
        store: Arc<Mutex<Store>>,
        conversation_id: String,
        progress: &Running,
    ) -> Self {
        let latest = store
            .lock()
            .unwrap()
            .get(&conversation_id)
            .and_then(|meta| meta.session_id.clone());
        let was = progress
            .lock()
            .unwrap()
            .get(&conversation_id)
            .copied()
            .unwrap_or_default();
        Self {
            streamed: was.streamed,
            started: was.started,
            ..Self::new(events, store, conversation_id, latest, progress.clone())
        }
    }

    fn note(&self) {
        self.progress.lock().unwrap().insert(
            self.conversation_id.clone(),
            Progress {
                started: self.started,
                streamed: self.streamed,
            },
        );
    }

    fn done(&self) {
        self.progress.lock().unwrap().remove(&self.conversation_id);
    }

    fn emit(&self, event: ServerEvent) {
        let _ = self.events.send(event);
    }

    fn event(&mut self, ev: EngineEvent) {
        let conversation_id = self.conversation_id.clone();
        match ev {
            EngineEvent::Started { session_id } => {
                self.started.get_or_insert_with(crate::sessions::now_secs);
                self.note();
                self.latest = Some(session_id.clone());
                self.store
                    .lock()
                    .unwrap()
                    .record_turn(&conversation_id, Some(&session_id));
                self.emit(ServerEvent::Started {
                    conversation_id,
                    session_id,
                });
                self.emit(conversations_event(&self.store));
            }
            EngineEvent::Delivered { joined } => {
                if !joined {
                    self.started = Some(crate::sessions::now_secs());
                    self.note();
                }
                self.emit(ServerEvent::Delivered { conversation_id });
            }
            EngineEvent::Typed { text } => self.emit(ServerEvent::Typed {
                conversation_id,
                text,
            }),
            EngineEvent::Incoming { from, text } => self.emit(ServerEvent::Incoming {
                conversation_id,
                from,
                text,
            }),
            EngineEvent::Delta { text } => {
                if !self.streamed {
                    self.streamed = true;
                    self.note();
                }
                self.emit(ServerEvent::Delta {
                    conversation_id,
                    text,
                });
            }
            EngineEvent::Compacting => self.emit(ServerEvent::Compacting { conversation_id }),
            EngineEvent::Retrying {
                attempt,
                max_retries,
                error,
            } => self.emit(ServerEvent::Retrying {
                conversation_id,
                attempt,
                max_retries,
                error,
            }),
            EngineEvent::Compacted(compaction) => {
                // The next turn reports the real size; until then, the summary's.
                {
                    let mut store = self.store.lock().unwrap();
                    let window = store.get(&conversation_id).and_then(|c| c.context.window);
                    let context = ContextUsage {
                        used: compaction.after,
                        window,
                    };
                    store.set_context(&conversation_id, context);
                }
                self.emit(ServerEvent::Compacted {
                    conversation_id,
                    compaction,
                });
                self.emit(conversations_event(&self.store));
            }
            EngineEvent::CompactSummary(text) => self.emit(ServerEvent::CompactSummary {
                conversation_id,
                text,
            }),
            EngineEvent::ThinkingDelta { text } => self.emit(ServerEvent::ThinkingDelta {
                conversation_id,
                text,
            }),
            EngineEvent::Quota(quota) => self.emit(ServerEvent::Quota(quota)),
            EngineEvent::ApprovalNeeded {
                request_id,
                call,
                scope,
            } => self.emit(ServerEvent::ApprovalRequested {
                conversation_id,
                request_id,
                call,
                scope,
            }),
            EngineEvent::ToolStarted(call) => self.emit(ServerEvent::ToolStarted {
                conversation_id,
                call: ToolCall {
                    started: call.started.or(Some(crate::sessions::now_millis())),
                    ..call
                },
            }),
            EngineEvent::Notice { text } => self.emit(ServerEvent::Notice {
                conversation_id,
                text,
            }),
            EngineEvent::Background {
                call_id,
                background,
            } => self.emit(ServerEvent::Background {
                conversation_id,
                call_id,
                background,
                at: Some(crate::sessions::now_millis()),
            }),
            EngineEvent::ToolResult {
                call_id,
                content,
                is_error,
            } => self.emit(ServerEvent::ToolResult {
                conversation_id,
                call_id,
                content,
                is_error,
                at: Some(crate::sessions::now_millis()),
            }),
            EngineEvent::Completed {
                session_id,
                is_error,
                context,
                text,
            } => {
                if session_id.is_some() {
                    self.latest = session_id;
                }
                // A local command such as `/context` answers in its result alone.
                let streamed = std::mem::take(&mut self.streamed);
                if let Some(text) = text.filter(|_| !streamed) {
                    self.emit(match crate::engine::cli::review(&text) {
                        Some(review) => ServerEvent::Review {
                            conversation_id: conversation_id.clone(),
                            review,
                        },
                        None => ServerEvent::Delta {
                            conversation_id: conversation_id.clone(),
                            text,
                        },
                    });
                }
                {
                    let mut store = self.store.lock().unwrap();
                    store.record_turn(&conversation_id, self.latest.as_deref());
                    // A turn without a request, such as a compaction, reports no usage.
                    if context.used > 0 {
                        store.set_context(&conversation_id, context);
                    }
                }
                crate::debug::log(
                    &conversation_id,
                    format!(
                        "turn ended{}, context {} of {}",
                        if is_error { " with an error" } else { "" },
                        context.used,
                        context.window.unwrap_or(0)
                    ),
                );
                let time = self.started.take().map(|started| TurnTime {
                    started,
                    ended: crate::sessions::now_secs(),
                });
                self.done();
                self.emit(ServerEvent::Completed {
                    conversation_id,
                    is_error,
                    context,
                    time,
                });
                self.emit(conversations_event(&self.store));
            }
            EngineEvent::Review(review) => self.emit(ServerEvent::Review {
                conversation_id,
                review,
            }),
            EngineEvent::Suggested { text } => {
                crate::debug::log(&conversation_id, format!("suggested {text:?}"));
                self.emit(ServerEvent::Suggested {
                    conversation_id,
                    text,
                })
            }
            EngineEvent::Error { detail } => {
                self.done();
                self.emit(ServerEvent::Error {
                    conversation_id: Some(conversation_id),
                    detail,
                })
            }
        }
    }
}

fn conversations_event(store: &Mutex<Store>) -> ServerEvent {
    let list = store
        .lock()
        .unwrap()
        .ordered()
        .into_iter()
        .map(|m| ConversationInfo {
            id: m.id,
            title: m.title,
            cwd: m.cwd.display().to_string(),
            attached: m.attached,
            archived: m.archived,
            overrides: m.overrides,
            context: m.context,
        })
        .collect();
    ServerEvent::Conversations { list }
}
