use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::usage::Usage;

/// For each window, by its key, the reset of the last one warned of, so a window warns once.
#[derive(Debug, Default)]
pub struct Warned {
    resets: BTreeMap<String, u64>,
    /// The panel of each display runs an applet of its own, and they keep the resets
    /// here, so only the first to see a window ahead of its pace warns.
    shared: Option<PathBuf>,
}

#[cfg_attr(not(feature = "cosmic"), allow(dead_code))]
impl Warned {
    /// Shared by the applets showing one tool, whose files start with `files`.
    pub fn shared(files: &str) -> Self {
        Self::sharing(runtime_dir().join(format!("{files}-warned")))
    }

    fn sharing(path: PathBuf) -> Self {
        Self {
            resets: BTreeMap::new(),
            shared: Some(path),
        }
    }

    /// The windows that would run out before their reset and have not warned yet: each
    /// label with how long until it runs out at the pace so far.
    pub fn fresh(&mut self, usage: &Usage, now: u64) -> Vec<(String, u64)> {
        // Held until the resets are written back. Without the file, this applet warns alone.
        let file = self.shared.as_deref().and_then(|path| locked(path).ok());
        if let Some(file) = &file {
            self.take_in(file);
        }
        let fresh: Vec<_> = usage
            .meters
            .iter()
            .filter_map(|meter| {
                let out_in = meter.standing(now).out_in?;
                let warned = self.resets.get(&meter.key());
                if warned.is_some_and(|&reset| now < reset) {
                    return None;
                }
                self.resets.insert(meter.key(), meter.window.resets_at);
                Some((meter.label(), out_in))
            })
            .collect();
        if let Some(file) = &file
            && !fresh.is_empty()
        {
            let _ = self.write(file);
        }
        fresh
    }

    fn take_in(&mut self, mut file: &File) {
        let mut text = String::new();
        let _ = file.read_to_string(&mut text);
        let theirs = text.split_whitespace().filter_map(|pair| {
            let (key, reset) = pair.rsplit_once(':')?;
            Some((key.to_string(), reset.parse().ok()?))
        });
        for (key, reset) in theirs {
            let ours = self.resets.entry(key).or_insert(reset);
            *ours = (*ours).max(reset);
        }
    }

    fn write(&self, mut file: &File) -> io::Result<()> {
        let text = self
            .resets
            .iter()
            .map(|(key, reset)| format!("{key}:{reset}"))
            .collect::<Vec<_>>()
            .join(" ");
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(text.as_bytes())
    }
}

pub(crate) fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

pub(crate) fn locked(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.lock()?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::Meter;
    use claudo_protocol::QuotaWindow;

    const HOUR: u64 = 3600;

    fn session(used: f32, resets_at: u64) -> Usage {
        Usage {
            meters: vec![Meter {
                length: 5 * HOUR,
                window: QuotaWindow { used, resets_at },
                model: None,
            }],
            measured_at: None,
        }
    }

    #[test]
    fn a_window_ahead_of_its_pace_warns_once_until_it_resets() {
        let mut warned = Warned::default();
        let now = 1_000_000;
        let reset = now + 4 * HOUR;
        assert_eq!(
            warned.fresh(&session(0.5, reset), now),
            vec![("Session".to_string(), HOUR)]
        );
        assert!(warned.fresh(&session(0.6, reset + 1), now + 60).is_empty());
        let next = reset + 5 * HOUR;
        assert!(
            warned.fresh(&session(0.4, next), reset).is_empty(),
            "just started"
        );
        assert_eq!(
            warned.fresh(&session(0.9, next), reset + HOUR).len(),
            1,
            "the next window warns again"
        );
    }

    #[test]
    fn applets_sharing_a_file_warn_once_between_them() {
        let path = std::env::temp_dir().join(format!("claudo-warned-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut panels = [Warned::sharing(path.clone()), Warned::sharing(path.clone())];
        let now = 1_000_000;
        let reset = now + 4 * HOUR;
        assert_eq!(panels[0].fresh(&session(0.5, reset), now).len(), 1);
        assert!(panels[1].fresh(&session(0.5, reset + 1), now).is_empty());
        let restarted = Warned::sharing(path.clone()).fresh(&session(0.6, reset), now + 60);
        assert!(restarted.is_empty(), "a restarted applet has warned too");
        let next = reset + 5 * HOUR;
        assert_eq!(panels[1].fresh(&session(0.9, next), reset + HOUR).len(), 1);
        assert!(
            panels[0]
                .fresh(&session(0.9, next), reset + HOUR)
                .is_empty()
        );
        let _ = std::fs::remove_file(&path);
    }
}
