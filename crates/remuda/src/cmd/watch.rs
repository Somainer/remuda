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
    about = "Classify live workers from their screens (working/done/blocked/idle/stalled/gone)."
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
        follow(&client, body, args.interval_secs.max(1), args.json).await
    } else {
        let value = client.post("/v1/workers/observe", &body).await?;
        if args.json {
            super::hub_client::print_json(&value)?;
        } else {
            print_table(items(&value), true);
        }
        Ok(0)
    }
}

/// Poll until every active worker is done. The first tick prints the current
/// table; later ticks print only changed rows.
async fn follow(
    client: &remuda_hub_client::HubClient,
    body: Value,
    interval_secs: u64,
    as_json: bool,
) -> anyhow::Result<i32> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut first = true;
    loop {
        let value = client.post("/v1/workers/observe", &body).await?;
        let items = items(&value);
        if as_json {
            for row in &items {
                let signature = signature(row);
                if first || !seen.contains(&signature) {
                    println!("{}", serde_json::to_string(row)?);
                }
                seen.insert(signature);
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
        }

        // Exit when there are no active workers or every one of them is done.
        let all_done = !items.is_empty() && items.iter().all(is_done);
        let none_active = items.is_empty();
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
