//! Native Claude print, PTY, and background driver implementations.
//!
//! M0-07 delivers the in-process [`Driver`] contract, launch [`materialize`],
//! thin [`ProviderProfile`] / [`SecretBroker`], binary pin, and a feature-gated
//! [`FakeDriver`] that replays built-in Observations.

pub mod agent_mcp;
pub mod attachment;
mod binary;
mod capabilities;
pub mod child_env;
pub mod claude_bg;
pub mod claude_onboarding;
pub mod claude_print;
pub mod claude_pty;
pub mod claude_transcript;
pub mod codex_rollout;
mod driver;
mod error;
mod flags;
pub mod generic_pty;
pub mod grok_session;
pub mod interaction;
pub mod launch;
mod materializer;
pub mod presets;
mod process;
mod profile;
pub mod promote;
mod pty_interaction;
mod pty_launch;
mod pty_resource;
mod recipe;
mod secrets;
pub mod shell_pty;
pub mod tty;
pub mod usage;

#[cfg(any(test, feature = "test-stub"))]
mod fake;

pub use attachment::{PromptAttachment, attachments_of, text_with_path_mentions};
pub use binary::{BinaryPin, default_command, hash_file, pin_binary, resolve_binary};
pub use capabilities::{
    ADAPTER_VERSION, MatrixMark, capability_matrix, capability_set, capability_snapshot,
};
pub use claude_bg::{ClaudeBgDriver, ClaudeBgOptions, parse_backgrounded};
pub use claude_onboarding::{
    HostClaudeConfig, SeedOutcome, StartupDialog, has_login_material, seed_scoped_config,
    startup_dialog,
};
pub use claude_print::TranscriptMapper;
pub use claude_pty::{ClaudePtyDriver, ClaudePtyOptions};
pub use claude_transcript::{
    BindingSource, SessionStartReport, TranscriptBinding, TranscriptCandidate, TranscriptTail,
    bind_by_pid_file, bind_by_session_id, bind_manual, cwd_matches, encode_project_dir,
    list_candidates, project_dir, recorded_cwd, transcript_belongs_to_cwd,
};
pub use driver::{CallContext, Driver, DriverAck, RunHandle};
pub use error::{DriverError, DriverResult};
/// Validate `InstanceSpec.args` against the per-driver launch allowlist.
///
/// Exported so the Hub can reject a bad flag with a 400 at create time instead
/// of letting it travel to the Node and fail there. One table, two callers: the
/// Node stays the authority and re-runs this during materialization.
pub use flags::validate_spec_args as validate_launch_args;
pub use generic_pty::{GenericPtyDriver, GenericPtyOptions, WaitUntil};
pub use launch::{
    HOOKS_ENABLE_ENV, HookOverlay, HookSession, HookSessionOptions, OverlayOptions,
    SHIM_DISABLE_ENV, ShimSet, TuiMode, materialize_overlay, materialize_shims, shim_disabled,
};
pub use materializer::{
    BinarySource, LaunchOrigin, MaterializeRequest, SessionAction, TokenBrokerBind, materialize,
    materialize_with_token_broker, render_api_key_helper_script,
};
pub use presets::{KindPreset, PRESETS, merge_yolo_argv, preset_by_id, preset_for_spec};
pub use process::current_process_identity;
pub use profile::{
    ClaudeProviderOverlay, Delegation, EnvFileSecretBroker, ProviderHealth, ProviderKind,
    ProviderProfile, Secret, SecretBroker, SecretRef, SecretRefPolicy,
    claude_provider_settings_json, write_claude_provider_overlay,
};
pub use promote::{
    AGENT_TABLE, AgentSignature, Detected, ProcessRow, ProcessTable, SystemProcessTable,
};
pub use pty_resource::{PtyResource, PtyResourceStore};
pub use recipe::{
    EnvAllowlistEntry, EnvAllowlistSource, FileLifetime, FileRole, LaunchAudit, LaunchRecipe,
    MaterializedFile, RecipePermission, RecipeProvider, TECH_DEBT_M0_PERM_01,
    report_launch_cleanup,
};
pub use remuda_protocol::DriverKind;
#[cfg(all(target_os = "macos", feature = "keychain"))]
pub use secrets::KeychainSecretBroker;
pub use secrets::{FileSecretStore, TokenBroker, fingerprint_secret};
#[cfg(unix)]
pub use secrets::{request_secret, serve_token_broker};
pub use shell_pty::{
    EMULATOR_ENV, ShellPtyDriver, ShellPtyOptions, default_shell, emulator_enabled,
};
pub use tty::{
    HerdrTty, LocalPty, PtySnapshot, SnapshotSource, TTY_SNAPSHOT_MAX, TtyBridge,
    logical_keys_to_bytes,
};

#[cfg(any(test, feature = "test-stub"))]
pub use fake::FakeDriver;
