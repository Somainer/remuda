//! `remuda profile` — declared model supply, capability catalog, dry-run.
//!
//! Coordinator design §4.2/§4.4: capability comes from the Hub's built-in
//! catalog; **supply is user-declared** (`declare`), observed evidence comes
//! back over `event`, and `probe --dry-run` answers "what would run where?"
//! for a task spec without spawning anything. No admission result is ever a
//! silent downgrade.

use clap::{Args, Subcommand};
use serde_json::{Value, json};

use super::hub_client::{HubOpts, block_on, print_json};

/// `remuda profile` subcommands.
#[derive(Args)]
#[command(about = "Declare model supply, inspect capabilities, dry-run admission.")]
pub(crate) struct ProfileArgs {
    #[command(flatten)]
    hub: HubOpts,
    #[command(subcommand)]
    command: ProfileCommand,
}

impl super::registry::Entrypoint for ProfileArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        run(self.hub, self.command).map(|()| 0)
    }
}

#[derive(Subcommand)]
enum ProfileCommand {
    /// Show the built-in capability table (family/class/context/effort).
    Catalog,
    /// Show every provider profile with its declared + observed supply.
    List,
    /// Show one profile's supply envelope (windows, cooldowns, concurrency).
    Show {
        /// `pvp_…` profile id.
        id: String,
    },
    /// Declare/replace supply for a profile (observed cooldowns are kept).
    Declare {
        /// `pvp_…` profile id.
        id: String,
        /// Preference order; larger wins (the user's order IS the objective).
        #[arg(long)]
        priority: Option<i64>,
        /// Account-level in-flight ceiling.
        #[arg(long)]
        concurrency_max: Option<i64>,
        /// Reserve for coordinator seats only (`none` / `coordinator-only`).
        #[arg(long)]
        reserve: Option<String>,
        /// Declared primary reset window length, minutes.
        #[arg(long)]
        reset_window_mins: Option<u64>,
        /// Declare a rate-limit window `id:families:durationMins`; repeat.
        /// Families is `*` or comma-separated family names.
        #[arg(long = "window", value_name = "ID:FAMILIES[:MINS]")]
        windows: Vec<String>,
    },
    /// Report observed supply evidence; 429 cools, 529 cools nothing.
    Event {
        /// `pvp_…` profile id.
        id: String,
        /// `textual` or `structured`.
        #[arg(long, default_value = "textual")]
        r#type: String,
        /// Raw screen/probe text (textual events).
        #[arg(long)]
        text: Option<String>,
        /// HTTP status from a probe (e.g. 429 / 529).
        #[arg(long)]
        http_status: Option<u16>,
        /// Model id the event concerns.
        #[arg(long)]
        model: Option<String>,
        /// Structured window as JSON (repeat), Codex rateLimits shape.
        #[arg(long = "window-json")]
        windows: Vec<String>,
    },
    /// Show per-model usage aggregation and budget band (estimates).
    Usage {
        /// `pvp_…` profile id.
        id: String,
        /// Optional estimated USD budget for the warn/stop band.
        #[arg(long)]
        budget_max_usd: Option<f64>,
    },
    /// Dry-run admission: "what would run where?" for a task spec.
    Probe {
        /// Minimum model class (`cheap` / `workhorse` / `frontier`).
        #[arg(long)]
        min_class: Option<String>,
        /// Task class (`research`/`implement`/`review`/`test`/`merge-gate`/`triage`/`docs`).
        #[arg(long)]
        task_class: Option<String>,
        /// Effort tier name.
        #[arg(long)]
        effort: Option<String>,
        /// Expected input tokens; filters context windows.
        #[arg(long)]
        expected_input_tokens: Option<u64>,
        /// Require a 1m long-context model.
        #[arg(long)]
        needs_long_context: bool,
        /// Cost sensitivity (`low` / `normal` / `high`).
        #[arg(long)]
        cost_sensitivity: Option<String>,
        /// Latency sensitivity (`low` / `normal` / `high`).
        #[arg(long)]
        latency_sensitivity: Option<String>,
        /// Estimated USD budget cap.
        #[arg(long)]
        max_usd: Option<f64>,
        /// Wall-clock cap, minutes.
        #[arg(long)]
        max_wall_mins: Option<u64>,
        /// Placement JSON (same shape as instance create).
        #[arg(long)]
        placement: Option<String>,
        /// Explicit host id.
        #[arg(long)]
        host_id: Option<String>,
        /// Pin a provider profile (`pvp_…`).
        #[arg(long)]
        pin_supply: Option<String>,
        /// Pin a model id.
        #[arg(long)]
        pin_model: Option<String>,
    },
}

fn run(hub: HubOpts, command: ProfileCommand) -> anyhow::Result<()> {
    block_on(async move {
        let client = hub.connect()?;
        let value = match command {
            ProfileCommand::Catalog => client.get("/v1/supply/catalog").await?,
            ProfileCommand::List => client.get("/v1/providers").await?,
            ProfileCommand::Show { id } => {
                client.get(&format!("/v1/providers/{id}/supply")).await?
            }
            ProfileCommand::Declare {
                id,
                priority,
                concurrency_max,
                reserve,
                reset_window_mins,
                windows,
            } => {
                let mut supply = json!({});
                if let Some(value) = priority {
                    supply["priority"] = json!(value);
                }
                if let Some(value) = concurrency_max {
                    supply["concurrency"] = json!({ "max": value });
                }
                if let Some(value) = reserve {
                    supply["reserve"] = json!(value);
                }
                if let Some(value) = reset_window_mins {
                    supply["resetWindowMins"] = json!(value);
                }
                if !windows.is_empty() {
                    let parsed: anyhow::Result<Vec<Value>> =
                        windows.iter().map(|raw| declared_window(raw)).collect();
                    supply["windows"] = json!(parsed?);
                }
                client
                    .put(&format!("/v1/providers/{id}/supply"), &supply)
                    .await?
            }
            ProfileCommand::Event {
                id,
                r#type,
                text,
                http_status,
                model,
                windows,
            } => {
                let mut body = json!({ "type": r#type });
                if let Some(value) = text {
                    body["text"] = json!(value);
                }
                if let Some(value) = http_status {
                    body["httpStatus"] = json!(value);
                }
                if let Some(value) = model {
                    body["model"] = json!(value);
                }
                if !windows.is_empty() {
                    let frames: anyhow::Result<Vec<Value>> = windows
                        .iter()
                        .map(|raw| {
                            serde_json::from_str(raw)
                                .map_err(|err| anyhow::anyhow!("--window-json {raw:?}: {err}"))
                        })
                        .collect();
                    body["windows"] = json!(frames?);
                }
                client
                    .post(&format!("/v1/providers/{id}/supply/events"), &body)
                    .await?
            }
            ProfileCommand::Usage { id, budget_max_usd } => {
                let path = match budget_max_usd {
                    Some(value) => format!("/v1/providers/{id}/usage?budgetMaxUsd={value}"),
                    None => format!("/v1/providers/{id}/usage"),
                };
                client.get(&path).await?
            }
            ProfileCommand::Probe {
                min_class,
                task_class,
                effort,
                expected_input_tokens,
                needs_long_context,
                cost_sensitivity,
                latency_sensitivity,
                max_usd,
                max_wall_mins,
                placement,
                host_id,
                pin_supply,
                pin_model,
            } => {
                let mut spec = json!({});
                if let Some(value) = min_class {
                    spec["minClass"] = json!(value);
                }
                if let Some(value) = task_class {
                    spec["class"] = json!(value);
                }
                if let Some(value) = effort {
                    spec["effort"] = json!(value);
                }
                if expected_input_tokens.is_some() || needs_long_context {
                    spec["contextNeed"] = json!({
                        "expectedInputTokens": expected_input_tokens.map(|v| v.to_string()),
                        "needsLongContext": needs_long_context,
                    });
                }
                if let Some(value) = cost_sensitivity {
                    spec["costSensitivity"] = json!(value);
                }
                if let Some(value) = latency_sensitivity {
                    spec["latencySensitivity"] = json!(value);
                }
                if max_usd.is_some() || max_wall_mins.is_some() {
                    spec["budget"] = json!({
                        "maxUsd": max_usd,
                        "maxWallMins": max_wall_mins,
                    });
                }
                if pin_supply.is_some() || pin_model.is_some() {
                    spec["pin"] = json!({
                        "supplyId": pin_supply,
                        "model": pin_model,
                    });
                }
                let mut body = json!({ "taskSpec": spec });
                if let Some(raw) = placement {
                    let value: Value = serde_json::from_str(&raw)
                        .map_err(|err| anyhow::anyhow!("--placement: {err}"))?;
                    body["placement"] = value;
                }
                if let Some(value) = host_id {
                    body["hostId"] = json!(value);
                }
                client.post("/v1/supply/resolve", &body).await?
            }
        };
        print_json(&value)
    })
}

/// Parse `id:families[:durationMins]` into a declared window object.
fn declared_window(raw: &str) -> anyhow::Result<Value> {
    let parts: Vec<&str> = raw.split(':').collect();
    if parts.len() < 2 || parts[0].trim().is_empty() || parts[1].trim().is_empty() {
        anyhow::bail!("--window expects ID:FAMILIES[:MINS], e.g. primary:*:300");
    }
    let applies_to: Vec<String> = parts[1]
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let mut window = json!({
        "id": parts[0].trim(),
        "appliesTo": applies_to,
        "source": "declared",
    });
    if let Some(duration) = parts.get(2).and_then(|v| v.trim().parse::<u64>().ok()) {
        window["windowDurationMins"] = json!(duration);
    }
    Ok(window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_shorthand_parses() {
        let window = declared_window("primary:*:300").unwrap();
        assert_eq!(window["id"], "primary");
        assert_eq!(window["appliesTo"], json!(["*"]));
        assert_eq!(window["windowDurationMins"], 300);
        let window = declared_window("model:es1,seed").unwrap();
        assert_eq!(window["appliesTo"], json!(["es1", "seed"]));
        assert!(window.get("windowDurationMins").is_none());
        assert!(declared_window("nope").is_err());
    }
}
