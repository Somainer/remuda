//! Node-side host gate for the `computer-use` capability (D-045 §2/§4).
//!
//! The CLI and Hub run the same check as preflight so an operator gets the
//! refusal with a retry direction before anything is persisted. This is the
//! boundary that actually controls materialization: a launch reaching the Node
//! is checked against the *local* inventory (the heartbeat the Hub read can be
//! stale) before any launch dir or managed home is written.
//!
//! Until the hostcap probe lands, the inventory has no `computer-use` row, so
//! every grant is refused here with the "not reported" message — fail closed,
//! exactly as the contract requires while the two batches are in flight.

use remuda_driver::DriverError;
use remuda_protocol::AgentKind;

use crate::inventory::{CliEntry, CollectRequest};

/// The one capability name this batch grants.
pub const CAPABILITY_COMPUTER_USE: &str = "computer-use";

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
        return Err(DriverError::InvalidLaunchSpec(format!(
            "this Node has not reported the {CAPABILITY_COMPUTER_USE:?} capability \
             (no computer-use row in its inventory); run a Node version that probes it"
        )));
    };
    if !row.installed {
        let probed = row
            .path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .filter(|path| !path.is_empty())
            .unwrap_or_else(|| "<no path reported>".into());
        return Err(DriverError::InvalidLaunchSpec(format!(
            "the {CAPABILITY_COMPUTER_USE:?} capability is not installed on this Node; \
             it probed {probed}"
        )));
    }
    Ok(())
}
