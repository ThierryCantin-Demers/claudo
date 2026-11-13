//! The messages the person sends agents often, from
//! `$XDG_CONFIG_HOME/claudo/quick-prompts.json`: a list of `{"label": ..., "text": ...}`,
//! written with a few to start from when it is missing.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use claudo_protocol::QuickPrompt;

use crate::room::Feed;

/// How often the file is looked at, so an edit shows without a restart.
const POLL: Duration = Duration::from_secs(5);

const STARTERS: [(&str, &str); 3] = [
    (
        "Quick review",
        "Have your changes reviewed quickly, fix what is real, and tell me what is left.",
    ),
    (
        "Deep review",
        "Have your changes reviewed in depth, fix what is real, and tell me what is left.",
    ),
    ("Draft PR", "Open a draft pull request for this work."),
];

/// Reads the file into `feed` whenever it changes, for as long as the process runs.
pub fn spawn(feed: Feed) {
    let Some(path) = path() else {
        return;
    };
    if !path.exists() {
        write_starters(&path);
    }
    std::thread::spawn(move || {
        let mut seen: Option<SystemTime> = None;
        loop {
            let changed = std::fs::metadata(&path).and_then(|meta| meta.modified()).ok();
            if changed != seen {
                seen = changed;
                feed.quick_prompts(read(&path));
            }
            std::thread::sleep(POLL);
        }
    });
}

fn read(path: &PathBuf) -> Vec<QuickPrompt> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    match serde_json::from_str(&text) {
        Ok(prompts) => prompts,
        Err(error) => {
            eprintln!("claudo: cannot read {}: {error}", path.display());
            Vec::new()
        }
    }
}

fn write_starters(path: &PathBuf) {
    let starters: Vec<QuickPrompt> = STARTERS
        .iter()
        .map(|(label, text)| QuickPrompt {
            label: label.to_string(),
            text: text.to_string(),
        })
        .collect();
    let text = serde_json::to_string_pretty(&starters).expect("plain data");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, text + "\n");
}

fn path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|dir| dir.join("claudo/quick-prompts.json"))
}
