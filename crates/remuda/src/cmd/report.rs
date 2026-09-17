//! `remuda report` — one fleet digest from the worker roster plus the
//! mergequeue gate/land reports (M1 batch 5b).
//!
//! A normal report shows the whole picture: what landed since the previous
//! report, which workers are done/awaiting-land, and what is blocked, stalled,
//! failed or gone and why.
//!
//! `--for-owner` implements the T1 contract in coordinator-hierarchy.md §5.3:
//! it emits *only* owner-actionable items and *only on state change* (diffed
//! against a per-repo marker), never on a timer. The coordinator's own
//! interventions (an idle-after-API-error worker that needs a nudge) are not
//! owner-actionable and are therefore excluded.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::Args;
use serde_json::{Value, json};

use super::hub_client::{HubOpts, block_on, print_json};
use super::registry::Entrypoint;

#[derive(Args)]
#[command(about = "Fleet digest: landed, done, blocked/stalled/gone, and owner asks.")]
pub(crate) struct ReportArgs {
    #[command(flatten)]
    hub: HubOpts,
    /// Restrict to one project (`prj_…`).
    #[arg(long)]
    project: Option<String>,
    /// Emit only owner-actionable items that changed since the last report.
    #[arg(long)]
    for_owner: bool,
    /// Emit JSON instead of the text digest.
    #[arg(long)]
    json: bool,
}

impl Entrypoint for ReportArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        block_on(async move { run(self).await })
    }
}

async fn run(args: ReportArgs) -> anyhow::Result<i32> {
    let client = args.hub.connect()?;
    let path = match &args.project {
        Some(project) => format!("/v1/workers?project={project}"),
        None => "/v1/workers".to_string(),
    };
    let roster = client.get(&path).await?;
    let workers: Vec<Value> = roster
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // Merge reports + the change marker live in the repo's common git dir; a
    // report run outside a repo just omits the gate/land sections.
    let common = git_common_dir();
    let reports = common
        .as_deref()
        .map(load_merge_reports)
        .unwrap_or_default();
    // Batch 6: the Hub gate queue is the source of truth for lane gating.
    let gate_path = match &args.project {
        Some(project) => format!("/v1/projects/{project}/gate?"),
        None => "/v1/gate/jobs?limit=100&".to_owned(),
    };
    let gate_jobs: Vec<Value> = client
        .get(&gate_path)
        .await
        .ok()
        .and_then(|value| value.get("items").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    let marker_path = common
        .as_ref()
        .map(|dir| dir.join("remuda/coordinator/last-report.json"));
    let mut marker = marker_path
        .as_ref()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or_else(|| json!({}));

    if args.for_owner {
        let asks = owner_asks(&workers, &reports, &gate_jobs);
        let previous = marker
            .get("owner")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let changed: Vec<&Value> = asks
            .iter()
            .filter(|ask| {
                previous.get(ask_signature(ask).as_str()) != Some(&json!(ask_reason(ask)))
            })
            .collect();
        let mut current = serde_json::Map::new();
        for ask in &asks {
            current.insert(ask_signature(ask), json!(ask_reason(ask)));
        }
        marker["owner"] = Value::Object(current);
        write_marker(marker_path.as_deref(), &marker);

        if args.json {
            print_json(&json!({ "items": changed }))?;
        } else if changed.is_empty() {
            eprintln!("report: nothing new for the owner");
        } else {
            print_owner_asks(&changed);
        }
        return Ok(0);
    }

    // Full digest.
    let last_landed = gate_jobs
        .iter()
        .filter(|job| job["state"].as_str() == Some("landed"))
        .max_by_key(|job| job.get("finishedAt").and_then(Value::as_str).unwrap_or(""))
        .cloned();
    let active_gate: Vec<&Value> = gate_jobs
        .iter()
        .filter(|job| matches!(job["state"].as_str(), Some("queued") | Some("running")))
        .collect();
    let gate_failures: Vec<&Value> = gate_jobs
        .iter()
        .filter(|job| job["state"].as_str() == Some("failed"))
        .collect();
    let landed = reports
        .iter()
        .filter(|report| report["status"].as_str() == Some("landed"))
        .cloned()
        .collect::<Vec<_>>();
    let gate_failed = reports
        .iter()
        .filter(|report| {
            matches!(
                report["status"].as_str(),
                Some("gate_failed") | Some("conflict") | Some("cas_lost") | Some("base_moved")
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    let verified = reports
        .iter()
        .filter(|report| report["status"].as_str() == Some("verified"))
        .cloned()
        .collect::<Vec<_>>();

    let previous_landed = marker
        .get("landed")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let new_landed: Vec<&Value> = landed
        .iter()
        .filter(|report| {
            let key = land_key(report);
            previous_landed.get(&key) != Some(&json!(report["merged"].as_str().unwrap_or("")))
        })
        .collect();
    let mut landed_marker = serde_json::Map::new();
    for report in &landed {
        landed_marker.insert(
            land_key(report),
            json!(report["merged"].as_str().unwrap_or("")),
        );
    }
    marker["landed"] = Value::Object(landed_marker);
    write_marker(marker_path.as_deref(), &marker);

    let attention = attention_workers(&workers);
    let done = workers
        .iter()
        .filter(|worker| status_of(worker) == "done")
        .cloned()
        .collect::<Vec<_>>();
    let active = workers
        .iter()
        .filter(|worker| status_of(worker) != "retired")
        .cloned()
        .collect::<Vec<_>>();

    let digest = json!({
        "counts": fleet_counts(&workers),
        "landedSinceLastReport": new_landed,
        "lastLandedSha": last_landed.and_then(|job| job.get("mergeSha").cloned()),
        "activeGateJobs": active_gate.iter().map(|job| json!({
            "id": job["id"], "branch": job["branch"], "mode": job["mode"],
            "state": job["state"], "laneId": job["laneId"],
        })).collect::<Vec<_>>(),
        "hubGateFailures": gate_failures.iter().map(|job| json!({
            "id": job["id"], "branch": job["branch"], "mode": job["mode"],
            "state": job["state"], "error": job["error"],
        })).collect::<Vec<_>>(),
        "verifiedAwaitingLand": verified,
        "gateFailures": gate_failed,
        "needsAttention": attention,
        "doneAwaitingLand": done,
        "ownerAsks": owner_asks(&workers, &reports, &gate_jobs),
        "activeWorkers": active.iter().map(|worker| json!({
            "name": worker["name"], "status": status_of(worker),
            "branch": worker["branch"], "evidence": evidence(worker),
        })).collect::<Vec<_>>(),
    });
    if args.json {
        print_json(&digest)?;
    } else {
        print_digest(&digest);
    }
    Ok(0)
}

// ── classification helpers ─────────────────────────────────────────────────

fn status_of(worker: &Value) -> String {
    worker
        .get("watch")
        .and_then(|watch| watch.get("status"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            worker["state"]
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string()
        })
}

fn evidence(worker: &Value) -> Value {
    if let Some(sha) = worker
        .get("watch")
        .and_then(|watch| watch.get("sha"))
        .or_else(|| worker["state"].get("sha"))
    {
        return sha.clone();
    }
    if let Some(reason) = worker
        .get("watch")
        .and_then(|watch| watch.get("reason"))
        .or_else(|| worker["state"].get("reason"))
    {
        return reason.clone();
    }
    json!(null)
}

fn detail_of(worker: &Value) -> String {
    worker
        .get("watch")
        .and_then(|watch| watch.get("detail"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Workers the coordinator must act on or escalate.
fn attention_workers(workers: &[Value]) -> Vec<Value> {
    workers
        .iter()
        .filter(|worker| {
            matches!(
                status_of(worker).as_str(),
                "blocked" | "stalled" | "gone" | "failed" | "idle-api-error"
            )
        })
        .cloned()
        .collect()
}

/// Only items the owner can resolve: formal BLOCKED, stalled, gone (a lost
/// worker the coordinator cannot silently resume around), a worker whose
/// instance failed/died after an errored turn (watch-failed-1), and hard gate
/// failures. A post-429 idle worker just needs a nudge, so it is excluded.
fn owner_asks(workers: &[Value], reports: &[Value], gate_jobs: &[Value]) -> Vec<Value> {
    let mut asks: Vec<Value> = Vec::new();
    for worker in workers {
        let name = worker["name"].clone();
        let branch = worker["branch"].clone();
        match status_of(worker).as_str() {
            "blocked" => asks.push(json!({
                "kind": "blocked", "name": name, "branch": branch,
                "reason": evidence(worker),
            })),
            "stalled" => asks.push(json!({
                "kind": "stalled", "name": name, "branch": branch,
                "reason": detail_of(worker),
            })),
            "gone" => asks.push(json!({
                "kind": "gone", "name": name, "branch": branch,
                "reason": detail_of(worker),
            })),
            "failed" => asks.push(json!({
                "kind": "failed", "name": name, "branch": branch,
                "reason": evidence(worker),
            })),
            _ => {}
        }
    }
    for report in reports {
        if matches!(
            report["status"].as_str(),
            Some("gate_failed") | Some("conflict") | Some("cas_lost") | Some("base_moved")
        ) {
            asks.push(json!({
                "kind": "gate", "branch": report["branch"],
                "status": report["status"], "base": report["base"],
            }));
        }
    }
    // Hub queue failures (batch 6 co-lanes).
    for job in gate_jobs {
        if job["state"].as_str() == Some("failed") {
            asks.push(json!({
                "kind": "gate", "branch": job["branch"],
                "status": format!("{}-failed", job["mode"].as_str().unwrap_or("verify")),
                "reason": job["error"],
            }));
        }
    }
    asks
}

fn ask_signature(ask: &Value) -> String {
    let kind = ask["kind"].as_str().unwrap_or("?");
    let who = ask["name"]
        .as_str()
        .or_else(|| ask["branch"].as_str())
        .unwrap_or("?");
    format!("{kind}:{who}")
}

fn ask_reason(ask: &Value) -> Value {
    ask.get("reason")
        .or_else(|| ask.get("status"))
        .cloned()
        .unwrap_or(json!(null))
}

fn land_key(report: &Value) -> String {
    format!(
        "{}@{}",
        report["branch"].as_str().unwrap_or("?"),
        report["base"].as_str().unwrap_or("?")
    )
}

fn fleet_counts(workers: &[Value]) -> Value {
    let mut counts = BTreeMap::new();
    for worker in workers {
        *counts.entry(status_of(worker)).or_insert(0u64) += 1;
    }
    json!(counts)
}

// ── merge report reading (read-only; see cmd/merge/reports.rs for layout) ──

fn git_common_dir() -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    let path = PathBuf::from(raw.trim());
    Some(if path.is_absolute() {
        path
    } else {
        std::env::current_dir().ok()?.join(path)
    })
}

fn load_merge_reports(common: &Path) -> Vec<Value> {
    let root = common.join("remuda/merge-reports");
    let mut reports = Vec::new();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(_) => return reports,
    };
    for slug_dir in entries.flatten() {
        let dir = slug_dir.path();
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains("preparing"))
            {
                continue;
            }
            if let Ok(raw) = std::fs::read_to_string(&path)
                && let Ok(report) = serde_json::from_str::<Value>(&raw)
            {
                reports.push(report);
            }
        }
    }
    reports
}

fn write_marker(path: Option<&Path>, marker: &Value) {
    if let Some(path) = path
        && let Some(parent) = path.parent()
        && let Ok(json) = serde_json::to_string_pretty(marker)
    {
        let _ = std::fs::create_dir_all(parent);
        let _ = std::fs::write(path, json);
    }
}

// ── text rendering ─────────────────────────────────────────────────────────

fn print_digest(digest: &Value) {
    println!("== fleet ==");
    println!("{}", digest["counts"]);
    if let Some(sha) = digest["lastLandedSha"].as_str() {
        println!("last landed main: {}", short(sha));
    }
    let active = digest["activeGateJobs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !active.is_empty() {
        println!("\n== gate queue ==");
        for job in &active {
            println!(
                "  {} [{}] {} ({})",
                job["branch"].as_str().unwrap_or("?"),
                job["mode"].as_str().unwrap_or("?"),
                job["state"].as_str().unwrap_or("?"),
                job["laneId"].as_str().unwrap_or("-")
            );
        }
    }
    let hub_failures = digest["hubGateFailures"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !hub_failures.is_empty() {
        println!("\n== gate queue failures ==");
        for job in &hub_failures {
            println!(
                "  {} [{}] {}",
                job["branch"].as_str().unwrap_or("?"),
                job["mode"].as_str().unwrap_or("?"),
                job["error"].as_str().unwrap_or("failed")
            );
        }
    }
    let landed = digest["landedSinceLastReport"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !landed.is_empty() {
        println!("\n== landed since last report ==");
        for report in &landed {
            println!(
                "  {} ({})",
                report["branch"].as_str().unwrap_or("?"),
                short(report["merged"].as_str().unwrap_or(""))
            );
        }
    }
    let verified = digest["verifiedAwaitingLand"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !verified.is_empty() {
        println!("\n== verified, awaiting land ==");
        for report in &verified {
            println!("  {}", report["branch"].as_str().unwrap_or("?"));
        }
    }
    let failures = digest["gateFailures"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !failures.is_empty() {
        println!("\n== gate failures ==");
        for report in &failures {
            println!(
                "  {} [{}] base {}",
                report["branch"].as_str().unwrap_or("?"),
                report["status"].as_str().unwrap_or("?"),
                short(report["base"].as_str().unwrap_or(""))
            );
        }
    }
    let attention = digest["needsAttention"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !attention.is_empty() {
        println!("\n== needs attention ==");
        for worker in &attention {
            let evidence = worker_evidence_text(worker);
            let detail = detail_of(worker);
            // A screenless failed worker's detail repeats the reason with a
            // provenance prefix; render the marker alone rather than the
            // reason twice.
            let detail = if detail
                .strip_prefix("screen-unavailable; ")
                .is_some_and(|tail| tail == evidence)
            {
                "screen-unavailable"
            } else {
                detail.as_str()
            };
            println!(
                "  {name} ({status}): {evidence} {detail}",
                name = worker["name"].as_str().unwrap_or("?"),
                status = status_of(worker),
            );
        }
    }
    let done = digest["doneAwaitingLand"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !done.is_empty() {
        println!("\n== done, awaiting gate/land ==");
        for worker in &done {
            println!(
                "  {name}: {sha}",
                name = worker["name"].as_str().unwrap_or("?"),
                sha = worker_evidence_text(worker),
            );
        }
    }
}

fn worker_evidence_text(worker: &Value) -> String {
    match &evidence(worker) {
        Value::String(value) => value.clone(),
        _ => "-".to_string(),
    }
}

fn print_owner_asks(asks: &[&Value]) {
    println!("== owner action needed ==");
    for ask in asks {
        let who = ask["name"]
            .as_str()
            .or_else(|| ask["branch"].as_str())
            .unwrap_or("?");
        let reason = ask_reason(ask);
        let reason = reason.as_str().unwrap_or("");
        println!(
            "  [{}] {} {}",
            ask["kind"].as_str().unwrap_or("?"),
            who,
            reason
        );
    }
}

fn short(sha: &str) -> &str {
    sha.get(..12.min(sha.len())).unwrap_or(sha)
}
