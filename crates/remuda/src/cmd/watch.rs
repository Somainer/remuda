//! `remuda watch` — roster-driven worker screen watcher (M1 batch 5b).
//!
//! One pass (`--once`, the default) asks the Hub to read every live worker's
//! screen through the Node, classify it and persist the classification on the
//! roster row, then prints a compact table (or `--json`).
//!
//! `--follow` re-runs the observation on an interval, prints only workers
//! whose classification changed since the last tick, and exits 0 once every
//! active worker is done (retired workers drop out of the active set). All
//! classification and echo-suppression logic lives in the Hub/Protocol; this
//! command only renders.

use std::collections::BTreeSet;

use clap::Args;
use serde_json::Value;

use super::hub_client::{HubOpts, block_on};
use super::registry::Entrypoint;

/// Default follow poll interval (seconds).
const DEFAULT_INTERVAL_SECS: u64 = 3;

#[derive(Args)]
#[command(
    about = "Classify live workers from their screens (working/done/blocked/idle/stalled/gone/failed)."
)]
pub(crate) struct WatchArgs {
    #[command(flatten)]
    hub: HubOpts,
    /// Restrict to one project (`prj_…`).
    #[arg(long)]
    project: Option<String>,
    /// Read screens once, print, exit (the default).
    #[arg(long)]
    once: bool,
    /// Stream classification changes until every worker is done or retired.
    #[arg(long)]
    follow: bool,
    /// Emit JSON instead of a table.
    #[arg(long)]
    json: bool,
    /// Follow poll interval in seconds.
    #[arg(long, default_value_t = DEFAULT_INTERVAL_SECS)]
    interval_secs: u64,
    /// Override the stall quiet-window in minutes.
    #[arg(long)]
    stall_mins: Option<i64>,
}

impl Entrypoint for WatchArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        block_on(async move { run(self).await })
    }
}

async fn run(args: WatchArgs) -> anyhow::Result<i32> {
    let client = args.hub.connect()?;
    let mut body = serde_json::Map::new();
    if let Some(project) = &args.project {
        body.insert("projectId".into(), Value::String(project.clone()));
    }
    if let Some(stall) = args.stall_mins {
        body.insert("stallMins".into(), stall.into());
    }
    let body = Value::Object(body);

    if args.follow {
        follow(
            &client,
            body,
            args.project.clone(),
            args.interval_secs.max(1),
            args.json,
        )
        .await
    } else {
        let value = client.post("/v1/workers/observe", &body).await?;
        let gates = fetch_gate_jobs(&client, args.project.as_deref()).await;
        if args.json {
            let mut combined = serde_json::Map::new();
            if let Some(object) = value.as_object() {
                combined.extend(object.clone());
            }
            combined.insert("gateJobs".into(), Value::Array(gates));
            super::hub_client::print_json(&Value::Object(combined))?;
        } else {
            print_table(items(&value), true);
            print_gate_jobs(&gates, true);
        }
        Ok(0)
    }
}

/// Fetch active gate jobs (batch 6 co-lanes).
async fn fetch_gate_jobs(
    client: &remuda_hub_client::HubClient,
    project: Option<&str>,
) -> Vec<Value> {
    let path = match project {
        Some(project) => format!("/v1/projects/{project}/gate?state=running&"),
        None => "/v1/gate/jobs?active=true&".to_owned(),
    };
    client
        .get(&path)
        .await
        .ok()
        .and_then(|value| value.get("items").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

/// Render active gate jobs under the worker table.
fn print_gate_jobs(jobs: &[Value], with_header: bool) {
    if jobs.is_empty() {
        return;
    }
    if with_header {
        println!();
        println!(
            "{:<30}  {:<7}  {:<9}  STEP_NAME",
            "GATE BRANCH", "MODE", "STATE"
        );
    }
    for job in jobs {
        let branch = job
            .get("branch")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .chars()
            .take(30)
            .collect::<String>();
        let mode = job.get("mode").and_then(Value::as_str).unwrap_or("?");
        let state = job.get("state").and_then(Value::as_str).unwrap_or("?");
        let step = job
            .get("steps")
            .and_then(Value::as_array)
            .and_then(|steps| {
                steps
                    .iter()
                    .rev()
                    .find(|step| {
                        step.get("status")
                            .and_then(Value::as_str)
                            .is_some_and(|status| status != "skipped")
                    })
                    .map(|step| {
                        format!(
                            "{}:{}",
                            step.get("name").and_then(Value::as_str).unwrap_or("?"),
                            step.get("status").and_then(Value::as_str).unwrap_or("?")
                        )
                    })
            })
            .unwrap_or_else(|| "-".into());
        println!("{branch:<30}  {mode:<7}  {state:<9}  {step}");
    }
}

/// Change signature for one gate job.
fn gate_signature(job: &Value) -> String {
    let steps = job
        .get("steps")
        .and_then(Value::as_array)
        .map(|steps| {
            steps
                .iter()
                .filter_map(|step| {
                    Some(format!(
                        "{}:{}",
                        step.get("name")?.as_str()?,
                        step.get("status")?.as_str()?
                    ))
                })
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    format!(
        "{}:{}:{steps}",
        job.get("id").and_then(Value::as_str).unwrap_or(""),
        job.get("state").and_then(Value::as_str).unwrap_or("")
    )
}

/// Poll until every active worker is done. The first tick prints the current
/// table; later ticks print only changed rows.
async fn follow(
    client: &remuda_hub_client::HubClient,
    body: Value,
    project: Option<String>,
    interval_secs: u64,
    as_json: bool,
) -> anyhow::Result<i32> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut gate_seen: BTreeSet<String> = BTreeSet::new();
    let mut first = true;
    loop {
        let value = client.post("/v1/workers/observe", &body).await?;
        let items = items(&value);
        let gates = fetch_gate_jobs(client, project.as_deref()).await;
        if as_json {
            for row in &items {
                let signature = signature(row);
                if first || !seen.contains(&signature) {
                    println!("{}", serde_json::to_string(row)?);
                }
                seen.insert(signature);
            }
            for job in &gates {
                let signature = gate_signature(job);
                if first || !gate_seen.contains(&signature) {
                    println!("{}", serde_json::to_string(job)?);
                }
                gate_seen.insert(signature);
            }
        } else {
            let changed: Vec<&Value> = items
                .iter()
                .filter(|row| first || !seen.contains(&signature(row)))
                .collect();
            if first {
                print_table(items.clone(), true);
            } else if !changed.is_empty() {
                print_table(changed.iter().copied().cloned().collect(), false);
            }
            for row in &items {
                seen.insert(signature(row));
            }
            let changed_gates: Vec<&Value> = gates
                .iter()
                .filter(|job| first || !gate_seen.contains(&gate_signature(job)))
                .collect();
            if !changed_gates.is_empty() {
                print_gate_jobs(
                    &changed_gates
                        .iter()
                        .map(|job| (*job).clone())
                        .collect::<Vec<_>>(),
                    first,
                );
            }
            for job in &gates {
                gate_seen.insert(gate_signature(job));
            }
        }

        // Exit when there are no active workers and no active gate jobs, or
        // every worker is done.
        let all_done = !items.is_empty() && items.iter().all(is_done) && gates.is_empty();
        let none_active = items.is_empty() && gates.is_empty();
        if all_done || none_active {
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({"allDone": true}))?
                );
            } else {
                eprintln!("watch: every worker is done or retired");
            }
            return Ok(0);
        }
        first = false;
        tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
    }
}

fn items(value: &Value) -> Vec<Value> {
    value
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Status classification label, preferring the point-in-time watch status and
/// falling back to the durable lifecycle state.
fn label(row: &Value) -> String {
    row.get("watch")
        .and_then(|watch| watch.get("status"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            row.get("state")
                .and_then(|state| state.get("state"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string()
        })
}

fn sha_or_reason(row: &Value) -> String {
    if let Some(watch) = row.get("watch") {
        if let Some(sha) = watch.get("sha").and_then(Value::as_str) {
            return sha.to_string();
        }
        if let Some(reason) = watch.get("reason").and_then(Value::as_str) {
            return reason.to_string();
        }
    }
    if let Some(sha) = row["state"].get("sha").and_then(Value::as_str) {
        return sha.to_string();
    }
    if let Some(reason) = row["state"].get("reason").and_then(Value::as_str) {
        return reason.to_string();
    }
    "-".to_string()
}

fn is_done(row: &Value) -> bool {
    label(row) == "done" || row["state"].get("state").and_then(Value::as_str) == Some("retired")
}

/// Change signature for `--follow` dedupe: worker + status + evidence.
fn signature(row: &Value) -> String {
    let id = row.get("id").and_then(Value::as_str).unwrap_or("");
    let detail = row
        .get("watch")
        .and_then(|watch| watch.get("detail"))
        .and_then(Value::as_str)
        .unwrap_or("");
    format!("{id}:{}:{}:{detail}", label(row), sha_or_reason(row))
}

fn print_table(rows: Vec<Value>, with_header: bool) {
    if rows.is_empty() {
        if with_header {
            println!("watch: no active workers in scope");
        }
        return;
    }
    struct Col {
        name: &'static str,
        cells: Vec<String>,
    }
    let mut name = Col {
        name: "NAME",
        cells: Vec::new(),
    };
    let mut status = Col {
        name: "STATUS",
        cells: Vec::new(),
    };
    let mut evidence = Col {
        name: "SHA/REASON",
        cells: Vec::new(),
    };
    let mut detail = Col {
        name: "DETAIL",
        cells: Vec::new(),
    };
    for row in &rows {
        name.cells.push(
            row.get("name")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string(),
        );
        status.cells.push(label(row));
        evidence.cells.push(truncate(sha_or_reason(row), 40));
        detail.cells.push(truncate(
            row.get("watch")
                .and_then(|watch| watch.get("detail"))
                .and_then(Value::as_str)
                .unwrap_or("-")
                .to_string(),
            48,
        ));
    }
    let cols = [&name, &status, &evidence, &detail];
    let widths: Vec<usize> = cols
        .iter()
        .map(|col| {
            col.name
                .len()
                .max(col.cells.iter().map(String::len).max().unwrap_or(0))
        })
        .collect();
    if with_header {
        for (col, width) in cols.iter().zip(&widths) {
            print!("{:<width$}  ", col.name, width = width);
        }
        println!();
    }
    for i in 0..rows.len() {
        print!("{:<width$}  ", name.cells[i], width = widths[0]);
        print!("{:<width$}  ", status.cells[i], width = widths[1]);
        print!("{:<width$}  ", evidence.cells[i], width = widths[2]);
        print!("{:<width$}", detail.cells[i], width = widths[3]);
        println!();
    }
}

fn truncate(value: String, max: usize) -> String {
    if value.chars().count() <= max {
        value
    } else {
        let head: String = value.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}
