//! Test doubles for `claude -p` stream-json and the Herdr socket API.
//!
//! The `fake-claude` binary speaks the NDJSON control protocol in
//! `docs/research/claude-stream-json-protocol.md`. Other crates spawn it through
//! [`FakeClaudeProcess`] and read fixture bytes from [`fixtures_dir`].
//!
//! `fake-herdr` impersonates herdr 0.9.0 JSON-RPC over a Unix socket so
//! `remuda-herdr` tests can run offline.
//!
//! `fake-gateway` is an offline Anthropic-Messages-compatible HTTP origin on
//! loopback, with scriptable SSE streams, `429`/`529`/`401` responses and
//! mid-stream aborts. The api-routing tests use it as the gateway the Hub or a
//! proxy host would relay to, so a relay can be proven end to end without a
//! live model and without any credential existing in the test process. See
//! [`fake_gateway`].

mod bin_locator;
mod client;
mod fake;
pub mod fake_gateway;
pub mod fake_harness;
mod fake_herdr;
mod flags;
mod parent_watch;
mod paths;
mod script;
mod stub;
mod workspace_roots;

pub use bin_locator::{
    cargo_bin_exe, cargo_target_dir, ensure_workspace_bin, env_bin_override, fallback_bin_path,
    locate_bin_in, locate_workspace_bin, workspace_root,
};
pub use client::{
    FakeClaudeProcess, SpawnOptions, fake_claude_argv, fake_claude_bin, is_control_subtype,
    is_system_subtype, is_type, spawn_fake_claude, spawn_fake_claude_sdk, transcript_path,
};
pub use fake::{FakeClaudeError, run_fake_claude};
pub use fake_gateway::{
    ANTHROPIC_VERSION, DEFAULT_LISTEN, DEFAULT_MODEL, DEFAULT_TEXT, FakeGateway, FakeGatewayError,
    RecordedRequest, SSE_CONTENT_TYPE, Script, parse_listen, parse_script, run_fake_gateway,
};
pub use fake_herdr::{
    FakeHerdrError, FakeHerdrOptions, FakeHerdrScript, FakeHerdrServer, fake_herdr_bin,
    herdr_frames_path, herdr_session_ok_path, run_fake_herdr, write_observe_frames,
};
pub use flags::ClaudeFlags;
pub use paths::{
    FIXED_SESSION_ID, ScriptKind, fixtures_dir, hook_message_stream_fixture, hook_session_fixture,
    hook_session_path, hook_workflow_fixture, script_kind_from_name, script_path,
};
pub use script::{load_script, load_script_from_env};
pub use stub::install_executable;
pub use workspace_roots::test_workspace_roots;

/// Directory containing captured Claude / Codex / grok / agy samples.
pub fn captured_samples_dir() -> std::path::PathBuf {
    fixtures_dir()
}
