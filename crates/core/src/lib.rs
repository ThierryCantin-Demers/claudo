//! Backend: everything above the Claude Code CLI. In M0 this is only the
//! startup detection; the engine (M1) and session store (M2) attach here.

pub mod backend;
pub mod board;
pub mod clients;
pub mod debug;
pub mod dibs;
pub mod engine;
pub mod github;
pub mod handover;
pub mod herdr;
pub mod hub;
pub mod live;
pub mod room;
pub mod runs;
pub mod sessions;
pub mod settings;
pub mod status;
pub mod transcripts;

pub use status::detect_system;

/// The Claude Code CLI binary to drive. Overridable for tests or custom installs.
pub(crate) fn claude_bin() -> String {
    std::env::var("CLAUDO_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string())
}
