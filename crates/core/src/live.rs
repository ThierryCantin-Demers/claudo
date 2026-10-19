//! The `claude` processes behind conversations: one per conversation, kept
//! between turns and closed once idle. The next turn after that respawns it
//! with `--resume`, so closing one loses nothing but startup time.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use claudo_protocol::{ConversationId, Decision};

use crate::claude_bin;
use crate::engine::cli::{Adopted, Incoming, SessionState};
use crate::engine::{EngineEvent, Resume, Session, SessionConfig, UserMessage};
use crate::handover::{Handed, Progress};

/// Events of turns a conversation's process started on its own, such as for another
/// agent's message, with the conversation they belong to.
pub type IncomingSender = mpsc::Sender<(ConversationId, EngineEvent)>;

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub struct LiveSessions {
    sessions: Arc<Mutex<HashMap<ConversationId, Session>>>,
    incoming: Option<IncomingSender>,
    /// Each process turns Remote Control on as it starts.
    remote_control: AtomicBool,
    _stop_reaper: mpsc::Sender<()>,
}

impl LiveSessions {
    /// A process with no turn running and no output for `idle_timeout` is closed.
    pub fn new(idle_timeout: Duration, incoming: Option<IncomingSender>) -> Self {
        let sessions: Arc<Mutex<HashMap<ConversationId, Session>>> = Arc::default();
        let (stop_reaper, stopped) = mpsc::channel::<()>();
        let reaped = sessions.clone();
        let tick = idle_timeout.min(Duration::from_secs(60));
        thread::spawn(move || {
            while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(tick) {
                reaped.lock().unwrap().retain(|_, session| {
                    session.is_alive()
                        && (session.is_busy()
                            || session.has_background()
                            || session.idle_for() < idle_timeout)
                });
            }
        });
        Self {
            sessions,
            incoming,
            remote_control: AtomicBool::new(false),
            _stop_reaper: stop_reaper,
        }
    }

    /// Turn Remote Control on or off for every process, and for those started from now on;
    /// `name` gives each conversation's.
    pub fn set_remote_control(&self, enabled: bool, name: impl Fn(&str) -> String) {
        self.remote_control.store(enabled, Ordering::Relaxed);
        for (conversation_id, session) in self.sessions.lock().unwrap().iter() {
            let _ = session.remote_control(enabled, &name(conversation_id));
        }
    }

    /// Start a turn on the conversation's process, or queue it behind the running
    /// one. A new process named `name` is spawned, resuming `resume`, when there is
    /// none, it exited, or its flags differ from `config`. A new process allows the
    /// `always` approval scopes without asking.
    pub fn send_turn(
        &self,
        conversation_id: &str,
        config: SessionConfig,
        resume: Option<Resume>,
        name: &str,
        always: &[String],
        message: UserMessage,
    ) -> Result<Receiver<EngineEvent>, String> {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get_mut(conversation_id)
            && session.is_alive()
        {
            // While a turn runs, a message joins it or waits for it, under the flags it has.
            // A process running something in the background keeps its flags too: replacing
            // it would leave that running where nothing reports on it.
            let busy = session.is_busy() || session.has_background();
            if (busy || *session.config() == config)
                && let Ok(turn) = session.send(message)
            {
                return Ok(turn);
            }
            if busy {
                return Err("claude stopped taking messages while still working".to_string());
            }
        }

        let incoming = self.incoming_for(conversation_id);
        let mut session = Session::spawn(config, resume, name, incoming)
            .map_err(|e| format!("failed to launch `{}`: {e}", claude_bin()))?;
        for scope in always {
            session.allow_always(scope);
        }
        if self.remote_control.load(Ordering::Relaxed) {
            let _ = session.remote_control(true, name);
        }
        let turn = session
            .send(message)
            .map_err(|e| format!("claude exited before the turn was sent: {e}"))?;
        sessions.insert(conversation_id.to_owned(), session);
        Ok(turn)
    }

    /// Where a conversation's process reports the turns it starts on its own.
    fn incoming_for(&self, conversation_id: &str) -> Option<Incoming> {
        self.incoming.clone().map(|incoming| {
            let conversation_id = conversation_id.to_owned();
            Box::new(move |ev| {
                let _ = incoming.send((conversation_id.clone(), ev));
            }) as Incoming
        })
    }

    /// Take over a conversation's process from the app this one replaced.
    pub fn adopt(&self, conversation_id: &str, state: SessionState) -> Adopted {
        let (session, adopted) = Session::adopt(state, self.incoming_for(conversation_id));
        self.sessions
            .lock()
            .unwrap()
            .insert(conversation_id.to_owned(), session);
        adopted
    }

    /// Replace this process with `exe`, handing it every process still running, each with
    /// what `progress` knew of its conversation's running turn. Returns only if that failed,
    /// and then everything goes on as it was.
    pub fn exec(&self, exe: &Path, progress: impl Fn(&str) -> Progress) -> io::Error {
        let sessions = self.sessions.lock().unwrap();
        let mut paused = Vec::new();
        let mut handed = Vec::new();
        for (conversation, session) in sessions.iter() {
            if let Some((reader, state)) = session.pause() {
                paused.push(reader);
                handed.push(Handed {
                    conversation: conversation.clone(),
                    state,
                    progress: progress(conversation),
                });
            }
        }
        crate::handover::exec(exe, handed)
    }

    /// Take a conversation's process away, so no later turn reaches it.
    pub fn remove(&self, conversation_id: &str) -> Option<Session> {
        self.sessions.lock().unwrap().remove(conversation_id)
    }

    /// Rename a conversation's process for the other agents, if it has one.
    pub fn rename(&self, conversation_id: &str, name: &str) {
        if let Some(session) = self.sessions.lock().unwrap().get(conversation_id) {
            let _ = session.rename(name);
        }
    }

    /// Stop a conversation's running turn.
    pub fn interrupt(&self, conversation_id: &str) -> Result<(), String> {
        let sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get(conversation_id)
            .ok_or_else(|| "that conversation has no running claude process".to_string())?;
        session
            .interrupt()
            .map_err(|e| format!("could not stop the turn: {e}"))
    }

    /// Answer the approval a conversation's running turn is waiting on. Returns its scope.
    pub fn answer(
        &self,
        conversation_id: &str,
        request_id: &str,
        decision: Decision,
    ) -> Result<String, String> {
        let sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get(conversation_id)
            .ok_or_else(|| "that conversation has no running claude process".to_string())?;
        session
            .answer(request_id, decision)
            .map_err(|e| format!("could not answer the approval: {e}"))
    }

    /// The process currently serving a conversation, if any.
    pub fn pid(&self, conversation_id: &str) -> Option<u32> {
        let sessions = self.sessions.lock().unwrap();
        sessions
            .get(conversation_id)
            .filter(|s| s.is_alive())
            .map(Session::pid)
    }
}
