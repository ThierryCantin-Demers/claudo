//! What agents noted about themselves on the board: a file per session, named by its id,
//! under `sessions` in `$BOARD_DIR` (default `~/.claude/projects/board`), one JSON line per
//! note with its `state` and `text`; the last line stands. All of its shapes stay in this file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use claudo_protocol::{Node, Note};
use serde_json::Value;

use crate::room::Feed;

/// How often the files are looked at. A file is read again only once it changed.
const POLL: Duration = Duration::from_secs(3);

/// Reads the board into `feed`, a collector of notes, for as long as the process runs.
pub fn spawn(feed: Feed) {
    let Some(sessions) = root().map(|root| root.join("sessions")) else {
        return;
    };
    std::thread::spawn(move || {
        let mut board = Board::default();
        loop {
            feed.replace(board.scan(&sessions));
            std::thread::sleep(POLL);
        }
    });
}

fn root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("BOARD_DIR") {
        return Some(dir.into());
    }
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".claude/projects/board"))
}

/// Each session's last note as last read, with when its file was written.
#[derive(Default)]
struct Board {
    read: HashMap<PathBuf, (SystemTime, Option<Node>)>,
}

impl Board {
    fn scan(&mut self, sessions: &Path) -> Vec<Node> {
        let Ok(files) = std::fs::read_dir(sessions) else {
            self.read.clear();
            return Vec::new();
        };
        let mut read = HashMap::new();
        for file in files.flatten() {
            let path = file.path();
            let Some(session) = path
                .file_stem()
                .filter(|_| path.extension().is_some_and(|ext| ext == "jsonl"))
            else {
                continue;
            };
            let Ok(written) = file.metadata().and_then(|meta| meta.modified()) else {
                continue;
            };
            let node = match self.read.remove(&path) {
                Some((at, node)) if at == written => node,
                _ => std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| noted(&session.to_string_lossy(), &text)),
            };
            read.insert(path, (written, node));
        }
        self.read = read;
        self.read
            .values()
            .filter_map(|(_, node)| node.clone())
            .collect()
    }
}

fn noted(session: &str, text: &str) -> Option<Node> {
    let last = text.lines().rev().find(|line| !line.trim().is_empty())?;
    let note: Value = serde_json::from_str(last).ok()?;
    let field = |key: &str| note.get(key).and_then(Value::as_str);
    Some(Node {
        id: format!("session:{session}"),
        note: Some(Note {
            waiting: field("state") == Some("waiting"),
            text: field("text").unwrap_or_default().to_string(),
        }),
        ..Node::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_session_shows_its_last_note() {
        let sessions = std::env::temp_dir().join(format!("claudo-board-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sessions);
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("s1.jsonl"),
            concat!(
                r#"{"state": "working", "text": "porting the parser"}"#,
                "\n",
                r#"{"state": "waiting", "text": "needs an answer: keep the old signature?"}"#,
                "\n",
            ),
        )
        .unwrap();
        std::fs::write(
            sessions.join("s2.jsonl"),
            r#"{"state": "blocked", "text": "needs a merge"}"#,
        )
        .unwrap();
        std::fs::write(sessions.join("notes.txt"), "not a session").unwrap();
        let mut nodes = Board::default().scan(&sessions);
        nodes.sort_by(|a, b| a.id.cmp(&b.id));
        let notes: Vec<_> = nodes
            .iter()
            .map(|node| (node.id.as_str(), node.note.clone().unwrap()))
            .collect();
        assert_eq!(
            notes,
            [
                (
                    "session:s1",
                    Note {
                        waiting: true,
                        text: "needs an answer: keep the old signature?".into()
                    }
                ),
                (
                    "session:s2",
                    Note {
                        waiting: false,
                        text: "needs a merge".into()
                    }
                ),
            ]
        );
        std::fs::remove_dir_all(sessions).unwrap();
    }
}
