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
    /// Cancel a queued or running job by `gjb_…` id (or its branch name).
    Cancel {
        #[command(flatten)]
        hub: HubOpts,
        /// Job id or branch name.
        job: String,
        /// Restrict to one project.
        #[arg(long)]
        project: Option<String>,
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
                Some(GateCommand::Cancel { hub, job, project }) => {
                    cancel_job(hub, project, job).await
                }
                None => {
                    let Some(branch) = self.branch else {
                        anyhow::bail!(
                            "usage: remuda gate <branch> | remuda gate list | remuda gate cancel <job>"
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
            if !as_json && let Some(error) = job.get("error").and_then(Value::as_str) {
                eprintln!("{error}");
            }
            if as_json {
                super::hub_client::print_json(&job)?;
            } else if state == "landed" {
                if let Some(sha) = job.get("mergeSha").and_then(Value::as_str) {
                    println!("landed: {sha}");
                }
            } else {
                println!("{mode}: {state}");
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

fn is_terminal(state: &str) -> bool {
    matches!(state, "passed" | "failed" | "landed" | "canceled")
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
        "{:<44}  {:<9}  {:<30}  {:<9}  MERGE_SHA",
        "JOB", "MODE", "BRANCH", "STATE"
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
        let sha = job
            .get("mergeSha")
            .and_then(Value::as_str)
            .map(|sha| sha.chars().take(12).collect::<String>())
            .unwrap_or_else(|| "-".into());
        println!("{id:<44}  {mode:<9}  {branch:<30}  {state:<9}  {sha}");
    }
    Ok(0)
}

async fn cancel_job(
    hub: HubOpts,
    project: Option<String>,
    job_or_branch: String,
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
    super::hub_client::print_json(&value)?;
    Ok(0)
}
