//! Native Claude print, PTY, and background driver implementations.
//!
//! M0-07 delivers the in-process [`Driver`] contract, launch [`materialize`],
//! thin [`ProviderProfile`] / [`SecretBroker`], binary pin, and a feature-gated
//! [`FakeDriver`] that replays built-in Observations.

mod binary;
mod capabilities;
pub mod claude_bg;
pub mod claude_print;
pub mod claude_pty;
mod driver;
mod error;
mod flags;
pub mod generic_pty;
pub mod interaction;
mod materializer;
mod process;
mod profile;
mod pty_interaction;
mod pty_resource;
mod recipe;
mod secrets;
pub mod shell_pty;
pub mod tty;

#[cfg(any(test, feature = "test-stub"))]
mod fake;

pub use binary::{BinaryPin, default_command, hash_file, pin_binary, resolve_binary};
pub use capabilities::{
    ADAPTER_VERSION, MatrixMark, capability_matrix, capability_set, capability_snapshot,
};
pub use claude_bg::{ClaudeBgDriver, ClaudeBgOptions, parse_backgrounded};
pub use claude_pty::{ClaudePtyDriver, ClaudePtyOptions};
pub use driver::{CallContext, Driver, DriverAck, RunHandle};
pub use error::{DriverError, DriverResult};
pub use generic_pty::{GenericPtyDriver, GenericPtyOptions, KindPreset, WaitUntil, preset_by_id};
pub use materializer::{
    BinarySource, LaunchOrigin, MaterializeRequest, SessionAction, TokenBrokerBind, materialize,
    materialize_with_token_broker, render_api_key_helper_script,
};
pub use process::current_process_identity;
pub use profile::{
    ClaudeProviderOverlay, Delegation, EnvFileSecretBroker, ProviderHealth, ProviderKind,
    ProviderProfile, Secret, SecretBroker, SecretRef, SecretRefPolicy,
    claude_provider_settings_json, write_claude_provider_overlay,
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
pub use shell_pty::{ShellPtyDriver, ShellPtyOptions, default_shell};
pub use tty::{HerdrTty, LocalPty, TTY_SNAPSHOT_MAX, TtyBridge, logical_keys_to_bytes};

#[cfg(any(test, feature = "test-stub"))]
pub use fake::FakeDriver;
