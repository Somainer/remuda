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
    let remote = "refs/remotes/origin/main";
    let deadline = Instant::now() + LAND_RECOVERY_DEADLINE;
    recovery_git(
        repo,
        &["fetch", "-q", "origin"],
        "post-push recovery fetch",
        true,
        deadline,
    )?;
    // The exact granted merge IS on remote main: the push landed, only the
    // acknowledgement failed. Treat as success — never failed/base-moved.
    if owned_is_ancestor(repo, merged, remote, deadline)? {
        return Ok(());
    }
    let tip_output = recovery_git(
        repo,
        &["rev-parse", remote],
        "post-push remote tip read",
        true,
        deadline,
    )?;
    let remote_tip = String::from_utf8_lossy(&tip_output.stdout)
        .trim()
        .to_owned();
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

/// What an owned subprocess did relative to its deadline (R6 single owner).
#[derive(Debug)]
enum OwnedRun {
    Completed(std::process::Output),
    TimedOut(std::process::Output),
}
impl OwnedRun {
    fn timed_out(&self) -> bool {
        matches!(self, OwnedRun::TimedOut(_))
    }
    fn into_output(self) -> std::process::Output {
        match self {
            OwnedRun::Completed(o) | OwnedRun::TimedOut(o) => o,
        }
    }
}

const OWNED_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);
const OWNED_KILL_GRACE: std::time::Duration = std::time::Duration::from_millis(500);
/// Bound on joining a pipe reader after exit/group-kill before abandoning it
/// (R7 — an inherited transport pipe is never joined unboundedly).
const OWNED_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// Drain one child pipe to EOF on a reader thread that never reaps the leader
/// (R6 — the owner is the sole reaper).
fn drain_pipe<R: std::io::Read + Send + 'static>(
    pipe: Option<R>,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    })
}

/// Bound a draining reader's join: its bytes once EOF arrives, else ABANDON the
/// thread (dropping the `JoinHandle` detaches it) after `grace`. R7 — a
/// surviving ssh grandchild that inherited a pipe must never pin recovery
/// forever; the owner killed the child's group, so the last fd closes on its
/// own and the abandoned thread then finishes.
fn join_drained(handle: std::thread::JoinHandle<Vec<u8>>, grace: std::time::Duration) -> Vec<u8> {
    let deadline = std::time::Instant::now() + grace;
    loop {
        if handle.is_finished() {
            return handle.join().unwrap_or_default();
        }
        if std::time::Instant::now() >= deadline {
            drop(handle);
            return Vec::new();
        }
        std::thread::sleep(OWNED_POLL_INTERVAL);
    }
}

/// R6/R7 single-owner runner (identical ownership rules to the Node's helper):
/// the calling thread polls try_wait (no second reaper), reader threads drain
/// pipes without reaping, the child runs in its OWN group (read-only recovery
/// probes — the granted land's actual push stays in the Node-tracked land
/// group), and on deadline the owner SIGKILLs the whole group WHILE THE LEADER
/// IS UNREAPED, reaps once, and bounds the reader joins.
fn run_owned_process(
    mut command: std::process::Command,
    deadline: std::time::Duration,
) -> std::io::Result<OwnedRun> {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn()?;
    let leader_pid = child.id() as i32;
    let out_handle = drain_pipe(child.stdout.take());
    let err_handle = drain_pipe(child.stderr.take());
    let started = std::time::Instant::now();
    loop {
        match child.try_wait()? {
            Some(status) => {
                return Ok(OwnedRun::Completed(std::process::Output {
                    status,
                    stdout: join_drained(out_handle, OWNED_DRAIN_GRACE),
                    stderr: join_drained(err_handle, OWNED_DRAIN_GRACE),
                }));
            }
            None if started.elapsed() >= deadline => {
                use nix::sys::signal::{Signal, killpg};
                use nix::unistd::Pid;
                let _ = killpg(Pid::from_raw(leader_pid), Some(Signal::SIGKILL));
                let kill_at = std::time::Instant::now();
                let mut status = None;
                loop {
                    if let Some(exit) = child.try_wait()? {
                        status = Some(exit);
                        if !group_exists(leader_pid) {
                            break;
                        }
                    }
                    if kill_at.elapsed() >= OWNED_KILL_GRACE {
                        if status.is_none()
                            && let Ok(exit) = child.wait()
                        {
                            status = Some(exit);
                        }
                        break;
                    }
                    std::thread::sleep(OWNED_POLL_INTERVAL);
                }
                let Some(status) = status else {
                    return Err(std::io::Error::other(
                        "owned process killed at deadline but could not be reaped",
                    ));
                };
                return Ok(OwnedRun::TimedOut(std::process::Output {
                    status,
                    stdout: join_drained(out_handle, OWNED_DRAIN_GRACE),
                    stderr: join_drained(err_handle, OWNED_DRAIN_GRACE),
                }));
            }
            None => std::thread::sleep(OWNED_POLL_INTERVAL),
        }
    }
}

/// `killpg(pgid,0)` == ESRCH (group gone), the kernel's authoritative answer.
fn group_exists(pgid: i32) -> bool {
    use nix::sys::signal::killpg;
    use nix::unistd::Pid;
    killpg(Pid::from_raw(pgid), None).is_ok()
}

/// Remaining time until an absolute deadline (saturates at zero).
fn remaining(deadline: std::time::Instant) -> std::time::Duration {
    deadline
        .checked_duration_since(std::time::Instant::now())
        .unwrap_or(std::time::Duration::ZERO)
}

/// Run a git command through the R6/R7 owner (own group, whole-group kill,
/// bounded drains). The caller treats TimedOut as Unknown.
fn owned_git(
    repo: &Path,
    args: &[&str],
    deadline: std::time::Duration,
) -> std::io::Result<OwnedRun> {
    let mut command = std::process::Command::new("git");
    command
        .current_dir(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    run_owned_process(command, deadline)
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
    let budget = RECOVERY_GIT_CAP.min(remaining(deadline));
    if budget.is_zero() {
        return Err(MergeStop::PushUncertain(format!(
            "the {LAND_RECOVERY_DEADLINE:?} post-push recovery deadline elapsed before {what}; \
             the remote may already contain the merge"
        ))
        .into());
    }
    let run = owned_git(repo, args, budget).map_err(|error| {
        MergeStop::PushUncertain(format!(
            "{what} could not run ({error}); push outcome unknown"
        ))
    })?;
    if run.timed_out() {
        return Err(MergeStop::PushUncertain(format!(
            "{what} exceeded the {budget:?} recovery bound and its group was killed; push \
             outcome unknown"
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
        let run = run_owned_process(command, std::time::Duration::from_millis(300))
            .expect("an owned run returns a verdict");
        use std::os::unix::process::ExitStatusExt;
        let output = match run {
            OwnedRun::TimedOut(output) => output,
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
            error.to_string().contains("recovery fetch"),
            "the uncertainty names the recovery fetch: {error}"
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
                        if !cmd.contains(marker) {
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
