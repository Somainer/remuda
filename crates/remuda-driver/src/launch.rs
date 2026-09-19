//! Per-session launch artifacts: the settings overlay and the launch shim.
//!
//! These two together are the physical basis of D-028's unification principle
//! (§1.0): *one agent session is one terminal session running an agent
//! command*. Remuda can put flags on a command it spawns itself, but it cannot
//! put flags on a command a human types. The shim closes that gap — it sits at
//! the front of the PATH of the shell the human is typing into, so `claude`
//! resolves to a wrapper that adds the overlay and then `exec`s the real
//! binary. From that point the two paths are identical, which is what P1 has
//! to prove (§13).
//!
//! Everything here lives under `<instance dir>/launch/` and dies with the
//! instance. Nothing writes to the user's own `~/.claude`, `~/.codex` or
//! `~/.grok` — the overlay only ever *merges* with what the user has, and it
//! reaches the harness through `--settings`, which is additive by construction
//! (verified against claude 2.1.270: a user `SessionStart` hook and an overlay
//! `SessionStart` hook both fire).

pub mod overlay;
pub(crate) mod runtime_dir;
pub mod session;
mod shadow;
pub mod shim;
pub mod skills;
pub mod user;

pub use overlay::{HookOverlay, OverlayOptions, TuiMode, materialize_overlay};
pub use runtime_dir::{instance_sockets_would_redirect, place_token_broker_socket};
pub use session::{HookSession, HookSessionOptions};
pub use shadow::{
    ShadowFile, ShadowHome, ShadowMcpServer, ShadowOptions, materialize_codex, materialize_grok,
};
pub use shim::{ShimSet, materialize_shims, shim_disabled};
pub use user::{
    apply_model_pin, is_model_env, is_overridden_provider_env, load_effective_user_settings,
    merge_provider_overlay_over_user, merge_settings_layers, redact_settings,
};

/// Environment variable that turns the shim off for a session.
///
/// Set to `off` / `0` / `false` in the Node's environment and no shim
/// directory is generated or prepended: `claude` resolves to whatever the user
/// installed and the signal tier degrades to screen. Rollback is an env change
/// and a Node restart, with no schema migration (§13 conflict rule ⑤).
pub const SHIM_DISABLE_ENV: &str = "REMUDA_SHIM";

/// Environment variable that gates the whole P1 hook path.
///
/// Off by default this phase. When unset, no socket is bound, no overlay is
/// written and no shim is generated, so a Node that has not opted in behaves
/// exactly as it did before.
pub const HOOKS_ENABLE_ENV: &str = "REMUDA_PTY_HOOKS";
