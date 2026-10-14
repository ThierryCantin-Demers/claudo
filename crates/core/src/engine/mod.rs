//! The Claude engine: drives the Claude Code CLI in headless stream-json mode.
//! Kept as its own module so nothing else in the tree depends on CLI specifics.
//! The CLI wrapper is the only implementation (no API-key path, by design).

pub mod cli;

pub use cli::{EngineEvent, Session, SessionConfig};
