//! The `claude` processes behind conversations: one per conversation, kept
//! between turns and closed once idle. The next turn after that respawns it
//! with `--resume`, so closing one loses nothing but startup time.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use claudo_protocol::{ConversationId, Decision};

use crate::claude_bin;
use crate::engine::{EngineEvent, Resume, Session, SessionConfig};

/// Events of turns a conversation's process started on its own, such as for another
/// agent's message, with the conversation they belong to.
pub type IncomingSender = mpsc::Sender<(ConversationId, EngineEvent)>;

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub struct LiveSessions {
    sessions: Arc<Mutex<HashMap<ConversationId, Session>>>,
    incoming: Option<IncomingSender>,
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
                    session.is_alive() && (session.is_busy() || session.idle_for() < idle_timeout)
                });
            }
        });
        Self {
            sessions,
            incoming,
            _stop_reaper: stop_reaper,
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
        prompt: &str,
    ) -> Result<Receiver<EngineEvent>, String> {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get_mut(conversation_id)
            && session.is_alive()
        {
            if *session.config() == config
                && let Ok(turn) = session.send(prompt)
            {
                return Ok(turn);
            }
            if session.is_busy() {
                return Err(
                    "a turn is running; change the model, thinking or permissions after it ends"
                        .to_string(),
                );
            }
        }

        let incoming = self.incoming.clone().map(|incoming| {
            let conversation_id = conversation_id.to_owned();
            Box::new(move |ev| {
                let _ = incoming.send((conversation_id.clone(), ev));
            }) as crate::engine::cli::Incoming
        });
        let mut session = Session::spawn(config, resume, name, incoming)
            .map_err(|e| format!("failed to launch `{}`: {e}", claude_bin()))?;
        for scope in always {
            session.allow_always(scope);
        }
        let turn = session
            .send(prompt)
            .map_err(|e| format!("claude exited before the turn was sent: {e}"))?;
        sessions.insert(conversation_id.to_owned(), session);
        Ok(turn)
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
