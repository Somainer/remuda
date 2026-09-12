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
                "logged_in" | "logged-in" => "logged_in",
                "logged_out" | "logged-out" => "logged_out",
                _ => "unknown",
            };
            json!({
                "kind": item.get("kind").cloned().unwrap_or(Value::Null),
                "version": item.get("version").cloned().unwrap_or(Value::Null),
                "path": path,
                "auth": auth,
            })
        })
        .collect();
    Some(Value::Array(mapped))
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
        assert_eq!(inv.herdr.as_ref().unwrap()["socket"], "/tmp/herdr.sock");
        assert_eq!(inv.resources.as_ref().unwrap()["cpuPct"], 8);
    }
}
