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

/// Roster reason for a proxied session whose proxy host went away (D-047
/// §B.5). Adjacent to `idle-api-error`; defined on the protocol layer.
const API_ROUTE_DOWN: &str = remuda_protocol::API_ROUTE_DOWN;

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
        let routes = fetch_instance_routes(&client).await;
        let rows = attach_routes(items(&value), &routes);
        if args.json {
            let mut combined = serde_json::Map::new();
            if let Some(object) = value.as_object() {
                for (key, item) in object {
                    if key == "items" {
                        combined.insert(key.clone(), Value::Array(rows.clone()));
                    } else {
                        combined.insert(key.clone(), item.clone());
                    }
                }
            }
            combined.insert("gateJobs".into(), Value::Array(gates));
            super::hub_client::print_json(&Value::Object(combined))?;
        } else {
            print_table(rows, true);
            print_gate_jobs(&gates, true);
        }
        Ok(0)
    }
}

/// Fetch the instance-indexed echoed routes (D-047). The roster row never
/// carries `apiRoute` — the instance projection does — so one fleet read per
/// tick supplies the ROUTE column without touching any Hub surface.
async fn fetch_instance_routes(
    client: &remuda_hub_client::HubClient,
) -> std::collections::BTreeMap<String, Value> {
    let Ok(value) = client.get("/v1/instances").await else {
        return std::collections::BTreeMap::new();
    };
    value
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let id = item.get("instanceId").and_then(Value::as_str)?;
                    // Attach only a real echo; a direct session omits the key,
                    // and a row must not gain a route the Node never reported.
                    Some((id.to_string(), item.get("apiRoute")?.clone()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Attach each row's Node-echoed `apiRoute` in place; returns the same rows.
/// Done client-side from the instance fleet read (see
/// [`fetch_instance_routes`]) so the roster schema stays untouched.
fn attach_routes(
    mut rows: Vec<Value>,
    routes: &std::collections::BTreeMap<String, Value>,
) -> Vec<Value> {
    for row in &mut rows {
        if let Some(instance_id) = row
            .get("instanceId")
            .and_then(Value::as_str)
            .or_else(|| row.pointer("/instance/id").and_then(Value::as_str))
            && let Some(route) = routes.get(instance_id)
            && let Some(obj) = row.as_object_mut()
        {
            obj.insert("apiRoute".into(), route.clone());
        }
    }
    rows
}

/// Render the echoed apiRoute as one watch clause: `direct`; `via <label>
/// hub-relay|direct-net`; the Hub host reads as `via self …`.
///
/// `None` only when the value carries no `mode` (absent echo), which the ROUTE
/// column renders as `-`.
pub(crate) fn route_clause(api_route: &Value) -> Option<String> {
    let mode = api_route.get("mode").and_then(Value::as_str)?;
    if mode != "via" {
        return Some("direct".to_string());
    }
    let host = api_route
        .get("viaHostLabel")
        .and_then(Value::as_str)
        .filter(|label| !label.is_empty())
        .or_else(|| api_route.get("viaHostId").and_then(Value::as_str))
        .unwrap_or("self");
    let kind = api_route
        .get("route")
        .and_then(Value::as_str)
        .unwrap_or("hub-relay");
    Some(format!("via {host} {kind}"))
}

/// Whether the row is a proxied session whose route went down mid-flight
/// (roster `Blocked{api-route-down}`, D-047 §B.5).
fn is_route_down(row: &Value) -> bool {
    row.pointer("/state/reason").and_then(Value::as_str) == Some(API_ROUTE_DOWN)
        || row.pointer("/watch/reason").and_then(Value::as_str) == Some(API_ROUTE_DOWN)
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
        let gates = fetch_gate_jobs(client, project.as_deref()).await;
        let routes = fetch_instance_routes(client).await;
        let items = attach_routes(items(&value), &routes);
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
    // The carrier the agent is actually running on, as the Node reported it at
    // launch. Shown because a worker on the wrong driver misbehaves in ways the
    // status column cannot explain — a `claude-print` worker looks idle forever,
    // since print exits after one turn.
    let mut driver = Col {
        name: "DRIVER",
        cells: Vec::new(),
    };
    // The model actually answering. `modelEffective` when a launch read-back
    // disagreed with the dispatch request, else the requested id. A pinned model
    // that was silently substituted used to be invisible here, because the row
    // only ever carried the request (model-pin-1); on a divergence the cell
    // renders `observed ⇐ requested`, so the two can be told apart at a glance.
    let mut model = Col {
        name: "MODEL",
        cells: Vec::new(),
    };
    // The API route the session actually got (D-047), from the instance's
    // Node-echoed `apiRoute`: `direct` or `via <host> hub-relay|direct-net`.
    // A `api-route-down` block is appended here rather than replacing the
    // route — the route did not reroute, it went down (D-035).
    let mut route = Col {
        name: "ROUTE",
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
        driver.cells.push(
            row.get("driver")
                .and_then(Value::as_str)
                .unwrap_or("-")
                .to_string(),
        );
        model.cells.push(truncate(model_label(row), 48));
        let route_cell = match row.get("apiRoute") {
            Some(api_route) => truncate(
                route_clause(api_route)
                    .map(|clause| {
                        if is_route_down(row) {
                            format!("{clause} · {API_ROUTE_DOWN}")
                        } else {
                            clause
                        }
                    })
                    .unwrap_or_else(|| "-".to_string()),
                48,
            ),
            None => "-".to_string(),
        };
        route.cells.push(route_cell);
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
    let cols = [&name, &status, &driver, &model, &route, &evidence, &detail];
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
        print!("{:<width$}  ", driver.cells[i], width = widths[2]);
        print!("{:<width$}  ", model.cells[i], width = widths[3]);
        print!("{:<width$}  ", route.cells[i], width = widths[4]);
        print!("{:<width$}  ", evidence.cells[i], width = widths[5]);
        print!("{:<width$}", detail.cells[i], width = widths[6]);
        println!();
    }
}

/// The model cell: the effective id when it diverged from the request, else the
/// requested id.
///
/// On a divergence this renders `observed ⇐ requested`, observed FIRST. The
/// observed id is the one thing the column exists to show, and the cell is head
/// truncated to fit — putting the requested id first (an earlier draft did) cut
/// the observed half off the demo's own 30-char ids and hid exactly the
/// substitution it was added for. The requested id follows, so the pin the
/// operator typed is still on the row.
fn model_label(row: &Value) -> String {
    let field = |key: &str| {
        row.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    match (field("model"), field("modelEffective")) {
        // Only a real divergence carries `modelEffective`; a gateway resolving
        // the pin to an upstream vendor name is stored as the request, so it
        // reaches the `None` arm and is not shown as a substitution.
        (Some(requested), Some(observed)) if requested != observed => {
            format!("{observed} ⇐ {requested}")
        }
        (_, Some(observed)) => observed.to_string(),
        (Some(requested), None) => requested.to_string(),
        (None, None) => "-".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A blocked-with-reason watch row exactly as `/v1/workers/observe`
    /// persists a screen-classified first-run dialog.
    fn dialog_row(title: &str) -> Value {
        json!({
            "id": "wkr_dialog",
            "name": "c-dialog",
            "driver": "shell-pty",
            "state": {"state": "blocked", "reason": title},
            "watch": {"status": "blocked", "reason": title, "detail": title},
        })
    }

    #[test]
    fn a_dialog_blocked_row_renders_blocked_with_the_dialog_title() {
        // dispatch-onboarding-1: `remuda watch` must surface the blocked
        // status and the dialog title, never fall back to the lifecycle state
        // or print "working" for a parked first-run modal.
        for title in [
            "Is this a project you created or one you trust?",
            "Allow reads outside the working directories?",
        ] {
            let row = dialog_row(title);
            assert_eq!(label(&row), "blocked");
            assert_eq!(sha_or_reason(&row), title);
            assert!(!is_done(&row));
            // The follow change-signature includes the reason so a worker
            // entering the modal actually prints a row.
            assert!(signature(&row).contains(title));
        }
    }

    #[test]
    fn blocked_status_wins_over_any_optimistic_lifecycle_state() {
        // The point-in-time watch classification is preferred even if the
        // durable row still says "working" (the incident's shape).
        let row = json!({
            "id": "wkr_x",
            "name": "c-x",
            "state": {"state": "working"},
            "watch": {
                "status": "blocked",
                "reason": "Is this a project you created or one you trust?",
            },
        });
        assert_eq!(label(&row), "blocked");
        assert_eq!(
            sha_or_reason(&row),
            "Is this a project you created or one you trust?"
        );
        assert!(!is_done(&row));
    }

    /// model-pin-1: the MODEL cell reports what answered, and makes a
    /// substitution legible instead of showing only the request.
    #[test]
    fn the_model_cell_reports_the_effective_id_and_marks_a_divergence() {
        // Agreement (or no observation yet): just the requested id.
        assert_eq!(
            model_label(&serde_json::json!({"model": "model_hub/es1_orange_o50[1m]"})),
            "model_hub/es1_orange_o50[1m]"
        );
        assert_eq!(
            model_label(&serde_json::json!({
                "model": "ark/seed-evolving[1m]",
                "modelEffective": "ark/seed-evolving[1m]",
            })),
            "ark/seed-evolving[1m]"
        );
        // The regression: asked for one model in the same namespace, another
        // answered. Observed is FIRST so head-truncation cannot cut the one
        // thing this column exists to show; the requested pin still follows.
        assert_eq!(
            model_label(&serde_json::json!({
                "model": "model_hub/es1_orange_o50[1m]",
                "modelEffective": "model_hub/es1_orange_o48[1m]",
            })),
            "model_hub/es1_orange_o48[1m] ⇐ model_hub/es1_orange_o50[1m]"
        );
        // An observation with no recorded request still reports honestly.
        assert_eq!(
            model_label(&serde_json::json!({"modelEffective": "claude-opus-5"})),
            "claude-opus-5"
        );
        // Nothing known, and blank values are not ids.
        assert_eq!(model_label(&serde_json::json!({})), "-");
        assert_eq!(
            model_label(&serde_json::json!({"model": "", "modelEffective": "  "})),
            "-"
        );
    }

    /// The observed id must survive the column's truncation — it is the half
    /// that answers, and the demo's ids are ~30 chars each.
    #[test]
    fn truncation_keeps_the_observed_id() {
        let row = serde_json::json!({
            "model": "model_hub/es1_orange_o50[1m]",
            "modelEffective": "model_hub/es1_orange_o48[1m]",
        });
        let cell = truncate(model_label(&row), 48);
        assert!(
            cell.contains("model_hub/es1_orange_o48"),
            "the observed id must remain in a 48-char cell: {cell}"
        );
    }

    /// D-047: the ROUTE column renders the Node-echoed route — direct, or via
    /// a named host over the resolved kind.
    #[test]
    fn route_column_renders_the_echoed_route() {
        assert_eq!(
            route_clause(&json!({"mode": "direct"})).as_deref(),
            Some("direct")
        );
        assert_eq!(
            route_clause(&json!({
                "mode": "via",
                "route": "hub-relay",
                "viaHostId": "hst_mac000000000000000000000000000a",
                "viaHostLabel": "mac-host"
            }))
            .as_deref(),
            Some("via mac-host hub-relay")
        );
        // No label: the id names the host; never a blank or the requested id.
        assert_eq!(
            route_clause(&json!({
                "mode": "via",
                "route": "direct-net",
                "viaHostId": "hst_sg0000000000000000000000000000b"
            }))
            .as_deref(),
            Some("via hst_sg0000000000000000000000000000b direct-net")
        );
        // `self` (the Hub host) carries no host id on the echo.
        assert_eq!(
            route_clause(&json!({"mode": "via", "route": "hub-relay"})).as_deref(),
            Some("via self hub-relay")
        );
        // No echo at all: an empty cell, not a guessed direct.
        assert_eq!(route_clause(&json!({})), None);
    }

    /// D-047 §B.5: the route-down block is detected from either the durable
    /// worker state or the point-in-time watch reason.
    #[test]
    fn route_down_is_detected_from_state_or_watch() {
        let row = json!({
            "state": {"state": "blocked", "reason": "api-route-down"},
            "watch": {"status": "working"}
        });
        assert!(is_route_down(&row));
        let row = json!({
            "state": {"state": "working"},
            "watch": {"status": "blocked", "reason": "api-route-down"}
        });
        assert!(is_route_down(&row));
        let row = json!({
            "state": {"state": "blocked", "reason": "first-run dialog"},
            "watch": {"status": "blocked", "reason": "first-run dialog"}
        });
        assert!(!is_route_down(&row));
    }
}
