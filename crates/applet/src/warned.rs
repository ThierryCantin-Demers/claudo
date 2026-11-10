use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use claudo_protocol::Quota;

/// For each window, the reset of the last one warned of, so a window warns once.
#[derive(Debug, Default)]
pub struct Warned {
    resets: [Option<u64>; 2],
    /// The panel of each display runs an applet of its own, and they keep the resets
    /// here, so only the first to see a window ahead of its pace warns.
    shared: Option<PathBuf>,
}

#[cfg_attr(not(feature = "cosmic"), allow(dead_code))]
impl Warned {
    pub fn shared() -> Self {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        Self::sharing(dir.join("claudo-applet-warned"))
    }

    fn sharing(path: PathBuf) -> Self {
        Self {
            resets: [None; 2],
            shared: Some(path),
        }
    }

    /// The windows that would run out before their reset and have not warned yet: each
    /// label with how long until it runs out at the pace so far.
    pub fn fresh(&mut self, quota: Quota, now: u64) -> Vec<(&'static str, u64)> {
        // Held until the resets are written back. Without the file, this applet warns alone.
        let file = self.shared.as_deref().and_then(|path| locked(path).ok());
        if let Some(file) = &file {
            self.take_in(file);
        }
        let fresh: Vec<_> = quota
            .windows()
            .into_iter()
            .zip(&mut self.resets)
            .filter_map(|((label, window, length), warned)| {
                let window = window?;
                let out_in = window.standing(length, now).out_in?;
                if warned.is_some_and(|reset| now < reset) {
                    return None;
                }
                *warned = Some(window.resets_at);
                Some((label, out_in))
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
        let mut theirs = text.split_whitespace().map(|reset| reset.parse().ok());
        for reset in &mut self.resets {
            *reset = (*reset).max(theirs.next().flatten());
        }
    }

    fn write(&self, mut file: &File) -> io::Result<()> {
        let text = self
            .resets
            .map(|reset| reset.map_or("-".to_string(), |reset| reset.to_string()))
            .join(" ");
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(text.as_bytes())
    }
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
    use claudo_protocol::QuotaWindow;

    const HOUR: u64 = 3600;

    fn session(used: f32, resets_at: u64) -> Quota {
        Quota {
            five_hour: Some(QuotaWindow { used, resets_at }),
            seven_day: None,
            measured_at: None,
        }
    }

    #[test]
    fn a_window_ahead_of_its_pace_warns_once_until_it_resets() {
        let mut warned = Warned::default();
        let now = 1_000_000;
        let reset = now + 4 * HOUR;
        assert_eq!(
            warned.fresh(session(0.5, reset), now),
            vec![("Session", HOUR)]
        );
        assert!(warned.fresh(session(0.6, reset + 1), now + 60).is_empty());
        let next = reset + 5 * HOUR;
        assert!(
            warned.fresh(session(0.4, next), reset).is_empty(),
            "just started"
        );
        assert_eq!(
            warned.fresh(session(0.9, next), reset + HOUR).len(),
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
        assert_eq!(panels[0].fresh(session(0.5, reset), now).len(), 1);
        assert!(panels[1].fresh(session(0.5, reset + 1), now).is_empty());
        let restarted = Warned::sharing(path.clone()).fresh(session(0.6, reset), now + 60);
        assert!(restarted.is_empty(), "a restarted applet has warned too");
        let next = reset + 5 * HOUR;
        assert_eq!(panels[1].fresh(session(0.9, next), reset + HOUR).len(), 1);
        assert!(panels[0].fresh(session(0.9, next), reset + HOUR).is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
