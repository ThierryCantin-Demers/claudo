//! The transport-agnostic backend: it owns the conversation store, drives the
//! engine, and emits `ServerEvent`s. A transport (in-process channel, or the
//! daemon's WebSocket server) feeds it `Command`s and forwards its events.
//!
//! Events go out on a tokio unbounded channel: the sender is `Send + Sync` and
//! usable from the per-turn worker threads, while a consumer can either
//! `try_recv` (native, no runtime) or `recv().await` (the daemon).

use std::sync::{Arc, Mutex};

use claudo_protocol::{Command, ConversationInfo, ServerEvent};
use tokio::sync::mpsc::UnboundedSender;

use crate::engine::{run_turn, EngineEvent};
use crate::sessions::{load_transcript, Store};

pub struct Backend {
    store: Arc<Mutex<Store>>,
    events: UnboundedSender<ServerEvent>,
}

impl Backend {
    pub fn new(events: UnboundedSender<ServerEvent>) -> Self {
        Self {
            store: Arc::new(Mutex::new(Store::load())),
            events,
        }
    }

    fn emit(&self, event: ServerEvent) {
        let _ = self.events.send(event);
    }

    /// Handle one command. Returns quickly; `SendMessage` runs the turn on a
    /// worker thread and streams events as they arrive.
    pub fn handle(&self, cmd: Command) {
        match cmd {
            Command::GetStatus => self.emit(ServerEvent::Status(crate::detect_system())),

            Command::GetSettings => self.emit(ServerEvent::Settings(crate::settings::load())),

            Command::UpdateSettings { settings } => {
                crate::settings::save(&settings);
                self.emit(ServerEvent::Settings(settings));
            }

            Command::ListConversations => {
                let event = conversations_event(&self.store, None);
                self.emit(event);
            }

            Command::NewScratch => {
                let id = self.store.lock().unwrap().create_scratch();
                self.emit(conversations_event(&self.store, Some(id.clone())));
                self.emit(ServerEvent::History {
                    conversation_id: id,
                    items: Vec::new(),
                });
            }

            Command::NewAttached { path } => {
                let id = self.store.lock().unwrap().create_attached(path.into());
                self.emit(conversations_event(&self.store, Some(id.clone())));
                self.emit(ServerEvent::History {
                    conversation_id: id,
                    items: Vec::new(),
                });
            }

            Command::OpenConversation { id } => self.open(id),

            Command::SetOverrides { id, overrides } => {
                self.store.lock().unwrap().set_overrides(&id, overrides);
                self.emit(conversations_event(&self.store, None));
            }

            Command::SendMessage {
                conversation_id,
                text,
                profile,
                model,
                effort,
            } => self.send(conversation_id, text, profile, model, effort),
        }
    }

    fn open(&self, id: String) {
        let session = {
            let store = self.store.lock().unwrap();
            match store.get(&id) {
                Some(meta) => meta.session_id.clone(),
                None => return,
            }
        };

        let items = match &session {
            Some(sid) => {
                let (items, ai_title) = load_transcript(sid);
                if let Some(title) = ai_title {
                    let mut store = self.store.lock().unwrap();
                    if let Some(meta) = store.get_mut(&id) {
                        if !meta.attached {
                            meta.title = title;
                        }
                    }
                    store.save();
                }
                items
            }
            None => Vec::new(),
        };

        self.emit(ServerEvent::History {
            conversation_id: id,
            items,
        });
        // Title may have been upgraded from the transcript's ai-title.
        self.emit(conversations_event(&self.store, None));
    }

    fn send(
        &self,
        conversation_id: String,
        text: String,
        profile: claudo_protocol::Profile,
        model: String,
        effort: claudo_protocol::Effort,
    ) {
        let (cwd, session_id) = {
            let store = self.store.lock().unwrap();
            match store.get(&conversation_id) {
                Some(meta) => (meta.cwd.clone(), meta.session_id.clone()),
                None => {
                    self.emit(ServerEvent::Error {
                        detail: "unknown conversation".to_string(),
                    });
                    return;
                }
            }
        };

        let events = self.events.clone();
        let store = self.store.clone();

        std::thread::spawn(move || {
            let mut latest = session_id.clone();
            let model_arg = (!model.trim().is_empty()).then_some(model.as_str());
            for ev in run_turn(
                &cwd,
                session_id.as_deref(),
                &text,
                profile,
                model_arg,
                effort.cli(),
            ) {
                match ev {
                    EngineEvent::Started { session_id } => {
                        latest = Some(session_id.clone());
                        store
                            .lock()
                            .unwrap()
                            .record_turn(&conversation_id, Some(&session_id), &text);
                        let _ = events.send(ServerEvent::Started {
                            conversation_id: conversation_id.clone(),
                            session_id,
                        });
                        let _ = events.send(conversations_event(&store, None));
                    }
                    EngineEvent::Delta { text } => {
                        let _ = events.send(ServerEvent::Delta {
                            conversation_id: conversation_id.clone(),
                            text,
                        });
                    }
                    EngineEvent::ToolStarted {
                        call_id,
                        name,
                        input,
                    } => {
                        let _ = events.send(ServerEvent::ToolStarted {
                            conversation_id: conversation_id.clone(),
                            call_id,
                            name,
                            input,
                        });
                    }
                    EngineEvent::ToolResult {
                        call_id,
                        content,
                        is_error,
                    } => {
                        let _ = events.send(ServerEvent::ToolResult {
                            conversation_id: conversation_id.clone(),
                            call_id,
                            content,
                            is_error,
                        });
                    }
                    EngineEvent::Completed {
                        session_id,
                        is_error,
                        context,
                    } => {
                        if let Some(sid) = &session_id {
                            latest = Some(sid.clone());
                        }
                        {
                            let mut s = store.lock().unwrap();
                            s.record_turn(&conversation_id, latest.as_deref(), &text);
                            s.set_context(&conversation_id, context);
                        }
                        let _ = events.send(ServerEvent::Completed {
                            conversation_id: conversation_id.clone(),
                            is_error,
                            context,
                        });
                        let _ = events.send(conversations_event(&store, None));
                    }
                    EngineEvent::Error { detail } => {
                        let _ = events.send(ServerEvent::Error { detail });
                    }
                }
            }
        });
    }
}

fn conversations_event(store: &Mutex<Store>, current: Option<String>) -> ServerEvent {
    let list = store
        .lock()
        .unwrap()
        .ordered()
        .into_iter()
        .map(|m| ConversationInfo {
            id: m.id,
            title: m.title,
            cwd: m.cwd.display().to_string(),
            overrides: m.overrides,
            context: m.context,
        })
        .collect();
    ServerEvent::Conversations { list, current }
}
