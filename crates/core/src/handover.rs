//! Replacing the running app with a new build of it without stopping its agents. The app
//! execs the new binary in its own process, so every `claude` child keeps its pid and its
//! pipes, and a file tells the new build what each one was doing.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::fd::RawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::engine::cli::{SessionState, set_cloexec, wait};

/// Names the handover file.
pub const FILE_VAR: &str = "CLAUDO_HANDOVER";
/// Every inherited process with its descriptors, `pid:fd,fd,fd` separated by `;`, in a form
/// any build reads: one that cannot read the file still closes them and reaps the processes.
pub const KEPT_VAR: &str = "CLAUDO_HANDOVER_KEPT";

const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub struct Handover {
    version: u32,
    pub sessions: Vec<Handed>,
    /// The replaced app's other children, such as an agent shutting down, which only the
    /// new build can reap.
    pub reap: Vec<u32>,
    #[serde(default)]
    pub waiting: Vec<Waited>,
}

/// Messages to a conversation another app's agent holds, oldest first.
#[derive(Serialize, Deserialize)]
pub struct Waited {
    pub conversation: String,
    pub messages: Vec<crate::backend::Outgoing>,
}

#[derive(Serialize, Deserialize)]
pub struct Handed {
    pub conversation: String,
    pub state: SessionState,
    pub progress: Progress,
}

/// What the backend knew of a conversation's running turn.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Progress {
    /// In Unix seconds.
    pub started: Option<u64>,
    /// Text streamed, so the turn's result does not repeat it.
    pub streamed: bool,
}

/// The binary installed where this one was started from, even once a new build replaced it.
pub fn installed() -> Option<PathBuf> {
    let exe = std::fs::read_link("/proc/self/exe").ok()?;
    let exe = exe.to_string_lossy();
    Some(PathBuf::from(
        exe.strip_suffix(" (deleted)").unwrap_or(&exe),
    ))
}

/// Replaces this process with `exe`, handing `sessions` and the messages `waiting` for
/// other apps' agents over to it. Returns only if that failed, and then nothing changed.
pub(crate) fn exec(exe: &Path, sessions: Vec<Handed>, waiting: Vec<Waited>) -> io::Error {
    let handed: HashSet<u32> = sessions.iter().map(|h| h.state.pid()).collect();
    let reap = children()
        .into_iter()
        .filter(|pid| !handed.contains(pid))
        .collect();
    let handover = Handover {
        version: VERSION,
        sessions,
        reap,
        waiting,
    };
    let file = path();
    if let Err(e) = write(&file, &handover) {
        return e;
    }
    let fds: Vec<RawFd> = handover
        .sessions
        .iter()
        .flat_map(|h| h.state.fds())
        .collect();
    for &fd in &fds {
        set_cloexec(fd, false);
    }
    let error = Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env(FILE_VAR, &file)
        .env(KEPT_VAR, kept(&handover))
        .exec();
    for &fd in &fds {
        set_cloexec(fd, true);
    }
    let _ = std::fs::remove_file(&file);
    error
}

static TAKEN: AtomicBool = AtomicBool::new(false);

/// The handover this process was started with, the first time it is asked for. One this
/// build cannot read has its agents' pipes closed, so each finishes its turn on its own.
pub fn take() -> Option<Handover> {
    if TAKEN.swap(true, Ordering::SeqCst) {
        return None;
    }
    let file = std::env::var_os(FILE_VAR)?;
    let handover = std::fs::read(&file)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Handover>(&bytes).ok())
        .filter(|handover| handover.version == VERSION);
    let _ = std::fs::remove_file(&file);
    if handover.is_none() {
        release(&std::env::var(KEPT_VAR).unwrap_or_default());
    }
    handover
}

/// Reaps each of `pids` once it exits.
pub fn reap(pids: impl IntoIterator<Item = u32>) {
    for pid in pids {
        std::thread::spawn(move || {
            let _ = wait(pid);
        });
    }
}

fn path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("claudo-handover-{}.json", std::process::id()))
}

fn write(file: &Path, handover: &Handover) -> io::Result<()> {
    let mut out = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(file)?;
    out.write_all(&serde_json::to_vec(handover)?)?;
    out.sync_all()
}

fn kept(handover: &Handover) -> String {
    let sessions = handover.sessions.iter().map(|h| {
        let fds = h.state.fds().map(|fd| fd.to_string()).join(",");
        format!("{}:{fds}", h.state.pid())
    });
    let others = handover.reap.iter().map(|pid| format!("{pid}:"));
    sessions.chain(others).collect::<Vec<_>>().join(";")
}

/// Closes every descriptor `kept` lists and reaps its processes.
fn release(kept: &str) {
    let kept = parse_kept(kept);
    for fd in kept.values().flatten() {
        // SAFETY: inherited for an agent this build could not take over; nothing else here
        // owns it.
        unsafe { libc::close(*fd) };
    }
    reap(kept.into_keys());
}

fn parse_kept(kept: &str) -> HashMap<u32, Vec<RawFd>> {
    kept.split(';')
        .filter_map(|entry| {
            let (pid, fds) = entry.split_once(':')?;
            let fds = fds.split(',').filter_map(|fd| fd.parse().ok()).collect();
            Some((pid.parse().ok()?, fds))
        })
        .collect()
}

/// This process's children, from every one of its threads.
fn children() -> Vec<u32> {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    tasks
        .flatten()
        .filter_map(|task| std::fs::read_to_string(task.path().join("children")).ok())
        .flat_map(|children| {
            children
                .split_whitespace()
                .filter_map(|pid| pid.parse().ok())
                .collect::<Vec<u32>>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_that_cannot_read_the_file_still_finds_every_process() {
        let kept = parse_kept("12:3,4,5;40:6,7,8;41:");
        assert_eq!(kept[&12], [3, 4, 5]);
        assert_eq!(kept[&40], [6, 7, 8]);
        assert!(
            kept[&41].is_empty(),
            "a child to reap, with nothing to close"
        );
        assert!(parse_kept("").is_empty());
    }
}
