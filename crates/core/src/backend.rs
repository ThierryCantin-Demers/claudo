//! The transport-agnostic backend: it owns the conversation store, drives the
//! engine, and emits `ServerEvent`s. A transport (in-process channel, or the
//! daemon's WebSocket server) feeds it `Command`s and forwards its events.
//!
//! Events go out on a tokio unbounded channel: the sender is `Send + Sync` and
//! usable from the worker threads forwarding each turn, while a consumer can either
//! `try_recv` (native, no runtime) or `recv().await` (the daemon).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use claudo_protocol::{
    Command, ContextUsage, ConversationInfo, Decision, Item, ModelOption, ServerEvent, TaskStatus,
    ToolCall, TurnTime,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::engine::{EngineEvent, Resume, SessionConfig, UserMessage};
use crate::live::{IDLE_TIMEOUT, LiveSessions};
use crate::sessions::{Store, load_transcript};

pub struct Backend {
    store: Arc<Mutex<Store>>,
    live: LiveSessions,
    events: UnboundedSender<ServerEvent>,
}

impl Backend {
    pub fn new(events: UnboundedSender<ServerEvent>) -> Self {
        let store = Arc::new(Mutex::new(Store::load()));
        let (incoming, turns) = std::sync::mpsc::channel();
        let (forwarded, kept) = (events.clone(), store.clone());
        std::thread::spawn(move || {
            let mut forwards: HashMap<String, Forward> = HashMap::new();
            for (id, ev) in turns {
                forwards
                    .entry(id)
                    .or_insert_with_key(|id| {
                        Forward::new(forwarded.clone(), kept.clone(), id.clone(), None)
                    })
                    .event(ev);
            }
        });
        let backend = Self {
            store,
            live: LiveSessions::new(IDLE_TIMEOUT, Some(incoming)),
            events,
        };
        if crate::settings::load().remote_control {
            backend.live.set_remote_control(true, |_| String::new());
        }
        backend
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
            } => self.send(conversation_id, text, profile, model, effort, attachments),

            Command::Stop { conversation_id } => {
                if let Err(detail) = self.live.interrupt(&conversation_id) {
                    self.error(conversation_id, detail);
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

        // Without a process of this claudo's, nothing here hears a background call end, so
        // one still marked running is shown as having ended.
        if self.live.pid(&id).is_none() {
            for item in &mut items {
                if let Item::Tool(call) = item
                    && let Some(background) = &mut call.background
                    && background.status == TaskStatus::Running
                {
                    background.status = TaskStatus::Unknown;
                }
            }
        }
        self.emit(ServerEvent::History {
            conversation_id: id,
            items,
        });
        // Title may have been upgraded from the transcript's ai-title.
        self.emit(conversations_event(&self.store));
    }

    fn send(
        &self,
        conversation_id: String,
        text: String,
        profile: claudo_protocol::Profile,
        model: String,
        effort: claudo_protocol::Effort,
        attachments: Vec<claudo_protocol::Attachment>,
    ) {
        let (cwd, session_id, borrowed, name, always) = {
            let mut store = self.store.lock().unwrap();
            // A message to an archived conversation means it is in use again.
            store.set_archived(&conversation_id, false);
            let Some(name) = store.title_for(&conversation_id, &text) else {
                drop(store);
                self.error(conversation_id, "unknown conversation".to_string());
                return;
            };
            let meta = store.get(&conversation_id).expect("titled above");
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
            profile,
            model: (!model.trim().is_empty()).then_some(model),
            effort: effort.cli().map(str::to_owned),
        };
        let resume = session_id.as_deref().map(|session_id| Resume {
            session_id,
            fork: borrowed,
        });
        let turn = match self.live.send_turn(
            &conversation_id,
            config,
            resume,
            &name,
            &always,
            UserMessage {
                text: &text,
                attachments: &attachments,
            },
        ) {
            Ok(turn) => turn,
            Err(detail) => {
                self.error(conversation_id, detail);
                return;
            }
        };

        let mut forward = Forward::new(
            self.events.clone(),
            self.store.clone(),
            conversation_id,
            session_id,
        );
        // A forked command's `init` comes only once it ends, so the turn starts when it is sent.
        forward.started = Some(crate::sessions::now_secs());
        std::thread::spawn(move || {
            for ev in turn {
                forward.event(ev);
            }
        });
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
}

impl Forward {
    fn new(
        events: UnboundedSender<ServerEvent>,
        store: Arc<Mutex<Store>>,
        conversation_id: String,
        latest: Option<String>,
    ) -> Self {
        Self {
            events,
            store,
            conversation_id,
            latest,
            streamed: false,
            started: None,
        }
    }

    fn emit(&self, event: ServerEvent) {
        let _ = self.events.send(event);
    }

    fn event(&mut self, ev: EngineEvent) {
        let conversation_id = self.conversation_id.clone();
        match ev {
            EngineEvent::Started { session_id } => {
                self.started.get_or_insert_with(crate::sessions::now_secs);
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
                self.streamed = true;
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
                    self.emit(ServerEvent::Delta {
                        conversation_id: conversation_id.clone(),
                        text,
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
                let time = self.started.take().map(|started| TurnTime {
                    started,
                    ended: crate::sessions::now_secs(),
                });
                self.emit(ServerEvent::Completed {
                    conversation_id,
                    is_error,
                    context,
                    time,
                });
                self.emit(conversations_event(&self.store));
            }
            EngineEvent::Error { detail } => self.emit(ServerEvent::Error {
                conversation_id: Some(conversation_id),
                detail,
            }),
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
