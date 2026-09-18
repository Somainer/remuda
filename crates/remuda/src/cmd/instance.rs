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
/// How long a *forwarded* `send` polls the command ledger for the Node's
/// mirrored journal to converge a lost RPC reply (queued + reconciling →
/// accepted / settled) before printing whatever it has. An offline send
/// (`forwarded = false`) never polls.
const SEND_RESOLVE_POLL_MS: u64 = 12_000;

/// `remuda instance` subcommands.
#[derive(clap::Args)]
#[command(about = "Create, list, send, wait, read, keys, stop, or rm a Hub-backed instance.")]
pub(crate) struct Args {
    #[command(flatten)]
    hub: HubOpts,
    #[command(subcommand)]
    pub(crate) command: InstanceCommand,
}

impl super::registry::Entrypoint for Args {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        run(self.hub, self.command).map(|()| 0)
    }
}

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
        worktree::require_operator_environment()?;
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
    let hosts = if client.caller_context().await?.origin == remuda_hub_client::CallerOrigin::Agent {
        Vec::new()
    } else {
        client.list_hosts().await.unwrap_or_default()
    };
    let items = list_instance_items(client, host).await?;
    let projected: Vec<Value> = items
        .iter()
        .map(|item| project_instance(item, &hosts))
        .collect();
    Ok(json!({ "items": projected, "nextCursor": null }))
}

async fn list_instance_items(client: &HubClient, host: Option<&str>) -> Result<Vec<Value>> {
    let caller = client.caller_context().await?;
    if caller.origin == remuda_hub_client::CallerOrigin::Agent {
        let mut items = Vec::new();
        for id in caller.instance_id.iter().chain(&caller.children) {
            let item = client.get(&format!("/v1/instances/{id}")).await?;
            if host.is_none_or(|host| host.is_empty() || item["hostId"] == host) {
                items.push(item);
            }
        }
        return Ok(items);
    }
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
    let mut body = client
        .post_command(instance_id, "instance.send", payload, command_id)
        .await?;
    resolve_command_state(client, instance_id, &mut body).await;
    Ok(body)
}

/// Replace a still-`queued`, forwarded send row in `body` with its resolved
/// ledger row and annotate `body` with `resolvedState` / `failureReason`, so the
/// operator sees how the send actually settled (accepted / settled, or settled
/// with a `rejected` settlement) rather than only the queued row the POST
/// returned.
///
/// A row that came back `forwarded = false` is short-circuited: the host was
/// offline, no forward intent exists and therefore no deadline is ever armed —
/// polling could only burn the whole window before printing the honest
/// `queued` result. Best effort: on any error the POST's row stands
/// (docs/design/evidence/instance-send-1.md).
pub(crate) async fn resolve_command_state(client: &HubClient, instance_id: &str, body: &mut Value) {
    let command_id = body
        .pointer("/command/commandId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let Some(command_id) = command_id else {
        return;
    };
    let mut resolved = body["command"].clone();
    // Offline host: the command was never forwarded, so nothing will converge
    // it on the Hub and a poll would just wait out the window. Print queued now.
    if !resolved
        .get("forwarded")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        annotate_resolved(body, &resolved);
        return;
    }
    // A forwarded send reaches accepted / a rejected settlement synchronously
    // on the RPC reply; only a lost reply rests at queued + reconciling and is
    // converged later by the Node's mirrored journal. Poll for that convergence,
    // bounded so a genuinely hung Node still returns rather than hanging.
    let deadline = Instant::now() + Duration::from_millis(SEND_RESOLVE_POLL_MS);
    let mut first = true;
    loop {
        let state = resolved.get("state").and_then(Value::as_str).unwrap_or("");
        if matches!(state, "accepted" | "settled") {
            break;
        }
        if !first {
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        first = false;
        let Ok(listing) = client
            .get(&format!("/v1/instances/{instance_id}/commands?limit=50"))
            .await
        else {
            break;
        };
        if let Some(found) = listing
            .get("commands")
            .and_then(Value::as_array)
            .and_then(|rows| {
                rows.iter().find(|row| {
                    row.get("commandId").and_then(Value::as_str) == Some(command_id.as_str())
                })
            })
        {
            resolved = found.clone();
        }
    }
    annotate_resolved(body, &resolved);
}

/// Stamp the resolved ledger row back onto the POST body and surface a
/// rejection's reason from the §2.5 settlement (never a top-level field).
fn annotate_resolved(body: &mut Value, resolved: &Value) {
    let state = resolved
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("queued")
        .to_owned();
    body["command"] = resolved.clone();
    if let Some(obj) = body.as_object_mut() {
        obj.insert("resolvedState".into(), json!(state));
        let rejected = resolved
            .pointer("/settlement/outcome")
            .and_then(Value::as_str)
            == Some("rejected");
        if rejected
            && let Some(reason) = resolved
                .pointer("/settlement/reason")
                .and_then(Value::as_str)
        {
            obj.insert("failureReason".into(), json!(reason));
        }
    }
}

/// Whether `condition` needs to scan journal events (vs. being decidable from
/// the live instance snapshot alone).
fn condition_scans_events(condition: &str) -> bool {
    condition.starts_with("line:")
        || matches!(
            condition,
            "done" | "observed-update" | "interaction" | "workflow-terminal" | "run-terminal"
        )
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
    let mut after: u64 = after_seq.and_then(|s| s.parse().ok()).unwrap_or(0);
    // Dedupe/order by seq: descending pages and repeat polls can otherwise
    // present the same event twice. Only NEW rows are cloned into it (entry /
    // or_insert_with), so a long wait never deep-copies the whole journal on
    // every poll.
    let mut by_seq: std::collections::BTreeMap<u64, Value> = std::collections::BTreeMap::new();
    // Lowest window floor a descent ever stopped at without reaching the
    // queried cursor, latched FOR THE LIFE OF THE CALL. Advancing `after` to
    // the tail makes later `(after, durable]` reads empty, and the Hub reports
    // reached_after_seq=true for an empty range by construction — so
    // completeness must never be re-derived from that trivial read. Once a
    // floor below is un-descended, the scan stays partial for the call.
    let mut lowest_unreached: Option<u64> = None;
    // Mirrors the web client's fillGap bound: ~16 windows of 2000 rows.
    const MAX_FILL_PAGES: u32 = 16;
    loop {
        // First page is the tail of (after, durable]. Further pages descend
        // with beforeSeq = fromSeq - 1 (same after) until reachedAfterSeq.
        let mut before: Option<String> = None;
        let mut poll_reached_bottom = false;
        let mut last_floor: Option<u64> = None;
        for _ in 0..MAX_FILL_PAGES {
            let journal = client
                .get_journal(instance_id, Some(&after.to_string()), before.as_deref())
                .await?;
            if let Some(batch) = journal.get("events").and_then(Value::as_array) {
                for event in batch {
                    let seq = super::agents::sequence(&event["seq"]);
                    by_seq.entry(seq).or_insert_with(|| event.clone());
                }
            }
            let page_reached = journal
                .get("reachedAfterSeq")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let from_seq = journal
                .get("fromSeq")
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<u64>().ok());
            last_floor = from_seq;
            if page_reached {
                poll_reached_bottom = true;
                break;
            }
            // Cursor comes from the last event actually received, never from
            // durableSeq; descend below the window floor first.
            let Some(floor) = from_seq else { break };
            before = Some(floor.saturating_sub(1).to_string());
        }
        if !poll_reached_bottom {
            // The descent budget could not reach the queried cursor: latch the
            // lowest floor whose rows below were never covered. Only extend
            // the partial region downward; if a later poll reaches the cursor
            // this remains the honest record of what this call never scanned.
            if let Some(floor) = last_floor {
                lowest_unreached = Some(
                    lowest_unreached
                        .map_or(floor, |low| low.min(floor))
                        .min(after),
                );
            }
        }
        // Highest received seq drives the next tail read and asOfSeq; it
        // advances even while partial because completeness is tracked
        // separately in lowest_unreached.
        let as_of = by_seq.keys().next_back().copied().unwrap_or(after);
        after = as_of;
        let window_complete = lowest_unreached.is_none();
        let snapshot = instance_snapshot(client, instance_id).await?;
        let lifecycle = snapshot.get("lifecycle").and_then(Value::as_str);
        let activity = snapshot.get("activity").and_then(Value::as_str);
        // A POSITIVE verdict is authoritative even on a partial window: a
        // matched line/event, or a terminal lifecycle for done/run-terminal,
        // is real regardless of rows the tail cut. Only a negative verdict is
        // qualified by the partial window (reported on timeout below).
        if until_met(condition, by_seq.values(), lifecycle, activity)? {
            let matched_line = matching_wait_line(condition, by_seq.values());
            return Ok(json!({
                "reason": "condition-met",
                "instanceId": instance_id,
                "until": condition,
                "condition": condition,
                "asOfSeq": as_of.to_string(),
                "lifecycle": lifecycle,
                "activity": activity,
                "matchedLine": matched_line,
                "eventCount": by_seq.len(),
                "outstandingWork": false,
                "windowComplete": window_complete,
                "reachedAfterSeq": window_complete,
            }));
        }
        if Instant::now() >= deadline {
            // Only an event-scanning condition blocked behind an un-descended
            // floor is still outstanding. A snapshot-only verdict (idle/
            // blocked) or a fully-covered event scan timed out on live state.
            let outstanding_work = condition_scans_events(condition) && !window_complete;
            return Ok(json!({
                "reason": "timeout",
                "instanceId": instance_id,
                "until": condition,
                "condition": condition,
                "asOfSeq": as_of.to_string(),
                "lifecycle": lifecycle,
                "activity": activity,
                "matchedLine": Value::Null,
                "eventCount": by_seq.len(),
                "outstandingWork": outstanding_work,
                // False names the latched bounded-window case: rows below the
                // received tail were never descended to, so a negative
                // event-scanning verdict is partial.
                "windowComplete": window_complete,
                "reachedAfterSeq": window_complete,
            }));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Ask the Hub for the live screen of a PTY-carried session.
///
/// `Ok(None)` means "this session has no readable screen" — an offline host, a
/// driver without a terminal, or a route an older Hub does not serve. Callers
/// fall back to the journal and *say* they did, rather than printing an empty
/// grid that looks like a blank terminal.
async fn live_screen(client: &HubClient, instance_id: &str) -> Option<Value> {
    let body = client
        .get(&format!("/v1/instances/{instance_id}/screen"))
        .await
        .ok()?;
    (body.get("supported").and_then(Value::as_bool) == Some(true)).then_some(body)
}

pub(crate) async fn read(
    client: &HubClient,
    instance_id: &str,
    after_seq: Option<&str>,
    lines: usize,
    source: &str,
) -> Result<Value> {
    // D-028 §4.6: a `shell-pty` (or any PTY) session holds a real emulator
    // grid, and that is what `--source screen` should mean. Only when no live
    // screen exists does this fall back to scraping screen-shaped events out
    // of the journal, which is all this command could ever do before.
    if source.trim().eq_ignore_ascii_case("screen")
        && let Some(screen) = live_screen(client, instance_id).await
    {
        let grid: Vec<String> = screen
            .get("lines")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .map(|row| row.as_str().unwrap_or_default().to_owned())
                    .collect()
            })
            .unwrap_or_default();
        let truncated = grid.len() > lines;
        let printed = if truncated {
            grid[grid.len() - lines..].to_vec()
        } else {
            grid
        };
        return Ok(json!({
            "instanceId": instance_id,
            "source": "screen",
            "screenSource": screen.get("source").cloned().unwrap_or(Value::Null),
            "cols": screen.get("cols").cloned().unwrap_or(Value::Null),
            "rows": screen.get("rows").cloned().unwrap_or(Value::Null),
            "cursor": screen.get("cursor").cloned().unwrap_or(Value::Null),
            "altScreen": screen.get("altScreen").cloned().unwrap_or(Value::Null),
            "lines": printed,
            "truncated": truncated,
            "completeness": if truncated { "truncated" } else { "complete" },
        }));
    }
    let journal = client.get_journal(instance_id, after_seq, None).await?;
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
        "driverHint": "no live screen for this session; \
                       showing journal events (a PTY carrier serves GET /v1/instances/<id>/screen)",
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
    let items = list_instance_items(client, None).await?;
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
    Ok(client.get(&format!("/v1/instances/{instance_id}")).await?)
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

pub(crate) fn until_met<'a>(
    until: &str,
    events: impl IntoIterator<Item = &'a Value>,
    lifecycle: Option<&str>,
    activity: Option<&str>,
) -> Result<bool> {
    // Collect references (cheap, no JSON clone) so the match arms below can
    // scan repeatedly.
    let events: Vec<&Value> = events.into_iter().collect();
    if let Some(pattern) = until.strip_prefix("line:") {
        let re = regex::Regex::new(pattern)
            .with_context(|| format!("invalid --until regex {pattern:?}"))?;
        let text = line_scan_text(events.iter().copied());
        return Ok(line_regex_matches(&re, &text));
    }
    Ok(match until {
        "idle" => is_idle(lifecycle, activity),
        "done" => {
            events.iter().any(|event| event_is_run_terminal(event))
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
        "interaction" => events.iter().any(|event| event_is_interaction(event)),
        "workflow-terminal" => events.iter().any(|event| event_is_workflow_terminal(event)),
        "run-terminal" => {
            events.iter().any(|event| event_is_run_terminal(event))
                || lifecycle
                    .is_some_and(|life| matches!(life, "closed" | "failed" | "idle" | "terminated"))
        }
        other => {
            events.iter().any(|event| event_is_run_terminal(event))
                || lifecycle
                    .is_some_and(|life| matches!(life, "closed" | "failed" | "idle" | "terminated"))
                || other == "done"
        }
    })
}

/// Concatenate the human-readable strings of the line-wait-eligible events,
/// one event's text block per newline-separated section, without cloning the
/// events into a new JSON array.
fn line_scan_text<'a>(events: impl Iterator<Item = &'a Value>) -> String {
    let mut out = String::new();
    for event in events.filter(|event| line_wait_event(event)) {
        let part = collect_strings(event);
        if part.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&part);
    }
    out
}

fn matching_wait_line<'a>(
    until: &str,
    events: impl IntoIterator<Item = &'a Value>,
) -> Option<String> {
    let pattern = until.strip_prefix("line:")?;
    let re = regex::Regex::new(pattern).ok()?;
    let text = line_scan_text(events.into_iter());
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
    use std::sync::Arc;

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

    /// A Hub double that serves a 100k-row journal with the same bounded tail
    /// semantics as Store::read_journal (newest 2000 rows; fromSeq floor;
    /// reachedAfterSeq only when the floor is exactly after+1, and true for an
    /// empty range). `match_seq` carries "BUILD FAILED". Counts requests.
    async fn spawn_window_hub(
        match_seq: i64,
    ) -> (
        std::net::SocketAddr,
        tokio::task::JoinHandle<()>,
        Arc<std::sync::atomic::AtomicU64>,
    ) {
        use axum::{
            Router,
            extract::{Path, Query, State},
            response::Json,
            routing::get,
        };
        use std::collections::HashMap;
        use std::sync::atomic::Ordering;

        const N: i64 = 100_000;
        const WINDOW: i64 = 2_000;

        #[derive(Clone)]
        struct Hub {
            match_seq: i64,
            requests: Arc<std::sync::atomic::AtomicU64>,
        }

        async fn journal(
            State(hub): State<Hub>,
            Query(q): Query<HashMap<String, String>>,
            Path(_id): Path<String>,
        ) -> Json<Value> {
            hub.requests.fetch_add(1, Ordering::SeqCst);
            let after: i64 = q.get("afterSeq").and_then(|s| s.parse().ok()).unwrap_or(0);
            let before: Option<i64> = q.get("beforeSeq").and_then(|s| s.parse().ok());
            // Inclusive high bound, clamped to durable (mirrors the Hub).
            let high = before.map(|b| b.min(N)).unwrap_or(N);
            // Newest WINDOW rows of (after, high], ascending.
            let lo = (after + 1).max(high - WINDOW + 1);
            let events: Vec<Value> = if high > after && lo <= high {
                (lo..=high)
                    .map(|seq| {
                        let text = if seq == hub.match_seq {
                            "BUILD FAILED".to_string()
                        } else {
                            format!("line {seq}")
                        };
                        json!({"seq": seq.to_string(),
                            "event":{"type":"message","payload":{"role":"assistant","text":text}}})
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let reached = events.first().map(|_| lo == after + 1).unwrap_or(true);
            let from_seq = events.first().map(|_| lo.to_string());
            Json(json!({
                "durableSeq": N.to_string(),
                "fromSeq": from_seq,
                "reachedAfterSeq": reached,
                "events": events,
            }))
        }
        async fn instance(Path(_id): Path<String>) -> Json<Value> {
            Json(json!({ "instanceId": "ins_deep", "lifecycle": "running", "activity": "working" }))
        }
        let hub = Hub {
            match_seq,
            requests: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        let requests = hub.requests.clone();
        let app = Router::new()
            .route("/v1/instances/{id}/journal", get(journal))
            .route("/v1/instances/{id}", get(instance))
            .with_state(hub);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, server, requests)
    }

    /// Match sits at seq 500, far below the 16-page descent floor (~68k). Over
    /// several polls the latched windowComplete stays false (the empty
    /// top-of-tail read on poll 2+ must not be mis-read as "reached") and the
    /// timeout verdict names outstanding work.
    #[tokio::test]
    async fn wait_partial_window_stays_incomplete_across_polls() {
        let (addr, server, requests) = spawn_window_hub(500).await;
        let client =
            HubClient::new(format!("http://{addr}"), Some("test-token".into()), None).unwrap();
        let result = wait(&client, "ins_deep", "line:BUILD FAILED", Some("0"), 5_000)
            .await
            .expect("wait returns");

        assert_eq!(result["reason"], json!("timeout"));
        assert_eq!(result["windowComplete"], json!(false));
        assert_eq!(result["reachedAfterSeq"], json!(false));
        assert_eq!(result["outstandingWork"], json!(true));
        // Poll 1 = 16 descent reads; poll 2+ read the now-empty (after,
        // durable] top of tail. At least one such trivial read happened and did
        // NOT reset completeness.
        assert!(requests.load(std::sync::atomic::Ordering::SeqCst) >= 17);

        server.abort();
    }

    /// Match sits at seq 70_000, inside the 16-page descended range, but the
    /// descent still stops above seq 1: a POSITIVE match returns condition-met
    /// immediately even though the window is partial.
    #[tokio::test]
    async fn wait_line_match_met_inside_partial_window() {
        let (addr, server, requests) = spawn_window_hub(70_000).await;
        let client =
            HubClient::new(format!("http://{addr}"), Some("test-token".into()), None).unwrap();
        let result = wait(&client, "ins_deep", "line:BUILD FAILED", Some("0"), 5_000)
            .await
            .expect("wait returns");

        assert_eq!(result["reason"], json!("condition-met"));
        assert_eq!(result["windowComplete"], json!(false));
        assert_eq!(result["reachedAfterSeq"], json!(false));
        assert_eq!(result["outstandingWork"], json!(false));
        assert!(
            result["matchedLine"]
                .as_str()
                .unwrap()
                .contains("BUILD FAILED")
        );
        // Exactly one descent pass (16 pages), no busy second poll.
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 16);

        server.abort();
    }

    /// Cursor 0, journal deeper than the descent budget, `--until idle`:
    /// snapshot-only verdicts must not be suppressed by a partial window — the
    /// wait returns condition-met with windowComplete=false.
    #[tokio::test]
    async fn wait_idle_met_on_partial_window() {
        use axum::{Router, extract::Path, response::Json, routing::get};

        // 40_000 rows behind one bounded tail window: the 16-page * 2000-row
        // descent from cursor 0 cannot reach seq 1, so windowComplete stays
        // false on every poll.
        async fn journal(Path(_id): Path<String>) -> Json<Value> {
            Json(json!({
                "durableSeq": "40000",
                "fromSeq": "38001",
                "reachedAfterSeq": false,
                "events": (38001..=40000)
                    .map(|seq| json!({ "seq": seq.to_string() }))
                    .collect::<Vec<_>>(),
            }))
        }
        async fn instance(Path(_id): Path<String>) -> Json<Value> {
            Json(json!({ "instanceId": "ins_deep", "lifecycle": "running", "activity": "idle" }))
        }
        let app = Router::new()
            .route("/v1/instances/{id}/journal", get(journal))
            .route("/v1/instances/{id}", get(instance));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client =
            HubClient::new(format!("http://{addr}"), Some("test-token".into()), None).unwrap();
        let result = wait(&client, "ins_deep", "idle", Some("0"), 5_000)
            .await
            .expect("wait returns");

        assert_eq!(result["reason"], json!("condition-met"));
        assert_eq!(result["windowComplete"], json!(false));
        assert_eq!(result["reachedAfterSeq"], json!(false));
        assert_eq!(result["outstandingWork"], json!(false));
        assert_eq!(result["activity"], json!("idle"));

        server.abort();
    }
}
