//! The `claude` processes behind conversations: one per conversation, kept
//! between turns and closed once idle. The next turn after that respawns it
//! with `--resume`, so closing one loses nothing but startup time.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use claudo_protocol::{ConversationId, Decision};

use crate::claude_bin;
use crate::engine::cli::{Adopted, Held, Incoming, SessionState};
use crate::engine::{EngineEvent, Resume, Session, SessionConfig, UserMessage};
use crate::handover::{Handed, Progress, Waited};

/// Events of turns a conversation's process started on its own, such as for another
/// agent's message, with the conversation they belong to.
pub type IncomingSender = mpsc::Sender<(ConversationId, EngineEvent)>;

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug)]
pub enum SendError {
    /// A process this app does not run is the conversation's agent: one a killed claudo
    /// left running, or another claudo's.
    Occupied,
    Failed(String),
}

/// Held by a conversation's agent for as long as it runs, so no second one starts on it,
/// whichever claudo would start it. The agent inherits the lock and keeps it when the app
/// that started it dies; the file holds its pid.
pub struct AgentLock(Option<File>);

impl AgentLock {
    fn path(conversation_id: &str) -> PathBuf {
        Self::dir().join(format!("{conversation_id}.lock"))
    }

    fn idle_path(conversation_id: &str) -> PathBuf {
        Self::dir().join(format!("{conversation_id}.idle"))
    }

    fn dir() -> PathBuf {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("claudo-agents")
    }

    fn open(conversation_id: &str) -> io::Result<File> {
        let path = Self::path(conversation_id);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
    }

    /// The lock, or the pid of the agent holding it. Where no lock can be had, there is
    /// nothing to wait on either.
    pub fn take(conversation_id: &str) -> Result<Self, u32> {
        let Ok(file) = Self::open(conversation_id) else {
            return Ok(Self(None));
        };
        match file.try_lock() {
            Ok(()) => {
                // Left by an agent that went while idle.
                let _ = std::fs::remove_file(Self::idle_path(conversation_id));
                Ok(Self(Some(file)))
            }
            Err(TryLockError::WouldBlock) => Err(Self::holder(&file)),
            Err(TryLockError::Error(_)) => Ok(Self(None)),
        }
    }

    fn holder(mut file: &File) -> u32 {
        let mut pid = String::new();
        let _ = file
            .seek(SeekFrom::Start(0))
            .and_then(|_| file.read_to_string(&mut pid));
        pid.trim().parse().unwrap_or(0)
    }

    /// Returns once whatever holds the conversation's lock has let go of it. Until then,
    /// `holder` hears the holder's pid and whether it sits idle, at first and whenever
    /// either changes.
    pub fn wait(conversation_id: &str, mut holder: impl FnMut(u32, bool)) {
        let Ok(file) = Self::open(conversation_id) else {
            return;
        };
        let idle = Self::idle_path(conversation_id);
        let mut told = None;
        while let Err(TryLockError::WouldBlock) = file.try_lock() {
            let now = (Self::holder(&file), idle.exists());
            if told != Some(now) {
                told = Some(now);
                holder(now.0, now.1);
            }
            thread::sleep(Duration::from_secs(1));
        }
    }

    fn held(self, conversation_id: &str) -> Option<Held> {
        self.0.map(|lock| Held {
            lock,
            idle: Self::idle_path(conversation_id),
        })
    }
}

/// Stops the agent holding a conversation's lock and whatever it started, once its pid is
/// surely still a `claude` process's: pids are reused.
pub fn stop_stray(conversation_id: &str) -> io::Result<()> {
    let Err(pid) = AgentLock::take(conversation_id) else {
        return Ok(());
    };
    let command = std::fs::read(format!("/proc/{pid}/cmdline"))?;
    if pid == 0 || !String::from_utf8_lossy(&command).contains("claude") {
        return Err(io::Error::other(format!(
            "process {pid} is not a claude agent"
        )));
    }
    for pid in descendants(pid).into_iter().chain([pid]) {
        // SAFETY: sends a signal; a process gone meanwhile makes it fail harmlessly.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    }
    Ok(())
}

fn descendants(pid: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let mut next = vec![pid];
    while let Some(parent) = next.pop() {
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{parent}/task")) else {
            continue;
        };
        for task in tasks.flatten() {
            let children =
                std::fs::read_to_string(task.path().join("children")).unwrap_or_default();
            for child in children.split_whitespace().filter_map(|c| c.parse().ok()) {
                found.push(child);
                next.push(child);
            }
        }
    }
    found
}

pub struct LiveSessions {
    sessions: Arc<Mutex<HashMap<ConversationId, Session>>>,
    incoming: Option<IncomingSender>,
    /// Each process turns Remote Control on as it starts.
    remote_control: AtomicBool,
    suggestions: Mutex<Suggestions>,
    _stop_reaper: mpsc::Sender<()>,
}

/// Guessing the next message costs a request over the whole conversation, so only
/// conversations on screen get one, and none while the setting is off. The CLI reads it
/// once, as a process starts, and ignores pausing one for now, so a message replaces an
/// idle process that guesses otherwise.
#[derive(Default)]
struct Suggestions {
    enabled: bool,
    on_screen: HashSet<ConversationId>,
}

impl Suggestions {
    fn paused(&self, conversation_id: &str) -> bool {
        !self.enabled || !self.on_screen.contains(conversation_id)
    }
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
            suggestions: Mutex::default(),
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

    /// Guess the next message after each turn, or no longer. Idle processes are closed,
    /// so the next turn starts one that does as the setting says.
    pub fn set_suggestions(&self, enabled: bool) {
        let changed =
            std::mem::replace(&mut self.suggestions.lock().unwrap().enabled, enabled) != enabled;
        if changed {
            self.sessions
                .lock()
                .unwrap()
                .retain(|_, session| session.is_busy() || session.has_background());
        }
        self.pause_suggestions();
    }

    /// Only these conversations are on screen.
    pub fn set_on_screen(&self, conversation_ids: Vec<ConversationId>) {
        self.suggestions.lock().unwrap().on_screen = conversation_ids.into_iter().collect();
        self.pause_suggestions();
    }

    fn pause_suggestions(&self) {
        let sessions = self.sessions.lock().unwrap();
        let suggestions = self.suggestions.lock().unwrap();
        for (conversation_id, session) in sessions.iter() {
            let _ = session.pause_suggestions(suggestions.paused(conversation_id));
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
    ) -> Result<Receiver<EngineEvent>, SendError> {
        let suggest = !self.suggestions.lock().unwrap().paused(conversation_id);
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get_mut(conversation_id)
            && session.is_alive()
        {
            // While a turn runs, a message joins it or waits for it, under the flags it has.
            // A process running something in the background keeps its flags too: replacing
            // it would leave that running where nothing reports on it.
            let busy = session.is_busy() || session.has_background();
            if (busy || (*session.config() == config && session.suggests() == suggest))
                && let Ok(turn) = session.send(message)
            {
                return Ok(turn);
            }
            if busy {
                return Err(SendError::Failed(
                    "claude stopped taking messages while still working".to_string(),
                ));
            }
        }
        // An idle process with other flags holds the conversation's lock until it has exited.
        if let Some(replaced) = sessions.remove(conversation_id) {
            drop(sessions);
            replaced.shutdown(Duration::from_secs(10));
            sessions = self.sessions.lock().unwrap();
        }

        let held = AgentLock::take(conversation_id)
            .map_err(|_| SendError::Occupied)?
            .held(conversation_id);
        let incoming = self.incoming_for(conversation_id);
        let mut session = Session::spawn_holding(config, resume, name, incoming, held, suggest)
            .map_err(|e| SendError::Failed(format!("failed to launch `{}`: {e}", claude_bin())))?;
        for scope in always {
            session.allow_always(scope);
        }
        if self.remote_control.load(Ordering::Relaxed) {
            let _ = session.remote_control(true, name);
        }
        let turn = session.send(message).map_err(|e| {
            SendError::Failed(format!("claude exited before the turn was sent: {e}"))
        })?;
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
    /// what `progress` knew of its conversation's running turn, and the messages `waiting`
    /// for another app's agent. Returns only if that failed, and then everything goes on as
    /// it was.
    pub fn exec(
        &self,
        exe: &Path,
        progress: impl Fn(&str) -> Progress,
        waiting: Vec<Waited>,
    ) -> io::Error {
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
        crate::handover::exec(exe, handed, waiting)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_conversation_has_one_agent_until_it_lets_go() {
        let conversation = format!("lock-test-{}", uuid::Uuid::new_v4());
        let Ok(AgentLock(Some(mut held))) = AgentLock::take(&conversation) else {
            panic!("a free conversation's lock");
        };
        write!(held, "4242").unwrap();
        assert_eq!(
            AgentLock::take(&conversation).err(),
            Some(4242),
            "the holder is named"
        );
        drop(held);
        assert!(AgentLock::take(&conversation).is_ok(), "free once let go");
        let _ = std::fs::remove_file(AgentLock::path(&conversation));
    }

    #[test]
    fn a_wait_hears_when_the_holder_sits_idle_and_ends_when_it_lets_go() {
        let conversation = format!("lock-test-{}", uuid::Uuid::new_v4());
        let Ok(AgentLock(Some(mut held))) = AgentLock::take(&conversation) else {
            panic!("a free conversation's lock");
        };
        write!(held, "4242").unwrap();
        let (heard, hear) = mpsc::channel();
        let waiter = {
            let conversation = conversation.clone();
            thread::spawn(move || {
                AgentLock::wait(&conversation, |pid, idle| heard.send((pid, idle)).unwrap())
            })
        };
        let next = || hear.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(next(), (4242, false));
        File::create(AgentLock::idle_path(&conversation)).unwrap();
        assert_eq!(next(), (4242, true));
        drop(held);
        waiter.join().unwrap();
        assert!(
            AgentLock::take(&conversation).is_ok(),
            "free once the waiter is done"
        );
        assert!(
            !AgentLock::idle_path(&conversation).exists(),
            "the next agent starts with no idle file left over"
        );
        let _ = std::fs::remove_file(AgentLock::path(&conversation));
    }
}
