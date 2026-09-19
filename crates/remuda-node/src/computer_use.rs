//! Node-side host gate for the `computer-use` capability (D-045 §2/§4).
//!
//! The CLI and Hub run the same check as preflight so an operator gets the
//! refusal with a retry direction before anything is persisted. This is the
//! boundary that actually controls materialization: a launch reaching the Node
//! is checked against the *local* inventory (the heartbeat the Hub read can be
//! stale) before any launch dir or managed home is written.
//!
//! The inventory probe now appends the `computer-use` row unconditionally, so
//! on a current Node the gate refuses on `installed: false` and names the path
//! it looked for. The "not reported" arm remains for a Node that predates the
//! probe — an absent row means nobody asked, which is refused too, but it is no
//! longer the ordinary outcome.

use remuda_driver::DriverError;
use remuda_protocol::AgentKind;

use crate::inventory::{CliEntry, CollectRequest, ProbeEnv};

/// The one capability name this batch grants.
pub const CAPABILITY_COMPUTER_USE: &str = "computer-use";

/// The default client path the hostcap probe looks for when the inventory
/// carries no explicit path.
///
/// Derived from [`crate::inventory::computer_use_bundle_path`], which is the
/// same constant the probe stats, so a refusal that names "where to install
/// this" cannot point somewhere the probe never looked.
pub fn default_client_path() -> Option<std::path::PathBuf> {
    Some(crate::inventory::computer_use_bundle_path(
        &ProbeEnv::from_process(),
    ))
}

/// Verify this host may deliver `computer-use` for `kind`.
pub fn host_preflight(kind: &AgentKind) -> Result<(), DriverError> {
    let os = std::env::consts::OS;
    let snapshot = crate::inventory::collect_fresh(&CollectRequest::default());
    let row = snapshot
        .cli
        .iter()
        .find(|row| row.kind == CAPABILITY_COMPUTER_USE);
    evaluate(kind, os, row)
}

/// The pure gate, split out so the host facts (`std::env::consts::OS` and the
/// live probe) are the only non-injectable inputs. The CLI/Hub preflight and
/// this gate must agree, so both are just matchings over `(kind, os, row)`.
pub fn evaluate(kind: &AgentKind, os: &str, row: Option<&CliEntry>) -> Result<(), DriverError> {
    if !matches!(kind, AgentKind::Claude | AgentKind::Codex) {
        return Err(DriverError::InvalidLaunchSpec(format!(
            "the {CAPABILITY_COMPUTER_USE:?} capability is not supported for agent kind {kind:?} \
             this batch; supported kinds are claude and codex"
        )));
    }
    if os != "macos" {
        return Err(DriverError::InvalidLaunchSpec(format!(
            "the {CAPABILITY_COMPUTER_USE:?} capability requires macOS; this host is {os}"
        )));
    }
    let Some(row) = row else {
        let probed = default_client_path()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| "<no path reported>".into());
        return Err(DriverError::InvalidLaunchSpec(format!(
            "this Node has not reported the {CAPABILITY_COMPUTER_USE:?} capability \
             (no computer-use row in its inventory); install/enable Codex Computer Use at \
             {probed}, or run a Node version that probes it"
        )));
    };
    if !row.installed {
        // Name the probed path: the row's own path when the probe reported one,
        // else the same default the probe stats (via `default_client_path`), so
        // the most common failure says where to install rather than shrugging.
        let probed = row
            .path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .filter(|path| !path.is_empty())
            .or_else(|| default_client_path().map(|path| path.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "<no path reported>".into());
        return Err(DriverError::InvalidLaunchSpec(format!(
            "the {CAPABILITY_COMPUTER_USE:?} capability is not installed on this Node; \
             enable Codex Computer Use at {probed}"
        )));
    }
    Ok(())
}
