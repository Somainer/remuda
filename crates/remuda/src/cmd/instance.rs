//! `remuda instance create|list|send|wait|read|keys|stop|rm` — Hub HTTP control plane.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use super::hub_client::{HubClient, HubOpts, block_on, pick_host, print_json};
use super::worktree;

/// Default `wait` budget; protocol.md §8.1.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Hard ceiling so `wait` cannot loop unbounded.
const MAX_TIMEOUT_MS: u64 = 300_000;
/// Default `read --lines` (herdr-style viewport).
const DEFAULT_LINES: usize = 120;

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
        /// Driver kind (`claude-print`, `generic-pty`; `pty` is an alias).
        #[arg(long, default_value = "claude-print")]
        driver: String,
        /// Optional workspace id.
        #[arg(long)]
        workspace_id: Option<String>,
        /// Working directory recorded on the instance workspace.
        #[arg(long)]
        cwd: Option<String>,
        /// Named worktree (`remuda worktree create` / `git worktree add -b wt/<name>/…`).
        #[arg(long)]
        worktree: Option<String>,
        /// Unique live name (`[a-z][a-z0-9_-]{0,31}`). Stored as the instance title.
        #[arg(long)]
        name: Option<String>,
        /// UI title (defaults to `--name`).
        #[arg(long)]
        title: Option<String>,
        /// Initial prompt (Hub `initialInput`).
        #[arg(long)]
        prompt: Option<String>,
        /// Read the initial prompt from a task-brief file.
        #[arg(long)]
        prompt_file: Option<PathBuf>,
        /// Optional command id for create idempotency.
        #[arg(long)]
        command_id: Option<String>,
    },
    /// List instances (`name`, `kind`, `status`, `cwd`, `host`).
    #[command(visible_alias = "ls")]
    List(super::agents::ListArgs),
    /// Send a prompt / steer to a running instance.
    Send {
        /// Instance id (`ins_…`) or `--name`.
        instance_id: String,
        /// Prompt text.
        #[arg(long)]
        text: Option<String>,
        /// Read prompt text from a file (herdr-style `--file`).
        #[arg(long, visible_alias = "input-file")]
        file: Option<PathBuf>,
        /// Optional command id.
        #[arg(long)]
        command_id: Option<String>,
        /// `native-turn` or `task`.
        #[arg(long, default_value = "native-turn")]
        completion_scope: String,
        /// Prompt words after the instance id.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        prompt: Vec<String>,
    },
    /// List pending prompts, or reply with --option, --text, or --answer JSON.
    Respond(super::instance_interaction::RespondOpts),
    /// Poll until idle, done, blocked, or a journal line matches.
    Wait {
        /// Instance id (`ins_…`) or name.
        instance_id: String,
        /// `idle`, `done`, `blocked`, or `line:<regex>`.
        #[arg(long)]
        until: Option<String>,
        /// Legacy wait condition (`run-terminal`, `workflow-terminal`, `interaction`, `observed-update`).
        #[arg(long)]
        condition: Option<String>,
        /// Exclusive journal seq to start after.
        #[arg(long)]
        after_seq: Option<String>,
        /// Timeout in milliseconds (default 30000, max 300000).
        #[arg(long, visible_alias = "timeout-ms", default_value_t = DEFAULT_TIMEOUT_MS)]
        timeout: u64,
    },
    /// Read screen or journal output.
    Read {
        /// Instance id (`ins_…`) or name.
        instance_id: String,
        /// Exclusive journal seq to start after.
        #[arg(long)]
        after_seq: Option<String>,
        /// Max lines / events (herdr `--lines`).
        #[arg(long, visible_alias = "limit", default_value_t = DEFAULT_LINES)]
        lines: usize,
        /// `journal` (mirrored events) or `screen` (tty/raw_tty observations).
        #[arg(long, default_value = "journal")]
        source: String,
    },
    /// Send logical keys (`enter`, `esc`, `ctrl+c`) through `tty.write`.
    Keys {
        /// Instance id (`ins_…`) or name.
        instance_id: String,
        /// Logical keys. Validated before any bytes are sent.
        #[arg(required = true)]
        keys: Vec<String>,
    },
    /// Cancel a run or close an instance.
    Stop {
        /// Instance id (`ins_…`) or name.
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
    /// Close an instance (`instance.close`).
    Rm {
        /// Instance id (`ins_…`) or name.
        instance_id: String,
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
    pub cwd: Option<String>,
    pub worktree: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub prompt: Option<String>,
    pub command_id: Option<String>,
}

/// Run a `remuda instance` subcommand.
pub(crate) fn run(hub: HubOpts, command: InstanceCommand) -> Result<()> {
    block_on(async move {
        let client = hub.connect()?;
        match command {
            InstanceCommand::Create {
                host,
                labels,
                kind,
                driver,
                workspace_id,
                cwd,
                worktree,
                name,
                title,
                prompt,
                prompt_file,
                command_id,
            } => {
                let prompt = match (prompt, prompt_file) {
                    (Some(_), Some(_)) => bail!("use --prompt or --prompt-file, not both"),
                    (Some(text), None) => Some(text),
                    (None, Some(path)) => Some(load_file(&path)?),
                    (None, None) => None,
                };
                let value = create(
                    &client,
                    CreateOpts {
                        host,
                        labels,
                        kind,
                        driver: normalize_driver(&driver),
                        workspace_id,
                        cwd,
                        worktree,
                        name,
                        title,
                        prompt,
                        command_id,
                    },
                )
                .await?;
                print_json(&value)
            }
            InstanceCommand::List(args) => {
                super::agents::list(std::sync::Arc::new(client), args).await
            }
            InstanceCommand::Send {
                instance_id,
                text,
                file,
                command_id,
                completion_scope,
                prompt,
            } => {
                let text = load_send_text(text, file, prompt)?;
                let instance_id = resolve_instance_id(&client, &instance_id).await?;
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
            InstanceCommand::Respond(opts) => {
                let value = super::instance_interaction::respond(&client, opts).await?;
                print_json(&value)
            }
            InstanceCommand::Wait {
                instance_id,
                until,
                condition,
                after_seq,
                timeout,
            } => {
                let instance_id = resolve_instance_id(&client, &instance_id).await?;
                let target = until.or(condition).unwrap_or_else(|| "done".into());
                let value = wait(
                    &client,
                    &instance_id,
                    &target,
                    after_seq.as_deref(),
                    timeout,
                )
                .await?;
                print_json(&value)
            }
            InstanceCommand::Read {
                instance_id,
                after_seq,
                lines,
                source,
            } => {
                let instance_id = resolve_instance_id(&client, &instance_id).await?;
                let value =
                    read(&client, &instance_id, after_seq.as_deref(), lines, &source).await?;
                print_json(&value)
            }
            InstanceCommand::Keys { instance_id, keys } => {
                let instance_id = resolve_instance_id(&client, &instance_id).await?;
                let value = send_keys(&client, &instance_id, &keys).await?;
                print_json(&value)
            }
            InstanceCommand::Stop {
                instance_id,
                scope,
                run_id,
                command_id,
            } => {
                let instance_id = resolve_instance_id(&client, &instance_id).await?;
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
            InstanceCommand::Rm {
                instance_id,
                command_id,
            } => {
                let instance_id = resolve_instance_id(&client, &instance_id).await?;
                let value = stop(
                    &client,
                    &instance_id,
                    "instance",
                    None,
                    command_id.as_deref(),
                )
                .await?;
                print_json(&value)
            }
        }
    })
}

pub(crate) async fn create(client: &HubClient, mut opts: CreateOpts) -> Result<Value> {
    if opts.host.is_some() && !opts.labels.is_empty() {
        bail!("use --host or --labels, not both");
    }
    if let Some(name) = &opts.name {
        worktree::validate_name(name)?;
    }
    opts.driver = normalize_driver(&opts.driver);
    if let Some(wt_name) = opts.worktree.clone() {
        worktree::validate_name(&wt_name)?;
        let record = worktree::ensure(&wt_name, None)?;
        if opts.cwd.is_none() {
            opts.cwd = Some(record.path.clone());
        }
        if opts.workspace_id.is_none() {
            opts.workspace_id = Some(record.path.clone());
        }
        if opts.name.is_none() {
            opts.name = Some(wt_name);
        }
    }
    if opts.title.is_none() {
        opts.title = opts.name.clone();
    }
    if opts.workspace_id.is_none() {
        opts.workspace_id = opts.cwd.clone();
    }

    let mut body = json!({
        "kind": opts.kind,
        "driver": opts.driver,
    });
    if let Some(workspace_id) = &opts.workspace_id {
        body["workspaceId"] = json!(workspace_id);
    }
    if let Some(cwd) = &opts.cwd {
        body["cwd"] = json!(cwd);
    }
    if let Some(worktree) = &opts.worktree {
        body["worktree"] = json!(worktree);
    }
    if let Some(name) = &opts.name {
        body["name"] = json!(name);
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
    if tty_attach_driver(&opts.driver) {
        body["requiredCapabilities"] = json!(["tty-attach", "live-attach"]);
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

pub(crate) async fn list_instances(client: &HubClient, host: Option<&str>) -> Result<Value> {
    let hosts = client.list_hosts().await.unwrap_or_default();
    let items = list_instance_items(client, host).await?;
    let projected: Vec<Value> = items
        .iter()
        .map(|item| project_instance(item, &hosts))
        .collect();
    Ok(json!({ "items": projected, "nextCursor": null }))
}

async fn list_instance_items(client: &HubClient, host: Option<&str>) -> Result<Vec<Value>> {
    let path = match host {
        Some(id) if !id.is_empty() => format!("/v1/instances?hostId={id}"),
        _ => "/v1/instances".into(),
    };
    let body = client.get(&path).await?;
    Ok(body
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
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
        bail!("timeout {timeout_ms} exceeds max {MAX_TIMEOUT_MS}");
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
        let snapshot = instance_snapshot(client, instance_id).await?;
        let lifecycle = snapshot.get("lifecycle").and_then(Value::as_str);
        let activity = snapshot.get("activity").and_then(Value::as_str);
        if until_met(condition, &events, lifecycle, activity)? {
            let matched_line = matching_wait_line(condition, &events);
            return Ok(json!({
                "reason": "condition-met",
                "instanceId": instance_id,
                "until": condition,
                "condition": condition,
                "asOfSeq": after,
                "lifecycle": lifecycle,
                "activity": activity,
                "matchedLine": matched_line,
                "eventCount": events.len(),
                "outstandingWork": false,
            }));
        }
        if Instant::now() >= deadline {
            return Ok(json!({
                "reason": "timeout",
                "instanceId": instance_id,
                "until": condition,
                "condition": condition,
                "asOfSeq": after,
                "lifecycle": lifecycle,
                "activity": activity,
                "matchedLine": Value::Null,
                "eventCount": events.len(),
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
    lines: usize,
    source: &str,
) -> Result<Value> {
    let journal = client.get_journal(instance_id, after_seq).await?;
    let all_events = journal
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let source = source.trim().to_ascii_lowercase();
    let (mut events, resolved_source) = match source.as_str() {
        "journal" => (all_events, "journal".to_string()),
        "screen" => {
            let tty: Vec<Value> = all_events
                .iter()
                .filter(|event| is_screen_event(event))
                .cloned()
                .collect();
            if tty.is_empty() {
                (all_events, "journal-fallback".to_string())
            } else {
                (tty, "screen".to_string())
            }
        }
        other => bail!("source must be screen or journal, got {other}"),
    };
    let truncated = events.len() > lines;
    if truncated {
        let skip = events.len() - lines;
        events = events[skip..].to_vec();
    }
    let text = collect_strings(&Value::Array(events.clone()));
    let line_vec: Vec<&str> = text.lines().collect();
    let line_start = line_vec.len().saturating_sub(lines);
    let printed: Vec<&str> = line_vec[line_start..].to_vec();
    Ok(json!({
        "instanceId": instance_id,
        "source": resolved_source,
        "durableSeq": journal.get("durableSeq").cloned().unwrap_or(json!("0")),
        "observations": events,
        "lines": printed,
        "truncated": truncated,
        "completeness": if truncated { "truncated" } else { "complete" },
        "asOfSeq": journal.get("durableSeq").cloned().unwrap_or(json!("0")),
        "driverHint": "screen/keys require a tty-attach driver such as generic-pty",
    }))
}

pub(crate) async fn send_keys(
    client: &HubClient,
    instance_id: &str,
    keys: &[String],
) -> Result<Value> {
    let encoded = encode_keys(keys)?;
    let payload = json!({
        "instanceId": instance_id,
        "keys": encoded.names,
        "dataBase64": encoded.data_base64,
        "source": "cli",
    });
    let command = client
        .post_command(instance_id, "tty.write", payload, None)
        .await?;
    Ok(json!({
        "command": command,
        "keys": encoded.names,
        "dataBase64": encoded.data_base64,
        "driverHint": "tty.write is delivered to a tty-attach driver (generic-pty / claude-pty)",
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

fn load_file(path: &PathBuf) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}

fn load_send_text(
    text: Option<String>,
    file: Option<PathBuf>,
    prompt: Vec<String>,
) -> Result<String> {
    if let Some(path) = file {
        return load_file(&path);
    }
    if let Some(text) = text {
        return Ok(text);
    }
    if !prompt.is_empty() {
        return Ok(prompt.join(" "));
    }
    bail!("provide a prompt, --text, or --file")
}

pub(crate) async fn resolve_instance_id(client: &HubClient, id_or_name: &str) -> Result<String> {
    if id_or_name.starts_with("ins_") {
        return Ok(id_or_name.to_string());
    }
    let items = client.list_instances().await?;
    let mut matches = Vec::new();
    for item in &items {
        let id = item.get("instanceId").and_then(Value::as_str).unwrap_or("");
        let title = item.get("title").and_then(Value::as_str).unwrap_or("");
        let name = item.get("name").and_then(Value::as_str).unwrap_or("");
        if id == id_or_name || title == id_or_name || name == id_or_name {
            matches.push(id.to_string());
        }
    }
    match matches.as_slice() {
        [id] => Ok(id.clone()),
        [] => Ok(id_or_name.to_string()),
        _ => bail!("ambiguous instance name {id_or_name}"),
    }
}

async fn instance_snapshot(client: &HubClient, instance_id: &str) -> Result<Value> {
    let items = client.list_instances().await?;
    Ok(items
        .into_iter()
        .find(|item| item.get("instanceId").and_then(Value::as_str) == Some(instance_id))
        .unwrap_or(json!({})))
}

pub(crate) fn project_instance(item: &Value, hosts: &[Value]) -> Value {
    let host_id = item.get("hostId").and_then(Value::as_str).unwrap_or("");
    let host = hosts
        .iter()
        .find(|h| h.get("hostId").and_then(Value::as_str) == Some(host_id));
    let host_label = host
        .and_then(|h| {
            h.get("hostname")
                .and_then(Value::as_str)
                .or_else(|| h.get("label").and_then(Value::as_str))
                .or_else(|| h.get("hostId").and_then(Value::as_str))
        })
        .unwrap_or(host_id);
    let instance_id = item.get("instanceId").and_then(Value::as_str).unwrap_or("");
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| item.get("title").and_then(Value::as_str))
        .unwrap_or(instance_id);
    let status = item
        .get("activity")
        .and_then(Value::as_str)
        .or_else(|| item.get("lifecycle").and_then(Value::as_str))
        .unwrap_or("unknown");
    let cwd = item
        .get("cwd")
        .and_then(Value::as_str)
        .or_else(|| item.get("workspaceId").and_then(Value::as_str))
        .unwrap_or("");
    json!({
        "name": name,
        "kind": item.get("kind").cloned().unwrap_or(json!("")),
        "status": status,
        "cwd": cwd,
        "host": host_label,
        "instanceId": instance_id,
        "hostId": host_id,
        "lifecycle": item.get("lifecycle").cloned().unwrap_or(json!("")),
        "activity": item.get("activity").cloned().unwrap_or(json!("")),
        "connectivity": item.get("connectivity").cloned().unwrap_or(json!("unknown")),
        "hostOnline": host.and_then(|h| h.get("online")).cloned().unwrap_or(Value::Null),
        "worktree": item.get("worktree").filter(|v| !v.is_null()).cloned().unwrap_or(json!(cwd)),
        "driver": item.get("driver").cloned().unwrap_or(json!("")),
        "title": item.get("title").cloned().unwrap_or(json!(name)),
        "workspaceId": item.get("workspaceId").cloned().unwrap_or(json!(cwd)),
    })
}

/// Strip leading whitespace and one TUI list marker so `(?m)^DONE` matches
/// pane text like `• DONE`. The raw journal line is unchanged; this is
/// match-time only.
pub(crate) fn normalize_wait_line(raw: &str) -> &str {
    let trimmed = raw.trim_start();
    for marker in ["•", "●", "◆", "▸", "▪", "-", "*", ">"] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return rest.trim_start();
        }
    }
    trimmed
}

fn line_regex_matches(re: &regex::Regex, text: &str) -> bool {
    first_matching_wait_line(re, text).is_some()
}

fn looks_like_wait_brief_echo(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("further input") || lower.contains("tui bullet") || lower.contains("for example")
}

/// Raw (unnormalized) line that satisfies `re`, if any.
fn first_matching_wait_line<'a>(re: &regex::Regex, text: &'a str) -> Option<&'a str> {
    text.lines().find(|line| {
        if looks_like_wait_brief_echo(line) {
            return false;
        }
        re.is_match(line) || re.is_match(normalize_wait_line(line))
    })
}

pub(crate) fn normalize_driver(driver: &str) -> String {
    match driver {
        "pty" | "generic-pty" | "generic_pty" | "genericPty" => "generic-pty".into(),
        "shell" | "shell-pty" | "terminal" => "shell-pty".into(),
        other => other.to_string(),
    }
}

pub(crate) fn tty_attach_driver(driver: &str) -> bool {
    matches!(
        normalize_driver(driver).as_str(),
        "generic-pty" | "claude-pty" | "claude-bg" | "shell-pty"
    )
}

pub(crate) fn until_met(
    until: &str,
    events: &[Value],
    lifecycle: Option<&str>,
    activity: Option<&str>,
) -> Result<bool> {
    if let Some(pattern) = until.strip_prefix("line:") {
        let re = regex::Regex::new(pattern)
            .with_context(|| format!("invalid --until regex {pattern:?}"))?;
        let filtered: Vec<Value> = events
            .iter()
            .filter(|event| line_wait_event(event))
            .cloned()
            .collect();
        let text = collect_strings(&Value::Array(filtered));
        return Ok(line_regex_matches(&re, &text));
    }
    Ok(match until {
        "idle" => is_idle(lifecycle, activity),
        "done" => {
            events.iter().any(event_is_run_terminal)
                || lifecycle.is_some_and(|life| matches!(life, "closed" | "failed" | "terminated"))
        }
        "blocked" => {
            // A historical requested/answered event cannot satisfy a current
            // blocked wait once the authoritative snapshot says idle.
            activity
                .map(|a| matches!(a, "blocked" | "waiting-interaction"))
                .unwrap_or_else(|| {
                    lifecycle.is_some_and(|life| life.eq_ignore_ascii_case("blocked"))
                })
        }
        "observed-update" => !events.is_empty(),
        "interaction" => events.iter().any(event_is_interaction),
        "workflow-terminal" => events.iter().any(event_is_workflow_terminal),
        "run-terminal" => {
            events.iter().any(event_is_run_terminal)
                || lifecycle
                    .is_some_and(|life| matches!(life, "closed" | "failed" | "idle" | "terminated"))
        }
        other => {
            events.iter().any(event_is_run_terminal)
                || lifecycle
                    .is_some_and(|life| matches!(life, "closed" | "failed" | "idle" | "terminated"))
                || other == "done"
        }
    })
}

fn matching_wait_line(until: &str, events: &[Value]) -> Option<String> {
    let pattern = until.strip_prefix("line:")?;
    let re = regex::Regex::new(pattern).ok()?;
    let filtered: Vec<Value> = events
        .iter()
        .filter(|event| line_wait_event(event))
        .cloned()
        .collect();
    let text = collect_strings(&Value::Array(filtered));
    first_matching_wait_line(&re, &text).map(str::to_string)
}

fn is_idle(lifecycle: Option<&str>, activity: Option<&str>) -> bool {
    let life = lifecycle.unwrap_or("");
    if matches!(life, "requested" | "creating" | "starting" | "") {
        return false;
    }
    if activity.is_some_and(|a| a.eq_ignore_ascii_case("blocked")) {
        return false;
    }
    matches!(life, "ready" | "idle") || activity.is_some_and(|a| a.eq_ignore_ascii_case("idle"))
}

/// Encoded logical keys ready for `tty.write`.
pub(crate) struct EncodedKeys {
    /// Normalized key names.
    pub names: Vec<String>,
    /// Base64 of the PTY bytes.
    pub data_base64: String,
}

pub(crate) fn encode_keys(keys: &[String]) -> Result<EncodedKeys> {
    if keys.is_empty() {
        bail!("keys requires at least one key");
    }
    let mut names = Vec::new();
    let mut bytes = Vec::new();
    for key in keys {
        let (name, encoded) = encode_one(key)?;
        names.push(name);
        bytes.extend_from_slice(&encoded);
    }
    Ok(EncodedKeys {
        names,
        data_base64: base64_encode(&bytes),
    })
}

fn encode_one(key: &str) -> Result<(String, Vec<u8>)> {
    let raw = key.trim();
    if raw.is_empty() {
        bail!("empty key");
    }
    if raw.chars().count() == 1 {
        return Ok((raw.to_string(), raw.as_bytes().to_vec()));
    }
    let k = raw.to_ascii_lowercase();
    let bytes = match k.as_str() {
        "enter" | "return" => vec![b'\r'],
        "tab" => vec![b'\t'],
        "esc" | "escape" => vec![0x1b],
        "space" => vec![b' '],
        "backspace" | "bs" => vec![0x7f],
        "delete" | "del" => vec![0x1b, b'[', b'3', b'~'],
        "up" => vec![0x1b, b'[', b'A'],
        "down" => vec![0x1b, b'[', b'B'],
        "right" => vec![0x1b, b'[', b'C'],
        "left" => vec![0x1b, b'[', b'D'],
        "home" => vec![0x1b, b'[', b'H'],
        "end" => vec![0x1b, b'[', b'F'],
        "ctrl+c" | "c-c" => vec![0x03],
        "ctrl+d" => vec![0x04],
        "ctrl+z" => vec![0x1a],
        "ctrl+l" => vec![0x0c],
        "ctrl+u" => vec![0x15],
        other if other.starts_with("ctrl+") && other.len() == 6 => {
            let c = other.as_bytes()[5];
            if c.is_ascii_lowercase() {
                vec![c - b'a' + 1]
            } else {
                bail!("unknown key {key:?}")
            }
        }
        _ => bail!("unknown key {key:?}; try enter, esc, ctrl+c, or a single character"),
    };
    Ok((k, bytes))
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i < input.len() {
        let remaining = input.len() - i;
        let b0 = input[i];
        let b1 = if remaining > 1 { input[i + 1] } else { 0 };
        let b2 = if remaining > 2 { input[i + 2] } else { 0 };
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if remaining > 1 {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if remaining > 2 {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        i += 3;
    }
    out
}

fn collect_strings(value: &Value) -> String {
    let mut out = String::new();
    fn walk(value: &Value, out: &mut String) {
        match value {
            Value::String(s) => {
                if !s.is_empty() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(s);
                }
            }
            Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            Value::Object(map) => map.values().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    walk(value, &mut out);
    out
}

#[cfg(test)]
fn condition_met(condition: &str, events: &[Value], lifecycle: Option<&str>) -> bool {
    until_met(condition, events, lifecycle, None).unwrap_or(false)
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

fn is_screen_event(event: &Value) -> bool {
    let ty = event_type(event);
    if ty.contains("tty") || ty.contains("screen") || ty.contains("terminal.frame") {
        return true;
    }
    if native_name(event).as_deref() == Some("screen") {
        return true;
    }
    completeness_of(event).is_some_and(|value| value.contains("screen"))
}

fn completeness_of(event: &Value) -> Option<String> {
    event
        .get("completeness")
        .and_then(Value::as_str)
        .or_else(|| {
            event
                .get("event")
                .and_then(|inner| inner.get("completeness"))
                .and_then(Value::as_str)
        })
        .map(str::to_ascii_lowercase)
}

fn is_prompt_echo(event: &Value) -> bool {
    native_name(event).as_deref() == Some("prompt_echo")
}

fn native_name(event: &Value) -> Option<String> {
    fn walk(value: &Value) -> Option<String> {
        match value {
            Value::Object(map) => {
                for key in ["nativeName", "native_name"] {
                    if let Some(name) = map.get(key).and_then(Value::as_str) {
                        return Some(name.to_string());
                    }
                }
                map.values().find_map(walk)
            }
            Value::Array(items) => items.iter().find_map(walk),
            _ => None,
        }
    }
    walk(event)
}

fn line_wait_event(event: &Value) -> bool {
    if is_prompt_echo(event) {
        return false;
    }
    if is_screen_event(event) {
        return true;
    }
    if native_name(event).as_deref() == Some("line-matcher")
        || native_name(event).as_deref() == Some("screen")
    {
        return true;
    }
    let ty = event_type(event);
    ty == "message" || ty.ends_with(".message")
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
    ty.contains("interaction") || ty.contains("permission") || ty.contains("approval")
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

    #[test]
    fn idle_ignores_requested_create() {
        assert!(!until_met("idle", &[], Some("requested"), Some("idle")).unwrap());
        assert!(until_met("idle", &[], Some("ready"), Some("idle")).unwrap());
    }

    #[test]
    fn done_does_not_fire_on_create_idle() {
        assert!(!until_met("done", &[], Some("requested"), Some("idle")).unwrap());
        assert!(
            until_met(
                "done",
                &[json!({"type":"run.terminal"})],
                Some("ready"),
                Some("idle")
            )
            .unwrap()
        );
    }

    #[test]
    fn until_line_matches_nested_text() {
        let events = [json!({ "event": { "type": "message", "text": "DONE abcdef" } })];
        assert!(until_met("line:DONE ", &events, None, None).unwrap());
        assert!(!until_met("line:MISSING", &events, None, None).unwrap());
    }

    #[test]
    fn until_line_matches_screen_snapshot() {
        let events = [json!({
            "nativeName": "screen",
            "completeness": "screen-derived",
            "status": "worker output\nDONE\n"
        })];
        assert!(until_met("line:(?m)^DONE", &events, None, None).unwrap());
        assert!(is_screen_event(&events[0]));
    }

    #[test]
    fn normalize_wait_line_strips_indent_and_one_marker() {
        assert_eq!(normalize_wait_line("DONE"), "DONE");
        assert_eq!(normalize_wait_line("  DONE"), "DONE");
        assert_eq!(normalize_wait_line("\tDONE"), "DONE");
        assert_eq!(normalize_wait_line("• DONE"), "DONE");
        assert_eq!(normalize_wait_line("- DONE"), "DONE");
        assert_eq!(normalize_wait_line("* DONE"), "DONE");
        assert_eq!(normalize_wait_line("> DONE"), "DONE");
        assert_eq!(normalize_wait_line("◆ DONE"), "DONE");
        assert_eq!(normalize_wait_line("  • DONE"), "DONE");
        assert_eq!(normalize_wait_line("  -  DONE"), "DONE");
        assert_eq!(normalize_wait_line("\t* DONE extra"), "DONE extra");
        assert_eq!(normalize_wait_line("• • DONE"), "• DONE");
        assert_eq!(normalize_wait_line("DONE •"), "DONE •");
    }

    #[test]
    fn until_line_matches_tui_list_markers() {
        for status in [
            "• DONE",
            "- DONE",
            "* DONE",
            "> DONE",
            "◆ DONE",
            "  • DONE",
            "\t* DONE",
            "output\n  > DONE\n",
        ] {
            let events = [json!({
                "nativeName": "screen",
                "completeness": "screen-derived",
                "status": status,
            })];
            assert!(
                until_met("line:(?m)^DONE", &events, None, None).unwrap(),
                "{status:?}"
            );
        }
        let raw = json!({
            "nativeName": "screen",
            "status": "• DONE"
        });
        assert_eq!(raw["status"], "• DONE");
        assert_eq!(
            matching_wait_line("line:(?m)^DONE", std::slice::from_ref(&raw)).as_deref(),
            Some("• DONE")
        );
        let miss = [json!({ "nativeName": "screen", "status": "not yet" })];
        assert!(!until_met("line:(?m)^DONE", &miss, None, None).unwrap());
        let wrapped_brief = [json!({
            "nativeName": "screen",
            "status": "print a standalone line that starts with\n  DONE (a TUI bullet before DONE is fine). Do not wait for\nfurther input."
        })];
        assert!(!until_met("line:(?m)^DONE", &wrapped_brief, None, None).unwrap());
        assert!(matching_wait_line("line:(?m)^DONE", &wrapped_brief).is_none());
    }

    #[test]
    fn until_line_ignores_prompt_echo() {
        let events = [json!({
            "nativeName": "prompt_echo",
            "status": "print a line that starts with DONE (for example: DONE)."
        })];
        assert!(!until_met("line:DONE", &events, None, None).unwrap());
    }

    #[test]
    fn until_line_ignores_create_prompt_payload() {
        let events = [json!({
            "type": "command",
            "operation": "instance.create",
            "payload": {
                "initialInput": {
                    "text": "create a file then print a line that starts with DONE (for example: DONE)."
                }
            }
        })];
        assert!(!until_met("line:DONE", &events, None, None).unwrap());
    }

    #[test]
    fn encode_keys_validates_before_bytes() {
        assert!(encode_keys(&["nope".into()]).is_err());
        let encoded = encode_keys(&["enter".into(), "ctrl+c".into()]).unwrap();
        assert_eq!(encoded.names, ["enter", "ctrl+c"]);
        assert_eq!(encoded.data_base64, "DQM=");
    }

    #[test]
    fn pty_alias_is_generic_pty() {
        assert_eq!(normalize_driver("pty"), "generic-pty");
        assert!(tty_attach_driver("pty"));
        assert!(!tty_attach_driver("claude-print"));
    }

    #[test]
    fn project_instance_exposes_herdr_columns() {
        let item = json!({
            "instanceId": "ins_1",
            "hostId": "hst_1",
            "kind": "codex",
            "driver": "generic-pty",
            "lifecycle": "ready",
            "activity": "idle",
            "title": "reviewer",
            "workspaceId": "/tmp/wt"
        });
        let hosts = [json!({"hostId":"hst_1","label":"sg","hostname":"box"})];
        let row = project_instance(&item, &hosts);
        assert_eq!(row["name"], json!("reviewer"));
        assert_eq!(row["kind"], json!("codex"));
        assert_eq!(row["status"], json!("idle"));
        assert_eq!(row["cwd"], json!("/tmp/wt"));
        assert_eq!(row["host"], json!("box"));
    }
}
