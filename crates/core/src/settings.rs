//! Persisted global preferences (`~/.local/share/claudo/settings.json`): the
//! default model, thinking level, and permission profile applied to every turn.

use std::fs;
use std::path::PathBuf;

use claudo_protocol::Settings;

use crate::sessions::data_root;

fn path() -> PathBuf {
    data_root().join("settings.json")
}

pub fn load() -> Settings {
    fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(settings: &Settings) {
    if let Some(parent) = path().parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = fs::write(path(), json);
    }
}
