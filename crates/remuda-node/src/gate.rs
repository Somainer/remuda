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
//!
//! Every step's output is tailed into bounded rings (see `RunTrace`); a failed
//! run hands the Hub a `GateRunLog` — the last 400 lines plus an extracted
//! cargo-test/Playwright summary — which the Hub stores as an `obj_…` log
//! object so the job row never carries megabytes (evidence: gate-log-1).

use crate::DevNode;
use crate::NodeError;
use remuda_protocol::{
    GateCancelParams, GateEventKind, GateEventParams, GateLandParams, GateLandResult, GateRunLog,
    GateRunParams, GateRunResult, GateStep, GateThenParams, GateThenResult, GateUnpinParams,
    GateUnpinResult,
};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet, VecDeque};
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
/// Default cap for a home-host land (fetch of the merge + leased push).
const DEFAULT_LAND_TIMEOUT_SECS: u64 = 10 * 60;
/// Captured output retained in a result.
const OUTPUT_CAP_BYTES: usize = 16 * 1024;

// Failure-evidence bounds (docs/design/evidence/gate-log-1.md): the lane
// runner tails every step's output into bounded rings; on failure it extracts
// a summary and stores only the last [`TAIL_LINES`] lines in the Hub log
// object.
const TAIL_LINES: usize = 400;
const TAIL_CAP_BYTES: usize = 96 * 1024;
const MAX_LINE_CHARS: usize = 4_096;
const STEP_RING_LINES: usize = 2_000;
const STEP_RING_BYTES: usize = 256 * 1024;
const GLOBAL_RING_LINES: usize = TAIL_LINES;
const GLOBAL_RING_BYTES: usize = 128 * 1024;
const FAILURES_SECTION_MAX: usize = 160;
const STANDALONE_SUMMARY_MAX: usize = 40;
const PLAYWRIGHT_TITLES_MAX: usize = 20;
const PLAYWRIGHT_BLOCK_MAX: usize = 100;

/// One live gate run: cancel flag and the process group to kill.
struct LiveRun {
    cancel: watch::Sender<bool>,
    pgid: std::sync::Mutex<Option<i32>>,
}

/// Bounded FIFO of text lines. Long runs (a 39-minute cargo-test) stream
/// megabytes; the ring keeps only the recent tail and counts what it dropped.
struct LineRing {
    lines: VecDeque<String>,
    bytes: usize,
    seen_lines: usize,
    line_cap: usize,
    byte_cap: usize,
}

impl LineRing {
    fn new(line_cap: usize, byte_cap: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            bytes: 0,
            seen_lines: 0,
            line_cap,
            byte_cap,
        }
    }

    fn push(&mut self, mut line: String) {
        if line.chars().count() > MAX_LINE_CHARS {
            let mut head: String = line.chars().take(MAX_LINE_CHARS - 1).collect();
            head.push('…');
            line = head;
        }
        self.bytes += line.len();
        self.lines.push_back(line);
        self.seen_lines += 1;
        while self.lines.len() > self.line_cap || self.bytes > self.byte_cap {
            let Some(old) = self.lines.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(old.len());
        }
    }

    fn last(&self, max_lines: usize, max_bytes: usize) -> (Vec<String>, bool) {
        let mut picked: Vec<String> = Vec::new();
        let mut bytes = 0usize;
        for line in self.lines.iter().rev().take(max_lines) {
            if bytes + line.len() > max_bytes && !picked.is_empty() {
                break;
            }
            bytes += line.len();
            picked.push(line.clone());
        }
        picked.reverse();
        let dropped = self.seen_lines > picked.len() || self.bytes > bytes;
        (picked, dropped)
    }
}

/// Per-step output captured live from the consumed merge CLI's stderr
/// (gate.sh streams every step there), keyed by its `gate: <step>` markers.
struct RunTrace {
    current: Option<String>,
    by_step: BTreeMap<String, LineRing>,
    global: LineRing,
}

impl RunTrace {
    fn new() -> Self {
        Self {
            current: None,
            by_step: BTreeMap::new(),
            global: LineRing::new(GLOBAL_RING_LINES, GLOBAL_RING_BYTES),
        }
    }

    fn push(&mut self, line: String) {
        if let Some((name, _retried)) = parse_step_marker(&line) {
            self.current = Some(name);
        }
        if let Some(name) = self.current.clone() {
            self.by_step
                .entry(name)
                .or_insert_with(|| LineRing::new(STEP_RING_LINES, STEP_RING_BYTES))
                .push(line.clone());
        }
        self.global.push(line);
    }
}

/// Parse a gate.sh attempt marker: exactly `gate: <name>` or
/// `gate: <name> (retried)`. Detail lines (`gate: cargo-test: …`) and the
/// pre-run banner (`gate: Rust tests: …`) keep their colon and never match.
fn parse_step_marker(line: &str) -> Option<(String, bool)> {
    let rest = line.strip_prefix("gate: ")?.trim_end();
    if rest.contains(':') {
        return None;
    }
    let (name, retried) = match rest.strip_suffix(" (retried)") {
        Some(name) => (name, true),
        None => (rest, false),
    };
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return None;
    }
    Some((name.to_owned(), retried))
}

/// Extract the actionable summary from one step's captured output.
///
/// * `cargo-test`: every `test <name> … FAILED` line, every `panicked at`
///   line, and the libtest `failures:` section (which carries the panic
///   detail); the rest of a thousands-of-lines run is dropped.
/// * Playwright web steps: the numbered failure titles and the first failure
///   block (error + expectation + stack head).
/// * everything else: no extraction — the bounded tail stays the evidence.
fn extract_summary(step: &str, lines: &[String]) -> Vec<String> {
    if step == "cargo-test" {
        cargo_test_summary(lines)
    } else if step.starts_with("web-") {
        playwright_summary(lines)
    } else {
        Vec::new()
    }
}

fn cargo_test_summary(lines: &[String]) -> Vec<String> {
    let mut section: Vec<String> = Vec::new();
    let mut standalone: Vec<String> = Vec::new();
    if let Some(start) = lines.iter().position(|line| line.trim() == "failures:") {
        let end = lines
            .get(start + 1..)
            .and_then(|rest| {
                rest.iter()
                    .position(|line| line.starts_with("test result:"))
                    .map(|offset| start + 1 + offset)
            })
            .unwrap_or(lines.len());
        section = lines
            .iter()
            .take(end.min(start + 1 + FAILURES_SECTION_MAX))
            .skip(start)
            .cloned()
            .collect();
    }
    for line in lines {
        let trimmed = line.trim();
        let failed_name = trimmed.strip_prefix("test ").and_then(|rest| {
            rest.strip_suffix(" FAILED")
                .or_else(|| rest.strip_suffix(" ... FAILED"))
        });
        let is_panic = trimmed.contains("panicked at ");
        if (failed_name.is_some() || is_panic) && standalone.len() < STANDALONE_SUMMARY_MAX {
            standalone.push(line.clone());
        }
    }
    let mut summary = section;
    let have: HashSet<String> = summary.iter().cloned().collect();
    for line in standalone {
        if !have.contains(&line) {
            summary.push(line);
        }
    }
    summary
}

/// Match a Playwright numbered failure header, e.g. `  1) [chromium] › …`.
fn numbered_failure(line: &str) -> bool {
    let trimmed = line.trim_start();
    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    !digits.is_empty() && trimmed[digits.len()..].starts_with(") ")
}

fn playwright_summary(lines: &[String]) -> Vec<String> {
    let headers: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| numbered_failure(line))
        .map(|(index, _)| index)
        .collect();
    if headers.is_empty() {
        return Vec::new();
    }
    let mut summary: Vec<String> = Vec::new();
    for index in headers.iter().take(PLAYWRIGHT_TITLES_MAX) {
        summary.push(lines[*index].trim().to_owned());
    }
    let block_end = headers
        .get(1)
        .copied()
        .unwrap_or_else(|| (headers[0] + 1 + PLAYWRIGHT_BLOCK_MAX).min(lines.len()))
        .min(headers[0] + 1 + PLAYWRIGHT_BLOCK_MAX);
    let have: HashSet<String> = summary.iter().cloned().collect();
    for line in &lines[headers[0]..block_end] {
        let trimmed = line.trim_end();
        if !trimmed.trim().is_empty() && !have.contains(trimmed) {
            summary.push(trimmed.to_owned());
        }
    }
    summary
}

/// First line of an error string; the job `reason` is one line by contract.
fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(text)
        .to_owned()
}

/// Build the bounded failure log for a failed step from a captured trace.
fn build_failure_log(
    trace: &RunTrace,
    step: &str,
    attempts: u32,
    step_error: Option<&str>,
    extra: Option<&str>,
) -> GateRunLog {
    let (tail, source_lines, truncated) = match trace.by_step.get(step) {
        Some(ring) => {
            let (tail, dropped) = ring.last(TAIL_LINES, TAIL_CAP_BYTES);
            let seen = ring.seen_lines;
            (tail, seen, dropped)
        }
        None => {
            let (tail, dropped) = trace.global.last(TAIL_LINES, TAIL_CAP_BYTES);
            let seen = trace.global.seen_lines;
            (tail, seen, dropped)
        }
    };
    let detail = extra
        .or(step_error)
        .filter(|text| !text.is_empty())
        .unwrap_or("failed");
    let mut headline = format!("{step} failed, {detail}");
    if attempts > 1 {
        headline.push_str(&format!(", attempts {attempts}, retried"));
    }
    let summary = extract_summary(step, &tail);
    GateRunLog {
        step: step.to_owned(),
        kind: "failed".into(),
        attempts,
        headline,
        summary,
        tail,
        captured_lines: source_lines,
        truncated,
    }
}

/// Build the whole-run `kept` log (`--keep-logs`) after a green run.
fn build_kept_log(trace: &RunTrace, headline: &str) -> GateRunLog {
    let (tail, truncated) = trace.global.last(TAIL_LINES, TAIL_CAP_BYTES);
    GateRunLog {
        step: "*".into(),
        kind: "kept".into(),
        attempts: 0,
        headline: headline.to_owned(),
        summary: Vec::new(),
        tail,
        captured_lines: trace.global.seen_lines,
        truncated,
    }
}

/// Attach the bounded log to a finished verdict: a failed step's evidence on
/// failure, a runner-level log when the gate died without one, or a `kept`
/// whole-run log on a green `--keep-logs` run. Passing runs stay cheap.
fn attach_failure_evidence(
    result: &mut GateRunResult,
    trace: &Arc<std::sync::Mutex<RunTrace>>,
    keep_logs: bool,
) {
    let captured = trace.lock().unwrap_or_else(|poison| poison.into_inner());
    if matches!(result.status.as_str(), "passed" | "landed") {
        if keep_logs {
            result.run_log = Some(build_kept_log(
                &captured,
                &format!("{}; log retained by --keep-logs", result.status),
            ));
        }
        return;
    }
    if result.status == "base-moved" || result.status == "stale-tip" {
        return;
    }
    if let Some(step) = result
        .steps
        .iter()
        .rev()
        .find(|step| step.status == "failed")
    {
        let log = build_failure_log(
            &captured,
            &step.name,
            step.attempts,
            step.error.as_deref(),
            None,
        );
        result.failed_step = Some(step.name.clone());
        result.reason = Some(log.headline.clone());
        result.run_log = Some(log);
    } else if result.status == "failed" {
        let step = captured.current.clone().unwrap_or_else(|| "gate".into());
        let log = build_failure_log(&captured, &step, 0, None, result.error.as_deref());
        result.failed_step = Some(step);
        result.reason = Some(log.headline.clone());
        result.run_log = Some(log);
    }
}

/// Ensure every failed verdict carries a one-line `reason` (first error line)
/// even when the failure happened before the gate produced step evidence.
fn finalize(mut result: GateRunResult) -> GateRunResult {
    if result.reason.is_none()
        && let Some(error) = &result.error
    {
        result.reason = Some(first_line(error));
    }
    result
}

/// The step whose marker was seen last in a captured trace.
fn current_step(trace: &Arc<std::sync::Mutex<RunTrace>>) -> Option<String> {
    trace
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .current
        .clone()
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

    /// Handle `gate.land` — the home host fetches a lane's verified merge and
    /// compare-and-swap pushes the base branch.
    pub(crate) async fn run_gate_land(&self, params: &Value) -> Result<Value, NodeError> {
        let request: GateLandParams = serde_json::from_value(params.clone()).map_err(|error| {
            NodeError::InvalidRequest(format!("invalid gate.land params: {error}"))
        })?;
        let result = self.run_gate_land_typed(request).await?;
        serde_json::to_value(result).map_err(NodeError::from)
    }

    /// Handle `gate.unpin` — drop a job's pinned merge refs on a lane host.
    pub(crate) async fn run_gate_unpin(&self, params: &Value) -> Result<Value, NodeError> {
        let request: GateUnpinParams = serde_json::from_value(params.clone()).map_err(|error| {
            NodeError::InvalidRequest(format!("invalid gate.unpin params: {error}"))
        })?;
        let repo = PathBuf::from(&request.repo_path);
        let job_id = request.job_id.clone();
        let removed = tokio::task::spawn_blocking(move || unpin_verified_merge(&repo, &job_id))
            .await
            .map_err(|error| NodeError::InvalidRequest(format!("unpin join: {error}")))?;
        serde_json::to_value(GateUnpinResult {
            job_id: request.job_id,
            removed,
        })
        .map_err(NodeError::from)
    }

    /// The `pushFrom: home` land: obtain the verified merge commit from the
    /// lane repo, then advance the remote base branch under a lease.
    ///
    /// The lease is what makes this safe to run while other coordinators are
    /// landing: `--force-with-lease=<base>:<baseSha>` is a server-side
    /// compare-and-swap, so a base that moved refuses the whole push rather
    /// than fast-forwarding partway. Nothing local is mutated on refusal, and
    /// the caller turns that into a re-queued verify (D-034).
    async fn run_gate_land_typed(
        &self,
        request: GateLandParams,
    ) -> Result<GateLandResult, NodeError> {
        let timeout = if request.timeout_secs > 0 {
            request.timeout_secs
        } else {
            DEFAULT_LAND_TIMEOUT_SECS
        };
        let job_id = request.job_id.clone();
        let landed = tokio::time::timeout(
            Duration::from_secs(timeout),
            tokio::task::spawn_blocking(move || land_from_home(&request)),
        )
        .await
        .map_err(|_| NodeError::InvalidRequest(format!("gate.land timed out after {timeout}s")))?
        .map_err(|error| NodeError::InvalidRequest(format!("gate.land join: {error}")))?;
        Ok(match landed {
            Ok(result) => result,
            Err(error) => GateLandResult {
                job_id,
                status: "failed".into(),
                error: Some(error),
                ..Default::default()
            },
        })
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
                result: Box::new(outcome.clone()),
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
            return finalize(result);
        }
        if let Err(error) = sync_local_main(&repo, &request.base_branch) {
            result.error = Some(format!("sync {base}: {error}", base = request.base_branch));
            return finalize(result);
        }

        // 2. Fast-forward the branch tip, refusing a stale local tip when a
        //    worker worktree holds the branch and has diverged.
        emit(GateEventKind::Phase { phase: "ff".into() });
        if let Err(error) = block_git(&repo, &["fetch", "-q", "origin", &request.branch]) {
            // The branch may already exist locally; fetch failure is only
            // fatal when the branch is absent.
            if block_git(&repo, &["rev-parse", "--verify", &request.branch]).is_err() {
                result.error = Some(format!("fetch branch: {error}"));
                return finalize(result);
            }
        }
        if let Err(error) = fast_forward_branch(&repo, &request.branch) {
            result.status = "stale-tip".into();
            result.error = Some(error);
            return finalize(result);
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
        //
        // `pushFrom: home` is the exception: this host holds no push
        // credential, so the lane must stay a pure verify. Running `--land`
        // here would advance lane-local main and report `landed` with nothing
        // on the remote — the half-updated outcome D-034 forbids. The Hub
        // lands the pinned merge from the home host instead.
        let lands_here =
            request.mode == "land" && request.push_from != remuda_protocol::GatePushFrom::Home;
        args.push("--gate".into());
        if lands_here {
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
        let trace = Arc::new(std::sync::Mutex::new(RunTrace::new()));
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                result.error = Some(format!("spawn {binary}: {error}"));
                return finalize(result);
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
        let mut stderr_task: Option<tokio::task::JoinHandle<()>> = None;
        if let Some(stderr) = child.stderr.take() {
            let node = self.clone();
            let job_id = request.job_id.clone();
            let trace = trace.clone();
            stderr_task = Some(tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    {
                        let mut captured =
                            trace.lock().unwrap_or_else(|poison| poison.into_inner());
                        captured.push(line.clone());
                    }
                    let _ = node.gate_registry().events.send(GateEventParams {
                        job_id: job_id.clone(),
                        kind: GateEventKind::Log { message: line },
                    });
                }
            }));
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
            if let Some(task) = stderr_task.take() {
                task.abort();
            }
            result.status = "canceled".into();
            result.error = Some("canceled by request; killed step process group".into());
            return result;
        }
        if timed_out {
            if let Some(task) = stderr_task.take() {
                task.abort();
            }
            let error = format!("gate run exceeded {deadline}s; killed step process group");
            let step = current_step(&trace).unwrap_or_else(|| "gate".into());
            let log = build_failure_log(
                &trace.lock().unwrap_or_else(|poison| poison.into_inner()),
                &step,
                0,
                None,
                Some(&error),
            );
            result.failed_step = Some(step);
            result.reason = Some(log.headline.clone());
            result.run_log = Some(log);
            result.error = Some(error);
            return finalize(result);
        }
        let Some(output) = output else {
            result.error = Some("gate produced no output".into());
            return finalize(result);
        };
        // Let the stderr pump drain the last lines (EOF follows child exit).
        if let Some(task) = stderr_task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        }

        let report: Value = match serde_json::from_slice(&output.stdout) {
            Ok(value) => value,
            Err(error) => {
                let message = format!("parse merge report: {error}");
                let step = current_step(&trace).unwrap_or_else(|| "gate".into());
                let log = build_failure_log(&trace.lock().unwrap(), &step, 0, None, Some(&message));
                result.failed_step = Some(step);
                result.reason = Some(log.headline.clone());
                result.run_log = Some(log);
                result.error = Some(message);
                return finalize(result);
            }
        };
        result.steps = report
            .get("steps")
            .and_then(Value::as_array)
            .map(|steps| {
                steps
                    .iter()
                    .filter_map(|step| serde_json::from_value::<GateStep>(step.clone()).ok())
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
        // Persist a passing verify before the scratch worktree is removed: the
        // merge commit is otherwise a dangling object in this repo and dies
        // with the next gc, which is exactly how a green run became unlandable
        // (evidence: gate-lane-2). The refs are the handoff to a home-host
        // land, and the Hub drops them on cancel, after a land, or by
        // retention.
        if result.status == "passed"
            && let Some(merge_sha) = result.merge_sha.clone()
        {
            match pin_verified_merge(
                &repo,
                &request.job_id,
                &merge_sha,
                result.head_sha.as_deref(),
            ) {
                Ok(merge_ref) => {
                    emit(GateEventKind::Phase {
                        phase: "pin".into(),
                    });
                    result.merge_ref = Some(merge_ref);
                }
                Err(error) => {
                    // A land needs the pin: without it there is nothing for
                    // the home host to fetch, so fail rather than report a
                    // pass that cannot be pushed.
                    if request.mode == "land" {
                        result.status = "failed".into();
                        result.error = Some(format!("pin verified merge: {error}"));
                    } else {
                        // A verify's green signal is still true. Leaving
                        // mergeRef absent already says "not landable" — the
                        // job simply has to be re-verified to be landed.
                        tracing::warn!(
                            %error,
                            job_id = request.job_id,
                            "could not pin the verified merge; the pass is not landable"
                        );
                    }
                }
            }
        }
        attach_failure_evidence(&mut result, &trace, request.keep_logs);
        finalize(result)
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

/// The home-host land, run on a blocking thread.
///
/// Order matters: fetch the merge object, re-check the parentage the Hub
/// asserted, then push under a lease. Every refusal returns before anything is
/// pushed, so `main` is never left half-updated.
fn land_from_home(request: &GateLandParams) -> Result<GateLandResult, String> {
    let repo = PathBuf::from(&request.repo_path);
    let mut log = String::new();
    let job_id = request.job_id.clone();

    // 1. Bring the verified merge commit over from the lane repo. For this
    //    slice that is a plain fetch of the pinned ref over the operator's
    //    existing remote; a Node RPC streaming the packfile would replace this
    //    call alone, which is why nothing below depends on how it arrived.
    let local_ref = format!("refs/remuda/gate/incoming/{job_id}");
    let refspec = format!("+{}:{}", request.merge_ref, local_ref);
    log.push_str(&format!("git fetch {} {refspec}\n", request.fetch_remote));
    run_git(
        &repo,
        &["fetch", "--no-tags", &request.fetch_remote, &refspec],
    )
    .map_err(|error| format!("fetch {} from lane: {error}", request.merge_ref))?;

    // 2. The fetched ref must be exactly the sha that passed the gate. A
    //    mismatch means the lane re-pinned under us; refuse rather than push
    //    an unverified commit.
    let fetched = git_output(&repo, &["rev-parse", "--verify", &local_ref])
        .map_err(|error| format!("resolve fetched {local_ref}: {error}"))?;
    if fetched != request.merge_sha {
        let _ = run_git(&repo, &["update-ref", "-d", &local_ref]);
        return Err(format!(
            "lane ref {} is {fetched}, but the verified merge is {}",
            request.merge_ref, request.merge_sha
        ));
    }

    // 3. Re-derive the merge's first parent here. The Hub passed baseSha, but
    //    the push guard must come from the object itself.
    let first_parent = git_output(&repo, &["rev-parse", "--verify", &format!("{fetched}^1")])
        .map_err(|error| format!("resolve first parent of {fetched}: {error}"))?;
    if first_parent != request.base_sha {
        let _ = run_git(&repo, &["update-ref", "-d", &local_ref]);
        return Err(format!(
            "verified merge {fetched} has first parent {first_parent}, not the verified base {}",
            request.base_sha
        ));
    }

    // 4. Compare the remote's current base branch before pushing. This is an
    //    early, clearer BaseMoved than the lease refusal alone would give.
    let remote_base = git_output(
        &repo,
        &[
            "ls-remote",
            "--exit-code",
            &request.push_remote,
            &format!("refs/heads/{}", request.base_branch),
        ],
    )
    .map_err(|error| {
        format!(
            "read {}/{}: {error}",
            request.push_remote, request.base_branch
        )
    })?;
    let remote_sha = remote_base
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();
    if remote_sha != request.base_sha {
        let _ = run_git(&repo, &["update-ref", "-d", &local_ref]);
        log.push_str(&format!(
            "{}/{} is {remote_sha}, verified base {} — refusing\n",
            request.push_remote, request.base_branch, request.base_sha
        ));
        return Ok(GateLandResult {
            job_id,
            status: "base-moved".into(),
            current_main_sha: Some(remote_sha),
            output: Some(log),
            ..Default::default()
        });
    }

    // 5. Push under a lease so the remote itself does the compare-and-swap:
    //    if the base moved between the check above and this call, the push is
    //    rejected whole and the remote stays exactly where it was. Never
    //    --force.
    let lease = format!(
        "--force-with-lease=refs/heads/{}:{}",
        request.base_branch, request.base_sha
    );
    let target = format!("{fetched}:refs/heads/{}", request.base_branch);
    log.push_str(&format!(
        "git push {lease} {} {target}\n",
        request.push_remote
    ));
    let pushed = git_output(&repo, &["push", &lease, &request.push_remote, &target]);
    let _ = run_git(&repo, &["update-ref", "-d", &local_ref]);
    match pushed {
        Ok(output) => {
            log.push_str(&output);
            Ok(GateLandResult {
                job_id,
                status: "landed".into(),
                merge_sha: Some(fetched),
                output: Some(log),
                ..Default::default()
            })
        }
        Err(error) => {
            // A lost lease is a moved base, not a failure: re-read the remote
            // to report where it went, and let the Hub re-queue a verify.
            let now = git_output(
                &repo,
                &[
                    "ls-remote",
                    &request.push_remote,
                    &format!("refs/heads/{}", request.base_branch),
                ],
            )
            .ok()
            .and_then(|line| line.split_whitespace().next().map(str::to_owned));
            log.push_str(&error);
            let stale = error.contains("stale info")
                || error.contains("fetch first")
                || error.contains("non-fast-forward")
                || error.contains("rejected");
            if stale && now.as_deref() != Some(request.base_sha.as_str()) {
                return Ok(GateLandResult {
                    job_id,
                    status: "base-moved".into(),
                    current_main_sha: now,
                    output: Some(log),
                    ..Default::default()
                });
            }
            Ok(GateLandResult {
                job_id,
                status: "failed".into(),
                current_main_sha: now,
                error: Some(error),
                output: Some(log),
                ..Default::default()
            })
        }
    }
}

/// Pin a passing verify's merge commit (and the verified branch tip) in the
/// lane repo so both outlive the scratch worktree.
///
/// The merge ref is `refs/remuda/gate/<job id>`; the branch tip rides a
/// `.branch` sibling because git refuses a ref that is simultaneously a file
/// and a directory. Both are verified to resolve after writing: a silent
/// no-op here would surface much later as an unlandable pass.
fn pin_verified_merge(
    repo: &Path,
    job_id: &str,
    merge_sha: &str,
    head_sha: Option<&str>,
) -> Result<String, String> {
    let merge_ref = remuda_protocol::gate_merge_ref(job_id);
    // Refuse to pin a commit this repo does not actually have, so the ref can
    // never advertise an object a later fetch cannot serve.
    run_git(
        repo,
        &["cat-file", "-e", &format!("{merge_sha}^{{commit}}")],
    )
    .map_err(|error| format!("merge commit {merge_sha} is not in the lane repo: {error}"))?;
    run_git(repo, &["update-ref", &merge_ref, merge_sha])?;
    let pinned = git_output(repo, &["rev-parse", "--verify", &merge_ref])?;
    if pinned != merge_sha {
        return Err(format!(
            "{merge_ref} resolved to {pinned}, expected {merge_sha}"
        ));
    }
    if let Some(head) = head_sha.filter(|sha| !sha.is_empty()) {
        let branch_ref = remuda_protocol::gate_branch_ref(job_id);
        run_git(repo, &["update-ref", &branch_ref, head])?;
    }
    Ok(merge_ref)
}

/// Drop a job's pinned refs (`gate.unpin`): cancel, post-land, or retention.
fn unpin_verified_merge(repo: &Path, job_id: &str) -> Vec<String> {
    let mut removed = Vec::new();
    for reference in [
        remuda_protocol::gate_merge_ref(job_id),
        remuda_protocol::gate_branch_ref(job_id),
    ] {
        // Only delete a ref that exists; -d on a missing ref is an error, and
        // unpin has to stay idempotent (the Hub may retry it).
        if git_output(repo, &["rev-parse", "--verify", "--quiet", &reference]).is_ok()
            && run_git(repo, &["update-ref", "-d", &reference]).is_ok()
        {
            removed.push(reference);
        }
    }
    removed
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
            push_from: remuda_protocol::GatePushFrom::default(),
            keep_logs: false,
            binary: Some(fixture.bin.to_string_lossy().into_owned()),
        }
    }

    /// Like PASSING_SCRIPT, but builds a *real* merge commit in the lane repo
    /// the way `merge --onto --gate` does — in a scratch worktree that is then
    /// removed — and reports its real sha. The commit is left unreferenced, so
    /// only the runner's own pin can keep it alive.
    const PASSING_REAL_MERGE_SCRIPT: &str = r#"
scratch=$(mktemp -d "/tmp/remuda-mq-fake.XXXXXX")
echo '{"name":"cargo-test","status":"ok","durationMs":22,"attempts":1,"retried":false}' > "$scratch/gate.jsonl"
base=$(git rev-parse main)
# The shared fixture's branch points at main, so give it a commit of its own;
# without one `git merge` is a fast-forward no-op and builds no merge commit.
tip="$scratch/tip"
git worktree add -q "$tip" wt/fake/task
echo work > "$tip/branch.txt"
git -C "$tip" add branch.txt
git -C "$tip" -c user.email=t@example.com -c user.name=T commit -q -m "branch work"
git worktree remove --force "$tip"
head=$(git rev-parse wt/fake/task)
tree="$scratch/worktree"
git worktree add -q --detach "$tree" "$base"
git -C "$tree" -c user.email=t@example.com -c user.name=T merge -q --no-ff --no-edit -m "merge: fake" "$head"
merged=$(git -C "$tree" rev-parse HEAD)
git worktree remove --force "$tree"
cat <<JSON
{"exitCode":0,"status":"verified","branch":"wt/fake/task","base":"$base","head":"$head","merged":"$merged","steps":[{"name":"cargo-test","status":"ok","durationMs":22}]}
JSON
"#;

    const PASSING_SCRIPT: &str = r#"
scratch=$(mktemp -d "/tmp/remuda-mq-fake.XXXXXX")
echo '{"name":"secret-scan","status":"ok","durationMs":11,"attempts":1,"retried":false}' > "$scratch/gate.jsonl"
echo '{"name":"cargo-test","status":"ok","durationMs":22,"attempts":1,"retried":false}' >> "$scratch/gate.jsonl"
echo 'gate: secret-scan' >&2
echo 'secret scan clean' >&2
echo 'gate: cargo-test' >&2
echo 'test result: ok. 312 passed; 0 failed' >&2
sleep 0.6
cat <<'JSON'
{"exitCode":0,"status":"verified","branch":"wt/fake/task","base":"1111111111111111111111111111111111111111","head":"2222222222222222222222222222222222222222","merged":"3333333333333333333333333333333333333333","steps":[{"name":"secret-scan","status":"ok","durationMs":11},{"name":"cargo-test","status":"ok","durationMs":22}]}
JSON
"#;

    const FAILING_CARGO_TEST_SCRIPT: &str = r#"
set +e
scratch=$(mktemp -d "/tmp/remuda-mq-fake.XXXXXX")
echo '{"name":"secret-scan","status":"ok","durationMs":11,"attempts":1,"retried":false}' > "$scratch/gate.jsonl"
emit_fail() {
    echo 'gate: cargo-test' >&2
    echo 'running 2 tests' >&2
    echo 'test remuda::works ... ok' >&2
    echo 'test remuda::gate::boom ... FAILED' >&2
    echo 'test remuda::gate::other ... FAILED' >&2
    echo '' >&2
    echo 'failures:' >&2
    echo '' >&2
    echo '---- remuda::gate::boom stdout ----' >&2
    echo 'thread "remuda::gate::boom" panicked at crates/remuda-node/src/gate.rs:42:9:' >&2
    echo 'assertion `left == right` failed' >&2
    echo 'note: run with `RUST_BACKTRACE=1` environment variable' >&2
    echo '' >&2
    echo '' >&2
    echo 'failures:' >&2
    echo '    remuda::gate::boom' >&2
    echo '    remuda::gate::other' >&2
    echo '' >&2
    echo 'test result: FAILED. 0 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out' >&2
    echo 'error: test failed, to rerun pass `-p remuda-node --lib gate`' >&2
}
emit_fail
echo 'gate: cargo-test (retried)' >&2
emit_fail
echo '{"name":"cargo-test","status":"failed","durationMs":5012,"attempts":2,"retried":true,"error":"exit status 101"}' >> "$scratch/gate.jsonl"
sleep 0.6
cat <<'JSON'
{"exitCode":1,"status":"gate_failed","error":"gate failed or returned an incomplete step report","base":"1111111111111111111111111111111111111111","head":"2222222222222222222222222222222222222222","steps":[{"name":"secret-scan","status":"ok","durationMs":11},{"name":"cargo-test","status":"failed","durationMs":5012,"attempts":2,"retried":true,"error":"exit status 101"}]}
JSON
"#;

    /// A passing verify must leave the merge commit reachable in the lane repo
    /// after the scratch worktree is gone, with `main` as its first parent —
    /// the property a later home-host land depends on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn passing_verify_pins_the_merge_at_a_ref_whose_first_parent_is_main() {
        let fixture = fixture(PASSING_REAL_MERGE_SCRIPT);
        let result = fixture
            .node
            .run_gate_typed(params(&fixture, "verify"))
            .await
            .unwrap();
        assert_eq!(
            result.status,
            "passed",
            "{}",
            result.error.clone().unwrap_or_default()
        );

        let merge_ref = result.merge_ref.expect("a passing verify reports mergeRef");
        assert_eq!(merge_ref, "refs/remuda/gate/gjb_test");
        let merge_sha = result.merge_sha.expect("a passing verify reports mergeSha");

        // The ref resolves to exactly the verified merge…
        let pinned = git(&fixture.lane, &["rev-parse", "--verify", &merge_ref]);
        assert_eq!(pinned, merge_sha, "{merge_ref} must pin the verified merge");
        // …the commit really is a merge of main and the branch…
        let first_parent = git(&fixture.lane, &["rev-parse", &format!("{merge_sha}^1")]);
        let main = git(&fixture.lane, &["rev-parse", "main"]);
        assert_eq!(
            first_parent, main,
            "the pinned merge's first parent must be main"
        );
        let second_parent = git(&fixture.lane, &["rev-parse", &format!("{merge_sha}^2")]);
        assert_eq!(
            second_parent,
            git(&fixture.lane, &["rev-parse", "wt/fake/task"]),
            "the pinned merge must contain the verified branch tip"
        );
        // …and the branch tip is pinned on the sibling ref.
        assert_eq!(
            git(
                &fixture.lane,
                &["rev-parse", "--verify", "refs/remuda/gate/gjb_test.branch"]
            ),
            second_parent,
            "the verified branch tip must be pinned too"
        );
        // The scratch worktree is gone, so the pin is the only thing keeping
        // the merge alive: prove it survives a prune+gc.
        assert!(
            !git(&fixture.lane, &["worktree", "list"]).contains("remuda-mq-fake"),
            "the fake merge must have removed its scratch worktree"
        );
        git(&fixture.lane, &["worktree", "prune"]);
        git(&fixture.lane, &["gc", "--prune=now", "--quiet"]);
        assert_eq!(
            git(&fixture.lane, &["rev-parse", "--verify", &merge_ref]),
            merge_sha,
            "the pinned merge must survive gc"
        );
    }

    /// `gate.unpin` drops both refs and is safe to call twice.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unpin_removes_both_refs_and_is_idempotent() {
        let fixture = fixture(PASSING_REAL_MERGE_SCRIPT);
        fixture
            .node
            .run_gate_typed(params(&fixture, "verify"))
            .await
            .unwrap();
        let unpin = serde_json::json!({
            "jobId": "gjb_test",
            "repoPath": fixture.lane.to_string_lossy(),
        });
        let first = fixture.node.run_gate_unpin(&unpin).await.unwrap();
        let removed = first["removed"].as_array().cloned().unwrap_or_default();
        assert_eq!(removed.len(), 2, "both refs are dropped: {first}");
        assert!(
            git_output(
                &fixture.lane,
                &["rev-parse", "--verify", "refs/remuda/gate/gjb_test"]
            )
            .is_err(),
            "the merge ref must be gone"
        );
        // A retry (the Hub may repeat it) reports nothing left, never an error.
        let second = fixture.node.run_gate_unpin(&unpin).await.unwrap();
        assert_eq!(
            second["removed"].as_array().map(Vec::len),
            Some(0),
            "unpin must be idempotent: {second}"
        );
    }

    /// The home-host land: fetch the lane's pinned merge over `fetchRemote`,
    /// then push it. `main` must move exactly once, and a base that moved
    /// under us must be refused as base-moved with nothing pushed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn home_host_land_pushes_once_and_refuses_a_moved_base() {
        let fixture = fixture(PASSING_REAL_MERGE_SCRIPT);
        let verified = fixture
            .node
            .run_gate_typed(params(&fixture, "verify"))
            .await
            .unwrap();
        assert_eq!(verified.status, "passed");
        let merge_sha = verified.merge_sha.clone().unwrap();
        let base_sha = verified.base_sha.clone().unwrap();

        // A separate home-host checkout of the same origin, with the lane repo
        // reachable as a plain remote (stands in for the operator's ssh alias).
        let home = fixture._dir.path().join("home");
        let origin = fixture._bare.path().join("origin.git");
        git(
            fixture._dir.path(),
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                home.to_str().unwrap(),
            ],
        );
        git(&home, &["config", "user.email", "t@example.com"]);
        git(&home, &["config", "user.name", "T"]);
        git(
            &home,
            &["remote", "add", "lane", fixture.lane.to_str().unwrap()],
        );

        let land = |base: &str| {
            serde_json::json!({
                "jobId": "gjb_test",
                "repoPath": home.to_string_lossy(),
                "branch": "wt/fake/task",
                "baseBranch": "main",
                "pushRemote": "origin",
                "fetchRemote": "lane",
                "mergeRef": "refs/remuda/gate/gjb_test",
                "mergeSha": merge_sha.clone(),
                "baseSha": base.to_owned(),
            })
        };

        // The lane host itself never pushed: origin/main is still the base.
        assert_eq!(git(&home, &["rev-parse", "origin/main"]), base_sha);

        let result = fixture.node.run_gate_land(&land(&base_sha)).await.unwrap();
        assert_eq!(
            result["status"].as_str(),
            Some("landed"),
            "land should push: {result}"
        );
        let origin_main = git(&origin, &["rev-parse", "main"]);
        assert_eq!(
            origin_main, merge_sha,
            "origin/main must be exactly the verified merge"
        );

        // Landing again with the now-stale base must refuse and leave main be.
        let again = fixture.node.run_gate_land(&land(&base_sha)).await.unwrap();
        assert_eq!(
            again["status"].as_str(),
            Some("base-moved"),
            "a moved base must refuse: {again}"
        );
        assert_eq!(
            again["currentMainSha"].as_str(),
            Some(origin_main.as_str()),
            "the refusal reports where main actually is"
        );
        assert_eq!(
            git(&origin, &["rev-parse", "main"]),
            origin_main,
            "main must move exactly once"
        );
    }

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
    async fn failed_cargo_test_carries_the_extracted_summary() {
        let fixture = fixture(FAILING_CARGO_TEST_SCRIPT);
        let result = fixture
            .node
            .run_gate_typed(params(&fixture, "verify"))
            .await
            .unwrap();
        assert_eq!(result.status, "failed", "{result:?}");
        assert_eq!(result.failed_step.as_deref(), Some("cargo-test"));
        let log = result.run_log.expect("failed run carries a bounded log");
        assert_eq!(log.step, "cargo-test");
        assert_eq!(log.kind, "failed");
        assert_eq!(log.attempts, 2);
        assert!(
            log.headline.contains("cargo-test failed")
                && log.headline.contains("exit status 101")
                && log.headline.contains("attempts 2"),
            "headline: {}",
            log.headline
        );
        assert_eq!(result.reason.as_deref(), Some(log.headline.as_str()));
        // The extracted summary names the failing tests and the panic site.
        let summary = log.summary.join("\n");
        assert!(
            summary.contains("test remuda::gate::boom ... FAILED"),
            "summary:\n{summary}"
        );
        assert!(
            summary.contains("panicked at crates/remuda-node/src/gate.rs:42:9:"),
            "summary:\n{summary}"
        );
        assert!(summary.contains("failures:"), "summary:\n{summary}");
        // The bounded tail keeps the end of the run, including the libtest
        // verdict and the cargo rerun hint.
        let tail = log.tail.join("\n");
        assert!(tail.contains("test result: FAILED"), "tail:\n{tail}");
        assert!(
            tail.contains("error: test failed, to rerun"),
            "tail:\n{tail}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn passing_run_keeps_no_log_without_keep_logs() {
        let fixture = fixture(PASSING_SCRIPT);
        let result = fixture
            .node
            .run_gate_typed(params(&fixture, "verify"))
            .await
            .unwrap();
        assert_eq!(result.status, "passed");
        assert!(
            result.run_log.is_none(),
            "green runs stay cheap: {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn keep_logs_retains_a_green_run_log() {
        let fixture = fixture(PASSING_SCRIPT);
        let mut params = params(&fixture, "verify");
        params.keep_logs = true;
        let result = fixture.node.run_gate_typed(params).await.unwrap();
        assert_eq!(result.status, "passed");
        let log = result.run_log.expect("--keep-logs retains the tail");
        assert_eq!(log.kind, "kept");
        assert_eq!(log.step, "*");
        assert!(log.headline.contains("--keep-logs"));
        assert!(!log.tail.is_empty());
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

    /// A `pushFrom: home` land must stay a verify on the lane. Passing --land
    /// here would move lane-local main and report `landed` while nothing
    /// reached the remote — the half-updated outcome D-034 forbids.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn push_from_home_keeps_the_lane_a_verify() {
        let script = r#"
echo "$@" > "$(dirname "$0")/args.txt"
base=$(git rev-parse main)
cat <<JSON
{"exitCode":0,"status":"verified","base":"$base","head":"$base","merged":"$base","steps":[]}
JSON
"#;
        let fixture = fixture(script);
        let mut p = params(&fixture, "land");
        p.push = true;
        p.push_from = remuda_protocol::GatePushFrom::Home;
        let result = fixture.node.run_gate_typed(p).await.unwrap();
        // The lane reports a pass, not a land: the home host does the push.
        assert_eq!(result.status, "passed", "{result:?}");
        let argv = std::fs::read_to_string(fixture.bin.parent().unwrap().join("args.txt")).unwrap();
        assert!(
            !argv.contains("--land"),
            "a credential-less lane must never run --land: {argv}"
        );
        assert!(
            argv.contains("--no-push"),
            "the lane must be told not to push: {argv}"
        );
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

    #[test]
    fn step_markers_match_only_attempt_banners() {
        assert_eq!(
            super::parse_step_marker("gate: cargo-test"),
            Some(("cargo-test".into(), false))
        );
        assert_eq!(
            super::parse_step_marker("gate: web-hub-e2e (retried)"),
            Some(("web-hub-e2e".into(), true))
        );
        // Detail/banner lines contain a colon and stay attributed to the step.
        assert!(super::parse_step_marker("gate: cargo-test: timed out").is_none());
        assert!(super::parse_step_marker("gate: Rust tests: remuda (full)").is_none());
        assert!(super::parse_step_marker("  gate: cargo-test").is_none());
        assert!(super::parse_step_marker("gate: Cargo-Test").is_none());
    }

    #[test]
    fn cargo_test_failure_summary_extracts_names_panics_and_section() {
        let lines: Vec<String> = r"
running 3 tests
test ok::one ... ok
test bad::boom ... FAILED
test bad::other ... FAILED

failures:

---- bad::boom stdout ----
thread 'bad::boom' panicked at src/lib.rs:9:5:
assertion failed: `(left == right)`

failures:
    bad::boom
    bad::other

test result: FAILED. 1 passed; 2 failed"
            .trim()
            .lines()
            .map(str::to_owned)
            .collect();
        let summary = super::extract_summary("cargo-test", &lines);
        let text = summary.join("\n");
        assert!(text.contains("failures:"));
        assert!(text.contains("bad::boom stdout"));
        assert!(text.contains("panicked at src/lib.rs:9:5:"));
        assert!(text.contains("test bad::boom ... FAILED"));
        assert!(!text.contains("test ok::one ... ok"));
        // The standalone FAILED lines are appended once each; the panic line
        // already lives inside the section and is not duplicated.
        for needle in [
            "test bad::boom ... FAILED",
            "test bad::other ... FAILED",
            "panicked at src/lib.rs:9:5:",
        ] {
            assert_eq!(
                summary.iter().filter(|line| line.contains(needle)).count(),
                1,
                "duplicated summary line {needle}: {summary:?}"
            );
        }
    }

    #[test]
    fn playwright_summary_extracts_titles_and_first_block() {
        let lines: Vec<String> = r"

Running 8 tests across 2 projects

  1) [chromium] › gate.spec.ts:42:1 › failing gate shows evidence
  2) [chromium] › gate.spec.ts:88:3 › gate log prints the tail

  1 failed
"
        .trim()
        .lines()
        .map(str::to_owned)
        .collect();
        let summary = super::extract_summary("web-hub-e2e", &lines);
        assert!(
            summary
                .iter()
                .any(|line| line.contains("failing gate shows evidence")),
            "{summary:?}"
        );
        assert!(
            summary
                .iter()
                .any(|line| line.contains("gate log prints the tail")),
            "{summary:?}"
        );
        assert!(super::extract_summary("secret-scan", &lines).is_empty());
    }

    #[test]
    fn line_ring_bounds_lines_and_bytes_and_reports_drops() {
        let mut ring = super::LineRing::new(4, 10_000);
        for n in 0..10 {
            ring.push(format!("line {n}"));
        }
        assert_eq!(ring.seen_lines, 10);
        assert_eq!(ring.lines.len(), 4);
        let (tail, dropped) = ring.last(100, 10_000);
        assert!(dropped);
        assert_eq!(tail.first().unwrap(), "line 6");
        assert_eq!(tail.last().unwrap(), "line 9");
        // Long lines are clipped before entering the ring.
        let mut ring = super::LineRing::new(8, 100_000);
        ring.push("x".repeat(super::MAX_LINE_CHARS * 3));
        assert!(ring.lines.front().unwrap().chars().count() <= super::MAX_LINE_CHARS);
    }

    #[test]
    fn trace_keeps_per_step_rings_following_markers() {
        let mut trace = super::RunTrace::new();
        for line in [
            "gate: Rust tests: full",
            "gate: cargo-fmt",
            "fmt detail",
            "gate: cargo-test",
            "test bad ... FAILED",
            "gate: cargo-test (retried)",
            "still cargo-test",
        ] {
            trace.push(line.to_owned());
        }
        assert_eq!(trace.current.as_deref(), Some("cargo-test"));
        let fmt = trace.by_step.get("cargo-fmt").unwrap();
        assert_eq!(fmt.seen_lines, 2);
        let test = trace.by_step.get("cargo-test").unwrap();
        assert_eq!(test.seen_lines, 4);
        // The banner line belongs to no step, but stays in the global ring.
        assert!(!trace.by_step.contains_key("Rust tests"));
    }
}
