//! `remuda instance create|send|wait|read|stop` — Hub HTTP control plane.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use super::hub_client::{HubClient, HubOpts, block_on, pick_host, print_json};

/// Default `wait` budget; protocol.md §8.1.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Hard ceiling so `wait` cannot loop unbounded.
const MAX_TIMEOUT_MS: u64 = 300_000;

/// `remuda instance` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum InstanceCommand {
    /// Create an instance on a host (`--host`) or matching `--labels`.
    Create {
        /// Target host id (`hst_…`).
        #[arg(long)]
        host: Option<String>,
        /// Placement labels (`key=value`, comma-separated or repeated).
        #[arg(long, value_delimiter = ',')]
        labels: Vec<String>,
        /// Agent kind.
        #[arg(long, default_value = "claude")]
        kind: String,
        /// Driver kind.
        #[arg(long, default_value = "claude-print")]
        driver: String,
        /// Optional workspace id.
        #[arg(long)]
        workspace_id: Option<String>,
        /// UI title.
        #[arg(long)]
        title: Option<String>,
        /// Initial prompt (Hub `initialInput`).
        #[arg(long)]
        prompt: Option<String>,
        /// Optional command id for create idempotency.
        #[arg(long)]
        command_id: Option<String>,
    },
    /// Send a prompt / steer to a running instance.
    Send {
        /// Instance id (`ins_…`).
        instance_id: String,
        /// Prompt text.
        #[arg(long)]
        text: Option<String>,
        /// Read prompt text from a file.
        #[arg(long)]
        input_file: Option<PathBuf>,
        /// Optional command id.
        #[arg(long)]
        command_id: Option<String>,
        /// `native-turn` or `task`.
        #[arg(long, default_value = "native-turn")]
        completion_scope: String,
    },
    /// Poll the mirrored journal until a condition or timeout.
    Wait {
        /// Instance id (`ins_…`).
        instance_id: String,
        /// `run-terminal`, `workflow-terminal`, `interaction`, or `observed-update`.
        #[arg(long, default_value = "run-terminal")]
        condition: String,
        /// Exclusive journal seq to start after.
        #[arg(long)]
        after_seq: Option<String>,
        /// Timeout in milliseconds (default 30000, max 300000).
        #[arg(long, default_value_t = DEFAULT_TIMEOUT_MS)]
        timeout_ms: u64,
    },
    /// Read mirrored journal events.
    Read {
        /// Instance id (`ins_…`).
        instance_id: String,
        /// Exclusive journal seq to start after.
        #[arg(long)]
        after_seq: Option<String>,
        /// Max events to return.
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Cancel a run or close an instance.
    Stop {
        /// Instance id (`ins_…`).
        instance_id: String,
        /// `run` → `instance.cancel`; `instance` → `instance.close`.
        #[arg(long, default_value = "run")]
        scope: String,
        /// Optional run id when `scope=run`.
        #[arg(long)]
        run_id: Option<String>,
        /// Optional command id.
        #[arg(long)]
        command_id: Option<String>,
    },
}

/// Inputs for [`create`].
#[derive(Debug, Clone)]
pub(crate) struct CreateOpts {
    pub host: Option<String>,
    pub labels: Vec<String>,
    pub kind: String,
    pub driver: String,
    pub workspace_id: Option<String>,
    pub title: Option<String>,
    pub prompt: Option<String>,
    pub command_id: Option<String>,
}

/// Run a `remuda instance` subcommand.
pub(crate) fn run(hub: HubOpts, command: InstanceCommand) -> Result<()> {
    block_on(async move {
        let client = HubClient::connect(&hub)?;
        match command {
            InstanceCommand::Create {
                host,
                labels,
                kind,
                driver,
                workspace_id,
                title,
                prompt,
                command_id,
            } => {
                let value = create(
                    &client,
                    CreateOpts {
                        host,
                        labels,
                        kind,
                        driver,
                        workspace_id,
                        title,
                        prompt,
                        command_id,
                    },
                )
                .await?;
                print_json(&value)
            }
            InstanceCommand::Send {
                instance_id,
                text,
                input_file,
                command_id,
                completion_scope,
            } => {
                let text = load_text(text, input_file)?;
                let value = send(
                    &client,
                    &instance_id,
                    &text,
                    command_id.as_deref(),
                    &completion_scope,
                    "cli",
                )
                .await?;
                print_json(&value)
            }
            InstanceCommand::Wait {
                instance_id,
                condition,
                after_seq,
                timeout_ms,
            } => {
                let value = wait(
                    &client,
                    &instance_id,
                    &condition,
                    after_seq.as_deref(),
                    timeout_ms,
                )
                .await?;
                print_json(&value)
            }
            InstanceCommand::Read {
                instance_id,
                after_seq,
                limit,
            } => {
                let value = read(&client, &instance_id, after_seq.as_deref(), limit).await?;
                print_json(&value)
            }
            InstanceCommand::Stop {
                instance_id,
                scope,
                run_id,
                command_id,
            } => {
                let value = stop(
                    &client,
                    &instance_id,
                    &scope,
                    run_id.as_deref(),
                    command_id.as_deref(),
                )
                .await?;
                print_json(&value)
            }
        }
    })
}

pub(crate) async fn create(client: &HubClient, opts: CreateOpts) -> Result<Value> {
    if opts.host.is_some() && !opts.labels.is_empty() {
        bail!("use --host or --labels, not both");
    }
    let mut body = json!({
        "kind": opts.kind,
        "driver": opts.driver,
    });
    if let Some(workspace_id) = &opts.workspace_id {
        body["workspaceId"] = json!(workspace_id);
    }
    if let Some(title) = &opts.title {
        body["title"] = json!(title);
    }
    if let Some(prompt) = &opts.prompt {
        body["prompt"] = json!(prompt);
    }
    if let Some(command_id) = &opts.command_id {
        body["commandId"] = json!(command_id);
    }

    // Hub placement (proposal.md §4.6) accepts hostId, labels[], or any.
    // Still send hostId when the CLI can resolve it so older Hubs that require
    // the field keep working; otherwise Hub pick_hosts runs.
    if let Some(host) = &opts.host {
        body["hostId"] = json!(host);
        body["placement"] = json!({ "host": host });
    } else if !opts.labels.is_empty() {
        body["placement"] = json!({ "labels": opts.labels });
        if let Ok(hosts) = client.list_hosts().await
            && let Ok(host_id) = pick_host(&hosts, &opts.labels)
        {
            body["hostId"] = json!(host_id);
        }
    } else {
        body["placement"] = json!({ "kind": "any" });
        if let Ok(hosts) = client.list_hosts().await
            && let Ok(host_id) = pick_host(&hosts, &[])
        {
            body["hostId"] = json!(host_id);
        }
    }

    Ok(client.create_instance(&body).await?)
}

pub(crate) async fn send(
    client: &HubClient,
    instance_id: &str,
    text: &str,
    command_id: Option<&str>,
    completion_scope: &str,
    origin: &str,
) -> Result<Value> {
    let payload = json!({
        "instanceId": instance_id,
        "input": {
            "type": "prompt",
            "mode": "new-turn",
            "blocks": [{ "type": "text", "text": text }],
            "origin": origin,
        },
        "completionScope": completion_scope,
    });
    Ok(client
        .post_command(instance_id, "instance.send", payload, command_id)
        .await?)
}

pub(crate) async fn wait(
    client: &HubClient,
    instance_id: &str,
    condition: &str,
    after_seq: Option<&str>,
    timeout_ms: u64,
) -> Result<Value> {
    if timeout_ms > MAX_TIMEOUT_MS {
        bail!("timeout-ms {timeout_ms} exceeds max {MAX_TIMEOUT_MS}");
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut after = after_seq.unwrap_or("0").to_string();
    let mut events: Vec<Value> = Vec::new();
    loop {
        let journal = client.get_journal(instance_id, Some(&after)).await?;
        if let Some(batch) = journal.get("events").and_then(Value::as_array) {
            events.extend(batch.iter().cloned());
        }
        if let Some(seq) = journal
            .get("durableSeq")
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            after = seq;
        }
        let lifecycle = instance_lifecycle(client, instance_id).await?;
        if condition_met(condition, &events, lifecycle.as_deref()) {
            return Ok(json!({
                "reason": "condition-met",
                "instanceId": instance_id,
                "condition": condition,
                "asOfSeq": after,
                "lifecycle": lifecycle,
                "events": events,
                "outstandingWork": false,
            }));
        }
        if Instant::now() >= deadline {
            return Ok(json!({
                "reason": "timeout",
                "instanceId": instance_id,
                "condition": condition,
                "asOfSeq": after,
                "lifecycle": lifecycle,
                "events": events,
                "outstandingWork": true,
            }));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

pub(crate) async fn read(
    client: &HubClient,
    instance_id: &str,
    after_seq: Option<&str>,
    limit: usize,
) -> Result<Value> {
    let journal = client.get_journal(instance_id, after_seq).await?;
    let mut events = journal
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let truncated = events.len() > limit;
    events.truncate(limit);
    Ok(json!({
        "instanceId": instance_id,
        "durableSeq": journal.get("durableSeq").cloned().unwrap_or(json!("0")),
        "observations": events,
        "truncated": truncated,
        "completeness": if truncated { "truncated" } else { "complete" },
        "asOfSeq": journal.get("durableSeq").cloned().unwrap_or(json!("0")),
    }))
}

pub(crate) async fn stop(
    client: &HubClient,
    instance_id: &str,
    scope: &str,
    run_id: Option<&str>,
    command_id: Option<&str>,
) -> Result<Value> {
    let (operation, mut payload) = match scope {
        "instance" => ("instance.close", json!({ "instanceId": instance_id })),
        "run" => ("instance.cancel", json!({ "instanceId": instance_id })),
        other => bail!("scope must be run or instance, got {other}"),
    };
    if let Some(run_id) = run_id {
        payload["runId"] = json!(run_id);
    }
    Ok(client
        .post_command(instance_id, operation, payload, command_id)
        .await?)
}

fn load_text(text: Option<String>, input_file: Option<PathBuf>) -> Result<String> {
    if let Some(path) = input_file {
        return std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()));
    }
    text.ok_or_else(|| anyhow::anyhow!("provide --text or --input-file"))
}

async fn instance_lifecycle(client: &HubClient, instance_id: &str) -> Result<Option<String>> {
    let items = client.list_instances().await?;
    Ok(items.into_iter().find_map(|item| {
        let id = item.get("instanceId").and_then(Value::as_str)?;
        if id != instance_id {
            return None;
        }
        item.get("lifecycle")
            .and_then(Value::as_str)
            .map(str::to_string)
    }))
}

pub(crate) fn condition_met(condition: &str, events: &[Value], lifecycle: Option<&str>) -> bool {
    match condition {
        "observed-update" => !events.is_empty(),
        "interaction" => events.iter().any(event_is_interaction),
        "workflow-terminal" => events.iter().any(event_is_workflow_terminal),
        _ => {
            events.iter().any(event_is_run_terminal)
                || lifecycle
                    .is_some_and(|life| matches!(life, "closed" | "failed" | "idle" | "terminated"))
        }
    }
}

fn event_type(event: &Value) -> String {
    event
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| {
            event
                .get("event")
                .and_then(|inner| inner.get("type"))
                .and_then(Value::as_str)
        })
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn event_is_run_terminal(event: &Value) -> bool {
    let ty = event_type(event);
    ty.contains("terminal")
        || ty.contains("complete")
        || ty.contains("failed")
        || ty.contains(".closed")
        || ty == "instance.closed"
}

fn event_is_workflow_terminal(event: &Value) -> bool {
    let ty = event_type(event);
    ty.contains("workflow") && (ty.contains("terminal") || ty.contains("complete"))
}

fn event_is_interaction(event: &Value) -> bool {
    let ty = event_type(event);
    ty.contains("interaction") || ty.contains("permission")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_terminal_matches_nested_event_type() {
        let events = [json!({ "event": { "type": "run.terminal" } })];
        assert!(condition_met("run-terminal", &events, None));
    }

    #[test]
    fn run_terminal_matches_closed_lifecycle() {
        assert!(condition_met("run-terminal", &[], Some("closed")));
        assert!(!condition_met("run-terminal", &[], Some("requested")));
    }

    #[test]
    fn observed_update_requires_events() {
        assert!(!condition_met("observed-update", &[], None));
        assert!(condition_met(
            "observed-update",
            &[json!({"type":"x"})],
            None
        ));
    }
}
