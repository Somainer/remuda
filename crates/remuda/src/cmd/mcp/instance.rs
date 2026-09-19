//! MCP instance tools: metadata and handler are registered together.

use super::{
    Tool,
    args::{opt_str, reject_removed_args, required_str, send_text_from_args, string_list},
};
use crate::cmd::instance::{CreateOpts, create, list_instances, read, send, send_keys, stop, wait};
use anyhow::Result;
use serde_json::{Value, json};

pub(super) fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
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
                    "commandId": { "type": "string" }
                }
            }),
            |client, args| {
                Box::pin(async move { create(client, create_opts_from_json(&args)?).await })
            },
        ),
        Tool::new(
            "remuda_instance_list",
            "List instances (name, kind, status, cwd, host).",
            json!({
                "type": "object",
                "properties": {
                    "host": { "type": "string" }
                }
            }),
            |client, args| {
                Box::pin(async move { list_instances(client, opt_str(&args, "host")).await })
            },
        ),
        Tool::new(
            "remuda_instance_send",
            "Send a prompt to a running instance. Pass the text inline; reading a local file is CLI-only.",
            json!({
                "type": "object",
                "required": ["instanceId"],
                "properties": {
                    "instanceId": { "type": "string" },
                    "text": { "type": "string" },
                    "commandId": { "type": "string" },
                    "completionScope": { "type": "string" }
                }
            }),
            |client, args| {
                Box::pin(async move {
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
                })
            },
        ),
        Tool::new(
            "remuda_instance_respond",
            "List pending interactions, or answer a displayed option/text through the first-answer-wins broker.",
            json!({"type":"object", "required":["instanceId"], "properties":{
                "instanceId":{"type":"string"}, "interactionId":{"type":"string"},
                "option":{"type":"string"}, "text":{"type":"string"},
                "answer":{"type":"object"}, "commandId":{"type":"string"}
            }}),
            |client, args| {
                Box::pin(async move {
                    crate::cmd::instance_interaction::respond(
                        client,
                        crate::cmd::instance_interaction::RespondOpts {
                            instance_id: required_str(&args, "instanceId")?.to_owned(),
                            interaction_id: opt_str(&args, "interactionId").map(str::to_owned),
                            option: opt_str(&args, "option").map(str::to_owned),
                            text: opt_str(&args, "text").map(str::to_owned),
                            answer: args.get("answer").map(Value::to_string),
                            command_id: opt_str(&args, "commandId").map(str::to_owned),
                        },
                    )
                    .await
                })
            },
        ),
        Tool::new(
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
            |client, args| {
                Box::pin(async move {
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
                })
            },
        ),
        Tool::new(
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
            |client, args| {
                Box::pin(async move {
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
                })
            },
        ),
        Tool::new(
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
            |client, args| {
                Box::pin(async move {
                    let instance_id = required_str(&args, "instanceId")?;
                    send_keys(client, instance_id, &string_list(&args, "keys")).await
                })
            },
        ),
        Tool::new(
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
            |client, args| {
                Box::pin(async move {
                    let instance_id = required_str(&args, "instanceId")?;
                    stop(
                        client,
                        instance_id,
                        opt_str(&args, "scope").unwrap_or("run"),
                        opt_str(&args, "runId"),
                        opt_str(&args, "commandId"),
                    )
                    .await
                })
            },
        ),
        Tool::new(
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
            |client, args| {
                Box::pin(async move {
                    let instance_id = required_str(&args, "instanceId")?;
                    stop(
                        client,
                        instance_id,
                        "instance",
                        None,
                        opt_str(&args, "commandId"),
                    )
                    .await
                })
            },
        ),
    ]
}

fn create_opts_from_json(args: &Value) -> Result<CreateOpts> {
    // `promptFile` is CLI-only for the same reason as `file`; see
    // `args::send_text_from_args` (security-review-2.md M5).
    reject_removed_args(args, &["promptFile"])?;
    let prompt = opt_str(args, "prompt").map(str::to_string);
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
        // The MCP surface does not offer per-launch host capabilities yet.
        capabilities: Vec::new(),
    })
}
