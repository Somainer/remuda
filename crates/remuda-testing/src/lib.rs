//! Test double for `claude -p` stream-json and captured CLI fixtures.
//!
//! The `fake-claude` binary speaks the NDJSON control protocol in
//! `docs/research/claude-stream-json-protocol.md`. Other crates spawn it through
//! [`FakeClaudeProcess`] and read fixture bytes from [`fixtures_dir`].

mod client;
mod fake;
mod flags;
mod paths;
mod script;

pub use client::{
    FakeClaudeProcess, SpawnOptions, fake_claude_bin, is_control_subtype, is_system_subtype,
    is_type, spawn_fake_claude, transcript_path,
};
pub use fake::{FakeClaudeError, run_fake_claude};
pub use flags::ClaudeFlags;
pub use paths::{FIXED_SESSION_ID, ScriptKind, fixtures_dir, script_kind_from_name, script_path};
pub use script::{load_script, load_script_from_env};

/// Directory containing captured Claude / Codex / grok / agy samples.
pub fn captured_samples_dir() -> std::path::PathBuf {
    fixtures_dir()
}
