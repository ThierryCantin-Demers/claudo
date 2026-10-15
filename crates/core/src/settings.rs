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
    let mut settings: Settings = fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    follow_latest(&mut settings.model);
    settings
}

/// Versions claudo itself once saved, now the aliases that follow each family's
/// newest; a version the user typed stays as it is.
const FORMER_PRESETS: [(&str, &str); 4] = [
    ("claude-opus-5-5", "opus"),
    ("claude-sonnet-5-5", "sonnet"),
    ("claude-haiku-5-5", "haiku"),
    ("claude-fable-5-1", "fable"),
];

pub(crate) fn follow_latest(model: &mut String) {
    if let Some((_, alias)) = FORMER_PRESETS.iter().find(|(id, _)| id == model) {
        *model = alias.to_string();
    }
}

pub fn save(settings: &Settings) {
    if let Some(parent) = path().parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = fs::write(path(), json);
    }
}
