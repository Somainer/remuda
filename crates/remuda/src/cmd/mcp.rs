//! stdio MCP server (`remuda mcp`) exposing instance/fleet tools over JSON-RPC 2.0.
//!
//! Framing: Claude Code / LSP `Content-Length` headers, or NDJSON (one JSON
//! object per line) for tests and curl-style clients. Hand-written rather than
//! the `rmcp` crate so the CLI crate stays a thin Hub HTTP client.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};

use super::fleet::{
    FleetFilter, FleetKeysOpts, FleetRunOpts, FleetSendOpts, fleet_keys, fleet_run, fleet_send_opts,
};
use super::hub_client::{HubClient, HubOpts, block_on};
use super::instance::{CreateOpts, create, list_instances, read, send, send_keys, stop, wait};
use super::merge;
use super::worktree;

const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    Unknown,
    Ndjson,
    Lsp,
}

/// Run `remuda mcp` on stdio until stdin EOF.
pub(crate) fn run(hub: HubOpts) -> Result<()> {
    block_on(async move {
        let client = hub.connect()?;
        let stdin = BufReader::new(tokio::io::stdin());
        let stdout = tokio::io::stdout();
        serve_rpc(stdin, stdout, client).await
    })
}

pub(crate) async fn serve_rpc<R, W>(mut reader: R, mut writer: W, client: HubClient) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut framing = Framing::Unknown;
    loop {
        let Some(msg) = read_rpc(&mut reader, &mut framing).await? else {
            return Ok(());
        };
        if let Some(response) = handle_rpc(&msg, &client).await {
            write_rpc(&mut writer, framing, &response).await?;
        }
    }
}

pub(crate) async fn handle_rpc(msg: &Value, client: &HubClient) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str);
    let id = msg.get("id").cloned()?;
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let Some(method) = method else {
        return Some(rpc_error(id, -32600, "invalid request"));
    };
    match method {
        "initialize" => Some(rpc_ok(id, initialize_result(&params))),
        "ping" => Some(rpc_ok(id, json!({}))),
        "tools/list" => Some(rpc_ok(id, json!({ "tools": tools_catalog() }))),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let result = call_tool(name, args, client).await;
            let operation_report = if matches!(name, "remuda_merge" | "remuda_doctor") {
                result.as_ref().ok().cloned()
            } else {
                None
            };
            let mut content = tool_content(result);
            if let Some(report) = operation_report {
                content["isError"] = json!(report["exitCode"] != 0);
                content["structuredContent"] = report;
            }
            Some(rpc_ok(id, content))
        }
        "notifications/initialized" | "initialized" | "notifications/cancelled" => None,
        other => Some(rpc_error(id, -32601, &format!("method not found: {other}"))),
    }
}

fn initialize_result(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSION);
    let version = if requested == PROTOCOL_VERSION || requested == "2025-03-26" {
        requested
    } else {
        PROTOCOL_VERSION
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "remuda",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

pub(crate) fn tools_catalog() -> Vec<Value> {
    vec![
        tool(
            "remuda_instance_create",
            "Create a Remuda instance on a Hub host (`host`) or matching `labels`. Optional `worktree` runs `git worktree add -b wt/<name>/…` and records the path as the instance workspace cwd.",
            json!({
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": "Host id (hst_…)" },
                    "labels": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Placement labels (key=value)"
                    },
                    "kind": { "type": "string" },
                    "driver": { "type": "string", "description": "claude-print or generic-pty (`pty` alias)" },
                    "workspaceId": { "type": "string" },
                    "cwd": { "type": "string" },
                    "worktree": { "type": "string" },
                    "name": { "type": "string" },
                    "title": { "type": "string" },
                    "prompt": { "type": "string" },
                    "promptFile": { "type": "string" },
                    "commandId": { "type": "string" }
                }
            }),
        ),
        tool(
            "remuda_instance_list",
            "List instances (name, kind, status, cwd, host).",
            json!({
                "type": "object",
                "properties": {
                    "host": { "type": "string" }
                }
            }),
        ),
        tool(
            "remuda_instance_send",
            "Send a prompt to a running instance. `file` is a local path read by this MCP process.",
            json!({
                "type": "object",
                "required": ["instanceId"],
                "properties": {
                    "instanceId": { "type": "string" },
                    "text": { "type": "string" },
                    "file": { "type": "string" },
                    "commandId": { "type": "string" },
                    "completionScope": { "type": "string" }
                }
            }),
        ),
        tool(
            "remuda_instance_respond",
            "List pending interactions, or answer a displayed option/text through the first-answer-wins broker.",
            json!({"type":"object", "required":["instanceId"], "properties":{
                "instanceId":{"type":"string"}, "interactionId":{"type":"string"},
                "option":{"type":"string"}, "text":{"type":"string"},
                "answer":{"type":"object"}, "commandId":{"type":"string"}
            }}),
        ),
        tool(
            "remuda_instance_wait",
            "Wait until idle, done, blocked, or line:<regex> (default done, timeout 30000 ms).",
            json!({
                "type": "object",
                "required": ["instanceId"],
                "properties": {
                    "instanceId": { "type": "string" },
                    "until": { "type": "string", "description": "idle | done | blocked | line:<regex>" },
                    "condition": { "type": "string" },
                    "afterSeq": { "type": "string" },
                    "timeoutMs": { "type": "integer" }
                }
            }),
        ),
        tool(
            "remuda_instance_read",
            "Read the last N lines from screen (tty) or journal.",
            json!({
                "type": "object",
                "required": ["instanceId"],
                "properties": {
                    "instanceId": { "type": "string" },
                    "afterSeq": { "type": "string" },
                    "lines": { "type": "integer" },
                    "limit": { "type": "integer" },
                    "source": { "type": "string", "description": "screen | journal" }
                }
            }),
        ),
        tool(
            "remuda_instance_keys",
            "Send logical keys (enter, esc, ctrl+c) via tty.write. Requires a tty-attach driver (generic-pty).",
            json!({
                "type": "object",
                "required": ["instanceId", "keys"],
                "properties": {
                    "instanceId": { "type": "string" },
                    "keys": { "type": "array", "items": { "type": "string" } }
                }
            }),
        ),
        tool(
            "remuda_instance_stop",
            "Cancel a run (`scope=run`) or close an instance (`scope=instance`).",
            json!({
                "type": "object",
                "required": ["instanceId"],
                "properties": {
                    "instanceId": { "type": "string" },
                    "scope": { "type": "string" },
                    "runId": { "type": "string" },
                    "commandId": { "type": "string" }
                }
            }),
        ),
        tool(
            "remuda_instance_rm",
            "Close an instance (`instance.close`).",
            json!({
                "type": "object",
                "required": ["instanceId"],
                "properties": {
                    "instanceId": { "type": "string" },
                    "commandId": { "type": "string" }
                }
            }),
        ),
        tool(
            "remuda_worktree_create",
            "Create a git worktree (`git worktree add -b wt/<name>/…`). Default path `../remuda-wt/<name>`.",
            json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string" },
                    "base": { "type": "string" },
                    "path": { "type": "string" },
                    "repo": { "type": "string" }
                }
            }),
        ),
        tool(
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
        ),
        tool(
            "remuda_doctor",
            "Preflight the local machine (default/local=true) or an active Hub host. Reports binary versions and login-marker states, data/identity, disk, ports, Hub reachability and registered host links. Nonzero exitCode means blockers. No credential values are returned.",
            json!({"type":"object","additionalProperties":false,"properties":{
                "host":{"type":"string"},"local":{"type":"boolean"},"dataDir":{"type":"string"}
            }}),
        ),
        tool(
            "remuda_worktree_rm",
            "Remove a registered linked Git worktree by Remuda name or explicit path on this MCP server. Keeps the branch. Refuses primary/current/main, locks, or active merge/rebase; dirty files require explicit force=true.",
            json!({"type":"object","additionalProperties":false,"required":["name"],"properties":{
                "name":{"type":"string"},"repo":{"type":"string"},"force":{"type":"boolean"}
            }}),
        ),
        tool(
            "remuda_merge",
            "Merge a local branch into main in a disposable worktree, run the shared gate, compare-and-swap main and push origin. Requires gate=true or dryRun=true. dryRun only inspects local refs. Reports exitCode 0 ok / 1 gate failed / 2 conflict / 3 CAS lost with step timings. Runs on the MCP server's machine.",
            json!({
                "type": "object",
                "required": ["branch"],
                "additionalProperties": false,
                "properties": {
                    "branch": { "type": "string" },
                    "gate": { "type": "boolean" },
                    "dryRun": { "type": "boolean" },
                    "affected": { "type": "boolean", "default": true },
                    "full": { "type": "boolean", "description": "Test the full workspace" },
                    "web": { "type": "boolean" },
                    "noPush": { "type": "boolean" },
                    "repo": { "type": "string" },
                    "targetDir": { "type": "string" },
                    "message": { "type": "string" }
                }
            }),
        ),
        tool(
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
        ),
        tool(
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
        ),
    ]
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
    })
}

async fn call_tool(name: &str, args: Value, client: &HubClient) -> Result<Value> {
    match name {
        "remuda_instance_create" => create(client, create_opts_from_json(&args)?).await,
        "remuda_instance_list" => list_instances(client, opt_str(&args, "host")).await,
        "remuda_instance_send" => {
            let instance_id = required_str(&args, "instanceId")?;
            let text = send_text_from_args(&args)?;
            send(
                client,
                instance_id,
                &text,
                opt_str(&args, "commandId"),
                opt_str(&args, "completionScope").unwrap_or("native-turn"),
                "mcp",
            )
            .await
        }
        "remuda_instance_respond" => {
            super::instance_interaction::respond(
                client,
                super::instance_interaction::RespondOpts {
                    instance_id: required_str(&args, "instanceId")?.to_owned(),
                    interaction_id: opt_str(&args, "interactionId").map(str::to_owned),
                    option: opt_str(&args, "option").map(str::to_owned),
                    text: opt_str(&args, "text").map(str::to_owned),
                    answer: args.get("answer").map(Value::to_string),
                    command_id: opt_str(&args, "commandId").map(str::to_owned),
                },
            )
            .await
        }
        "remuda_instance_wait" => {
            let instance_id = required_str(&args, "instanceId")?;
            let timeout_ms = args
                .get("timeoutMs")
                .and_then(Value::as_u64)
                .or_else(|| args.get("timeout").and_then(Value::as_u64))
                .unwrap_or(30_000);
            let until = opt_str(&args, "until")
                .or_else(|| opt_str(&args, "condition"))
                .unwrap_or("done");
            wait(
                client,
                instance_id,
                until,
                opt_str(&args, "afterSeq"),
                timeout_ms,
            )
            .await
        }
        "remuda_instance_read" => {
            let instance_id = required_str(&args, "instanceId")?;
            let lines = args
                .get("lines")
                .and_then(Value::as_u64)
                .or_else(|| args.get("limit").and_then(Value::as_u64))
                .unwrap_or(120) as usize;
            read(
                client,
                instance_id,
                opt_str(&args, "afterSeq"),
                lines,
                opt_str(&args, "source").unwrap_or("journal"),
            )
            .await
        }
        "remuda_instance_keys" => {
            let instance_id = required_str(&args, "instanceId")?;
            send_keys(client, instance_id, &string_list(&args, "keys")).await
        }
        "remuda_instance_stop" => {
            let instance_id = required_str(&args, "instanceId")?;
            stop(
                client,
                instance_id,
                opt_str(&args, "scope").unwrap_or("run"),
                opt_str(&args, "runId"),
                opt_str(&args, "commandId"),
            )
            .await
        }
        "remuda_instance_rm" => {
            let instance_id = required_str(&args, "instanceId")?;
            stop(
                client,
                instance_id,
                "instance",
                None,
                opt_str(&args, "commandId"),
            )
            .await
        }
        "remuda_worktree_create" => {
            let name = required_str(&args, "name")?;
            let base = opt_str(&args, "base").unwrap_or("main");
            let path = opt_str(&args, "path").map(std::path::PathBuf::from);
            let repo = opt_str(&args, "repo").map(std::path::PathBuf::from);
            let record = worktree::create(name, base, path.as_deref(), repo.as_deref())?;
            Ok(json!({
                "name": record.name,
                "path": record.path,
                "branch": record.branch,
                "base": record.base,
            }))
        }
        "remuda_fleet_run" => fleet_run(client, fleet_opts_from_json(&args)?).await,
        "remuda_merge" => {
            let options: merge::MergeArgs = serde_json::from_value(args)?;
            let report = tokio::task::spawn_blocking(move || merge::execute(options)).await?;
            Ok(serde_json::to_value(report)?)
        }
        "remuda_doctor" => {
            let mut config = crate::config::Config::load(None)?;
            let mut args = args;
            if let Some(path) = args.as_object_mut().and_then(|map| map.remove("dataDir")) {
                config.data_dir = std::path::PathBuf::from(
                    path.as_str()
                        .ok_or_else(|| anyhow!("dataDir must be a string"))?,
                );
            }
            let options = serde_json::from_value(args)?;
            super::doctor::inspect(&config, &options, client).await
        }
        "remuda_worktree_rm" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct RemoveArgs {
                name: String,
                repo: Option<std::path::PathBuf>,
                #[serde(default)]
                force: bool,
            }
            let options: RemoveArgs = serde_json::from_value(args)?;
            tokio::task::spawn_blocking(move || {
                worktree::remove(&options.name, options.repo.as_deref(), options.force)
            })
            .await?
        }
        "remuda_fleet_send" => {
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
        }
        "remuda_fleet_keys" => {
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
        }
        other => Err(anyhow!("unknown tool: {other}")),
    }
}

fn send_text_from_args(args: &Value) -> Result<String> {
    if let Some(path) = opt_str(args, "file") {
        return std::fs::read_to_string(path).map_err(|err| anyhow!("read {path}: {err}"));
    }
    args.get("text")
        .and_then(Value::as_str)
        .or_else(|| args.get("input").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("text or file is required"))
}

fn create_opts_from_json(args: &Value) -> Result<CreateOpts> {
    let mut prompt = opt_str(args, "prompt").map(str::to_string);
    if prompt.is_none()
        && let Some(path) = opt_str(args, "promptFile")
    {
        prompt = Some(std::fs::read_to_string(path).map_err(|err| anyhow!("read {path}: {err}"))?);
    }
    Ok(CreateOpts {
        host: opt_str(args, "host").map(str::to_string),
        labels: string_list(args, "labels"),
        kind: opt_str(args, "kind").unwrap_or("claude").to_string(),
        driver: match opt_str(args, "driver").unwrap_or("claude-print") {
            "pty" => "generic-pty".to_string(),
            other => other.to_string(),
        },
        workspace_id: opt_str(args, "workspaceId").map(str::to_string),
        cwd: opt_str(args, "cwd").map(str::to_string),
        worktree: opt_str(args, "worktree").map(str::to_string),
        name: opt_str(args, "name").map(str::to_string),
        title: opt_str(args, "title").map(str::to_string),
        prompt,
        command_id: opt_str(args, "commandId").map(str::to_string),
    })
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

fn string_list(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(Value::String(s)) => s
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    opt_str(args, key).ok_or_else(|| anyhow!("{key} is required"))
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn tool_content(result: Result<Value>) -> Value {
    match result {
        Ok(value) => json!({
            "content": [{ "type": "text", "text": value.to_string() }],
            "isError": false,
        }),
        Err(err) => json!({
            "content": [{ "type": "text", "text": err.to_string() }],
            "isError": true,
        }),
    }
}

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

async fn read_rpc<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    framing: &mut Framing,
) -> Result<Option<Value>> {
    match *framing {
        Framing::Lsp => read_lsp(reader, None).await,
        Framing::Ndjson => read_ndjson(reader).await,
        Framing::Unknown => loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                return Ok(None);
            }
            if line.to_ascii_lowercase().starts_with("content-length:") {
                *framing = Framing::Lsp;
                return read_lsp(reader, Some(line)).await;
            }
            if line.trim().is_empty() {
                continue;
            }
            *framing = Framing::Ndjson;
            return Ok(Some(serde_json::from_str(line.trim())?));
        },
    }
}

async fn read_ndjson<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Value>> {
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return Ok(Some(serde_json::from_str(trimmed)?));
    }
}

async fn read_lsp<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    first_line: Option<String>,
) -> Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    if let Some(line) = first_line.as_deref() {
        parse_lsp_header(line, &mut content_length);
    }
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        parse_lsp_header(&line, &mut content_length);
    }
    let len = content_length.ok_or_else(|| anyhow!("MCP Content-Length missing"))?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(Some(serde_json::from_slice(&buf)?))
}

fn parse_lsp_header(line: &str, content_length: &mut Option<usize>) {
    let Some((key, value)) = line.split_once(':') else {
        return;
    };
    if key.trim().eq_ignore_ascii_case("content-length") {
        *content_length = value.trim().parse().ok();
    }
}

async fn write_rpc<W: AsyncWrite + Unpin>(
    writer: &mut W,
    framing: Framing,
    msg: &Value,
) -> Result<()> {
    let body = serde_json::to_vec(msg)?;
    match framing {
        Framing::Lsp => {
            let header = format!("Content-Length: {}\r\n\r\n", body.len());
            writer.write_all(header.as_bytes()).await?;
            writer.write_all(&body).await?;
        }
        Framing::Ndjson | Framing::Unknown => {
            writer.write_all(&body).await?;
            writer.write_all(b"\n").await?;
        }
    }
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::hub_client::connect_for_test;
    use crate::cmd::test_hub::spawn_mock_hub;

    fn dummy_client() -> HubClient {
        connect_for_test("http://127.0.0.1:1".into(), "t".into()).expect("client")
    }

    #[tokio::test]
    async fn initialize_and_tools_list() {
        let client = dummy_client();
        let init = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" }
            }
        });
        let resp = handle_rpc(&init, &client).await.expect("response");
        assert_eq!(resp["result"]["serverInfo"]["name"], json!("remuda"));
        assert_eq!(resp["result"]["protocolVersion"], json!(PROTOCOL_VERSION));

        let list = json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}});
        let resp = handle_rpc(&list, &client).await.expect("response");
        let names: Vec<String> = resp["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        for expected in [
            "remuda_instance_create",
            "remuda_instance_list",
            "remuda_instance_send",
            "remuda_instance_wait",
            "remuda_instance_read",
            "remuda_instance_keys",
            "remuda_instance_respond",
            "remuda_instance_stop",
            "remuda_instance_rm",
            "remuda_worktree_create",
            "remuda_fleet_run",
            "remuda_fleet_send",
            "remuda_fleet_keys",
            "remuda_merge",
            "remuda_doctor",
            "remuda_worktree_rm",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
    }

    #[tokio::test]
    async fn unknown_method_is_json_rpc_error() {
        let client = dummy_client();
        let msg = json!({"jsonrpc":"2.0","id":9,"method":"nope"});
        let resp = handle_rpc(&msg, &client).await.expect("response");
        assert_eq!(resp["error"]["code"], json!(-32601));
    }

    #[tokio::test]
    async fn tools_call_list_keys_and_fleet_send_against_mock_hub() {
        let mock = spawn_mock_hub().await;
        let client = connect_for_test(format!("http://{}", mock.addr), "t".into()).expect("client");
        let list = json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": { "name": "remuda_instance_list", "arguments": {} }
        });
        let resp = handle_rpc(&list, &client).await.expect("list");
        assert_eq!(resp["result"]["isError"], json!(false));
        let text = resp["result"]["content"][0]["text"].as_str().expect("text");
        let body: Value = serde_json::from_str(text).expect("json");
        assert_eq!(body["items"][0]["name"], json!("reviewer"));
        assert_eq!(body["items"][0]["cwd"], json!("/tmp/wt"));
        assert_eq!(body["items"][0]["host"], json!("sg"));

        let keys = json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": {
                "name": "remuda_instance_keys",
                "arguments": { "instanceId": "ins_test", "keys": ["enter"] }
            }
        });
        let resp = handle_rpc(&keys, &client).await.expect("keys");
        assert_eq!(resp["result"]["isError"], json!(false), "{resp}");

        let send = json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "tools/call",
            "params": {
                "name": "remuda_fleet_send",
                "arguments": { "all": true, "text": "PAUSE git commits" }
            }
        });
        let resp = handle_rpc(&send, &client).await.expect("fleet send");
        assert_eq!(resp["result"]["isError"], json!(false), "{resp}");
        let text = resp["result"]["content"][0]["text"].as_str().expect("text");
        let body: Value = serde_json::from_str(text).expect("json");
        assert_eq!(body["accepted"], json!(1));
        assert_eq!(body["failed"], json!(0));
        assert_eq!(body["results"][0]["instanceId"], json!("ins_test"));
        assert_eq!(body["text"], json!("PAUSE git commits"));

        let keys = json!({
            "jsonrpc": "2.0",
            "id": 13,
            "method": "tools/call",
            "params": {
                "name": "remuda_fleet_keys",
                "arguments": { "all": true, "kind": "claude", "keys": ["esc"] }
            }
        });
        let resp = handle_rpc(&keys, &client).await.expect("fleet keys");
        assert_eq!(resp["result"]["isError"], json!(false), "{resp}");
        let text = resp["result"]["content"][0]["text"].as_str().expect("text");
        let body: Value = serde_json::from_str(text).expect("json");
        assert_eq!(body["operation"], json!("tty.write"));
        assert_eq!(body["accepted"], json!(1));
        assert_eq!(body["keys"], json!(["esc"]));

        // A kind filter that matches nothing still returns a summary, not an error.
        let miss = json!({
            "jsonrpc": "2.0",
            "id": 14,
            "method": "tools/call",
            "params": {
                "name": "remuda_fleet_send",
                "arguments": { "all": true, "kinds": ["codex"], "text": "hi" }
            }
        });
        let resp = handle_rpc(&miss, &client).await.expect("fleet send miss");
        assert_eq!(resp["result"]["isError"], json!(false), "{resp}");
        let text = resp["result"]["content"][0]["text"].as_str().expect("text");
        let body: Value = serde_json::from_str(text).expect("json");
        assert_eq!(body["accepted"], json!(0));
        assert_eq!(body["skipped"], json!(1));
    }

    #[tokio::test]
    async fn fleet_keys_rejects_unknown_key_before_hub_call() {
        let client = dummy_client();
        let call = json!({
            "jsonrpc": "2.0",
            "id": 15,
            "method": "tools/call",
            "params": {
                "name": "remuda_fleet_keys",
                "arguments": { "all": true, "keys": ["nope"] }
            }
        });
        let resp = handle_rpc(&call, &client).await.expect("response");
        assert_eq!(resp["result"]["isError"], json!(true), "{resp}");
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("unknown key"), "{text}");
    }

    #[tokio::test]
    async fn tools_call_create_against_mock_hub() {
        let mock = spawn_mock_hub().await;
        let client = connect_for_test(format!("http://{}", mock.addr), "t".into()).expect("client");
        let call = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "remuda_instance_create",
                "arguments": { "host": "hst_1", "prompt": "cargo test" }
            }
        });
        let resp = handle_rpc(&call, &client).await.expect("response");
        assert_eq!(resp["result"]["isError"], json!(false));
        let text = resp["result"]["content"][0]["text"].as_str().expect("text");
        let body: Value = serde_json::from_str(text).expect("json tool body");
        assert_eq!(body["instance"]["instanceId"], json!("ins_test"));
        assert_eq!(body["instance"]["hostId"], json!("hst_1"));
    }

    #[tokio::test]
    async fn tools_call_fleet_run_is_error_when_hub_404() {
        let mock = spawn_mock_hub().await;
        let client = connect_for_test(format!("http://{}", mock.addr), "t".into()).expect("client");
        let call = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "remuda_fleet_run",
                "arguments": { "hosts": ["hst_1"], "prompt": "cargo test" }
            }
        });
        let resp = handle_rpc(&call, &client).await.expect("response");
        assert_eq!(resp["result"]["isError"], json!(true));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("not deployed") || text.contains("/v1/fleet"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn serve_rpc_ndjson_roundtrip() {
        let mock = spawn_mock_hub().await;
        let client = connect_for_test(format!("http://{}", mock.addr), "t".into()).expect("client");
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
            "\n",
        );
        let mut out = Vec::new();
        serve_rpc(BufReader::new(input.as_bytes()), &mut out, client)
            .await
            .expect("serve");
        let lines: Vec<&str> = std::str::from_utf8(&out)
            .expect("utf8")
            .lines()
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(lines.len(), 2);
        let list: Value = serde_json::from_str(lines[1]).expect("list json");
        assert!(
            list["result"]["tools"]
                .as_array()
                .expect("tools")
                .iter()
                .any(|t| t["name"] == "remuda_instance_create")
        );
    }

    #[tokio::test]
    async fn serve_rpc_lsp_framing() {
        let client = dummy_client();
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#;
        let input = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let mut out = Vec::new();
        serve_rpc(BufReader::new(input.as_bytes()), &mut out, client)
            .await
            .expect("serve");
        let text = std::str::from_utf8(&out).expect("utf8");
        assert!(text.to_ascii_lowercase().contains("content-length:"));
        assert!(text.contains("\"jsonrpc\":\"2.0\""));
    }
}
