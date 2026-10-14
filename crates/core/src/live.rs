//! The `claude` processes behind conversations: one per conversation, kept
//! between turns and closed once idle. The next turn after that respawns it
//! with `--resume`, so closing one loses nothing but startup time.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use claudo_protocol::ConversationId;

use crate::claude_bin;
use crate::engine::{EngineEvent, Session, SessionConfig};

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub struct LiveSessions {
    sessions: Arc<Mutex<HashMap<ConversationId, Session>>>,
    _stop_reaper: mpsc::Sender<()>,
}

impl LiveSessions {
    /// A process with no turn running and no output for `idle_timeout` is closed.
    pub fn new(idle_timeout: Duration) -> Self {
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
            _stop_reaper: stop_reaper,
        }
    }

    /// Start a turn on the conversation's process. A new one is spawned,
    /// resuming `resume`, when there is none, it exited, or its flags differ
    /// from `config`.
    pub fn send_turn(
        &self,
        conversation_id: &str,
        config: SessionConfig,
        resume: Option<&str>,
        prompt: &str,
    ) -> Result<Receiver<EngineEvent>, String> {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get_mut(conversation_id) {
            if session.is_busy() {
                return Err("a turn is already running in this conversation".to_string());
            }
            if session.is_alive()
                && *session.config() == config
                && let Ok(turn) = session.send(prompt)
            {
                return Ok(turn);
            }
        }

        let mut session = Session::spawn(config, resume)
            .map_err(|e| format!("failed to launch `{}`: {e}", claude_bin()))?;
        let turn = session
            .send(prompt)
            .map_err(|e| format!("claude exited before the turn was sent: {e}"))?;
        sessions.insert(conversation_id.to_owned(), session);
        Ok(turn)
    }

    /// Answer the approval a conversation's running turn is waiting on.
    pub fn answer(
        &self,
        conversation_id: &str,
        request_id: &str,
        allow: bool,
    ) -> Result<(), String> {
        let sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get(conversation_id)
            .ok_or_else(|| "that conversation has no running claude process".to_string())?;
        session
            .answer(request_id, allow)
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
