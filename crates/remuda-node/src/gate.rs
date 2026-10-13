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
    GateAbortLandingParams, GateCancelParams, GateEventKind, GateEventParams, GateLandParams,
    GateLandResult, GateResolveLandingParams, GateResolveLandingResult, GateRunLog, GateRunParams,
    GateRunResult, GateStep, GateThenParams, GateThenResult, GateUnpinParams, GateUnpinResult,
};
use serde_json::{Value, json};
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

/// Gate RPCs that can park a carrier's request loop for minutes and so must be
/// dispatched off the read path (lease-free on the outbound WSS runtime).
///
/// `gate.cancel` is deliberately excluded: it is the escape hatch that kills a
/// running gate, so it must run inline and immediately — a queued cancel is a
/// dead cancel.
///
/// This stays gate-only because the WSS arm that consumes it routes straight to
/// [`DevNode::dispatch_gate_rpc`], which rejects anything that is not a gate
/// method. Carriers that serve frames one at a time on their read loop
/// (ssh-stdio, the daemon controller) use the broader
/// [`is_long_carrier_method`] and the general dispatch entry point instead.
#[must_use]
pub fn is_long_gate_method(method: &str) -> bool {
    is_gate_method(method) && method != remuda_protocol::METHOD_GATE_CANCEL
}

/// Every Hub→Node method whose body can do unbounded blocking work — a gate run
/// or a `--then`, a home-host land or unpin, a worker provision/remove that
/// shells out to `git worktree` and recursively reclaims a cargo target, an
/// instance close, or a worktree/SCM read that shells out to git.
///
/// The single-reader carriers (ssh-stdio at `stdio.rs`, the daemon controller
/// at `daemon.rs`) serve one frame at a time on their read loop, so awaiting any
/// of these inline freezes every outgoing pump beside it — journals, tty frames,
/// gate events and, fatally, `gate.cancel` — until the body returns. Live
/// evidence: a `remuda retire` (worker.remove) or a `remuda worker resume` over
/// a dead instance parked the ssh-stdio loop for good with no gate running.
/// Those carriers spawn every method in this set on its own task and let the
/// reply ride the outgoing channel back to the single writer.
///
/// `gate.cancel` and every cheap read (tty.screen, host.resources, heartbeats,
/// interactions) stay inline: the cancel must run at once to be an escape hatch,
/// and the reads return promptly.
#[must_use]
pub fn is_long_carrier_method(method: &str) -> bool {
    is_long_gate_method(method)
        || crate::worker::is_worker_method(method)
        || method == "instance.close"
        || crate::worktree::is_worktree_method(method)
        || crate::workspace_scm::is_scm_method(method)
}

/// How many long carrier methods may be in flight on one Node at once.
///
/// Off-loop spawning alone is not enough. Several of these bodies used to be
/// synchronous (`provision_worker_typed`, `worktree_rpc`, `handle_rpc`) and the
/// ones that are not still do blocking work, so a spawn is only honest if the
/// work also leaves the async runtime's threads. Without a cap, N concurrent
/// long methods park all N runtime workers — `available_parallelism`, which is
/// one or two on a small remote host — and the carrier loop with them,
/// `gate.cancel` included. That is worse than the serialized inline path this
/// branch removed, which could at least never consume every worker at once.
///
/// Excess long methods wait for a permit rather than piling more blocking work
/// onto the runtime. They are all minutes-long background operations, so
/// queueing is what they expect; the cheap reads and `gate.cancel` are not in
/// this set and are never held back by it.
///
/// Kept well under the smallest realistic runtime width so a burst of long
/// methods can always leave workers for the pumps and cheap reads.
const MAX_IN_FLIGHT_LONG_METHODS: usize = 4;

/// Cap on long methods in flight, process-wide.
///
/// A `OnceLock` rather than a field so the bound holds across every carrier and
/// every dispatch table in the process, including carriers that construct their
/// own `DevNode`. It is a cap on *work*, not on any one caller, so sharing it
/// globally is the point.
fn long_method_permits() -> &'static std::sync::Arc<tokio::sync::Semaphore> {
    static PERMITS: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    PERMITS.get_or_init(|| {
        std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT_LONG_METHODS))
    })
}

/// Run one long carrier method under the in-flight cap, on a blocking thread.
///
/// The carriers call this from their spawned task instead of awaiting the body
/// directly, so a method that blocks (a `git fetch`, a worktree walk, a cargo
/// target delete) cannot occupy a runtime worker thread that a pump or another
/// request needs. `body` is the synchronous part of the method; anything
/// genuinely async stays with the caller.
///
/// The permit is held across the blocking call and released when the body
/// returns (or unwinds), so at most [`MAX_IN_FLIGHT_LONG_METHODS`] of these run
/// at once process-wide. Excess callers wait for a permit *without* occupying a
/// runtime worker, which is what keeps the cap from being worked around by the
/// callers it is meant to bound.
pub(crate) async fn run_long_method<T, B>(body: B) -> Result<T, crate::NodeError>
where
    T: Send + 'static,
    B: FnOnce() -> Result<T, crate::NodeError> + Send + 'static,
{
    let permit = long_method_permits()
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| crate::NodeError::DriverUnavailable)?;
    tokio::task::spawn_blocking(move || {
        let result = body();
        // Released as the body returns — and on unwind, since the permit is a
        // local of this closure.
        drop(permit);
        result
    })
    .await
    .map_err(|error| crate::NodeError::Driver(format!("long carrier method join: {error}")))?
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

/// One live gate run: cancel flag and the process group(s) to kill.
struct LiveRun {
    /// The dispatch generation THIS run executes. A delayed gate.cancel /
    /// gate.abortLanding for a different generation must never signal this
    /// run's groups (r15 item 2): the comparison happens under the registry
    /// lock with the live row in hand.
    generation: u32,
    cancel: watch::Sender<bool>,
    /// Verify child process group.
    pgid: std::sync::Mutex<Option<i32>>,
    /// Granted LAND child process group. `None` until the grant was taken and
    /// the land child spawned. A read-only `gate.resolveLanding` probe treats a
    /// live land group as "the push may still happen" and reports `unknown`
    /// rather than `absent` (r14 R1).
    land_pgid: std::sync::Mutex<Option<i32>>,
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

/// Hard cap on remembered canceled dispatches. This is a MEMORY bound, not
/// an authorization window: a record never expires on its own, so elapsed
/// time can never authorize a stale dispatch. When a NEW job would exceed the
/// cap the cancel is refused (backpressure) rather than silently dropping an
/// older job's authority; an already-tracked job always updates in place.
const MAX_CANCELED_RECORDS: usize = 4096;

/// One generation recorded as canceled for a job.
#[derive(Clone, Copy)]
struct CanceledDispatch {
    /// Highest canceled dispatch generation for the job.
    generation: u32,
}

/// Live runs and canceled-dispatch records, behind ONE lock: the "is this
/// job canceled for this generation?" check in `gate.cancel`/`gate.run` and
/// the "may this run register?" decision must be atomic. Otherwise a cancel
/// can land strictly between the run's lane-slot acquisition and its
/// registration and be answered `ok:true` while the run still executes.
pub(crate) struct GateRunTable {
    /// jobId -> live run handle.
    runs: BTreeMap<String, Arc<LiveRun>>,
    /// jobId -> the highest dispatch generation recorded canceled. A run at
    /// the same or an older generation refuses however late its frame
    /// arrives; a strictly newer generation supersedes (and clears) the
    /// record. Keyed by job count, never by age.
    canceled: BTreeMap<String, CanceledDispatch>,
}

impl GateRunTable {
    fn new() -> Self {
        Self {
            runs: BTreeMap::new(),
            canceled: BTreeMap::new(),
        }
    }

    /// Record `generation` as canceled for `job`. Supersedes an older
    /// generation; keeps the highest. Returns false (and records nothing)
    /// when a NEW job would exceed the job-count cap — the caller must then
    /// refuse/backpressure instead of silently evicting authority.
    fn record_cancel(&mut self, job_id: &str, generation: u32) -> bool {
        if let Some(existing) = self.canceled.get_mut(job_id) {
            existing.generation = existing.generation.max(generation);
            return true;
        }
        if self.canceled.len() >= MAX_CANCELED_RECORDS {
            return false;
        }
        self.canceled
            .insert(job_id.to_owned(), CanceledDispatch { generation });
        true
    }

    /// True when `run_generation` is at or below the canceled generation for
    /// this job — i.e. the dispatch the cancel covered, however delayed.
    fn is_canceled_generation(&self, job_id: &str, run_generation: u32) -> bool {
        self.canceled
            .get(job_id)
            .is_some_and(|record| run_generation <= record.generation)
    }

    /// A strictly newer generation supersedes the cancel: drop its record so
    /// a re-dispatch of the requeued job runs. Returns true when present.
    fn clear_if_superseded(&mut self, job_id: &str, run_generation: u32) -> bool {
        if self
            .canceled
            .get(job_id)
            .is_some_and(|record| run_generation > record.generation)
        {
            self.canceled.remove(job_id);
            true
        } else {
            false
        }
    }
}

/// Answer to a land-grant request at the verify→land boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LandGrantAnswer {
    /// True when the Hub transitioned (or had transitioned) Running → Landing.
    pub granted: bool,
    /// True when a grant was already held (idempotent re-answer).
    pub landing_in_progress: bool,
}

/// Outbound authority checks a lane run asks before executing and at the
/// irreversible land boundary. Registered by the live carrier session;
/// absent on carrier-less/direct callers (tests, foreground stdio), where
/// the Node-local floor alone applies and the Hub still guards the result.
pub trait GateAuthorizer: Send + Sync {
    /// Ask the Hub whether `(job_id, generation)` is still the live Running
    /// attempt. Any error/timeout/missing link must be treated as false.
    fn authorize<'a>(
        &'a self,
        job_id: &'a str,
        generation: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;

    /// Ask the Hub to transition Running → Landing before the push. Any
    /// error/timeout/refusal must be treated as not granted. `merge_sha` is
    /// the verified merge this grant authorizes (recorded on the Landing row).
    fn request_land_grant<'a>(
        &'a self,
        job_id: &'a str,
        generation: u32,
        merge_sha: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = LandGrantAnswer> + Send + 'a>>;
}

/// Build the verdict for a `gate.run` whose generation is at or below the
/// canceled generation, and publish its terminal `finished` event so the Hub
/// converges even if this RPC reply is lost (the same dual verdict a
/// finished run carries).
fn canceled_before_run(registry: &GateRegistry, job_id: &str, generation: u32) -> GateRunResult {
    let why = "canceled before the run registered";
    let result = GateRunResult {
        job_id: job_id.to_owned(),
        generation,
        status: "canceled".into(),
        error: Some(why.to_owned()),
        reason: Some(why.to_owned()),
        ..Default::default()
    };
    let _ = registry.events.send(GateEventParams {
        job_id: job_id.to_owned(),
        generation,
        kind: GateEventKind::Finished {
            result: Box::new(result.clone()),
        },
    });
    result
}

/// r16 item 1 test-only fault for granted-land fence persistence. The fence
/// closure signals `entered`, parks on the `release` condvar (the spawned land
/// child is blocked on its start barrier the whole time), and then fails or
/// succeeds.
#[cfg(any(test, feature = "test-faults"))]
pub struct LandFenceFault {
    /// Signalled once the persistence closure has entered.
    pub(crate) entered: Arc<tokio::sync::Notify>,
    /// Set the bool and notify to release the parked persistence.
    pub(crate) release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    /// Make the persistence FAIL after the hold releases.
    pub(crate) fail: bool,
}

/// Lane locks, running jobs and the per-Node event uplink.
pub(crate) struct GateRegistry {
    /// laneId → one slot; a held slot rejects a second run (`lane-busy`).
    lanes: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    /// Live runs plus canceled-dispatch records (one lock).
    runs: Mutex<GateRunTable>,
    /// Live-carrier hook the run calls to ask the Hub for authority before
    /// executing. Replaced per session; absence always refuses (r12 item 5).
    authorizer: tokio::sync::RwLock<Option<std::sync::Arc<dyn GateAuthorizer>>>,
    /// Test-only holds that park the NEXT gate.run handler at its entry (the
    /// cancel-beats-run race needs the run frame to be in flight, before the
    /// registry and lane are touched, when gate.cancel arrives). One-shot:
    /// parked at the entry and consumed by the first run.
    #[cfg(any(test, feature = "test-faults"))]
    run_holds: Mutex<Vec<Arc<GateRunHold>>>,
    /// Test-only override for the granted land child's wall-clock budget, so
    /// the kill-on-timeout path (r12 item 4) can be exercised without waiting
    /// the production 10 minutes.
    #[cfg(any(test, feature = "test-faults"))]
    land_timeout: Mutex<Option<Duration>>,
    /// Test-only override for the consumed merge/land CLI (`request.binary` is
    /// still preferred). Lets a real-DevNode e2e drive genuine git fetch/merge/
    /// push against a local bare origin with a tiny stand-in CLI while every
    /// authority decision still runs through the real Hub.
    #[cfg(any(test, feature = "test-faults"))]
    merge_bin: Mutex<Option<std::path::PathBuf>>,
    /// r16 item 1 test-only: the NEXT granted-land fence persistence parks
    /// (the spawned land child blocked on its start barrier) until the fault is
    /// released, then fails or succeeds. One-shot.
    #[cfg(any(test, feature = "test-faults"))]
    land_fence_fault: std::sync::Mutex<Option<LandFenceFault>>,
    /// r16 item 1 test-only: force the land child's start-barrier setup to fail
    /// so the "refuse to spawn, report uncertain" arm can be exercised.
    #[cfg(any(test, feature = "test-faults"))]
    force_barrier_fault: std::sync::Mutex<bool>,
    /// Durable ledger directory for granted LAND process groups (r14 R1). One
    /// record per granted land, written right after the land child spawns and
    /// removed only once the whole group is verified gone. A leftover record
    /// at Node boot fences (kills + verifies) the possibly-orphaned group
    /// before any carrier is served; a record present at runtime proves a push
    /// may still land, so a read-only landing probe reports `unknown`.
    land_fence_dir: std::path::PathBuf,
    events: tokio::sync::broadcast::Sender<GateEventParams>,
}

impl GateRegistry {
    /// Construct the registry rooted at a Node data dir, fencing any land
    /// groups orphaned by a previous Node process BEFORE returning. The land
    /// child runs in its own process group with kill_on_drop(false), so a Node
    /// crash would otherwise leave it free to keep pushing while the Hub could
    /// only guess the outcome. The startup fence makes "the land process group
    /// is verified dead" provable across restarts (r14 R1). With no leftover
    /// records — the normal boot — this is one directory read.
    pub(crate) fn open(land_fence_dir: std::path::PathBuf) -> Arc<Self> {
        #[cfg(unix)]
        fence_orphaned_land_groups(&land_fence_dir);
        Self::new(land_fence_dir)
    }

    pub(crate) fn new(land_fence_dir: std::path::PathBuf) -> Arc<Self> {
        let (events, _) = tokio::sync::broadcast::channel(512);
        Arc::new(Self {
            lanes: Mutex::new(BTreeMap::new()),
            runs: Mutex::new(GateRunTable::new()),
            authorizer: tokio::sync::RwLock::new(None),
            #[cfg(any(test, feature = "test-faults"))]
            run_holds: Mutex::new(Vec::new()),
            #[cfg(any(test, feature = "test-faults"))]
            land_timeout: Mutex::new(None),
            #[cfg(any(test, feature = "test-faults"))]
            merge_bin: Mutex::new(None),
            #[cfg(any(test, feature = "test-faults"))]
            land_fence_fault: std::sync::Mutex::new(None),
            #[cfg(any(test, feature = "test-faults"))]
            force_barrier_fault: std::sync::Mutex::new(false),
            land_fence_dir,
            events,
        })
    }

    /// Test-only: override the consumed merge/land CLI.
    #[cfg(any(test, feature = "test-faults"))]
    pub(crate) async fn set_merge_bin(&self, bin: std::path::PathBuf) {
        *self.merge_bin.lock().await = Some(bin);
    }

    /// Resolve the merge/land child binary: the dispatch-provided path, then a
    /// test-only configured CLI, then this Node's own executable.
    async fn merge_binary(&self, provided: &Option<String>) -> String {
        if let Some(binary) = provided {
            return binary.clone();
        }
        #[cfg(any(test, feature = "test-faults"))]
        {
            if let Some(bin) = self.merge_bin.lock().await.clone() {
                return bin.to_string_lossy().into_owned();
            }
        }
        std::env::current_exe()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }

    /// Test-only: override the granted land child timeout (r12 item 4).
    #[cfg(any(test, feature = "test-faults"))]
    pub(crate) async fn set_land_timeout(&self, timeout: Duration) {
        *self.land_timeout.lock().await = Some(timeout);
    }

    /// The granted land child budget: the test override or the production cap.
    async fn land_timeout(&self) -> Duration {
        #[cfg(any(test, feature = "test-faults"))]
        {
            if let Some(timeout) = *self.land_timeout.lock().await {
                return timeout;
            }
        }
        #[allow(unreachable_code)]
        Duration::from_secs(DEFAULT_LAND_TIMEOUT_SECS)
    }

    /// Durably record the granted land child's process group while the child is
    /// still stopped before exec (the child-start barrier, r15 R5). A missing
    /// leader identity or any write failure propagates so the caller refuses to
    /// release (SIGCONT) the land child until the fence is durable.
    #[cfg(unix)]
    fn record_land_group(&self, job_id: &str, generation: u32, pgid: i32) -> std::io::Result<()> {
        // r16 item 1: the injected hold/fail runs while the spawned child is
        // parked on its start barrier, before the real fence write.
        #[cfg(any(test, feature = "test-faults"))]
        self.run_land_fence_fault()?;
        let identity = sample_leader_identity(pgid).ok_or_else(|| {
            std::io::Error::other("could not read the land leader's start-time identity")
        })?;
        write_land_fence(&self.land_fence_dir, job_id, generation, pgid, &identity)
    }

    /// r16 item 1 test-only: arm the one-shot land-fence persistence hold/fail.
    #[cfg(any(test, feature = "test-faults"))]
    pub(crate) fn set_land_fence_fault(&self, fault: LandFenceFault) {
        *self
            .land_fence_fault
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(fault);
    }

    /// r16 item 1 test-only: force the land start-barrier setup to fail.
    #[cfg(any(test, feature = "test-faults"))]
    pub(crate) fn set_force_barrier_fault(&self, on: bool) {
        *self
            .force_barrier_fault
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = on;
    }

    #[cfg(any(test, feature = "test-faults"))]
    fn barrier_setup_fault(&self) -> bool {
        *self
            .force_barrier_fault
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Build the land child's one-shot start barrier (or a Failed variant the
    /// caller refuses to spawn on).
    #[cfg(unix)]
    fn make_land_start_barrier(&self) -> LandStartBarrier {
        #[cfg(any(test, feature = "test-faults"))]
        if self.barrier_setup_fault() {
            return LandStartBarrier::failed(std::io::Error::other(
                "injected land start-barrier setup failure",
            ));
        }
        match LandStartBarrier::new() {
            Ok(barrier) => barrier,
            Err(error) => LandStartBarrier::failed(error),
        }
    }

    /// Run the one-shot fence-persistence fault: signal `entered`, park until
    /// `release`, then fail or pass. Sync-only (the fence closure runs inline
    /// on the async worker), so the latch is a condvar, not an async wait.
    #[cfg(any(test, feature = "test-faults"))]
    fn run_land_fence_fault(&self) -> std::io::Result<()> {
        let Some(fault) = self
            .land_fence_fault
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        else {
            return Ok(());
        };
        fault.entered.notify_one();
        let mut guard = fault
            .release
            .0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while !*guard {
            let (next, wait) = fault
                .release
                .1
                .wait_timeout(guard, Duration::from_secs(30))
                .unwrap_or_else(|poison| poison.into_inner());
            guard = next;
            if wait.timed_out() {
                return Err(std::io::Error::other(
                    "test land-fence persistence hold was never released",
                ));
            }
        }
        drop(guard);
        if fault.fail {
            Err(std::io::Error::other(
                "injected land fence persistence failure",
            ))
        } else {
            Ok(())
        }
    }

    /// The land leader has already been reaped by the caller. Finish off any
    /// surviving group member with SIGKILL, verify the whole group is ESRCH (the
    /// kernel's authoritative answer — never a `/proc` enumeration, r15 R5),
    /// and only then write the durable DEATH CERTIFICATE and remove the fence
    /// record. A later probe with no live run and no fence then attests death
    /// from the certificate rather than guessing from a missing ledger
    /// (r16 item 2, R5). If the cert cannot be written the fence record stays.
    #[cfg(unix)]
    async fn settle_land_group(&self, job_id: &str, pgid: i32) {
        let dir = self.land_fence_dir.clone();
        let job_id = job_id.to_owned();
        let _ = tokio::task::spawn_blocking(move || {
            if land_group_exists(pgid) {
                use nix::sys::signal::{Signal, killpg};
                use nix::unistd::Pid;
                let _ = killpg(Pid::from_raw(pgid), Some(Signal::SIGKILL));
                wait_until_group_gone(pgid, Duration::from_millis(500));
            }
            if land_group_exists(pgid) {
                // ESRCH still not proven (e.g. EPERM, or a zombie under a
                // subreaper): the record stays and the startup fence proves it
                // later. Unknown never releases the proof.
                return;
            }
            // Certify only the EXACT on-disk snapshot for this pgid. A missing
            // or changed fence (a newer attempt re-fenced the job) is never
            // certified from this run's stale pgid observation (r17 item 1).
            let fence_path = land_fence_path(&dir, &job_id);
            let text = match std::fs::read_to_string(&fence_path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(error) => {
                    tracing::warn!(%error, job_id, "settling the land group could not read its fence");
                    return;
                }
            };
            let Ok(record) = serde_json::from_str::<Value>(&text) else {
                tracing::warn!(job_id, "settling the land group found an unreadable fence; keeping it");
                return;
            };
            match certify_dead_and_remove_fence(&dir, &record, pgid) {
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    %error,
                    job_id,
                    "certifying the settled land group failed; keeping the fence"
                ),
            }
        })
        .await;
    }

    /// Whether a granted push for `job_id` can still land, as a tri-state
    /// evidence verdict (r16 item 2, R5):
    /// * [`LandPushLiveness::MayPush`] — a live run in THIS process has a land
    ///   group the kernel still reports present, or the grant may be in flight
    ///   before that group is recorded, or a durable fence ledger exists;
    /// * [`LandPushLiveness::VerifiedDead`] — an AUTHORITATIVE observation that
    ///   the exact attempt's land group is gone: either the live run's recorded
    ///   group now answers ESRCH, or a durable death certificate (written only
    ///   after the kernel verified ESRCH) is on disk;
    /// * [`LandPushLiveness::Unknown`] — everything else, crucially a MISSING
    ///   OR UNREADABLE ledger. No ledger is never proof of death: without a
    ///   death certificate the push remains possible (an older Node, a lost
    ///   record, an unwitnessed crash), so a read-only probe must stay Unknown
    ///   rather than attest `absent`.
    #[cfg(unix)]
    async fn land_push_liveness(&self, job_id: &str) -> LandPushLiveness {
        let table = self.runs.lock().await;
        if let Some(run) = table.runs.get(job_id) {
            let land_pgid = *run
                .land_pgid
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            return match land_pgid {
                // The kernel's authoritative killpg(pgid, 0) answer on the
                // exact group this process spawned and recorded.
                Some(pgid) => {
                    if land_group_exists(pgid) {
                        LandPushLiveness::MayPush
                    } else {
                        LandPushLiveness::VerifiedDead
                    }
                }
                // The run exists and the grant may already have been taken but
                // the land child has not recorded its group yet: conservative.
                None => LandPushLiveness::MayPush,
            };
        }
        drop(table);
        let dir = self.land_fence_dir.clone();
        let job_id = job_id.to_owned();
        tokio::task::spawn_blocking(move || land_ledger_liveness(&dir, &job_id))
            .await
            .unwrap_or(LandPushLiveness::Unknown)
    }

    /// Platforms without process-group accounting can never attest death.
    #[cfg(not(unix))]
    async fn land_push_liveness(&self, _job_id: &str) -> LandPushLiveness {
        LandPushLiveness::Unknown
    }

    /// Register the live session's Hub-authority hook (called on connect).
    pub(crate) async fn set_authorizer(&self, authorizer: std::sync::Arc<dyn GateAuthorizer>) {
        *self.authorizer.write().await = Some(authorizer);
    }

    /// Clear the hook when its session ends: with no link the run must fail
    /// closed rather than authorize against a stale/replacement session.
    pub(crate) async fn clear_authorizer(&self) {
        *self.authorizer.write().await = None;
    }

    /// Ask the Hub whether the dispatch may execute. The ONLY way to get a
    /// yes is a live, registered carrier authorizer returning true within the
    /// bound. No authorizer (a Node that never attached a carrier, or one
    /// whose session dropped), a transport error or a timeout all mean NO —
    /// missing authority is permission, never an allow (r12 item 5). Tests
    /// that need a yes install a test-only authorizer explicitly.
    async fn hub_authorizes(&self, job_id: &str, generation: u32) -> bool {
        let authorizer = self.authorizer.read().await.clone();
        match authorizer {
            Some(authorizer) => tokio::time::timeout(
                Duration::from_secs(5),
                authorizer.authorize(job_id, generation),
            )
            .await
            .unwrap_or(false),
            None => false,
        }
    }

    /// Request the Running → Landing grant at the verify→land boundary. With
    /// no registered carrier the answer is always a refusal (r12 item 5): a
    /// production DevNode that never attached must not fabricate a grant and
    /// push. Tests that need a grant install a test-only authorizer.
    async fn hub_land_grant(
        &self,
        job_id: &str,
        generation: u32,
        merge_sha: &str,
    ) -> LandGrantAnswer {
        let authorizer = self.authorizer.read().await.clone();
        match authorizer {
            Some(authorizer) => tokio::time::timeout(
                Duration::from_secs(5),
                authorizer.request_land_grant(job_id, generation, merge_sha),
            )
            .await
            .unwrap_or(LandGrantAnswer {
                granted: false,
                landing_in_progress: false,
            }),
            None => LandGrantAnswer {
                granted: false,
                landing_in_progress: false,
            },
        }
    }

    /// Test-only: recorded canceled generation for a job, if any.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_canceled_generation(&self, job_id: &str) -> Option<u32> {
        self.runs
            .lock()
            .await
            .canceled
            .get(job_id)
            .map(|record| record.generation)
    }

    /// Test-only: the live run's dispatch generation and whether its stand-down
    /// flag has been set. Lets a regression latch on the PRODUCTION cancel/abort
    /// decision itself (a stale-generation RPC must leave `canceled=false`)
    /// instead of watching a process over a time window (r15 item 2).
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_live_run_state(&self, job_id: &str) -> Option<(u32, bool)> {
        self.runs
            .lock()
            .await
            .runs
            .get(job_id)
            .map(|run| (run.generation, *run.cancel.borrow()))
    }

    /// Test-only: drop all local canceled records, simulating a Node process
    /// restart that begins with an empty registry.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_reset_cancel_records(&self) {
        self.runs.lock().await.canceled.clear();
    }

    /// Test-only: park the next `gate.run` at the handler entry on this hold.
    #[cfg(any(test, feature = "test-faults"))]
    pub(crate) async fn arm_run_hold(&self, hold: Arc<GateRunHold>) {
        self.run_holds.lock().await.push(hold);
    }

    /// Test-only: take and await all armed entry holds for one gate.run,
    /// BEFORE the canceled-generation check and the lane slot.
    #[cfg(any(test, feature = "test-faults"))]
    pub(crate) async fn await_run_holds(&self, job_id: &str, generation: u32) {
        let holds = {
            let mut armed = self.run_holds.lock().await;
            std::mem::take(&mut *armed)
        };
        for hold in holds {
            hold.handler_parked(job_id, generation).await;
        }
    }

    /// Subscribe to Node-originated gate events for carrier pumps.
    pub(crate) fn subscribe(&self) -> tokio::sync::broadcast::Receiver<GateEventParams> {
        self.events.subscribe()
    }
}

/// Test-only gate.run entry hold. The run parks until [`GateRunHold::release`]
/// is called; [`GateRunHold::wait_arrived`] resolves with the dispatched
/// (job id, generation) once the frame has reached the Node handler (claim
/// already committed on the Hub) and is parked before the run registry /
/// lane / git steps.
///
/// Both signals are oneshot channels. A `release()` that happens BEFORE the
/// handler parks still unblocks it — oneshot stores the message for a later
/// `.await`, unlike `Notify::notify_waiters()` whose permit is dropped when
/// no notified future exists yet.
#[doc(hidden)]
#[cfg(any(test, feature = "test-faults"))]
pub struct GateRunHold {
    /// Handler takes the sender on arrival; the test awaits the receiver.
    arrived_tx: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<(String, u32)>>>,
    arrived_rx: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<(String, u32)>>>,
    /// Test takes the sender to release; the handler awaits the receiver.
    release_tx: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release_rx: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

#[cfg(any(test, feature = "test-faults"))]
impl GateRunHold {
    /// Bounded wait for a test-only gate hold to resolve. A parked gate.run
    /// parks for minutes legitimately, but a TEST must never block forever on
    /// a missed arm/arrival: fail loudly instead of hanging the harness.
    #[cfg(any(test, feature = "test-faults"))]
    const HOLD_WAIT: Duration = Duration::from_secs(30);

    /// Called by the run handler: publish arrival, then park until released.
    /// Both happen through oneshot channels, so a release that precedes the
    /// park is already stored and the await returns at once.
    async fn handler_parked(self: &std::sync::Arc<Self>, job_id: &str, generation: u32) {
        if let Some(tx) = self.arrived_tx.lock().expect("arrived tx").take() {
            let _ = tx.send((job_id.to_owned(), generation));
        }
        if let Some(rx) = self.release_rx.lock().await.take() {
            let _ = tokio::time::timeout(Self::HOLD_WAIT, rx)
                .await
                .expect("gate run hold never released within the bounded wait");
        }
    }

    /// Create a disarmed hold; arm it on a Node with
    /// [`DevNode::gate_test_arm_run_hold`].
    #[must_use]
    pub fn new() -> std::sync::Arc<Self> {
        let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        std::sync::Arc::new(Self {
            arrived_tx: std::sync::Mutex::new(Some(arrived_tx)),
            arrived_rx: tokio::sync::Mutex::new(Some(arrived_rx)),
            release_tx: std::sync::Mutex::new(Some(release_tx)),
            release_rx: tokio::sync::Mutex::new(Some(release_rx)),
        })
    }

    /// Wait until the held gate.run frame arrives at the Node; returns its
    /// (job id, generation).
    pub async fn wait_arrived(&self) -> (String, u32) {
        let rx = self
            .arrived_rx
            .lock()
            .await
            .take()
            .expect("arrival awaited once");
        tokio::time::timeout(Self::HOLD_WAIT, rx)
            .await
            .expect("gate run never arrived within the bounded wait")
            .expect("gate run hold arrival sender dropped")
    }

    /// Release the parked gate.run so its handler proceeds. Safe to call
    /// before the frame arrives — a oneshot message already stored unblocks
    /// the handler's later await immediately.
    pub fn release(&self) {
        if let Some(tx) = self.release_tx.lock().expect("release tx").take() {
            let _ = tx.send(());
        }
    }
}

impl DevNode {
    /// Test-only: recorded canceled generation for a job, if any.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_canceled_generation(&self, job_id: &str) -> Option<u32> {
        self.gate_registry()
            .gate_test_canceled_generation(job_id)
            .await
    }

    /// Test-only: live run generation and stand-down flag (r15 item 2).
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_live_run_state(&self, job_id: &str) -> Option<(u32, bool)> {
        self.gate_registry().gate_test_live_run_state(job_id).await
    }

    /// Test-only: install a Hub-authority hook directly (drives the pre-run
    /// gate.authorize check without a live carrier).
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_set_authorizer(&self, authorizer: std::sync::Arc<dyn GateAuthorizer>) {
        self.gate_registry().set_authorizer(authorizer).await;
    }

    /// Test-only: drop the Hub-authority hook (simulates a missing/dead link).
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_clear_authorizer(&self) {
        self.gate_registry().clear_authorizer().await;
    }

    /// Test-only: drop all local canceled records (simulates a Node restart).
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_reset_cancel_records(&self) {
        self.gate_registry().gate_test_reset_cancel_records().await;
    }

    /// Test-only: arm a hold that parks the NEXT Hub-dispatched `gate.run`
    /// at the handler entry (before run registration, the lane slot and any
    /// git step), so a test can deterministically land `gate.cancel` while
    /// the run frame is genuinely in flight over the real carrier.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_arm_run_hold(&self, hold: std::sync::Arc<GateRunHold>) {
        self.gate_registry().arm_run_hold(hold).await;
    }

    /// Test-only: shorten the granted land child budget to exercise its
    /// kill-the-process-group timeout path (r12 item 4).
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_set_land_timeout(&self, timeout: std::time::Duration) {
        self.gate_registry().set_land_timeout(timeout).await;
    }

    /// Test-only: point the consumed verify/land child at a stand-in CLI that
    /// still performs real git against a local bare origin (r13 item 1 e2e).
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_set_merge_bin(&self, bin: std::path::PathBuf) {
        self.gate_registry().set_merge_bin(bin).await;
    }

    /// Test-only (r16 item 1): arm the one-shot granted-land fence
    /// persistence hold/failure while the spawned child sits on its barrier.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_set_land_fence_fault(&self, fault: LandFenceFault) {
        self.gate_registry().set_land_fence_fault(fault);
    }

    /// Test-only (r16 item 1): force the granted-land start-barrier setup to
    /// fail so the node refuses to spawn the land child and reports uncertain.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_set_force_barrier_fault(&self, on: bool) {
        self.gate_registry().set_force_barrier_fault(on);
    }

    /// Dispatch one Hub→Node gate RPC by method name.
    ///
    /// The carrier in `server.rs` routes each method directly; this is the
    /// entry point for callers outside the crate (integration tests that drive
    /// a real Node over real repositories).
    pub async fn dispatch_gate_rpc(
        &self,
        method: &str,
        params: &Value,
    ) -> Result<Value, NodeError> {
        match method {
            remuda_protocol::METHOD_GATE_RUN => self.run_gate(params).await,
            remuda_protocol::METHOD_GATE_CANCEL => self.cancel_gate(params).await,
            remuda_protocol::METHOD_GATE_THEN => self.run_gate_then(params).await,
            remuda_protocol::METHOD_GATE_LAND => self.run_gate_land(params).await,
            remuda_protocol::METHOD_GATE_UNPIN => self.run_gate_unpin(params).await,
            remuda_protocol::METHOD_GATE_RESOLVE_LANDING => {
                self.run_gate_resolve_landing(params).await
            }
            remuda_protocol::METHOD_GATE_ABORT_LANDING => self.abort_gate_landing(params).await,
            other => Err(NodeError::InvalidRequest(format!(
                "not a gate method: {other}"
            ))),
        }
    }

    /// Handle `gate.run`.
    pub(crate) async fn run_gate(&self, params: &Value) -> Result<Value, NodeError> {
        let request: GateRunParams = serde_json::from_value(params.clone()).map_err(|error| {
            NodeError::InvalidRequest(format!("invalid gate.run params: {error}"))
        })?;
        let result = self.run_gate_typed(request).await?;
        serde_json::to_value(result).map_err(NodeError::from)
    }

    /// Handle `gate.abortLanding` — the Hub settled a Landing WITHOUT a landed
    /// push (the merge is provably absent with the group dead, or an operator
    /// resolved it). Terminate the granted LAND process group the same way a
    /// land timeout does (TERM → bounded wait → KILL the whole group; the run
    /// task reaps the leader and releases the durable fence record) so the lane
    /// frees for a re-queued attempt and an operator-failed job cannot push
    /// after the fact. The generation is tombstoned like a cancel, so the
    /// killed run's late verdict refuses. A run that has already gone (the
    /// common established-absent case) makes this a no-op. r14 R1.
    pub(crate) async fn abort_gate_landing(&self, params: &Value) -> Result<Value, NodeError> {
        let request: GateAbortLandingParams =
            serde_json::from_value(params.clone()).map_err(|error| {
                NodeError::InvalidRequest(format!("invalid gate.abortLanding params: {error}"))
            })?;
        let registry = self.gate_registry();
        let target = {
            let mut table = registry.runs.lock().await;
            // r15 item 2: a delayed abort for an OLDER generation must not
            // touch the NEWER attempt that reused the job slot. Decide against
            // the LIVE row's generation UNDER THIS LOCK, and only carry the
            // pgid out when the generations are equal. A newer live run is
            // left entirely alone (its land child keeps pushing under its own
            // grant); the tombstone is still recorded so the stale RPC stands
            // down.
            let run = table.runs.get(&request.job_id);
            let same_generation = run.is_some_and(|run| run.generation == request.generation);
            let land_pgid = if same_generation {
                let run = run.expect("same_generation implies a run");
                let land_pgid = *run
                    .land_pgid
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if land_pgid.is_none() {
                    // The grant never got to spawn its land child (or the row
                    // was settled while the verify group was the live one):
                    // release its cancel flag so a verify-phase child stands
                    // down too.
                    run.cancel.send(true).ok();
                }
                land_pgid
            } else {
                None
            };
            table.record_cancel(&request.job_id, request.generation);
            land_pgid
        };
        #[cfg(unix)]
        if let Some(pgid) = target {
            let _ = tokio::task::spawn_blocking(move || {
                terminate_and_verify_group(pgid, LAND_GROUP_TERM_GRACE);
            })
            .await;
        }

        Ok(serde_json::json!({
            "ok": true,
            "jobId": request.job_id,
            "generation": request.generation,
        }))
    }

    /// Handle `gate.cancel` — kills the running step's process group and
    /// records the canceled dispatch generation so a delayed same-generation
    /// `gate.run` refuses instead of executing.
    pub(crate) async fn cancel_gate(&self, params: &Value) -> Result<Value, NodeError> {
        let request: GateCancelParams =
            serde_json::from_value(params.clone()).map_err(|error| {
                NodeError::InvalidRequest(format!("invalid gate.cancel params: {error}"))
            })?;
        let registry = self.gate_registry();
        let mut table = registry.runs.lock().await;
        let mut group_to_kill: Option<i32> = None;
        if let Some(run) = table.runs.get(&request.job_id) {
            // r15 item 2: only the live run that belongs to EXACTLY this
            // dispatch generation may be signalled. A cancel frame delayed
            // past a re-dispatch (the job slot reused by a newer attempt)
            // records its tombstone below but must not kill the newer
            // attempt's verify group — the newer run is authorized on its own
            // generation and the stale cancel has no authority over it.
            if run.generation == request.generation {
                run.cancel.send(true).ok();
                if let Some(pgid) = *run.pgid.lock().unwrap_or_else(|poison| poison.into_inner()) {
                    group_to_kill = Some(pgid);
                }
            }
        }
        // Record the canceled generation whether or not the run is registered
        // yet: if the frame is still in flight this is the tombstone; if it is
        // already running this also blocks any duplicate/late same-generation
        // dispatch. Keyed by generation, never by age. At the hard job-count
        // cap a NEW job cannot be recorded: refuse (backpressure) instead of
        // evicting an older job's authority.
        let recorded = table.record_cancel(&request.job_id, request.generation);
        drop(table);
        // r14 item 4: force-kill a cancel that ignored TERM. Detached so the
        // bounded kill never holds the registry lock or the RPC pump.
        if let Some(pgid) = group_to_kill {
            tokio::task::spawn_blocking(move || terminate_group(pgid));
        }
        if !recorded {
            return Err(NodeError::InvalidRequest(format!(
                "gate.cancel: too many canceled gate jobs ({MAX_CANCELED_RECORDS}); \
                 refusing rather than evicting cancel authority"
            )));
        }
        Ok(serde_json::json!({
            "ok": true,
            "jobId": request.job_id,
            "generation": request.generation,
        }))
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

    /// Handle `gate.resolveLanding` — read-only post-restart probe: is the
    /// granted merge already an ancestor of the remote base branch, provably
    /// absent, or still indeterminate? Never pushes; `unknown` is the honest
    /// answer whenever it cannot confirm either terminal fact. The Hub decides
    /// nothing from this reply except what the reply ESTABLISHED (r14 R1).
    pub(crate) async fn run_gate_resolve_landing(
        &self,
        params: &Value,
    ) -> Result<Value, NodeError> {
        let request: GateResolveLandingParams =
            serde_json::from_value(params.clone()).map_err(|error| {
                NodeError::InvalidRequest(format!("invalid gate.resolveLanding params: {error}"))
            })?;
        // r14 R1 / r16 item 2 R5: `absent` is attestable ONLY with
        // authoritative death evidence for the exact attempt — a live kernel
        // ESRCH on the group this process recorded, or a durable death
        // certificate. A missing/unreadable fence ledger with no cert is
        // Unknown, never "the push is dead".
        let land_group_verified_dead = matches!(
            self.gate_registry()
                .land_push_liveness(&request.job_id)
                .await,
            LandPushLiveness::VerifiedDead
        );
        let result = tokio::task::spawn_blocking(move || {
            resolve_landing_after_restart(&request, land_group_verified_dead)
        })
        .await
        .map_err(|error| NodeError::InvalidRequest(format!("resolveLanding join: {error}")))?;
        serde_json::to_value(result).map_err(NodeError::from)
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
        // Test-only entry hold: parks with the frame in flight (Hub claim
        // already committed, run unregistered) so a test can land
        // gate.cancel before the handler proceeds. Runs before ANY state.
        #[cfg(any(test, feature = "test-faults"))]
        registry
            .await_run_holds(&request.job_id, request.generation)
            .await;
        // A cancel covering THIS generation (acked while no run was
        // registered, however long ago — refusal is generation-scoped, not
        // age-scoped) blocks the run before it touches the lane so it never
        // reports lane-busy. The decisive re-check below, atomically with
        // registration, closes the cancel-during-acquire window.
        let canceled = {
            let table = registry.runs.lock().await;
            table.is_canceled_generation(&request.job_id, request.generation)
        };
        if canceled {
            return Ok(canceled_before_run(
                registry,
                &request.job_id,
                request.generation,
            ));
        }
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
                    generation: request.generation,
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
            generation: request.generation,
            cancel: cancel_tx,
            pgid: std::sync::Mutex::new(None),
            land_pgid: std::sync::Mutex::new(None),
        });
        {
            let mut table = registry.runs.lock().await;
            // A cancel for THIS generation that landed while this frame was
            // acquiring the lane slot: the same atomic decision as
            // cancel_gate's record.
            if table.is_canceled_generation(&request.job_id, request.generation) {
                drop(table);
                drop(lane_guard);
                return Ok(canceled_before_run(
                    registry,
                    &request.job_id,
                    request.generation,
                ));
            }
            // A strictly NEWER generation for this job supersedes an older
            // cancel record (the job was requeued and re-dispatched): clear
            // it so this run and later frames are not blocked.
            table.clear_if_superseded(&request.job_id, request.generation);
            if table.runs.contains_key(&request.job_id) {
                return Err(NodeError::InvalidRequest(format!(
                    "gate job {} is already running",
                    request.job_id
                )));
            }
            table.runs.insert(request.job_id.clone(), live.clone());
        }

        // HUB AUTHORITY (before any fetch/push): the local floor only knows
        // what this Node has seen; a record eviction, a Node restart that lost
        // the registry, or a paused frame delivered to a replacement session
        // all defeat it. Ask the Hub — the single writer of the job row —
        // whether this generation is still the live Running attempt. Any
        // non-authorized answer, timeout or missing link stops here, before
        // execute_gate touches git.
        if !registry
            .hub_authorizes(&request.job_id, request.generation)
            .await
        {
            {
                let mut table = registry.runs.lock().await;
                table.runs.remove(&request.job_id);
            }
            drop(lane_guard);
            return Ok(canceled_before_run(
                registry,
                &request.job_id,
                request.generation,
            ));
        }

        let outcome = self.execute_gate(&request, live.clone(), cancel_rx).await;

        {
            let mut table = registry.runs.lock().await;
            table.runs.remove(&request.job_id);
        }
        drop(lane_guard);

        // The finished event is authoritative even if the RPC reply is lost.
        let _ = registry.events.send(GateEventParams {
            job_id: request.job_id.clone(),
            generation: request.generation,
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
        let registry = self.gate_registry();
        let mut result = GateRunResult {
            job_id: request.job_id.clone(),
            generation: request.generation,
            status: "failed".into(),
            ..Default::default()
        };
        let repo = PathBuf::from(&request.repo_path);
        let emit = |kind: GateEventKind| {
            let _ = self.gate_registry().events.send(GateEventParams {
                job_id: request.job_id.clone(),
                generation: request.generation,
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

        // PHASE 1 — verify only. The merge CLI verifies (`--gate`) but never
        // advances/pushes main in this child. A land is a SEPARATE child
        // after the Hub grants Landing (below), so a cancel/drop before the
        // grant cannot be followed by a push.
        let phase = "gate";
        emit(GateEventKind::Phase {
            phase: phase.into(),
        });
        let binary = registry.merge_binary(&request.binary).await;
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
        // The lane host lands when mode==land and pushFrom!=home. The home
        // host performs the CAS push via a separate `gate.land` RPC; the lane
        // is always verify-only.
        let lands_here =
            request.mode == "land" && request.push_from != remuda_protocol::GatePushFrom::Home;
        args.push("--gate".into());
        // Verify child: never land/push.
        args.push("--no-push".into());
        if request.web == "always" {
            args.push("--web".into());
            args.push("--web-e2e".into());
        }

        // The child gets an explicitly built environment, never the Node's
        // own: the Node may itself run under its REMUDA_PTY_* carrier flags,
        // which must not leak into cargo and every test binary (gate-lane-3).
        let child_env = gate_run_child_env(request, &host_env_snapshot());
        let mut command = Command::new(&binary);
        command
            .args(&args)
            .current_dir(&repo)
            .env_clear()
            .envs(&child_env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
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
        {
            // None id (or an unconvertible one) leaves the pgid unset: a
            // Some(0) here would later become killpg(0) against the Node's
            // own process group.
            if let Some(pid) = child.id()
                && let Ok(pgid) = i32::try_from(pid)
            {
                *live
                    .pgid
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()) = Some(pgid);
            }
        }

        // Stream gate.jsonl step results as they are flushed (gate.sh flushes
        // every step), and stderr lines as log events. Scoped to this child's
        // own scratch roots: another merge in the same TMPDIR must not leak
        // its steps into this job.
        let run_started = std::time::SystemTime::now();
        let child_pid = child.id().unwrap_or(0);
        let step_task = tokio::spawn(stream_gate_report(
            self.clone(),
            request.job_id.clone(),
            request.generation,
            run_started,
            child_pid,
        ));
        let mut stderr_task: Option<tokio::task::JoinHandle<()>> = None;
        if let Some(stderr) = child.stderr.take() {
            let node = self.clone();
            let job_id = request.job_id.clone();
            let generation = request.generation;
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
                        generation,
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
                let pgid = *live.pgid.lock().unwrap_or_else(|poison| poison.into_inner());
                if let Some(pgid) = pgid {
                    // r14 item 4: a cancel gets the SAME guaranteed sequence as a
                    // land timeout — TERM → bounded wait → KILL the whole
                    // group → ESRCH. child.kill() then only reaps the leader.
                    let _ = tokio::task::spawn_blocking(move || terminate_group(pgid)).await;
                }
                // Reap; stdout is dropped.
                let _ = child.kill().await;
                let _ = child.wait().await;
                (None, started.elapsed())
            }
            _ = tokio::time::sleep(Duration::from_secs(deadline)) => {
                timed_out = true;
                let pgid = *live.pgid.lock().unwrap_or_else(|poison| poison.into_inner());
                if let Some(pgid) = pgid {
                    // A timeout must kill stubborn descendants, not just the
                    // leader (r14 item 4): the bounded whole-group kill.
                    let _ = tokio::task::spawn_blocking(move || terminate_group(pgid)).await;
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
        // PHASE 2 — land. Only AFTER a passing verify, and only when THIS lane
        // host is the pusher, ask the Hub to transition Running → Landing for
        // exactly this (job, generation, session). A cancel committed first is
        // refused and nothing is pushed. Once granted, run the land-only
        // merge child (CAS + push); the grant already held survives a cancel.
        if lands_here && result.status == "passed" {
            // Final local re-check right before the grant RPC: a
            // gate.cancel that landed while the verify child ran is visible.
            {
                let table = registry.runs.lock().await;
                if table.is_canceled_generation(&request.job_id, request.generation) {
                    let refused =
                        canceled_before_run(registry, &request.job_id, request.generation);
                    drop(table);
                    return refused;
                }
            }
            let grant = registry
                .hub_land_grant(
                    &request.job_id,
                    request.generation,
                    result.merge_sha.as_deref().unwrap_or(""),
                )
                .await;
            if !grant.granted {
                let why = "land not granted (canceled/stale/session changed); not pushed";
                result.status = "canceled".into();
                result.error = Some(why.into());
                result.reason = Some(why.into());
                let _ = registry.events.send(GateEventParams {
                    job_id: request.job_id.clone(),
                    generation: request.generation,
                    kind: GateEventKind::Finished {
                        result: Box::new(result.clone()),
                    },
                });
                return result;
            }
            emit(GateEventKind::Phase {
                phase: "land".into(),
            });
            let mut land_args = vec![
                "merge".to_owned(),
                request.branch.clone(),
                "--land".to_owned(),
                "--onto".to_owned(),
                request.base_branch.clone(),
                "--json".to_owned(),
                "--repo".to_owned(),
                request.repo_path.clone(),
                "--target-dir".to_owned(),
                request.target_dir.clone(),
            ];
            // Pin the exact verified tuple (r12 item 6): the land child reads
            // the immutable report for THIS merge sha, not the latest report
            // on the same base, so a second verification cannot substitute it.
            if let Some(merge_sha) = result.merge_sha.as_deref().filter(|sha| !sha.is_empty()) {
                land_args.push("--report".to_owned());
                land_args.push(merge_sha.to_owned());
            }
            if !request.push {
                land_args.push("--no-push".into());
            }
            // r16 item 1: assemble the COMPLETE child environment BEFORE the
            // start barrier is wired. The barrier env used to be set first and
            // `.env_clear()` below wiped it, so the land CLI never saw
            // REMUDA_START_BARRIER_FD, ran unblocked, and the "durable fence
            // before exec" guarantee was fictional.
            let land_child_env = gate_run_child_env(request, &host_env_snapshot());
            let mut land_command = Command::new(&binary);
            land_command
                .args(&land_args)
                .current_dir(&repo)
                .env_clear()
                .envs(&land_child_env)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(false);
            #[cfg(unix)]
            land_command.process_group(0);
            // r16 item 1: the barrier is configured ONLY after the environment
            // is final. If the barrier cannot be made Ready there is NO SAFE
            // CHILD TO SPAWN: refuse to start it at all and report UNCERTAIN
            // (the grant is held; the Hub reconciles the exact merge against
            // the remote). The old code spawned anyway with no barrier env —
            // an unblocked pusher — and then reported a definite "failed".
            #[cfg(unix)]
            let land_start_barrier: Option<LandStartBarrier> = {
                match registry.make_land_start_barrier() {
                    mut ready @ LandStartBarrier::Ready { .. } => {
                        land_command.stdin(ready.take_child_stdin());
                        land_command.env(REMUDA_START_BARRIER_FD, "0");
                        Some(ready)
                    }
                    LandStartBarrier::Failed(error) => {
                        land_command.stdin(Stdio::null());
                        tracing::error!(
                            %error,
                            "land child-start barrier unavailable; refusing to spawn the land child"
                        );
                        None
                    }
                }
            };
            #[cfg(not(unix))]
            let land_start_barrier: Option<()> = None;
            #[cfg(unix)]
            let barrier_ready = land_start_barrier.is_some();
            #[cfg(not(unix))]
            let barrier_ready = true;
            let mut land_child = if !barrier_ready {
                result.status = "uncertain".into();
                result.error = Some(
                    "the land child-start barrier could not be created; the granted land child was \
                     NOT started (nothing pushed); the Hub reconciles the granted merge against the \
                     remote"
                        .into(),
                );
                result.reason = Some(
                    "land child-start barrier unavailable; child not spawned; push outcome uncertain"
                        .into(),
                );
                return finalize(result);
            } else {
                match land_command.spawn() {
                    Ok(child) => child,
                    Err(error) => {
                        result.status = "failed".into();
                        result.error = Some(format!("spawn land child: {error}"));
                        attach_failure_evidence(&mut result, &trace, request.keep_logs);
                        return finalize(result);
                    }
                }
            };
            // Record the land child's OWN process group so a timeout can kill
            // the whole push (the child plus any git it spawned) and reap it.
            // Without this the child was dropped with kill_on_drop(false) and
            // kept pushing after we reported failed.
            #[cfg(unix)]
            let land_pgid = land_child.id().and_then(|pid| i32::try_from(pid).ok());
            #[cfg(unix)]
            if let Some(pgid) = land_pgid
                && let Some(barrier) = land_start_barrier
            {
                *live
                    .land_pgid
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()) = Some(pgid);
                // The child is blocked on the barrier before user logic runs.
                // Persist the fence BEFORE releasing it. On any barrier/fence
                // failure drop the write end (EOF) and KILL the held child — it
                // ran no git. r16 item 1 / R5: the verdict is UNCERTAIN, never
                // a definite "failed": the grant is already held by the Hub,
                // which reconciles the exact granted merge against the remote.
                if let Err(error) = barrier.release_after(|| {
                    registry.record_land_group(&request.job_id, request.generation, pgid)
                }) {
                    use nix::sys::signal::{Signal, killpg};
                    use nix::unistd::Pid;
                    let _ = killpg(Pid::from_raw(pgid), Some(Signal::SIGKILL));
                    let _ = land_child.wait().await;
                    result.status = "uncertain".into();
                    result.error = Some(format!(
                        "could not durably record the granted land group before releasing the land \
                         child ({error}); the child was held before start and killed, so nothing was \
                         pushed; the Hub reconciles against the remote"
                    ));
                    result.reason = Some(
                        "land fence persistence failed before child start; not pushed; outcome \
                         uncertain"
                            .into(),
                    );
                    attach_failure_evidence(&mut result, &trace, request.keep_logs);
                    return finalize(result);
                }
            }
            let land_stdout = land_child.stdout.take();
            // r13 item 6: drain stderr CONCURRENTLY for the life of the child
            // (and keep the handle for timeout/kill). A remote rejection can
            // print far more than one pipe buffer; never reading it blocked the
            // land child forever (deadlock) — the exact failure the verify child
            // already guarded against with its own stderr pump.
            //
            // r14 item 9: the drained lines are ALSO accumulated (the child's
            // stderr handle is consumed by this pump, so the waited Output's
            // stderr is always empty). A nonzero land exit then reports the
            // captured remote rejection/error text instead of a blank reason.
            let captured_stderr: Arc<std::sync::Mutex<Vec<String>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));
            let mut land_stderr_task = land_child.stderr.take().map(|stderr| {
                let node = self.clone();
                let job_id = request.job_id.clone();
                let generation = request.generation;
                let captured = captured_stderr.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncBufReadExt, BufReader};
                    let mut lines = BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if let Ok(mut buffer) = captured.lock() {
                            buffer.push(line.clone());
                        }
                        let _ = node.gate_registry().events.send(GateEventParams {
                            job_id: job_id.clone(),
                            generation,
                            kind: GateEventKind::Log { message: line },
                        });
                    }
                })
            });
            // Land runs to completion even if cancel fires (grant already
            // held); await it bounded by the (test-overridable) land budget.
            // wait_with_output_piped BORROWS the child (unlike the consuming
            // wait_with_output), so on timeout the child is still owned here
            // and can be killed and reaped.
            let land_budget = registry.land_timeout().await;
            // Scope the waiting future so its borrow of `land_child` is
            // released before the timeout arm kills and reaps the child.
            let land_outcome: Result<Option<std::process::Output>, ()> = {
                let wait = land_child.wait_with_output_piped(land_stdout);
                tokio::pin!(wait);
                tokio::select! {
                    output = &mut wait => Ok(output),
                    _ = tokio::time::sleep(land_budget) => Err(()),
                }
            };
            // On a normal child exit, join the stderr pump FIRST so every
            // rejected/error line is in `captured_stderr` before the verdict
            // arms read it (the Output's own stderr is empty — see above).
            if matches!(land_outcome, Ok(Some(_)))
                && let Some(task) = land_stderr_task.take()
            {
                let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
            }
            match land_outcome {
                Err(()) => {
                    // The budget expired. Terminate the WHOLE land group
                    // synchronously (TERM, bounded wait, KILL, reap) before
                    // reporting, so a SIGTERM-ignoring git/ssh descendant cannot
                    // keep pushing after this verdict. Even then, the remote may
                    // already have accepted the ref before the client hung, so
                    // the outcome is reported UNCERTAIN — never "failed": the
                    // Hub keeps the Landing row and reconciles against the exact
                    // granted merge on the remote (r13 item 5).
                    #[cfg(unix)]
                    {
                        if let Some(pgid) = land_pgid {
                            tokio::task::spawn_blocking(move || terminate_group(pgid))
                                .await
                                .ok();
                        }
                        // Reap the land leader (a SIGKILL'd zombie otherwise).
                        let _ = land_child.wait().await;
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = land_child.kill().await;
                        let _ = land_child.wait().await;
                    }
                    result.status = "uncertain".into();
                    result.error = Some(format!(
                        "land exceeded {}s; terminated the land process group; push outcome \
                         unknown (the Hub reconciles against the remote)",
                        land_budget.as_secs()
                    ));
                    result.reason = Some(format!(
                        "land timed out after {}s; push outcome uncertain",
                        land_budget.as_secs()
                    ));
                }
                Ok(Some(land_output)) if land_output.status.success() => {
                    let land_report: Value =
                        serde_json::from_slice(&land_output.stdout).unwrap_or_else(|_| json!({}));
                    result.status = "landed".into();
                    result.current_main_sha = string_field(&land_report, "currentMain");
                    result.merge_sha = string_field(&land_report, "merged").or(result.merge_sha);
                }
                Ok(Some(output)) => {
                    // r16 item 4 (R5): the child was RELEASED, so its push may
                    // have reached the remote. Only a RECOGNIZED structured
                    // verdict may settle the Landing; a crash (signal), a
                    // missing/malformed/unparseable report, or an unrecognized
                    // status is UNCERTAIN — the Hub reconciles against origin —
                    // never a definite failure.
                    let drained_stderr = captured_stderr
                        .lock()
                        .map(|lines| lines.join("\n"))
                        .unwrap_or_default();
                    match classify_released_land_outcome(&output, &drained_stderr) {
                        ReleasedLandOutcome::Landed(land_report) => {
                            result.status = "landed".into();
                            result.current_main_sha = string_field(&land_report, "currentMain");
                            result.merge_sha =
                                string_field(&land_report, "merged").or(result.merge_sha);
                        }
                        ReleasedLandOutcome::BaseMoved(land_report) => {
                            // `merge --land` exits nonzero when the
                            // compare-and-swap lost (base moved under the
                            // lease): keep the requeue verdict and currentMain.
                            result.status = "base-moved".into();
                            result.current_main_sha = string_field(&land_report, "currentMain");
                            result.error = string_field(&land_report, "error");
                            result.reason = None;
                        }
                        ReleasedLandOutcome::Uncertain { error, reason } => {
                            result.status = "uncertain".into();
                            result.error = Some(error);
                            result.reason = Some(reason);
                        }
                        ReleasedLandOutcome::Failed(error) => {
                            result.status = "failed".into();
                            result.error = Some(error);
                        }
                    }
                }
                Ok(None) => {
                    // r16 item 4: no wait status at all after the release is
                    // not an established failure: the push may have landed.
                    result.status = "uncertain".into();
                    result.error = Some(
                        "the released land child ended without any result; the push may have \
                         reached the remote; the Hub reconciles against the exact granted merge"
                            .into(),
                    );
                    result.reason =
                        Some("land child produced no result; push outcome uncertain".into());
                }
            }
            // The child has exited (or been killed on timeout); let the stderr
            // pump drain to EOF (bounded) so a large final rejection/error burst
            // is fully read and the pipe can never have wedged the child.
            if let Some(task) = land_stderr_task {
                let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
            }
            // r14 R1: the leader was awaited on every arm above; finish any
            // surviving group member, verify the group is ESRCH, then release
            // the durable fence record so a later remote probe may attest
            // `absent`. Never release it while the group can still push.
            #[cfg(unix)]
            if let Some(pgid) = land_pgid {
                registry.settle_land_group(&request.job_id, pgid).await;
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
        // The post-land command gets the same explicitly built environment as
        // the gate child: no REMUDA_PTY_* carrier flags leak from the Node
        // (gate-lane-3). It has no toolchain PATH prefix or port block of its
        // own, so only the lane env, the build passthrough and a base PATH.
        let child_env = gate_then_child_env(&request.env, &host_env_snapshot());
        let mut command = Command::new("bash");
        command
            .args(["-lc", &request.command])
            .current_dir(&cwd)
            .env_clear()
            .envs(&child_env)
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

/// What a RELEASED granted-land child's exit established (r16 item 4, R5).
enum ReleasedLandOutcome {
    Landed(Value),
    BaseMoved(Value),
    Uncertain {
        error: String,
        reason: String,
    },
    /// Only the merge CLI's recognized, structured failure verdict.
    Failed(String),
}

/// Combine the child's own stderr (empty under the concurrent pump) with the
/// pump-drained lines for a verdict message.
fn combine_land_stderr(output: &std::process::Output, drained: &str) -> String {
    let direct = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let text = if direct.is_empty() {
        drained.trim().to_owned()
    } else {
        direct
    };
    text.chars().take(2000).collect()
}

/// Classify a granted-land child that already ran (its start barrier was
/// released, so its push MAY have reached the remote). Every outcome is
/// Established or Unknown (r16 item 4, R5):
/// * exit 0 → `Landed` (the CLI only exits 0 after the push is acknowledged);
/// * a recognized structured verdict → `BaseMoved` (CAS loss, nothing pushed),
///   `Uncertain` (the CLI itself could not establish the push), or `Failed`
///   ONLY for `gate_failed` at exit 1 — a pre-push rejection or a
///   recovery-ESTABLISHED remote rejection, both proving the exact merge did
///   not land;
/// * death by signal (no exit code), missing/malformed stdout JSON, or an
///   unrecognized status → `Uncertain`, never Failed: the child may have
///   pushed and crashed before reporting, so the Hub must reconcile.
fn classify_released_land_outcome(
    output: &std::process::Output,
    drained_stderr: &str,
) -> ReleasedLandOutcome {
    if output.status.success() {
        let report = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| json!({}));
        return ReleasedLandOutcome::Landed(report);
    }
    let stderr_text = combine_land_stderr(output, drained_stderr);
    // Killed by a signal (no exit code) AFTER release: crash-only, Unknown.
    let Some(exit_code) = output.status.code() else {
        return ReleasedLandOutcome::Uncertain {
            error: format!(
                "the land child was killed by a signal after it was released; the push may have \
                 reached the remote (stderr: {stderr_text})"
            ),
            reason: "land child killed by a signal; push outcome uncertain".into(),
        };
    };
    // Missing or malformed JSON: no recognized verdict, Unknown.
    let Ok(report) = serde_json::from_slice::<Value>(&output.stdout) else {
        return ReleasedLandOutcome::Uncertain {
            error: format!(
                "the land child exited {exit_code} without a parseable result after it was \
                 released; the push may have reached the remote (stderr: {stderr_text})"
            ),
            reason: "land child returned no readable verdict; push outcome uncertain".into(),
        };
    };
    match report.get("status").and_then(Value::as_str) {
        Some("base_moved") | Some("base-moved") => ReleasedLandOutcome::BaseMoved(report),
        Some("uncertain") => ReleasedLandOutcome::Uncertain {
            error: string_field(&report, "error").unwrap_or_else(|| {
                "land push outcome could not be established against the remote".into()
            }),
            reason: "land push outcome unknown; the Hub reconciles against the remote".into(),
        },
        // The merge CLI's one recognized failure verdict (exit 1): it only
        // reaches this status for a pre-push error (nothing pushed) or after
        // recovery established the exact merge absent with the remote base
        // unchanged (a genuine rejection).
        Some("gate_failed") if exit_code == 1 => ReleasedLandOutcome::Failed(format!(
            "land failed after grant (exit 1): {}",
            string_field(&report, "error")
                .unwrap_or_else(|| stderr_text.clone())
                .trim()
        )),
        other => ReleasedLandOutcome::Uncertain {
            error: format!(
                "the land child exited {exit_code} with an unrecognized verdict {other:?}; the \
                 push may have reached the remote (stderr: {stderr_text})"
            ),
            reason: "land child verdict unrecognized; push outcome uncertain".into(),
        },
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

/// Tail this run's `<tmp>/remuda-mq-<child_pid>-*/gate.jsonl` and emit one
/// step event per new line. Only reports modified at or after the run started
/// (mtime granularity tolerated) are eligible.
///
/// The merge CLI names each scratch root after its OWN pid
/// (`remuda-mq-<pid>-<n>-<nanos>`, crates/remuda/src/cmd/merge/scratch.rs), so
/// the child pid scopes the tail to this single run. Tailing every
/// `remuda-mq-*` dir made concurrent tests and real landing gates on the same
/// TMPDIR stream one another's step results into the wrong job.
async fn stream_gate_report(
    node: DevNode,
    job_id: String,
    generation: u32,
    run_started: std::time::SystemTime,
    child_pid: u32,
) {
    let own_prefix = format!("remuda-mq-{child_pid}-");
    let window_start = run_started - Duration::from_secs(5);
    let mut offsets: BTreeMap<PathBuf, u64> = BTreeMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    loop {
        let temp = std::env::temp_dir();
        if let Ok(entries) = std::fs::read_dir(&temp) {
            for entry in entries.flatten() {
                if !entry.file_name().to_string_lossy().starts_with(&own_prefix) {
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
                            generation,
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

/// Whitelisted base `PATH` for a gate child. A lane must be reproducible from
/// its own config, so the base never comes from the Node's ambient `PATH`; the
/// toolchain prefix (`request.toolchain_path`) is composed ahead of it, and a
/// lane that sets `PATH` in its env replaces this default (gate-lane-3).
const GATE_BASE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Host environment a build legitimately needs, read once from the Node's
/// process env. Only this whitelist crosses into a gate child; every other
/// inherited variable — the Node's own `REMUDA_PTY_CARRIER`/`_EMULATOR`/`_HOOKS`
/// carrier flags included — is dropped by `env_clear` so a lane host and the
/// coordinator ssh gate present the child with the same environment
/// (gate-lane-3).
fn host_env_snapshot() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for key in ["HOME", "USER", "SHELL", "TMPDIR", "LANG", "SSH_AUTH_SOCK"] {
        if let Some(value) = std::env::var_os(key)
            && let Ok(value) = value.into_string()
        {
            env.insert(key.to_owned(), value);
        }
    }
    env
}

/// The environment shared by every gate child (the merge CLI and the
/// `gate.then` bash step), built from scratch so no ambient Node variable
/// leaks. Precedence, lowest first:
/// 1. `TERM=dumb` plus the host passthrough whitelist ([`host_env_snapshot`]);
/// 2. the lane env from the request (`ProjectGateLane::env`) — the only channel
///    that may (re)introduce a `REMUDA_PTY_*` variable, and the only one that
///    may override `PATH`'s base or the passthrough.
///
/// `PATH` is finalized by the caller so the toolchain prefix can lead it.
fn gate_child_base_env(
    lane_env: &BTreeMap<String, String>,
    host: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("TERM".to_owned(), "dumb".to_owned());
    for (key, value) in host {
        env.insert(key.clone(), value.clone());
    }
    for (key, value) in lane_env {
        env.insert(key.clone(), value.clone());
    }
    env
}

/// Compose `PATH` for a gate child: the toolchain prefix, when present, ahead
/// of `base` (the lane's own `PATH` or [`GATE_BASE_PATH`]).
fn compose_gate_path(prefix: Option<&str>, base: &str) -> String {
    match prefix.filter(|prefix| !prefix.is_empty()) {
        Some(prefix) => format!("{prefix}:{base}"),
        None => base.to_owned(),
    }
}

/// Build the explicit environment for the merge child (`remuda merge --gate`).
///
/// Final key set: the shared base ([`gate_child_base_env`]); the derived keys
/// the runner sets (`CARGO_TARGET_DIR`, `CARGO_INCREMENTAL`, `VITE_NO_WATCH`,
/// and — when the request carries them — `PW_TEST_CONNECT_WS_ENDPOINT` +
/// `PW_CHANNEL`, `REMUDA_E2E_LOCK`, `HUB_E2E_LISTEN` + `HUB_E2E_WEB_PORT`,
/// `REMUDA_GATE_STEP_TIMEOUTS`); and `PATH` as the toolchain prefix ahead of
/// the whitelisted base.
fn gate_run_child_env(
    request: &GateRunParams,
    host: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut env = gate_child_base_env(&request.env, host);
    env.insert("CARGO_TARGET_DIR".to_owned(), request.target_dir.clone());
    env.insert("CARGO_INCREMENTAL".to_owned(), "0".to_owned());
    env.insert("VITE_NO_WATCH".to_owned(), "1".to_owned());
    if let Some(endpoint) = &request.pw_endpoint {
        env.insert("PW_TEST_CONNECT_WS_ENDPOINT".to_owned(), endpoint.clone());
        env.insert("PW_CHANNEL".to_owned(), "chromium".to_owned());
    }
    if let Some(lock) = &request.lock_path {
        env.insert("REMUDA_E2E_LOCK".to_owned(), lock.clone());
    }
    if let Some((listen, web_port)) = port_pair(request.ports.as_deref()) {
        env.insert("HUB_E2E_LISTEN".to_owned(), listen);
        env.insert("HUB_E2E_WEB_PORT".to_owned(), web_port);
    }
    if !request.timeouts.is_empty()
        && let Ok(json) = serde_json::to_string(&request.timeouts)
    {
        env.insert("REMUDA_GATE_STEP_TIMEOUTS".to_owned(), json);
    }
    let base = env
        .get("PATH")
        .cloned()
        .unwrap_or_else(|| GATE_BASE_PATH.to_owned());
    env.insert(
        "PATH".to_owned(),
        compose_gate_path(request.toolchain_path.as_deref(), &base),
    );
    env
}

/// Build the explicit environment for the `gate.then` bash step: the shared
/// base plus a `PATH` (the lane's own or the whitelisted base). It carries no
/// toolchain prefix, cargo target or port block of its own.
fn gate_then_child_env(
    lane_env: &BTreeMap<String, String>,
    host: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut env = gate_child_base_env(lane_env, host);
    let base = env
        .get("PATH")
        .cloned()
        .unwrap_or_else(|| GATE_BASE_PATH.to_owned());
    env.insert("PATH".to_owned(), base);
    env
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

/// Read-only landing resolution. Fetches the remote base branch and reports:
/// * `landed` when the granted merge is already an ancestor of
///   `origin/<base>`;
/// * `absent` when a fresh fetch succeeded, the merge is NOT an ancestor, and
///   `land_group_verified_dead` is true — the caller holds AUTHORITATIVE death
///   evidence for the exact attempt's land group (a live kernel ESRCH or a
///   durable death certificate; r14 R1, r16 item 2 R5);
/// * `unknown` for every fetch/parse/ancestry failure, non-ancestry without
///   authoritative death evidence, and crucially a MISSING OR UNREADABLE
///   ledger (missing proof is not proof of death). It never pushes and never
///   asserts a push it cannot prove.
fn resolve_landing_after_restart(
    request: &GateResolveLandingParams,
    land_group_verified_dead: bool,
) -> GateResolveLandingResult {
    let repo = PathBuf::from(&request.repo_path);
    let unknown = |error: String| GateResolveLandingResult {
        job_id: request.job_id.clone(),
        status: "unknown".into(),
        base_sha: None,
        error: Some(error),
    };
    if request.merge_sha.trim().is_empty() {
        return unknown("no granted merge sha recorded".into());
    }
    if let Err(error) = block_git(&repo, &["fetch", "-q", "origin"]) {
        return unknown(format!("fetch origin: {error}"));
    }
    let remote_base = format!("origin/{}", request.base_branch);
    let base_sha = match block_git(&repo, &["rev-parse", &remote_base]) {
        Ok(sha) => sha,
        Err(error) => return unknown(format!("resolve {remote_base}: {error}")),
    };
    // merge-base --is-ancestor exits 0 iff merge_sha is reachable from the
    // remote base — i.e. the granted push actually landed there. r16 item 3:
    // exit 0, exit 1 and an abnormal termination are distinguished; a signal
    // or any other non-0/1 exit is Unknown, and exit 1 proves non-ancestry
    // ONLY with complete (non-shallow) history — a bounded safe deepen is
    // attempted first, otherwise the answer stays Unknown.
    match establish_ancestry(&repo, &request.merge_sha, &remote_base) {
        AncestryVerdict::Ancestor => GateResolveLandingResult {
            job_id: request.job_id.clone(),
            status: "landed".into(),
            base_sha: Some(base_sha),
            error: None,
        },
        AncestryVerdict::NotAncestor if !land_group_verified_dead => unknown(format!(
            "{} is not an ancestor of {remote_base} but the granted land group's death is not \
             authoritatively established (no live ESRCH or death certificate; a missing ledger is \
             not proof); push outcome unknown",
            request.merge_sha
        )),
        AncestryVerdict::NotAncestor => GateResolveLandingResult {
            job_id: request.job_id.clone(),
            status: "absent".into(),
            base_sha: Some(base_sha),
            error: None,
        },
        AncestryVerdict::Unknown(error) => unknown(format!(
            "cannot establish whether {} reached {remote_base}: {error}",
            request.merge_sha
        )),
    }
}

/// An ancestry read that distinguishes an Established fact from Unknown
/// (r16 item 3, R5): exit 0 / exit 1 / anything else are separate outcomes.
#[derive(Debug, PartialEq, Eq)]
enum AncestryVerdict {
    Ancestor,
    NotAncestor,
    Unknown(String),
}

/// Same bounded, additive deepen schedule as the merge CLI (r16 item 3): a
/// deepen only adds history and never rewrites refs.
const ANCESTRY_DEEPEN_ARGS: [&[&str]; 2] = [
    &["fetch", "-q", "--deepen=256", "origin"],
    &["fetch", "-q", "--deepen=4096", "origin"],
];

/// Map a finished merge-base process to the tri-state verdict (r16 item 3):
/// exit 0 / exit 1 are the only Established outcomes; any other exit code or
/// death by signal (no exit code) is Unknown, never a guessed boolean.
fn classify_ancestry_output(output: &std::process::Output) -> AncestryVerdict {
    match output.status.code() {
        Some(0) => AncestryVerdict::Ancestor,
        Some(1) => AncestryVerdict::NotAncestor,
        other => AncestryVerdict::Unknown(format!(
            "merge-base --is-ancestor ended abnormally ({other:?}): {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// Run `git merge-base --is-ancestor` once, mapping raw process outcomes.
fn raw_ancestry(repo: &Path, ancestor: &str, descendant: &str) -> AncestryVerdict {
    match std::process::Command::new("git")
        .current_dir(repo)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
    {
        Ok(output) => classify_ancestry_output(&output),
        Err(error) => AncestryVerdict::Unknown(format!("spawn merge-base: {error}")),
    }
}

/// True iff the repo has no shallow graft (a read error is itself Unknown, so
/// reported as shallow to force a deepen/refusal rather than trusting exit 1).
fn git_is_shallow(repo: &Path) -> bool {
    match std::process::Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "--is-shallow-repository"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim() == "true"
        }
        _ => true,
    }
}

/// Count reachable commits; used to detect whether a shallow `--deepen` added
/// history. A read failure is `None` (the caller then cannot prove progress).
fn git_reachable_commit_count(repo: &Path) -> Option<u64> {
    let output = std::process::Command::new("git")
        .current_dir(repo)
        .args(["rev-list", "--count", "--all"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// Read ancestry as Established-or-Unknown (r16 item 3, R5). An exit-1
/// "not ancestor" is trusted only when the history spanning both tips is
/// complete; in a shallow clone bounded safe deepens are attempted and the
/// read re-checked after EACH one — the edge can appear as soon as the
/// boundary moves past the queried tips, even while the repo stays shallow.
/// A deepen that adds no history (a shallow remote), a failed deepen, or a
/// deepen budget exhausted while shallow keeps the answer Unknown.
fn establish_ancestry(repo: &Path, ancestor: &str, descendant: &str) -> AncestryVerdict {
    match raw_ancestry(repo, ancestor, descendant) {
        // Complete history: the raw exit-0/exit-1 answer is itself the fact.
        verdict if !git_is_shallow(repo) => verdict,
        // Shallow but exit 0 (present edge) needs no extra history.
        verdict @ AncestryVerdict::Ancestor => verdict,
        AncestryVerdict::Unknown(error) => AncestryVerdict::Unknown(error),
        AncestryVerdict::NotAncestor => {
            let mut last_count = git_reachable_commit_count(repo);
            for args in ANCESTRY_DEEPEN_ARGS {
                match std::process::Command::new("git")
                    .current_dir(repo)
                    .args(args)
                    .env("GIT_TERMINAL_PROMPT", "0")
                    .output()
                {
                    Ok(output) if !output.status.success() => {
                        return AncestryVerdict::Unknown(format!(
                            "shallow merge-base said not-ancestor and the bounded deepen failed \
                             ({}): {}",
                            output.status,
                            String::from_utf8_lossy(&output.stderr).trim()
                        ));
                    }
                    Err(error) => {
                        return AncestryVerdict::Unknown(format!(
                            "shallow merge-base said not-ancestor and the bounded deepen could not \
                             run: {error}"
                        ));
                    }
                    Ok(_) => {}
                }
                // Re-read immediately: the edge may be established even while
                // the repository is still shallow.
                match raw_ancestry(repo, ancestor, descendant) {
                    AncestryVerdict::Ancestor => return AncestryVerdict::Ancestor,
                    AncestryVerdict::Unknown(error) => return AncestryVerdict::Unknown(error),
                    AncestryVerdict::NotAncestor => {}
                }
                if !git_is_shallow(repo) {
                    // Complete history: the not-ancestor answer is established.
                    return AncestryVerdict::NotAncestor;
                }
                let next_count = git_reachable_commit_count(repo);
                if next_count == last_count {
                    return AncestryVerdict::Unknown(
                        "merge-base said not-ancestor in a shallow clone and the bounded deepen \
                         added no history (a shallow remote?); the edge cannot be confirmed"
                            .into(),
                    );
                }
                last_count = next_count;
            }
            AncestryVerdict::Unknown(
                "merge-base still says not-ancestor after the bounded deepens; shallow history is \
                 not complete"
                    .into(),
            )
        }
    }
}

fn git_output(repo: &Path, args: &[&str]) -> Result<String, String> {
    block_git(repo, args)
}

// ── granted-land process-group fence (r14 R1) ──────────────────────────────
//
// A granted land child runs in its own process group with kill_on_drop(false):
// if the Node dies mid-push the group is orphaned but can still update the
// remote. The Hub must never guess that outcome, so the Node keeps a DURABLE
// ledger (`<data_dir>/gate-land/land-<job>.json`, pgid + leader start tick):
// written right after the land child spawns, removed only once the whole group
// is verified gone, and fenced (TERM → bounded wait → KILL → ESRCH) at the next
// boot before any carrier is served. A read-only landing probe reports `absent`
// only with AUTHORITATIVE death evidence: a live kernel ESRCH on the group it
// spawned, or a durable DEATH CERTIFICATE (`dead-<job>.json`) written only when
// the kernel verified the group ESRCH. A missing or unreadable ledger/cert is
// Unknown — never proof the push is dead (r16 item 2, R5).

/// What the Node can establish about a granted land's push liveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LandPushLiveness {
    /// A live land group may still push the granted merge.
    MayPush,
    /// The exact attempt's land group is authoritatively dead (live ESRCH on the
    /// recorded group, or a durable death certificate).
    VerifiedDead,
    /// Nothing could be established (missing/unreadable ledger, no cert); the
    /// caller must hold, never attest absence.
    Unknown,
}

/// Ledger-only half of [`GateRegistry::land_push_liveness`] (no live run):
/// a present fence ledger (readable or not) says a group MAY push; with no
/// ledger the push is provably dead ONLY when a valid death certificate for
/// the exact job exists. Missing both, a stat/read/parse error, or a cert for a
/// different job is Unknown.
#[cfg(unix)]
fn land_ledger_liveness(dir: &std::path::Path, job_id: &str) -> LandPushLiveness {
    match std::fs::metadata(land_fence_path(dir, job_id)) {
        Ok(_) => LandPushLiveness::MayPush,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match read_land_group_dead_cert(dir, job_id) {
                Some(_) => LandPushLiveness::VerifiedDead,
                None => LandPushLiveness::Unknown,
            }
        }
        // A stat/read error on the ledger directory is Unknown.
        Err(_) => LandPushLiveness::Unknown,
    }
}

#[cfg(unix)]
fn land_fence_path(dir: &std::path::Path, job_id: &str) -> std::path::PathBuf {
    dir.join(format!("land-{}.json", land_slug(job_id)))
}

/// Durable DEATH CERTIFICATE path: written only after the kernel verified the
/// granted land group ESRCH, and the only durable evidence that lets a later
/// read-only probe attest the push cannot still land (r16 item 2, R5).
#[cfg(unix)]
fn land_dead_cert_path(dir: &std::path::Path, job_id: &str) -> std::path::PathBuf {
    dir.join(format!("dead-{}.json", land_slug(job_id)))
}

#[cfg(unix)]
fn land_slug(job_id: &str) -> String {
    job_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

/// Persist the death certificate for a verified-gone land group. Takes the
/// fence RECORD's provenance (job, generation, pgid, leader identity) so the
/// certificate attests the death of that EXACT attempt, plus an authoritative
/// timestamp. Atomic like the fence write; any error propagates (the caller
/// then keeps the fence record rather than lose the only proof).
#[cfg(unix)]
fn write_land_group_dead_cert(
    dir: &std::path::Path,
    record: &Value,
    pgid: i32,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let Some(job_id) = record.get("jobId").and_then(Value::as_str) else {
        return Err(std::io::Error::other(
            "land fence record has no jobId for the death certificate",
        ));
    };
    let mut cert = record.clone();
    cert["diedAt"] = json!(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    );
    cert["cause"] = json!("killpg-esrch");
    let path = land_dead_cert_path(dir, job_id);
    let temporary = dir.join(format!(".dead-{pgid}-{}.tmp", std::process::id()));
    std::fs::write(&temporary, serde_json::to_vec(&cert).unwrap_or_default())?;
    std::fs::rename(&temporary, &path)?;
    Ok(())
}

/// Read and VALIDATE a death certificate for the exact job. `None` covers a
/// missing file, any read/parse error, a job mismatch, or missing provenance
/// (pgid + leader identity) — every one of those is Unknown, never a valid
/// death attestation.
#[cfg(unix)]
fn read_land_group_dead_cert(dir: &std::path::Path, job_id: &str) -> Option<Value> {
    let text = std::fs::read_to_string(land_dead_cert_path(dir, job_id)).ok()?;
    let cert: Value = serde_json::from_str(&text).ok()?;
    if cert.get("jobId").and_then(Value::as_str) != Some(job_id) {
        return None;
    }
    cert.get("pgid")
        .and_then(Value::as_i64)
        .filter(|pgid| i32::try_from(*pgid).is_ok())?;
    LeaderIdentity::from_record(&cert)?;
    Some(cert)
}

/// Env var telling the granted land child to read its one start-release byte
/// from the given fd (its stdin, fd 0 — modern Rust's spawn fd hygiene closes
/// ambient fds, so the pipe read end is passed as the registered stdin). The
/// `remuda merge` CLI blocks on it before running (crates/remuda/src/cmd/
/// merge.rs); the Node writes the byte only once the durable fence record
/// exists (r15 R5 child-start barrier).
#[cfg(unix)]
const REMUDA_START_BARRIER_FD: &str = "REMUDA_START_BARRIER_FD";

/// One-shot child-start barrier backed by a pipe whose READ end is handed to
/// the land child as its stdin (std's spawn fd hygiene makes ambient fd
/// inheritance unreliable, so the read end is passed through the registered
/// stdio slot — always fd 0 in the child). The merge CLI blocks reading one
/// byte until the parent either releases it (fence durable) or drops the write
/// end (fencing failed → EOF → the CLI exits before touching git).
#[cfg(unix)]
enum LandStartBarrier {
    Ready {
        write: std::os::fd::OwnedFd,
        child_stdin: Option<std::process::Stdio>,
    },
    Failed(String),
}

#[cfg(unix)]
impl LandStartBarrier {
    fn new() -> std::io::Result<Self> {
        use std::fs::File;
        let (read, write) = nix::unistd::pipe()?;
        // nix hands back CLOEXEC fds; std registers the read end explicitly as
        // the child's stdin (fd 0), so no ambient-fd inheritance is relied on.
        let read_file = File::from(read);
        Ok(Self::Ready {
            write,
            child_stdin: Some(std::process::Stdio::from(read_file)),
        })
    }

    fn failed(error: std::io::Error) -> Self {
        Self::Failed(error.to_string())
    }

    /// Take the child stdio for the spawn (consumed once).
    fn take_child_stdin(&mut self) -> std::process::Stdio {
        match self {
            Self::Ready { child_stdin, .. } => {
                child_stdin.take().unwrap_or(std::process::Stdio::null())
            }
            Self::Failed(_) => std::process::Stdio::null(),
        }
    }

    /// Persist via `fence`, then release the waiting child with one byte. On
    /// any error the write end drops when self is consumed: the child's read
    /// returns EOF and the CLI exits without running.
    fn release_after<F>(self, fence: F) -> std::io::Result<()>
    where
        F: FnOnce() -> std::io::Result<()>,
    {
        match self {
            Self::Failed(error) => Err(std::io::Error::other(format!(
                "start barrier unavailable: {error}"
            ))),
            Self::Ready { write, .. } => {
                fence()?;
                nix::unistd::write(&write, &[1u8; 1])?;
                Ok(())
            }
        }
    }
}

/// Durable leader identity sampled when the land child spawns and re-checked
/// at every later boot. A signal is authorized ONLY by an equal identity at
/// the same pid (r15 R5).
#[cfg(unix)]
#[derive(Clone, PartialEq, Eq, Debug)]
enum LeaderIdentity {
    /// Linux clock ticks since boot (`/proc/<pid>/stat` field 22).
    StartTick(u64),
    /// Portable fallback on platforms without `/proc` (macOS): the `ps
    /// -o lstart=` start timestamp text, compared verbatim on the same host.
    StartedAt(String),
}

#[cfg(unix)]
impl LeaderIdentity {
    /// Read the identity recorded in a durable fence record. Absent/malformed
    /// is Unknown to the caller, never a match or a mismatch.
    fn from_record(value: &Value) -> Option<Self> {
        if let Some(tick) = value.get("startTick").and_then(Value::as_u64) {
            return Some(LeaderIdentity::StartTick(tick));
        }
        value
            .get("startTime")
            .and_then(Value::as_str)
            .map(|text| LeaderIdentity::StartedAt(text.to_owned()))
    }

    /// Write the identity into the fence JSON.
    fn write(&self, record: &mut Value) {
        match self {
            LeaderIdentity::StartTick(tick) => record["startTick"] = json!(*tick),
            LeaderIdentity::StartedAt(text) => record["startTime"] = json!(text),
        }
    }
}

/// Sample the leader's start-time identity. Returns `None` whenever it cannot
/// be read (missing `/proc`, `ps` unavailable/failed, unparseable output) —
/// `None` means Unknown: the callers never treat it as a match (no signal) or a
/// mismatch (r15 R5).
#[cfg(unix)]
fn sample_leader_identity(pid: i32) -> Option<LeaderIdentity> {
    #[cfg(target_os = "linux")]
    {
        // /proc/<pid>/stat: field 22 (index 19 after the comm field) is the
        // start time in clock ticks. Pairs with the pid to defeat pid reuse.
        let stat = std::fs::read_to_string(proc_root().join(format!("{pid}/stat"))).ok()?;
        let after_comm = stat.rsplit_once(')').map(|(_, rest)| rest)?;
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        fields
            .get(19)
            .and_then(|value| value.parse().ok())
            .map(LeaderIdentity::StartTick)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Portable process start time: `ps -o lstart= -p <pid>` prints one
        // canonical timestamp line; the same host sampled both values, so a
        // byte comparison is an identity check without assuming /proc.
        let output = std::process::Command::new("ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        (!text.is_empty()).then_some(LeaderIdentity::StartedAt(text))
    }
}

/// Root of the process-info filesystem (Linux `/proc`). Overridable under
/// test-faults to simulate a platform where it is missing/unreadable (macOS),
/// which every consumer must treat as Unknown — never as an empty enumeration.
/// An RwLock makes an override globally exclusive: a read on any OTHER thread
/// blocks until the override drops, so parallel tests that spawn real children
/// never see a ghost `/proc`.
#[cfg(all(unix, any(test, feature = "test-faults")))]
fn proc_root() -> std::path::PathBuf {
    if let Some(local) = PROC_OVERRIDE_LOCAL.with(|cell| cell.take()) {
        PROC_OVERRIDE_LOCAL.with(|cell| cell.set(Some(local.clone())));
        return local;
    }
    PROC_ROOT_RW
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("/proc"))
}

#[cfg(all(unix, not(any(test, feature = "test-faults"))))]
fn proc_root() -> std::path::PathBuf {
    std::path::PathBuf::from("/proc")
}

#[cfg(all(unix, any(test, feature = "test-faults")))]
static PROC_ROOT_RW: std::sync::RwLock<Option<std::path::PathBuf>> = std::sync::RwLock::new(None);

// Reentrant per-thread view of the PROC_ROOT_RW override, so a thread
// holding the write lock still reads its own override without deadlocking.
#[cfg(all(unix, any(test, feature = "test-faults")))]
thread_local! {
    static PROC_OVERRIDE_LOCAL: std::cell::Cell<Option<std::path::PathBuf>> =
        const { std::cell::Cell::new(None) };
}

/// True iff a process group with `pgid` still exists (signal 0 probe). EPERM
/// means a member exists owned by another uid; ESRCH means the group is gone.
#[cfg(unix)]
fn land_group_exists(pgid: i32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::killpg;
    use nix::unistd::Pid;
    match killpg(Pid::from_raw(pgid), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        Err(_) => true,
    }
}

/// Persist the granted land group fence. Every failure (missing identity,
/// directory/temp/rename error) propagates: the caller has the land child
/// stopped before exec and refuses to release it until this record is durable
/// (child-start barrier, r15 R5). A missing record can never be silently
/// skipped into an unfenced, possibly-orphaned pusher.
#[cfg(unix)]
fn write_land_fence(
    dir: &std::path::Path,
    job_id: &str,
    generation: u32,
    pgid: i32,
    identity: &LeaderIdentity,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut record = serde_json::json!({
        "jobId": job_id,
        "generation": generation,
        "pgid": pgid,
    });
    identity.write(&mut record);
    let path = land_fence_path(dir, job_id);
    // A new granted attempt supersedes any death certificate for the same job
    // (e.g. a requeue): the old cert must never attest death for THIS attempt.
    let _ = std::fs::remove_file(land_dead_cert_path(dir, job_id));
    let temporary = dir.join(format!(".land-{pgid}-{}.tmp", std::process::id()));
    std::fs::write(&temporary, serde_json::to_vec(&record).unwrap_or_default())?;
    std::fs::rename(&temporary, &path)?;
    Ok(())
}

/// Prefix of a durable live land-group fence ledger (`land-<slug>.json`).
/// Death certificates use a different `dead-` prefix so the startup fence
/// never treats a certificate as an actionable fence (r17 item 1).
#[cfg(unix)]
const LAND_FENCE_FILE_PREFIX: &str = "land-";

/// True only for a live-fence ledger file name (`land-<slug>.json`), never a
/// `dead-<slug>.json` certificate or a temp file.
#[cfg(unix)]
fn is_land_fence_file_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    name.starts_with(LAND_FENCE_FILE_PREFIX) && name.ends_with(".json")
}

/// Whether two fence/cert records describe the EXACT same granted attempt:
/// job, generation, process group, and durable leader identity. Used to refuse
/// certifying a LIVE REPLACEMENT group from a stale observation (r17 item 1).
#[cfg(unix)]
fn land_fence_snapshot_matches(a: &Value, b: &Value) -> bool {
    if a.get("jobId").and_then(Value::as_str) != b.get("jobId").and_then(Value::as_str)
        || a.get("generation").and_then(Value::as_u64)
            != b.get("generation").and_then(Value::as_u64)
        || a.get("pgid").and_then(Value::as_i64) != b.get("pgid").and_then(Value::as_i64)
    {
        return false;
    }
    match (
        LeaderIdentity::from_record(a),
        LeaderIdentity::from_record(b),
    ) {
        (Some(identity_a), Some(identity_b)) => identity_a == identity_b,
        _ => false,
    }
}

/// Authoritative group death has been observed (killpg ESRCH on the recorded
/// pgid): persist the death certificate FROM THE OBSERVED FENCE SNAPSHOT, and
/// only then remove the fence (r16 item 2; r17 item 1 hardening).
///
/// Returns `Ok(true)` when the cert was written and the fence removed, and
/// `Ok(false)` when the observation must NOT certify (the fence is gone or no
/// longer matches the observed snapshot — a LIVE REPLACEMENT group re-fenced
/// the same job slug). Any other I/O failure propagates as `Err` so the caller
/// keeps the fence (Unknown never silently loses its proof). The fence is
/// re-read and compared to the observed snapshot immediately before writing; a
/// stale observation (e.g. a leftover death certificate for a recycled pgid)
/// can never certify a different, live group.
#[cfg(unix)]
fn certify_dead_and_remove_fence(
    dir: &std::path::Path,
    observed: &Value,
    observed_pgid: i32,
) -> std::io::Result<bool> {
    let Some(job_id) = observed.get("jobId").and_then(Value::as_str) else {
        return Ok(false);
    };
    // The observation must be a complete live-fence snapshot, never a
    // certificate or a record missing provenance.
    if observed.get("pgid").and_then(Value::as_i64) != Some(i64::from(observed_pgid))
        || observed.get("generation").and_then(Value::as_u64).is_none()
        || LeaderIdentity::from_record(observed).is_none()
        || observed.get("diedAt").is_some()
        || observed.get("cause").is_some()
    {
        return Ok(false);
    }
    let fence_path = land_fence_path(dir, job_id);
    let current_text = match std::fs::read_to_string(&fence_path) {
        Ok(text) => text,
        // The fence is already gone: never manufacture a certificate from a
        // stale in-memory observation (another path owns the current state).
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let current: Value = serde_json::from_str(&current_text)
        .map_err(|_| std::io::Error::other("land fence record is unreadable at certification"))?;
    if !land_fence_snapshot_matches(&current, observed) {
        tracing::warn!(
            job_id,
            observed_pgid,
            "land fence changed since the death observation; refusing to certify a possible live \
             replacement group"
        );
        return Ok(false);
    }
    write_land_group_dead_cert(dir, observed, observed_pgid)?;
    match std::fs::remove_file(&fence_path) {
        Ok(()) => Ok(true),
        // A concurrent same-process removal after our re-read: the cert
        // attests the exact snapshot that is now gone, which is consistent.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => {
            // The fence still exists and the unlink failed: roll the
            // certificate back so the fence remains the single, conservative
            // proof (fence presence reads as MayPush), then surface the error.
            let _ = std::fs::remove_file(land_dead_cert_path(dir, job_id));
            Err(error)
        }
    }
}

/// Boundedly poll until the kernel reports the group gone (`killpg(pgid,0)`
/// == ESRCH) or the budget elapses. No /proc enumeration participates: only
/// the kernel's authoritative answer can prove death (r15 R5).
#[cfg(unix)]
fn wait_until_group_gone(pgid: i32, budget: Duration) {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if !land_group_exists(pgid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// SIGTERM a whole land group, boundedly wait for the GROUP ITSELF to disappear
/// (killpg ESRCH), then ALWAYS SIGKILL the WHOLE group with killpg (never an
/// enumeration that could miss a member and never an early return on an empty
/// scan), and verify the group is gone by ESRCH again. Returns false only when
/// the kernel still cannot report ESRCH at the deadline (the fence record is
/// kept for the next boot's fence). A missing `/proc` or a transient empty
/// enumeration can never skip the KILL — no enumeration is consulted at all
/// (r14 item 4, r15 R5).
#[cfg(unix)]
fn terminate_and_verify_group(pgid: i32, term_grace: Duration) -> bool {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    let _ = killpg(Pid::from_raw(pgid), Some(Signal::SIGTERM));
    wait_until_group_gone(pgid, term_grace);
    // Still occupied or merely UNPROVEN at the deadline: unconditionally KILL
    // the whole group. This is never skipped — even when the TERM-wait poll
    // already saw ESRCH, the extra KILL is harmless (ESRCH) and costs nothing
    // compared with missing a member one /proc read away.
    let _ = killpg(Pid::from_raw(pgid), Some(Signal::SIGKILL));
    wait_until_group_gone(pgid, Duration::from_millis(500));
    // Authoritative death claim only.
    !land_group_exists(pgid)
}

/// r14 R1 / r15 R5 startup fence: every leftover record names a granted land
/// group the PREVIOUS Node process may have orphaned mid-push. Every decision
/// is Established or Unknown:
/// * record unreadable/unparseable/missing fields/missing identity → Unknown,
///   KEEP the record (never delete the only proof, never signal);
/// * leader pid ALIVE with the SAME durable start identity → positively ours;
///   terminate the whole group (TERM → bounded wait → KILL → ESRCH) and remove
///   the record only once death is authoritative;
/// * leader pid alive with a DIFFERENT identity (pid reused by a stranger) →
///   no signal; group ownership can no longer be positively established → KEEP
///   the record (the Landing stays outcome-unknown and the remote resolves it);
/// * leader pid ESRCH and the group ESRCH → established dead → remove;
/// * leader pid ESRCH but the group still exists (orphaned members, no leader)
///   → Unknown without the leader identity → no signal, KEEP the record;
/// * `kill(pid, 0)` EPERM/other → Unknown → keep, never signal.
#[cfg(unix)]
fn fence_orphaned_land_groups(dir: &std::path::Path) {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // r17 item 1: scan ONLY live-fence ledgers (`land-*.json`). A leftover
        // `dead-*.json` certificate must never be read as an actionable fence:
        // its pgid may have been reused by a LIVE replacement group, and acting
        // on it could certify that replacement dead. Certs are consumed only
        // via read_land_group_dead_cert by the read-only probe.
        if !is_land_fence_file_name(&entry.file_name()) {
            continue;
        }
        // Read/parse/field failures all mean the record cannot be acted on:
        // KEEP it. Deleting an unparseable record used to destroy the only
        // proof that a group might push (r15 R5 fail-open).
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        // Defense in depth: never act on a certificate even if one was
        // mis-named with the fence prefix.
        if record.get("diedAt").is_some() || record.get("cause").is_some() {
            continue;
        }
        let Some(pgid) = record["pgid"]
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
        else {
            continue;
        };
        let Some(recorded_identity) = LeaderIdentity::from_record(&record) else {
            continue;
        };
        let Some(_job) = record.get("jobId").and_then(Value::as_str) else {
            continue;
        };
        match kill(Pid::from_raw(pgid), None) {
            Ok(()) => {
                // A live process holds the leader pid. Signal ONLY on a
                // positive identity match.
                match sample_leader_identity(pgid) {
                    Some(current) if current == recorded_identity => {
                        if terminate_and_verify_group(pgid, LAND_GROUP_TERM_GRACE) {
                            match certify_dead_and_remove_fence(dir, &record, pgid) {
                                Ok(true) | Ok(false) => {}
                                Err(error) => tracing::warn!(
                                    %error,
                                    pgid,
                                    "certifying the terminated land group failed; keeping the fence"
                                ),
                            }
                        }
                        // Still not provably gone: keep for the next boot.
                    }
                    Some(_) => {
                        tracing::warn!(
                            pgid,
                            "land fence leader pid was reused by an unrelated process; refusing \
                             to signal the group and keeping the fence record"
                        );
                    }
                    None => {
                        tracing::warn!(
                            pgid,
                            "land fence leader is alive but its identity cannot be read; refusing \
                             to signal the group and keeping the fence record"
                        );
                    }
                }
            }
            Err(nix::errno::Errno::ESRCH) => {
                // No process at the leader pid. With no leader, a still-present
                // group can no longer be positively distinguished from a
                // same-session process that joined it: only an authoritative
                // group ESRCH settles it; anything else keeps the record.
                if !land_group_exists(pgid) {
                    match certify_dead_and_remove_fence(dir, &record, pgid) {
                        Ok(true) | Ok(false) => {}
                        Err(error) => tracing::warn!(
                            %error,
                            pgid,
                            "certifying the dead land group failed; keeping the fence"
                        ),
                    }
                }
            }
            Err(error) => {
                // EPERM (a process owned by another uid) or any other probe
                // failure: Unknown ownership — no signal, keep.
                tracing::warn!(%error, pgid, "land fence leader probe failed; keeping record");
            }
        }
    }
}

/// Bounded grace a land child's process group gets to exit after SIGTERM
/// before SIGKILL. Short enough that a timed-out land returns promptly, long
/// enough for a well-behaved git/ssh to finish a clean shutdown.
#[cfg(unix)]
const LAND_GROUP_TERM_GRACE: Duration = Duration::from_millis(750);

/// Pids currently alive (non-zombie) whose process group is `pgid` (Linux
/// `/proc` enumeration). TEST/DIAGNOSTIC ONLY: it is never consulted to prove
/// death or to skip a signal — production termination claims death only on the
/// kernel's authoritative `killpg(pgid, 0)` ESRCH, so an empty scan (or a
/// platform without `/proc`) can never be read as "cleared" (r15 R5).
#[cfg(all(unix, any(test, feature = "test-faults")))]
pub(crate) fn alive_pids_in_group(pgid: i32) -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir(proc_root()) else {
        return Vec::new();
    };
    let mut pids = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        // /proc/<pid>/stat: after the comm field '(…)', field 1 is state and
        // field 3 is the process group. comm may contain spaces/parens, so
        // split from the last ')'.
        let Ok(stat) = std::fs::read_to_string(proc_root().join(format!("{pid}/stat"))) else {
            continue;
        };
        let Some(after_comm) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
            continue;
        };
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        let state = fields.first().copied().unwrap_or("");
        let pgrp = fields.get(2).and_then(|v| v.parse::<i32>().ok());
        if pgrp == Some(pgid) && state != "Z" {
            pids.push(pid);
        }
    }
    pids
}

/// Terminate a whole land process group SYNCHRONOUSLY: SIGTERM the group,
/// boundedly wait for every non-zombie member (including a git/ssh descendant)
/// to exit, then SIGKILL the survivors. The caller reaps its own leader after
/// this returns (a killed leader is a zombie until that wait). r13 item 5 — a
/// detached 500 ms KILL used to leave a SIGTERM-ignoring descendant free to
/// keep pushing while the Node reported a verdict.
/// Terminate a whole process group SYNCHRONOUSLY with the guaranteed sequence
/// (r14 item 4): SIGTERM the whole group (killpg, never a per-pid /proc list),
/// wait boundedly for the group itself to disappear (`killpg(pgid, 0)` ==
/// ESRCH — the kernel's own, authoritative answer, so a missing /proc or a
/// transient empty enumeration can never skip the kill), then SIGKILL the WHOLE
/// group unconditionally and wait again until ESRCH is proven. Returning early
/// on the first empty scan used to let a SIGTERM-ignoring descendant (one
/// /proc read away from the signal, or hidden by a /proc hiccup) survive and
/// keep pushing. The caller reaps the group leader after this returns.
#[cfg(unix)]
fn terminate_group(pgid: i32) {
    let _ = terminate_and_verify_group(pgid, LAND_GROUP_TERM_GRACE);
}

/// Test-only accessor for the process-group liveness scan (r13 item 5).
#[cfg(unix)]
#[doc(hidden)]
#[cfg(any(test, feature = "test-faults"))]
pub fn gate_test_alive_pids_in_group(pgid: i32) -> Vec<i32> {
    alive_pids_in_group(pgid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Test-only GateAuthorizer that always authorizes and grants, so the
    /// in-module runner tests reach execute_gate without a live carrier.
    /// Exists only under cfg(test); a production DevNode has no such default.
    struct TestAllowAuthorizer;
    impl GateAuthorizer for TestAllowAuthorizer {
        fn authorize<'a>(
            &'a self,
            _job_id: &'a str,
            _generation: u32,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
            Box::pin(std::future::ready(true))
        }
        fn request_land_grant<'a>(
            &'a self,
            _job_id: &'a str,
            _generation: u32,
            _merge_sha: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = LandGrantAnswer> + Send + 'a>>
        {
            Box::pin(std::future::ready(LandGrantAnswer {
                granted: true,
                landing_in_progress: false,
            }))
        }
    }

    /// At most [`MAX_IN_FLIGHT_LONG_METHODS`] long methods run at once.
    ///
    /// This is the property that keeps a burst of long methods from parking
    /// every runtime worker — and so the carrier loop — on a small host. The
    /// bodies here hold until the test says otherwise, so without the cap all
    /// of them would be inside `body` simultaneously.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_long_method_cap_bounds_concurrency() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        // Bodies block on this rather than sleeping, so "is it in the body" is
        // observable rather than a timing race. A std channel is the honest
        // primitive here: the bodies are on blocking threads and must not need
        // the async runtime to make progress.
        let (release, released) = std::sync::mpsc::channel::<()>();
        let released = Arc::new(std::sync::Mutex::new(released));
        let total = MAX_IN_FLIGHT_LONG_METHODS + 3;

        let mut handles = Vec::new();
        for _ in 0..total {
            let running = Arc::clone(&running);
            let peak = Arc::clone(&peak);
            let released = Arc::clone(&released);
            handles.push(tokio::spawn(async move {
                run_long_method(move || {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    // Blocking wait: this is a blocking thread, and the body
                    // must not yield back to the runtime.
                    let _ = released
                        .lock()
                        .expect("release receiver lock")
                        .recv_timeout(std::time::Duration::from_secs(30));
                    running.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
                .await
            }));
        }

        // Give every permitted body time to enter, then confirm the cap holds.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            running.load(Ordering::SeqCst),
            MAX_IN_FLIGHT_LONG_METHODS,
            "exactly the cap's worth of bodies should be running"
        );

        // Releasing drains the queue without ever exceeding the cap.
        for _ in 0..total {
            let _ = release.send(());
        }
        for handle in handles {
            handle.await.unwrap().unwrap();
        }
        assert_eq!(
            peak.load(Ordering::SeqCst),
            MAX_IN_FLIGHT_LONG_METHODS,
            "peak concurrency must equal the cap: more means the cap leaked, \
             fewer means the bodies never overlapped"
        );
    }

    /// A body that panics must not leak its permit.
    ///
    /// The panic is caught and surfaced as an error (that is what keeps a
    /// panicking carrier method from taking the Node down), so the property to
    /// prove is that the permit still comes back: run *more than the cap* of
    /// panicking bodies concurrently and require every one to finish. A leaked
    /// permit would strand the excess past the cap and the deadline would fire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicking_long_method_releases_its_permit() {
        // The expected panics are noise here; the assertion is what matters.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let total = MAX_IN_FLIGHT_LONG_METHODS + 3;
        let mut handles = Vec::new();
        for _ in 0..total {
            handles.push(tokio::spawn(async {
                run_long_method(|| -> Result<(), crate::NodeError> {
                    panic!("body panicked");
                })
                .await
            }));
        }
        let drained = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            for handle in handles {
                let result = handle
                    .await
                    .expect("the spawned task itself does not panic");
                assert!(
                    result.is_err(),
                    "a panicking body is reported as an error, not a value"
                );
            }
        })
        .await;
        std::panic::set_hook(previous);
        drained.expect("a leaked permit would strand the excess bodies past the deadline");
    }

    /// A body that errors returns the error rather than panicking the caller.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failing_long_method_returns_its_error() {
        let result: Result<(), crate::NodeError> =
            run_long_method(|| Err(crate::NodeError::InvalidRequest("nope".into()))).await;
        assert!(matches!(result, Err(crate::NodeError::InvalidRequest(_))));
    }

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
        // r12 item 5: production fails closed with no carrier authorizer, so
        // the in-module runner tests must install an explicit test-only
        // allow authorizer (unreachable from a production DevNode) to reach
        // execute_gate at all.
        *node.gate_registry().authorizer.try_write().unwrap() =
            Some(std::sync::Arc::new(TestAllowAuthorizer));
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
            initiator: None,
            generation: 0,
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
scratch=$(mktemp -d "${TMPDIR:-/tmp}/remuda-mq-$$-0-XXXXXX")
trap 'rm -rf "$scratch"' EXIT
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
scratch=$(mktemp -d "${TMPDIR:-/tmp}/remuda-mq-$$-0-XXXXXX")
trap 'rm -rf "$scratch"' EXIT
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
scratch=$(mktemp -d "${TMPDIR:-/tmp}/remuda-mq-$$-0-XXXXXX")
trap 'rm -rf "$scratch"' EXIT
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
            !git(&fixture.lane, &["worktree", "list"]).contains("remuda-mq-"),
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

    /// A `gate.jsonl` under ANOTHER merge's pid scratch root must never be
    /// streamed into this job: the tailer is scoped to the child pid this
    /// runner spawned. Re-exec'd under a private TMPDIR (see the inner test)
    /// so the planted foreign root never lands in the host's ambient temp dir.
    #[test]
    fn a_foreign_runs_gate_report_is_not_streamed() {
        let scratch = TempDir::new().expect("private TMPDIR");
        let exe = std::env::current_exe().expect("test binary");
        let output = std::process::Command::new(exe)
            .args([
                "--exact",
                "gate::tests::foreign_gate_report_is_not_streamed_inner",
                "--ignored",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env("TMPDIR", scratch.path())
            .output()
            .expect("re-exec the foreign-report test under a private TMPDIR");
        assert!(
            output.status.success(),
            "foreign-report test broke under a private TMPDIR\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("1 passed"),
            "the inner foreign-report test did not run: {stdout}"
        );
    }

    /// Inner half of [`a_foreign_runs_gate_report_is_not_streamed`]: runs in a
    /// re-exec'd process whose TMPDIR is private, so the foreign root it
    /// plants can neither collide with another test run's nor pollute the
    /// host ambient temp dir.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "re-exec'd by a_foreign_runs_gate_report_is_not_streamed under a private TMPDIR"]
    async fn foreign_gate_report_is_not_streamed_inner() {
        let fixture = fixture(PASSING_SCRIPT);
        // Simulate a concurrent gate on this host: fresh, mtime-fresh report
        // under a pid this runner did not spawn.
        let foreign = std::env::temp_dir().join("remuda-mq-999999-0-1");
        let _ = std::fs::remove_dir_all(&foreign);
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(
            foreign.join("gate.jsonl"),
            "{\"name\":\"foreign-other-gate\",\"status\":\"ok\",\"durationMs\":99,\"attempts\":1,\"retried\":false}\n",
        )
        .unwrap();
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
        let mut streamed = Vec::new();
        while let Ok(Ok(event)) =
            tokio::time::timeout(Duration::from_millis(300), events.recv()).await
        {
            if let GateEventKind::Step { step } = event.kind {
                streamed.push(step.name);
            }
        }
        std::fs::remove_dir_all(&foreign).ok();
        assert!(
            streamed.iter().all(|name| name != "foreign-other-gate"),
            "a foreign gate's step leaked into this job's stream: {streamed:?}"
        );
        assert!(
            streamed.iter().any(|name| name == "cargo-test"),
            "the run's own steps must still stream: {streamed:?}"
        );
    }

    /// Regression for hosts where the coordinator exports `TMPDIR` outside
    /// `/tmp` (the shared build host): the fake gate must flush its report
    /// under `$TMPDIR` — the same root the runner's `std::env::temp_dir()`
    /// tailer scans — or no Step events stream. Process-env mutation is
    /// forbidden workspace-wide, so re-exec this binary with TMPDIR redirected
    /// to a private scratch dir and run the ignored inner test there.
    #[test]
    fn step_streaming_honours_a_redirected_tmpdir() {
        let scratch = TempDir::new().expect("private TMPDIR");
        let exe = std::env::current_exe().expect("test binary");
        let output = std::process::Command::new(exe)
            .args([
                "--exact",
                "gate::tests::verify_streams_steps_with_redirected_tmpdir",
                "--ignored",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env("TMPDIR", scratch.path())
            .output()
            .expect("re-exec the gate streaming test under a redirected TMPDIR");
        assert!(
            output.status.success(),
            "gate step streaming broke under a redirected TMPDIR\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        // Guard against the inner test silently not running.
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("1 passed"),
            "the inner TMPDIR streaming test did not run: {stdout}"
        );
    }

    /// Inner half of [`step_streaming_honours_a_redirected_tmpdir`]: runs in a
    /// re-exec'd process whose TMPDIR is a private, non-`/tmp` directory. The
    /// gate child inherits it through `host_env_snapshot`, so the fake gate's
    /// mktemp and the in-process report tailer must resolve the same root.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "re-exec'd by step_streaming_honours_a_redirected_tmpdir with TMPDIR redirected"]
    async fn verify_streams_steps_with_redirected_tmpdir() {
        let tmpdir = std::env::var_os("TMPDIR").expect("outer test must export TMPDIR");
        assert_ne!(
            std::path::Path::new(&tmpdir),
            std::path::Path::new("/tmp"),
            "the regression needs a TMPDIR that is not /tmp"
        );
        assert_eq!(
            std::env::temp_dir(),
            std::path::PathBuf::from(&tmpdir),
            "the report tailer scans $TMPDIR"
        );

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
        let mut streamed = Vec::new();
        while let Ok(Ok(event)) =
            tokio::time::timeout(Duration::from_millis(300), events.recv()).await
        {
            if let GateEventKind::Step { step } = event.kind {
                streamed.push(step.name);
            }
        }
        assert!(
            !streamed.is_empty(),
            "no steps streamed under the redirected TMPDIR: {streamed:?}"
        );
        assert!(
            streamed.iter().any(|name| name == "cargo-test"),
            "{streamed:?}"
        );

        // The streamed steps prove the tailer scanned $TMPDIR (not hardcoded
        // /tmp); the fake gate also removes its scratch root on exit, so none
        // of the pid-named dirs may remain.
        let leftovers = std::fs::read_dir(&tmpdir)
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("remuda-mq-")
            })
            .count();
        assert_eq!(
            leftovers, 0,
            "the fake gate must remove its scratch root under $TMPDIR"
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
        // r11 item 1: two children — verify (--gate --no-push) then land
        // (--land). The script records every argv line and answers "verified"
        // for the verify phase, "landed" for the land phase.
        let script = r#"
echo "$@" >> "$(dirname "$0")/args.txt"
base=$(git rev-parse main 2>/dev/null || echo "1111111111111111111111111111111111111111")
head=$(git rev-parse wt/fake/task 2>/dev/null || echo "$base")
if printf '%s\n' "$@" | grep -q -- '--land'; then
  echo "{\"exitCode\":0,\"status\":\"landed\",\"merged\":\"$head\",\"currentMain\":\"$head\",\"steps\":[]}"
else
  echo "{\"exitCode\":0,\"status\":\"verified\",\"base\":\"$base\",\"head\":\"$head\",\"merged\":\"$head\",\"steps\":[]}"
fi
"#;
        let fixture = fixture(script);
        let mut p = params(&fixture, "land");
        p.push = true;
        // Direct DevNode has no carrier, so the Hub-land-grant stage is skipped
        // (local floor). The fake cannot perform a real --land (it does not
        // persist a gate verify report), so this unit test asserts the
        // TWO-PHASE ARG SPLIT from argv (a verify child and a land child are
        // both spawned); the real bare-repo verify→land round trip is covered
        // end-to-end through the stdio/real-gate.sh harness.
        let _ = fixture.node.run_gate_typed(p).await;
        let argv = std::fs::read_to_string(fixture.bin.parent().unwrap().join("args.txt")).unwrap();
        // A verify child (--gate --no-push) and a separate land child
        // (--land, no --gate).
        assert!(argv.contains("--land"), "argv: {argv}");
        assert!(
            argv.lines()
                .any(|line| line.contains("--gate") && line.contains("--no-push")),
            "a verify child with --gate --no-push must run first: {argv}"
        );
        assert!(
            !argv
                .lines()
                .any(|line| line.contains("--land") && line.contains("--no-push")),
            "the land child must push: {argv}"
        );
    }

    /// The fake CLI speaks the production child-start barrier protocol: the
    /// verify child must NOT see the barrier variable; the LAND child must see
    /// REMUDA_START_BARRIER_FD=0, block on one stdin byte before any git work,
    /// and exit 4 on EOF (the real `remuda merge` does exactly this).
    const BARRIER_LAND_SCRIPT: &str = r#"
dir=$(dirname "$0")
echo "$*" >> "$dir/args.txt"
base=$(git rev-parse main 2>/dev/null || echo "1111111111111111111111111111111111111111")
head=$(git rev-parse wt/fake/task 2>/dev/null || echo "$base")
if ! printf '%s\n' "$*" | grep -q -- '--land'; then
  if [ "${REMUDA_START_BARRIER_FD+x}" = x ]; then
    echo "verify child must not carry the start-barrier var" >&2
    exit 97
  fi
  echo "{\"exitCode\":0,\"status\":\"verified\",\"base\":\"$base\",\"head\":\"$head\",\"merged\":\"$head\",\"steps\":[]}"
  exit 0
fi
if [ "${REMUDA_START_BARRIER_FD-}" != "0" ]; then
  echo "land child lost REMUDA_START_BARRIER_FD after env_clear" >&2
  exit 98
fi
head -c 1 >/dev/null || { echo "start barrier closed before release" >&2; exit 4; }
echo released > "$dir/released.marker"
echo "{\"exitCode\":0,\"status\":\"landed\",\"merged\":\"$head\",\"currentMain\":\"$head\",\"steps\":[]}"
"#;

    /// Read main from the fixture's REAL bare origin.
    fn origin_main(fixture: &Fixture) -> String {
        let origin = fixture._bare.path().join("origin.git");
        String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["--git-dir", origin.to_str().unwrap(), "rev-parse", "main"])
                .output()
                .expect("read origin main")
                .stdout,
        )
        .trim()
        .to_owned()
    }

    /// r16 item 1: through the PRODUCTION spawn builder, the granted land
    /// child survives env_clear with its start-barrier variable, parks on the
    /// barrier while the durable fence write is LATCHED, and when that
    /// persistence FAILS it is killed (never released) and the run reports
    /// UNCERTAIN — never a definite failure — and nothing pushed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn land_fence_persistence_failure_holds_child_and_reports_uncertain() {
        let fixture = fixture(BARRIER_LAND_SCRIPT);
        let base_main = origin_main(&fixture);
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        fixture
            .node
            .gate_registry()
            .set_land_fence_fault(LandFenceFault {
                entered: entered.clone(),
                release: release.clone(),
                fail: true,
            });
        let mut p = params(&fixture, "land");
        p.push = false;
        let node = fixture.node.clone();
        let run = tokio::spawn(async move { node.run_gate_typed(p).await });
        // The fence-persistence closure has ENTERED: the land child spawned
        // and is parked behind its barrier (the release byte is unsent).
        tokio::time::timeout(Duration::from_secs(15), entered.notified())
            .await
            .expect("the latched fence persistence was entered");
        let argv_path = fixture.bin.parent().unwrap().join("args.txt");
        // The child records argv immediately before parking on the barrier;
        // wait for that line (the persistence latch fires as soon as the
        // parent spawned, which can beat the child's first instruction).
        let argv = {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let text = std::fs::read_to_string(&argv_path).unwrap_or_default();
                if text.lines().any(|line| line.contains("--land")) {
                    break text;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the land child never recorded its argv: {text}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        assert!(
            argv.lines().any(|line| line.contains("--land")),
            "the land child spawned through the production builder: {argv}"
        );
        assert!(
            !fixture
                .bin
                .parent()
                .unwrap()
                .join("released.marker")
                .exists(),
            "the child must not pass the barrier while the fence is unpersisted"
        );
        assert_eq!(
            origin_main(&fixture),
            base_main,
            "nothing pushed while held"
        );
        // Release the latch as a PERSISTENCE FAILURE.
        {
            let (lock, cvar) = &*release;
            let mut done = lock.lock().unwrap();
            *done = true;
            cvar.notify_one();
        }
        let result = tokio::time::timeout(Duration::from_secs(30), run)
            .await
            .expect("the run finished")
            .expect("the run task joined")
            .expect("run_gate returned");
        assert_eq!(
            result.status, "uncertain",
            "a fence persistence failure with the child held is Unknown, never failed: {result:?}"
        );
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|text| text.contains("durably record")),
            "the error names the fence persistence failure: {:?}",
            result.error
        );
        assert!(
            !fixture
                .bin
                .parent()
                .unwrap()
                .join("released.marker")
                .exists(),
            "the child must never be released after the fence failed"
        );
        assert_eq!(
            origin_main(&fixture),
            base_main,
            "a fenced-off child never pushes"
        );
    }

    /// r16 item 1: when the start barrier itself cannot be set up, the land
    /// child is NOT SPAWNED AT ALL and the run reports UNCERTAIN (the Hub
    /// reconciles the held grant), instead of spawning an unblocked pusher and
    /// reporting a definite failure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_start_barrier_setup_failure_refuses_to_spawn_and_reports_uncertain() {
        let fixture = fixture(BARRIER_LAND_SCRIPT);
        let base_main = origin_main(&fixture);
        fixture.node.gate_registry().set_force_barrier_fault(true);
        let mut p = params(&fixture, "land");
        p.push = false;
        let result = fixture.node.run_gate_typed(p).await.unwrap();
        assert_eq!(
            result.status, "uncertain",
            "an uncreatable barrier means no spawn + Unknown: {result:?}"
        );
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|text| text.contains("NOT started")),
            "the error states the child was not started: {:?}",
            result.error
        );
        let argv = std::fs::read_to_string(fixture.bin.parent().unwrap().join("args.txt"))
            .unwrap_or_default();
        assert!(
            !argv.lines().any(|line| line.contains("--land")),
            "the land child must never spawn without a barrier: {argv}"
        );
        assert!(
            !fixture
                .bin
                .parent()
                .unwrap()
                .join("released.marker")
                .exists(),
            "no land phase may run"
        );
        assert_eq!(origin_main(&fixture), base_main, "nothing was pushed");
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
        // Latched, bounded hold instead of a fixed sleep: park the run at the
        // handler entry (before run registration / lane / git), so the cancel is
        // guaranteed to arrive before the run reads its canceled tombstone.
        let hold = GateRunHold::new();
        node.gate_registry().arm_run_hold(hold.clone()).await;
        let running = tokio::spawn(async move { node.run_gate_typed(p).await });
        hold.wait_arrived().await;
        let canceled = fixture
            .node
            .cancel_gate(&serde_json::json!({ "jobId": job_id }))
            .await
            .unwrap();
        assert_eq!(canceled["ok"], true);
        hold.release();
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stale_generation_cancel_never_signals_the_newer_live_attempt() {
        // r15 item 2: the job slot is reused by attempt 2. A delayed
        // gate.cancel frame naming attempt 1 must record its tombstone but
        // neither set attempt 2's stand-down flag nor signal its group. The
        // matching-generation cancel (2) then does.
        let fixture = fixture("sleep 30\necho '{\"exitCode\":0,\"status\":\"verified\"}'");
        let node = fixture.node.clone();
        let mut p = params(&fixture, "verify");
        p.generation = 2;
        let job_id = p.job_id.clone();
        let running = tokio::spawn(async move { node.run_gate_typed(p).await });
        // Bounded latch: the live gen-2 run is registered with a group.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some((generation, canceled)) =
                fixture.node.gate_test_live_run_state(&job_id).await
            {
                assert_eq!(generation, 2);
                assert!(!canceled);
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("gen-2 run never registered");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // The delayed cancel for the OLDER attempt 1.
        fixture
            .node
            .cancel_gate(&serde_json::json!({ "jobId": job_id, "generation": 1 }))
            .await
            .unwrap();
        let state = fixture
            .node
            .gate_test_live_run_state(&job_id)
            .await
            .expect("gen-2 run still live");
        assert_eq!(
            state,
            (2, false),
            "a stale generation 1 cancel must not stand down the gen-2 run"
        );
        // The stale tombstone is recorded (a genuinely late gen-1 frame still
        // refuses), without killing the gen-2 group.
        assert_eq!(
            fixture.node.gate_test_canceled_generation(&job_id).await,
            Some(1)
        );
        // The CURRENT generation's cancel works as usual.
        fixture
            .node
            .cancel_gate(&serde_json::json!({ "jobId": job_id, "generation": 2 }))
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(10), running)
            .await
            .expect("the matching cancel reaps the run")
            .unwrap()
            .unwrap();
        assert_eq!(result.status, "canceled");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stale_generation_abort_never_signals_the_newer_live_attempt() {
        // r15 item 2: a delayed gate.abortLanding naming the settled attempt 1
        // must not stand down or kill the group of a newer attempt 2 that
        // reclaimed the job slot.
        let fixture = fixture("sleep 30\necho '{\"exitCode\":0,\"status\":\"verified\"}'");
        let node = fixture.node.clone();
        let mut p = params(&fixture, "verify");
        p.generation = 3;
        let job_id = p.job_id.clone();
        let running = tokio::spawn(async move { node.run_gate_typed(p).await });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if fixture
                .node
                .gate_test_live_run_state(&job_id)
                .await
                .is_some_and(|(generation, canceled)| generation == 3 && !canceled)
            {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("gen-3 run never registered");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Delayed abort for attempt 2 (older than the live attempt 3).
        fixture
            .node
            .abort_gate_landing(&serde_json::json!({
                "jobId": job_id,
                "generation": 2,
            }))
            .await
            .unwrap();
        assert_eq!(
            fixture
                .node
                .gate_test_live_run_state(&job_id)
                .await
                .expect("gen-3 run still live"),
            (3, false),
            "a stale abort must not stand down or signal the newer attempt"
        );
        // Matching generation aborts DO stand the run down (its verify phase,
        // since this fake never reaches a granted land).
        fixture
            .node
            .abort_gate_landing(&serde_json::json!({
                "jobId": job_id,
                "generation": 3,
            }))
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(10), running)
            .await
            .expect("the matching abort reaps the run")
            .unwrap()
            .unwrap();
        assert_eq!(
            result.status, "canceled",
            "the same-generation abort must terminate the live run: {:?}",
            result.status
        );
    }

    #[test]
    fn port_pair_and_path_helpers() {
        let (listen, web) = super::port_pair(Some("58480-58489")).unwrap();
        assert_eq!(listen, "127.0.0.1:58480");
        assert_eq!(web, "58489");
        assert!(super::port_pair(Some("garbage")).is_none());
    }

    /// A bare `GateRunParams` with only the identity fields set; env-shaping
    /// tests fill the pieces they exercise.
    fn bare_run_params() -> GateRunParams {
        GateRunParams {
            initiator: None,
            generation: 0,
            job_id: "gjb_env".into(),
            lane_id: "lane1".into(),
            repo_path: "/tmp/lane".into(),
            target_dir: "/tmp/target-lane".into(),
            branch: "wt/fake/task".into(),
            base_branch: "main".into(),
            mode: "verify".into(),
            web: "auto".into(),
            env: BTreeMap::new(),
            lock_path: None,
            pw_endpoint: None,
            toolchain_path: None,
            ports: None,
            timeouts: BTreeMap::new(),
            gate_timeout_secs: 0,
            push: false,
            keep_logs: false,
            binary: None,
            push_from: remuda_protocol::GatePushFrom::default(),
        }
    }

    #[test]
    fn gate_child_env_drops_ambient_pty_flags_and_composes_path() {
        // A representative host snapshot standing in for the Node's process
        // env: the carrier flags the lane Node runs under (which must NOT
        // leak) plus a legitimate passthrough var. host_env_snapshot() itself
        // only ever selects the whitelist, so the carrier flags never appear
        // in it — but assert on an explicit map so the test does not depend on
        // this process's ambient environment.
        let host: BTreeMap<String, String> = [
            ("HOME", "/home/lane"),
            ("USER", "lane"),
            ("PATH", "/opt/leaky/bin:/usr/bin"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();

        let mut request = bare_run_params();
        request.toolchain_path = Some("/opt/toolchain/bin".into());
        request.env.insert("CARGO_BUILD_JOBS".into(), "8".into());

        let child = super::gate_run_child_env(&request, &host);

        // No REMUDA_PTY_* variable is present unless the lane env set it.
        for key in child.keys() {
            assert!(
                !key.starts_with("REMUDA_PTY_"),
                "carrier flag leaked into gate child: {key}"
            );
        }
        // The derived build keys are set.
        assert_eq!(
            child.get("CARGO_TARGET_DIR").map(String::as_str),
            Some("/tmp/target-lane")
        );
        assert_eq!(
            child.get("CARGO_INCREMENTAL").map(String::as_str),
            Some("0")
        );
        assert_eq!(child.get("VITE_NO_WATCH").map(String::as_str), Some("1"));
        assert_eq!(child.get("CARGO_BUILD_JOBS").map(String::as_str), Some("8"));
        // TERM is forced to dumb; the passthrough whitelist crosses.
        assert_eq!(child.get("TERM").map(String::as_str), Some("dumb"));
        assert_eq!(child.get("HOME").map(String::as_str), Some("/home/lane"));
        assert_eq!(child.get("USER").map(String::as_str), Some("lane"));
        // PATH leads with the toolchain prefix and derives its base from the
        // explicit env, not a captured ambient PATH.
        let path = child.get("PATH").expect("PATH set");
        assert!(
            path.starts_with("/opt/toolchain/bin:"),
            "toolchain prefix must lead PATH: {path}"
        );
        assert!(
            path.contains("/opt/leaky/bin:/usr/bin"),
            "the lane's own PATH is the composed base: {path}"
        );
    }

    #[test]
    fn gate_child_path_falls_back_to_whitelisted_base() {
        // With no lane PATH and no toolchain prefix, PATH is exactly the
        // whitelisted base — reproducible from config, never the Node's.
        let host = BTreeMap::new();
        let request = bare_run_params();
        let child = super::gate_run_child_env(&request, &host);
        assert_eq!(
            child.get("PATH").map(String::as_str),
            Some(super::GATE_BASE_PATH)
        );
    }

    #[test]
    fn gate_step_timeouts_reach_the_child_env_as_the_serialized_map() {
        // A non-empty per-step timeouts map is forwarded to the gate runner as
        // the serialized `REMUDA_GATE_STEP_TIMEOUTS` env var; an empty map sets
        // nothing.
        let host = BTreeMap::new();
        let mut request = bare_run_params();
        request.timeouts.insert("cargo-test".into(), 2400);
        request.timeouts.insert("web-hub-e2e".into(), 0);
        let child = super::gate_run_child_env(&request, &host);
        assert_eq!(
            child.get("REMUDA_GATE_STEP_TIMEOUTS").map(String::as_str),
            Some(r#"{"cargo-test":2400,"web-hub-e2e":0}"#)
        );

        let bare = super::gate_run_child_env(&bare_run_params(), &BTreeMap::new());
        assert!(!bare.contains_key("REMUDA_GATE_STEP_TIMEOUTS"));
    }

    #[test]
    fn lane_env_is_the_only_channel_for_a_pty_flag() {
        // A lane that deliberately sets a carrier flag in its config is honored
        // (the escape hatch); nothing else introduces one.
        let host = BTreeMap::new();
        let mut request = bare_run_params();
        request.env.insert("REMUDA_PTY_EMULATOR".into(), "1".into());
        let child = super::gate_run_child_env(&request, &host);
        assert_eq!(
            child.get("REMUDA_PTY_EMULATOR").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn host_env_snapshot_selects_only_the_whitelist() {
        // Even with a carrier flag genuinely exported into this test process,
        // the snapshot never carries it — env_clear plus this whitelist is
        // what keeps the gate child clean (gate-lane-3). The test only reads
        // the ambient env; it does not mutate it (unsafe set_var is forbidden
        // workspace-wide), so it asserts the shape rather than a specific
        // sentinel.
        let snapshot = super::host_env_snapshot();
        for key in snapshot.keys() {
            assert!(
                !key.starts_with("REMUDA_PTY_"),
                "whitelist must never include a carrier flag: {key}"
            );
            assert!(
                ["HOME", "USER", "SHELL", "TMPDIR", "LANG", "SSH_AUTH_SOCK"]
                    .contains(&key.as_str()),
                "unexpected key in host snapshot: {key}"
            );
        }
    }

    #[test]
    fn gate_then_child_env_is_lane_env_plus_base_path() {
        let host: BTreeMap<String, String> = [("HOME", "/home/lane")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let mut lane_env = BTreeMap::new();
        lane_env.insert("DEPLOY_TAG".to_owned(), "v1".to_owned());
        let child = super::gate_then_child_env(&lane_env, &host);
        assert_eq!(child.get("DEPLOY_TAG").map(String::as_str), Some("v1"));
        assert_eq!(child.get("TERM").map(String::as_str), Some("dumb"));
        assert_eq!(child.get("HOME").map(String::as_str), Some("/home/lane"));
        // No toolchain prefix, cargo target or port block of its own.
        assert_eq!(
            child.get("PATH").map(String::as_str),
            Some(super::GATE_BASE_PATH)
        );
        assert!(!child.contains_key("CARGO_TARGET_DIR"));
        for key in child.keys() {
            assert!(!key.starts_with("REMUDA_PTY_"), "leak: {key}");
        }
    }

    /// Inner half of [`ambient_pty_flag_never_reaches_the_gate_child`]: run in a
    /// re-exec'd process that has `REMUDA_PTY_EMULATOR=1` genuinely exported.
    /// It builds the gate child env through the real [`host_env_snapshot`] —
    /// the code path a live lane runner takes — and proves the ambient flag
    /// does not survive `env_clear` + whitelist. `#[ignore]` keeps it out of a
    /// normal run; the outer test drives it with the sentinel set.
    #[test]
    #[ignore = "re-exec'd by ambient_pty_flag_never_reaches_the_gate_child with the sentinel set"]
    fn gate_child_env_from_real_process_env() {
        assert_eq!(
            std::env::var("REMUDA_PTY_EMULATOR").ok().as_deref(),
            Some("1"),
            "guard: the outer test must export the sentinel before re-exec"
        );
        let mut request = bare_run_params();
        request.toolchain_path = Some("/opt/toolchain/bin".into());
        let child = super::gate_run_child_env(&request, &super::host_env_snapshot());
        assert!(
            !child.contains_key("REMUDA_PTY_EMULATOR"),
            "ambient REMUDA_PTY_EMULATOR leaked into the gate child: {child:#?}"
        );
        for key in child.keys() {
            assert!(
                !key.starts_with("REMUDA_PTY_"),
                "carrier flag leaked: {key}"
            );
        }
        let path = child.get("PATH").expect("PATH set");
        assert!(
            path.starts_with("/opt/toolchain/bin:"),
            "toolchain prefix must lead PATH: {path}"
        );
    }

    #[test]
    fn ambient_pty_flag_never_reaches_the_gate_child() {
        // set_var is forbidden workspace-wide, so export the sentinel by
        // re-exec'ing this binary with REMUDA_PTY_EMULATOR=1 and running the
        // ignored inner test against the real process environment.
        let exe = std::env::current_exe().expect("test binary");
        let output = std::process::Command::new(exe)
            .args([
                "--exact",
                "gate::tests::gate_child_env_from_real_process_env",
                "--ignored",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env("REMUDA_PTY_EMULATOR", "1")
            .env("REMUDA_PTY_CARRIER", "native")
            .env("REMUDA_PTY_HOOKS", "1")
            .output()
            .expect("re-exec the gate env test");
        assert!(
            output.status.success(),
            "gate child inherited an ambient carrier flag\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        // Guard against the inner test silently not running.
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("1 passed"),
            "the inner env test did not run: {stdout}"
        );
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

    // ── r15 R5 land fence / termination tests ──────────────────────────────
    use std::os::unix::process::CommandExt as StdUnixCommandExt;

    /// Serializes tests that read (or override) the `/proc` root: the override
    /// is process-global, so identity/enumeration tests take this one lock.
    static PROC_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn proc_lock() -> std::sync::MutexGuard<'static, ()> {
        PROC_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// While held, process-info reads see a root that does not exist — the
    /// macOS/no-`/proc` condition every consumer must treat as Unknown. The
    /// write lock blocks proc reads on every other thread; the thread-local
    /// keeps this thread's own reads reentrant (no self-deadlock).
    struct MissingProc {
        _write: std::sync::RwLockWriteGuard<'static, Option<std::path::PathBuf>>,
    }
    impl MissingProc {
        fn set() -> Self {
            let mut write = PROC_ROOT_RW
                .write()
                .unwrap_or_else(|poison| poison.into_inner());
            let ghost = std::env::temp_dir().join(format!(
                "r15-missing-proc-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            assert!(!ghost.exists());
            *write = Some(ghost.clone());
            super::PROC_OVERRIDE_LOCAL.with(|cell| cell.set(Some(ghost)));
            Self { _write: write }
        }
    }
    impl Drop for MissingProc {
        fn drop(&mut self) {
            super::PROC_OVERRIDE_LOCAL.with(|cell| cell.set(None));
            *self._write = None;
        }
    }

    /// Spawn `program` in its OWN new process group (pgid == child pid); the
    /// caller reaps the leader. Safe-only (std's process_group, no pre_exec).
    fn spawn_grouped(program: &str, args: &[&str]) -> std::process::Child {
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        command.spawn().expect("spawn grouped child")
    }

    /// Bounded: reap the leader so killpg can reach ESRCH.
    fn reap(mut child: std::process::Child) {
        let _ = child.wait();
    }

    /// Spawn a group whose leader ignores SIGTERM and SURVIVES the whole TERM
    /// grace (a tight loop; each `sleep` child is disposable). The leader
    /// writes `ready` to `marker` only AFTER installing the trap, so the test
    /// never sends TERM during the race window before exec/handler setup (in
    /// which a default-disposition process would die instantly).
    fn spawn_term_ignoring_group(marker: &std::path::Path) -> std::process::Child {
        let child = spawn_grouped(
            "sh",
            &[
                "-c",
                &format!(
                    "trap '' TERM; echo ready > '{}'; while true; do sleep 1; done",
                    marker.display()
                ),
            ],
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(marker.exists(), "TERM-ignoring leader never became ready");
        child
    }

    #[test]
    fn terminate_always_kills_a_term_ignoring_group_and_proves_esrch() {
        let _seq = proc_lock();
        // sh ignores SIGTERM (trap '') and holds the group open through the
        // whole TERM grace; only the unconditional KILL can clear it, and the
        // death claim is the kernel ESRCH (never an enumeration).
        let tmp = tempfile::TempDir::new().unwrap();
        let mut child = spawn_term_ignoring_group(&tmp.path().join("ready1"));
        let pgid = child.id() as i32;
        assert!(super::land_group_exists(pgid), "group never came up");
        // Fence in a thread while the test reaps the killed leader (a zombie
        // leader keeps killpg returning success until reaped).
        let verdict = std::thread::scope(|scope| {
            let handle = scope
                .spawn(move || super::terminate_and_verify_group(pgid, Duration::from_millis(150)));
            // Bounded: wait for the child to die, then reap it.
            let kill_deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if std::time::Instant::now() >= kill_deadline => {
                        panic!("TERM-ignoring group was not KILLed")
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                    Err(_) => break,
                }
            }
            reap(child);
            handle.join().unwrap()
        });
        assert!(verdict, "group must be provably ESRCH after the KILL");
        assert!(!super::land_group_exists(pgid), "group survived");
    }

    #[test]
    fn termination_kills_even_when_process_enumeration_is_unavailable() {
        // The macOS condition: /proc is absent, so the diagnostic enumeration
        // answers empty. A SIGTERM-ignoring member is nevertheless alive and
        // would keep pushing. Termination must wait the bounded grace, ALWAYS
        // KILL the whole group, and claim death only on ESRCH — an empty
        // enumeration must never skip the KILL or report "cleared".
        let _missing = MissingProc::set();
        // Sanity: the seam really does hide process info.
        assert!(super::alive_pids_in_group(1).is_empty());
        let tmp = tempfile::TempDir::new().unwrap();
        let mut child = spawn_term_ignoring_group(&tmp.path().join("ready2"));
        let pgid = child.id() as i32;
        assert!(super::land_group_exists(pgid));
        let verdict = std::thread::scope(|scope| {
            let started = std::time::Instant::now();
            let handle = scope
                .spawn(move || super::terminate_and_verify_group(pgid, Duration::from_millis(300)));
            let kill_deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if std::time::Instant::now() >= kill_deadline => {
                        panic!("TERM-ignoring group was not KILLed without /proc")
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                    Err(_) => break,
                }
            }
            reap(child);
            let verdict = handle.join().unwrap();
            // The leader must die at the KILL (after the full TERM grace), not
            // be reported "cleared" by an empty enumeration ten ms in.
            assert!(
                started.elapsed() >= Duration::from_millis(280),
                "the KILL must wait for the whole TERM grace, not skip it"
            );
            verdict
        });
        assert!(
            verdict,
            "death must be proven by ESRCH, not by an empty scan"
        );
        assert!(!super::land_group_exists(pgid));
    }

    #[test]
    fn startup_fence_with_unreadable_identity_holds_and_does_not_signal() {
        // /proc is absent (the read fails): the leader pid is live but its
        // identity is Unknown. The startup fence must NOT signal the group and
        // must KEEP the record — previously a missing identity fell through to
        // a killpg on the pgid, which could kill an unrelated reused group.
        let _missing = MissingProc::set();
        let mut stranger = spawn_grouped("sleep", &["30"]);
        let pgid = stranger.id() as i32;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !super::land_group_exists(pgid) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(super::land_group_exists(pgid));
        assert!(super::sample_leader_identity(pgid).is_none());
        let dir = tempfile::TempDir::new().unwrap();
        let path = super::land_fence_path(dir.path(), "ghost");
        std::fs::write(
            &path,
            serde_json::json!({"jobId":"ghost","generation":1,"pgid":pgid,"startTick":42})
                .to_string(),
        )
        .unwrap();
        super::fence_orphaned_land_groups(dir.path());
        assert!(
            stranger.try_wait().unwrap().is_none(),
            "a group with unknown identity must never be signalled"
        );
        assert!(path.exists(), "the record must be kept on unknown identity");
        let _ = stranger.kill();
        reap(stranger);
    }

    #[test]
    fn startup_fence_keeps_unreadable_or_identityless_records() {
        let dir = tempfile::TempDir::new().unwrap();
        let fence = dir.path();
        std::fs::write(fence.join("land-bad.json"), b"{not json").unwrap();
        std::fs::write(
            fence.join("land-noid.json"),
            serde_json::json!({"jobId":"j","generation":1,"pgid":12345}).to_string(),
        )
        .unwrap();
        std::fs::write(
            fence.join("land-nopgid.json"),
            serde_json::json!({"jobId":"j","generation":1,"startTick":7}).to_string(),
        )
        .unwrap();
        super::fence_orphaned_land_groups(fence);
        // Unknown evidence: every record is KEPT, none deleted.
        for name in ["land-bad.json", "land-noid.json", "land-nopgid.json"] {
            assert!(
                fence.join(name).is_file(),
                "{name} must be kept on unknown evidence"
            );
        }
    }

    #[test]
    fn startup_fence_never_signals_a_reused_leader_and_keeps_the_record() {
        let _seq = proc_lock();
        // A live process at the leader pid whose start identity DIFFERS from
        // the record: pid was reused by a stranger. No signal, record kept.
        let mut stranger = spawn_grouped("sleep", &["30"]);
        let pgid = stranger.id() as i32;
        let dir = tempfile::TempDir::new().unwrap();
        let fence = dir.path();
        let recorded = match super::sample_leader_identity(pgid) {
            Some(super::LeaderIdentity::StartTick(tick)) => tick.saturating_add(1_000_000),
            other => panic!("expected a start tick, got {other:?}"),
        };
        std::fs::write(
            super::land_fence_path(fence, "job-x"),
            serde_json::json!({
                "jobId": "job-x",
                "generation": 1,
                "pgid": pgid,
                "startTick": recorded,
            })
            .to_string(),
        )
        .unwrap();
        super::fence_orphaned_land_groups(fence);
        // The stranger is still alive...
        assert!(
            stranger.try_wait().unwrap().is_none(),
            "the reused-pid stranger must not have been signalled"
        );
        // ...and the record was kept (unknown ownership, Landing held).
        assert!(super::land_fence_path(fence, "job-x").is_file());
        let _ = stranger.kill();
        reap(stranger);
    }

    #[test]
    fn startup_fence_removes_the_record_only_when_death_is_esrch() {
        let _seq = proc_lock();
        // Established dead: leader killed+reaped and the group gone.
        let mut child = spawn_grouped("sleep", &["1"]);
        let pgid = child.id() as i32;
        let identity = super::sample_leader_identity(pgid).expect("identity");
        let dir = tempfile::TempDir::new().unwrap();
        let path = super::land_fence_path(dir.path(), "dead");
        let mut record = serde_json::json!({"jobId":"dead","generation":1,"pgid":pgid});
        identity.write(&mut record);
        std::fs::write(&path, record.to_string()).unwrap();
        let _ = child.kill();
        reap(child);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while super::land_group_exists(pgid) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!super::land_group_exists(pgid));
        super::fence_orphaned_land_groups(dir.path());
        assert!(!path.exists(), "ESRCH must release the record");
        // r16 item 2: the fence removal leaves a DURABLE DEATH CERTIFICATE for
        // the exact job, so a later ledger read attests VerifiedDead instead of
        // guessing from the missing ledger (which is Unknown on its own).
        assert!(
            super::read_land_group_dead_cert(dir.path(), "dead").is_some(),
            "authoritative ESRCH at startup must leave a death certificate"
        );
        assert_eq!(
            super::land_ledger_liveness(dir.path(), "dead"),
            super::LandPushLiveness::VerifiedDead
        );
        assert_eq!(
            super::land_ledger_liveness(dir.path(), "other-job"),
            super::LandPushLiveness::Unknown,
            "the certificate is scoped to the exact job"
        );
    }

    #[test]
    fn startup_fence_holds_when_the_leader_is_gone_but_members_survive() {
        let _seq = proc_lock();
        // Leader exits (kills itself) while a descendant keeps the pgid: with
        // no leader identity to match, ownership is Unknown — no signal, the
        // orphan survives, the record stays.
        let child = spawn_grouped(
            "sh",
            &["-c", "sleep 30 & kill -9 $$; wait 2>/dev/null || true"],
        );
        let pgid = child.id() as i32;
        // Capture the leader identity before it exits.
        let identity = {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                if let Some(value) = super::sample_leader_identity(pgid) {
                    break value;
                }
                if std::time::Instant::now() >= deadline {
                    panic!("never sampled the leader");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        reap(child);
        // The orphan keeps the group present.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut present = false;
        while std::time::Instant::now() < deadline {
            if super::land_group_exists(pgid) {
                present = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(present, "the orphaned member should hold the pgid");

        let dir = tempfile::TempDir::new().unwrap();
        let path = super::land_fence_path(dir.path(), "orphan");
        let mut record = serde_json::json!({"jobId":"orphan","generation":1,"pgid":pgid});
        identity.write(&mut record);
        std::fs::write(&path, record.to_string()).unwrap();
        super::fence_orphaned_land_groups(dir.path());
        assert!(path.exists(), "no leader identity => record kept");
        assert!(
            super::land_group_exists(pgid),
            "the orphan must not have been signalled"
        );
        // Cleanup: the established-same-leader path isn't available, so the
        // test itself clears the group now (it owns it).
        assert!(super::terminate_and_verify_group(
            pgid,
            Duration::from_millis(200)
        ));
    }

    /// r17 item 1: a leftover death certificate for a DEAD attempt-1 group must
    /// never be treated as an actionable fence. Here a LIVE replacement group
    /// now holds the same job slug (a different pgid, and a fence identity that
    /// does not match the live process so it is correctly NOT signalled). The
    /// old scan read every `*.json` (including the cert): the dead cert's ESRCH
    /// made it certify-from and DELETE the live replacement fence.
    #[test]
    fn a_stale_certificate_for_a_dead_group_never_certifies_a_live_replacement() {
        let _seq = proc_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        let job = "gjb_replacement";

        // A guaranteed-gone pgid for the stale attempt-1 certificate.
        let dead = spawn_grouped("sh", &["-c", "exit 0"]);
        let dead_pgid = dead.id() as i32;
        reap(dead);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while super::land_group_exists(dead_pgid) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!super::land_group_exists(dead_pgid));

        // A LIVE replacement group holding the same job slug.
        let mut live = spawn_grouped("sleep", &["300"]);
        let live_pgid = live.id() as i32;
        // Its fence records a FOREIGN identity (a fence the live process does
        // not own), so a correct startup fence refuses to signal it.
        super::write_land_fence(dir, job, 2, live_pgid, &super::LeaderIdentity::StartTick(1))
            .unwrap();
        // The leftover attempt-1 certificate for the DEAD pgid.
        let stale = {
            let mut v = serde_json::json!({
                "jobId": job,
                "generation": 1,
                "pgid": dead_pgid,
            });
            super::LeaderIdentity::StartTick(2).write(&mut v);
            v
        };
        super::write_land_group_dead_cert(dir, &stale, dead_pgid).unwrap();

        super::fence_orphaned_land_groups(dir);

        // The live replacement group survived and its fence was retained.
        assert!(
            super::land_group_exists(live_pgid),
            "the live replacement group must not be signalled"
        );
        assert!(
            super::land_fence_path(dir, job).exists(),
            "the live replacement fence must not be unlinked"
        );
        // No certificate was derived for the replacement snapshot.
        let cert = super::read_land_group_dead_cert(dir, job).expect("cert still present");
        assert_eq!(cert["pgid"], json!(dead_pgid), "{cert}");
        assert_eq!(cert["generation"], json!(1), "{cert}");

        let _ = live.kill();
        reap(live);
    }

    /// r17 item 1: `certify_dead_and_remove_fence` refuses when the fence on
    /// disk is no longer the observed snapshot — it never certifies a different
    /// (replacement) attempt from a stale observation.
    #[test]
    fn certify_refuses_when_the_on_disk_fence_is_a_different_snapshot() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        let job = "gjb_snapshot";
        // On disk: attempt 2 at pgid 4242.
        super::write_land_fence(dir, job, 2, 4242, &super::LeaderIdentity::StartTick(22)).unwrap();
        // Observed (stale): attempt 1 at a different pgid/identity.
        let observed = {
            let mut v = serde_json::json!({"jobId": job, "generation": 1, "pgid": 4141});
            super::LeaderIdentity::StartTick(11).write(&mut v);
            v
        };
        let certified = super::certify_dead_and_remove_fence(dir, &observed, 4141).unwrap();
        assert!(
            !certified,
            "a mismatched on-disk snapshot must not be certified"
        );
        assert!(
            super::land_fence_path(dir, job).exists(),
            "the current fence stays"
        );
        assert!(
            super::read_land_group_dead_cert(dir, job).is_none(),
            "no certificate may be written for the wrong snapshot"
        );
    }

    #[test]
    fn record_land_group_fails_when_the_fence_dir_cannot_be_written() {
        let _seq = proc_lock();
        // A FILE where the fence directory must live: create_dir_all fails, so
        // the grant refuses to persist — the caller must then fail the land
        // rather than release an unfenced child.
        let tmp = tempfile::TempDir::new().unwrap();
        let blocked = tmp.path().join("gate-land");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let registry = GateRegistry::new(blocked);
        let error = registry
            .record_land_group("job-fail", 1, std::process::id() as i32)
            .expect_err("fence persistence must fail");
        assert!(
            error.to_string().contains("entity") || error.raw_os_error().is_some(),
            "expected an io error, got {error}"
        );
    }

    #[test]
    fn start_barrier_delivers_the_release_byte_only_after_the_fence() {
        // A real child receives the read end as its STDIN and blocks on
        // `head -c 1`; it exits 0 once release_after writes the byte.
        let mut barrier = super::LandStartBarrier::new().expect("barrier");
        let mut child = std::process::Command::new("sh")
            .args(["-c", "head -c 1"])
            .stdin(barrier.take_child_stdin())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn barrier waiter");
        // Give the child a moment to park in read(2).
        std::thread::sleep(Duration::from_millis(50));
        if let Ok(Some(status)) = child.try_wait() {
            panic!("child must block before the release byte: {status}");
        }
        barrier
            .release_after(|| Ok(()))
            .expect("fence write and release");
        let status = child.wait().expect("reap barrier waiter");
        assert!(status.success(), "released child exits 0");
    }

    #[test]
    fn start_barrier_failed_variant_refuses_release() {
        let barrier = super::LandStartBarrier::failed(std::io::Error::other("boom"));
        let error = barrier.release_after(|| Ok(())).expect_err("no release");
        assert!(error.to_string().contains("start barrier unavailable"));
    }

    /// r16 item 2: ledger-only liveness. A missing ledger with no death
    /// certificate is UNKNOWN, not "dead". A present fence (even malformed)
    /// says a group may push; a valid certificate for the exact job is the only
    /// durable death evidence; malformed/foreign certificates stay Unknown.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn land_push_liveness_a_missing_ledger_is_unknown_without_a_death_cert() {
        use super::LandPushLiveness::*;
        let fixture = fixture(PASSING_SCRIPT);
        let registry = fixture.node.gate_registry();
        let dir = registry.land_fence_dir.clone();
        std::fs::create_dir_all(&dir).unwrap();
        let job = "gjb_liveness";
        let fence = super::land_fence_path(&dir, job);
        let cert = super::land_dead_cert_path(&dir, job);
        let _ = std::fs::remove_file(&fence);
        let _ = std::fs::remove_file(&cert);

        // Nothing on disk: Unknown — never a death attestation.
        assert_eq!(registry.land_push_liveness(job).await, Unknown);

        // A malformed fence ledger is still a PRESENT ledger: may push.
        std::fs::write(&fence, b"not json").unwrap();
        assert_eq!(registry.land_push_liveness(job).await, MayPush);

        // Ledger gone, malformed cert: Unknown.
        std::fs::remove_file(&fence).unwrap();
        std::fs::write(&cert, b"not json").unwrap();
        assert_eq!(registry.land_push_liveness(job).await, Unknown);

        // Cert present but missing its leader identity: Unknown.
        std::fs::write(
            &cert,
            br#"{"jobId":"gjb_liveness","generation":1,"pgid":12345}"#,
        )
        .unwrap();
        assert_eq!(registry.land_push_liveness(job).await, Unknown);

        // Cert naming a different job never proves THIS job dead.
        let foreign =
            serde_json::json!({"jobId":"gjb_other","generation":1,"pgid":1,"startTick":7});
        super::write_land_group_dead_cert(&dir, &foreign, 1).unwrap();
        assert_eq!(registry.land_push_liveness(job).await, Unknown);

        // A complete certificate for the exact job: VerifiedDead.
        let own = serde_json::json!({"jobId":job,"generation":1,"pgid":12345,"startTick":42});
        super::write_land_group_dead_cert(&dir, &own, 12345).unwrap();
        assert_eq!(registry.land_push_liveness(job).await, VerifiedDead);
    }

    /// r16 item 2 end-to-end through the read-only `gate.resolveLanding` RPC:
    /// with the granted merge NOT on the fetched remote, a missing ledger keeps
    /// the verdict `unknown`; only a valid death certificate for the exact job
    /// lets it settle `absent`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_landing_absent_requires_a_death_certificate() {
        let fixture = fixture(PASSING_SCRIPT);
        // A real dangling root commit in the lane: a valid object that is NOT
        // an ancestor of origin/main (merge-base --is-ancestor exits 1).
        let tree = git(&fixture.lane, &["rev-parse", "main^{tree}"]);
        let orphan = git(&fixture.lane, &["commit-tree", &tree, "-m", "orphan"]);
        assert_ne!(orphan, fixture_lane_main(&fixture));

        let params = serde_json::json!({
            "jobId": "gjb_deadprobe",
            "repoPath": fixture.lane.to_string_lossy(),
            "baseBranch": "main",
            "mergeSha": orphan,
        });
        // No live run, no fence ledger, no certificate → Unknown.
        let reply = fixture
            .node
            .run_gate_resolve_landing(&params)
            .await
            .unwrap();
        assert_eq!(reply["status"], json!("unknown"), "{reply}");
        assert!(
            reply["error"]
                .as_str()
                .unwrap_or("")
                .contains("not authoritatively established"),
            "{reply}"
        );

        // A valid death certificate for the exact job attests VerifiedDead, so
        // the non-ancestry result settles `absent` with the remote base sha.
        let dir = fixture.node.gate_registry().land_fence_dir.clone();
        let record = serde_json::json!({
            "jobId": "gjb_deadprobe", "generation": 1, "pgid": 4242, "startTick": 99
        });
        super::write_land_group_dead_cert(&dir, &record, 4242).unwrap();
        let reply = fixture
            .node
            .run_gate_resolve_landing(&params)
            .await
            .unwrap();
        assert_eq!(reply["status"], json!("absent"), "{reply}");
        assert!(reply["baseSha"].is_string(), "{reply}");
    }

    /// r16 item 4: classify the RELEASED land child's exit as
    /// Established-or-Unknown.
    #[cfg(unix)]
    #[test]
    fn released_land_outcome_recognizes_failure_but_not_crash_or_garbage() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::{ExitStatus, Output};
        fn output_from(raw: i32, stdout: &str, stderr: &str) -> Output {
            Output {
                status: ExitStatus::from_raw(raw),
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
            }
        }
        let gate_failed = r#"{"exitCode":1,"status":"gate_failed","error":"hook refused"}"#;

        // exit 0 → landed, even with an empty body.
        assert!(matches!(
            super::classify_released_land_outcome(&output_from(0, "{}", ""), ""),
            super::ReleasedLandOutcome::Landed(_)
        ));
        // The one recognized structured failure (exit 1 + gate_failed).
        assert!(matches!(
            super::classify_released_land_outcome(&output_from(1 << 8, gate_failed, ""), ""),
            super::ReleasedLandOutcome::Failed(message) if message.contains("hook refused")
        ));
        // base_moved keeps its recognized verdict.
        assert!(matches!(
            super::classify_released_land_outcome(
                &output_from(2 << 8, r#"{"status":"base_moved","currentMain":"abc"}"#, ""),
                ""
            ),
            super::ReleasedLandOutcome::BaseMoved(_)
        ));
        // The CLI's own uncertainty stays uncertain.
        assert!(matches!(
            super::classify_released_land_outcome(
                &output_from(4 << 8, r#"{"status":"uncertain"}"#, ""),
                ""
            ),
            super::ReleasedLandOutcome::Uncertain { .. }
        ));
        // Missing JSON, malformed JSON, an empty object and an unrecognized
        // status are all Unknown — never Failed.
        for stdout in [
            "",
            "not json",
            "{}",
            r#"{"status":"conflict"}"#,
            r#"{"status":"wat"}"#,
        ] {
            assert!(
                matches!(
                    super::classify_released_land_outcome(
                        &output_from(1 << 8, stdout, "some stderr"),
                        ""
                    ),
                    super::ReleasedLandOutcome::Uncertain { .. }
                ),
                "stdout {stdout:?} must be Unknown"
            );
        }
        // gate_failed on a NON-1 exit is not the recognized failure.
        assert!(matches!(
            super::classify_released_land_outcome(&output_from(2 << 8, gate_failed, ""), ""),
            super::ReleasedLandOutcome::Uncertain { .. }
        ));
        // Killed by SIGKILL/SIGSEGV after release with no exit code: crash-only
        // is Unknown even if a well-formed gate_failed happened to be printed.
        for signal in [9, 11] {
            assert!(
                matches!(
                    super::classify_released_land_outcome(
                        &output_from(signal, gate_failed, ""),
                        ""
                    ),
                    super::ReleasedLandOutcome::Uncertain { .. }
                ),
                "signal {signal} must be Unknown"
            );
        }
    }

    /// r16 item 4 end-to-end through the production builder: a granted land
    /// child that is RELEASED and then dies on a signal without a JSON verdict
    /// reports UNCERTAIN — it may have pushed — never Failed.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_released_land_child_killed_by_a_signal_is_uncertain_never_failed() {
        let script = r#"
base=$(git rev-parse main 2>/dev/null || echo "1111111111111111111111111111111111111111")
head=$(git rev-parse wt/fake/task 2>/dev/null || echo "$base")
if ! printf '%s\n' "$*" | grep -q -- '--land'; then
  echo "{\"exitCode\":0,\"status\":\"verified\",\"base\":\"$base\",\"head\":\"$head\",\"merged\":\"$head\",\"steps\":[]}"
  exit 0
fi
head -c 1 >/dev/null || exit 4
echo 'pushed then crashed?' >&2
kill -9 $$
"#;
        let fixture = fixture(script);
        let mut p = params(&fixture, "land");
        p.push = false;
        let result = fixture.node.run_gate_typed(p).await.unwrap();
        assert_eq!(
            result.status, "uncertain",
            "a signal death after the barrier release is Unknown: {result:?}"
        );
        assert!(
            result.error.as_deref().unwrap_or("").contains("signal"),
            "the error names the signal death: {:?}",
            result.error
        );
    }

    fn fixture_lane_main(fixture: &Fixture) -> String {
        git(&fixture.lane, &["rev-parse", "origin/main"])
    }

    /// r16 item 3: raw merge-base termination is tri-state. Exit 0 / exit 1
    /// are the only Established outcomes; death by signal (raw wait status 9 =
    /// SIGKILL) and any other code are Unknown, never a guessed boolean.
    #[cfg(unix)]
    #[test]
    fn ancestry_output_distinguishes_zero_one_and_signals() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::{ExitStatus, Output};
        fn output(raw: i32) -> Output {
            Output {
                status: ExitStatus::from_raw(raw),
                stdout: Vec::new(),
                stderr: b"boom".to_vec(),
            }
        }
        assert_eq!(
            super::classify_ancestry_output(&output(0)),
            super::AncestryVerdict::Ancestor
        );
        assert_eq!(
            super::classify_ancestry_output(&output(1 << 8)),
            super::AncestryVerdict::NotAncestor
        );
        assert!(matches!(
            super::classify_ancestry_output(&output(2 << 8)),
            super::AncestryVerdict::Unknown(_)
        ));
        // Killed by SIGKILL before exiting: no exit code at all → Unknown.
        assert!(matches!(
            super::classify_ancestry_output(&output(9)),
            super::AncestryVerdict::Unknown(_)
        ));
    }

    /// Helper for the shallow-history resolve tests: `git clone` through the
    /// smart file:// transport (plain local paths ignore --depth).
    fn shallow_clone(from: &std::path::Path, to: &std::path::Path, depth: &str) {
        let url = format!("file://{}", from.display());
        let output = std::process::Command::new("git")
            .args(["clone", "-q", "--depth", depth, &url, to.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "shallow clone: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// r16 item 3 through the production read-only resolver: a genuinely
    /// unrelated merge in a depth-1 clone of the FULL origin is established
    /// `absent` after the bounded safe deepen completes the history — even
    /// though the first raw merge-base also said exit 1.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_landing_deepens_shallow_history_before_attesting_absence() {
        let fixture = fixture(PASSING_SCRIPT);
        let origin = fixture._bare.path().join("origin.git");
        let lane = fixture._dir.path().join("shallow-full");
        shallow_clone(&origin, &lane, "1");
        git(&lane, &["config", "user.email", "t@example.com"]);
        git(&lane, &["config", "user.name", "T"]);
        assert_eq!(
            git(&lane, &["rev-parse", "--is-shallow-repository"]),
            "true"
        );
        // A local orphan commit: present, and genuinely not an ancestor.
        let tree = git(&lane, &["rev-parse", "origin/main^{tree}"]);
        let orphan = git(&lane, &["commit-tree", &tree, "-m", "orphan"]);

        let dir = fixture.node.gate_registry().land_fence_dir.clone();
        let record = serde_json::json!({
            "jobId": "gjb_shallow_full", "generation": 1, "pgid": 5252, "startTick": 7
        });
        super::write_land_group_dead_cert(&dir, &record, 5252).unwrap();
        let reply = fixture
            .node
            .run_gate_resolve_landing(&serde_json::json!({
                "jobId": "gjb_shallow_full",
                "repoPath": lane.to_string_lossy(),
                "baseBranch": "main",
                "mergeSha": orphan,
            }))
            .await
            .unwrap();
        assert_eq!(reply["status"], json!("absent"), "{reply}");
        assert_eq!(
            git(&lane, &["rev-parse", "--is-shallow-repository"]),
            "false",
            "the safe deepen unshallows the lane"
        );
    }

    /// r16 item 3: when the remote is ITSELF depth-1 shallow the bounded
    /// deepens add no history and the exit-1 "not ancestor" cannot be
    /// confirmed — the resolver stays UNKNOWN even with a death certificate
    /// (absence is never attested on incomplete history).
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_landing_with_an_uncompletable_shallow_history_stays_unknown() {
        let fixture = fixture(PASSING_SCRIPT);
        let root = fixture._dir.path();
        let origin = fixture._bare.path().join("origin.git");
        let server = root.join("shallow-server");
        let lane = root.join("shallow-client");
        shallow_clone(&origin, &server, "1");
        shallow_clone(&server, &lane, "1");
        git(&lane, &["config", "user.email", "t@example.com"]);
        git(&lane, &["config", "user.name", "T"]);
        assert_eq!(
            git(&lane, &["rev-parse", "--is-shallow-repository"]),
            "true"
        );
        let tree = git(&lane, &["rev-parse", "origin/main^{tree}"]);
        let orphan = git(&lane, &["commit-tree", &tree, "-m", "orphan"]);

        let dir = fixture.node.gate_registry().land_fence_dir.clone();
        let record = serde_json::json!({
            "jobId": "gjb_shallow_stuck", "generation": 1, "pgid": 6262, "startTick": 8
        });
        super::write_land_group_dead_cert(&dir, &record, 6262).unwrap();
        let reply = fixture
            .node
            .run_gate_resolve_landing(&serde_json::json!({
                "jobId": "gjb_shallow_stuck",
                "repoPath": lane.to_string_lossy(),
                "baseBranch": "main",
                "mergeSha": orphan,
            }))
            .await
            .unwrap();
        assert_eq!(reply["status"], json!("unknown"), "{reply}");
        assert!(
            reply["error"]
                .as_str()
                .unwrap_or("")
                .contains("added no history"),
            "the unknown names the uncompletable shallow deepen: {reply}"
        );
        assert_eq!(
            git(&lane, &["rev-parse", "--is-shallow-repository"]),
            "true"
        );
    }
}
