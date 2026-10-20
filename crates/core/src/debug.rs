//! While the debug setting is on, what claudo does with each agent goes to `claudo.log`,
//! and each agent started meanwhile writes the CLI's own debug log beside it.

use std::fmt::Display;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

static ON: AtomicBool = AtomicBool::new(false);

pub fn set(on: bool) {
    if ON.swap(on, Ordering::Relaxed) != on {
        log("", if on { "debug log on" } else { "debug log off" });
    }
}

/// `$XDG_STATE_HOME/claudo/debug`.
pub fn dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir)
        .join("claudo/debug")
}

/// One line about `conversation`, or about none when it is empty.
pub fn log(conversation: &str, what: impl Display) {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    let dir = dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("claudo.log"))
    {
        let short = conversation.get(..8).unwrap_or(conversation);
        let _ = writeln!(file, "{} {short:8} {what}", timestamp(""));
    }
}

/// Where a new agent for `conversation` writes the CLI's debug log, while the setting is on.
pub fn cli_file(conversation: &str) -> Option<PathBuf> {
    ON.load(Ordering::Relaxed)
        .then(|| dir().join(format!("cli-{conversation}-{}.log", timestamp("-"))))
}

/// The time in UTC, as the CLI's own logs write it; `sep` stands between date and time
/// fields where a file name cannot have `:`.
fn timestamp(sep: &str) -> String {
    let secs = crate::sessions::now_secs() as libc::time_t;
    // SAFETY: `gmtime_r` writes only the `tm` it is given.
    let tm = unsafe {
        let mut tm = std::mem::zeroed::<libc::tm>();
        libc::gmtime_r(&secs, &mut tm);
        tm
    };
    let colon = if sep.is_empty() { ":" } else { sep };
    format!(
        "{:04}-{:02}-{:02}T{:02}{colon}{:02}{colon}{:02}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}
