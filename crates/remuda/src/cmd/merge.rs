//! Coordinator merge: verify an immutable merge, then compare-and-swap main.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use clap::Args;
use serde::{Deserialize, Serialize};

#[derive(Debug, Args, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MergeArgs {
    /// Local branch to merge into main (the committed snapshot is pinned).
    pub branch: String,
    /// Run the full gate before advancing main.
    #[arg(long, required_unless_present = "dry_run")]
    #[serde(default)]
    pub gate: bool,
    /// Inspect local refs and print the plan; do not fetch, merge, or run checks.
    #[arg(long)]
    #[serde(default)]
    pub dry_run: bool,
    /// Include web checks even when the merge does not change web/.
    #[arg(long)]
    #[serde(default)]
    pub web: bool,
    /// Advance local main after verification without pushing origin.
    #[arg(long)]
    #[serde(default)]
    pub no_push: bool,
    /// Emit one structured JSON result on stdout; progress goes to stderr.
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
    /// Repository checkout (default: current directory).
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Dedicated gate build directory (default: <repo>/target-gate).
    #[arg(long)]
    pub target_dir: Option<PathBuf>,
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
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MergeReport {
    pub exit_code: i32,
    status: String,
    branch: String,
    dry_run: bool,
    expected_main: Option<String>,
    source: Option<String>,
    merged: Option<String>,
    target_dir: Option<PathBuf>,
    gate_override: bool,
    web: bool,
    main_updated: bool,
    pushed: bool,
    conflicts: Vec<String>,
    steps: Vec<Step>,
    error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
enum MergeStop {
    #[error("merge conflicts: {0:?}")]
    Conflict(Vec<String>),
    #[error("main changed during verification; compare-and-swap lost")]
    CasLost,
}

pub(crate) fn run(args: MergeArgs) -> Result<i32> {
    let json = args.json;
    let report = execute(args);
    if json {
        super::hub_client::print_json(&serde_json::to_value(&report)?)?;
    } else {
        for step in &report.steps {
            println!(
                "{}: {} ({} ms){}",
                step.name,
                step.status,
                step.duration_ms,
                if step.retried { " [retried]" } else { "" }
            );
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
    let mut report = MergeReport {
        exit_code: 1,
        status: "gate_failed".into(),
        branch: args.branch.clone(),
        dry_run: args.dry_run,
        expected_main: None,
        source: None,
        merged: None,
        target_dir: None,
        gate_override: std::env::var_os("REMUDA_MERGE_GATE_COMMAND")
            .is_some_and(|value| !value.is_empty()),
        web: args.web,
        main_updated: false,
        pushed: false,
        conflicts: Vec::new(),
        steps: Vec::new(),
        error: None,
    };
    let mut temporary = None;
    let result = execute_inner(&args, &mut report, &mut temporary);
    match result {
        Ok(()) => {
            report.exit_code = 0;
            report.status = if args.dry_run { "dry_run" } else { "ok" }.into();
        }
        Err(error) => {
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
                None => {}
            }
            report.error = Some(format!("{error:#}"));
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

fn execute_inner(
    args: &MergeArgs,
    report: &mut MergeReport,
    temporary: &mut Option<TemporaryWorktree>,
) -> Result<()> {
    let (repo, reference) = record(report, "repository", || {
        ensure!(args.gate || args.dry_run, "--gate or --dry-run is required");
        let cwd = args.repo.clone().unwrap_or(std::env::current_dir()?);
        let repo = PathBuf::from(git(&cwd, &["rev-parse", "--show-toplevel"])?);
        let reference = branch_ref(&args.branch)?;
        git(&repo, &["check-ref-format", &reference])?;
        Ok((repo, reference))
    })?;
    let target = args
        .target_dir
        .as_ref()
        .map(|path| repo.join(path))
        .unwrap_or_else(|| repo.join("target-gate"));
    report.target_dir = Some(target.clone());
    if args.dry_run {
        report.steps.push(Step::planned("fetch"));
    } else {
        record(report, "fetch", || git(&repo, &["fetch", "origin"]))?;
    }
    let (expected, source) = record(report, "preflight", || {
        check_branch_worktrees(&repo, &reference)?;
        Ok((
            resolve(&repo, "refs/heads/main")?,
            resolve(&repo, &reference)?,
        ))
    })?;
    report.expected_main = Some(expected.clone());
    report.source = Some(source.clone());

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
        report.steps.push(Step::planned("worktree"));
        report.steps.push(Step::planned("merge"));
        report.steps.extend(gate_plan(&repo, report.web)?);
        report.steps.push(Step::planned("verify-tree"));
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
    let merged = record(report, "merge", || {
        let output = git_output(
            &worktree,
            &[
                "-c",
                "core.editor=true",
                "merge",
                "--no-ff",
                "--no-edit",
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
    let diff = git_output(
        &worktree,
        &["diff", "--name-only", "-z", &expected, &merged, "--"],
    )?;
    successful(&diff)?;
    report.web |= web_changed(&diff.stdout);
    let report_file = worktree.with_file_name("gate.jsonl");
    run_gate(report, &worktree, &target, &report_file)?;
    record(report, "verify-tree", || {
        ensure!(
            resolve(&worktree, "HEAD")? == merged,
            "gate changed the merge HEAD"
        );
        git(&worktree, &["diff", "--exit-code", "HEAD", "--"])?;
        Ok(())
    })?;
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

fn web_changed(paths: &[u8]) -> bool {
    paths
        .split(|byte| *byte == 0)
        .any(|path| path.starts_with(b"web/"))
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

fn gate_command(repo: &Path, web: bool) -> Command {
    let mut command = Command::new("bash");
    command
        .arg(repo.join("scripts/ci/gate.sh"))
        .current_dir(repo)
        .stdin(Stdio::null());
    if web {
        command.arg("--web");
    }
    command
}

fn gate_plan(repo: &Path, web: bool) -> Result<Vec<Step>> {
    let output = gate_command(repo, web)
        .arg("--list")
        .output()
        .context("read gate plan")?;
    successful(&output)?;
    serde_json::from_slice(&output.stdout).context("parse gate plan")
}

fn run_gate(
    report: &mut MergeReport,
    repo: &Path,
    target: &Path,
    report_file: &Path,
) -> Result<()> {
    let planned = gate_plan(repo, report.web)?;
    let status = gate_command(repo, report.web)
        .arg("--report")
        .arg(report_file)
        .env("CARGO_TARGET_DIR", target)
        .env("CARGO_INCREMENTAL", "0")
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
    ensure!(
        status.success() && complete,
        "gate failed or returned an incomplete step report"
    );
    Ok(())
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
    directory: PathBuf,
    path: PathBuf,
    removed: bool,
}

impl TemporaryWorktree {
    fn new(repo: &Path) -> Result<Self> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let parent = repo.join("data/tmp");
        fs::create_dir_all(&parent).context("create data/tmp")?;
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let directory = parent.join(format!(
            "merge-{}-{timestamp}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).context("reserve temporary merge directory")?;
        // Match the path recorded by Git even when data/tmp is a symlink.
        let directory = directory
            .canonicalize()
            .context("resolve merge directory")?;
        Ok(Self {
            repo: repo.into(),
            path: directory.join("worktree"),
            directory,
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
        fs::remove_dir_all(&self.directory).context("remove temporary merge directory")?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!web_changed(b"webish/a.rs\0docs/web/a.rs\0"));
        assert!(!web_changed(b""));
    }

    #[test]
    fn gate_plan_has_shared_order_and_optional_web_steps() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let plan = gate_plan(&repo, false).unwrap();
        let names: Vec<_> = plan.iter().map(|step| step.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "secret-scan",
                "cargo-fmt",
                "cargo-check",
                "cargo-clippy",
                "cargo-test",
                "web-install",
                "web-build",
                "web-test"
            ]
        );
        assert!(plan[5..].iter().all(|step| step.status == "skipped"));
        assert!(
            gate_plan(&repo, true)
                .unwrap()
                .iter()
                .all(|step| step.status == "planned")
        );
        let output = gate_command(&repo, false)
            .args(["--list", "--web-only"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let web_only: Vec<Step> = serde_json::from_slice(&output.stdout).unwrap();
        assert!(web_only[..5].iter().all(|step| step.status == "skipped"));
        assert!(web_only[5..].iter().all(|step| step.status == "planned"));
    }
}
