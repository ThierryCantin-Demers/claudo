//! How full each Claude agent's context is, from its transcript's last request of the main
//! agent. A note on `session:<id>`, so only an agent another collector shows gets one.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use claudo_protocol::Node;

use crate::room::Feed;

/// How often the transcripts are looked at. One is read again only once it changed.
const POLL: Duration = Duration::from_secs(10);

/// A transcript untouched for longer belongs to no running agent.
const RECENT: Duration = Duration::from_secs(12 * 60 * 60);

/// How much of a transcript's end is read for its last request, which a long tool result
/// or a long reply can push far from the end.
const TAIL: u64 = 1 << 20;

/// Reads the transcripts into `feed`, a collector of notes, for as long as the process runs.
pub fn spawn(feed: Feed) {
    let Some(projects) = crate::sessions::claude_projects_dir() else {
        return;
    };
    std::thread::spawn(move || {
        let mut transcripts = Transcripts::default();
        loop {
            feed.replace(transcripts.scan(&projects, SystemTime::now()));
            std::thread::sleep(POLL);
        }
    });
}

/// Each recent transcript's context as last read, with when it was written.
#[derive(Default)]
struct Transcripts {
    read: HashMap<PathBuf, (SystemTime, Option<u64>)>,
}

impl Transcripts {
    fn scan(&mut self, projects: &Path, now: SystemTime) -> Vec<Node> {
        let mut read = HashMap::new();
        // A session that moved leaves a copy behind; the newest is the live one.
        let mut sessions: HashMap<String, (SystemTime, Option<u64>)> = HashMap::new();
        let files = std::fs::read_dir(projects)
            .into_iter()
            .flatten()
            .flatten()
            .flat_map(|project| {
                std::fs::read_dir(project.path())
                    .into_iter()
                    .flatten()
                    .flatten()
            });
        for file in files {
            let path = file.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            let Ok(written) = file.metadata().and_then(|meta| meta.modified()) else {
                continue;
            };
            if now.duration_since(written).unwrap_or_default() > RECENT {
                continue;
            }
            let context = match self.read.remove(&path) {
                Some((at, context)) if at == written => context,
                _ => last_context(&path),
            };
            read.insert(path.clone(), (written, context));
            let Some(session) = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
            else {
                continue;
            };
            let newest = sessions.get(&session).is_none_or(|(at, _)| *at < written);
            if newest {
                sessions.insert(session, (written, context));
            }
        }
        self.read = read;
        sessions
            .into_iter()
            .filter_map(|(session, (_, context))| {
                Some(Node {
                    id: format!("session:{session}"),
                    context: Some(context?),
                    ..Node::default()
                })
            })
            .collect()
    }
}

fn last_context(path: &Path) -> Option<u64> {
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    // A read from the middle starts partway through a line.
    let whole = if start > 0 {
        text.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        &text
    };
    whole
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str(line).ok())
        .find_map(|line| crate::engine::cli::request_context(&line))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agents_context_is_its_last_main_request() {
        let projects =
            std::env::temp_dir().join(format!("claudo-transcripts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&projects);
        std::fs::create_dir_all(projects.join("-src-alpha")).unwrap();
        let usage = |tokens: u64| {
            format!(
                r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":{tokens},"cache_read_input_tokens":1000,"cache_creation_input_tokens":0,"output_tokens":0}}}}}}"#
            )
        };
        let transcript = [
            usage(5),
            usage(7),
            r#"{"type":"user","message":{"content":"next"}}"#.to_string(),
        ]
        .join("\n");
        std::fs::write(projects.join("-src-alpha/s1.jsonl"), transcript).unwrap();
        std::fs::write(
            projects.join("-src-alpha/s2.jsonl"),
            r#"{"type":"user","message":{"content":"first"}}"#,
        )
        .unwrap();
        let nodes = Transcripts::default().scan(&projects, SystemTime::now());
        let shown: Vec<_> = nodes
            .iter()
            .map(|node| (node.id.as_str(), node.context))
            .collect();
        assert_eq!(
            shown,
            [("session:s1", Some(1007))],
            "a session yet to ask has none"
        );
        let later = SystemTime::now() + RECENT + Duration::from_secs(60);
        assert!(Transcripts::default().scan(&projects, later).is_empty());
        std::fs::remove_dir_all(projects).unwrap();
    }
}
