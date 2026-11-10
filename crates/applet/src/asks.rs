use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::usage::Usage;
use crate::warned::{locked, runtime_dir};

/// Claude's usage endpoint answers every Claude Code process of the account, and refuses them
/// all for a while once asked too often; the CLI then answers from its saved snapshot,
/// up to an hour old, and its own wait before asking again is this long.
const REFUSED_FOR: u64 = 300;

/// The Claude CLI answers from a snapshot younger than this without asking the endpoint,
/// so an older one means the endpoint refused.
const SNAPSHOT_AGE: u64 = 60;

/// The panel of each display runs an applet of its own. They ask through this file, so
/// the tool is asked once between them, and not at all while its endpoint refuses.
#[derive(Debug, Clone)]
pub struct Asks {
    path: Option<PathBuf>,
    every: u64,
}

#[derive(Serialize, Deserialize)]
struct Last {
    usage: Usage,
    /// Unix seconds before which the next applet takes this answer rather than asking.
    next_at: u64,
}

#[cfg_attr(not(feature = "cosmic"), allow(dead_code))]
impl Asks {
    /// Shared by the applets showing one tool, whose files start with `files`.
    pub fn shared(files: &str, every: u64) -> Self {
        Self::sharing(runtime_dir().join(format!("{files}-usage")), every)
    }

    fn sharing(path: PathBuf, every: u64) -> Self {
        Self {
            path: Some(path),
            every,
        }
    }

    /// The usage another applet got recently, or a new ask's. `force` asks anyway, as
    /// the popup's refresh button does.
    pub fn usage<E>(
        &self,
        force: bool,
        ask: impl FnOnce() -> Result<Usage, E>,
    ) -> Result<Usage, E> {
        // Held across the ask, so an applet waiting on it takes its answer.
        let file = self.path.as_deref().and_then(|path| locked(path).ok());
        let now = now();
        if !force
            && let Some(last) = file.as_ref().and_then(read)
            && now < last.next_at
        {
            return Ok(last.usage);
        }
        let usage = ask()?;
        let refused = usage.measured_at.is_some_and(|at| at + SNAPSHOT_AGE < now);
        let next_at = now
            + if refused {
                REFUSED_FOR
            } else {
                self.every * 3 / 4
            };
        if let Some(file) = &file {
            let _ = write(
                file,
                &Last {
                    usage: usage.clone(),
                    next_at,
                },
            );
        }
        Ok(usage)
    }
}

fn read(mut file: &File) -> Option<Last> {
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    serde_json::from_str(&text).ok()
}

fn write(mut file: &File, last: &Last) -> io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&serde_json::to_vec(last)?)
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::Meter;
    use claudo_protocol::QuotaWindow;

    fn measured(at: u64) -> Usage {
        Usage {
            meters: vec![Meter {
                length: 5 * 3600,
                window: QuotaWindow {
                    used: 0.11,
                    resets_at: at + 3600,
                },
            }],
            measured_at: Some(at),
        }
    }

    fn unasked() -> io::Result<Usage> {
        panic!("asked the CLI")
    }

    fn sharing(name: &str) -> (PathBuf, [Asks; 2]) {
        let path = std::env::temp_dir().join(format!("claudo-asks-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let asks = Asks::sharing(path.clone(), 120);
        (path, [asks.clone(), asks])
    }

    #[test]
    fn applets_sharing_a_file_ask_once_between_them() {
        let (path, [first, second]) = sharing("once");
        let usage = first
            .usage(false, || io::Result::Ok(measured(now())))
            .unwrap();
        assert_eq!(
            second.usage(false, unasked).unwrap().measured_at,
            usage.measured_at
        );
        assert!(
            second
                .usage(true, || io::Result::Ok(measured(now())))
                .is_ok(),
            "a forced ask asks"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_refused_ask_holds_every_applet_back() {
        let (path, [first, second]) = sharing("refused");
        let snapshot = measured(now() - 20 * 60);
        let measured_at = snapshot.measured_at;
        first.usage(false, || io::Result::Ok(snapshot)).unwrap();
        let held = read(&File::open(&path).unwrap()).unwrap().next_at;
        assert!(
            held >= now() + REFUSED_FOR - 1,
            "held for the endpoint's refusal"
        );
        assert_eq!(
            second.usage(false, unasked).unwrap().measured_at,
            measured_at
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_failed_ask_leaves_the_next_applet_to_ask() {
        let (path, [first, second]) = sharing("failed");
        assert!(
            first
                .usage(false, || io::Result::Err(io::ErrorKind::Other.into()))
                .is_err()
        );
        assert!(
            second
                .usage(false, || io::Result::Ok(measured(now())))
                .is_ok()
        );
        let _ = std::fs::remove_file(&path);
    }
}
