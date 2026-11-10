use claudo_core::engine::cli;
use claudo_protocol::{Quota, QuotaWindow, Standing};
use serde::{Deserialize, Serialize};

use crate::codex;

/// A plan's usage windows, from whichever tool reports them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub meters: Vec<Meter>,
    /// When the server measured them, in Unix seconds.
    pub measured_at: Option<u64>,
}

/// One window, which plans size differently: Claude's run five hours and a week, a Codex
/// plan's whatever its server says.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Meter {
    /// In seconds.
    pub length: u64,
    pub window: QuotaWindow,
}

impl Meter {
    pub fn standing(self, now: u64) -> Standing {
        self.window.standing(self.length, now)
    }

    /// What the panel calls it: "5h", "7d".
    pub fn short(self) -> String {
        let hours = self.length / 3600;
        match (hours % 24, hours) {
            (_, 0) => format!("{}m", self.length.div_ceil(60)),
            (0, _) => format!("{}d", hours / 24),
            _ => format!("{hours}h"),
        }
    }

    /// What the popup calls it.
    pub fn label(self) -> String {
        match self.length / 86_400 {
            0 if self.length == 5 * 3600 => "Session".to_string(),
            7 => "Week".to_string(),
            28..=31 => "Month".to_string(),
            _ => format!("{} window", self.short()),
        }
    }
}

impl From<Quota> for Usage {
    fn from(quota: Quota) -> Self {
        Usage {
            meters: quota
                .windows()
                .into_iter()
                .filter_map(|(_, window, length)| {
                    Some(Meter {
                        length,
                        window: window?,
                    })
                })
                .collect(),
            measured_at: quota.measured_at,
        }
    }
}

/// Why the last ask failed.
#[derive(Debug, Clone)]
pub enum Problem {
    NotInstalled,
    LoggedOut,
    Failed(String),
}

impl Problem {
    /// What the panel shows in place of the usage.
    pub fn brief(&self) -> Option<&'static str> {
        match self {
            Problem::NotInstalled => Some("not found"),
            Problem::LoggedOut => Some("log in"),
            Problem::Failed(_) => None,
        }
    }
}

/// A tool whose plan the applet shows, one applet each.
pub trait Source: 'static {
    const APP_ID: &'static str;
    const NAME: &'static str;
    /// Its applets' shared files in `$XDG_RUNTIME_DIR` start with this.
    const FILES: &'static str;
    /// The popup's heading and help for a missing command, then for a logged out one.
    const NOT_INSTALLED: (&'static str, &'static str);
    const LOGGED_OUT: (&'static str, &'static str);
    /// Icon names its apps install, the first found shown in the panel.
    const ICONS: &'static [&'static str];

    fn usage() -> Result<Usage, Problem>;
}

pub struct Claude;

impl Source for Claude {
    const APP_ID: &'static str = "com.claudo.Usage";
    const NAME: &'static str = "Claude";
    const FILES: &'static str = "claudo-applet";
    const NOT_INSTALLED: (&'static str, &'static str) = (
        "Claude Code not found",
        "Install the `claude` command and log in with `claude auth login`.",
    );
    const LOGGED_OUT: (&'static str, &'static str) = (
        "Not logged in to Claude",
        "Run `claude auth login` in a terminal, then refresh.",
    );
    const ICONS: &'static [&'static str] = &[
        "claude",
        "claude-desktop",
        "com.anthropic.Claude",
        "claude-desktop-unofficial",
    ];

    fn usage() -> Result<Usage, Problem> {
        cli::usage().map(Usage::from).map_err(|error| {
            // Only a failed ask checks the login, which costs another process.
            let system = claudo_core::detect_system();
            if !system.claude_installed {
                Problem::NotInstalled
            } else if !system.auth.logged_in {
                Problem::LoggedOut
            } else {
                Problem::Failed(error.to_string())
            }
        })
    }
}

pub struct Codex;

impl Source for Codex {
    const APP_ID: &'static str = "com.claudo.CodexUsage";
    const NAME: &'static str = "Codex";
    const FILES: &'static str = "claudo-applet-codex";
    const NOT_INSTALLED: (&'static str, &'static str) = (
        "Codex not found",
        "Install the `codex` command, or the ChatGPT app, and log in with `codex login`.",
    );
    const LOGGED_OUT: (&'static str, &'static str) = (
        "Not logged in to Codex",
        "Run `codex login` in a terminal, then refresh.",
    );
    const ICONS: &'static [&'static str] = &["codex", "com.openai.codex", "chatgpt"];

    fn usage() -> Result<Usage, Problem> {
        codex::usage()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meter(length: u64) -> Meter {
        Meter {
            length,
            window: QuotaWindow {
                used: 0.0,
                resets_at: 0,
            },
        }
    }

    #[test]
    fn windows_are_named_by_their_length() {
        let names = [5 * 3600, 7 * 86_400, 30 * 86_400, 2 * 3600, 3 * 86_400]
            .map(|length| (meter(length).short(), meter(length).label()));
        assert_eq!(
            names.map(|(short, label)| format!("{short} {label}")),
            [
                "5h Session",
                "7d Week",
                "30d Month",
                "2h 2h window",
                "3d 3d window"
            ]
        );
    }
}
