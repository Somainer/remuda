//! `remuda gate` / `remuda land` — project gate-queue verbs (batch 6 co-lanes).
//!
//! The Hub schedules the job onto a lane Node, which fetches, fast-forwards
//! the branch (stale-tip refusal), runs the consumed `remuda merge --gate`
//! CLI in the lane checkout, and — for land — CAS-pushes main from the lane
//! host. These verbs only render; they never ssh or touch a checkout
//! themselves.

use anyhow::Context;
use clap::{Args, Subcommand};
use serde_json::Value;

use super::hub_client::{HubOpts, block_on};
use super::registry::Entrypoint;

/// Poll interval while waiting for a job.
const WAIT_POLL_SECS: u64 = 1;

// ── remuda gate ────────────────────────────────────────────────────────────

#[derive(Args)]
#[command(about = "Verify a branch on a project gate lane (or list/cancel queued jobs).")]
pub(crate) struct GateArgs {
    #[command(flatten)]
    hub: HubOpts,
    #[command(subcommand)]
    command: Option<GateCommand>,
    /// Branch to verify.
    branch: Option<String>,
    /// Project (`prj_…`); optional when exactly one project is in scope.
    #[arg(long)]
    project: Option<String>,
    /// Web gate selection: auto (default), always (incl. live Hub e2e), never.
    #[arg(long)]
    web: Option<String>,
    /// Pin the job to one configured lane id.
    #[arg(long)]
    lane: Option<String>,
    /// Enqueue and return immediately without waiting for the verdict.
    #[arg(long)]
    no_wait: bool,
    /// Retain the bounded step log even when the run passes.
    #[arg(long)]
    keep_logs: bool,
    /// Emit the job(s) as JSON instead of the step table.
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum GateCommand {
    /// List gate jobs (`--state queued|running|passed|failed|landed|canceled`).
    List {
        #[command(flatten)]
        hub: HubOpts,
        /// Restrict to one project.
        #[arg(long)]
        project: Option<String>,
        /// Filter by state.
        #[arg(long)]
        state: Option<String>,
        /// Restrict to one branch.
        #[arg(long)]
        branch: Option<String>,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the bounded failure log for a job (`gjb_…` or `obj_…` id).
    Log {
        #[command(flatten)]
        hub: HubOpts,
        /// Gate job id or log object id.
        id: String,
    },
    /// Cancel a queued or running job by `gjb_…` id (or its branch name).
    Cancel {
        #[command(flatten)]
        hub: HubOpts,
        /// Job id or branch name.
        job: String,
        /// Restrict to one project.
        #[arg(long)]
        project: Option<String>,
        /// Request the cancel and return without waiting for the terminal state.
        #[arg(long)]
        no_wait: bool,
        /// Emit the job as JSON instead of the step table.
        #[arg(long)]
        json: bool,
    },
}

impl Entrypoint for GateArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        block_on(async move {
            match self.command {
                Some(GateCommand::List {
                    hub,
                    project,
                    state,
                    branch,
                    json,
                }) => list_jobs(hub, project, state, branch, json).await,
                Some(GateCommand::Log { hub, id }) => print_log(hub, &id).await,
                Some(GateCommand::Cancel {
                    hub,
                    job,
                    project,
                    no_wait,
                    json,
                }) => cancel_job(hub, project, job, no_wait, json).await,
                None => {
                    let Some(branch) = self.branch else {
                        anyhow::bail!(
                            "usage: remuda gate <branch> | remuda gate list | remuda gate log <gjb> | remuda gate cancel <job>"
                        );
                    };
                    enqueue_and_wait(
                        GateInvocation {
                            hub: self.hub,
                            branch,
                            project: self.project,
                            web: self.web,
                            lane: self.lane,
                            no_wait: self.no_wait,
                            keep_logs: self.keep_logs,
                            json: self.json,
                            then_command: None,
                        },
                        "verify",
                    )
                    .await
                }
            }
        })
    }
}

// ── remuda land ────────────────────────────────────────────────────────────

#[derive(Args)]
#[command(about = "Verify a branch on a lane, then compare-and-swap and push main.")]
pub(crate) struct LandArgs {
    #[command(flatten)]
    hub: HubOpts,
    /// Branch to land.
    branch: String,
    /// Project (`prj_…`); optional when exactly one project is in scope.
    #[arg(long)]
    project: Option<String>,
    /// Web gate selection.
    #[arg(long)]
    web: Option<String>,
    /// Pin the job to one configured lane id.
    #[arg(long)]
    lane: Option<String>,
    /// Post-land command run on the project's home host.
    #[arg(long)]
    then: Option<String>,
    /// Enqueue without waiting for the land.
    #[arg(long)]
    no_wait: bool,
    /// Retain the bounded step log even when the run passes.
    #[arg(long)]
    keep_logs: bool,
    /// Emit the job as JSON.
    #[arg(long)]
    json: bool,
}

impl Entrypoint for LandArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        block_on(async move {
            enqueue_and_wait(
                GateInvocation {
                    hub: self.hub,
                    branch: self.branch,
                    project: self.project,
                    web: self.web,
                    lane: self.lane,
                    no_wait: self.no_wait,
                    keep_logs: self.keep_logs,
                    json: self.json,
                    then_command: self.then,
                },
                "land",
            )
            .await
        })
    }
}

/// One enqueue request, built by either verb.
struct GateInvocation {
    hub: HubOpts,
    branch: String,
    project: Option<String>,
    web: Option<String>,
    lane: Option<String>,
    no_wait: bool,
    keep_logs: bool,
    json: bool,
    then_command: Option<String>,
}

// ── implementation ─────────────────────────────────────────────────────────

async fn enqueue_and_wait(args: GateInvocation, mode: &str) -> anyhow::Result<i32> {
    let client = args.hub.connect()?;
    let project = resolve_project(&client, &args.project).await?;
    let mut body = serde_json::Map::new();
    body.insert("branch".into(), Value::String(args.branch.clone()));
    body.insert("mode".into(), Value::String(mode.into()));
    if let Some(web) = &args.web {
        body.insert("web".into(), Value::String(web.clone()));
    }
    if let Some(lane) = &args.lane {
        body.insert("laneId".into(), Value::String(lane.clone()));
    }
    if let Some(then) = &args.then_command {
        body.insert("thenCommand".into(), Value::String(then.clone()));
    }
    if args.keep_logs {
        body.insert("keepLogs".into(), Value::Bool(true));
    }
    let job = client
        .post(
            &format!("/v1/projects/{project}/gate"),
            &Value::Object(body),
        )
        .await?;
    let job_id = job
        .get("id")
        .and_then(Value::as_str)
        .context("gate response missing job id")?
        .to_owned();
    if args.no_wait {
        super::hub_client::print_json(&job)?;
        return Ok(0);
    }
    wait_for_job(&client, &project, &job_id, mode, args.json).await
}

/// Resolve `--project`, falling back to the sole project in scope.
pub(crate) async fn resolve_project(
    client: &remuda_hub_client::HubClient,
    explicit: &Option<String>,
) -> anyhow::Result<String> {
    if let Some(project) = explicit {
        return Ok(project.clone());
    }
    let value = client.get("/v1/projects").await?;
    let ids: Vec<String> = value
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    match ids.as_slice() {
        [only] => Ok(only.clone()),
        [] => anyhow::bail!("no projects visible; pass --project prj_…"),
        _ => anyhow::bail!("{} projects visible; pass --project prj_…", ids.len()),
    }
}

/// Poll the job and print steps as they finish, mirroring
/// `remuda merge --gate`'s per-step lines.
pub(crate) async fn wait_for_job(
    client: &remuda_hub_client::HubClient,
    project: &str,
    job_id: &str,
    mode: &str,
    as_json: bool,
) -> anyhow::Result<i32> {
    let path = format!("/v1/projects/{project}/gate/jobs/{job_id}");
    let mut printed: std::collections::BTreeSet<String> = Default::default();
    loop {
        let job = client.get(&path).await?;
        for step in job
            .get("steps")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let name = step.get("name").and_then(Value::as_str).unwrap_or("?");
            let status = step.get("status").and_then(Value::as_str).unwrap_or("?");
            if status == "planned" {
                continue;
            }
            let key = format!("{name}:{status}");
            if printed.insert(key) && !as_json {
                let duration = step.get("durationMs").and_then(Value::as_u64).unwrap_or(0);
                let retried = step
                    .get("retried")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                println!(
                    "{name}: {status} ({duration} ms){}",
                    if retried { " [retried]" } else { "" }
                );
            }
        }
        let state = job.get("state").and_then(Value::as_str).unwrap_or("");
        if is_terminal(state) {
            if as_json {
                super::hub_client::print_json(&job)?;
            } else if state == "landed" {
                if let Some(sha) = job.get("mergeSha").and_then(Value::as_str) {
                    println!("landed: {sha}");
                }
            } else if state == "failed" {
                print_failure(client, &job).await?;
                println!("{mode}: {state}");
            } else {
                if let Some(reason) = failure_reason(&job) {
                    eprintln!("{reason}");
                }
                println!("{mode}: {state}");
                if state == "passed" {
                    print_land_hint(&job);
                }
            }
            return Ok(match state {
                "passed" | "landed" => 0,
                "canceled" => 2,
                _ => 1,
            });
        }
        tokio::time::sleep(std::time::Duration::from_secs(WAIT_POLL_SECS)).await;
    }
}

/// After a passing verify, say where the merge is pinned and exactly what to
/// run to land it. A pass is not a land: without this the operator is left to
/// guess the branch spelling and whether the merge still exists.
fn print_land_hint(job: &Value) {
    let Some(merge_ref) = job.get("mergeRef").and_then(Value::as_str) else {
        return;
    };
    if let Some(sha) = job.get("mergeSha").and_then(Value::as_str) {
        println!("mergeRef: {merge_ref} -> {sha}");
    } else {
        println!("mergeRef: {merge_ref}");
    }
    if let Some(branch) = job.get("branch").and_then(Value::as_str) {
        let project = job
            .get("projectId")
            .and_then(Value::as_str)
            .map(|id| format!(" --project {id}"))
            .unwrap_or_default();
        println!("land it with: remuda land {branch}{project}");
    }
}

/// The job's short failure reason: the extracted-summary headline first,
/// falling back to the raw error.
fn failure_reason(job: &Value) -> Option<String> {
    job.get("reason")
        .and_then(Value::as_str)
        .or_else(|| job.get("error").and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// Fetch a job's bounded log envelope (`GET /v1/gate/logs/{id}`).
async fn fetch_log(client: &remuda_hub_client::HubClient, id: &str) -> Option<Value> {
    client
        .get(&format!("/v1/gate/logs/{id}"))
        .await
        .ok()
        .filter(|value| value.get("log").is_some())
}

/// Print the failed step, its extracted summary lines, and where to fetch the
/// full bounded log on a failed gate/land.
async fn print_failure(client: &remuda_hub_client::HubClient, job: &Value) -> anyhow::Result<()> {
    let job_id = job.get("id").and_then(Value::as_str).unwrap_or("");
    let failed_step = job.get("failedStep").and_then(Value::as_str);
    if let Some(reason) = failure_reason(job) {
        match failed_step {
            Some(step) => eprintln!("{step}: {reason}"),
            None => eprintln!("{reason}"),
        }
    }
    let envelope = fetch_log(client, job_id).await;
    if let Some(summary) = envelope
        .as_ref()
        .and_then(|value| value.get("log"))
        .and_then(|log| log.get("summary"))
        .and_then(Value::as_array)
    {
        for line in summary.iter().filter_map(Value::as_str) {
            eprintln!("{line}");
        }
    }
    let object_id = job.get("logObjectId").and_then(Value::as_str);
    if let Some(object_id) = object_id {
        eprintln!("full log: remuda gate log {job_id} ({object_id})");
    } else if job_id.starts_with("gjb_") {
        eprintln!("full log: remuda gate log {job_id}");
    }
    Ok(())
}

fn is_terminal(state: &str) -> bool {
    matches!(state, "passed" | "failed" | "landed" | "canceled")
}

/// `remuda gate log <gjb|obj>` — print the bounded failure log (summary and
/// the last captured lines).
async fn print_log(hub: HubOpts, id: &str) -> anyhow::Result<i32> {
    let client = hub.connect()?;
    let envelope = client.get(&format!("/v1/gate/logs/{id}")).await?;
    let log = envelope
        .get("log")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no gate log for {id} (logs are kept for 30 days)"))?;
    let step = log.get("step").and_then(Value::as_str).unwrap_or("?");
    let kind = log.get("kind").and_then(Value::as_str).unwrap_or("?");
    let object_id = envelope
        .get("objectId")
        .and_then(Value::as_str)
        .unwrap_or("?");
    if let Some(headline) = log.get("headline").and_then(Value::as_str) {
        println!("== {step} ({kind}) {object_id} ==");
        println!("{headline}");
    }
    if let Some(summary) = log.get("summary").and_then(Value::as_array)
        && !summary.is_empty()
    {
        println!("\n-- summary --");
        for line in summary.iter().filter_map(Value::as_str) {
            println!("{line}");
        }
    }
    if let Some(tail) = log.get("tail").and_then(Value::as_array)
        && !tail.is_empty()
    {
        println!("\n-- last {} lines --", tail.len());
        for line in tail.iter().filter_map(Value::as_str) {
            println!("{line}");
        }
    }
    Ok(0)
}

async fn list_jobs(
    hub: HubOpts,
    project: Option<String>,
    state: Option<String>,
    branch: Option<String>,
    as_json: bool,
) -> anyhow::Result<i32> {
    let client = hub.connect()?;
    let has_project_filter = project.is_some();
    let value = if let Some(project) = project {
        let mut path = format!("/v1/projects/{project}/gate?");
        if let Some(state) = &state {
            path.push_str(&format!("state={state}&"));
        }
        if let Some(branch) = &branch {
            path.push_str(&format!("branch={branch}&"));
        }
        client.get(&path).await?
    } else {
        let mut path = String::from("/v1/gate/jobs?");
        if let Some(state) = &state {
            path.push_str(&format!("state={state}&"));
        }
        if state.is_none() && branch.is_none() {
            path.push_str("active=true&");
        }
        client.get(&path).await?
    };
    let mut items = value
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !has_project_filter && let Some(branch) = &branch {
        items.retain(|job| job.get("branch").and_then(Value::as_str) == Some(branch));
    }
    if as_json {
        super::hub_client::print_json(&serde_json::json!({
            "items": items,
            "nextCursor": null,
        }))?;
        return Ok(0);
    }
    if items.is_empty() {
        println!("gate: no jobs");
        return Ok(0);
    }
    println!(
        "{:<44}  {:<9}  {:<30}  {:<9}  {:<14}  MERGE_SHA",
        "JOB", "MODE", "BRANCH", "STATE", "FAILED_STEP"
    );
    for job in &items {
        let id = job.get("id").and_then(Value::as_str).unwrap_or("?");
        let mode = job.get("mode").and_then(Value::as_str).unwrap_or("?");
        let branch = job
            .get("branch")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .chars()
            .take(30)
            .collect::<String>();
        let state = job.get("state").and_then(Value::as_str).unwrap_or("?");
        let failed_step = job
            .get("failedStep")
            .and_then(Value::as_str)
            .unwrap_or("-")
            .chars()
            .take(14)
            .collect::<String>();
        let sha = job
            .get("mergeSha")
            .and_then(Value::as_str)
            .map(|sha| sha.chars().take(12).collect::<String>())
            .unwrap_or_else(|| "-".into());
        println!("{id:<44}  {mode:<9}  {branch:<30}  {state:<9}  {failed_step:<14}  {sha}");
    }
    Ok(0)
}

async fn cancel_job(
    hub: HubOpts,
    project: Option<String>,
    job_or_branch: String,
    no_wait: bool,
    as_json: bool,
) -> anyhow::Result<i32> {
    let client = hub.connect()?;
    let project = resolve_project(&client, &project).await?;
    let job_id = if job_or_branch.starts_with("gjb_") {
        job_or_branch
    } else {
        let value = client
            .get(&format!(
                "/v1/projects/{project}/gate?branch={job_or_branch}&limit=1"
            ))
            .await?;
        value
            .get("items")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|job| job.get("id").and_then(Value::as_str))
            .ok_or_else(|| anyhow::anyhow!("no gate job for branch {job_or_branch:?}"))?
            .to_owned()
    };
    let value = client
        .post(
            &format!("/v1/projects/{project}/gate/jobs/{job_id}/cancel"),
            &serde_json::json!({}),
        )
        .await?;
    // A cancel is a request, not a verdict: the run still decides. Unless the
    // caller opted out, poll to the real terminal state and let the exit code
    // reflect what actually happened — including a job that landed anyway.
    if no_wait {
        super::hub_client::print_json(&value)?;
        return Ok(0);
    }
    let code = wait_for_job(&client, &project, &job_id, "cancel", as_json).await?;
    // wait_for_job returns 0 for a landed job; a canceled request that landed
    // anyway is not a success from the operator's point of view.
    let job = client
        .get(&format!("/v1/projects/{project}/gate/jobs/{job_id}"))
        .await?;
    if job.get("state").and_then(Value::as_str) == Some("landed") {
        return Ok(1);
    }
    Ok(code)
}
