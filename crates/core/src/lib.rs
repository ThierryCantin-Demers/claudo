//! Backend: everything above the Claude Code CLI. In M0 this is only the
//! startup detection; the engine (M1) and session store (M2) attach here.

pub mod backend;
pub mod engine;
pub mod sessions;
pub mod settings;
pub mod status;

pub use status::detect_system;

/// The Claude Code CLI binary to drive. Overridable for tests or custom installs.
pub(crate) fn claude_bin() -> String {
    std::env::var("CLAUDO_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string())
}
