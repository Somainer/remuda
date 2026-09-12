//! `remuda fleet run` — Hub fleet HTTP (`docs/design/proposal.md` §4.6).

use anyhow::{Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use super::hub_client::{HubClient, HubOpts, block_on, print_json};

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
        let client = HubClient::connect(&hub)?;
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
