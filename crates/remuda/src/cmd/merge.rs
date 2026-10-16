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
    /// Internal: land the immutable verified report keyed by this merge sha
    /// (the exact base/head/merged tuple the gate captured). Prevents a second
    /// verification on the same base from substituting the report landed.
    #[arg(long, hide = true, requires = "land")]
    #[serde(default)]
    pub report: Option<String>,
    /// Verify the listed branches as an optimistic queue across --lanes lanes.
    #[arg(long, num_args = 1.., value_name = "BRANCH", conflicts_with_all = ["dry_run", "list", "land", "onto", "message", "branch"])]
    #[serde(default)]
    pub queue: Vec<String>,
    /// Verification lanes for --queue (each lane gets its own target directory).
    #[arg(long, default_value_t = 2)]
    #[serde(default = "default_lanes")]
    pub lanes: usize,
    /// Base port for per-lane Hub e2e pairs; lane N uses base+10*(N-1), +9
    /// (web), and +1 (upstream). Absent, an inherited HUB_E2E_LISTEN keeps its
    /// port; otherwise 58980.
    #[arg(long)]
    #[serde(default)]
    pub e2e_port_base: Option<u16>,
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

#[cfg(test)]
impl MergeArgs {
    /// Minimal args for unit tests that only build a MergeReport.
    pub(crate) fn default_for_test() -> Self {
        Self {
            branch: None,
            gate: false,
            list: false,
            message: None,
            dry_run: false,
            onto: None,
            land: false,
            report: None,
            queue: Vec::new(),
            lanes: 2,
            e2e_port_base: None,
            e2e_lock: None,
            e2e_lane: 1,
            affected: true,
            full: false,
            web: false,
            web_e2e: false,
            no_push: false,
            json: false,
            wait: false,
            repo: None,
            target_dir: None,
        }
    }
}

fn default_lanes() -> usize {
    2
}

fn default_e2e_lane() -> usize {
    1
}

/// Default Hub listen base when neither `--e2e-port-base` nor an inherited
/// `HUB_E2E_LISTEN` selects one.
pub(crate) const DEFAULT_E2E_PORT_BASE: u16 = 58980;

/// Parse the port out of a `host:port` (or bare `:port`) environment value.
fn port_from_host_port(value: &str) -> Option<u16> {
    value.rsplit(':').next()?.parse().ok()
}

/// The lane-1 Hub base port in precedence order: the explicit
/// `--e2e-port-base` flag, an inherited `HUB_E2E_LISTEN`'s port, then the
/// 58980 default. The queue driver resolves this once and passes it
/// explicitly to every lane so inherited ports shift with the lane block.
pub(crate) fn e2e_port_base(args: &MergeArgs) -> u16 {
    args.e2e_port_base.unwrap_or_else(|| {
        std::env::var("HUB_E2E_LISTEN")
            .ok()
            .and_then(|value| port_from_host_port(&value))
            .unwrap_or(DEFAULT_E2E_PORT_BASE)
    })
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
    /// r15 R5: the push's server-side outcome cannot be ESTABLISHED. The push
    /// returned a failure, but the recovery fetch/ancestry read needed to prove
    /// the ref absent (or present) itself failed, so the merge may already have
    /// landed. Never a `failed` verdict: the consumer (the Node land path) keeps
    /// its Landing occupancy and the Hub reconciles the exact merge against the
    /// remote. The carried text records what could and could not be established.
    #[error("land push outcome unknown: {0}")]
    PushUncertain(String),
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
    // r15 R5 child-start barrier: when the Node spawned this process as a
    // granted LAND child, it blocks here on the inherited pipe fd until the
    // Node has persisted the durable land-group fence and writes one release
    // byte. EOF/error before the byte means the parent could not fence the
    // granted push — exit BEFORE acquiring locks or touching git, so an
    // unfenced child can never push.
    await_start_barrier();
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
    //
    // r15 R5: an UNCERTAIN push outcome is not a failure — the exact merge may
    // already be on the remote — so keep its reachability pin like a
    // cancel-after-verify; dropping it could garbage-collect a merge the remote
    // accepted while the acknowledgement was lost.
    if args.onto.is_some() && report.exit_code != 0 && report.status != "uncertain" {
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

/// Block on the Node's granted-land child-start barrier when present (env
/// `REMUDA_START_BARRIER_FD` names the fd carrying the release byte — the
/// granted land child's stdin, fd 0): the Node writes one byte only after the
/// granted land group's durable fence record exists. EOF (the parent dropped
/// the write end because fencing failed) or a read error exits the process
/// before any git operation, so nothing can be pushed by an unfenced child
/// (r15 R5). After the release, /dev/null is dup2'd over the fd so later
/// gate/git children inherit a neutral stdin rather than the drained pipe.
#[cfg(unix)]
fn await_start_barrier() {
    use std::os::fd::{AsRawFd, RawFd};
    let Some(fd) = std::env::var("REMUDA_START_BARRIER_FD")
        .ok()
        .and_then(|raw| raw.parse::<RawFd>().ok())
    else {
        return;
    };
    let mut byte = [0u8; 1];
    loop {
        match nix::unistd::read(fd, &mut byte) {
            Ok(0) => {
                eprintln!(
                    "remuda merge: start barrier closed before release; the land fence was not \
                     persisted — refusing to run"
                );
                std::process::exit(4);
            }
            Ok(_) => break,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(error) => {
                eprintln!("remuda merge: start barrier read failed ({error}); refusing to run");
                std::process::exit(4);
            }
        }
    }
    // Replace the drained barrier fd (the land child's stdin) with /dev/null so
    // gate/git descendants never inherit the pipe.
    if let Ok(null) = std::fs::File::open("/dev/null") {
        let _ = nix::unistd::dup2(null.as_raw_fd(), fd);
    }
}

#[cfg(not(unix))]
fn await_start_barrier() {}

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
        Some(MergeStop::PushUncertain(_)) => {
            // r15 R5: exit 4 / status "uncertain" — not a failure verdict. The
            // granted merge MAY be on the remote; the Node keeps the Landing
            // row and the Hub establishes the truth against the remote.
            report.exit_code = 4;
            report.status = "uncertain".into();
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
        // The gate verified the merge against the `--onto` base captured at
        // preflight. When the base is local main itself, re-read it AFTER the
        // gate: another coordinator may have advanced local main while the
        // checks ran, which would make this a stale-base verification. Surface
        // that as base_moved(expected, current) instead of persisting a
        // "verified" report a later --land would only reject. An explicit sha
        // base is caller-pinned and not checked here.
        if args.onto.as_deref() == Some("main") {
            let current_main = resolve(&repo, "refs/heads/main")?;
            if current_main != expected {
                return Err(MergeStop::BaseMoved(expected.clone(), current_main).into());
            }
        }
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
        record(report, "push", || push_main_cas(&repo, &merged, &expected))?;
        report.pushed = true;
    }
    Ok(())
}

/// Push the verified merge to origin main as a compare-and-swap against the
/// base the merge was built on. A successful push needs no lease because the
/// merge is a strict descendant of `expected`. If the push is rejected, fetch
/// origin and classify it: when remote main has advanced past `expected`
/// (an ORIGIN-only move the local checkout had not seen) return
/// `MergeStop::BaseMoved(expected, remote)` with the real current remote main;
/// any other failure stays a normal error.
/// Push the verified merge to the remote base branch as a compare-and-swap:
/// `git push origin <merged>:refs/heads/main`. A successful push needs no
/// lease because the merge is a strict descendant of `expected`.
///
/// r14 R4 + r15 R5: a failed push is NOT a verdict. Recover the situation
/// against the REMOTE state, and every recovery READ is itself classified as
/// Established or Unknown:
/// * the exact granted merge is present on remote main (a lost
///   acknowledgement — the ref update landed server-side but the client got an
///   error: a failing post-receive hook, a killed connection) → this LANDED;
///   never a failure and never a base-moved requeue;
/// * the merge is established ABSENT (a fresh fetch + ancestry succeeded) and
///   remote main moved to a descendant of `expected` (an origin-only base
///   move) → `MergeStop::BaseMoved`;
/// * the merge is established absent AND remote main is still `expected` → the
///   real push rejection (e.g. a hook refused, the ref never moved);
/// * ANY recovery read fails (fetch error, ancestry/ref resolution error) →
///   `MergeStop::PushUncertain`: the ref may have moved server-side, so the
///   outcome is Unknown and must be reconciled against the remote, never
///   reported failed.
fn push_main_cas(repo: &Path, merged: &str, expected: &str) -> Result<()> {
    let output = git_output(
        repo,
        &["push", "origin", &format!("{merged}:refs/heads/main")],
    )?;
    if output.status.success() {
        return Ok(());
    }
    // A failed push is NOT a verdict. Fetch and resolve against the REMOTE
    // before reporting anything (r14 R4): the acknowledgement may be lost
    // while the ref update committed server-side.
    //
    // r19 item 2 / R7: ONE absolute deadline (< the Hub's 30 s RPC timeout)
    // bounds EVERY recovery subprocess — fetch, ancestry, shallow and ref
    // reads — each routed through the R6 owner; expiry is PushUncertain.
    //
    // r27 item 1 / R5: fetch the EXACT requested base into a PRIVATE
    // recovery ref. A bare `git fetch origin` honors the configured
    // remote.origin.fetch refspec, which may omit main entirely — the fetch
    // then succeeds while refs/remotes/origin/main stays stale, and a lost-ack
    // landing gets classified against the old tip (false absent / false base
    // move).
    //
    // r28 item 1: the destination is a unique, FLAT per-invocation ref, so
    // overlapping recoveries (a Node probe and this CLI recovery on the same
    // common directory, or two CLI recoveries) cannot overwrite one another's
    // snapshot between fetch and SHA capture, and no leftover ref can block a
    // base rename. Every read below is pinned to the captured SHA.
    let deadline = Instant::now() + LAND_RECOVERY_DEADLINE;
    let remote_tip = recovery_fetch_base_sha(repo, "main", deadline)?;
    // The exact granted merge IS on remote main: the push landed, only the
    // acknowledgement failed. Treat as success — never failed/base-moved.
    if owned_is_ancestor(repo, merged, &remote_tip, deadline)? {
        return Ok(());
    }
    if remote_tip != expected && owned_is_ancestor(repo, expected, &remote_tip, deadline)? {
        // Origin-only base move: the merge is absent but main advanced from our
        // base to another commit — the real CAS loss the caller requeues on.
        return Err(MergeStop::BaseMoved(expected.to_owned(), remote_tip).into());
    }
    // Recovery established: the merge is absent and the remote base did not
    // advance past ours. The rejection itself (hook refusal, etc.) is the
    // established failure — the ref provably never moved.
    successful(&output)
}

/// An object ID in the repository's native format: SHA-1 (40 hex) or SHA-256
/// (64 hex). Git itself emitted the string from ls-remote; this bounds shapes.
fn is_native_object_hex(sha: &str) -> bool {
    (sha.len() == 40 || sha.len() == 64) && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Parse `git ls-remote --exit-code origin refs/heads/<base>` output. Exactly
/// one line naming the requested ref with a native-format object ID; an
/// absent base, ambiguous output or a malformed line is an error.
fn parse_ls_remote_base(output: &[u8], base_branch: &str) -> std::result::Result<String, String> {
    let expected_ref = format!("refs/heads/{base_branch}");
    let mut matches = Vec::new();
    for line in String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let mut parts = line.split_whitespace();
        let (Some(sha), Some(refname)) = (parts.next(), parts.next()) else {
            return Err("ls-remote returned a malformed line".to_owned());
        };
        if parts.next().is_some() {
            return Err("ls-remote returned a malformed line".to_owned());
        }
        if refname == expected_ref {
            matches.push(sha.to_owned());
        }
    }
    match matches.as_slice() {
        [sha] if is_native_object_hex(sha) => Ok(sha.clone()),
        [] => Err("the remote does not advertise the requested base branch".to_owned()),
        _ => Err("ls-remote advertised the requested base more than once".to_owned()),
    }
}

/// R10/r30 item 2: true ONLY for the exact diagnostics git emits when the
/// installed build cannot create a SHA-256 repository; matching is anchored to
/// the message forms, so a bad-config error whose PATH contains "sha256" or
/// "invalid" is not mistaken for an unsupported build.
#[cfg(test)]
fn is_unsupported_sha256_diagnostic(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    const SHAPES: &[&str] = &[
        "unknown option `object-format'",
        "unknown option '--object-format",
        "unknown value for --object-format",
        "unknown hash algorithm 'sha256'",
        "attempt to use sha256 has not been compiled in",
        "sha256 repositories are not supported",
    ];
    SHAPES.iter().any(|needle| s.contains(needle))
}

/// Run `git init --object-format=sha256` via `program`: true when created;
/// false ONLY on a recognized unsupported-format diagnostic; spawn or other
/// init failures panic with the detail.
#[cfg(test)]
fn probe_sha256_support_with(program: &str, probe_repo: &Path) -> bool {
    let output = match std::process::Command::new(program)
        .args([
            "init",
            "-q",
            "--object-format=sha256",
            "-b",
            "main",
            probe_repo.to_str().unwrap(),
        ])
        .output()
    {
        Ok(output) => output,
        Err(error) => panic!("failed to spawn {program} for the SHA-256 support probe: {error}"),
    };
    if output.status.success() {
        return true;
    }
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if is_unsupported_sha256_diagnostic(&stderr) {
        eprintln!(
            "skipping: this git lacks SHA-256 support: {}",
            stderr.trim()
        );
        false
    } else {
        panic!("SHA-256 probe init failed for an unrelated reason: {stderr}");
    }
}

#[cfg(test)]
fn probe_sha256_support(program: &str, probe_repo: &Path) -> bool {
    probe_sha256_support_with(program, probe_repo)
}

/// Read the authoritative remote `main` tip WITHOUT WRITING ANY REFS and
/// classify a failed push against it. Any failure is PushUncertain — recovery
/// never guesses.
///
/// R10 (r30): ref-free recovery, identical in shape to the Node gateway:
/// 1. `git ls-remote --exit-code origin refs/heads/main` reads the exact
///    advertised tip (a configured remote.origin.fetch cannot stale it);
/// 2. `git fetch --no-tags --no-write-fetch-head origin <sha>` fetches the
///    commit's objects with no destination refspec — no ref is created and
///    FETCH_HEAD is never written; --no-tags defeats a configured tagOpt
///    clobber;
/// 3. `git cat-file -e <sha>^{commit}` verifies the object.
///
/// Every command goes through the R6 owner under one absolute deadline.
fn recovery_fetch_base_sha(repo: &Path, base: &str, deadline: Instant) -> Result<String> {
    // 1. Authoritative advertised tip. Exit 2 means the base ref is not
    // advertised (the parser names that precisely); any other non-zero exit is
    // a transport/command failure; timeouts/spawn failures fail too.
    let remote_ref = format!("refs/heads/{base}");
    let listing = recovery_git(
        repo,
        &["ls-remote", "--exit-code", "origin", &remote_ref],
        "post-push remote base ls-remote",
        false,
        deadline,
    )?;
    if listing.status.code() != Some(2) && !listing.status.success() {
        return Err(MergeStop::PushUncertain(format!(
            "post-push remote base ls-remote exited {:?}: {}; push outcome unknown",
            listing.status.code(),
            String::from_utf8_lossy(&listing.stderr).trim()
        ))
        .into());
    }
    let remote_tip = parse_ls_remote_base(&listing.stdout, base)
        .map_err(|message| MergeStop::PushUncertain(format!("{message}; push outcome unknown")))?;
    // r30 item 1 TEST-ONLY seam: park after the remote SHA was read.
    #[cfg(test)]
    recovery_read_park(repo);
    // 2. Fetch the objects of THAT commit with an object-ID refspec and no
    //    destination. An empty --refmap ignores configured remote.origin.fetch
    //    mappings (a destination-less CLI refspec alone does not), so a
    //    configured `+<sha>:refs/tags/x` cannot force-write x; --no-prune
    //    neutralises fetch.prune / remote.origin.prune; --no-tags defeats a
    //    configured tagOpt=--tags clobber.
    recovery_git(
        repo,
        &[
            "fetch",
            "--no-tags",
            "--no-prune",
            "--refmap=",
            "--no-write-fetch-head",
            "origin",
            &remote_tip,
        ],
        "post-push remote base object fetch",
        true,
        deadline,
    )?;
    // 3. The fetched object must peel to a commit.
    recovery_git(
        repo,
        &["cat-file", "-e", &format!("{remote_tip}^{{commit}}")],
        "post-push fetched base commit verification",
        true,
        deadline,
    )?;
    Ok(remote_tip)
}

/// r30 item 1 TEST-ONLY seam: parks a CLI recovery after its `ls-remote` read
/// and before the object fetch. Keyed by repository path.
#[cfg(test)]
mod recovery_read_seam {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};

    pub(super) struct Slot {
        pub(super) arrived: AtomicBool,
        pub(super) released: AtomicBool,
    }

    pub(super) struct Park {
        pub(super) repo: String,
        pub(super) slots: Mutex<Vec<Arc<Slot>>>,
        pub(super) expected: usize,
        parked: AtomicUsize,
    }

    static SEAM: OnceLock<Mutex<Option<Arc<Park>>>> = OnceLock::new();

    fn seam() -> &'static Mutex<Option<Arc<Park>>> {
        SEAM.get_or_init(|| Mutex::new(None))
    }

    pub(super) fn arm(repo: &Path, expected: usize) {
        let slots = (0..expected)
            .map(|_| {
                Arc::new(Slot {
                    arrived: AtomicBool::new(false),
                    released: AtomicBool::new(false),
                })
            })
            .collect();
        *seam().lock().expect("seam") = Some(Arc::new(Park {
            repo: repo.to_string_lossy().into_owned(),
            slots: Mutex::new(slots),
            expected,
            parked: AtomicUsize::new(0),
        }));
    }

    pub(super) fn disarm() {
        *seam().lock().expect("seam") = None;
    }

    pub(super) fn park(repo: &Path) {
        let park = {
            let guard = seam().lock().expect("seam");
            match guard.as_ref() {
                Some(park) if park.repo == repo.to_string_lossy() => park.clone(),
                _ => return,
            }
        };
        let index = park.parked.fetch_add(1, Ordering::AcqRel);
        assert!(index < park.expected, "more recoveries parked than armed");
        let slot = park.slots.lock().expect("slots")[index].clone();
        slot.arrived.store(true, Ordering::Release);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !slot.released.load(Ordering::Acquire) {
            assert!(
                std::time::Instant::now() < deadline,
                "recovery park never released"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn park_for(repo: &Path) -> Arc<Park> {
        let guard = seam().lock().expect("seam");
        let park = guard.as_ref().expect("seam armed").clone();
        assert_eq!(park.repo, repo.to_string_lossy());
        park
    }

    fn slot(repo: &Path, index: usize) -> Arc<Slot> {
        park_for(repo).slots.lock().expect("slots")[index].clone()
    }

    pub(super) fn wait_arrived(repo: &Path, index: usize) {
        let slot = slot(repo, index);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !slot.arrived.load(Ordering::Acquire) {
            assert!(
                std::time::Instant::now() < deadline,
                "slotted recovery never arrived"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    pub(super) fn release(repo: &Path, index: usize) {
        let slot = slot(repo, index);
        slot.released.store(true, Ordering::Release);
    }
}

#[cfg(test)]
fn recovery_read_park(repo: &Path) {
    recovery_read_seam::park(repo);
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
    // r19 item 5: initialise report.merged from the PINNED land argument BEFORE
    // any fallible preflight. Every refusal on the granted land path (the
    // branch moved since verification, a stale/unsupported report, a missing
    // merge parent) then reports a `gate_failed` that NAMES the attempted merge
    // M, instead of omitting it (the verified report's merged was only stamped
    // after the whole preflight succeeded). The pinned load below proves the
    // report is immutable for exactly M, so the later stamp is the same sha;
    // the unpinned path leaves this None until the report loads.
    if let Some(pinned) = args.report.as_deref() {
        report.merged = Some(pinned.to_owned());
    }
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
        // Resolve the CURRENT local main up front only for the unpinned path.
        // When the caller pins the captured merge (the Node always does for a
        // granted land), the verified base comes from the immutable report —
        // loading the report must NOT be conditioned on current main: a local
        // main move between verify and land is a confirmed CAS move (classified
        // by the land step as `base_moved(expected, current)`), not a generic
        // report-load failure (r14 item 6).
        let current_base = resolve_onto(&repo, onto)?;
        let verified = match args.report.as_deref() {
            Some(pinned) => {
                let loaded = reports::load_by_merged(&repo, &reference, pinned)?;
                ensure!(
                    loaded.merged.as_deref() == Some(pinned),
                    "immutable report for {pinned} does not name that merge"
                );
                loaded
            }
            None => {
                let loaded = reports::load(&repo, &reference, &current_base)?;
                ensure!(
                    loaded.base.as_deref() == Some(current_base.as_str()),
                    "verification report was for a different base; re-verify before landing"
                );
                loaded
            }
        };
        let base = verified
            .base
            .clone()
            .context("verification report has no base")?;
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
    if args.no_push {
        // Nothing remote to wait for: the merge is main locally and reachable
        // on its own; drop the pin.
        let _ = git(
            &repo,
            &[
                "update-ref",
                "-d",
                &reports::verified_ref(&reference, &base),
            ],
        );
    } else {
        // r15 R5: drop the reachability pin ONLY once the push outcome is
        // established. A PushUncertain (lost ack + unreadable recovery) leaves
        // the pin in place so a merge the remote may already contain is never
        // garbage-collected before the Hub reconciles it.
        record(report, "push", || push_main_cas(&repo, &merged, &base))?;
        report.pushed = true;
        let _ = git(
            &repo,
            &[
                "update-ref",
                "-d",
                &reports::verified_ref(&reference, &base),
            ],
        );
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

/// r19 item 2 / R7: per-subprocess cap so one recovery git command cannot
/// consume the whole recovery budget.
const RECOVERY_GIT_CAP: std::time::Duration = std::time::Duration::from_secs(10);

/// r19 item 2 / R7: ONE absolute deadline for the whole post-push recovery
/// sequence (fetch + ref read + shallow check + ancestry). Stays below the
/// Hub's 30 s RPC timeout and bounds standalone CLI execution.
const LAND_RECOVERY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(25);

/// Remaining time until an absolute deadline (saturates at zero).
fn remaining(deadline: Instant) -> std::time::Duration {
    deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(std::time::Duration::ZERO)
}

/// Run a git command through THE OWNER CONTRACT helper in `remuda-unix` (own
/// group, non-reaping exit observation, survivor cleanup while the leader is
/// unreaped, one latched absolute deadline over the reap and both drains). The
/// caller treats `TimedOut`/Err as Unknown.
fn owned_git(
    repo: &Path,
    args: &[&str],
    cap: std::time::Duration,
    deadline: Instant,
) -> std::io::Result<remuda_unix::OwnedRun> {
    let mut command = std::process::Command::new("git");
    command
        .current_dir(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    remuda_unix::run_owned_process(command, cap, deadline)
}

/// Whether the repository has a shallow boundary that could hide an ancestry
/// edge. Query errors are themselves Unknown and reported as shallow (so the
/// caller deepens or refuses rather than trusting exit 1).
fn repository_is_shallow(repo: &Path) -> bool {
    match git_output(repo, &["rev-parse", "--is-shallow-repository"]) {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim() == "true"
        }
        _ => true,
    }
}

/// The raw tri-state outcome of one `merge-base --is-ancestor` process, before
/// any shallow-history refinement (r16 item 3, R5).
enum RawAncestry {
    /// Exit 0: the edge is present.
    Ancestor,
    /// Exit 1: the edge is absent IN THE HISTORY THAT IS AVAILABLE LOCALLY.
    /// Only trustworthy once that history is complete (see [`is_ancestor`]).
    NotAncestor,
    /// Exit code other than 0/1, death by signal, or unreadable output: the
    /// edge is Unknown — never a boolean.
    Abnormal(String),
}

/// Map a finished git process to [`RawAncestry`], distinguishing exit 0,
/// exit 1 and every abnormal termination (r16 item 3).
fn classify_ancestry_output(output: &std::process::Output) -> RawAncestry {
    match output.status.code() {
        Some(0) => RawAncestry::Ancestor,
        Some(1) => RawAncestry::NotAncestor,
        other => RawAncestry::Abnormal(format!(
            "merge-base --is-ancestor ended abnormally ({other:?}): {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// Read `git merge-base --is-ancestor` as a tri-state (r16 item 3, R5) under
/// the r19 item 2 / R7 shallow rule:
/// * exit 0 → `Ok(true)` (ancestor ESTABLISHED);
/// * exit 1 → `Ok(false)` ONLY with COMPLETE (non-shallow) history. Gate
///   recovery NEVER deepens: a shallow edge may be a graft illusion, so the
///   answer is Unknown and the caller fails closed;
/// * any other exit, death by signal, or spawn failure → Unknown error, never
///   a false boolean.
///
/// This blocking form backs the verify-time preflight checks. Post-push
/// recovery goes through [`owned_is_ancestor`] under the absolute deadline.
fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let output = git_output(repo, &["merge-base", "--is-ancestor", ancestor, descendant])?;
    match classify_ancestry_output(&output) {
        RawAncestry::Ancestor => Ok(true),
        RawAncestry::Abnormal(message) => Err(anyhow::Error::msg(message)),
        RawAncestry::NotAncestor if !repository_is_shallow(repo) => Ok(false),
        RawAncestry::NotAncestor => Err(anyhow::Error::msg(format!(
            "{repo:?} is a shallow clone: ancestry of {ancestor} in {descendant} cannot be \
             decided locally without deepening, which gate recovery never does; push outcome \
             unknown"
        ))),
    }
}

/// One post-push recovery subprocess through the R6/R7 owner under ONE
/// absolute deadline set at recovery entry. A timeout (the owner killed the
/// child's whole group), a spawn failure, or — when required — a nonzero exit
/// is a [`MergeStop::PushUncertain`]: recovery never guesses.
fn recovery_git(
    repo: &Path,
    args: &[&str],
    what: &str,
    require_success: bool,
    deadline: Instant,
) -> Result<std::process::Output> {
    if remaining(deadline).is_zero() {
        return Err(MergeStop::PushUncertain(format!(
            "the {LAND_RECOVERY_DEADLINE:?} post-push recovery deadline elapsed before {what}; \
             the remote may already contain the merge"
        ))
        .into());
    }
    // R8(b): the per-command soft cap decides when a still-running git is
    // KILLed; the ABSOLUTE deadline bounds the reap and every pipe drain.
    let run = owned_git(repo, args, RECOVERY_GIT_CAP, deadline).map_err(|error| {
        MergeStop::PushUncertain(format!(
            "{what} could not run ({error}); push outcome unknown"
        ))
    })?;
    if run.timed_out() {
        return Err(MergeStop::PushUncertain(format!(
            "{what} exceeded the {RECOVERY_GIT_CAP:?} recovery bound and its group was killed; \
             push outcome unknown"
        ))
        .into());
    }
    let output = run.into_output();
    if require_success && !output.status.success() {
        return Err(MergeStop::PushUncertain(format!(
            "{what} failed ({}); push outcome unknown",
            output.status
        ))
        .into());
    }
    Ok(output)
}

/// Shallow check through the owned recovery helper; a failed read is Unknown
/// (PushUncertain), not a guessed boolean.
fn owned_repository_is_shallow(repo: &Path, deadline: Instant) -> Result<bool> {
    let output = recovery_git(
        repo,
        &["rev-parse", "--is-shallow-repository"],
        "post-push shallow check",
        true,
        deadline,
    )?;
    Ok(String::from_utf8_lossy(&output.stdout).trim() == "true")
}

/// Tri-state ancestry through the OWNED recovery helper under the absolute
/// deadline (r19 item 2 / R7). Shallow history, an abnormal termination or a
/// stalled read are all PushUncertain — never a false "absent".
fn owned_is_ancestor(
    repo: &Path,
    ancestor: &str,
    descendant: &str,
    deadline: Instant,
) -> Result<bool> {
    let output = recovery_git(
        repo,
        &["merge-base", "--is-ancestor", ancestor, descendant],
        "post-push ancestry read",
        false,
        deadline,
    )?;
    match classify_ancestry_output(&output) {
        RawAncestry::Ancestor => Ok(true),
        RawAncestry::Abnormal(message) => Err(MergeStop::PushUncertain(message).into()),
        RawAncestry::NotAncestor if !owned_repository_is_shallow(repo, deadline)? => Ok(false),
        RawAncestry::NotAncestor => Err(MergeStop::PushUncertain(format!(
            "{repo:?} is a shallow clone: ancestry of {ancestor} in {descendant} cannot be \
             decided locally without deepening, which gate recovery never does; push outcome \
             unknown"
        ))
        .into()),
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
    // Per-lane Hub ports: lane 1 keeps the resolved base, lane N shifts by 10.
    // The base is the explicit --e2e-port-base flag, else an inherited
    // HUB_E2E_LISTEN's port, else 58980.
    let shift = 10u16 * u16::try_from(args.e2e_lane.saturating_sub(1)).unwrap_or(0);
    let base_port = e2e_port_base(args);
    let hub_port = base_port.saturating_add(shift);
    // Lane 1 of a run without an explicit flag keeps whatever the operator's
    // environment already selected (web/upstream included); every other case
    // derives the pair from the lane's own block so queued lanes cannot share
    // the default 58881 upstream.
    let inherited_lane_env = shift == 0 && args.e2e_port_base.is_none();
    let hub_listen = if inherited_lane_env
        && let Ok(value) = std::env::var("HUB_E2E_LISTEN")
        && !value.is_empty()
    {
        value
    } else {
        format!("127.0.0.1:{hub_port}")
    };
    let web_port = if inherited_lane_env
        && let Ok(value) = std::env::var("HUB_E2E_WEB_PORT")
        && value.parse::<u16>().is_ok()
    {
        value
    } else {
        hub_port.saturating_add(9).to_string()
    };
    let upstream_port = if inherited_lane_env
        && let Ok(value) = std::env::var("HUB_E2E_UPSTREAM_LISTEN")
        && !value.is_empty()
    {
        value
    } else {
        format!("127.0.0.1:{}", hub_port.saturating_add(1))
    };
    command
        .env("HUB_E2E_LISTEN", hub_listen)
        .env("HUB_E2E_WEB_PORT", web_port)
        .env("HUB_E2E_UPSTREAM_LISTEN", upstream_port);
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
                "no-host-wide-kills",
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
        assert!(plan[7..].iter().all(|step| step.status == "skipped"));
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
        assert!(web_only[..7].iter().all(|step| step.status == "skipped"));
        assert!(web_only[7..10].iter().all(|step| step.status == "planned"));
        assert_eq!(web_only[10].status, "skipped");
    }

    // ── r16 item 3: merge-base exit tri-state + shallow history ────────────

    fn tgit(repo: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .current_dir(repo)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap()
    }

    fn tgit_ok(repo: &Path, args: &[&str]) -> String {
        let output = tgit(repo, args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// Build: full working repo with 5 main commits + a branch off main~2
    /// merged BACK into main, a bare origin carrying both refs, and return
    /// (temp keepalive, origin path, branch-tip sha, main-tip sha).
    fn ancestry_fixture() -> (tempfile::TempDir, PathBuf, String, String) {
        let keep = tempfile::tempdir().unwrap();
        let root = keep.path();
        let full = root.join("full");
        let origin = root.join("origin.git");
        std::fs::create_dir_all(&full).unwrap();
        tgit_ok(&full, &["init", "-q", "-b", "main"]);
        tgit_ok(&full, &["config", "user.email", "t@example.com"]);
        tgit_ok(&full, &["config", "user.name", "T"]);
        for index in 1..=5 {
            std::fs::write(full.join("f"), format!("{index}\n")).unwrap();
            tgit_ok(&full, &["add", "f"]);
            tgit_ok(&full, &["commit", "-q", "-m", &format!("c{index}")]);
        }
        tgit_ok(&full, &["branch", "topic", "main~2"]);
        tgit_ok(&full, &["checkout", "-q", "topic"]);
        std::fs::write(full.join("branch.txt"), "topic\n").unwrap();
        tgit_ok(&full, &["add", "branch.txt"]);
        tgit_ok(&full, &["commit", "-q", "-m", "topic work"]);
        let topic = tgit_ok(&full, &["rev-parse", "topic"]);
        tgit_ok(&full, &["checkout", "-q", "main"]);
        tgit_ok(
            &full,
            &["merge", "-q", "--no-ff", "-m", "merge topic", "topic"],
        );
        let main = tgit_ok(&full, &["rev-parse", "main"]);
        let _ = std::process::Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main"])
            .arg(&origin)
            .status()
            .unwrap();
        tgit_ok(
            &full,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        tgit_ok(&full, &["push", "-q", "origin", "main", "topic"]);
        (keep, origin, topic, main)
    }

    fn shallow_clone(keep: &Path, name: &str, from: &Path, depth: &str) -> PathBuf {
        let target = keep.join(name);
        // file:// forces the smart transport: a plain local-path clone
        // hardlinks objects and ignores --depth.
        let url = format!("file://{}", from.display());
        let output = tgit(
            keep,
            &[
                "clone",
                "-q",
                "--depth",
                depth,
                "--no-single-branch",
                &url,
                target.to_str().unwrap(),
            ],
        );
        assert!(
            output.status.success(),
            "shallow clone: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        target
    }

    /// Exit 0 and exit 1 map to booleans on complete history; a signal death
    /// (raw wait status 9 = SIGKILL) and an exit 2 are Unknown errors, never a
    /// false boolean.
    #[test]
    fn ancestry_exit_codes_distinguish_zero_one_and_abnormal() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::{ExitStatus, Output};
        fn output_from(raw: i32) -> Output {
            Output {
                status: ExitStatus::from_raw(raw),
                stdout: Vec::new(),
                stderr: b"boom".to_vec(),
            }
        }
        assert!(matches!(
            classify_ancestry_output(&output_from(0)),
            RawAncestry::Ancestor
        ));
        assert!(matches!(
            classify_ancestry_output(&output_from(1 << 8)),
            RawAncestry::NotAncestor
        ));
        assert!(matches!(
            classify_ancestry_output(&output_from(2 << 8)),
            RawAncestry::Abnormal(_)
        ));
        // Killed by SIGKILL: no exit code → Abnormal/Unknown.
        assert!(matches!(
            classify_ancestry_output(&output_from(9)),
            RawAncestry::Abnormal(_)
        ));
    }

    /// r19 item 2 / R6/R7: a recovery git past its deadline is SIGKILLed WITH
    /// ITS WHOLE GROUP (leader + a same-group grandchild) while the leader is
    /// unreaped, then reaped once, returning TimedOut (not an error).
    #[test]
    fn an_owned_git_past_its_deadline_kills_the_whole_group() {
        let keep = tempfile::tempdir().unwrap();
        let git = keep.path().join("git");
        std::fs::write(&git, "#!/bin/sh\n(sleep 47.73 &)\nexec sleep 47.73\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut command = std::process::Command::new(git.to_str().unwrap());
        command
            .current_dir(keep.path())
            .args(["fetch", "-q", "origin"]);
        let start = std::time::Instant::now();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let run = remuda_unix::run_owned_process(
            command,
            std::time::Duration::from_millis(300),
            deadline,
        )
        .expect("an owned run returns a verdict");
        use std::os::unix::process::ExitStatusExt;
        let output = match run {
            remuda_unix::OwnedRun::TimedOut(output) => output,
            other => panic!("expected TimedOut, got {other:?}"),
        };
        assert_eq!(
            output.status.signal(),
            Some(9),
            "SIGKILLed: {:?}",
            output.status
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "the bound must apply promptly"
        );
    }

    /// r20 item 5 / R8(b): the git leader EXITS UNSUCCESSFULLY FIRST (the ssh
    /// wrapper backgrounds its transport and exits) while a same-group
    /// grandchild keeps a pipe open. The owner observes the exit without
    /// reaping, kills the survivor while the unreaped leader pins the pgid,
    /// reaps the leader and returns its exit code; no `sleep` lingers. The old
    /// owner reaped the leader with try_wait on the normal arm and never
    /// signalled, so the descendant and the blocked reader survived.
    #[test]
    fn an_owned_git_whose_leader_exits_first_kills_its_surviving_transport() {
        let keep = tempfile::tempdir().unwrap();
        let git = keep.path().join("git-transport");
        // Backgrounded transport in the SAME group: stdout detached to
        // /dev/null, stderr INHERITED (holds the captured pipe); leader exits.
        std::fs::write(&git, "#!/bin/sh\n(sleep 47.66 >/dev/null) &\nexit 42\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
        let command = std::process::Command::new(git.to_str().unwrap());
        let start = std::time::Instant::now();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let run =
            remuda_unix::run_owned_process(command, std::time::Duration::from_secs(2), deadline)
                .expect("an owned run returns a verdict");
        let output = match run {
            remuda_unix::OwnedRun::Completed(output) => output,
            other => panic!("expected Completed after survivor cleanup, got {other:?}"),
        };
        assert_eq!(
            output.status.code(),
            Some(42),
            "the git leader's own exit status drives the result"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(4),
            "cleanup is bounded, not the grandchild's sleep: {:?}",
            start.elapsed()
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let left = live_marker_processes("sleep 47.66");
            if left == 0 || std::time::Instant::now() >= deadline {
                assert_eq!(
                    left, 0,
                    "the surviving transport grandchild outlived the owner"
                );
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    // r21 item 7: the blocked-pipe/deadline regression no longer lives here.
    // The old test depended on Linux /proc enumeration timing (a TERM-ignoring
    // descendant consuming the 400 ms deadline); with no /proc the owner
    // signals and reaps immediately and returns Completed, so the mandatory
    // TimedOut assertion failed under ordinary macOS scheduling. The
    // platform-independent coverage is the injected post-exit-observation
    // seam test in remuda-unix
    // (`owner::tests::completion_after_the_deadline_is_latched_unknown`),
    // which holds the whole cleanup/reap/drain sequence beyond the absolute
    // deadline on ANY platform and asserts the latched Unknown. This binary
    // keeps the portable owner-sequence tests above (soft-cap kill, normal
    // exit with survivor cleanup).

    /// r19 item 2 / R7: a depth-1 clone grafts both tips as roots, so
    /// merge-base's exit 1 ("not ancestor") cannot be trusted. Gate recovery
    /// NEVER deepens: the edge is Unknown (an Err) and the clone stays exactly
    /// as shallow as it was -- no `git fetch --deepen` is attempted.
    #[test]
    fn shallow_ancestry_is_unknown_and_never_deepens() {
        let (keep, origin, topic, _main) = ancestry_fixture();
        let client = shallow_clone(keep.path(), "client", &origin, "1");
        assert_eq!(
            tgit_ok(&client, &["rev-parse", "--is-shallow-repository"]),
            "true"
        );
        let error = is_ancestor(&client, &topic, "origin/main")
            .expect_err("a shallow edge is Unknown without deepening");
        assert!(
            error.to_string().contains("shallow clone"),
            "the error names the shallow condition: {error}"
        );
        assert_eq!(
            tgit_ok(&client, &["rev-parse", "--is-shallow-repository"]),
            "true",
            "R7 recovery leaves the clone shallow"
        );
    }

    /// r19 item 2 / R7 through the PRODUCTION post-push recovery path: the push
    /// fails (receive-pack refuses), then the recovery FETCH stalls in its
    /// transport. The R6 owner kills the fetch group at the per-command cap,
    /// push_main_cas returns PushUncertain well before the 30 s RPC timeout,
    /// and no stalled transport process survives.
    #[test]
    fn a_stalled_post_push_recovery_fetch_is_uncertain_and_leaves_no_process() {
        let (keep, _origin, _topic, main) = ancestry_fixture();
        let repo = keep.path().join("full");
        // Fake transport: a receive-pack invocation refuses (push fails
        // immediately); an upload-pack invocation never returns (fetch stalls).
        let fake_ssh = keep.path().join("fake-ssh-r19.sh");
        // git probes resolved connection settings with `ssh -G host` first;
        // answer that with a minimal config so it proceeds to the real call.
        std::fs::write(
            &fake_ssh,
            "#!/bin/sh\ncase \"$1\" in\n  -G) printf 'hostname %s\\nuser git\\nport 22\\n' \"$2\"; exit 0 ;;\nesac\ncase \" $*\" in\n  *git-receive-pack*) exit 1 ;;\n  *) exec sleep 47.61 ;;\nesac\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        tgit_ok(
            &repo,
            &["remote", "set-url", "origin", "ssh://example.test/repo.git"],
        );
        tgit_ok(
            &repo,
            &["config", "core.sshCommand", fake_ssh.to_str().unwrap()],
        );

        let started = std::time::Instant::now();
        let error = push_main_cas(&repo, &main, &main)
            .expect_err("a stalled recovery fetch is PushUncertain");
        assert!(
            matches!(
                error.downcast_ref::<MergeStop>(),
                Some(MergeStop::PushUncertain(_))
            ),
            "PushUncertain, got {error:#}"
        );
        assert!(
            error.to_string().contains("ls-remote"),
            "the uncertainty names the bounded remote read: {error}"
        );
        assert!(
            error.to_string().contains("recovery bound"),
            "the uncertainty names the bounded kill: {error}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(13),
            "the recovery must fail closed at the cap, not hang: {:?}",
            started.elapsed()
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let left = live_marker_processes("sleep 47.61");
            if left == 0 || std::time::Instant::now() >= deadline {
                assert_eq!(
                    left, 0,
                    "the stalled fetch transport survived the owner group kill"
                );
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Build a working repo + bare origin on base B with a granted merge M
    /// (descendant of B) and a divergent remote commit R also based on B.
    /// Returns (keep, full, B, M, R).
    fn divergence_fixture() -> (tempfile::TempDir, PathBuf, String, String, String) {
        let keep = tempfile::tempdir().unwrap();
        let root = keep.path();
        let full = root.join("full");
        let origin = root.join("origin.git");
        std::fs::create_dir_all(&full).unwrap();
        tgit_ok(&full, &["init", "-q", "-b", "main"]);
        tgit_ok(&full, &["config", "user.email", "t@example.com"]);
        tgit_ok(&full, &["config", "user.name", "T"]);
        std::fs::write(full.join("f"), "b\n").unwrap();
        tgit_ok(&full, &["add", "f"]);
        tgit_ok(&full, &["commit", "-q", "-m", "B"]);
        let base = tgit_ok(&full, &["rev-parse", "main"]);
        tgit_ok(&full, &["branch", "rline"]);
        std::fs::write(full.join("m"), "m\n").unwrap();
        tgit_ok(&full, &["add", "m"]);
        tgit_ok(&full, &["commit", "-q", "-m", "M"]);
        let merge = tgit_ok(&full, &["rev-parse", "main"]);
        // Divergent R on B, published as remote main (M then cannot ff).
        tgit_ok(&full, &["checkout", "-q", "rline"]);
        std::fs::write(full.join("r"), "r\n").unwrap();
        tgit_ok(&full, &["add", "r"]);
        tgit_ok(&full, &["commit", "-q", "-m", "R"]);
        let remote_r = tgit_ok(&full, &["rev-parse", "rline"]);
        let output = std::process::Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main"])
            .current_dir(root)
            .arg(&origin)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "bare init: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        tgit_ok(
            &full,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        tgit_ok(&full, &["push", "-q", "origin", "rline:main"]);
        tgit_ok(&full, &["checkout", "-q", "main"]);
        (keep, full, base, merge, remote_r)
    }

    /// The FULL for-each-ref listing with object IDs — R10 requires it to be
    /// byte-identical across a recovery.
    fn refs_listing(repo: &Path) -> String {
        tgit_ok(repo, &["for-each-ref", "--format=%(refname) %(objectname)"])
    }

    /// R10: ls-remote parsing accepts exactly one well-formed line for the
    /// requested ref and rejects everything else.
    #[test]
    fn ls_remote_base_parsing() {
        let sha1 = "0123456789abcdef0123456789abcdef01234567";
        let sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(
            parse_ls_remote_base(format!("{sha1}\trefs/heads/main\n").as_bytes(), "main").unwrap(),
            sha1
        );
        assert_eq!(
            parse_ls_remote_base(format!("{sha256}\trefs/heads/x\n").as_bytes(), "x").unwrap(),
            sha256
        );
        assert!(parse_ls_remote_base(b"", "main").is_err());
        assert!(
            parse_ls_remote_base(format!("{sha1}\trefs/heads/other\n").as_bytes(), "main").is_err()
        );
        assert!(
            parse_ls_remote_base(
                format!("{sha1}\trefs/heads/main\n{sha1}\trefs/heads/main\n").as_bytes(),
                "main"
            )
            .is_err()
        );
        assert!(parse_ls_remote_base(b"garbage\n", "main").is_err());
        assert!(parse_ls_remote_base(b"zz\trefs/heads/main\n", "main").is_err());
        assert!(
            parse_ls_remote_base(
                format!("{}\trefs/heads/main\n", &sha1[..39]).as_bytes(),
                "main"
            )
            .is_err()
        );
        assert_eq!(
            parse_ls_remote_base(
                format!("{sha1}\trefs/heads/main\n{sha1}\trefs/tags/v1^{{}}\n").as_bytes(),
                "main"
            )
            .unwrap(),
            sha1
        );
    }

    /// R10: two overlapping post-push recoveries never swap snapshots — each
    /// reads the authoritative advertised tip with ls-remote: B (D containing
    /// M) certifies Landed; A (R, before M landed) classifies BaseMoved.
    #[test]
    fn overlapping_cli_recoveries_read_distinct_tips_without_writing_refs() {
        use super::recovery_read_seam as seam;
        let (keep, full, base_b, merge_m, remote_r) = divergence_fixture();
        let spawn_recovery = || {
            let repo = full.clone();
            let merged = merge_m.clone();
            let expected = base_b.clone();
            std::thread::spawn(move || push_main_cas(&repo, &merged, &expected))
        };

        seam::arm(&full, 2);
        let a = spawn_recovery();
        seam::wait_arrived(&full, 0);
        // A read R. Land M on the remote for real: merge R into the M-based
        // main, producing D which contains both, and publish it.
        tgit_ok(&full, &["fetch", "-q", "origin"]);
        tgit_ok(&full, &["merge", "-q", "--no-ff", "-m", "D", "origin/main"]);
        tgit_ok(&full, &["push", "-q", "origin", "main"]);
        let before = refs_listing(&full);

        let b = spawn_recovery();
        seam::wait_arrived(&full, 1);
        seam::release(&full, 1);
        let result_b = b.join().expect("recovery B joins");
        seam::release(&full, 0);
        let result_a = a.join().expect("recovery A joins");
        seam::disarm();

        assert!(
            result_b.is_ok(),
            "B read D containing M — Landed must win: {:?}",
            result_b
                .as_ref()
                .err()
                .map(std::string::ToString::to_string)
        );
        let err_a = result_a.expect_err("A read R, which does not contain M");
        assert!(
            matches!(err_a.downcast_ref::<MergeStop>(), Some(MergeStop::BaseMoved(_, tip)) if tip == &remote_r),
            "A must classify against its OWN read R: {err_a:#}"
        );
        assert_eq!(
            refs_listing(&full),
            before,
            "either recovery created, updated or deleted no ref"
        );
        drop(keep);
    }

    /// R10 r31 item 1: configured SHA-source remote.origin.fetch mappings and
    /// fetch.prune must not write or prune refs during the destination-less
    /// recovery fetch (empty --refmap + --no-prune). Destination absent,
    /// destination existing, and stale tracking ref under prune=true.
    #[test]
    fn configured_fetch_mappings_and_prune_write_no_refs_during_recovery() {
        let (keep, full, base_b, merge_m, _remote_r) = divergence_fixture();
        // Put M on remote main so recovery certifies Landed: merge the
        // divergent R back into M, publish D.
        tgit_ok(&full, &["fetch", "-q", "origin"]);
        tgit_ok(&full, &["merge", "-q", "--no-ff", "-m", "D", "origin/main"]);
        tgit_ok(&full, &["push", "-q", "origin", "main"]);

        // (a) destination ABSENT
        tgit_ok(
            &full,
            &[
                "config",
                "--add",
                "remote.origin.fetch",
                &format!("+{merge_m}:refs/tags/keep"),
            ],
        );
        let before = refs_listing(&full);
        push_main_cas(&full, &merge_m, &base_b).expect("M is contained in remote main: Landed");
        assert_eq!(
            refs_listing(&full),
            before,
            "absent destination must not be created"
        );
        assert!(
            tgit(
                &full,
                &["rev-parse", "--verify", "--quiet", "refs/tags/keep"]
            )
            .status
            .code()
                != Some(0)
        );

        // (b) destination EXISTING at B
        tgit_ok(&full, &["tag", "keep", &base_b]);
        let before = refs_listing(&full);
        push_main_cas(&full, &merge_m, &base_b).expect("Landed");
        assert_eq!(refs_listing(&full), before);
        assert_eq!(tgit_ok(&full, &["rev-parse", "refs/tags/keep"]), base_b);

        // (c) fetch.prune=true with a stale tracking ref
        tgit_ok(&full, &["config", "fetch.prune", "true"]);
        tgit_ok(&full, &["update-ref", "refs/remotes/origin/gone", &base_b]);
        let before = refs_listing(&full);
        push_main_cas(&full, &merge_m, &base_b).expect("Landed");
        assert_eq!(
            refs_listing(&full),
            before,
            "stale tracking ref must survive"
        );
        drop(keep);
    }

    /// R10: a configured tagOpt=--tags with a clobbered local tag used to
    /// abort recovery fetch; ref-free recovery uses --no-tags and still
    /// certifies Landed, leaving every ref byte-identical.
    #[test]
    fn a_clobbered_tag_with_tagopt_does_not_break_ref_free_recovery() {
        let (keep, full, base_b, merge_m, _remote_r) = divergence_fixture();
        // Remote main already contains M in this fixture (R and M are both
        // children of B; the published main is R — merge M into it remotely).
        tgit_ok(&full, &["fetch", "-q", "origin"]);
        tgit_ok(&full, &["merge", "-q", "--no-ff", "-m", "D", "origin/main"]);
        tgit_ok(&full, &["push", "-q", "origin", "main"]);
        // Clobber: local tag at B, remote tag moved to an orphan after clone.
        tgit_ok(&full, &["tag", "v1", &base_b]);
        let orphan = tgit_ok(&full, &["commit-tree", "HEAD^{tree}", "-m", "sideways"]);
        tgit_ok(
            &full,
            &[
                "push",
                "-q",
                "-f",
                "origin",
                &format!("{orphan}:refs/tags/v1"),
            ],
        );
        tgit_ok(&full, &["config", "remote.origin.tagOpt", "--tags"]);
        // Sanity: a plain fetch honoring tagOpt still fails on the clobber.
        assert!(!tgit(&full, &["fetch", "-q", "origin"]).status.success());
        let before = refs_listing(&full);

        push_main_cas(&full, &merge_m, &base_b)
            .expect("--no-tags bypasses the clobber; M is contained in remote main");
        assert_eq!(
            refs_listing(&full),
            before,
            "recovery created, updated or deleted no ref"
        );
        assert_eq!(
            tgit_ok(&full, &["rev-parse", "refs/tags/v1"]),
            base_b,
            "the clobbered local tag is untouched"
        );
        drop(keep);
    }

    /// R10: a recovery whose base is not advertised (or whose origin is
    /// unreachable) is PushUncertain and writes no ref on either path.
    #[test]
    fn a_failed_ref_free_recovery_writes_no_refs() {
        let (keep, _origin, _topic, main) = ancestry_fixture();
        let repo = keep.path().join("full");
        let merged = main.clone();
        let expected = tgit_ok(&repo, &["rev-parse", &format!("{main}^1")]);
        let before = refs_listing(&repo);

        tgit_ok(
            &repo,
            &[
                "remote",
                "set-url",
                "origin",
                keep.path().join("no-such-origin-r30.git").to_str().unwrap(),
            ],
        );
        let error = push_main_cas(&repo, &merged, &expected)
            .expect_err("an unreachable remote stays uncertain");
        assert!(
            matches!(
                error.downcast_ref::<MergeStop>(),
                Some(MergeStop::PushUncertain(_))
            ),
            "PushUncertain, got {error:#}"
        );
        assert_eq!(
            refs_listing(&repo),
            before,
            "the failed recovery created, updated or deleted no ref"
        );
        drop(keep);
    }

    /// r30 item 2: exact unsupported-SHA-256 diagnostics, as a pure
    /// classifier over controlled strings (independent of installed git).
    #[test]
    fn sha256_unsupported_diagnostic_classifier_is_precise() {
        for stderr in [
            "fatal: unknown hash algorithm 'sha256'\n",
            "fatal: attempt to use sha256 has not been compiled in\n",
            "error: unknown option `object-format'\nusage: git init [..]",
            "error: unknown option '--object-format=sha256'\n",
            "fatal: unknown value for --object-format: sha256\n",
        ] {
            assert!(
                is_unsupported_sha256_diagnostic(stderr),
                "unsupported form must be recognized: {stderr:?}"
            );
        }
        for stderr in [
            // A path containing both trigger words must NOT match.
            "fatal: bad config line 3 in file /home/dev/invalid-sha256-config\n",
            "error: invalid key: foo.sha256.invalid in /etc/gitconfig\n",
            "fatal: unknown hash algorithm 'bogus'\n",
            "",
        ] {
            assert!(
                !is_unsupported_sha256_diagnostic(stderr),
                "unrelated diagnostic must not skip: {stderr:?}"
            );
        }
    }

    /// r30 item 2: a spawn failure of the probe program is a hard failure,
    /// never a silent skip (uses a nonexistent executable path).
    #[test]
    fn a_sha256_probe_spawn_failure_panics() {
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir
            .path()
            .join("no-such-bin-dir")
            .join("git")
            .to_string_lossy()
            .into_owned();
        let target = dir.path().join("p");
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            probe_sha256_support_with(&bogus, &target)
        }))
        .is_err();
        assert!(panicked, "a spawn failure must panic, not skip");
    }

    /// R10: post-push recovery in a SHA-256 repository reads the native
    /// 64-hex advertised tip, fetches objects without writing a ref, and
    /// recognizes a merge already contained in remote main.
    #[test]
    fn recovery_in_a_sha256_repo_reads_a_64_hex_tip_without_refs() {
        let probe = tempfile::tempdir().unwrap();
        let supported = probe_sha256_support("git", &probe.path().join("p"));
        drop(probe);
        if !supported {
            return;
        }

        let keep = tempfile::tempdir().unwrap();
        let full = keep.path().join("full");
        let origin = keep.path().join("origin.git");
        std::fs::create_dir_all(&full).unwrap();
        tgit_ok(
            &full,
            &["init", "-q", "--object-format=sha256", "-b", "main"],
        );
        tgit_ok(&full, &["config", "user.email", "t@example.com"]);
        tgit_ok(&full, &["config", "user.name", "T"]);
        std::fs::write(full.join("f"), "b\n").unwrap();
        tgit_ok(&full, &["add", "f"]);
        tgit_ok(&full, &["commit", "-q", "-m", "B"]);
        let base = tgit_ok(&full, &["rev-parse", "main"]);
        assert_eq!(base.len(), 64, "fixture must be SHA-256: {base}");
        tgit_ok(&full, &["branch", "topic"]);
        std::fs::write(full.join("m"), "m\n").unwrap();
        tgit_ok(&full, &["add", "m"]);
        tgit_ok(&full, &["commit", "-q", "-m", "M"]);
        let merged = tgit_ok(&full, &["rev-parse", "main"]);
        // Remote advances to D, containing M.
        std::fs::write(full.join("d"), "d\n").unwrap();
        tgit_ok(&full, &["add", "d"]);
        tgit_ok(&full, &["commit", "-q", "-m", "D"]);
        let output = std::process::Command::new("git")
            .args([
                "init",
                "-q",
                "--bare",
                "--object-format=sha256",
                "-b",
                "main",
            ])
            .current_dir(keep.path())
            .arg(&origin)
            .output()
            .unwrap();
        assert!(output.status.success());
        tgit_ok(
            &full,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        tgit_ok(&full, &["push", "-q", "origin", "main", "topic"]);
        // Stale post-crash checkout.
        tgit_ok(&full, &["update-ref", "refs/remotes/origin/main", &base]);
        tgit_ok(
            &full,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/topic:refs/remotes/origin/topic",
            ],
        );
        let before = refs_listing(&full);

        push_main_cas(&full, &merged, &base)
            .expect("a 64-hex advertised tip must not be rejected; M landed");
        assert_eq!(
            refs_listing(&full),
            before,
            "the sha256 recovery created, updated or deleted no ref"
        );
        assert_eq!(
            tgit_ok(&full, &["rev-parse", "refs/remotes/origin/main"]),
            base,
            "the configured tracking ref is never touched"
        );
    }

    /// R5 fail-open: a restricted remote.origin.fetch leaves the configured
    /// tracking ref stale, but recovery reads the advertised tip DIRECTLY and
    /// certifies Landed without touching any ref; an unreachable origin is
    /// PushUncertain, likewise ref-free.
    #[test]
    fn recovery_reads_the_advertised_base_past_a_restricted_refspec() {
        let (keep, _origin, _topic, main) = ancestry_fixture();
        let repo = keep.path().join("full");
        let merged = main.clone();
        let expected = tgit_ok(&repo, &["rev-parse", &format!("{main}^1")]);
        tgit_ok(&repo, &["checkout", "-q", "main"]);
        std::fs::write(repo.join("post-land.txt"), "remote moved\n").unwrap();
        tgit_ok(&repo, &["add", "post-land.txt"]);
        tgit_ok(&repo, &["commit", "-q", "-m", "later remote commit D"]);
        tgit_ok(&repo, &["push", "-q", "origin", "main"]);
        assert_ne!(tgit_ok(&repo, &["rev-parse", "main"]), merged);
        tgit_ok(
            &repo,
            &["update-ref", "refs/remotes/origin/main", &expected],
        );
        tgit_ok(
            &repo,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/topic:refs/remotes/origin/topic",
            ],
        );
        tgit_ok(&repo, &["fetch", "-q", "origin"]);
        assert_eq!(
            tgit_ok(&repo, &["rev-parse", "refs/remotes/origin/main"]),
            expected,
            "the restricted fetch leaves origin/main stale"
        );
        let before = refs_listing(&repo);

        push_main_cas(&repo, &merged, &expected).expect(
            "a merge already contained in remote main is Landed despite a stale tracking ref",
        );
        assert_eq!(
            refs_listing(&repo),
            before,
            "a successful recovery creates, updates or deletes no ref"
        );
        assert_eq!(
            tgit_ok(&repo, &["rev-parse", "refs/remotes/origin/main"]),
            expected,
            "the configured tracking ref is never touched"
        );

        tgit_ok(
            &repo,
            &[
                "remote",
                "set-url",
                "origin",
                keep.path().join("no-such-origin-r30.git").to_str().unwrap(),
            ],
        );
        let error = push_main_cas(&repo, &merged, &expected)
            .expect_err("an unreachable recovery stays uncertain");
        assert!(
            matches!(
                error.downcast_ref::<MergeStop>(),
                Some(MergeStop::PushUncertain(_))
            ),
            "PushUncertain, got {error:#}"
        );
        assert_eq!(
            refs_listing(&repo),
            before,
            "a failed recovery creates, updates or deletes no ref"
        );
    }

    /// Count LIVE (non-zombie) processes whose /proc/<pid>/cmdline carries the
    /// marker -- r19 item 2 leftover-process assertion.
    fn live_marker_processes(marker: &str) -> usize {
        std::fs::read_dir("/proc")
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
                    .filter_map(|name| name.parse::<i32>().ok())
                    .filter(|pid| {
                        let base = std::path::Path::new("/proc").join(pid.to_string());
                        let Ok(cmd) = std::fs::read_to_string(base.join("cmdline")) else {
                            return false;
                        };
                        // /proc cmdline separates argv with NUL bytes.
                        if !cmd.replace('\0', " ").contains(marker) {
                            return false;
                        }
                        if let Ok(stat) = std::fs::read_to_string(base.join("stat"))
                            && let Some(after) = stat.rsplit_once(')').map(|(_, r)| r)
                        {
                            let state = after.split_whitespace().next().unwrap_or("");
                            return state != "Z";
                        }
                        true
                    })
                    .count()
            })
            .unwrap_or(0)
    }
}
