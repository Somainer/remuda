//! Lane gate runner: Hub RPC `gate.run` / `gate.cancel` / `gate.then`.
//!
//! Decision D-034: the Node runs the gate through **its own `remuda` binary**
//! in the lane checkout (`remuda merge --gate --onto main` / `--land`), the
//! same CLI r-mergequeue ships. The gate step authority therefore stays with
//! `scripts/ci/gate.sh` in the merged worktree; this module owns only the lane
//! mechanics the old `remote-gate.sh` performed: fetch, the fast-forward with
//! stale-tip refusal, lane/process-group locking, per-step timeout plumbing,
//! cancel-kill and the land push from the lane host.
//!
//! One job runs per lane at a time (backstop to the Hub queue); step results
//! stream back as Node-originated `gate.event` frames on every carrier, and the
//! final verdict is both the `gate.run` reply and a `finished` event.

use crate::DevNode;
use crate::NodeError;
use remuda_protocol::{
    GateCancelParams, GateEventKind, GateEventParams, GateRunParams, GateRunResult, GateThenParams,
    GateThenResult,
};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, watch};

/// Hub→Node gate RPCs served by every carrier (outbound WSS and ssh-stdio).
#[must_use]
pub fn is_gate_method(method: &str) -> bool {
    remuda_protocol::is_gate_call(method)
}

/// Default cap for a whole gate run and for a `--then` command.
const DEFAULT_GATE_TIMEOUT_SECS: u64 = 60 * 60;
const DEFAULT_THEN_TIMEOUT_SECS: u64 = 10 * 60;
/// Captured output retained in a result.
const OUTPUT_CAP_BYTES: usize = 16 * 1024;

/// One live gate run: cancel flag and the process group to kill.
struct LiveRun {
    cancel: watch::Sender<bool>,
    pgid: std::sync::Mutex<Option<i32>>,
}

/// Lane locks, running jobs and the per-Node event uplink.
pub(crate) struct GateRegistry {
    /// laneId → one slot; a held slot rejects a second run (`lane-busy`).
    lanes: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    /// jobId → live run handle.
    runs: Mutex<BTreeMap<String, Arc<LiveRun>>>,
    events: tokio::sync::broadcast::Sender<GateEventParams>,
}

impl GateRegistry {
    pub(crate) fn new() -> Self {
        let (events, _) = tokio::sync::broadcast::channel(512);
        Self {
            lanes: Mutex::new(BTreeMap::new()),
            runs: Mutex::new(BTreeMap::new()),
            events,
        }
    }

    /// Subscribe to Node-originated gate events for carrier pumps.
    pub(crate) fn subscribe(&self) -> tokio::sync::broadcast::Receiver<GateEventParams> {
        self.events.subscribe()
    }
}

impl DevNode {
    /// Handle `gate.run`.
    pub(crate) async fn run_gate(&self, params: &Value) -> Result<Value, NodeError> {
        let request: GateRunParams = serde_json::from_value(params.clone()).map_err(|error| {
            NodeError::InvalidRequest(format!("invalid gate.run params: {error}"))
        })?;
        let result = self.run_gate_typed(request).await?;
        serde_json::to_value(result).map_err(NodeError::from)
    }

    /// Handle `gate.cancel` — kills the running step's process group.
    pub(crate) async fn cancel_gate(&self, params: &Value) -> Result<Value, NodeError> {
        let request: GateCancelParams =
            serde_json::from_value(params.clone()).map_err(|error| {
                NodeError::InvalidRequest(format!("invalid gate.cancel params: {error}"))
            })?;
        let runs = self.gate_registry().runs.lock().await;
        if let Some(run) = runs.get(&request.job_id) {
            run.cancel.send(true).ok();
            if let Some(pgid) = *run.pgid.lock().unwrap_or_else(|poison| poison.into_inner()) {
                kill_group(pgid);
            }
        }
        drop(runs);
        Ok(serde_json::json!({ "ok": true, "jobId": request.job_id }))
    }

    /// Handle `gate.then` — a post-land command on the project's home host.
    pub(crate) async fn run_gate_then(&self, params: &Value) -> Result<Value, NodeError> {
        let request: GateThenParams = serde_json::from_value(params.clone()).map_err(|error| {
            NodeError::InvalidRequest(format!("invalid gate.then params: {error}"))
        })?;
        let result = self.run_gate_then_typed(request).await?;
        serde_json::to_value(result).map_err(NodeError::from)
    }

    async fn run_gate_typed(&self, request: GateRunParams) -> Result<GateRunResult, NodeError> {
        let registry = self.gate_registry();
        // One running job per lane (Node-side backstop; the Hub queue is the
        // authority).
        let slot = {
            let mut lanes = registry.lanes.lock().await;
            lanes.entry(request.lane_id.clone()).or_default().clone()
        };
        let lane_guard = match slot.try_lock_owned() {
            Ok(guard) => guard,
            Err(_) => {
                return Ok(GateRunResult {
                    job_id: request.job_id.clone(),
                    status: "lane-busy".into(),
                    error: Some(format!(
                        "lane {} already has a running job",
                        request.lane_id
                    )),
                    ..Default::default()
                });
            }
        };

        let (cancel_tx, cancel_rx) = watch::channel(false);
        let live = Arc::new(LiveRun {
            cancel: cancel_tx,
            pgid: std::sync::Mutex::new(None),
        });
        {
            let mut runs = registry.runs.lock().await;
            if runs.contains_key(&request.job_id) {
                return Err(NodeError::InvalidRequest(format!(
                    "gate job {} is already running",
                    request.job_id
                )));
            }
            runs.insert(request.job_id.clone(), live.clone());
        }

        let outcome = self.execute_gate(&request, live.clone(), cancel_rx).await;

        {
            let mut runs = registry.runs.lock().await;
            runs.remove(&request.job_id);
        }
        drop(lane_guard);

        // The finished event is authoritative even if the RPC reply is lost.
        let _ = registry.events.send(GateEventParams {
            job_id: request.job_id.clone(),
            kind: GateEventKind::Finished {
                result: outcome.clone(),
            },
        });
        Ok(outcome)
    }

    async fn execute_gate(
        &self,
        request: &GateRunParams,
        live: Arc<LiveRun>,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> GateRunResult {
        let mut result = GateRunResult {
            job_id: request.job_id.clone(),
            status: "failed".into(),
            ..Default::default()
        };
        let repo = PathBuf::from(&request.repo_path);
        let emit = |kind: GateEventKind| {
            let _ = self.gate_registry().events.send(GateEventParams {
                job_id: request.job_id.clone(),
                kind,
            });
        };

        // 1. Fetch and move the lane's local main to origin/main (the checkout
        //    is product-managed; mirrors remote-gate.sh's checkout -B).
        emit(GateEventKind::Phase {
            phase: "fetch".into(),
        });
        if let Err(error) = block_git(&repo, &["fetch", "-q", "origin"]) {
            result.error = Some(format!("fetch origin: {error}"));
            return result;
        }
        if let Err(error) = sync_local_main(&repo, &request.base_branch) {
            result.error = Some(format!("sync {base}: {error}", base = request.base_branch));
            return result;
        }

        // 2. Fast-forward the branch tip, refusing a stale local tip when a
        //    worker worktree holds the branch and has diverged.
        emit(GateEventKind::Phase { phase: "ff".into() });
        if let Err(error) = block_git(&repo, &["fetch", "-q", "origin", &request.branch]) {
            // The branch may already exist locally; fetch failure is only
            // fatal when the branch is absent.
            if block_git(&repo, &["rev-parse", "--verify", &request.branch]).is_err() {
                result.error = Some(format!("fetch branch: {error}"));
                return result;
            }
        }
        if let Err(error) = fast_forward_branch(&repo, &request.branch) {
            result.status = "stale-tip".into();
            result.error = Some(error);
            return result;
        }

        // 3. Run the consumed merge CLI in the lane checkout.
        let phase = if request.mode == "land" {
            "land"
        } else {
            "gate"
        };
        emit(GateEventKind::Phase {
            phase: phase.into(),
        });
        let binary = request.binary.clone().unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
        let mut args = vec![
            "merge".to_owned(),
            request.branch.clone(),
            "--onto".to_owned(),
            request.base_branch.clone(),
            "--json".to_owned(),
            "--repo".to_owned(),
            request.repo_path.clone(),
            "--target-dir".to_owned(),
            request.target_dir.clone(),
        ];
        // Both modes verify the merge onto current `main` inside this
        // invocation; land additionally CAS-updates and pushes main. A
        // base-moved result makes the Hub re-queue and re-verify (D-034).
        args.push("--gate".into());
        if request.mode == "land" {
            args.push("--land".into());
            if !request.push {
                args.push("--no-push".into());
            }
        } else {
            args.push("--no-push".into());
        }
        if request.web == "always" {
            args.push("--web".into());
            args.push("--web-e2e".into());
        }

        let mut command = Command::new(&binary);
        command
            .args(&args)
            .current_dir(&repo)
            .envs(request.env.iter().map(|(k, v)| (k.clone(), v.clone())))
            .env("CARGO_TARGET_DIR", &request.target_dir)
            .env("CARGO_INCREMENTAL", "0")
            .env("VITE_NO_WATCH", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        prepend_path(&mut command, request.toolchain_path.as_deref());
        if let Some(endpoint) = &request.pw_endpoint {
            command.env("PW_TEST_CONNECT_WS_ENDPOINT", endpoint);
            command.env("PW_CHANNEL", "chromium");
        }
        if let Some(lock) = &request.lock_path {
            command.env("REMUDA_E2E_LOCK", lock);
        }
        if let Some((listen, web_port)) = port_pair(request.ports.as_deref()) {
            command.env("HUB_E2E_LISTEN", listen);
            command.env("HUB_E2E_WEB_PORT", web_port);
        }
        if !request.timeouts.is_empty()
            && let Ok(json) = serde_json::to_string(&request.timeouts)
        {
            command.env("REMUDA_GATE_STEP_TIMEOUTS", json);
        }
        #[cfg(unix)]
        command.process_group(0);

        let started = Instant::now();
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                result.error = Some(format!("spawn {binary}: {error}"));
                return result;
            }
        };
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            *live
                .pgid
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) =
                Some(i32::try_from(pid).unwrap_or(0));
        }

        // Stream gate.jsonl step results as they are flushed (gate.sh flushes
        // every step), and stderr lines as log events.
        let run_started = std::time::SystemTime::now();
        let step_task = tokio::spawn(stream_gate_report(
            self.clone(),
            request.job_id.clone(),
            run_started,
        ));
        if let Some(stderr) = child.stderr.take() {
            let node = self.clone();
            let job_id = request.job_id.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = node.gate_registry().events.send(GateEventParams {
                        job_id: job_id.clone(),
                        kind: GateEventKind::Log { message: line },
                    });
                }
            });
        }
        let stdout = child.stdout.take();

        let deadline = if request.gate_timeout_secs > 0 {
            request.gate_timeout_secs
        } else {
            DEFAULT_GATE_TIMEOUT_SECS
        };
        let wait = async {
            let output = child.wait_with_output_piped(stdout).await;
            (output, started.elapsed())
        };
        let mut canceled = false;
        let mut timed_out = false;
        let (output, _elapsed) = tokio::select! {
            pair = wait => pair,
            _ = cancel_rx.changed() => {
                canceled = true;
                if let Some(pgid) = *live.pgid.lock().unwrap_or_else(|poison| poison.into_inner()) {
                    kill_group(pgid);
                }
                // Reap; stdout is dropped.
                let _ = child.kill().await;
                let _ = child.wait().await;
                (None, started.elapsed())
            }
            _ = tokio::time::sleep(Duration::from_secs(deadline)) => {
                timed_out = true;
                if let Some(pgid) = *live.pgid.lock().unwrap_or_else(|poison| poison.into_inner()) {
                    kill_group(pgid);
                }
                let _ = child.kill().await;
                let _ = child.wait().await;
                (None, started.elapsed())
            }
        };
        step_task.abort();
        if canceled {
            result.status = "canceled".into();
            result.error = Some("canceled by request; killed step process group".into());
            return result;
        }
        if timed_out {
            result.error = Some(format!(
                "gate run exceeded {deadline}s; killed step process group"
            ));
            return result;
        }
        let Some(output) = output else {
            result.error = Some("gate produced no output".into());
            return result;
        };

        let report: Value = match serde_json::from_slice(&output.stdout) {
            Ok(value) => value,
            Err(error) => {
                result.error = Some(format!(
                    "parse merge report: {error}; stderr: {}",
                    String::from_utf8_lossy(&output.stderr)
                        .chars()
                        .take(2000)
                        .collect::<String>()
                ));
                return result;
            }
        };
        result.steps = report
            .get("steps")
            .and_then(Value::as_array)
            .map(|steps| {
                steps
                    .iter()
                    .filter_map(|step| {
                        serde_json::from_value::<remuda_protocol::GateStep>(step.clone()).ok()
                    })
                    .collect()
            })
            .unwrap_or_default();
        result.base_sha = string_field(&report, "base");
        result.head_sha = string_field(&report, "head");
        result.merge_sha = string_field(&report, "merged");
        result.current_main_sha = string_field(&report, "currentMain");
        match report.get("status").and_then(Value::as_str).unwrap_or("") {
            "ok" | "verified" => result.status = "passed".into(),
            "landed" => result.status = "landed".into(),
            "base_moved" => {
                result.status = "base-moved".into();
                result.error = string_field(&report, "error");
            }
            other => {
                result.status = "failed".into();
                result.error = string_field(&report, "error")
                    .or_else(|| Some(format!("merge status {other:?}")));
            }
        }
        result
    }

    async fn run_gate_then_typed(
        &self,
        request: GateThenParams,
    ) -> Result<GateThenResult, NodeError> {
        let (_, workspace_root) = self.resolve_workspace_cwd(None, None)?;
        let cwd = request.cwd.map(PathBuf::from).unwrap_or(workspace_root);
        let mut command = Command::new("bash");
        command
            .args(["-lc", &request.command])
            .current_dir(&cwd)
            .envs(request.env.iter().map(|(k, v)| (k.clone(), v.clone())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let child = command.spawn().map_err(|error| {
            NodeError::InvalidRequest(format!("spawn gate.then command: {error}"))
        })?;
        let timeout = if request.timeout_secs > 0 {
            request.timeout_secs
        } else {
            DEFAULT_THEN_TIMEOUT_SECS
        };
        let output = tokio::time::timeout(Duration::from_secs(timeout), child.wait_with_output())
            .await
            .map_err(|_| {
                NodeError::InvalidRequest(format!("gate.then timed out after {timeout}s"))
            })?
            .map_err(|error| NodeError::InvalidRequest(format!("gate.then wait: {error}")))?;
        let mut captured = String::from_utf8_lossy(&output.stdout).into_owned();
        captured.push_str(&String::from_utf8_lossy(&output.stderr));
        captured.truncate(OUTPUT_CAP_BYTES);
        Ok(GateThenResult {
            job_id: request.job_id,
            exit_code: output.status.code().unwrap_or(-1),
            output: captured,
        })
    }
}

/// Extension trait so a piped-stdout child can still consume wait_with_output.
trait WaitWithOutputPiped {
    async fn wait_with_output_piped(
        &mut self,
        stdout: Option<tokio::process::ChildStdout>,
    ) -> Option<std::process::Output>;
}

impl WaitWithOutputPiped for tokio::process::Child {
    async fn wait_with_output_piped(
        &mut self,
        stdout: Option<tokio::process::ChildStdout>,
    ) -> Option<std::process::Output> {
        use tokio::io::AsyncReadExt;
        let mut stdout_bytes = Vec::new();
        if let Some(mut out) = stdout {
            let _ = out.read_to_end(&mut stdout_bytes).await;
        }
        let status = self.wait().await.ok()?;
        Some(std::process::Output {
            status,
            stdout: stdout_bytes,
            stderr: Vec::new(),
        })
    }
}

/// Tail every `<tmp>/remuda-mq-*/gate.jsonl` the merge gate flushes and emit
/// one step event per new line. Only reports modified at or after the run
/// started (mtime granularity tolerated) are eligible.
async fn stream_gate_report(node: DevNode, job_id: String, run_started: std::time::SystemTime) {
    let window_start = run_started - Duration::from_secs(5);
    let mut offsets: BTreeMap<PathBuf, u64> = BTreeMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    loop {
        let temp = std::env::temp_dir();
        if let Ok(entries) = std::fs::read_dir(&temp) {
            for entry in entries.flatten() {
                if !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("remuda-mq-")
                {
                    continue;
                }
                let report = entry.path().join("gate.jsonl");
                if !report.is_file() {
                    continue;
                }
                let fresh = report
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .is_ok_and(|modified| modified >= window_start);
                if !fresh {
                    continue;
                }
                if let Ok(text) = std::fs::read_to_string(&report) {
                    let prev = offsets.entry(report.clone()).or_insert(0);
                    let bytes = text.as_bytes();
                    let start = (*prev as usize).min(bytes.len());
                    if start >= bytes.len() {
                        continue;
                    }
                    let chunk = &text[start..];
                    *prev = bytes.len() as u64;
                    for line in chunk.lines() {
                        let Ok(step) = serde_json::from_str::<remuda_protocol::GateStep>(line)
                        else {
                            continue;
                        };
                        let dedupe = format!("{}:{}:{}", step.name, step.status, step.duration_ms);
                        if !seen.insert(dedupe) {
                            continue;
                        }
                        let _ = node.gate_registry().events.send(GateEventParams {
                            job_id: job_id.clone(),
                            kind: GateEventKind::Step { step },
                        });
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

/// Merge lane checkout prep: move local `main` to `origin/main` before a gate.
fn sync_local_main(repo: &Path, base_branch: &str) -> Result<(), String> {
    let origin = format!("refs/remotes/origin/{base_branch}");
    let remote = git_output(repo, &["rev-parse", "--verify", &origin])?;
    let local = git_output(
        repo,
        &[
            "rev-parse",
            "--verify",
            &format!("refs/heads/{base_branch}"),
        ],
    );
    match local {
        Ok(local) if local == remote => Ok(()),
        _ => run_git(
            repo,
            &["update-ref", &format!("refs/heads/{base_branch}"), &remote],
        ),
    }
}

/// Fast-forward the local branch to its origin tip. When a worker worktree
/// holds the branch checked out, ff that worktree instead. A divergent held
/// tip is the stale-tip refusal.
fn fast_forward_branch(repo: &Path, branch: &str) -> Result<(), String> {
    let remote_ref = format!("origin/{branch}");
    let remote = git_output(repo, &["rev-parse", "--verify", &remote_ref])?;
    if run_git(repo, &["branch", "-f", branch, &remote]).is_ok() {
        return Ok(());
    }
    // The branch is checked out somewhere: ff that worktree.
    let porcelain = git_output(repo, &["worktree", "list", "--porcelain"])?;
    let needle = format!("refs/heads/{branch}");
    let mut worktree: Option<PathBuf> = None;
    let mut current = PathBuf::new();
    for line in porcelain.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current = PathBuf::from(path);
        } else if line == format!("branch {needle}") {
            worktree = Some(current.clone());
        }
    }
    let Some(worktree) = worktree else {
        // Not checked out anywhere: create the branch at the remote tip.
        return run_git(repo, &["branch", branch, &remote]);
    };
    run_git(&worktree, &["merge", "-q", "--ff-only", &remote])?;
    let local = git_output(repo, &["rev-parse", branch])?;
    if local != remote {
        return Err(format!(
            "local {branch} ({}) is not at {remote_ref} ({remote}) — worktree diverged; refusing to gate a stale tip",
            local.chars().take(12).collect::<String>()
        ));
    }
    Ok(())
}

fn prepend_path(command: &mut Command, prefix: Option<&str>) {
    if let Some(prefix) = prefix
        && !prefix.is_empty()
    {
        let current = std::env::var_os("PATH");
        let mut value = std::ffi::OsString::from(prefix);
        if let Some(current) = current {
            value.push(":");
            value.push(current);
        }
        command.env("PATH", value);
    }
}

/// Derive `(HUB_E2E_LISTEN, HUB_E2E_WEB_PORT)` from a `58480-58489` block.
fn port_pair(block: Option<&str>) -> Option<(String, String)> {
    let block = block?.trim();
    let mut parts = block.splitn(2, '-');
    let first = parts.next()?.trim();
    let last = parts.next().map_or(first, str::trim);
    first.parse::<u16>().ok()?;
    Some((format!("127.0.0.1:{first}"), last.to_owned()))
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn block_git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .current_dir(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| format!("spawn git: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn run_git(repo: &Path, args: &[&str]) -> Result<(), String> {
    block_git(repo, args).map(|_| ())
}

fn git_output(repo: &Path, args: &[&str]) -> Result<String, String> {
    block_git(repo, args)
}

#[cfg(unix)]
fn kill_group(pgid: i32) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    let _ = killpg(Pid::from_raw(pgid), Signal::SIGTERM);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    struct Fixture {
        node: DevNode,
        lane: PathBuf,
        target: PathBuf,
        bin: PathBuf,
        _dir: TempDir,
        _bare: TempDir,
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(repo)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// A bare origin plus a lane clone, with `main` and a `wt/fake/task`
    /// branch carrying one commit.
    fn fixture(script: &str) -> Fixture {
        let dir = TempDir::new().unwrap();
        let bare = TempDir::new().unwrap();
        let origin = bare.path().join("origin.git");
        std::process::Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg("-b")
            .arg("main")
            .arg(&origin)
            .status()
            .unwrap();
        let seed = dir.path().join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "-b", "main"]);
        git(&seed, &["config", "user.email", "t@example.com"]);
        git(&seed, &["config", "user.name", "T"]);
        std::fs::write(seed.join("file.txt"), "base\n").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-q", "-m", "init"]);
        git(&seed, &["branch", "wt/fake/task"]);
        git(
            &seed,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&seed, &["push", "-q", "origin", "main", "wt/fake/task"]);

        let lane = dir.path().join("lane");
        git(
            dir.path(),
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                lane.to_str().unwrap(),
            ],
        );
        git(&lane, &["config", "user.email", "t@example.com"]);
        git(&lane, &["config", "user.name", "T"]);
        git(&lane, &["fetch", "-q", "origin"]);

        let bin = dir.path().join("fake-merge.sh");
        std::fs::write(&bin, format!("#!/bin/bash\nset -eu\n{script}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let target = dir.path().join("target");
        std::fs::create_dir_all(&target).unwrap();

        let config = crate::DevServerConfig::loopback(0)
            .with_workspace_root(lane.clone())
            .with_workspace_roots(vec![dir.path().to_path_buf()])
            .with_workspace_registry(dir.path().to_path_buf());
        let node = DevNode::new(&config).unwrap();
        Fixture {
            node,
            lane,
            target,
            bin,
            _dir: dir,
            _bare: bare,
        }
    }

    fn params(fixture: &Fixture, mode: &str) -> GateRunParams {
        GateRunParams {
            job_id: "gjb_test".into(),
            lane_id: "lane1".into(),
            repo_path: fixture.lane.to_string_lossy().into_owned(),
            target_dir: fixture.target.to_string_lossy().into_owned(),
            branch: "wt/fake/task".into(),
            base_branch: "main".into(),
            mode: mode.into(),
            web: "auto".into(),
            env: std::collections::BTreeMap::new(),
            lock_path: None,
            pw_endpoint: None,
            toolchain_path: None,
            ports: None,
            timeouts: std::collections::BTreeMap::new(),
            gate_timeout_secs: 0,
            push: false,
            binary: Some(fixture.bin.to_string_lossy().into_owned()),
        }
    }

    const PASSING_SCRIPT: &str = r#"
scratch=$(mktemp -d "/tmp/remuda-mq-fake.XXXXXX")
echo '{"name":"secret-scan","status":"ok","durationMs":11,"attempts":1,"retried":false}' > "$scratch/gate.jsonl"
echo '{"name":"cargo-test","status":"ok","durationMs":22,"attempts":1,"retried":false}' >> "$scratch/gate.jsonl"
sleep 0.6
cat <<'JSON'
{"exitCode":0,"status":"verified","branch":"wt/fake/task","base":"1111111111111111111111111111111111111111","head":"2222222222222222222222222222222222222222","merged":"3333333333333333333333333333333333333333","steps":[{"name":"secret-scan","status":"ok","durationMs":11},{"name":"cargo-test","status":"ok","durationMs":22}]}
JSON
"#;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verify_passes_and_streams_steps() {
        let fixture = fixture(PASSING_SCRIPT);
        let mut events = fixture.node.gate_registry().subscribe();
        let result = fixture
            .node
            .run_gate_typed(params(&fixture, "verify"))
            .await
            .unwrap();
        assert_eq!(
            result.status,
            "passed",
            "{}",
            result.error.unwrap_or_default()
        );
        assert_eq!(
            result.merge_sha.as_deref(),
            Some("3333333333333333333333333333333333333333")
        );
        // The lane checkout is synced to origin/main.
        let local_main = git(&fixture.lane, &["rev-parse", "main"]);
        let origin_main = git(&fixture.lane, &["rev-parse", "origin/main"]);
        assert_eq!(local_main, origin_main);
        // Steps streamed through the event bus.
        let mut streamed = Vec::new();
        while let Ok(Ok(event)) =
            tokio::time::timeout(Duration::from_millis(300), events.recv()).await
        {
            if let GateEventKind::Step { step } = event.kind {
                streamed.push(step.name);
            }
        }
        assert!(
            streamed.iter().any(|name| name == "cargo-test"),
            "{streamed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn land_mode_invokes_the_merge_cli_with_land() {
        // The fake records argv and returns a landed verdict.
        let script = r#"
echo "$@" > "$(dirname "$0")/args.txt"
echo '{"exitCode":0,"status":"landed","merged":"4444444444444444444444444444444444444444","steps":[]}'
"#;
        let fixture = fixture(script);
        let mut p = params(&fixture, "land");
        p.push = true;
        let result = fixture.node.run_gate_typed(p).await.unwrap();
        assert_eq!(result.status, "landed");
        assert_eq!(
            result.merge_sha.as_deref(),
            Some("4444444444444444444444444444444444444444")
        );
        let argv = std::fs::read_to_string(fixture.bin.parent().unwrap().join("args.txt")).unwrap();
        assert!(argv.contains("--land"), "argv: {argv}");
        assert!(argv.contains("--gate"), "argv: {argv}");
        assert!(!argv.contains("--no-push"), "land with push=true: {argv}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_tip_is_refused_when_a_worktree_holds_a_diverged_branch() {
        let fixture = fixture(PASSING_SCRIPT);
        // A worker worktree holds wt/fake/task checked out at a divergent tip.
        let wt = fixture._dir.path().join("worker-wt");
        git(
            &fixture.lane,
            &[
                "worktree",
                "add",
                "-B",
                "wt/fake/task",
                wt.to_str().unwrap(),
                "origin/wt/fake/task",
            ],
        );
        std::fs::write(wt.join("local.txt"), "diverged\n").unwrap();
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-q", "-m", "local-only"]);
        let result = fixture
            .node
            .run_gate_typed(params(&fixture, "verify"))
            .await
            .unwrap();
        assert_eq!(
            result.status,
            "stale-tip",
            "{}",
            result.error.unwrap_or_default()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_concurrent_run_on_same_lane_is_rejected() {
        let fixture =
            fixture("sleep 3\necho '{\"exitCode\":0,\"status\":\"verified\",\"steps\":[]}'");
        let node = fixture.node.clone();
        let p1 = params(&fixture, "verify");
        let mut p2 = params(&fixture, "verify");
        p2.job_id = "gjb_second".into();
        let first = tokio::spawn(async move { node.run_gate_typed(p1).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let second = fixture.node.run_gate_typed(p2).await.unwrap();
        assert_eq!(second.status, "lane-busy");
        let first = first.await.unwrap().unwrap();
        assert_eq!(first.status, "passed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_kills_the_running_process_group() {
        // A fake that ignores normal completion until killed; run under its
        // own process group.
        let script = "sleep 30\necho '{\"exitCode\":0,\"status\":\"verified\"}'";
        let fixture = fixture(script);
        let node = fixture.node.clone();
        let p = params(&fixture, "verify");
        let job_id = p.job_id.clone();
        let running = tokio::spawn(async move { node.run_gate_typed(p).await });
        tokio::time::sleep(Duration::from_millis(400)).await;
        let canceled = fixture
            .node
            .cancel_gate(&serde_json::json!({ "jobId": job_id }))
            .await
            .unwrap();
        assert_eq!(canceled["ok"], true);
        let result = tokio::time::timeout(Duration::from_secs(10), running)
            .await
            .expect("cancel reaped the run")
            .unwrap()
            .unwrap();
        assert_eq!(
            result.status,
            "canceled",
            "{}",
            result.error.unwrap_or_default()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_deadline_kills_and_fails() {
        let fixture = fixture("sleep 30\necho '{\"exitCode\":0,\"status\":\"verified\"}'");
        let mut p = params(&fixture, "verify");
        p.gate_timeout_secs = 1;
        let started = std::time::Instant::now();
        let result = fixture.node.run_gate_typed(p).await.unwrap();
        assert!(result.status == "failed" && result.error.unwrap().contains("exceeded"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn port_pair_and_path_helpers() {
        let (listen, web) = super::port_pair(Some("58480-58489")).unwrap();
        assert_eq!(listen, "127.0.0.1:58480");
        assert_eq!(web, "58489");
        assert!(super::port_pair(Some("garbage")).is_none());
    }
}
