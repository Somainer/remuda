//! Normalize `node.hello` / heartbeat host inventory (D-013).

use crate::transport::TransportKind;
use serde_json::{Value, json};

/// Optional inventory fields applied on hello and refreshed on heartbeat.
#[derive(Clone, Debug, Default)]
pub struct HostInventoryUpdate {
    /// Display name (`label` string, not placement tags).
    pub display_label: Option<String>,
    /// Placement tags (`region=sg` or `region:sg`).
    pub labels: Option<Value>,
    /// CLI inventory, Hub shape `{kind,version,path,auth}`.
    pub cli: Option<Value>,
    /// Herdr presence `{version,socket,path}`.
    pub herdr: Option<Value>,
    /// Load `{cpuPct,memPct}`.
    pub resources: Option<Value>,
    /// Concurrent instance ceiling.
    pub max_instances: Option<i64>,
    /// Hostname / SSH alias.
    pub hostname: Option<String>,
    /// `std::env::consts::OS` (`macos` / `linux` / …); D-045 preflight.
    pub host_os: Option<String>,
    /// Node binary version.
    pub node_version: Option<String>,
    /// Carrier reported by the Node.
    pub transport: Option<TransportKind>,
}

/// Pull inventory from a JSON-RPC params object (flat or nested under `host`).
pub fn from_node_params(params: &Value) -> HostInventoryUpdate {
    let nested = params.get("host").unwrap_or(params);
    HostInventoryUpdate {
        display_label: string_field(params, "label").or_else(|| string_field(nested, "label")),
        labels: normalize_labels(nested.get("labels").or_else(|| params.get("labels"))),
        cli: normalize_cli(nested.get("cli").or_else(|| params.get("cli"))),
        herdr: normalize_herdr(nested.get("herdr").or_else(|| params.get("herdr"))),
        resources: nested
            .get("resources")
            .cloned()
            .or_else(|| params.get("resources").cloned()),
        max_instances: int_field(nested, "maxInstances")
            .or_else(|| int_field(params, "maxInstances")),
        hostname: string_field(nested, "hostname").or_else(|| string_field(params, "hostname")),
        host_os: string_field(nested, "os").or_else(|| string_field(params, "os")),
        node_version: string_field(params, "nodeVersion")
            .or_else(|| string_field(nested, "nodeVersion")),
        transport: string_field(params, "transport")
            .or_else(|| string_field(params, "carrier"))
            .or_else(|| string_field(nested, "transport"))
            .and_then(|raw| TransportKind::parse(&raw)),
    }
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn int_field(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_u64().map(|n| n as i64))
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

fn normalize_labels(value: Option<&Value>) -> Option<Value> {
    let value = value?;
    if let Some(arr) = value.as_array() {
        let tags: Vec<Value> = arr
            .iter()
            .filter_map(Value::as_str)
            .map(|s| json!(s))
            .collect();
        return Some(Value::Array(tags));
    }
    if let Some(map) = value.as_object() {
        let tags: Vec<Value> = map
            .iter()
            .map(|(k, v)| {
                let rhs = v.as_str().unwrap_or("");
                json!(format!("{k}={rhs}"))
            })
            .collect();
        return Some(Value::Array(tags));
    }
    None
}

fn normalize_cli(value: Option<&Value>) -> Option<Value> {
    let items = value?.as_array()?;
    let mapped: Vec<Value> = items
        .iter()
        .map(|item| {
            let path = item
                .get("path")
                .cloned()
                .or_else(|| item.get("absolutePath").cloned())
                .unwrap_or(Value::Null);
            let auth = item
                .get("auth")
                .and_then(Value::as_str)
                .or_else(|| item.get("authState").and_then(Value::as_str))
                .unwrap_or("unknown");
            let auth = match auth {
                "gateway-native" | "gateway_native" => "gateway-native",
                "logged_in" | "logged-in" => "logged_in",
                "logged_out" | "logged-out" => "logged_out",
                "none" => "none",
                _ => "unknown",
            };
            let installed = item
                .get("installed")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| path.as_str().is_some_and(|s| !s.is_empty()));
            let mut row = json!({
                "kind": item.get("kind").cloned().unwrap_or(Value::Null),
                "version": item.get("version").cloned().unwrap_or(Value::Null),
                "path": path,
                "auth": auth,
                "installed": installed,
            });
            if let Some(flag) = item.get("nativeGateway").and_then(Value::as_bool) {
                row["nativeGateway"] = json!(flag);
            }
            row
        })
        .collect();
    Some(Value::Array(mapped))
}

/// The one per-launch host capability granted this batch (D-045).
/// The capability name.
pub const CAPABILITY_COMPUTER_USE: &str = "computer-use";

/// The remote-host location the c-cua-hostcap Node probe stats (must match
/// `COMPUTER_USE_CLIENT` in `remuda-node/src/inventory.rs` on
/// `wt/c-cua-hostcap`), shown symbolically for the *host* — never expanded
/// from the CLI/Hub process env (a Linux coordinator must not print its own
/// `$HOME` as a path on a Mac). The host resolves `$CODEX_HOME` to its codex
/// home, falling back to `$HOME/.codex`.
pub const COMPUTER_USE_CLIENT_SYMBOLIC: &str = "$CODEX_HOME/computer-use/Codex Computer Use.app/Contents/SharedSupport/SkyComputerUseClient.app/Contents/MacOS/SkyComputerUseClient";

/// The fallback symbolic location for a host with `CODEX_HOME` unset.
pub const COMPUTER_USE_CLIENT_SYMBOLIC_DEFAULT: &str = "$HOME/.codex/computer-use/Codex Computer Use.app/Contents/SharedSupport/SkyComputerUseClient.app/Contents/MacOS/SkyComputerUseClient";

/// Validate requested capability spellings the same way the Node materializer
/// does: an unknown value is an error naming it, never silently dropped.
pub fn validate_capabilities(capabilities: &[String]) -> Result<(), String> {
    for value in capabilities {
        if value != CAPABILITY_COMPUTER_USE {
            return Err(format!(
                "unknown capability {value:?}; this build accepts only \"{CAPABILITY_COMPUTER_USE}\""
            ));
        }
    }
    Ok(())
}

/// D-045 gate 2 server-side: the resolved target host must be macOS and its
/// heartbeat `cli[]` must report `computer-use` installed. The CLI runs the
/// identical check pre-placement; this covers placement-resolved launches the
/// CLI cannot preflight and closes the door on a direct API caller.
///
/// Every error names the host id and the observation, matching the refusal
/// table in `codex-cua.md` §4.
pub fn computer_use_preflight(host: &crate::store::HostRecord) -> Result<(), String> {
    let host_id = host.host_id.as_str();
    if let Some(os) = host.host_os.as_deref()
        && os != "macos"
    {
        return Err(format!(
            "host {host_id} is {os}, but the \"{CAPABILITY_COMPUTER_USE}\" capability requires macOS; \
             pick a Mac with --host"
        ));
    }
    let rows = host.cli.as_array();
    let row = rows.and_then(|rows| {
        rows.iter()
            .find(|row| row.get("kind").and_then(Value::as_str) == Some(CAPABILITY_COMPUTER_USE))
    });
    let Some(row) = row else {
        return Err(format!(
            "host {host_id} has not reported the \"{CAPABILITY_COMPUTER_USE}\" capability \
             (no computer-use row in its inventory); enable Codex Computer Use at \
             {COMPUTER_USE_CLIENT_SYMBOLIC} on that host (or \
             {COMPUTER_USE_CLIENT_SYMBOLIC_DEFAULT} when CODEX_HOME is unset), then update/run \
             a Node that probes it, or pick another host with --host"
        ));
    };
    if row.get("installed").and_then(Value::as_bool) != Some(true) {
        // The row's path when present is the host's own reported location; when
        // absent the hostcap probe omits it for an uninstalled bundle, so name
        // the symbolic location it probes (never a local-process path).
        let probed = row
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .unwrap_or(COMPUTER_USE_CLIENT_SYMBOLIC);
        return Err(format!(
            "host {host_id} reports \"{CAPABILITY_COMPUTER_USE}\" as not installed; enable \
             Codex Computer Use at {probed} on that host (or \
             {COMPUTER_USE_CLIENT_SYMBOLIC_DEFAULT} when CODEX_HOME is unset), or pick another \
             host with --host"
        ));
    }
    Ok(())
}

fn normalize_herdr(value: Option<&Value>) -> Option<Value> {
    let value = value?;
    if value.is_null() {
        return Some(Value::Null);
    }
    Some(json!({
        "version": value.get("version").cloned().unwrap_or(Value::Null),
        "socket": value.get("socket").cloned().unwrap_or(Value::Null),
        "path": value.get("path").cloned().or_else(|| value.get("absolutePath").cloned()).unwrap_or(Value::Null),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_hello_inventory_normalizes_cli_and_labels() {
        let params = json!({
            "hostId": "hst_x",
            "label": "bolt-sg",
            "host": {
                "hostname": "devbox-sg",
                "labels": { "region": "sg", "gpu": "none" },
                "maxInstances": 8,
                "cli": [{
                    "kind": "claude",
                    "version": "2.1.268",
                    "absolutePath": "/usr/bin/claude",
                    "authState": "unknown"
                }],
                "herdr": { "version": "0.9.0", "socket": "/tmp/herdr.sock" },
                "resources": { "cpuPct": 8, "memPct": 31 }
            }
        });
        let inv = from_node_params(&params);
        assert_eq!(inv.display_label.as_deref(), Some("bolt-sg"));
        assert_eq!(inv.hostname.as_deref(), Some("devbox-sg"));
        assert_eq!(inv.max_instances, Some(8));
        let labels = inv.labels.unwrap();
        let tags: Vec<&str> = labels
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(tags.contains(&"region=sg"));
        assert_eq!(inv.cli.as_ref().unwrap()[0]["path"], "/usr/bin/claude");
        assert_eq!(inv.cli.as_ref().unwrap()[0]["auth"], "unknown");
        assert_eq!(inv.cli.as_ref().unwrap()[0]["installed"], true);
        assert_eq!(inv.herdr.as_ref().unwrap()["socket"], "/tmp/herdr.sock");
        assert_eq!(inv.resources.as_ref().unwrap()["cpuPct"], 8);
    }

    #[test]
    fn computer_use_capability_names_are_validated() {
        assert!(validate_capabilities(&[]).is_ok());
        assert!(validate_capabilities(&["computer-use".to_owned()]).is_ok());
        let error = validate_capabilities(&["desktop".to_owned()]).unwrap_err();
        assert!(error.contains("\"desktop\""), "{error}");
    }

    #[test]
    fn computer_use_preflight_classifies_every_host_shape() {
        let host = |os: Option<&str>, cli: Value| crate::store::HostRecord {
            host_id: "hst_x".to_owned(),
            label: "x".into(),
            state: "online".into(),
            online: true,
            last_seen_at: None,
            node_version: None,
            cli,
            capabilities: json!({}),
            instance_count: 0,
            transport: "outbound-wss".into(),
            labels: vec![],
            herdr: None,
            resources: None,
            max_instances: 8,
            hostname: None,
            host_os: os.map(str::to_string),
            ssh: None,
            last_error: None,
            provider_binding: "auto".into(),
            default_launch_args: None,
            claude_binary_path: None,
            default_tui: None,
            workspaces: vec![],
            workspace_revision: 0,
        };

        // Installed row + macOS passes.
        let ok = host(
            Some("macos"),
            json!([{"kind":"computer-use","installed":true,"auth":"unknown"}]),
        );
        computer_use_preflight(&ok).unwrap();

        // No row: capability-unknown style refusal.
        let missing = host(Some("macos"), json!([]));
        assert!(
            computer_use_preflight(&missing)
                .unwrap_err()
                .contains("not reported")
        );

        // Installed=false names the probed path.
        let absent = host(
            Some("macos"),
            json!([{"kind":"computer-use","installed":false,
                    "path":"/Users/u/.codex/computer-use/SkyComputerUseClient"}]),
        );
        let error = computer_use_preflight(&absent).unwrap_err();
        assert!(error.contains("not installed") && error.contains("SkyComputerUseClient"));

        // Installed=false with no path names the symbolic remote-host location,
        // never a bare placeholder (round-6 item 1).
        let pathless = host(
            Some("macos"),
            json!([{"kind":"computer-use","installed":false,"auth":"unknown"}]),
        );
        let error = computer_use_preflight(&pathless).unwrap_err();
        assert!(error.contains("not installed"), "{error}");
        assert!(error.contains("$CODEX_HOME/"), "{error}");
        assert!(error.contains("SkyComputerUseClient"), "{error}");
        assert!(error.contains("$HOME/.codex"), "{error}");
        assert!(!error.contains("<no path reported>"), "{error}");

        // Non-macOS refuses before the row matters.
        let linux = host(
            Some("linux"),
            json!([{"kind":"computer-use","installed":true}]),
        );
        assert!(
            computer_use_preflight(&linux)
                .unwrap_err()
                .contains("linux")
        );

        // A Node that reported no os but did report the row passes (os is
        // additive; the Node-side gate catches an actual non-Mac process).
        let no_os = host(None, json!([{"kind":"computer-use","installed":true}]));
        computer_use_preflight(&no_os).unwrap();
    }

    #[test]
    fn gateway_native_auth_is_preserved() {
        let params = json!({
            "cli": [{
                "kind": "claude",
                "path": "/usr/bin/claude",
                "auth": "gateway-native",
                "installed": true,
                "nativeGateway": true
            }]
        });
        let inv = from_node_params(&params);
        assert_eq!(inv.cli.as_ref().unwrap()[0]["auth"], "gateway-native");
        assert_eq!(inv.cli.as_ref().unwrap()[0]["nativeGateway"], true);
        assert_eq!(inv.cli.as_ref().unwrap()[0]["installed"], true);
    }
}
