//! Client-side preflight for per-launch host capabilities (D-045).
//!
//! The Node materializer is the enforcement boundary (it refuses unknown
//! values, agent origins, bypass+computer-use and unsupported kinds), but the
//! contract requires refusals *before* create/dispatch is persisted and wants
//! the operator to get a retry direction at the command line. This module is
//! the CLI half of that: it validates the requested names and reads the target
//! host's `GET /v1/hosts/{id}` view (`cli[]`, `os`) the same way the Hub does.
//! When no host is resolvable client-side (dispatch with placement), the Hub
//! runs the identical check server-side rather than silently launching.

use anyhow::{Result, bail};
use serde_json::Value;

/// The one capability name this build accepts.
pub const COMPUTER_USE: &str = "computer-use";

/// Validate requested capability spellings before they reach the Hub: an
/// unknown value is refused and named, never silently dropped.
pub fn validate_requested(capabilities: &[String]) -> Result<()> {
    for value in capabilities {
        if value != COMPUTER_USE {
            bail!(
                "unknown --capability {value:?}; this build accepts only \
                 \"{COMPUTER_USE}\""
            );
        }
    }
    Ok(())
}

/// True when the granted list includes `computer-use`.
#[must_use]
pub fn requests_computer_use(capabilities: &[String]) -> bool {
    capabilities.iter().any(|value| value == COMPUTER_USE)
}

/// The remote-host file the c-cua-hostcap Node probe stats, shown symbolically
/// for the *target host* — never expanded from this CLI process's own
/// `CODEX_HOME`/`HOME` (a Linux coordinator refusing a Mac host must not print
/// a path that only exists on its own box). Must match `COMPUTER_USE_CLIENT`
/// in `remuda-node/src/inventory.rs` on the hostcap branch. The host resolves
/// `$CODEX_HOME`, falling back to `$HOME/.codex`.
const COMPUTER_USE_CLIENT_SYMBOLIC: &str = "$CODEX_HOME/computer-use/Codex Computer Use.app/Contents/SharedSupport/SkyComputerUseClient.app/Contents/MacOS/SkyComputerUseClient";
const COMPUTER_USE_CLIENT_SYMBOLIC_DEFAULT: &str = "$HOME/.codex/computer-use/Codex Computer Use.app/Contents/SharedSupport/SkyComputerUseClient.app/Contents/MacOS/SkyComputerUseClient";

/// Classify one host view against the `computer-use` gate (D-045 §2/§4).
///
/// `host` is the JSON `GET /v1/hosts/{id}` returns: `{hostId, os, cli: [...]}`.
/// `Ok(())` means the host reports an installed capability on macOS. Every
/// error names the host id and what was actually observed, with a retry
/// direction.
pub fn host_supports_computer_use(host: &Value) -> Result<()> {
    // Path named in the not-installed / not-reported messages: the host's own
    // reported path when present, else the symbolic remote-host location the
    // hostcap probe stats (never this CLI process's local env).
    let probed_path = || -> String {
        let rows = host.get("cli").and_then(Value::as_array);
        let row_path = rows
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row.get("kind").and_then(Value::as_str) == Some("computer-use"))
            })
            .and_then(|row| row.get("path"))
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty());
        row_path
            .map(str::to_owned)
            .unwrap_or_else(|| COMPUTER_USE_CLIENT_SYMBOLIC.to_owned())
    };
    let host_id = host
        .get("hostId")
        .and_then(Value::as_str)
        .or_else(|| host.get("id").and_then(Value::as_str))
        .unwrap_or("<unknown>");
    let os = host.get("os").and_then(Value::as_str);

    // Non-macOS is its own refusal with the observed OS named: the CUA
    // launchers fail closed off Darwin, and the message must say how to retry.
    if let Some(os) = os
        && os != "macos"
    {
        bail!(
            "host {host_id} is {os}, but the {COMPUTER_USE:?} capability requires macOS; \
             pick a Mac with --host"
        );
    }

    let rows = host.get("cli").and_then(Value::as_array);
    let row = rows.and_then(|rows| {
        rows.iter()
            .find(|row| row.get("kind").and_then(Value::as_str) == Some("computer-use"))
    });
    let Some(row) = row else {
        let probed = probed_path();
        bail!(
            "host {host_id} has not reported the {COMPUTER_USE:?} capability \
             (no computer-use row in its inventory); enable Codex Computer Use at {probed} \
             on that host (or {COMPUTER_USE_CLIENT_SYMBOLIC_DEFAULT} when CODEX_HOME is unset), \
             or update/run a Node that probes it, or pick another host with --host"
        );
    };
    if row.get("installed").and_then(Value::as_bool) != Some(true) {
        let probed = probed_path();
        bail!(
            "host {host_id} reports {COMPUTER_USE:?} as not installed; enable Codex Computer \
             Use at {probed} on that host (or {COMPUTER_USE_CLIENT_SYMBOLIC_DEFAULT} when \
             CODEX_HOME is unset), or pick another host with --host"
        );
    }
    Ok(())
}
