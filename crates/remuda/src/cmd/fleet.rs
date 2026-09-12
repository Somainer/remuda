//! `remuda fleet run|send` — Hub fleet HTTP (`docs/design/proposal.md` §4.6).

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use super::hub_client::{HubClient, HubOpts, block_on, host_matches_labels, print_json};
use super::instance::send;

/// `remuda fleet` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum FleetCommand {
    /// Create one instance per selected host and return `fleetId`.
    Run {
        /// Explicit host ids (`hst_…`, comma-separated or repeated).
        #[arg(long, value_delimiter = ',')]
        hosts: Vec<String>,
        /// Placement labels (`key=value`, comma-separated or repeated).
        #[arg(long, value_delimiter = ',')]
        labels: Vec<String>,
        /// Maximum number of instances (defaults to `hosts.len()` when set).
        #[arg(long)]
        max: Option<u32>,
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
        /// Initial prompt.
        #[arg(long)]
        prompt: Option<String>,
    },
    /// Broadcast a prompt to running instances (`--all` or `--labels`).
    Send {
        /// Every known instance.
        #[arg(long)]
        all: bool,
        /// Instances whose host matches these labels.
        #[arg(long, value_delimiter = ',')]
        labels: Vec<String>,
        /// Read the prompt from a file.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Prompt text after the flags.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        text: Vec<String>,
    },
}

/// Inputs for [`fleet_run`].
#[derive(Debug, Clone)]
pub(crate) struct FleetRunOpts {
    pub hosts: Vec<String>,
    pub labels: Vec<String>,
    pub max: Option<u32>,
    pub kind: String,
    pub driver: String,
    pub workspace_id: Option<String>,
    pub title: Option<String>,
    pub prompt: Option<String>,
}

/// Run a `remuda fleet` subcommand.
pub(crate) fn run(hub: HubOpts, command: FleetCommand) -> Result<()> {
    block_on(async move {
        let client = hub.connect()?;
        match command {
            FleetCommand::Run {
                hosts,
                labels,
                max,
                kind,
                driver,
                workspace_id,
                title,
                prompt,
            } => {
                let value = fleet_run(
                    &client,
                    FleetRunOpts {
                        hosts,
                        labels,
                        max,
                        kind,
                        driver,
                        workspace_id,
                        title,
                        prompt,
                    },
                )
                .await?;
                print_json(&value)
            }
            FleetCommand::Send {
                all,
                labels,
                file,
                text,
            } => {
                let prompt = load_fleet_text(file, text)?;
                let value = fleet_send(&client, all, labels, &prompt).await?;
                print_json(&value)
            }
        }
    })
}

pub(crate) async fn fleet_run(client: &HubClient, opts: FleetRunOpts) -> Result<Value> {
    if !opts.hosts.is_empty() && !opts.labels.is_empty() {
        bail!("use --hosts or --labels, not both");
    }
    let mut spec = json!({
        "kind": opts.kind,
        "driver": opts.driver,
    });
    if let Some(workspace_id) = &opts.workspace_id {
        spec["workspaceId"] = json!(workspace_id);
    }
    if let Some(title) = &opts.title {
        spec["title"] = json!(title);
    }
    if let Some(prompt) = &opts.prompt {
        spec["prompt"] = json!(prompt);
    }
    if opts.hosts.len() == 1 {
        spec["placement"] = json!({ "host": opts.hosts[0] });
    } else if !opts.labels.is_empty() {
        spec["placement"] = json!({ "labels": opts.labels });
    } else if opts.hosts.is_empty() {
        spec["placement"] = json!({ "kind": "any" });
    }

    let mut body = json!({ "spec": spec });
    if !opts.hosts.is_empty() {
        body["hosts"] = json!(opts.hosts);
    }
    if !opts.labels.is_empty() {
        body["labels"] = json!(opts.labels);
    }
    let max = opts
        .max
        .or((!opts.hosts.is_empty()).then_some(opts.hosts.len() as u32));
    if let Some(max) = max {
        body["max"] = json!(max);
    }

    Ok(client.create_fleet(&body).await?)
}

/// Inputs for [`fleet_send`].
#[derive(Debug, Clone)]
pub(crate) struct FleetSendOpts {
    pub all: bool,
    pub labels: Vec<String>,
    pub text: String,
}

pub(crate) async fn fleet_send(
    client: &HubClient,
    all: bool,
    labels: Vec<String>,
    text: &str,
) -> Result<Value> {
    fleet_send_opts(
        client,
        FleetSendOpts {
            all,
            labels,
            text: text.to_string(),
        },
    )
    .await
}

pub(crate) async fn fleet_send_opts(client: &HubClient, opts: FleetSendOpts) -> Result<Value> {
    if opts.all && !opts.labels.is_empty() {
        bail!("use --all or --labels, not both");
    }
    if !opts.all && opts.labels.is_empty() {
        bail!("provide --all or --labels");
    }
    if opts.text.is_empty() {
        bail!("provide a prompt or --file");
    }
    let hosts = client.list_hosts().await.unwrap_or_default();
    let allowed_hosts: Option<Vec<String>> = if opts.all {
        None
    } else {
        Some(
            hosts
                .iter()
                .filter(|host| host_matches_labels(host, &opts.labels))
                .filter_map(|host| {
                    host.get("hostId")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect(),
        )
    };
    if let Some(ids) = &allowed_hosts
        && ids.is_empty()
    {
        bail!("no host matched labels {:?}", opts.labels);
    }
    let instances = client.list_instances().await?;
    let mut results = Vec::new();
    for item in instances {
        let Some(instance_id) = item.get("instanceId").and_then(Value::as_str) else {
            continue;
        };
        let host_id = item.get("hostId").and_then(Value::as_str).unwrap_or("");
        if let Some(ids) = &allowed_hosts
            && !ids.iter().any(|id| id == host_id)
        {
            continue;
        }
        match send(
            client,
            instance_id,
            &opts.text,
            None,
            "native-turn",
            "fleet",
        )
        .await
        {
            Ok(value) => results.push(json!({
                "instanceId": instance_id,
                "hostId": host_id,
                "ok": true,
                "command": value,
            })),
            Err(err) => results.push(json!({
                "instanceId": instance_id,
                "hostId": host_id,
                "ok": false,
                "error": err.to_string(),
            })),
        }
    }
    Ok(json!({
        "sent": results.len(),
        "text": opts.text,
        "results": results,
    }))
}

fn load_fleet_text(file: Option<PathBuf>, text: Vec<String>) -> Result<String> {
    if let Some(path) = file {
        return std::fs::read_to_string(&path)
            .map_err(|err| anyhow::anyhow!("read {}: {err}", path.display()));
    }
    if text.is_empty() {
        bail!("provide a prompt or --file");
    }
    Ok(text.join(" "))
}
