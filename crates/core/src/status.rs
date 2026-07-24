//! Detect the local Claude Code CLI and its login state via the CLI's own
//! `--version` and `auth status --json` commands. No credential files are read.

use std::process::Command;

use claudo_protocol::{AuthStatus, SystemStatus};
use serde::Deserialize;

use crate::claude_bin;

pub fn detect_system() -> SystemStatus {
    let claude_version = claude_version();
    let claude_installed = claude_version.is_some();
    let auth = if claude_installed {
        auth_status()
    } else {
        AuthStatus::default()
    };
    SystemStatus {
        claude_installed,
        claude_version,
        auth,
    }
}

fn claude_version() -> Option<String> {
    let out = Command::new(claude_bin()).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn auth_status() -> AuthStatus {
    let Ok(out) = Command::new(claude_bin())
        .args(["auth", "status", "--json"])
        .output()
    else {
        return AuthStatus::default();
    };
    if !out.status.success() {
        return AuthStatus::default();
    }
    serde_json::from_slice::<RawAuth>(&out.stdout)
        .map(AuthStatus::from)
        .unwrap_or_default()
}

/// Shape of `claude auth status --json`.
#[derive(Deserialize)]
struct RawAuth {
    #[serde(rename = "loggedIn", default)]
    logged_in: bool,
    #[serde(rename = "authMethod", default)]
    auth_method: Option<String>,
    #[serde(rename = "apiProvider", default)]
    api_provider: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(rename = "subscriptionType", default)]
    subscription_type: Option<String>,
}

impl From<RawAuth> for AuthStatus {
    fn from(r: RawAuth) -> Self {
        AuthStatus {
            logged_in: r.logged_in,
            method: r.auth_method,
            api_provider: r.api_provider,
            email: r.email,
            subscription_type: r.subscription_type,
        }
    }
}
