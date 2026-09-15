//! Coordinator merge: verify an immutable merge, then compare-and-swap main.

mod generated_api;
mod lock;
mod queue;
mod reports;
mod scratch;
mod web_e2e;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use clap::Args;
use serde::{Deserialize, Serialize};

#[derive(Debug, Args, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[command(about = "Verify a branch in a temporary worktree, then advance and push main.")]
pub(crate) struct MergeArgs {
    /// Local branch to merge into main (the committed snapshot is pinned).
    #[arg(required_unless_present_any = ["list", "queue"])]
    pub branch: Option<String>,
    /// Run the gate before advancing main.
    #[arg(long, required_unless_present_any = ["dry_run", "list", "land"])]
    #[serde(default)]
    pub gate: bool,
    /// Show local wt/* branches ahead of main and their mergeability.
    #[arg(long, visible_alias = "pending", conflicts_with_all = ["branch", "gate", "dry_run", "message", "web", "web_e2e", "no_push", "onto", "land", "queue"])]
    #[serde(default)]
    pub list: bool,
    /// Override the merge title; the gate summary is still appended to the body.
    #[arg(long, conflicts_with = "queue")]
    pub message: Option<String>,
    /// Inspect local refs and print the plan; do not fetch, merge, or run checks.
    #[arg(long, conflicts_with_all = ["onto", "land", "queue"])]
    #[serde(default)]
    pub dry_run: bool,
    /// Verify the branch merged onto an explicit base (a sha or `main`)
    /// without advancing main; pair with --land to land the verified result.
    #[arg(long, conflicts_with_all = ["dry_run", "list", "queue"])]
    pub onto: Option<String>,
    /// Fast-forward main to a report verified for exactly this (branch, --onto)
    /// pair; fails without re-running the gate when main has moved.
    #[arg(long, requires = "onto", conflicts_with_all = ["dry_run", "list", "queue"])]
    #[serde(default)]
    pub land: bool,
    /// Verify the listed branches as an optimistic queue across --lanes lanes.
    #[arg(long, num_args = 1.., value_name = "BRANCH", conflicts_with_all = ["dry_run", "list", "land", "onto", "message", "branch"])]
    #[serde(default)]
    pub queue: Vec<String>,
    /// Verification lanes for --queue (each lane gets its own target directory).
    #[arg(long, default_value_t = 2)]
    #[serde(default = "default_lanes")]
    pub lanes: usize,
    /// Base port for per-lane Hub e2e pairs; lane N uses base+10*(N-1) and +9.
    #[arg(long, default_value_t = 58980)]
    #[serde(default = "default_e2e_port_base")]
    pub e2e_port_base: u16,
    /// Advisory lock serialising the shared-browser web e2e step across lanes.
    #[arg(long)]
    pub e2e_lock: Option<PathBuf>,
    /// Internal: lane index used by --queue to derive ports (1-based; hidden).
    #[arg(long, hide = true, default_value_t = 1)]
    #[serde(default = "default_e2e_lane")]
    pub e2e_lane: usize,
    /// Test changed crates and their reverse dependencies (default).
    #[arg(long, default_value_t = true, conflicts_with = "full")]
    #[serde(default = "default_affected")]
    pub affected: bool,
    /// Test the entire workspace instead of selecting affected crates.
    #[arg(long)]
    #[serde(default)]
    pub full: bool,
    /// Include web checks even when the merge does not change crates/ or web/.
    #[arg(long)]
    #[serde(default)]
    pub web: bool,
    /// Include the live Hub Playwright suite (`gate.sh --web-e2e`).
    #[arg(long)]
    #[serde(default)]
    pub web_e2e: bool,
    /// Advance local main after verification without pushing origin.
    #[arg(long)]
    #[serde(default)]
    pub no_push: bool,
    /// Emit one structured JSON result on stdout; progress goes to stderr.
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
    /// Wait for another in-progress merge/queue on this repo instead of exiting 3.
    #[arg(long)]
    #[serde(default)]
    pub wait: bool,
    /// Repository checkout (default: current directory).
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Dedicated gate build directory (default: <repo>/target-gate).
    #[arg(long)]
    pub target_dir: Option<PathBuf>,
}

fn default_affected() -> bool {
    true
}

fn default_lanes() -> usize {
    2
}

fn default_e2e_port_base() -> u16 {
    58980
}

fn default_e2e_lane() -> usize {
    1
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Step {
    name: String,
    status: String,
    duration_ms: u64,
    attempts: u32,
    retried: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    crates: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    selection: Option<String>,
    /// Per-step wall-clock budget the gate driver enforced (0 = disabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeout_seconds: Option<u64>,
    /// "timeout" | "child-lost" when the gate driver killed the step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

impl Step {
    fn planned(name: &str) -> Self {
        Self {
            name: name.into(),
            status: "planned".into(),
            duration_ms: 0,
            attempts: 0,
            retried: false,
            command: Vec::new(),
            cwd: None,
            error: None,
            crates: None,
            selection: None,
            timeout_seconds: None,
            reason: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MergeReport {
    pub exit_code: i32,
    status: String,
    branch: String,
    dry_run: bool,
    affected: bool,
    expected_main: Option<String>,
    source: Option<String>,
    merged: Option<String>,
    /// `--onto` base the merge was constructed and verified on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Pinned branch commit the verified merge incorporated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Tree oid of the verified merge commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    /// Current main when a `--land` compare-and-swap found the base had moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_main: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_dir: Option<PathBuf>,
    gate_override: bool,
    web: bool,
    web_e2e: bool,
    main_updated: bool,
    pushed: bool,
    conflicts: Vec<String>,
    steps: Vec<Step>,
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    queue: Option<queue::QueueSummary>,
    // Internal bookkeeping for failure-path report persistence; never serialised.
    #[serde(skip)]
    repo_root: Option<PathBuf>,
    #[serde(skip)]
    reference: Option<String>,
}

#[derive(Debug, thiserror::Error)]
enum MergeStop {
    #[error("merge conflicts: {0:?}")]
    Conflict(Vec<String>),
    #[error("main changed during verification; compare-and-swap lost")]
    CasLost,
    #[error("main moved from {0} to {1} since verification; re-verify onto {1}")]
    BaseMoved(String, String),
}

pub(crate) fn run(args: MergeArgs) -> Result<i32> {
    if args.list {
        let value = pending(args.repo.as_deref())?;
        if args.json {
            super::hub_client::print_json(&value)?;
        } else {
            let rows = value["items"]
                .as_array()
                .context("queue items")?
                .iter()
                .map(|item| {
                    vec![
                        item["branch"].as_str().unwrap_or("").to_owned(),
                        item["ahead"].to_string(),
                        item["status"].as_str().unwrap_or("unknown").to_owned(),
                        item["worktreeStatus"]
                            .as_str()
                            .unwrap_or("unknown")
                            .to_owned(),
                        item["conflicts"]
                            .as_array()
                            .map(|paths| {
                                paths
                                    .iter()
                                    .filter_map(|p| p.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_default(),
                    ]
                })
                .collect::<Vec<_>>();
            print!(
                "{}",
                super::table::render(
                    &["BRANCH", "AHEAD", "MERGE", "WORKTREE", "CONFLICTS"],
                    &[40, 6, 10, 14, 60],
                    &rows
                )
            );
        }
        return Ok(0);
    }
    let json = args.json;
    let report = execute(args);
    if json {
        super::hub_client::print_json(&serde_json::to_value(&report)?)?;
        if let Some(error) = &report.error {
            eprintln!("{error}");
        }
    } else if let Some(summary) = &report.queue {
        render_queue(&report, summary);
    } else {
        for step in &report.steps {
            println!(
                "{}: {} ({} ms){}",
                step.name,
                step.status,
                step.duration_ms,
                if step.retried { " [retried]" } else { "" }
            );
            if let Some(crates) = &step.crates {
                println!(
                    "  crates: {} ({})",
                    crates.join(", "),
                    step.selection.as_deref().unwrap_or("")
                );
            }
        }
        println!("merge: {}", report.status);
        if let Some(sha) = &report.merged {
            println!("merged commit: {sha}");
        }
        if let Some(error) = &report.error {
            eprintln!("{error}");
        }
        for path in &report.conflicts {
            eprintln!("conflict: {path}");
        }
    }
    Ok(report.exit_code)
}

/// Shared synchronous operation; MCP runs this on a blocking worker.
pub(crate) fn execute(args: MergeArgs) -> MergeReport {
    // Concurrency guard: one merge/queue per repository (+ one per gate
    // target directory). Contention is a structured exit-3 report, available
    // to both the CLI and the MCP entry points.
    let _locks = match acquire_locks(&args) {
        Ok(locks) => locks,
        Err(error) => match error.downcast_ref::<lock::Contended>() {
            Some(contended) => return locked_report(&args, &contended.0),
            None => {
                let mut report = new_report(
                    args.branch.as_deref().unwrap_or(&args.queue.join(", ")),
                    &args,
                );
                report.exit_code = 1;
                report.status = "gate_failed".into();
                report.error = Some(format!("{error:#}"));
                return report;
            }
        },
    };
    if !args.queue.is_empty() {
        return queue::run_queue(args);
    }
    let mut report = new_report(args.branch.as_deref().unwrap_or_default(), &args);
    let mut temporary = None;
    // 1. Verify (construct merge, run gate). Land-only skips this.
    let verified = if args.land && !args.gate {
        Ok(())
    } else {
        execute_inner(&args, &mut report, &mut temporary)
    };
    if let Err(error) = verified {
        apply_stop(&mut report, error);
    } else {
        report.exit_code = 0;
        if report.status == "gate_failed" {
            report.status = if args.dry_run {
                "dry_run"
            } else if args.onto.is_some() && !args.land {
                "verified"
            } else {
                "ok"
            }
            .into();
        }
        // 2. Persist the passing verification so --land can consume it.
        if args.onto.is_some() {
            persist_report(&args, &report);
        }
        // 3. Verify-then-land, or land-only, publishes main immediately.
        if args.land
            && let Err(error) = execute_land(&args, &mut report)
        {
            apply_stop(&mut report, error);
        }
    }
    // A failed gate leaves a persisted report too, naming the merge and base
    // that failed, for inspection and for the queue to read the verdict. The
    // merge never lands, so drop its reachability pin.
    if args.onto.is_some() && report.exit_code != 0 {
        persist_report(&args, &report);
        if let (Some(repo), Some(reference), Some(base)) = (
            report.repo_root.as_ref(),
            report.reference.as_ref(),
            report.base.as_ref(),
        ) {
            let _ = git(
                repo,
                &["update-ref", "-d", &reports::verified_ref(reference, base)],
            );
        }
    }
    if let Some(mut temporary) = temporary
        && let Err(error) = record(&mut report, "cleanup", || temporary.remove())
    {
        if report.exit_code == 0 {
            report.exit_code = 1;
            report.status = "gate_failed".into();
        }
        report.error = Some(format!(
            "{}; cleanup: {error:#}",
            report.error.as_deref().unwrap_or("merge completed")
        ));
    }
    report
}

/// Acquire the concurrency-guard locks before running a merge/queue.
fn acquire_locks(args: &MergeArgs) -> Result<lock::MergeLocks> {
    if args.dry_run {
        return Ok(lock::MergeLocks::empty());
    }
    let cwd = args.repo.clone().unwrap_or(std::env::current_dir()?);
    let repo = PathBuf::from(git(&cwd, &["rev-parse", "--show-toplevel"])?);
    if !args.queue.is_empty() {
        // The queue parent holds the repository lock; lane children are
        // marked QUEUE_WORKER and lock their own per-lane target directories.
        lock::MergeLocks::acquire_repo_lock(&repo, args.wait)
    } else {
        // The queue parent already suffixes per-lane target dirs before
        // spawning children, so use the (already resolved) --target-dir as
        // given instead of re-applying the lane suffix.
        let target = args.gate.then(|| {
            args.target_dir
                .as_ref()
                .map(|path| repo.join(path))
                .unwrap_or_else(|| repo.join("target-gate"))
        });
        lock::MergeLocks::acquire(&repo, target.as_deref(), args.wait)
    }
}

fn locked_report(args: &MergeArgs, info: &lock::LockInfo) -> MergeReport {
    let name = if args.queue.is_empty() {
        args.branch.clone().unwrap_or_default()
    } else {
        format!("queue[{}]", args.queue.join(", "))
    };
    let mut report = new_report(&name, args);
    if !args.queue.is_empty() {
        report.branch = name;
    }
    report.exit_code = 3;
    report.status = "locked".into();
    report.error = Some(lock::Contended(info.clone()).to_string());
    report
}

fn apply_stop(report: &mut MergeReport, error: anyhow::Error) {
    match error.downcast_ref::<MergeStop>() {
        Some(MergeStop::Conflict(files)) => {
            report.exit_code = 2;
            report.status = "conflict".into();
            report.conflicts = files.clone();
        }
        Some(MergeStop::CasLost) => {
            report.exit_code = 3;
            report.status = "cas_lost".into();
        }
        Some(MergeStop::BaseMoved(_, current)) => {
            report.exit_code = 2;
            report.status = "base_moved".into();
            report.current_main = Some(current.clone());
        }
        None => {
            report.exit_code = 1;
            report.status = "gate_failed".into();
        }
    }
    report.error = Some(format!("{error:#}"));
}

fn persist_report(args: &MergeArgs, report: &MergeReport) {
    if !args.gate && args.land {
        return; // land-only consumes a report; it never rewrites it
    }
    let (Some(repo), Some(reference)) = (report.repo_root.clone(), report.reference.clone()) else {
        return;
    };
    if let Err(error) = reports::save(&repo, &reference, report) {
        eprintln!("merge: could not persist verification report: {error:#}");
    }
}

fn render_queue(report: &MergeReport, summary: &queue::QueueSummary) {
    println!(
        "queue: {} ({} lane{})",
        report.status,
        summary.lanes,
        if summary.lanes == 1 { "" } else { "s" }
    );
    for outcome in &summary.branches {
        println!(
            "{}. {}: {}",
            outcome.order + 1,
            outcome.branch,
            outcome.status
        );
        for verification in &outcome.verifications {
            println!(
                "   lane {} verified on {}{}: {} (reused={})",
                verification.lane,
                verification.base,
                if verification.speculative {
                    " (speculative)"
                } else {
                    ""
                },
                verification.status,
                verification.reused,
            );
        }
        if let Some(sha) = &outcome.landed_sha {
            println!("   landed: {sha}");
        }
        println!("   {}", outcome.why);
    }
    if let Some(after) = &summary.main_after {
        println!("main: {} -> {after}", summary.main_before);
    }
    if let Some(error) = &report.error {
        eprintln!("{error}");
    }
}

fn new_report(branch: &str, args: &MergeArgs) -> MergeReport {
    MergeReport {
        exit_code: 1,
        status: "gate_failed".into(),
        branch: branch.into(),
        dry_run: args.dry_run,
        affected: args.affected && !args.full,
        expected_main: None,
        source: None,
        merged: None,
        base: None,
        head: None,
        tree: None,
        current_main: None,
        target_dir: None,
        gate_override: std::env::var_os("REMUDA_MERGE_GATE_COMMAND")
            .is_some_and(|value| !value.is_empty()),
        web: args.web,
        web_e2e: args.web_e2e,
        main_updated: false,
        pushed: false,
        conflicts: Vec::new(),
        steps: Vec::new(),
        error: None,
        queue: None,
        repo_root: None,
        reference: None,
    }
}

fn execute_inner(
    args: &MergeArgs,
    report: &mut MergeReport,
    temporary: &mut Option<TemporaryWorktree>,
) -> Result<()> {
    let (repo, reference) = record(report, "repository", || {
        ensure!(args.gate || args.dry_run, "--gate or --dry-run is required");
        let cwd = args.repo.clone().unwrap_or(std::env::current_dir()?);
        let repo = PathBuf::from(git(&cwd, &["rev-parse", "--show-toplevel"])?);
        let reference = branch_ref(args.branch.as_deref().context("branch is required")?)?;
        ensure!(
            args.message
                .as_ref()
                .is_none_or(|message| !message.trim().is_empty()),
            "--message must not be empty"
        );
        git(&repo, &["check-ref-format", &reference])?;
        Ok((repo, reference))
    })?;
    report.repo_root = Some(repo.clone());
    report.reference = Some(reference.clone());
    let verify_onto = args.onto.is_some();
    let target = args
        .target_dir
        .as_ref()
        .map(|path| repo.join(path))
        .unwrap_or_else(|| repo.join("target-gate"));
    report.target_dir = Some(target.clone());
    // `--onto` names an explicit local base; the caller controls freshness and
    // concurrent lanes must not race `git fetch` locks.
    if args.dry_run {
        report.steps.push(Step::planned("fetch"));
    } else if !verify_onto {
        record(report, "fetch", || git(&repo, &["fetch", "origin"]))?;
    }
    let (expected, source) = record(report, "preflight", || {
        let expected = if let Some(onto) = args.onto.as_deref() {
            resolve_onto(&repo, onto)?
        } else {
            resolve(&repo, "refs/heads/main")?
        };
        if !verify_onto {
            ensure!(
                is_ancestor(&repo, &expected, &resolve(&repo, "HEAD")?)?,
                "checkout is behind main; update the coordinator checkout before merging"
            );
            if let Ok(remote) = resolve(&repo, "refs/remotes/origin/main") {
                ensure!(
                    is_ancestor(&repo, &remote, &expected)?,
                    "local main is behind or diverged from origin/main; refresh the coordinator checkout before merging"
                );
            }
        }
        check_branch_worktrees(&repo, &reference)?;
        Ok((expected, resolve(&repo, &reference)?))
    })?;
    report.expected_main = Some(expected.clone());
    report.base = Some(expected.clone());
    report.source = Some(source.clone());
    report.head = Some(source.clone());

    if args.dry_run {
        let diff = git_output(
            &repo,
            &[
                "diff",
                "--name-only",
                "-z",
                &format!("{expected}...{source}"),
                "--",
            ],
        )?;
        successful(&diff)?;
        report.web |= web_changed(&diff.stdout);
        report.web_e2e |= web_e2e::changed(&diff.stdout);
        report.web |= report.web_e2e;
        report.steps.push(Step::planned("worktree"));
        report.steps.push(Step::planned("merge"));
        report.steps.extend(gate_plan(
            &repo,
            report.web,
            test_range(report),
            report.web_e2e,
        )?);
        report.steps.push(generated_api::plan(report.web));
        report.steps.push(Step::planned("verify-tree"));
        report.steps.push(Step::planned("record-gate"));
        report.steps.push(Step::planned("update-main"));
        if !args.no_push {
            report.steps.push(Step::planned("push"));
        }
        report.steps.push(Step::planned("cleanup"));
        return Ok(());
    }

    let worktree = record(report, "worktree", || {
        let guard = TemporaryWorktree::new(&repo)?;
        let path = guard.path.clone();
        *temporary = Some(guard);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--detach",
                &path.to_string_lossy(),
                &expected,
            ],
        )?;
        Ok(path)
    })?;
    let message = args.message.clone().unwrap_or_else(|| {
        format!(
            "merge: {} into main",
            reference.trim_start_matches("refs/heads/")
        )
    });
    let mut merged = record(report, "merge", || {
        let output = git_output(
            &worktree,
            &[
                "-c",
                "core.editor=true",
                "merge",
                "--no-ff",
                "--no-edit",
                "-m",
                &message,
                &source,
            ],
        )?;
        if !output.status.success() {
            let conflicts =
                git_output(&worktree, &["diff", "--name-only", "--diff-filter=U", "-z"])?;
            successful(&conflicts)?;
            let files = nul_strings(&conflicts.stdout)?;
            if !files.is_empty() {
                return Err(MergeStop::Conflict(files).into());
            }
            successful(&output)?;
        }
        resolve(&worktree, "HEAD")
    })?;
    report.merged = Some(merged.clone());
    report.tree = Some(git(&worktree, &["rev-parse", "HEAD^{tree}"])?);
    if verify_onto {
        // Keep the constructed merge reachable after the worktree goes away
        // and publish the preparing sidecar for speculative queue lanes.
        record(report, "pin-merge", || {
            git(
                &repo,
                &[
                    "update-ref",
                    &reports::verified_ref(&reference, &expected),
                    &merged,
                ],
            )?;
            reports::write_preparing(&repo, &reference, &expected, &merged, &source)
                .context("publish preparing report")
        })?;
    }
    let diff = git_output(
        &worktree,
        &["diff", "--name-only", "-z", &expected, &merged, "--"],
    )?;
    successful(&diff)?;
    report.web |= web_changed(&diff.stdout);
    report.web_e2e |= web_e2e::changed(&diff.stdout);
    report.web |= report.web_e2e;
    let report_file = worktree.with_file_name("gate.jsonl");
    run_gate(report, &worktree, &target, &report_file, args)?;
    record(report, "verify-tree", || {
        ensure!(
            resolve(&worktree, "HEAD")? == merged,
            "gate changed the merge HEAD"
        );
        git(&worktree, &["diff", "--exit-code", "HEAD", "--"])?;
        git(
            &worktree,
            &["diff", "--cached", "--exit-code", "HEAD", "--"],
        )?;
        Ok(())
    })?;
    if merged != expected {
        let summary = gate_summary(report);
        if verify_onto {
            // The merge sha is the identity a queue lane speculates onto, so
            // it must not change after the gate. Attach the gate summary as a
            // git note (refs/notes/remuda-gate) instead of amending.
            record(report, "record-gate", || {
                let note_args = [
                    "notes",
                    "--ref=remuda-gate",
                    "add",
                    "--force",
                    "-m",
                    summary.as_str(),
                    merged.as_str(),
                ];
                git(&worktree, &note_args)?;
                ensure!(
                    resolve(&worktree, "HEAD")? == merged,
                    "recording the gate note ref changed HEAD"
                );
                Ok(())
            })?;
        } else {
            merged = record(report, "record-gate", || {
                let tree = git(&worktree, &["rev-parse", "HEAD^{tree}"])?;
                git(
                    &worktree,
                    &[
                        "commit", "--amend", "--only", "-m", &message, "-m", &summary,
                    ],
                )?;
                ensure!(
                    git(&worktree, &["rev-parse", "HEAD^{tree}"])? == tree,
                    "recording gate summary changed the verified tree"
                );
                resolve(&worktree, "HEAD")
            })?;
            report.merged = Some(merged.clone());
            report.tree = Some(git(&worktree, &["rev-parse", "HEAD^{tree}"])?);
        }
    }
    if verify_onto {
        // Verification only: the report (persisted by execute) is the claim.
        // Advancing main is a separate `--land` compare-and-swap.
        return Ok(());
    }
    record(report, "update-main", || {
        let output = git_output(
            &repo,
            &["update-ref", "refs/heads/main", &merged, &expected],
        )?;
        if !output.status.success()
            && resolve(&repo, "refs/heads/main").ok().as_ref() != Some(&expected)
        {
            return Err(MergeStop::CasLost.into());
        }
        successful(&output)
    })?;
    report.main_updated = true;
    if !args.no_push {
        record(report, "push", || {
            // Push exactly the verified commit even if another coordinator moves
            // local main immediately after our successful CAS. Never force-push.
            git(
                &repo,
                &["push", "origin", &format!("{merged}:refs/heads/main")],
            )
        })?;
        report.pushed = true;
    }
    Ok(())
}

fn branch_ref(branch: &str) -> Result<String> {
    let name = branch.strip_prefix("refs/heads/").unwrap_or(branch);
    ensure!(
        !name.is_empty() && !name.starts_with('-'),
        "invalid local branch"
    );
    ensure!(name != "main", "source branch must differ from main");
    Ok(format!("refs/heads/{name}"))
}

/// Resolve `--onto`: the literal `main`, or a local commit/sha.
fn resolve_onto(repo: &Path, onto: &str) -> Result<String> {
    let reference = if onto == "main" {
        "refs/heads/main"
    } else {
        onto
    };
    resolve(repo, &format!("{reference}^{{commit}}"))
}

/// Lane target directories keep parallel builds out of one Cargo lock.
/// Lane 1 keeps the historical `<target-dir>` / `target-gate` path.
pub(crate) fn lane_target_dir(repo: &Path, target_dir: Option<&Path>, lane: usize) -> PathBuf {
    let target = target_dir
        .map(|path| repo.join(path))
        .unwrap_or_else(|| repo.join("target-gate"));
    if lane <= 1 {
        target
    } else {
        let mut name = target
            .file_name()
            .map(|name| name.to_os_string())
            .unwrap_or_else(|| std::ffi::OsString::from("target-gate"));
        name.push(format!("-lane{lane}"));
        target.with_file_name(name)
    }
}

/// `--land`: publish a previously verified merge without touching the gate.
fn execute_land(args: &MergeArgs, report: &mut MergeReport) -> Result<()> {
    let (repo, reference) = record(report, "repository", || {
        let cwd = args.repo.clone().unwrap_or(std::env::current_dir()?);
        let repo = PathBuf::from(git(&cwd, &["rev-parse", "--show-toplevel"])?);
        let reference = branch_ref(args.branch.as_deref().context("branch is required")?)?;
        git(&repo, &["check-ref-format", &reference])?;
        Ok((repo, reference))
    })?;
    report.repo_root = Some(repo.clone());
    report.reference = Some(reference.clone());
    let onto = args.onto.as_deref().context("--land requires --onto")?;
    let (base, verified) = record(report, "preflight", || {
        let base = resolve_onto(&repo, onto)?;
        let verified = reports::load(&repo, &reference, &base)?;
        ensure!(
            verified.exit_code == 0 && verified.merged.is_some(),
            "verification report for {} on {base} is not a passing gate",
            reference.trim_start_matches("refs/heads/")
        );
        let merged = verified.merged.as_deref().unwrap_or_default();
        let head = verified.head.as_deref().context("report has no head")?;
        // The branch snapshot must be exactly the one that was verified.
        let current_head = resolve(&repo, &reference)?;
        ensure!(
            head == current_head,
            "branch moved since verification ({head} -> {current_head}); re-verify before landing"
        );
        // The merge commit and its parentage must still be present locally.
        git(&repo, &["cat-file", "-e", &format!("{merged}^{{commit}}")])?;
        let first = git(&repo, &["rev-parse", &format!("{merged}^1")])?;
        let second = git(&repo, &["rev-parse", &format!("{merged}^2")])?;
        ensure!(first == base, "verified merge is not based on {base}");
        ensure!(
            second == head,
            "verified merge does not contain the pinned branch head"
        );
        Ok((base, verified))
    })?;
    report.base = Some(base.clone());
    report.head = verified.head.clone();
    report.tree = verified.tree.clone();
    report.source = verified.head.clone();
    report.expected_main = Some(base.clone());
    let merged = verified.merged.clone().unwrap_or_default();
    report.merged = Some(merged.clone());
    report.affected = verified.affected;
    report.web = verified.web;
    report.web_e2e = verified.web_e2e;
    record(report, "land", || {
        let current = resolve(&repo, "refs/heads/main")?;
        if current != base {
            return Err(MergeStop::BaseMoved(base.clone(), current).into());
        }
        let output = git_output(&repo, &["update-ref", "refs/heads/main", &merged, &base])?;
        if !output.status.success() {
            let now = resolve(&repo, "refs/heads/main")?;
            if now != base {
                return Err(MergeStop::BaseMoved(base.clone(), now).into());
            }
            successful(&output)?;
        }
        Ok(())
    })?;
    report.main_updated = true;
    report.status = "landed".into();
    // The merge is main now and reachable on its own; drop the pin.
    let _ = git(
        &repo,
        &[
            "update-ref",
            "-d",
            &reports::verified_ref(&reference, &base),
        ],
    );
    if !args.no_push {
        record(report, "push", || {
            git(
                &repo,
                &["push", "origin", &format!("{merged}:refs/heads/main")],
            )
        })?;
        report.pushed = true;
    }
    Ok(())
}

fn gate_summary(report: &MergeReport) -> String {
    let mut summary = format!("Gate: passed (override={})\n", report.gate_override);
    for step in report.steps.iter().filter(|step| !step.command.is_empty()) {
        summary.push_str(&format!(
            "\n{}: {} ({} ms), attempts={}, retried={}",
            step.name, step.status, step.duration_ms, step.attempts, step.retried
        ));
    }
    summary
}

fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let output = git_output(repo, &["merge-base", "--is-ancestor", ancestor, descendant])?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => {
            successful(&output)?;
            Ok(false)
        }
    }
}

/// Inspect the local coordinator queue without changing refs or worktrees.
pub(crate) fn pending(repo: Option<&Path>) -> Result<serde_json::Value> {
    use serde_json::json;
    let cwd = repo
        .map(Path::to_path_buf)
        .unwrap_or(std::env::current_dir()?);
    let root = PathBuf::from(git(&cwd, &["rev-parse", "--show-toplevel"])?);
    let main = resolve(&root, "refs/heads/main")?;
    let refs = git(
        &root,
        &["for-each-ref", "--format=%(refname)", "refs/heads/wt/"],
    )?;
    let mut items = Vec::new();
    for reference in refs.lines() {
        let source = resolve(&root, reference)?;
        let ahead: u64 = git(
            &root,
            &["rev-list", "--count", &format!("{main}..{source}")],
        )?
        .parse()?;
        if ahead == 0 {
            continue;
        }
        let merge = git_output(
            &root,
            &[
                "merge-tree",
                "--write-tree",
                "--name-only",
                "--no-messages",
                "-z",
                &main,
                &source,
            ],
        )?;
        let (status, conflicts) = match merge.status.code() {
            Some(0) => ("clean", Vec::new()),
            Some(1) => (
                "conflict",
                nul_strings(&merge.stdout)?.into_iter().skip(1).collect(),
            ),
            _ => {
                successful(&merge)?;
                unreachable!("failed merge-tree")
            }
        };
        let safety = check_branch_worktrees(&root, reference);
        items.push(json!({"branch":reference.trim_start_matches("refs/heads/"),"source":source,"ahead":ahead,
            "status":status,"conflicts":conflicts,"worktreeStatus":if safety.is_ok() { "clean" } else { "blocked" },
            "reason":safety.err().map(|error| format!("{error:#}"))}));
    }
    Ok(json!({"exitCode":0,"main":main,"items":items}))
}

fn web_changed(paths: &[u8]) -> bool {
    paths
        .split(|byte| *byte == 0)
        .any(|path| path.starts_with(b"crates/") || path.starts_with(b"web/"))
}

fn record<T>(
    report: &mut MergeReport,
    name: &str,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let started = Instant::now();
    let result = operation();
    let mut step = Step::planned(name);
    step.status = if result.is_ok() { "ok" } else { "failed" }.into();
    step.duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    step.attempts = 1;
    step.error = result.as_ref().err().map(|error| format!("{error:#}"));
    report.steps.push(step);
    result
}

fn test_range(report: &MergeReport) -> Option<(&str, &str)> {
    report
        .expected_main
        .as_deref()
        .zip(report.merged.as_deref().or(report.source.as_deref()))
        .filter(|_| report.affected)
}

// Execution always uses the merge-result worktree; dry-run uses the synchronized checkout.
fn gate_command(
    gate_worktree: &Path,
    web: bool,
    range: Option<(&str, &str)>,
    web_e2e: bool,
) -> Command {
    let mut command = Command::new("bash");
    command
        .arg(gate_worktree.join("scripts/ci/gate.sh"))
        .current_dir(gate_worktree)
        .stdin(Stdio::null());
    if web {
        command.arg("--web");
    }
    if web_e2e {
        command.arg("--web-e2e");
    }
    if let Some((base, head)) = range {
        command.args(["--affected", "--base", base, "--head", head]);
    } else {
        command.arg("--full");
    }
    command
}

fn gate_plan(
    repo: &Path,
    web: bool,
    range: Option<(&str, &str)>,
    web_e2e: bool,
) -> Result<Vec<Step>> {
    ensure!(
        repo.join("scripts/ci/gate.sh").is_file(),
        "scripts/ci/gate.sh is missing from the selected tree; the checkout may be behind main"
    );
    let output = gate_command(repo, web, range, web_e2e)
        .arg("--list")
        .output()
        .context("read gate plan")?;
    successful(&output)?;
    serde_json::from_slice(&output.stdout).context("parse gate plan")
}

fn run_gate(
    report: &mut MergeReport,
    merged_worktree: &Path,
    target: &Path,
    report_file: &Path,
    args: &MergeArgs,
) -> Result<()> {
    let planned = gate_plan(
        merged_worktree,
        report.web,
        test_range(report),
        report.web_e2e,
    )?;
    let mut command = gate_command(
        merged_worktree,
        report.web,
        test_range(report),
        report.web_e2e,
    );
    command
        .arg("--report")
        .arg(report_file)
        .env("CARGO_TARGET_DIR", target)
        .env("CARGO_INCREMENTAL", "0");
    // Per-lane Hub ports: lane 1 keeps --e2e-port-base, lane N shifts by 10.
    let shift = 10u16 * u16::try_from(args.e2e_lane.saturating_sub(1)).unwrap_or(0);
    let hub_port = args.e2e_port_base.saturating_add(shift);
    let web_port = hub_port.saturating_add(9);
    command
        .env("HUB_E2E_LISTEN", format!("127.0.0.1:{hub_port}"))
        .env("HUB_E2E_WEB_PORT", web_port.to_string());
    if let Some(lock) = &args.e2e_lock {
        command.env("REMUDA_E2E_LOCK", lock);
    }
    let status = command
        .stdout(Stdio::from(std::io::stderr()))
        .stderr(Stdio::inherit())
        .status()
        .context("run gate")?;
    let results = fs::read_to_string(report_file).context("read gate report")?;
    let steps: Vec<Step> = results
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()
        .context("parse gate report")?;
    let complete = steps.len() == planned.len()
        && steps.iter().zip(&planned).all(|(actual, plan)| {
            actual.name == plan.name
                && actual.status
                    == if plan.status == "skipped" {
                        "skipped"
                    } else {
                        "ok"
                    }
        });
    report.steps.extend(steps);
    if !status.success() || !complete {
        report.steps.push(generated_api::plan(false));
    }
    ensure!(
        status.success() && complete,
        "gate failed or returned an incomplete step report"
    );
    generated_api::run(report, merged_worktree, target)
}

#[derive(Default)]
struct Worktree {
    path: PathBuf,
    branch: Option<String>,
    bare: bool,
}

fn worktrees(repo: &Path) -> Result<Vec<Worktree>> {
    let output = git_output(repo, &["worktree", "list", "--porcelain", "-z"])?;
    successful(&output)?;
    let text = std::str::from_utf8(&output.stdout).context("non-UTF-8 worktree path")?;
    let mut trees = Vec::new();
    let mut current = Worktree::default();
    for field in text.split('\0') {
        if let Some(path) = field.strip_prefix("worktree ") {
            current.path = PathBuf::from(path);
        } else if let Some(branch) = field.strip_prefix("branch ") {
            current.branch = Some(branch.into());
        } else if field == "bare" {
            current.bare = true;
        } else if field.is_empty() && !current.path.as_os_str().is_empty() {
            trees.push(std::mem::take(&mut current));
        }
    }
    Ok(trees)
}

fn check_branch_worktrees(repo: &Path, reference: &str) -> Result<()> {
    for tree in worktrees(repo)? {
        if tree.bare {
            continue;
        }
        let attached = tree.branch.as_deref() == Some(reference);
        if !tree.path.exists() {
            ensure!(
                !attached,
                "source worktree is missing: {}",
                tree.path.display()
            );
            continue;
        }
        let git_dir = PathBuf::from(git(&tree.path, &["rev-parse", "--absolute-git-dir"])?);
        let rebases = [git_dir.join("rebase-merge"), git_dir.join("rebase-apply")];
        // Rebase detaches HEAD, so `worktree list` no longer names the branch.
        let mut rebasing_source = false;
        for path in rebases.iter().filter(|path| path.exists()) {
            let name = fs::read_to_string(path.join("head-name"))
                .with_context(|| format!("identify rebase in {}", tree.path.display()))?;
            rebasing_source |= name.trim() == reference;
        }
        if attached || rebasing_source {
            ensure!(
                !git_dir.join("MERGE_HEAD").exists() && !rebases.iter().any(|path| path.exists()),
                "source worktree has a rebase/merge in progress: {}",
                tree.path.display()
            );
            let staged = git_output(&tree.path, &["diff", "--cached", "--name-only", "-z"])?;
            successful(&staged)?;
            ensure!(
                staged.stdout.is_empty(),
                "source worktree has staged changes: {:?}",
                nul_strings(&staged.stdout)?
            );
        }
    }
    Ok(())
}

fn nul_strings(bytes: &[u8]) -> Result<Vec<String>> {
    Ok(std::str::from_utf8(bytes)
        .context("non-UTF-8 git path")?
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect())
}

fn resolve(repo: &Path, reference: &str) -> Result<String> {
    git(
        repo,
        &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
    )
}

fn git_output(repo: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .current_dir(repo)
        .args(args)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_MERGE_AUTOEDIT", "no")
        .output()
        .with_context(|| format!("git {}", args.first().unwrap_or(&"")))
}

fn successful(output: &Output) -> Result<()> {
    ensure!(
        output.status.success(),
        "command failed ({}): {}{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    Ok(())
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = git_output(repo, args)?;
    successful(&output)?;
    Ok(String::from_utf8(output.stdout)
        .context("non-UTF-8 git output")?
        .trim()
        .to_owned())
}

struct TemporaryWorktree {
    repo: PathBuf,
    /// Scratch root; only it and paths strictly inside it may be removed.
    scratch: PathBuf,
    path: PathBuf,
    removed: bool,
}

impl TemporaryWorktree {
    fn new(repo: &Path) -> Result<Self> {
        let scratch = scratch::create_root()?;
        let path = scratch.join("worktree");
        Ok(Self {
            repo: repo.into(),
            scratch,
            path,
            removed: false,
        })
    }

    fn remove(&mut self) -> Result<()> {
        if self.removed {
            return Ok(());
        }
        if worktrees(&self.repo)?
            .iter()
            .any(|tree| tree.path == self.path)
        {
            git(
                &self.repo,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    &self.path.to_string_lossy(),
                ],
            )?;
        }
        // Containment-checked deletion: no caller path can ever reach here.
        scratch::remove_within(&self.scratch, &self.scratch)?;
        self.removed = true;
        Ok(())
    }
}

impl Drop for TemporaryWorktree {
    fn drop(&mut self) {
        if let Err(error) = self.remove() {
            tracing::error!(%error, "temporary merge worktree cleanup failed");
        }
    }
}

impl super::registry::Entrypoint for MergeArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        run(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affected_is_default_and_full_is_an_explicit_override() {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            args: MergeArgs,
        }
        let defaults = Cli::try_parse_from(["merge", "topic", "--gate"])
            .unwrap()
            .args;
        assert!(defaults.affected && !defaults.full);
        let full = Cli::try_parse_from(["merge", "topic", "--gate", "--full"])
            .unwrap()
            .args;
        assert!(full.full);
        assert!(Cli::try_parse_from(["merge", "topic", "--gate", "--full", "--affected"]).is_err());
        let mcp: MergeArgs =
            serde_json::from_value(serde_json::json!({"branch":"topic", "gate":true})).unwrap();
        assert!(mcp.affected && !mcp.full && !mcp.web_e2e);
        let e2e = Cli::try_parse_from(["merge", "topic", "--gate", "--web-e2e"])
            .unwrap()
            .args;
        assert!(e2e.web_e2e);
        let mcp_e2e: MergeArgs = serde_json::from_value(
            serde_json::json!({"branch":"topic", "gate":true, "webE2e":true}),
        )
        .unwrap();
        assert!(mcp_e2e.web_e2e);
    }

    #[test]
    fn branch_inputs_are_local_and_never_options() {
        assert_eq!(branch_ref("topic").unwrap(), "refs/heads/topic");
        assert_eq!(branch_ref("refs/heads/topic").unwrap(), "refs/heads/topic");
        for invalid in ["", "-bad", "main", "refs/heads/main"] {
            assert!(branch_ref(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn web_selection_uses_nul_delimited_repo_paths() {
        assert!(web_changed(b"crates/a.rs\0web/a file\n.ts\0"));
        assert!(web_changed(b"crates/remuda-hub/openapi/openapi.json\0"));
        assert!(!web_changed(
            b"webish/a.rs\0docs/web/a.rs\0cratesish/a.rs\0"
        ));
        assert!(!web_changed(b""));
    }

    #[test]
    fn gate_plan_has_shared_order_and_optional_web_steps() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let plan = gate_plan(&repo, false, None, false).unwrap();
        let names: Vec<_> = plan.iter().map(|step| step.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "secret-scan",
                "no-tunnel-scan",
                "cargo-fmt",
                "cargo-check",
                "cargo-clippy",
                "cargo-test",
                "web-install",
                "web-build",
                "web-test",
                "web-hub-e2e"
            ]
        );
        assert!(plan[6..].iter().all(|step| step.status == "skipped"));
        assert!(
            gate_plan(&repo, true, None, true)
                .unwrap()
                .iter()
                .all(|step| step.status == "planned")
        );
        let output = gate_command(&repo, false, None, false)
            .args(["--list", "--web-only"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let web_only: Vec<Step> = serde_json::from_slice(&output.stdout).unwrap();
        assert!(web_only[..6].iter().all(|step| step.status == "skipped"));
        assert!(web_only[6..9].iter().all(|step| step.status == "planned"));
        assert_eq!(web_only[9].status, "skipped");
    }
}
