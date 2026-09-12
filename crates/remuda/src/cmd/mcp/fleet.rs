//! MCP fleet tools: metadata and handler are registered together.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use super::{Tool, args::{opt_str, string_list, send_text_from_args}};
use crate::cmd::fleet::{FleetFilter, FleetKeysOpts, FleetRunOpts, FleetSendOpts, fleet_keys, fleet_run, fleet_send_opts};

pub(super) fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "remuda_fleet_run",
            "Create one instance per host (POST /v1/fleet/instances; 404 if Hub fleet is not deployed).",
            json!({
                "type": "object",
                "properties": {
                    "hosts": { "type": "array", "items": { "type": "string" } },
                    "labels": { "type": "array", "items": { "type": "string" } },
                    "max": { "type": "integer" },
                    "kind": { "type": "string" },
                    "driver": { "type": "string" },
                    "workspaceId": { "type": "string" },
                    "title": { "type": "string" },
                    "prompt": { "type": "string" }
                }
            }),
            |client, args| Box::pin(async move {
fleet_run(client, fleet_opts_from_json(&args)?).await
            }),
        ),
        Tool::new(
            "remuda_fleet_send",
            "Broadcast a prompt to running instances (POST /v1/fleet/broadcast). Select with `all` and/or `labels`/`hosts`/`kinds`; returns per-instance results with an accepted/failed summary.",
            json!({
                "type": "object",
                "properties": {
                    "all": { "type": "boolean" },
                    "labels": { "type": "array", "items": { "type": "string" } },
                    "hosts": { "type": "array", "items": { "type": "string" } },
                    "kinds": { "type": "array", "items": { "type": "string" } },
                    "idempotencyKey": { "type": "string" },
                    "text": { "type": "string" },
                    "file": { "type": "string" }
                }
            }),
            |client, args| Box::pin(async move {
let text = send_text_from_args(&args)?;
            fleet_send_opts(
                client,
                FleetSendOpts {
                    filter: fleet_filter_from_json(&args),
                    idempotency_key: opt_str(&args, "idempotencyKey").map(str::to_string),
                    text,
                },
            )
            .await
            }),
        ),
        Tool::new(
            "remuda_fleet_keys",
            "Broadcast logical keys (`enter`, `esc`, `ctrl+c`) to running instances (POST /v1/fleet/broadcast with tty.write). Keys are validated before any bytes are sent.",
            json!({
                "type": "object",
                "required": ["keys"],
                "properties": {
                    "all": { "type": "boolean" },
                    "labels": { "type": "array", "items": { "type": "string" } },
                    "hosts": { "type": "array", "items": { "type": "string" } },
                    "kinds": { "type": "array", "items": { "type": "string" } },
                    "idempotencyKey": { "type": "string" },
                    "keys": { "type": "array", "items": { "type": "string" } }
                }
            }),
            |client, args| Box::pin(async move {
let keys = string_list(&args, "keys");
            if keys.is_empty() {
                return Err(anyhow!("remuda_fleet_keys requires keys"));
            }
            fleet_keys(
                client,
                FleetKeysOpts {
                    filter: fleet_filter_from_json(&args),
                    idempotency_key: opt_str(&args, "idempotencyKey").map(str::to_string),
                    keys,
                },
            )
            .await
            }),
        ),
    ]
}

fn fleet_opts_from_json(args: &Value) -> Result<FleetRunOpts> {
    Ok(FleetRunOpts {
        hosts: string_list(args, "hosts"),
        labels: string_list(args, "labels"),
        max: args.get("max").and_then(Value::as_u64).map(|n| n as u32),
        kind: opt_str(args, "kind").unwrap_or("claude").to_string(),
        driver: match opt_str(args, "driver").unwrap_or("claude-print") {
            "pty" => "generic-pty".to_string(),
            other => other.to_string(),
        },
        workspace_id: opt_str(args, "workspaceId").map(str::to_string),
        title: opt_str(args, "title").map(str::to_string),
        prompt: opt_str(args, "prompt").map(str::to_string),
    })
}

/// Shared `all` / `labels` / `hosts` / `kinds` selection for the fleet
/// broadcast tools. `host` / `kind` are accepted as singular aliases.
fn fleet_filter_from_json(args: &Value) -> FleetFilter {
    let mut hosts = string_list(args, "hosts");
    hosts.extend(string_list(args, "host"));
    let mut kinds = string_list(args, "kinds");
    kinds.extend(string_list(args, "kind"));
    FleetFilter {
        all: args.get("all").and_then(Value::as_bool).unwrap_or(false),
        labels: string_list(args, "labels"),
        hosts,
        kinds,
    }
}

