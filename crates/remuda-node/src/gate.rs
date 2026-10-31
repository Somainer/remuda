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
/// r20 items 4+5 / R8(b): fixed owner-cleanup allowance added to the soft land
/// cap to form the ABSOLUTE deadline — group TERM/KILL settle, the bounded
/// leader reap and the bounded pipe drains. Platform-independent constants so
/// the deadline exists on every target; the actual group signalling is unix.
const LAND_OWNER_CLEANUP_BUDGET: Duration = Duration::from_millis(750 + 500 + 2_000);
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
// r23 item 2: EVERY variable-length field the Node records is bounded at the
// source, not only the output tail. A `gate: <name>` marker whose name is too
// long is NOT a marker (so a subprocess emitting a 600 KiB "gate: aaa…" line
// cannot create a 600 KiB RunTrace key / GateRunLog.step); GateStep free-form
// fields read from gate.jsonl and the built evidence headline are clipped on a
// UTF-8 boundary. The Hub's evidence truncator (its own, smaller final-byte
// cap) is the second line of defense for supplied logs.
const MAX_STEP_NAME_BYTES: usize = 128;
const MAX_HEADLINE_BYTES: usize = MAX_LINE_CHARS;
const MAX_STEP_FIELD_BYTES: usize = MAX_LINE_CHARS;

/// Clip a free-form string for a bounded verdict/evidence field on a UTF-8
/// boundary, marking the cut. Input already within the cap is returned
/// verbatim.
fn clip_field(text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut cut = max_bytes.saturating_sub("…".len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut text = text;
    text.truncate(cut);
    text.push('…');
    text
}

/// r23 item 4: incremental RAW-BYTE line framer for streamed logs.
///
/// Each `read()` chunk is appended UNDECODED; complete lines are framed by
/// splitting only at the ASCII `\n` (which can never be a byte inside a
/// multi-byte UTF-8 sequence), and only the framed line is lossily decoded.
/// Thus a valid character whose bytes arrive across reads — e.g. read 1
/// returns `E2`, read 2 returns `82 AC 0A` (`€` + newline) — stays assembled
/// in the raw pending buffer and decodes to `€`, rather than being turned into
/// two replacement characters by per-chunk `String::from_utf8_lossy`. Bytes
/// that are genuinely invalid inside a completed line still decode to the
/// replacement character (lossy), preserving the r22 raw-byte evidence
/// behavior.
struct LossyLineFramer {
    pending: Vec<u8>,
}

impl LossyLineFramer {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// Append one read chunk and emit every now-complete line (newline
    /// stripped, a trailing CR trimmed).
    fn push(&mut self, chunk: &[u8], mut emit: impl FnMut(String)) {
        self.pending.extend_from_slice(chunk);
        while let Some(newline) = self.pending.iter().position(|&byte| byte == b'\n') {
            let rest = self.pending.split_off(newline + 1);
            let mut line = std::mem::replace(&mut self.pending, rest);
            while matches!(line.last(), Some(b'\n') | Some(b'\r')) {
                line.pop();
            }
            emit(String::from_utf8_lossy(&line).into_owned());
        }
    }

    /// Emit a final unterminated line at EOF, if any bytes remain.
    fn finish(self, mut emit: impl FnMut(String)) {
        let mut line = self.pending;
        while matches!(line.last(), Some(b'\n') | Some(b'\r')) {
            line.pop();
        }
        if !line.is_empty() {
            emit(String::from_utf8_lossy(&line).into_owned());
        }
    }
}

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
    /// rather than `absent` (r14 R1). Read-only for liveness; the land child's
    /// unique OWNER (the run task) is the only thing that signals it — abort is
    /// delivered as a MESSAGE over [`LiveRun::land_abort`] (r19 item 1 / R6),
    /// never by signalling this saved pgid from another task.
    land_pgid: std::sync::Mutex<Option<i32>>,
    /// Latch for an abortLanding request aimed at THIS run's land child. The
    /// land owner selects on a receiver of this; firing it makes the owner kill
    /// the group WHILE the leader is unreaped and reap it. An abort that lands
    /// before the land child spawns latches and is honoured once it does.
    land_abort: watch::Sender<bool>,
    /// Kept alive so `land_abort.send(true)` succeeds (and retains its value
    /// for a later owner subscriber) even before the land child owner
    /// subscribes — tokio watch drops a value sent with zero receivers.
    #[allow(dead_code)]
    land_abort_keep_rx: watch::Receiver<bool>,
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
        || name.len() > MAX_STEP_NAME_BYTES
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

/// Clip a field and report whether it lost bytes, so callers can fold the
/// clipping into their truncation marker (r24 item 6).
fn clip_field_track(text: String, max_bytes: usize) -> (String, bool) {
    let clipped = text.len() > max_bytes;
    (clip_field(text, max_bytes), clipped)
}

/// Build the bounded failure log for a failed step from a captured trace.
fn build_failure_log(
    trace: &RunTrace,
    step: &str,
    attempts: u32,
    step_error: Option<&str>,
    extra: Option<&str>,
) -> GateRunLog {
    let (tail, source_lines, mut truncated) = match trace.by_step.get(step) {
        Some(ring) => {
            let (tail, dropped) = ring.last(TAIL_LINES, TAIL_CAP_BYTES);
            let seen = ring.seen_lines;
            (tail, seen, dropped)
        }
        None => {
            let (tail, dropped) = trace.global.last(TAIL_LINES, TAIL_CAP_BYTES);
            (tail, trace.global.seen_lines, dropped)
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
    // r23 item 2: bound every variable-length field at the source — the step
    // name (a gate.jsonl/GateStep name may exceed the marker cap), the
    // headline (built from a free-form error) and every summary line.
    // r24 item 6: clipping any field IS truncation — OR it into the ring's
    // own drop marker in BOTH builders, or the Hub's under-cap fast path
    // stores a log that lost bytes while explicitly claiming it did not.
    let mut clip_any = |clipped: bool| truncated |= clipped;
    let summary = extract_summary(step, &tail)
        .into_iter()
        .map(|line| {
            let (line, clipped) = clip_field_track(line, MAX_STEP_FIELD_BYTES);
            clip_any(clipped);
            line
        })
        .collect();
    let tail = tail
        .into_iter()
        .map(|line| {
            let (line, clipped) = clip_field_track(line, MAX_STEP_FIELD_BYTES);
            clip_any(clipped);
            line
        })
        .collect();
    let (step_name, clipped) = clip_field_track(step.to_owned(), MAX_STEP_NAME_BYTES);
    clip_any(clipped);
    let (headline, clipped) = clip_field_track(headline, MAX_HEADLINE_BYTES);
    clip_any(clipped);
    GateRunLog {
        step: step_name,
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
    let (tail, mut truncated) = trace.global.last(TAIL_LINES, TAIL_CAP_BYTES);
    let tail = tail
        .into_iter()
        .map(|line| {
            let (line, clipped) = clip_field_track(line, MAX_STEP_FIELD_BYTES);
            truncated |= clipped;
            line
        })
        .collect();
    let (headline, clipped) = clip_field_track(headline.to_owned(), MAX_HEADLINE_BYTES);
    truncated |= clipped;
    GateRunLog {
        step: "*".into(),
        kind: "kept".into(),
        attempts: 0,
        headline,
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
    // r23 item 2: the verdict parsed from the consumed merge CLI's JSON is
    // also untrusted input. Bound every variable-length field the Hub
    // persists (steps, failedStep, reason, error) here — the single
    // chokepoint every GateRunResult passes through — so the bounded
    // run-log builder above is not the only bound on a supplied verdict.
    for step in &mut result.steps {
        step.name = clip_field(std::mem::take(&mut step.name), MAX_STEP_NAME_BYTES);
        step.error = step
            .error
            .take()
            .map(|text| clip_field(text, MAX_STEP_FIELD_BYTES));
        step.reason = step
            .reason
            .take()
            .map(|text| clip_field(text, MAX_STEP_FIELD_BYTES));
    }
    result.failed_step = result
        .failed_step
        .take()
        .map(|text| clip_field(text, MAX_STEP_NAME_BYTES));
    result.error = result
        .error
        .take()
        .map(|text| clip_field(text, OUTPUT_CAP_BYTES));
    result.reason = result
        .reason
        .take()
        .map(|text| clip_field(text, MAX_HEADLINE_BYTES));
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

/// r22 items 3/4/6 test-only seams for the granted-land owner's pipe drains.
/// Per-Node (held on the [`GateRegistry`], never process-global) so parallel
/// tests each drive only their own fixture's seams. Defaults are all inert.
#[cfg(any(test, feature = "test-faults"))]
#[derive(Default)]
pub(crate) struct LandDrainSeams {
    /// r22 item 3: the stderr reader returns a synthetic read error after its
    /// first non-empty read, exercising the "a failed drain is uncertainty"
    /// arm without a real OS read failure.
    pub(crate) stderr_read_fault: bool,
    /// r22 item 4: one-shot hold parked AFTER both pipe joins completed but
    /// BEFORE the final drain-acceptance deadline latch, so a test can let the
    /// original absolute deadline expire with both drains already finished and
    /// then require uncertainty (tokio's timeout polls the inner join first).
    pub(crate) final_accept_hold: Option<Arc<FinalDrainHold>>,
    /// r22 item 5: one-shot hold taken by the survivor-cleanup blocking task
    /// BEFORE it signals the group, modelling blocking-pool queueing/contention
    /// so the response must stop waiting at the original deadline while the
    /// SAME detached owner continuation still runs cleanup and the single
    /// reap.
    pub(crate) cleanup_hold: Option<Arc<CleanupHold>>,
    /// r22 item 6: one-shot, portable replacement for the external-`setsid`
    /// blocked-stderr test. When taken, the stderr reader PARKS at EOF instead
    /// of returning — exactly modelling a surviving holder in ANOTHER session
    /// that keeps the captured stderr write end open after the leader exits
    /// (which same-group cleanup cannot signal). No descendant, no `setsid`,
    /// no timing: valid on macOS.
    pub(crate) stderr_eof_hold: bool,
    /// r24 item 4: one-shot hold taken by the fence FINALIZER's blocking task
    /// (`settle_land_group`) at entry, modelling blocking-pool queueing or
    /// stalled fence filesystem I/O AFTER every owner step (survivor cleanup,
    /// leader reap, both drains) already completed. The response must latch
    /// uncertainty at the absolute deadline even though the land child returned
    /// a recognized definitive report.
    pub(crate) finalize_hold: Option<Arc<CleanupHold>>,
}

/// r22 item 4 test-only hold at the granted-land owner's final drain
/// acceptance.
#[cfg(any(test, feature = "test-faults"))]
pub struct FinalDrainHold {
    /// Notified once the owner reaches the parked point.
    pub(crate) entered: Arc<tokio::sync::Notify>,
    /// The owner parks on this until the test notifies release.
    pub(crate) release: Arc<tokio::sync::Notify>,
}

/// r22 item 5 test-only hold for the granted-land owner's survivor-cleanup
/// blocking task. Uses std primitives because it parks a blocking-pool
/// thread.
#[cfg(any(test, feature = "test-faults"))]
pub struct CleanupHold {
    /// Set once the cleanup task has entered the hold (polled lock-free).
    pub(crate) entered: Arc<std::sync::atomic::AtomicBool>,
    /// Set true + notify to let the parked cleanup proceed.
    pub(crate) release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}

#[cfg(any(test, feature = "test-faults"))]
impl CleanupHold {
    /// Construct an unset, unreleased hold.
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            entered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            release: Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new())),
        })
    }

    /// Whether the parked task has reached the hold (lock-free poll).
    pub fn has_entered(&self) -> bool {
        self.entered.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Release (and wake) any task parked on the hold.
    pub fn release(&self) {
        let (lock, cvar) = &*self.release;
        let mut released = lock.lock().unwrap_or_else(|poison| poison.into_inner());
        *released = true;
        cvar.notify_all();
    }
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
    /// r22 items 3/4/6 test-only seams for the granted-land owner's pipe
    /// drains. Per-Node (NOT process-global), so parallel tests each drive
    /// their own fixture's seams.
    #[cfg(any(test, feature = "test-faults"))]
    land_drain_seams: std::sync::Mutex<LandDrainSeams>,
    /// Durable ledger directory for granted LAND process groups (r14 R1). One
    /// record per granted land, written right after the land child spawns and
    /// removed only once the whole group is verified gone. At Node boot a
    /// leftover record is reconciled read-only: the restarted Node never
    /// signals a group it does not own (R8(a)); only a kernel-proven-dead
    /// group releases the record, every other fence stays as Unknown for
    /// operator resolution. A record present at runtime proves a push may
    /// still land, so a read-only landing probe reports `unknown`.
    land_fence_dir: std::path::PathBuf,
    events: tokio::sync::broadcast::Sender<GateEventParams>,
    /// r36 item 2 test-only: the most recently finished run of one job, so a
    /// real-Node integration test can observe the production land verdict and
    /// its acquired lane without a scripted fake.
    #[cfg(any(test, feature = "test-faults"))]
    last_verdicts: Mutex<std::collections::BTreeMap<String, LastGateVerdict>>,
    /// r37 item 1 test-only: one-shot barrier parked AFTER a land run has
    /// observed group death and persisted its certificate but BEFORE it
    /// releases the lane / unregisters. Per-registry (not global) so only the
    /// test's own DevNode parks; consumed by the first land finalization, so a
    /// later generation-2 dispatch passes straight through.
    #[cfg(any(test, feature = "test-faults"))]
    owner_release_hold: Mutex<Option<std::sync::Arc<CleanupHold>>>,
}

/// r36 item 2 test-only record of one finished gate.run.
#[cfg(any(test, feature = "test-faults"))]
#[derive(Clone)]
pub struct LastGateVerdict {
    /// Dispatch generation that produced the verdict.
    pub generation: u32,
    /// Lane slot the dispatch acquired.
    pub lane_id: String,
    /// Final production verdict status.
    pub status: String,
    /// Verdict error/reason, if any.
    pub error: Option<String>,
}

impl GateRegistry {
    /// Construct the registry rooted at a Node data dir, reconciling any land
    /// groups orphaned by a previous Node process BEFORE returning. The land
    /// child runs in its own process group with kill_on_drop(false), so a Node
    /// crash could leave it free to keep pushing while the Hub could only
    /// guess the outcome. The startup scan makes "the land process group is
    /// verified dead" provable across restarts (r14 R1); per R8(a) it NEVER
    /// signals the orphaned groups — live/uncertain fences are kept and the
    /// Landing goes to operator resolution. With no leftover records — the
    /// normal boot — this is one directory read.
    pub(crate) fn open(land_fence_dir: std::path::PathBuf) -> Arc<Self> {
        #[cfg(unix)]
        {
            // r37 item 4 / r38 item 1: reclaim abandoned cert temps and scan
            // orphaned fences under an exclusive ledger flock, skipping when a
            // detached writer from a previous process still holds the ledger.
            reconcile_ledger_at_startup(&land_fence_dir);
        }
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
            #[cfg(any(test, feature = "test-faults"))]
            land_drain_seams: std::sync::Mutex::new(LandDrainSeams::default()),
            land_fence_dir,
            events,
            #[cfg(any(test, feature = "test-faults"))]
            last_verdicts: Mutex::new(std::collections::BTreeMap::new()),
            #[cfg(any(test, feature = "test-faults"))]
            owner_release_hold: Mutex::new(None),
        })
    }

    /// r37 item 1 test-only: arm the owner-release barrier (one-shot).
    #[cfg(any(test, feature = "test-faults"))]
    pub(crate) async fn arm_owner_release_hold(&self, hold: std::sync::Arc<CleanupHold>) {
        *self.owner_release_hold.lock().await = Some(hold);
    }

    /// r36 item 2 test-only: the most recently finished (generation, status,
    /// error) for a job — the production classification a real-Node
    /// integration test asserts on.
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_last_verdict(&self, job_id: &str) -> Option<LastGateVerdict> {
        self.last_verdicts.lock().await.get(job_id).cloned()
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
    ///
    /// r35 item 2: `merge_sha` is the Hub-granted merge this attempt pushes;
    /// it is durable attempt identity and the finalizer/probe bind evidence to
    /// it. An empty merge refuses the fence (fail closed → uncertain).
    #[cfg(unix)]
    fn record_land_group(
        &self,
        job_id: &str,
        generation: u32,
        pgid: i32,
        merge_sha: &str,
    ) -> std::io::Result<()> {
        // r16 item 1: the injected hold/fail runs while the spawned child is
        // parked on its start barrier, before the real fence write.
        #[cfg(any(test, feature = "test-faults"))]
        self.run_land_fence_fault()?;
        let identity = sample_leader_identity(pgid).ok_or_else(|| {
            std::io::Error::other("could not read the land leader's start-time identity")
        })?;
        write_land_fence(
            &self.land_fence_dir,
            job_id,
            generation,
            pgid,
            &identity,
            merge_sha,
        )
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

    /// r22 item 3 test-only: arm/clear the synthetic stderr read fault for the
    /// granted-land owner.
    #[cfg(any(test, feature = "test-faults"))]
    pub(crate) fn set_land_stderr_read_fault(&self, on: bool) {
        self.land_drain_seams
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .stderr_read_fault = on;
    }

    /// r22 item 3 test-only: whether the land stderr reader should fail after
    /// its first non-empty read.
    #[cfg(any(test, feature = "test-faults"))]
    fn land_stderr_read_fault(&self) -> bool {
        self.land_drain_seams
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .stderr_read_fault
    }

    /// r22 item 6 test-only: arm/clear the portable blocked-stderr seam — the
    /// reader parks at EOF instead of finishing (a surviving pipe holder in
    /// another session).
    #[cfg(any(test, feature = "test-faults"))]
    pub(crate) fn set_land_stderr_eof_hold(&self, on: bool) {
        self.land_drain_seams
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .stderr_eof_hold = on;
    }

    /// r22 item 6 test-only: whether the land stderr reader parks at EOF.
    #[cfg(any(test, feature = "test-faults"))]
    fn land_stderr_eof_hold(&self) -> bool {
        self.land_drain_seams
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .stderr_eof_hold
    }

    /// r22 item 4 test-only: arm the one-shot final drain-acceptance hold.
    #[cfg(any(test, feature = "test-faults"))]
    pub(crate) fn arm_final_drain_accept_hold(&self, hold: Arc<FinalDrainHold>) {
        self.land_drain_seams
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .final_accept_hold = Some(hold);
    }

    /// r22 item 4 test-only: take (and park on) the armed final-acceptance
    /// hold. Returns None when no hold is armed.
    #[cfg(any(test, feature = "test-faults"))]
    async fn take_final_drain_accept_hold(&self) -> Option<Arc<FinalDrainHold>> {
        let hold = {
            self.land_drain_seams
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .final_accept_hold
                .take()
        }?;
        hold.entered.notify_one();
        // Suspend on a Notify (registered before the test can release), never
        // block an async worker on a condvar.
        hold.release.notified().await;
        Some(hold)
    }

    /// r22 item 5 test-only: take the armed survivor-cleanup hold (one-shot).
    #[cfg(any(test, feature = "test-faults"))]
    fn take_cleanup_hold(&self) -> Option<Arc<CleanupHold>> {
        self.land_drain_seams
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .cleanup_hold
            .take()
    }

    /// r22 item 5 test-only: arm the survivor-cleanup blocking-task hold.
    #[cfg(any(test, feature = "test-faults"))]
    pub(crate) fn arm_cleanup_hold(&self, hold: Arc<CleanupHold>) {
        self.land_drain_seams
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .cleanup_hold = Some(hold);
    }

    /// r24 item 4 test-only: take the armed fence-finalizer hold (one-shot).
    #[cfg(any(test, feature = "test-faults"))]
    fn take_finalize_hold(&self) -> Option<Arc<CleanupHold>> {
        self.land_drain_seams
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .finalize_hold
            .take()
    }

    /// r24 item 4 test-only: arm the fence-finalizer blocking-task hold.
    #[cfg(any(test, feature = "test-faults"))]
    pub(crate) fn arm_finalize_hold(&self, hold: Arc<CleanupHold>) {
        self.land_drain_seams
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .finalize_hold = Some(hold);
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

    /// The land leader has already been reaped by the caller (R6 — the owner in
    /// `execute_gate` kills the group WHILE the leader is unreaped, then reaps).
    /// This routine therefore NEVER signals a saved pgid: after the leader is
    /// reaped its pid/pgid can be reused, so a killpg here could hit an
    /// unrelated group. It only OBSERVES `killpg(pgid,0)`; if the group is
    /// already ESRCH it writes the death certificate, otherwise it keeps the
    /// fence for the next-boot fence (Unknown). Straggler termination is the
    /// owner's job, before the reap (r18 item 3).
    ///
    /// r35 item 2: `generation` + `merge_sha` are THIS attempt's identity,
    /// captured when the attempt was created. After observing ESRCH the
    /// finalizer opens ONLY `land-<job>-g<generation>.json` and validates its
    /// full snapshot (including the merge sha and the recorded pgid). It NEVER
    /// discovers an attempt by scanning for the recyclable pgid, so a detached
    /// finalizer for attempt A cannot certify attempt B after pgid reuse.
    #[cfg(unix)]
    async fn settle_land_group(&self, job_id: &str, generation: u32, merge_sha: &str, pgid: i32) {
        let dir = self.land_fence_dir.clone();
        let job_id = job_id.to_owned();
        let merge_sha = merge_sha.to_owned();
        // r24 item 4 test-only: park the finalizer closure on entry AFTER every
        // other owner step completed, so a test can expire the absolute
        // deadline during finalization specifically.
        #[cfg(any(test, feature = "test-faults"))]
        let finalize_hold = self.take_finalize_hold();
        let _ = tokio::task::spawn_blocking(move || {
            #[cfg(any(test, feature = "test-faults"))]
            if let Some(hold) = finalize_hold {
                hold.entered
                    .store(true, std::sync::atomic::Ordering::Release);
                let (rel_lock, rel_cvar) = &*hold.release;
                let mut released = rel_lock.lock().unwrap_or_else(|poison| poison.into_inner());
                // Bounded self-release: a failed assertion that never notifies
                // the hold must not park a blocking thread (and the dropping
                // runtime) for the whole test binary; the happy path notifies
                // well before this.
                while !*released {
                    let (next, timeout) = rel_cvar
                        .wait_timeout(released, Duration::from_secs(20))
                        .unwrap_or_else(|poison| poison.into_inner());
                    released = next;
                    if timeout.timed_out() {
                        *released = true;
                    }
                }
            }
            // Observe only — no signal. The owner killed stragglers before
            // reaping the leader; a still-present group after that is either a
            // group we can no longer safely identify or an EPERM case, both
            // Unknown.
            if land_group_exists(pgid) {
                tracing::warn!(
                    pgid,
                    job_id,
                    "land group still present after the owner reaped its leader; refusing to \
                     signal a possibly-reused pgid (keeping the fence)"
                );
                return;
            }
            // r35 item 2 TEST-ONLY: park AFTER the ESRCH observation but BEFORE
            // the exact-path fence lookup, so a regression can install a newer
            // attempt B with a recycled pgid at exactly this point.
            #[cfg(test)]
            park_settle_after_esrch();
            settle_fence_for_exact_attempt(&dir, &job_id, generation, &merge_sha, pgid);
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
    async fn land_push_liveness(
        &self,
        job_id: &str,
        generation: Option<u32>,
        merge_sha: Option<&str>,
    ) -> LandPushLiveness {
        let table = self.runs.lock().await;
        if let Some(run) = table.runs.get(job_id) {
            // r34 item 2: the OWNER is still registered (its run has not
            // finalized and released the lane). Even a live-kernel ESRCH must
            // NOT be attested as absence while the owner holds the lane: the
            // old finalizer may still be writing/releasing, and requeuing now
            // races the next dispatch into a still-held lane (lane-busy ->
            // spurious Failed). Hold: MayPush while the group is alive, Unknown
            // once it is gone-but-still-owned (durable evidence comes only after
            // the owner finalizes and unregisters).
            let is_requested_run = generation.is_none_or(|requested| run.generation == requested);
            if !is_requested_run {
                // A different attempt owns the job right now: the requested
                // attempt cannot be certified either way.
                drop(table);
                return LandPushLiveness::Unknown;
            }
            let land_pgid = *run
                .land_pgid
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            return match land_pgid {
                Some(pgid) if land_group_exists(pgid) => LandPushLiveness::MayPush,
                // Gone, or group not yet recorded, but the owner still holds
                // the lane — never VerifiedDead until it finalizes.
                Some(_) | None => LandPushLiveness::Unknown,
            };
        }
        drop(table);
        // r35 item 2: the owner is gone, so durable evidence is the only
        // authority; hand the FULL requested identity (generation + granted
        // merge) to the ledger reader. The live-owner arms above never return
        // VerifiedDead, so a wrong/absent merge can only keep the answer Unknown.
        let dir = self.land_fence_dir.clone();
        let job_id = job_id.to_owned();
        let merge_sha = merge_sha.map(str::to_owned);
        tokio::task::spawn_blocking(move || {
            land_ledger_liveness(&dir, &job_id, generation, merge_sha.as_deref())
        })
        .await
        .unwrap_or(LandPushLiveness::Unknown)
    }

    /// Platforms without process-group accounting can never attest death.
    #[cfg(not(unix))]
    async fn land_push_liveness(
        &self,
        _job_id: &str,
        _generation: Option<u32>,
        _merge_sha: Option<&str>,
    ) -> LandPushLiveness {
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

    /// r23 item 3 test-only: the recorded land process-group id for a live run,
    /// so a blocking-pool saturation regression can poll the kernel for the
    /// group a detached owner was still terminating after the response
    /// returned.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_live_land_pgid(&self, job_id: &str) -> Option<i32> {
        self.runs.lock().await.runs.get(job_id).and_then(|run| {
            *run.land_pgid
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
        })
    }

    /// r34 item 2 test-only: insert a synthetic registered live run for
    /// `job_id`@`generation` whose recorded land group is `land_pgid`, then
    /// remove it. Lets the withholding regression run deterministically
    /// without a full land child: while the owner is registered, an
    /// ESRCH-dead group must read Unknown (never Absent); after release, the
    /// durable attempt-qualified certificate attests Absent.
    #[cfg(test)]
    pub(crate) async fn gate_test_insert_land_run(
        &self,
        job_id: &str,
        generation: u32,
        land_pgid: i32,
    ) {
        let (cancel_tx, _cancel_rx) = tokio::sync::watch::channel(false);
        let (land_abort_tx, land_abort_keep_rx) = tokio::sync::watch::channel(false);
        let live = Arc::new(LiveRun {
            generation,
            cancel: cancel_tx,
            pgid: std::sync::Mutex::new(None),
            land_pgid: std::sync::Mutex::new(Some(land_pgid)),
            land_abort: land_abort_tx,
            land_abort_keep_rx,
        });
        self.runs.lock().await.runs.insert(job_id.to_owned(), live);
    }

    /// r34 item 2 test-only: unregister the synthetic run.
    #[cfg(test)]
    pub(crate) async fn gate_test_remove_run(&self, job_id: &str) {
        self.runs.lock().await.runs.remove(job_id);
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

    /// r23 item 3 test-only passthrough for the live land pgid.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_live_land_pgid(&self, job_id: &str) -> Option<i32> {
        self.gate_registry().gate_test_live_land_pgid(job_id).await
    }

    /// r36 item 2 test-only: last production gate verdict for a job.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_last_verdict(&self, job_id: &str) -> Option<LastGateVerdict> {
        self.gate_registry().gate_test_last_verdict(job_id).await
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

    /// Test-only (r22 item 3): make the granted-land owner's stderr reader
    /// return a synthetic read error after its first non-empty read, so a
    /// failed drain deterministically yields an uncertain verdict.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_set_land_stderr_read_fault(&self, on: bool) {
        self.gate_registry().set_land_stderr_read_fault(on);
    }

    /// Test-only (r22 item 6): make the granted-land owner's stderr reader
    /// park at EOF instead of finishing — the portable, macOS-valid
    /// replacement for an external `setsid` holder keeping stderr open.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_set_land_stderr_eof_hold(&self, on: bool) {
        self.gate_registry().set_land_stderr_eof_hold(on);
    }

    /// Test-only (r22 item 4): arm the one-shot hold at the granted-land
    /// owner's final drain acceptance.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_arm_final_drain_accept_hold(&self, hold: Arc<FinalDrainHold>) {
        self.gate_registry().arm_final_drain_accept_hold(hold);
    }

    /// Test-only (r22 item 5): arm the one-shot survivor-cleanup blocking-task
    /// hold so the response must stop waiting at the original deadline while
    /// the same detached owner finishes cleanup and the single reap.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub fn gate_test_arm_cleanup_hold(&self, hold: Arc<CleanupHold>) {
        self.gate_registry().arm_cleanup_hold(hold);
    }

    /// Test-only (r24 item 4): arm the one-shot fence-finalizer blocking-task
    /// hold, so a normal-exit land whose cleanup/reap/drains all finished in
    /// time can still cross the absolute deadline in the finalizer.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub fn gate_test_arm_finalize_hold(&self, hold: Arc<CleanupHold>) {
        self.gate_registry().arm_finalize_hold(hold);
    }

    /// r37 item 1 test-only: arm the owner-release barrier (one-shot).
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn gate_test_arm_owner_release_hold(&self, hold: Arc<CleanupHold>) {
        self.gate_registry().arm_owner_release_hold(hold).await;
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
            // D-057 §7.5: the lane's own fence check — gate.run carries the
            // job's initiator (opId = job id); follow-on steps of a started
            // job carry none and are admitted as its continuation.
            remuda_protocol::METHOD_GATE_RUN => {
                self.direct_op(method, params, self.run_gate(params)).await
            }
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
    /// push. r19 item 1 / R6: this handler NEVER signals a saved process group
    /// (the run task may already have reaped its leader, after which the pgid can
    /// be reused). It delivers an abort MESSAGE to the live run's unique land
    /// owner under the registry lock; the owner alone kills the group while its
    /// leader is unreaped and reaps it, and the run verdict becomes uncertain.
    /// If the land child has not spawned yet the latch also stops the
    /// verify-phase child via the cancel message. A run already gone is a no-op.
    /// The generation is tombstoned like a cancel. r14 R1.
    pub(crate) async fn abort_gate_landing(&self, params: &Value) -> Result<Value, NodeError> {
        let request: GateAbortLandingParams =
            serde_json::from_value(params.clone()).map_err(|error| {
                NodeError::InvalidRequest(format!("invalid gate.abortLanding params: {error}"))
            })?;
        let registry = self.gate_registry();
        // Whether the land child had spawned (its owner will reap it); if not,
        // the verify child is cancelled instead.
        let mut land_spawned = false;
        {
            let mut table = registry.runs.lock().await;
            // r15 item 2: act only on the LIVE run of EXACTLY this generation;
            // a newer attempt is left entirely alone, but the tombstone is still
            // recorded.
            if let Some(run) = table
                .runs
                .get(&request.job_id)
                .filter(|run| run.generation == request.generation)
            {
                // MESSAGE to the unique owner; latches if the land child is
                // still parked behind its start barrier or not yet waited.
                run.land_abort.send(true).ok();
                land_spawned = run
                    .land_pgid
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .is_some();
                if !land_spawned {
                    run.cancel.send(true).ok();
                }
            }
            table.record_cancel(&request.job_id, request.generation);
        }
        // R6: this handler never signals; but it waits (bounded) for the run
        // task — the child's unique owner — to finish killing/reaping and
        // UNREGISTER, so a freshly queued re-dispatch does not race onto a lane
        // whose old owner is still unwinding (the pre-r19 external-kill path had
        // the same synchronous ordering). Pure observation; if the land child
        // had not spawned (verify cancel), or the run is already gone, skip.
        if land_spawned {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let gone = {
                    let table = registry.runs.lock().await;
                    table
                        .runs
                        .get(&request.job_id)
                        .is_none_or(|run| run.generation != request.generation)
                };
                if gone {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
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
        if let Some(run) = table.runs.get(&request.job_id) {
            // r15 item 2 + r19 item 1 (R6): only the live run of EXACTLY this
            // generation receives the cancel MESSAGE; a stale cancel for a newer
            // attempt records its tombstone but never signals. The run task is
            // the child's unique owner — selecting on this watch it kills its
            // own verify group while the leader is unreaped and reaps it; this
            // handler never signals a saved pgid itself.
            if run.generation == request.generation {
                run.cancel.send(true).ok();
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
        //
        // r35 item 2: bind the durable read to the FULL attempt identity the
        // Hub probed — generation AND the granted merge sha (an empty merge is
        // treated as no authority → Unknown).
        let probed_merge = (!request.merge_sha.is_empty()).then_some(request.merge_sha.as_str());
        let land_group_verified_dead = matches!(
            self.gate_registry()
                .land_push_liveness(&request.job_id, request.generation, probed_merge)
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
        // Keep the first receiver alive (stored on LiveRun) so the land-abort
        // value is retained even before the land child owner subscribes.
        let (land_abort_tx, land_abort_keep_rx) = watch::channel(false);
        let live = Arc::new(LiveRun {
            generation: request.generation,
            cancel: cancel_tx,
            pgid: std::sync::Mutex::new(None),
            land_pgid: std::sync::Mutex::new(None),
            land_abort: land_abort_tx,
            land_abort_keep_rx,
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

        // r37 item 1 TEST-ONLY: for a lane-land run, park AFTER execute_gate has
        // observed group death and persisted the attempt-qualified certificate
        // (inside settle_land_group) but BEFORE the lane is released and the run
        // unregistered. This is the exact dead-group/still-registered window the
        // read-only liveness path must keep Unknown; the regression forces a
        // Hub reconciliation here and asserts no requeue. One-shot: the hold is
        // consumed by this first land finalization.
        //
        // r38 item 4: this watchdog is longer than the ENTIRE permitted window
        // the regression holds the owner (the read-only probe deadline plus the
        // test's assertion/polling interval), so a slow in-window reconciliation
        // can never auto-release the owner and invalidate the
        // still-registered assertion. Explicit release remains the normal path.
        #[cfg(any(test, feature = "test-faults"))]
        if request.mode == "land" && request.push_from != remuda_protocol::GatePushFrom::Home {
            let hold = {
                let mut armed = registry.owner_release_hold.lock().await;
                armed.take()
            };
            if let Some(hold) = hold {
                hold.entered
                    .store(true, std::sync::atomic::Ordering::Release);
                let (lock, cvar) = &*hold.release;
                let mut released = lock.lock().unwrap_or_else(|poison| poison.into_inner());
                while !*released {
                    let (next, timeout) = cvar
                        .wait_timeout(released, OWNER_RELEASE_HOLD_WATCHDOG)
                        .unwrap_or_else(|poison| poison.into_inner());
                    released = next;
                    if timeout.timed_out() {
                        *released = true;
                    }
                }
            }
        }

        // Release the lane lock BEFORE unregistering the run: a registry entry
        // must therefore imply the lane is still held, so an abort that waits for
        // the entry to disappear is guaranteed the lane guard is free (r19
        // item 1 / R6).
        drop(lane_guard);
        {
            let mut table = registry.runs.lock().await;
            table.runs.remove(&request.job_id);
        }
        // r36 item 2 test-only: retain the last finished verdict (with the
        // acquired lane) so a real-Node regression can observe the PRODUCTION
        // classification and lane acquisition, not a fake's canned reply.
        #[cfg(any(test, feature = "test-faults"))]
        {
            let mut last = registry.last_verdicts.lock().await;
            last.insert(
                request.job_id.clone(),
                LastGateVerdict {
                    generation: request.generation,
                    lane_id: request.lane_id.clone(),
                    status: outcome.status.clone(),
                    error: outcome.error.clone(),
                },
            );
        }

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
                //
                // r35 item 2: the fence binds the attempt's FULL identity — the
                // Hub-granted merge sha from the passing verify is the one the
                // grant was taken for (and the `--report` land arg pins), so it
                // is authoritative for THIS generation.
                let granted_merge_sha = result
                    .merge_sha
                    .as_deref()
                    .filter(|sha| !sha.is_empty())
                    .unwrap_or("");
                if let Err(error) = barrier.release_after(|| {
                    registry.record_land_group(
                        &request.job_id,
                        request.generation,
                        pgid,
                        granted_merge_sha,
                    )
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
            // r21 item 1 (R8(b) owner contract): drain stdout on its OWN task
            // CONCURRENTLY with the non-reaping exit observation. The old owner
            // did `read_to_end` BEFORE its first waitid poll, so a descendant
            // that inherited stdout hid the leader's exit: the valid verdict
            // sat unread until the soft land timeout. This task keeps the bytes
            // (the leader's final JSON) while the owner observes the exit, and
            // it is JOINED only after survivor cleanup and the reap, inside the
            // absolute deadline.
            let mut land_stdout_task = land_stdout.map(|mut stdout| {
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    // r22 item 3: drain RAW BYTES to EOF and report the
                    // outcome explicitly. A read error is never swallowed into
                    // a successful empty/partial drain — the owner treats it as
                    // uncertainty rather than building a verdict on truncated
                    // output.
                    let mut stdout_bytes = Vec::new();
                    stdout
                        .read_to_end(&mut stdout_bytes)
                        .await
                        .map(|_| stdout_bytes)
                })
            });
            // r13 item 6: drain stderr CONCURRENTLY for the life of the child
            // (and keep the handle for timeout/kill). A remote rejection can
            // print far more than one pipe buffer; never reading it blocked the
            // land child forever (deadlock) — the exact failure the verify child
            // already guarded against with its own stderr pump.
            //
            // r14 item 9: the drained bytes are ALSO accumulated (the child's
            // stderr handle is consumed by this pump, so the waited Output's
            // stderr is always empty). A nonzero land exit then reports the
            // captured remote rejection/error text instead of a blank reason.
            //
            // r22 item 3: the pump drains RAW BYTES (never `BufReader::lines`,
            // whose UTF-8 decoding error silently ends the loop and is reported
            // as a successful drain). Decoding is lossy and happens ONLY for
            // the live line presentation; the captured evidence keeps the raw
            // bytes. A real read error is returned so the owner fails closed.
            let captured_stderr: Arc<std::sync::Mutex<Vec<u8>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));
            let mut land_stderr_task = land_child.stderr.take().map(|mut stderr| {
                let node = self.clone();
                let job_id = request.job_id.clone();
                let generation = request.generation;
                let captured = captured_stderr.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut buf = [0u8; 4096];
                    // r23 item 4: raw-byte pending carried across reads, used
                    // ONLY to split the lossy presentation into lines. Framing
                    // on the raw bytes (not a per-chunk lossy String) keeps a
                    // valid multi-byte character intact when it is split
                    // across reads.
                    let mut framer = LossyLineFramer::new();
                    let emit = |node: &DevNode, message: String| {
                        let _ = node.gate_registry().events.send(GateEventParams {
                            job_id: job_id.clone(),
                            generation,
                            kind: GateEventKind::Log { message },
                        });
                    };
                    loop {
                        match stderr.read(&mut buf).await {
                            Ok(0) => {
                                // r22 item 6 portable seam: a surviving holder
                                // in ANOTHER session keeps the captured stderr
                                // open after the leader exits, so EOF never
                                // really arrives and the drain must stay
                                // pending (the owner's bounded join then fails
                                // closed). Park instead of finishing — no
                                // descendant or `setsid`, valid on macOS. The
                                // task is detached and aborted at test-runtime
                                // shutdown, exactly like a real abandoned
                                // reader.
                                #[cfg(any(test, feature = "test-faults"))]
                                if node.gate_registry().land_stderr_eof_hold() {
                                    std::future::pending::<()>().await;
                                    unreachable!("the EOF hold parks until runtime shutdown");
                                }
                                break;
                            }
                            Ok(n) => {
                                if let Ok(mut raw) = captured.lock() {
                                    raw.extend_from_slice(&buf[..n]);
                                }
                                framer.push(&buf[..n], |line| emit(&node, line));
                                // r22 item 3 deterministic seam: fail AFTER a
                                // non-empty read so valid bytes exist but the
                                // drain is explicitly unsuccessful.
                                #[cfg(any(test, feature = "test-faults"))]
                                if node.gate_registry().land_stderr_read_fault() {
                                    return Err(std::io::Error::other(
                                        "synthetic land stderr read fault (r22 item 3 test seam)",
                                    ));
                                }
                            }
                            // A signal interrupts one read; the drain continues.
                            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                                continue;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    // Flush the final unterminated line for presentation.
                    framer.finish(|line| emit(&node, line));
                    std::io::Result::Ok(())
                })
            });
            // Land runs to completion even if cancel fires (grant already
            // held); await it bounded by the (test-overridable) land budget.
            // wait_with_output_piped BORROWS the child (unlike the consuming
            // wait_with_output), so on timeout the child is still owned here
            // and can be killed and reaped.
            let land_budget = registry.land_timeout().await;
            // r20 items 4+5 / R8(b): the land_budget is the SOFT cap (when a
            // still-running leader is KILLed); the ABSOLUTE deadline adds the
            // fixed owner cleanup allowance (TERM/KILL settle + bounded reap +
            // pipe drains) and bounds the ENTIRE sequence. A reap or pipe
            // reader still blocked at the absolute deadline is abandoned inside
            // this owner (detached reaper/task) and the response is uncertain;
            // it never blocks the settlement.
            let land_deadline = std::time::Instant::now() + land_budget + LAND_OWNER_CLEANUP_BUDGET;
            // r19 item 1 / R6: abort is a MESSAGE to this child's unique owner.
            // Subscribe before the barrier release so an abort latched while the
            // child is still parked is observed by the select below.
            let mut land_abort_rx = live.land_abort.subscribe();
            let mut land_aborted = false;
            // Scope the waiting future so its borrow of `land_child` is
            // released before the timeout arm kills and reaps the child.
            // r19 item 3: a normal exit is observed WNOWAIT (leader UNREAPED)
            // so the owner cleans the group before reaping.
            let land_outcome: Result<Option<ObservedExit>, ()> = {
                let wait = land_child.observe_leader_exit();
                tokio::pin!(wait);
                // Resolves if a latch was already set OR on a later send.
                let land_abort_fired = async {
                    loop {
                        if *land_abort_rx.borrow_and_update() {
                            return;
                        }
                        if land_abort_rx.changed().await.is_err() {
                            // Sender dropped without abort: never fire.
                            return std::future::pending::<()>().await;
                        }
                    }
                };
                tokio::select! {
                    output = &mut wait => Ok(output),
                    _ = tokio::time::sleep(land_budget) => Err(()),
                    _ = land_abort_fired => {
                        land_aborted = true;
                        Err(())
                    }
                }
            };
            // r21 item 2: the stderr (and stdout) reader handles are RETAINED
            // through survivor cleanup and the leader reap on every arm below.
            // They are joined only AFTER the reap, with the remaining absolute
            // budget; an incomplete or failed drain of either pipe makes the
            // verdict uncertain. No reader is joined, timed out and discarded
            // here before cleanup (the old owner did exactly that, so a
            // blocked stderr pump was ignored and a definitive verdict was
            // returned while the reader was still blocked).
            match land_outcome {
                Err(()) => {
                    // Timeout OR an abortLanding message. THIS run task is the
                    // land child's unique owner (r19 item 1 / R6): it alone
                    // terminates the WHOLE land group (TERM, bounded wait, KILL)
                    // WHILE THE LEADER IS UNREAPED and then reaps it — no other
                    // task signals the saved pgid. Report UNCERTAIN either way
                    // (the remote may already hold the ref); the Hub reconciles.
                    //
                    // r23 item 3: the ordered terminate→reap sequence is ONE
                    // owner continuation raced against the ORIGINAL absolute
                    // deadline, exactly like the normal-exit arm. The blocking
                    // terminate (which itself waits on Tokio's blocking pool)
                    // is no longer joined unbounded: when the pool is saturated
                    // so the closure stays QUEUED past the deadline, the SAME
                    // continuation detaches and finishes terminate + the single
                    // reap while the response already returned uncertain.
                    // r24 item 7: whether the ordered terminate→reap
                    // continuation actually COMPLETED before the response. When
                    // it detached past the deadline the group may still be
                    // alive and may still push, so the verdict text must say
                    // cleanup is pending in the owner rather than claim the
                    // group was terminated.
                    let mut cleanup_completed = true;
                    #[cfg(unix)]
                    {
                        let term_pgid = land_pgid;
                        #[cfg(any(test, feature = "test-faults"))]
                        let cleanup_hold = registry.take_cleanup_hold();
                        let mut continuation: std::pin::Pin<
                            Box<dyn std::future::Future<Output = ()> + Send>,
                        > = Box::pin(async move {
                            if let Some(pgid) = term_pgid {
                                let _ = tokio::task::spawn_blocking(move || {
                                    // Same pool-contention hold seam as the
                                    // normal-exit cleanup, so a test may park
                                    // the executing closure too.
                                    #[cfg(any(test, feature = "test-faults"))]
                                    if let Some(hold) = cleanup_hold {
                                        hold.entered
                                            .store(true, std::sync::atomic::Ordering::Release);
                                        let (rel_lock, rel_cvar) = &*hold.release;
                                        let mut released = rel_lock
                                            .lock()
                                            .unwrap_or_else(|poison| poison.into_inner());
                                        while !*released {
                                            released = rel_cvar
                                                .wait(released)
                                                .unwrap_or_else(|poison| poison.into_inner());
                                        }
                                    }
                                    terminate_group(pgid);
                                })
                                .await;
                            }
                            // Reap the leader exactly once, AFTER the
                            // terminate finished — the only reap on this arm.
                            let _ = land_child.wait().await;
                        });
                        if tokio::time::timeout(remaining(land_deadline), continuation.as_mut())
                            .await
                            .is_err()
                        {
                            cleanup_completed = false;
                            tokio::spawn(async move {
                                continuation.await;
                            });
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = land_child.kill().await;
                        let _ = land_child.wait().await;
                    }
                    result.status = "uncertain".into();
                    if !cleanup_completed {
                        // The detached owner continuation still owes the
                        // terminate→reap: do not claim the group is dead (it
                        // may push while the cleanup is queued).
                        let detail = if land_aborted {
                            "land was aborted, but owner cleanup (terminating and reaping the land \
                             process group) had not completed by the response deadline — cleanup is \
                             pending in the owner and the group is not yet confirmed terminated; \
                             the push outcome is unknown (the Hub reconciles against the remote)"
                        } else {
                            "land exceeded the time budget, but owner cleanup (terminating and \
                             reaping the land process group) had not completed by the response \
                             deadline — cleanup is pending in the owner and the group is not yet \
                             confirmed terminated; the push outcome is unknown (the Hub reconciles \
                             against the remote)"
                        };
                        result.error = Some(detail.into());
                        result.reason = Some(
                            "cleanup pending in owner; land group termination not yet confirmed; \
                             push outcome uncertain"
                                .into(),
                        );
                    } else if land_aborted {
                        result.error = Some(
                            "land was aborted; the land process group was terminated by its owner; \
                             the push outcome is unknown (the Hub reconciles against the remote)"
                                .into(),
                        );
                        result.reason = Some("land aborted; push outcome uncertain".into());
                    } else {
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
                }
                Ok(Some(observed)) => {
                    // The leader EXITED. On unix that exit was observed
                    // WNOWAIT, so the leader is still UNREAPED: THIS owner —
                    // the only signaler — kills every surviving same-group
                    // descendant BEFORE reaping while the unreaped leader pins
                    // its pgid (r19 item 3). A detached-output runaway can no
                    // longer outlive the land and hold its fence.
                    //
                    // r21 item 1 — R8(b) owner sequence: the exit was observed
                    // independently of the pipes, so after observing it the
                    // owner cleans the group, reaps the leader, and ONLY then
                    // joins the concurrent stdout reader, all bounded by the
                    // absolute deadline. The survivor KILL is what closes the
                    // last stdout write end a descendant inherited; without
                    // the concurrent observation the owner could not even
                    // begin this sequence until EOF.
                    let land_status = match observed {
                        ObservedExit::Unreaped => {
                            // r22 item 5: cleanup AND the single leader reap are
                            // ONE owner continuation, in that fixed order. The
                            // response races the WHOLE continuation against the
                            // ORIGINAL absolute deadline — this also bounds
                            // blocking-pool QUEUEING time (the old code awaited
                            // the cleanup JoinHandle with no timeout, so a
                            // closure stuck behind a busy pool blocked the
                            // response indefinitely).
                            //
                            // If the deadline expires, the SAME continuation is
                            // detached to finish cleanup and the reap; the
                            // response never reaps independently while a queued
                            // cleanup could later run and signal a REUSED group,
                            // and the corpse is still reaped exactly once.
                            #[cfg(any(test, feature = "test-faults"))]
                            let cleanup_hold = registry.take_cleanup_hold();
                            let mut continuation: std::pin::Pin<
                                Box<
                                    dyn std::future::Future<
                                            Output = Option<std::process::ExitStatus>,
                                        > + Send,
                                >,
                            > = Box::pin(async move {
                                #[cfg(unix)]
                                if let Some(pgid) = land_pgid {
                                    // R8(b): THE OWNER CONTRACT's survivor
                                    // cleanup, shared with every recovery git
                                    // subprocess (remuda-unix). It clamps its
                                    // own waits to the absolute deadline.
                                    let clean = tokio::task::spawn_blocking(move || {
                                        #[cfg(any(test, feature = "test-faults"))]
                                        if let Some(hold) = cleanup_hold {
                                            // Signal entry (lock-free atomic),
                                            // then park on release until the test
                                            // lets cleanup proceed.
                                            hold.entered
                                                .store(true, std::sync::atomic::Ordering::Release);
                                            let (rel_lock, rel_cvar) = &*hold.release;
                                            let mut released = rel_lock
                                                .lock()
                                                .unwrap_or_else(|poison| poison.into_inner());
                                            while !*released {
                                                released = rel_cvar
                                                    .wait(released)
                                                    .unwrap_or_else(|poison| poison.into_inner());
                                            }
                                        }
                                        remuda_unix::clean_surviving_group_until(
                                            pgid,
                                            land_deadline,
                                        )
                                    });
                                    // Detach only the blocking handle when the
                                    // whole continuation is handed off below;
                                    // until then the response waits here.
                                    let _ = clean.await;
                                }
                                // Reap the leader exactly once, AFTER cleanup
                                // finished — this is the only place the child is
                                // reaped on this arm.
                                land_child.wait().await.ok()
                            });
                            match tokio::time::timeout(
                                remaining(land_deadline),
                                continuation.as_mut(),
                            )
                            .await
                            {
                                Ok(status) => status,
                                Err(_) => {
                                    // Deadline elapsed (queued/running cleanup or
                                    // a blocked reap): hand the SAME ordered
                                    // continuation to a detached owner and answer
                                    // uncertain. No independent response-path
                                    // reap.
                                    tokio::spawn(async move {
                                        let _ = continuation.await;
                                    });
                                    None
                                }
                            }
                        }
                        ObservedExit::Reaped(status) => Some(status),
                    };
                    // r21 item 2 / r22 item 3 — THE OWNER CONTRACT's drain
                    // step: join BOTH reader tasks here. They drained
                    // CONCURRENTLY with the exit observation and were RETAINED
                    // through survivor cleanup and the reap; each is now awaited
                    // with only the REMAINING absolute budget. An incomplete OR
                    // failed drain of EITHER pipe is UNCERTAINTY, never a
                    // definitive land: a descendant in ANOTHER session
                    // (`setsid sleep … >/dev/null &` keeps stderr in a session
                    // the group KILL cannot reach) can hold a pipe long after
                    // the leader's reaping, and no verdict may be built while a
                    // pipe is still open or its reader FAILED (join panic OR a
                    // raw read error, not just an elapsed timeout).
                    let mut drain_ok = !remaining(land_deadline).is_zero();
                    let stdout_bytes = if !drain_ok {
                        None
                    } else if let Some(task) = land_stdout_task.take() {
                        match tokio::time::timeout(remaining(land_deadline), task).await {
                            // Joined, EOF reached, raw bytes in hand.
                            Ok(Ok(Ok(bytes))) => Some(bytes),
                            // Elapsed timeout, a panicked/aborted join, OR the
                            // reader's explicit read error — all uncertainty.
                            _ => {
                                drain_ok = false;
                                None
                            }
                        }
                    } else {
                        Some(Vec::new())
                    };
                    // r22 item 4: LATCH after the first joined step. Tokio's
                    // timeout POLLS THE INNER FUTURE FIRST, so an already
                    // complete join can resolve even at/after a zero remaining
                    // budget; the clock is re-checked explicitly here.
                    if std::time::Instant::now() >= land_deadline {
                        drain_ok = false;
                    }
                    if drain_ok && let Some(task) = land_stderr_task.take() {
                        match tokio::time::timeout(remaining(land_deadline), task).await {
                            // Joined and drained to EOF.
                            Ok(Ok(Ok(()))) => {}
                            // Deadline elapsed, the join failed, or the raw
                            // reader reported a read error: the captured bytes
                            // are partial or the pipe is still held.
                            _ => drain_ok = false,
                        }
                    }
                    // r22 item 4 deterministic seam: park at FINAL drain
                    // acceptance (both joins done) so a test can expire the
                    // original deadline with both drains already finished.
                    #[cfg(any(test, feature = "test-faults"))]
                    registry.take_final_drain_accept_hold().await;
                    // r22 item 4: LATCH immediately BEFORE accepting any
                    // definitive outcome — a drain that completed only after
                    // the absolute deadline expired (while the owner was
                    // descheduled, behind on the blocking pool, or parked at
                    // the seam) does not un-expire the deadline; the land is
                    // uncertain even if every join returned Ok.
                    if std::time::Instant::now() >= land_deadline {
                        drain_ok = false;
                    }
                    let land_output = match (land_status, stdout_bytes, drain_ok) {
                        (Some(status), Some(stdout), true) => Some(std::process::Output {
                            status,
                            stdout,
                            stderr: Vec::new(),
                        }),
                        _ => None,
                    };
                    // r22 item 3: decode the RAW captured bytes LOSSILY only
                    // for presentation (a nonzero exit's reason / evidence).
                    let drained_stderr = captured_stderr
                        .lock()
                        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                        .unwrap_or_default();
                    match land_output {
                        Some(land_output) if land_output.status.success() => {
                            // r17 item 4: a zero exit does NOT bypass validation.
                            // The child must return a parseable, recognized `landed`
                            // report whose `merged` sha is the EXACT granted/verified
                            // merge; a malformed/missing report, an unknown status,
                            // or a merge identity mismatch is uncertain regardless
                            // of the exit code (the Hub reconciles rather than
                            // trust a bare success).
                            match classify_released_land_outcome(
                                &land_output,
                                &drained_stderr,
                                result.merge_sha.as_deref(),
                            ) {
                                ReleasedLandOutcome::Landed(land_report) => {
                                    result.status = "landed".into();
                                    result.current_main_sha =
                                        string_field(&land_report, "currentMain");
                                    result.merge_sha =
                                        string_field(&land_report, "merged").or(result.merge_sha);
                                }
                                ReleasedLandOutcome::BaseMoved(land_report) => {
                                    // A recognized CAS-loss verdict on a zero
                                    // exit is honored (the CLI uses nonzero, but
                                    // accept the structured verdict either way).
                                    result.status = "base-moved".into();
                                    result.current_main_sha =
                                        string_field(&land_report, "currentMain");
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
                        Some(output) => {
                            // r16 item 4 (R5): the child was RELEASED, so its
                            // push may have reached the remote. Only a
                            // RECOGNIZED structured verdict may settle the
                            // Landing; a crash (signal), a
                            // missing/malformed/unparseable report, or an
                            // unrecognized status is UNCERTAIN — the Hub
                            // reconciles against origin — never a definite
                            // failure.
                            match classify_released_land_outcome(
                                &output,
                                &drained_stderr,
                                result.merge_sha.as_deref(),
                            ) {
                                ReleasedLandOutcome::Landed(land_report) => {
                                    result.status = "landed".into();
                                    result.current_main_sha =
                                        string_field(&land_report, "currentMain");
                                    result.merge_sha =
                                        string_field(&land_report, "merged").or(result.merge_sha);
                                }
                                ReleasedLandOutcome::BaseMoved(land_report) => {
                                    // `merge --land` exits nonzero when the
                                    // compare-and-swap lost (base moved under
                                    // the lease): keep the requeue verdict and
                                    // currentMain.
                                    result.status = "base-moved".into();
                                    result.current_main_sha =
                                        string_field(&land_report, "currentMain");
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
                        None if land_status.is_some() && !drain_ok => {
                            // r21 item 2: the leader genuinely exited and was
                            // reaped, but at least one pipe did not drain to
                            // EOF (or its reader failed) before the absolute
                            // deadline. That is uncertainty, never a definitive
                            // land: the push may already have reached the
                            // remote and a reader still blocked means the
                            // captured output is partial.
                            result.status = "uncertain".into();
                            result.error = Some(
                                "the released land leader exited and was reaped, but its \
                                 stdout/stderr did not both drain to EOF before the absolute \
                                 cleanup deadline (a surviving pipe holder outside the land \
                                 group can cause this); the push may have reached the remote; \
                                 the Hub reconciles against the exact granted merge"
                                    .into(),
                            );
                            result.reason = Some(
                                "land pipe drain incomplete at the absolute deadline; push \
                                 outcome uncertain"
                                    .into(),
                            );
                        }
                        None => {
                            // r16 item 4: no wait status after the release is
                            // not an established failure: the push may have
                            // landed.
                            result.status = "uncertain".into();
                            result.error = Some(
                                "the released land child ended without any result; the push may \
                                 have reached the remote; the Hub reconciles against the exact \
                                 granted merge"
                                    .into(),
                            );
                            result.reason = Some(
                                "land child produced no result; push outcome uncertain".into(),
                            );
                        }
                    }
                }
                Ok(None) => {
                    // The non-reaping observation could not even see the
                    // leader; the push may have landed — uncertain.
                    result.status = "uncertain".into();
                    result.error = Some(
                        "the released land child could not be observed at exit; the push may \
                         have reached the remote; the Hub reconciles against the exact granted \
                         merge"
                            .into(),
                    );
                    result.reason =
                        Some("land child produced no result; push outcome uncertain".into());
                }
            }
            // The child has exited (or been killed on timeout); let the
            // remaining reader tasks drain to EOF (bounded by the absolute land
            // deadline — R8(b): a task still blocked then is ABANDONED, never
            // joined past it) so a large final burst is fully read whenever
            // time remains, and a wedged pipe never delays the verdict. The
            // normal-exit arm already joined whichever task it needed; only a
            // handle left here is awaited.
            if let Some(task) = land_stdout_task.take() {
                let _ = tokio::time::timeout(
                    Duration::from_secs(2).min(remaining(land_deadline)),
                    task,
                )
                .await;
            }
            if let Some(task) = land_stderr_task {
                let _ = tokio::time::timeout(
                    Duration::from_secs(2).min(remaining(land_deadline)),
                    task,
                )
                .await;
            }
            // r14 R1: the leader was awaited on every arm above; finish any
            // surviving group member, verify the group is ESRCH, then release
            // the durable fence record so a later remote probe may attest
            // `absent`. Never release it while the group can still push.
            //
            // r23 item 3: this non-signalling finalizer is BOUNDED by the
            // original absolute deadline. It only OBSERVES killpg and, when the
            // group is gone, certifies/removes the fence — it never signals — so
            // if a saturated blocking pool keeps its closure queued past the
            // deadline the response returns uncertain and the already-queued
            // blocking closure keeps running detached (dropping a
            // spawn_blocking JoinHandle does not cancel the task); a group it
            // still finds present (or never reaches) simply keeps the fence for
            // the next-boot reconciler. No unbounded blocking join on the
            // response path.
            #[cfg(unix)]
            let mut fence_finalization_expired = false;
            #[cfg(unix)]
            if let Some(pgid) = land_pgid {
                // r35 item 2: pass THIS attempt's exact identity (captured from
                // the passing verify/grant) so a detached finalizer opens only
                // its own fence path; a recyclable pgid is never a lookup key.
                let settle = registry.settle_land_group(
                    &request.job_id,
                    request.generation,
                    result.merge_sha.as_deref().unwrap_or(""),
                    pgid,
                );
                if tokio::time::timeout(remaining(land_deadline), settle)
                    .await
                    .is_err()
                {
                    fence_finalization_expired = true;
                    tracing::debug!(
                        job_id = %request.job_id,
                        "land fence finalizer still queued/running at the absolute deadline; \
                         it continues detached and the fence stays for reconciliation"
                    );
                }
            }
            // r24 item 4: the fence finalizer crossing the absolute deadline
            // (a saturated blocking pool queuing its closure, or fence
            // filesystem I/O stalling) is itself an expiry. The child may have
            // returned a recognized definitive report and every earlier owner
            // step (survivor cleanup, the single leader reap, both drains) may
            // have finished in time — but the push could still complete only
            // through fence I/O that has not returned, so a definitive
            // landed/failed verdict carried here is unsafe. Latch uncertainty
            // (an already-uncertain timeout/abort verdict keeps its own
            // reason), and RECHECK THE ABSOLUTE CLOCK after finalization:
            // tokio's timeout polls the inner future first, so a finalization
            // accepted at/after a zero remaining budget reports Ok and must be
            // caught here too. The Hub reconciles the exact granted merge
            // against the remote; the detached finalizer still certifies the
            // fence when it eventually completes.
            #[cfg(unix)]
            if (fence_finalization_expired || std::time::Instant::now() >= land_deadline)
                && result.status != "uncertain"
            {
                result.status = "uncertain".into();
                result.error = Some(
                    "the land child returned a definitive report, but its fence finalization did \
                     not complete before the absolute deadline (blocking-pool contention or stalled \
                     fence I/O); the push outcome is unknown and the Hub reconciles against the \
                     exact granted merge"
                        .into(),
                );
                result.reason = Some(
                    "land fence finalization crossed the absolute deadline; push outcome uncertain"
                        .into(),
                );
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
/// Established or Unknown (r16 item 4; r17 item 4 hardens the success arm):
/// * exit 0 → `Landed` ONLY with a parseable report whose status is `landed`
///   AND whose `merged` sha names the exact granted merge; a bare zero exit
///   with missing/malformed JSON, an unknown status, a missing merge or a
///   mismatched merge is `Uncertain` (the exit code never bypasses validation);
/// * a recognized structured verdict → `BaseMoved` (CAS loss, nothing pushed),
///   `Uncertain` (the CLI itself could not establish the push), or `Failed`
///   ONLY for `gate_failed` at exit 1 — a pre-push rejection or a
///   recovery-ESTABLISHED remote rejection, both proving the exact merge did
///   not land;
/// * death by signal (no exit code) or an unrecognized status → `Uncertain`,
///   never Failed: the child may have pushed and crashed before reporting, so
///   the Hub must reconcile.
fn classify_released_land_outcome(
    output: &std::process::Output,
    drained_stderr: &str,
    granted_merge: Option<&str>,
) -> ReleasedLandOutcome {
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
    // Missing or malformed JSON: no recognized verdict, Unknown on EVERY exit
    // path (a zero exit with garbage output cannot attest a landing).
    let Ok(report) = serde_json::from_slice::<Value>(&output.stdout) else {
        return ReleasedLandOutcome::Uncertain {
            error: format!(
                "the land child exited {exit_code} without a parseable result after it was \
                 released; the push may have reached the remote (stderr: {stderr_text})"
            ),
            reason: "land child returned no readable verdict; push outcome uncertain".into(),
        };
    };
    let status = report.get("status").and_then(Value::as_str);
    // r18 item 1: EVERY definitive outcome (landed/base_moved/gate_failed) must
    // carry a nonempty "merged" equal to the GRANTED merge; a refusal with a
    // missing or different merge cannot be trusted to describe THIS attempt and
    // is Unknown. The outer result keeps the verified merge_sha regardless.
    let reported_merge = string_field(&report, "merged");
    let merge_matches_grant = |outcome: &str| -> bool {
        match (
            granted_merge.filter(|sha| !sha.is_empty()),
            reported_merge.as_deref(),
        ) {
            (Some(granted), Some(reported)) => {
                if reported != granted {
                    tracing::warn!(
                        outcome,
                        granted,
                        reported,
                        "land verdict merge does not match the grant; treating as Unknown"
                    );
                    false
                } else {
                    true
                }
            }
            _ => {
                tracing::warn!(
                    outcome,
                    "land verdict carries no nonempty merged sha for the grant; Unknown"
                );
                false
            }
        }
    };
    if output.status.success() {
        // r17 item 4: a zero exit must report the recognized `landed` verdict
        // for the EXACT granted merge. Anything else is Unknown.
        if status == Some("landed") && merge_matches_grant("landed") {
            return ReleasedLandOutcome::Landed(report);
        }
        // A structured CAS-loss verdict riding a zero exit is honored only when
        // it names the granted merge.
        if matches!(status, Some("base_moved") | Some("base-moved"))
            && merge_matches_grant("base_moved")
        {
            return ReleasedLandOutcome::BaseMoved(report);
        }
        return ReleasedLandOutcome::Uncertain {
            error: format!(
                "the land child exited 0 but its report did not establish the granted merge landed \
                 (status={status:?}, reported merged={reported_merge:?}, granted={granted_merge:?}); \
                 the push outcome is uncertain (stderr: {stderr_text})"
            ),
            reason: "zero-exit land report missing/mismatched; push outcome uncertain".into(),
        };
    }
    match status {
        Some("base_moved") | Some("base-moved") if merge_matches_grant("base_moved") => {
            ReleasedLandOutcome::BaseMoved(report)
        }
        Some("uncertain") => ReleasedLandOutcome::Uncertain {
            error: string_field(&report, "error").unwrap_or_else(|| {
                "land push outcome could not be established against the remote".into()
            }),
            reason: "land push outcome unknown; the Hub reconciles against the remote".into(),
        },
        // The merge CLI's one recognized failure verdict (exit 1): it only
        // reaches this status for a pre-push error (nothing pushed) or after
        // recovery established the exact merge absent with the remote base
        // unchanged (a genuine rejection). It must name the granted merge.
        Some("gate_failed") if exit_code == 1 && merge_matches_grant("gate_failed") => {
            ReleasedLandOutcome::Failed(format!(
                "land failed after grant (exit 1): {}",
                string_field(&report, "error")
                    .unwrap_or_else(|| stderr_text.clone())
                    .trim()
            ))
        }
        other => ReleasedLandOutcome::Uncertain {
            error: format!(
                "the land child exited {exit_code} with an unrecognized or merge-mismatched \
                 verdict {other:?} (reported merged={reported_merge:?}, granted={granted_merge:?}); \
                 the push may have reached the remote (stderr: {stderr_text})"
            ),
            reason: "land child verdict unrecognized/merge-mismatched; push outcome uncertain"
                .into(),
        },
    }
}

/// Extension trait so a piped-stdout child can still consume wait_with_output.
trait WaitWithOutputPiped {
    async fn wait_with_output_piped(
        &mut self,
        stdout: Option<tokio::process::ChildStdout>,
    ) -> Option<std::process::Output>;

    /// r19 item 3 / R6, r20 R8(b)/(c), r21 item 1: wait until the leader
    /// EXITS, WITHOUT reading any pipe. stdout and stderr are drained by their
    /// OWN tasks CONCURRENTLY with this observation, so a descendant that
    /// inherits a pipe can no longer hide the leader's exit behind EOF. On
    /// unix the exit is observed WITHOUT reaping
    /// (`waitid(WEXITED|WNOHANG|WNOWAIT)`, portable via libc) so the owner can
    /// clean surviving same-group members while the leader is still unreaped
    /// and only then reap; other platforms reap here and report `Reaped`.
    async fn observe_leader_exit(&mut self) -> Option<ObservedExit>;
}

/// How [`WaitWithOutputPiped::observe_leader_exit`] saw the leader leave.
/// r21 item 1: the bytes live on the concurrent stdout drain task, so neither
/// variant carries them.
enum ObservedExit {
    /// unix: exit observed, leader still UNREAPED.
    Unreaped,
    /// non-unix fallback: the child was reaped the ordinary way; carries its
    /// exit status.
    #[allow(dead_code)] // constructed only on non-unix
    Reaped(std::process::ExitStatus),
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

    #[cfg(unix)]
    async fn observe_leader_exit(&mut self) -> Option<ObservedExit> {
        let leader_pid = self.id()? as i32;
        loop {
            // waitid must run off the async worker: spawn_blocking per poll.
            // R8(c): a failed/aborted blocking task is `None` — NOT an exit
            // observation — exactly like an EINVAL/ECHILD from waitid: the
            // owner keeps waiting (the land budget bounds the loop) and may
            // never conclude the leader exited on an infrastructure error.
            let observed =
                tokio::task::spawn_blocking(move || leader_exit_observed_unreaped(leader_pid))
                    .await
                    .ok()
                    .flatten();
            match observed {
                Some(LeaderExitObservation::Exited) => return Some(ObservedExit::Unreaped),
                Some(LeaderExitObservation::StillRunning) | None => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    }

    #[cfg(not(unix))]
    async fn observe_leader_exit(&mut self) -> Option<ObservedExit> {
        self.wait().await.ok().map(ObservedExit::Reaped)
    }
}

/// What one non-reaping `waitid` poll established about the land leader.
#[cfg(unix)]
#[derive(Debug)]
enum LeaderExitObservation {
    /// The leader EXITED; the zombie is deliberately left unreaped (WNOWAIT)
    /// so the unique owner can clean the group before reaping it.
    Exited,
    /// The kernel reported the leader still running (WNOHANG, no event).
    StillRunning,
}

/// Non-reaping exit observation (r19 item 3; r20 item 1 / R8(c)):
/// `waitid(P_PID, pid, WEXITED | WNOHANG | WNOWAIT)` reports the leader's exit
/// WITHOUT consuming the zombie.
///
/// The syscall lives in the `remuda-unix` crate because nix 0.28 gates
/// `waitid`/`Id` off Apple targets (they do not link on macOS), while the libc
/// call is supported on both Linux and macOS.
///
/// Result discipline (R8(c)): `StillRunning` means the kernel said the leader
/// is alive; `Exited` means waitid reported THIS pid's exit. Any other error
/// (`EINVAL` from bad flags, `ECHILD` because it is not ours, …) is `None`:
/// NOT an exit observation. The owner keeps waiting and may NEVER treat an
/// observation error as an exit (the old code mapped EINVAL to "exited": on
/// Linux an event selector is REQUIRED, so WNOHANG|WNOWAIT alone returned
/// EINVAL and a live leader whose stdout had closed got TERM/KILLed).
#[cfg(unix)]
fn leader_exit_observed_unreaped(pid: i32) -> Option<LeaderExitObservation> {
    match remuda_unix::observe_exit_without_reaping(pid as u32) {
        Ok(remuda_unix::ObservedChildExit::Exited) => Some(LeaderExitObservation::Exited),
        Ok(remuda_unix::ObservedChildExit::StillRunning) => {
            Some(LeaderExitObservation::StillRunning)
        }
        Err(error) => {
            tracing::debug!(%error, pid, "non-reaping leader exit observation failed");
            None
        }
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
                        let Ok(mut step) = serde_json::from_str::<remuda_protocol::GateStep>(line)
                        else {
                            continue;
                        };
                        // r23 item 2: bound every variable-length GateStep
                        // field at the source — the Hub job row persists these
                        // alongside the bounded evidence object.
                        step.name = clip_field(step.name, MAX_STEP_NAME_BYTES);
                        step.error = step
                            .error
                            .map(|text| clip_field(text, MAX_STEP_FIELD_BYTES));
                        step.reason = step
                            .reason
                            .map(|text| clip_field(text, MAX_STEP_FIELD_BYTES));
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

/// Read-only landing resolution. Reads the authoritative remote base tip
/// WITHOUT WRITING ANY REFS and reports:
/// * `landed` when the granted merge is already an ancestor of the immutable
///   remote base SHA;
/// * `absent` when the remote tip was freshly read and fetched, the merge is
///   NOT an ancestor, and `land_group_verified_dead` is true — the caller holds
///   AUTHORITATIVE death evidence for the exact attempt's land group (a live
///   kernel ESRCH or a durable death certificate; r14 R1, r16 item 2 R5);
/// * `unknown` for every read/fetch/parse/ancestry failure, non-ancestry
///   without authoritative death evidence, and crucially a MISSING OR
///   UNREADABLE ledger (missing proof is not proof of death). It never pushes
///   and never asserts a push it cannot prove.
///
/// R10 (r30): recovery is REF-FREE and therefore immune to ref substitution,
/// symbolic-ref deletion, base-rename prefix collisions, clock-based
/// reclamation and leftover-ref accumulation:
/// 1. `git ls-remote --exit-code origin refs/heads/<base>` reads the exact
///    advertised tip — configured remote.origin.fetch refspecs cannot make it
///    stale (the original R5 fail-open);
/// 2. `git fetch --no-tags --no-write-fetch-head origin <sha>` fetches that
///    commit's objects with NO destination refspec, so it creates no ref and
///    never reads or writes FETCH_HEAD; `--no-tags` also defeats a configured
///    tagOpt=--tags tag clobber;
/// 3. `git cat-file -e <sha>^{commit}` verifies the object before ancestry is
///    checked against the SHA.
///
/// Every git command runs through the R6 owner under one absolute deadline;
/// any failure is Unknown, never Absent.
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
    // R7/R6, r20 R8(b): ONE absolute deadline bounds EVERY recovery
    // subprocess AND its whole cleanup (survivor kill, reap, pipe drains); the
    // per-command soft cap only decides when a still-running git is KILLed.
    let deadline = std::time::Instant::now() + LANDING_PROBE_DEADLINE;
    // R11: one hardening guard for the whole probe (env + config + fetch flags;
    // its scratch hooks dir is removed when the probe returns).
    let hardening = remuda_unix::RecoveryGit::new();
    // Read the remote tip and fetch its objects without touching any ref. On
    // error the probe is Unknown.
    let base_sha = match recovery_fetch_base_sha(&repo, &request.base_branch, deadline, &hardening)
    {
        Ok(sha) => sha,
        Err(message) => return unknown(message),
    };
    let run_owned = |args: &[&str],
                     what: &str,
                     require_success: bool|
     -> Result<std::process::Output, GateResolveLandingResult> {
        if remaining(deadline).is_zero() {
            return Err(unknown(format!(
                "landing probe deadline ({LANDING_PROBE_DEADLINE:?}) exhausted before {what} \
                     in {repo:?}; ancestry unknown"
            )));
        }
        match owned_git(&repo, args, RECOVERY_GIT_CAP, deadline, &hardening) {
            Ok(run) if run.timed_out() => Err(unknown(format!(
                "{what} exceeded the {RECOVERY_GIT_CAP:?} probe bound and was killed; push outcome \
                 unknown"
            ))),
            Ok(run) => {
                let output = run.into_output();
                if require_success && !output.status.success() {
                    return Err(unknown(format!(
                        "{what} exited {:?}; push outcome unknown",
                        output.status.code()
                    )));
                }
                Ok(output)
            }
            Err(error) => Err(unknown(format!("{what} could not run: {error}"))),
        }
    };

    // R7: shallow history is never deepened; absence is undecidable → Unknown.
    let shallow_output = match run_owned(
        &["rev-parse", "--is-shallow-repository"],
        "shallow check",
        true,
    ) {
        Ok(o) => o,
        Err(result) => return result,
    };
    if String::from_utf8_lossy(&shallow_output.stdout).trim() == "true" {
        return unknown(format!(
            "{repo:?} is a shallow clone; gate recovery does not deepen, so ancestry of {} \
             cannot be decided locally after one bounded fetch; push outcome unknown (perform a \
             full clone to enable remote reconciliation)",
            request.merge_sha
        ));
    }
    // merge-base --is-ancestor: exit 0 landed, exit 1 not-ancestor (valid only
    // with complete history, checked above), anything else Unknown. The
    // descendant is the immutable remote SHA read by ls-remote.
    let ancestry_output = match run_owned(
        &["merge-base", "--is-ancestor", &request.merge_sha, &base_sha],
        "ancestry check",
        false,
    ) {
        Ok(o) => o,
        Err(result) => return result,
    };
    let base_preview = &base_sha[..base_sha.len().min(12)];
    match ancestry_output.status.code() {
        Some(0) => GateResolveLandingResult {
            job_id: request.job_id.clone(),
            status: "landed".into(),
            base_sha: Some(base_sha),
            error: None,
        },
        Some(1) if land_group_verified_dead => GateResolveLandingResult {
            job_id: request.job_id.clone(),
            status: "absent".into(),
            base_sha: Some(base_sha),
            error: None,
        },
        Some(1) => unknown(format!(
            "{} is not an ancestor of {base_preview} (the freshly read remote base) but the \
             granted land group's death is not authoritatively established (no live ESRCH or death \
             certificate; a missing ledger is not proof); push outcome unknown",
            request.merge_sha
        )),
        other => unknown(format!(
            "merge-base --is-ancestor ended abnormally ({other:?}); ancestry of {} unknown",
            request.merge_sha
        )),
    }
}

/// An object ID in the repository's native format: SHA-1 (40 hex) or SHA-256
/// (64 hex). Git itself emitted the string from ls-remote; this only bounds
/// the accepted shapes.
fn is_native_object_hex(sha: &str) -> bool {
    // r31 item 5: object IDs git advertises are lowercase; uppercase A-F must
    // be rejected as uncertain (and never compared against a lowercase SHA).
    (sha.len() == 40 || sha.len() == 64)
        && sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Parse `git ls-remote --exit-code origin refs/heads/<base>` output. Exactly
/// one line naming the requested ref with a native-format object ID; an
/// absent base (no lines, exit 2), ambiguous output or a malformed line is an
/// error — never a guessed tip.
fn parse_ls_remote_base(output: &[u8], base_branch: &str) -> std::result::Result<String, String> {
    let expected_ref = format!("refs/heads/{base_branch}");
    let mut matches = Vec::new();
    for line in String::from_utf8_lossy(output).lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }
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

/// R10 ref-free remote-tip acquisition. All three commands go through the R6
/// owner under the caller's absolute deadline; every failure is an Unknown
/// message, never a guessed absence. No local ref is created, updated or
/// deleted at any point.
#[cfg(unix)]
fn recovery_fetch_base_sha(
    repo: &Path,
    base_branch: &str,
    deadline: std::time::Instant,
    hardening: &remuda_unix::RecoveryGit,
) -> std::result::Result<String, String> {
    let owned = |args: &[&str],
                 what: &str,
                 require_success: bool|
     -> std::result::Result<std::process::Output, String> {
        if remaining(deadline).is_zero() {
            return Err(format!(
                "landing probe deadline ({LANDING_PROBE_DEADLINE:?}) exhausted before {what}; push \
                 outcome unknown"
            ));
        }
        match owned_git(repo, args, RECOVERY_GIT_CAP, deadline, hardening) {
            Ok(run) if run.timed_out() => Err(format!(
                "{what} exceeded the {RECOVERY_GIT_CAP:?} probe bound and was killed; push outcome \
                 unknown"
            )),
            Ok(run) => {
                let output = run.into_output();
                if require_success && !output.status.success() {
                    return Err(format!(
                        "{what} exited {:?}: {}; push outcome unknown",
                        output.status.code(),
                        String::from_utf8_lossy(&output.stderr).trim()
                    ));
                }
                Ok(output)
            }
            Err(error) => Err(format!("{what} could not run: {error}")),
        }
    };
    // 1. The authoritative advertised tip. ls-remote consults the remote
    // directly; a stale configured tracking ref cannot be read here. git exits
    // 2 when no advertised ref matches — let the parser report that precisely.
    // Every other non-zero exit (a wrapper failure, a transport error) is an
    // ordinary command failure; timeouts and spawn failures fail too.
    let remote_ref = format!("refs/heads/{base_branch}");
    let listing = owned(
        &["ls-remote", "--exit-code", "origin", &remote_ref],
        "remote base ls-remote",
        false,
    )?;
    if listing.status.code() != Some(2) && !listing.status.success() {
        return Err(format!(
            "remote base ls-remote exited {:?}: {}; push outcome unknown",
            listing.status.code(),
            String::from_utf8_lossy(&listing.stderr).trim()
        ));
    }
    let sha = parse_ls_remote_base(&listing.stdout, base_branch)?;
    // r30 item 1 TEST-ONLY seam: park after the remote SHA was read and
    // before its objects are fetched, so an overlap regression can move the
    // remote tip deterministically while a second recovery is in flight.
    #[cfg(test)]
    recovery_read_park(repo);
    // 2. Fetch the objects of THAT commit with an explicit object-ID refspec
    //    and no destination. An empty --refmap makes git ignore ALL configured
    //    remote.origin.fetch mappings — a destination-less command-line refspec
    //    alone does NOT disable them (a configured `+<sha>:refs/tags/x` would
    //    otherwise force-write x); --no-prune neutralises fetch.prune /
    //    remote.origin.prune so no stale remote-tracking ref is deleted.
    //    --no-tags defeats a configured tagOpt=--tags clobber. A tip that moved
    //    so the SHA is no longer fetchable fails here → Unknown.
    //
    //    No --no-write-fetch-head: older git (observed on the build hosts)
    //    rejects the option and would make every recovery Unknown forever.
    //    FETCH_HEAD is not a ref, recovery never reads it, and writing it
    //    cannot create or move any branch/tag; it is harmless.
    let mut fetch_args: Vec<&str> = vec!["fetch"];
    fetch_args.extend_from_slice(remuda_unix::RECOVERY_FETCH_FLAGS);
    fetch_args.push("origin");
    fetch_args.push(&sha);
    owned(&fetch_args, "remote base object fetch", true)?;
    // 3. The fetched object must exist locally and peel to a commit.
    owned(
        &["cat-file", "-e", &format!("{sha}^{{commit}}")],
        "fetched base commit verification",
        true,
    )?;
    Ok(sha)
}

/// r30 item 1 TEST-ONLY seam: parks a recovery after its `ls-remote` read and
/// before the object fetch. Keyed by repository path so other fixtures pass.
#[cfg(test)]
mod recovery_read_seam {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};

    pub(super) struct Slot {
        pub(super) arrived: AtomicBool,
        pub(super) released: AtomicBool,
    }

    pub(super) struct Park {
        pub(super) slots: Mutex<Vec<Arc<Slot>>>,
        pub(super) expected: usize,
        parked: AtomicUsize,
    }

    // r31 item 4: parks are keyed by repository path so tests arming the seam
    // on DIFFERENT fixture repos can run in parallel without clobbering each
    // other.
    static SEAM: OnceLock<Mutex<HashMap<PathBuf, Arc<Park>>>> = OnceLock::new();

    fn seam() -> &'static Mutex<HashMap<PathBuf, Arc<Park>>> {
        SEAM.get_or_init(|| Mutex::new(HashMap::new()))
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
        seam().lock().expect("seam").insert(
            repo.to_path_buf(),
            Arc::new(Park {
                slots: Mutex::new(slots),
                expected,
                parked: AtomicUsize::new(0),
            }),
        );
    }

    pub(super) fn disarm(repo: &Path) {
        seam().lock().expect("seam").remove(repo);
    }

    pub(super) fn park(repo: &Path) {
        let park = match seam().lock().expect("seam").get(repo) {
            Some(park) => park.clone(),
            None => return,
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

    fn slot(repo: &Path, index: usize) -> Arc<Slot> {
        let guard = seam().lock().expect("seam");
        let park = guard.get(repo).expect("seam armed").clone();
        drop(guard);
        park.slots.lock().expect("slots")[index].clone()
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

/// R10/r30 item 2: true ONLY for the exact diagnostics git emits when the
/// installed build cannot create a SHA-256 repository. Matching is anchored
/// to the message forms themselves (with surrounding whitespace/punctuation),
/// never to substrings that could appear in a PATH — so a bad-config error
/// whose filename happens to contain "sha256" or "invalid" is not a skip.
#[cfg(test)]
pub(crate) fn is_unsupported_sha256_diagnostic(stderr: &str) -> bool {
    unsupported_sha256_diagnostic(stderr)
}

/// r31 item 3 / r32 item 4: true ONLY when git prints one of its exact
/// unsupported-SHA-256 diagnostic LINES (full line, exact operand).
#[cfg(test)]
fn unsupported_sha256_diagnostic_impl(stderr: &str) -> bool {
    // r32 item 4: COMPLETE, case-sensitive diagnostics with the exact expected
    // operand. Each accepted form is the full line git prints; no
    // starts_with/contains, so a wrong operand (`bogus`, `sha1`,
    // `sha256extra`) or trailing text never reads as unsupported-SHA-256.
    const EXACT_LINES: &[&str] = &[
        // Current git: the hash algorithm is unknown.
        "fatal: unknown hash algorithm 'sha256'",
        // Current git: the backend is not compiled in.
        "fatal: attempt to use sha256 has not been compiled in",
        "fatal: sha256 repositories are not supported",
        // Older git: the option itself is unknown (pre --object-format builds).
        "error: unknown option `object-format'",
        "error: unknown option '--object-format'",
        "error: unknown option '--object-format=sha256'",
        "fatal: unknown option `--object-format'",
        "fatal: unknown option '--object-format'",
        "fatal: unknown option '--object-format=sha256'",
        "error: unknown option for '--object-format'",
        "fatal: unknown option for '--object-format'",
        // Older git: the value is rejected. The ONLY unsupported operand is
        // sha256 (sha1 is accepted; bogus names an algorithm, not a build that
        // cannot do sha256).
        "fatal: unknown value for --object-format: sha256",
        "error: unknown value for --object-format: sha256",
    ];
    stderr
        .lines()
        .map(str::trim_end)
        .any(|line| EXACT_LINES.contains(&line))
}

#[cfg(test)]
fn unsupported_sha256_diagnostic(stderr: &str) -> bool {
    unsupported_sha256_diagnostic_impl(stderr)
}
/// Run `git init --object-format=sha256` through `program` and classify the
/// outcome: true when the repository is created; false ONLY on a recognized
/// unsupported-format diagnostic; a spawn failure or any other init error
/// panics with the underlying detail.
#[cfg(unix)]
#[cfg(test)]
pub(crate) fn probe_sha256_support_with(program: &str, probe_repo: &Path) -> bool {
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

/// Default SHA-256 support probe using the PATH `git`.
#[cfg(unix)]
#[cfg(test)]
pub(crate) fn probe_sha256_support(program: &str, probe_repo: &Path) -> bool {
    probe_sha256_support_with(program, probe_repo)
}

/// Per-subprocess cap so one git command cannot consume the whole probe.
#[cfg(unix)]
const RECOVERY_GIT_CAP: Duration = Duration::from_secs(10);
/// ABSOLUTE deadline for a whole read-only landing probe: covers the fetch,
/// ref read, shallow check and ancestry read, and stays below the Hub's 30 s
/// `gate.resolveLanding` RPC timeout (R7/R6 — one deadline for the probe).
#[cfg(unix)]
const LANDING_PROBE_DEADLINE: Duration = Duration::from_secs(25);

/// r38 item 4 TEST-ONLY: watchdog for the owner-release barrier. It must
/// outlast the complete interval in which the regression parks the owner —
/// the 25 s read-only probe deadline plus the test's forced reconcile/poll
/// window — so a slow probe can never auto-release the owner and break the
/// still-registered assertion. The barrier is normally released explicitly.
#[cfg(any(test, feature = "test-faults"))]
const OWNER_RELEASE_HOLD_WATCHDOG: Duration = Duration::from_secs(120);

/// Run a git command through THE OWNER CONTRACT helper in `remuda-unix`
/// (own group, non-reaping exit observation, survivor cleanup while the leader
/// is unreaped, one latched absolute deadline over the reap and both drains).
/// The caller treats `TimedOut`/Err as Unknown.
#[cfg(unix)]
fn owned_git(
    repo: &Path,
    args: &[&str],
    cap: Duration,
    deadline: std::time::Instant,
    hardening: &remuda_unix::RecoveryGit,
) -> std::io::Result<remuda_unix::OwnedRun> {
    // R11: every recovery git invocation goes through the single hardened
    // builder (replacement/graft/prompt env, no-maintenance/no-submodule/
    // no-hooks config); no recovery call can bypass it.
    let command = hardening.command(repo, args);
    remuda_unix::run_owned_process(command, cap, deadline)
}

/// Remaining time until an absolute deadline (saturates at zero). Used by the
/// unix-only recovery owner AND the platform-independent land owner, so it is
/// not itself cfg-gated.
fn remaining(deadline: std::time::Instant) -> Duration {
    deadline
        .checked_duration_since(std::time::Instant::now())
        .unwrap_or(Duration::ZERO)
}

fn git_output(repo: &Path, args: &[&str]) -> Result<String, String> {
    block_git(repo, args)
}

// ── granted-land process-group fence (r14 R1) ──────────────────────────────
//

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
///
/// r35 item 1/2: durable evidence is bound to the FULL attempt identity the
/// Hub probed — generation AND the exact granted merge sha — so neither an
/// attempt-greeting pin nor a merge-only match can attest death.
#[cfg(unix)]
fn land_ledger_liveness(
    dir: &std::path::Path,
    job_id: &str,
    generation: Option<u32>,
    merge_sha: Option<&str>,
) -> LandPushLiveness {
    // Durable evidence is read ONLY for the exact requested attempt. Without a
    // requested generation WITH its authoritative granted merge sha, or when
    // no attempt-qualified file exists for it, the answer is Unknown (never
    // another attempt's cert, never generic evidence).
    let (Some(generation), Some(merge_sha)) = (generation, merge_sha) else {
        return LandPushLiveness::Unknown;
    };
    if merge_sha.is_empty() {
        return LandPushLiveness::Unknown;
    }
    match std::fs::metadata(land_fence_path(dir, job_id, generation)) {
        Ok(_) => LandPushLiveness::MayPush,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match read_land_group_dead_cert(dir, job_id, generation, merge_sha) {
                Some(_) => LandPushLiveness::VerifiedDead,
                None => LandPushLiveness::Unknown,
            }
        }
        // A stat/read error on the ledger directory is Unknown.
        Err(_) => LandPushLiveness::Unknown,
    }
}

#[cfg(unix)]
fn land_fence_path(dir: &std::path::Path, job_id: &str, generation: u32) -> std::path::PathBuf {
    // r34 item 1: attempt-qualified, IMMUTABLE fence path. Each granted attempt
    // (a generation) gets its own file; a finalizer only ever unlinks its own
    // attempt's fence, so a detached older finalizer cannot remove a newer
    // attempt's live fence.
    dir.join(format!("land-{}-g{}.json", land_slug(job_id), generation))
}

/// Durable DEATH CERTIFICATE path: written only after the kernel verified the
/// granted land group ESRCH, and the only durable evidence that lets a later
/// read-only probe attest the push cannot still land (r16 item 2, R5).
/// r34 item 1: attempt-qualified like the fence — one certificate per
/// generation, so an older attempt's certificate is never read as death
/// evidence for a newer attempt.
#[cfg(unix)]
fn land_dead_cert_path(dir: &std::path::Path, job_id: &str, generation: u32) -> std::path::PathBuf {
    dir.join(format!("dead-{}-g{}.json", land_slug(job_id), generation))
}

/// Legacy (pre-r34) live-fence path: `land-<slug>.json`, qualified by
/// contents only. Kept solely so startup migration can LOCATE such files;
/// nothing else ever opens this name.
#[cfg(unix)]
fn legacy_land_fence_path(dir: &std::path::Path, job_id: &str) -> std::path::PathBuf {
    dir.join(format!("land-{}.json", land_slug(job_id)))
}

/// Legacy (pre-r34) death-certificate path: `dead-<slug>.json`. Read ONLY by
/// the explicit, generation-pinned legacy migration in
/// [`read_land_group_dead_cert`], never as generic death evidence.
#[cfg(unix)]
fn legacy_land_dead_cert_path(dir: &std::path::Path, job_id: &str) -> std::path::PathBuf {
    dir.join(format!("dead-{}.json", land_slug(job_id)))
}

/// Provenance stamped on certificates migrated from legacy (pre-r34) evidence.
/// A migrated certificate never carries a merge sha (the legacy record had
/// none); the missing merge is bound ONLY through the Hub's authoritative
/// generation-pinned probe — never treated as generic death evidence.
#[cfg(unix)]
const LEGACY_FENCE_CERT_PROVENANCE: &str = "legacy-fence";
#[cfg(unix)]
const LEGACY_DEAD_CERT_PROVENANCE: &str = "legacy-cert";

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
    if record.get("pgid").and_then(Value::as_i64) != Some(i64::from(pgid)) {
        return Err(std::io::Error::other(
            "land fence record pgid does not match the observed dead group",
        ));
    }
    let mut cert = record.clone();
    cert["diedAt"] = json!(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    );
    cert["cause"] = json!("killpg-esrch");
    let generation = record
        .get("generation")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| {
            std::io::Error::other("land fence record has no generation for the death certificate")
        })?;
    let job_id = record.get("jobId").and_then(Value::as_str).ok_or_else(|| {
        std::io::Error::other("land fence record has no jobId for the death certificate")
    })?;
    write_qualified_dead_cert_bytes(dir, &cert, job_id, generation)
}

/// Per-process sequence used so every certificate write gets a distinct temp
/// file even within the same nanosecond (r36 item 1).
#[cfg(unix)]
static CERT_TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One unique, single-use temporary-file name for a certificate write.
#[cfg(unix)]
fn cert_temp_name(dir: &std::path::Path, job_id: &str, generation: u32) -> std::path::PathBuf {
    let seq = CERT_TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    dir.join(format!(
        ".dead-{}-g{}.{}-{:09x}-{:x}.tmp",
        land_slug(job_id),
        generation,
        std::process::id(),
        nanos,
        seq
    ))
}

/// r38 item 1: ledger-scoped advisory lock guarding certificate writes against
/// startup reclamation. Every live/detached certificate writer holds a SHARED
/// flock on this file for its whole create→sync→rename; startup reconciliation
/// takes an EXCLUSIVE non-blocking flock and skips reclamation entirely if a
/// writer (possibly a detached finalizer from a previous process) still holds a
/// shared lock. flock conflicts across distinct open file descriptions even in
/// the same process, which is what makes the in-process regression faithful.
#[cfg(unix)]
fn ledger_lock_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join(".gate-ledger.lock")
}

/// Open the ledger advisory-lock file WITHOUT creating the ledger directory
/// (startup path): a Node that has never written a fence owns no ledger, so
/// there is nothing to reclaim/scan and we must not leave state behind under a
/// caller's workspace root.
#[cfg(unix)]
fn open_ledger_lock_file_existing(dir: &std::path::Path) -> Option<std::fs::File> {
    if !dir.is_dir() {
        return None;
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(false)
        .truncate(false)
        .open(ledger_lock_path(dir))
        .ok()
}

/// Open (creating dir + file if absent) the ledger advisory-lock file. Used by
/// runtime certificate writers, which are the first to actually touch the
/// ledger.
#[cfg(unix)]
fn open_ledger_lock_file(dir: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::create_dir_all(dir)?;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(ledger_lock_path(dir))
}

/// Atomically write an already-assembled certificate JSON to the qualified
/// `dead-<job>-g<gen>.json` path, holding the ledger SHARED flock for the whole
/// create→sync→rename (r38 item 1) so startup reclamation can never unlink an
/// in-flight writer's temp. Each invocation also creates its OWN exclusive
/// temp file (O_EXCL/`create_new`, name includes job + generation + a unique
/// nonce) and renames only that file (r36 item 1). A failed
/// create/write/rename removes only this invocation's temp — never the
/// destination or the live fence — and the caller's `?` leaves the fence in
/// place.
#[cfg(unix)]
fn write_qualified_dead_cert_bytes(
    dir: &std::path::Path,
    cert: &Value,
    job_id: &str,
    generation: u32,
) -> std::io::Result<()> {
    use nix::fcntl::{Flock, FlockArg};
    let lock_file = open_ledger_lock_file(dir)?;
    // Blocking SHARED lock: startup is the only EXCLUSIVE holder and skips
    // reclamation rather than blocking, so a writer waits only in the rare
    // case startup is actively scanning (which never writes a writer's temp).
    // The guard is held to the end of the function, covering create→rename.
    let _ledger_guard = Flock::lock(lock_file, FlockArg::LockShared)
        .map_err(|(_file, error)| std::io::Error::other(format!("ledger shared flock: {error}")))?;
    write_qualified_dead_cert_bytes_locked(dir, cert, job_id, generation)
}

/// Certificate write body that assumes the caller already holds the ledger
/// SHARED flock. Only two callers qualify: the public writer above, and the
/// startup legacy-fence migration, which runs inside the startup scan's SHARED
/// guard. Every other writer, including the runtime legacy-cert migration
/// (r39 item 1), must go through [`write_qualified_dead_cert_bytes`].
#[cfg(unix)]
fn write_qualified_dead_cert_bytes_locked(
    dir: &std::path::Path,
    cert: &Value,
    job_id: &str,
    generation: u32,
) -> std::io::Result<()> {
    use std::io::Write;
    let path = land_dead_cert_path(dir, job_id, generation);
    let bytes = serde_json::to_vec(&cert).unwrap_or_default();
    // Residual name collisions (only possible across processes) retry.
    for _ in 0..8 {
        let temporary = cert_temp_name(dir, job_id, generation);
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let written = file.write_all(&bytes).and_then(|_| file.sync_all());
        if let Err(error) = written {
            drop(file);
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
        drop(file);
        // r36 item 1 TEST-ONLY: park with the temp fully written but before the
        // rename, so a regression can run a second writer to the exact
        // interleave where a shared temp name would swap certificate contents.
        #[cfg(test)]
        park_cert_temp_before_rename(dir, job_id);
        if let Err(error) = std::fs::rename(&temporary, &path) {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
        return Ok(());
    }
    Err(std::io::Error::other(
        "could not allocate a unique certificate temp file",
    ))
}

/// A certificate temp file (one that never completed its rename into a
/// qualified `dead-<job>-g<gen>.json`). Both the current unique-name scheme
/// (`.dead-<slug>-g<gen>.<pid>-<nanos>-<seq>.tmp`) and the older per-pid/pgid
/// scheme are matched: a hidden `.dead-` prefix and a `.tmp` suffix.
#[cfg(unix)]
fn is_abandoned_cert_temp(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    name.starts_with(".dead-") && name.ends_with(".tmp")
}

/// r37 item 4 / r38 item 1: startup ledger reconciliation.
///
/// Reclaims crash-abandoned certificate temps and scans orphaned fences ONLY
/// while holding the ledger EXCLUSIVE advisory flock. The lock is acquired
/// non-blockingly: if any certificate writer still holds a SHARED lock — a
/// detached finalizer that survived its deadline, including one outliving a
/// Node shutdown/reopen of the same data dir — this returns without touching
/// the ledger (the wait is retried on the next start). That is what prevents
/// startup from unlinking a temp a still-running writer is about to rename.
/// Runtime certificate writers take SHARED locks and never conflict with each
/// other.
#[cfg(unix)]
fn reconcile_ledger_at_startup(dir: &std::path::Path) {
    use nix::fcntl::{Flock, FlockArg};
    // No-create open first: a fresh Node whose ledger DIRECTORY was never
    // created has nothing to reconcile and must not leave state under the
    // caller's workspace root. If the dir exists (this Node or a prior build
    // wrote fences there) the lock file may just be older than the locking
    // change, so create it then.
    let lock_file = match open_ledger_lock_file_existing(dir) {
        Some(file) => file,
        None if dir.is_dir() => match open_ledger_lock_file(dir) {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!(?dir, %error, "could not create the land ledger lock; skipping startup fence reconciliation");
                return;
            }
        },
        None => return,
    };
    let exclusive = Flock::lock(lock_file, FlockArg::LockExclusiveNonblock);
    match exclusive {
        Ok(exclusive_guard) => {
            // EXCLUSIVE for reclamation: this is the only step that unlinks
            // writer temps, so no writer may be active.
            reclaim_abandoned_cert_temps(dir);
            // The orphan-fence scan certifies dead fences and itself invokes
            // certificate writers (each a SHARED flock on its OWN open fd).
            // Holding EXCLUSIVE here across those would self-deadlock (flock is
            // per open-file-description, and a process can't hold EX on one fd
            // and SH on another of the same file). Temp reclamation is already
            // finished, so release EXCLUSIVE and hold a SHARED guard for the
            // scan: it composes with the nested writer SHARED locks and still
            // excludes a second EXCLUSIVE startup from interleaving.
            drop(exclusive_guard);
            // Reopen the ledger lock for the SHARED scan phase.
            let Some(scan_file) = open_ledger_lock_file_existing(dir) else {
                return;
            };
            match Flock::lock(scan_file, FlockArg::LockShared) {
                Ok(scan_guard) => {
                    fence_orphaned_land_groups(dir);
                    drop(scan_guard);
                }
                Err((_file, error)) => {
                    tracing::warn!(?dir, %error, "could not take the shared ledger lock for the orphan-fence scan");
                }
            }
        }
        Err((_file, nix::errno::Errno::EWOULDBLOCK)) => {
            tracing::warn!(
                ?dir,
                "land ledger is locked by a still-active certificate writer; \
                 skipping startup fence reclamation (retried on the next start)"
            );
        }
        Err((_file, error)) => {
            tracing::warn!(?dir, %error, "land ledger exclusive flock failed; skipping startup fence reconciliation");
        }
    }
}

/// Remove crash-abandoned certificate temporary files from the ledger
/// directory. Caller MUST hold the ledger EXCLUSIVE flock
/// ([`reconcile_ledger_at_startup`]); the lock is what proves no writer still
/// owns a temp. The files are never read as evidence (readers open only
/// qualified names); this bounds their cross-restart accumulation.
#[cfg(unix)]
fn reclaim_abandoned_cert_temps(dir: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0usize;
    for entry in entries.flatten() {
        if !is_abandoned_cert_temp(&entry.file_name()) {
            continue;
        }
        // Best-effort: a file we cannot remove (permissions, vanished) is left
        // for the next boot; it is still ignored by every evidence read.
        if std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Read and VALIDATE a death certificate for the exact requested attempt.
/// `None` covers a missing file, any read/parse error, a job/generation
/// mismatch, missing provenance (pgid + leader identity), or a merge mismatch —
/// every one of those is Unknown, never a valid death attestation.
///
/// r35 item 2: new-style certificates carry the granted `mergeSha` and it must
/// equal the Hub-probed merge EXACTLY. Certificates written before the merge
/// binding (pre-r34 migrations stamped `provenance: "legacy-…"`; r34-era
/// qualified certs are simply merge-less) carry no merge sha; such a cert is
/// accepted ONLY under a generation-pinned probe that supplies a NON-EMPTY merge
/// sha — the Hub's own grant record binds the missing merge. Without that
/// authority (empty/absent merge) it is refused, so an unqualified certificate
/// is never accepted as generic death evidence.
#[cfg(unix)]
fn read_land_group_dead_cert(
    dir: &std::path::Path,
    job_id: &str,
    generation: u32,
    merge_sha: &str,
) -> Option<Value> {
    let qualified = land_dead_cert_path(dir, job_id, generation);
    match std::fs::read_to_string(&qualified) {
        Ok(text) => validate_dead_cert(text, job_id, generation, merge_sha),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // No qualified certificate: perform the ONE explicit legacy
            // migration (legacy `dead-<job>.json`) under this exact
            // generation-pinned, merge-bearing request.
            migrate_legacy_dead_cert(dir, job_id, generation, merge_sha)
        }
        Err(_) => None,
    }
}

/// Validate one parsed certificate's identity against the requested attempt
/// (see [`read_land_group_dead_cert`] for the merge-binding rules).
#[cfg(unix)]
fn validate_dead_cert(
    text: String,
    job_id: &str,
    generation: u32,
    merge_sha: &str,
) -> Option<Value> {
    let cert: Value = serde_json::from_str(&text).ok()?;
    // The certificate must attest the EXACT requested attempt: job AND
    // generation, plus the required provenance (pgid + leader identity).
    if cert.get("jobId").and_then(Value::as_str) != Some(job_id)
        || cert.get("generation").and_then(Value::as_u64) != Some(u64::from(generation))
    {
        return None;
    }
    cert.get("pgid")
        .and_then(Value::as_i64)
        .filter(|pgid| i32::try_from(*pgid).is_ok())?;
    LeaderIdentity::from_record(&cert)?;
    match cert.get("mergeSha").and_then(Value::as_str) {
        // New-style evidence: the attested merge must match EXACTLY.
        Some(cert_merge) if cert_merge == merge_sha => Some(cert),
        Some(_) => None,
        // Merge-less evidence (r34-era qualified certs or explicitly migrated
        // pre-r34 records): the Hub's generation-pinned probe supplies the
        // merge authority. Empty authority (and no merge at all) is refused —
        // never generic death evidence.
        None if !merge_sha.is_empty() => Some(cert),
        None => None,
    }
}

/// Migrate a verified-dead LEGACY live fence (`land-<job>.json`) into the
/// attempt-qualified certificate (`dead-<job>-g<gen>.json`) observed at
/// startup. Called only after the kernel established the WHOLE group ESRCH and
/// the record passed the same validation as a current fence (exact job,
/// generation, pgid, leader identity). The migrated certificate gets the
/// `legacy-fence` provenance marker and NO merge sha: the merge the old Node
/// pushed is not recoverable from the lane record, and it must never be
/// guessed. The certificate therefore attests death ONLY under the Hub's later
/// generation-pinned probe carrying the authoritative granted merge sha
/// ([`read_land_group_dead_cert`]). When a NEWER qualified certificate (or a
/// qualified fence that is not the observed r34 source itself) already exists
/// the migration is skipped (the newer evidence wins; the legacy file is left
/// for the documented operator route).
#[cfg(unix)]
fn migrate_legacy_fence_to_cert(
    dir: &std::path::Path,
    source_path: &std::path::Path,
    record: &Value,
    job_id: &str,
    generation: u32,
) {
    if land_dead_cert_path(dir, job_id, generation).exists()
        || (land_fence_path(dir, job_id, generation).exists()
            && land_fence_path(dir, job_id, generation) != source_path)
    {
        tracing::warn!(
            job_id,
            generation,
            "a qualified fence/certificate already exists for this attempt; leaving the legacy \
             fence unmigrated (operator resolution via the Hub /resolve route)"
        );
        return;
    }
    let mut cert = record.clone();
    cert["diedAt"] = json!(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    );
    cert["cause"] = json!("killpg-esrch");
    cert["provenance"] = json!(LEGACY_FENCE_CERT_PROVENANCE);
    // Defensive: never carry a merge into a legacy migration even if a stray
    // field existed (the format checks above required its absence).
    if let Some(object) = cert.as_object_mut() {
        object.remove("mergeSha");
    }
    match write_qualified_dead_cert_bytes_locked(dir, &cert, job_id, generation) {
        Ok(()) => {
            if let Err(error) = std::fs::remove_file(source_path) {
                tracing::warn!(
                    %error,
                    job_id,
                    generation,
                    "legacy fence migrated but its source file could not be removed; it will be \
                     skipped on the next boot"
                );
            }
        }
        Err(error) => tracing::warn!(
            %error,
            job_id,
            generation,
            "migrating the legacy land fence to a certificate failed; keeping it"
        ),
    }
}

/// Read, validate and migrate a legacy `dead-<job>.json` certificate to its
/// attempt-qualified name, serving it ONLY for the exact generation-pinned,
/// merge-bearing request. The legacy record must already carry the attempt
/// generation (pre-r34 fences/certs did); a legacy record that cannot be bound
/// to an exact attempt is left untouched (Unknown; the operator resolution
/// route documented at [`fence_orphaned_land_groups`] applies).
#[cfg(unix)]
fn migrate_legacy_dead_cert(
    dir: &std::path::Path,
    job_id: &str,
    generation: u32,
    merge_sha: &str,
) -> Option<Value> {
    if merge_sha.is_empty() {
        return None;
    }
    let legacy_path = legacy_land_dead_cert_path(dir, job_id);
    let text = std::fs::read_to_string(&legacy_path).ok()?;
    // Never migrate over a qualified file that appeared concurrently.
    if land_fence_path(dir, job_id, generation).exists()
        || land_dead_cert_path(dir, job_id, generation).exists()
    {
        return None;
    }
    let cert: Value = serde_json::from_str(&text).ok()?;
    // Defense in depth: a legacy LIVE fence for the same job means a possibly
    // in-flight group was never finalized — the certificate can never settle
    // it (its pgid may have been reused by that live replacement). Leave both
    // for startup/operator handling.
    if legacy_land_fence_path(dir, job_id).exists() {
        return None;
    }
    // Same validation as a qualified certificate, but the merge binding comes
    // from the request authority and the record must prove its legacy origin by
    // the legacy file + death fields.
    if cert.get("jobId").and_then(Value::as_str) != Some(job_id)
        || cert.get("generation").and_then(Value::as_u64) != Some(u64::from(generation))
    {
        return None;
    }
    cert.get("pgid")
        .and_then(Value::as_i64)
        .filter(|pgid| i32::try_from(*pgid).is_ok())?;
    LeaderIdentity::from_record(&cert)?;
    if cert.get("diedAt").is_none() || cert.get("cause").is_none() {
        // A live-fence record misnamed with the cert prefix is not a cert.
        return None;
    }
    // Persist the migration under the qualified name with the legacy marker;
    // never stamp the request's merge into durable evidence. Best-effort: a
    // migration I/O failure simply re-attempts on the next probe.
    //
    // r39 item 1: this runtime path (a read-only probe migrating a legacy
    // certificate) goes through the SHARED-flock writer, NOT the `_locked`
    // helper which only a lock-holding caller (startup) may use — otherwise a
    // migration rename could race startup reclamation the same way a live
    // finalizer can.
    let mut migrated = cert.clone();
    migrated["provenance"] = json!(LEGACY_DEAD_CERT_PROVENANCE);
    if write_qualified_dead_cert_bytes(dir, &migrated, job_id, generation).is_ok() {
        let _ = std::fs::remove_file(&legacy_path);
    }
    validate_dead_cert(
        serde_json::to_string(&migrated).unwrap_or_default(),
        job_id,
        generation,
        merge_sha,
    )
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

/// Persist the granted land group fence. Every failure (missing identity or
/// granted merge sha, directory/temp/rename error) propagates: the caller has
/// the land child stopped before exec and refuses to release it until this
/// record is durable (child-start barrier, r15 R5). A missing record can never
/// be silently skipped into an unfenced, possibly-orphaned pusher.
///
/// r35 item 2: the record binds the attempt's FULL identity — generation AND
/// the exact granted merge sha. A finalizer opens only this attempt-qualified
/// path and the read-only probe accepts a certificate only for the exact
/// (job, generation, merge) tuple.
#[cfg(unix)]
fn write_land_fence(
    dir: &std::path::Path,
    job_id: &str,
    generation: u32,
    pgid: i32,
    identity: &LeaderIdentity,
    merge_sha: &str,
) -> std::io::Result<()> {
    if merge_sha.is_empty() {
        return Err(std::io::Error::other(
            "refusing to fence a granted land group without its exact merge sha",
        ));
    }
    std::fs::create_dir_all(dir)?;
    let mut record = serde_json::json!({
        "jobId": job_id,
        "generation": generation,
        "pgid": pgid,
        "mergeSha": merge_sha,
    });
    identity.write(&mut record);
    let path = land_fence_path(dir, job_id, generation);
    // r34 item 1: remove only THIS attempt's stale certificate (same
    // generation). Older/newer attempts' certificates are separate files and
    // must never be touched; their paths are immutable and attempt-qualified.
    if let Err(error) = std::fs::remove_file(land_dead_cert_path(dir, job_id, generation))
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error);
    }
    let temporary = dir.join(format!(
        ".land-g{generation}-{pgid}-{}.tmp",
        std::process::id()
    ));
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
/// job, generation, granted merge sha, process group, and durable leader
/// identity. Used to refuse certifying a LIVE REPLACEMENT group from a stale
/// observation (r17 item 1; r35 item 2 binds the merge sha too).
#[cfg(unix)]
fn land_fence_snapshot_matches(a: &Value, b: &Value) -> bool {
    if a.get("jobId").and_then(Value::as_str) != b.get("jobId").and_then(Value::as_str)
        || a.get("generation").and_then(Value::as_u64)
            != b.get("generation").and_then(Value::as_u64)
        || a.get("mergeSha").and_then(Value::as_str) != b.get("mergeSha").and_then(Value::as_str)
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

/// r34 item 1 TEST-ONLY: one-shot park inserted AFTER certify's snapshot
/// comparison passes but BEFORE the death certificate is written and the fence
/// unlinked — exactly the window in which an older attempt's detached
/// finalizer used to be able to delete a newer attempt's live fence. A test
/// parks the A finalizer here, installs attempt B's fence, resumes A, and
/// proves B's fence survives and A's certificate never certifies B.
#[cfg(test)]
static CERTIFY_COMPARE_HOLD: std::sync::OnceLock<
    std::sync::Mutex<Option<std::sync::Arc<CleanupHold>>>,
> = std::sync::OnceLock::new();

/// r34 item 1: park at the post-compare/pre-unlink certify seam.
#[cfg(test)]
fn park_certify_after_compare() {
    let guard = CERTIFY_COMPARE_HOLD
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let Some(hold) = guard.as_ref().cloned() else {
        return;
    };
    drop(guard);
    hold.entered
        .store(true, std::sync::atomic::Ordering::Release);
    let (lock, cvar) = &*hold.release;
    let mut released = lock.lock().unwrap_or_else(|poison| poison.into_inner());
    while !*released {
        let (next, timeout) = cvar
            .wait_timeout(released, std::time::Duration::from_secs(20))
            .unwrap_or_else(|poison| poison.into_inner());
        released = next;
        if timeout.timed_out() {
            *released = true;
        }
    }
}

/// r34 item 1 test helper: arm the post-compare certify hold (one-shot).
#[cfg(test)]
pub(crate) fn arm_certify_compare_hold(hold: std::sync::Arc<CleanupHold>) {
    *CERTIFY_COMPARE_HOLD
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(hold);
}

/// r35 item 2 TEST-ONLY: one-shot park inserted AFTER the finalizer observed
/// `killpg` ESRCH but BEFORE it opens the exact-attempt fence path — the window
/// in which a recycled pgid can belong to a newer attempt B.
#[cfg(test)]
static SETTLE_ESRCH_HOLD: std::sync::OnceLock<
    std::sync::Mutex<Option<std::sync::Arc<CleanupHold>>>,
> = std::sync::OnceLock::new();

/// r35 item 2: park at the post-ESRCH/pre-lookup settle seam.
#[cfg(test)]
fn park_settle_after_esrch() {
    let guard = SETTLE_ESRCH_HOLD
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let Some(hold) = guard.as_ref().cloned() else {
        return;
    };
    drop(guard);
    hold.entered
        .store(true, std::sync::atomic::Ordering::Release);
    let (lock, cvar) = &*hold.release;
    let mut released = lock.lock().unwrap_or_else(|poison| poison.into_inner());
    while !*released {
        let (next, timeout) = cvar
            .wait_timeout(released, std::time::Duration::from_secs(20))
            .unwrap_or_else(|poison| poison.into_inner());
        released = next;
        if timeout.timed_out() {
            *released = true;
        }
    }
}

/// r35 item 2 test helper: arm the post-ESRCH settle hold (one-shot).
#[cfg(test)]
pub(crate) fn arm_settle_esrch_hold(hold: std::sync::Arc<CleanupHold>) {
    *SETTLE_ESRCH_HOLD
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(hold);
}

/// r36 item 1 / r37 item 2 TEST-ONLY: like CleanupHold but counts how many
/// certificate writers have parked, so a two-writer regression can wait until
/// BOTH have written their temp before letting either rename. r37 item 2
/// scopes it to the regression's own ledger directory and intended job pair so
/// an unrelated test's certificate write never counts as an arrival.
#[cfg(test)]
struct CountedHold {
    entered: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    release: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    dir: std::path::PathBuf,
    jobs: Vec<String>,
}

#[cfg(test)]
impl CountedHold {
    /// Only a write into THIS scoped ledger dir for one of the intended jobs
    /// parks; every other cert writer (sharing the process-global static)
    /// passes straight through and never touches `entered`.
    fn matches(&self, dir: &std::path::Path, job_id: &str) -> bool {
        dir == self.dir && self.jobs.iter().any(|job| job == job_id)
    }
}

#[cfg(test)]
static CERT_TEMP_HOLD: std::sync::OnceLock<std::sync::Mutex<Option<std::sync::Arc<CountedHold>>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
fn park_cert_temp_before_rename(dir: &std::path::Path, job_id: &str) {
    // Clone (do not consume): both scoped writers must observe the armed hold.
    let hold = {
        let guard = CERT_TEMP_HOLD
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        guard
            .as_ref()
            .filter(|hold| hold.matches(dir, job_id))
            .cloned()
    };
    let Some(hold) = hold else {
        return;
    };
    hold.entered
        .fetch_add(1, std::sync::atomic::Ordering::Release);
    let (lock, cvar) = &*hold.release;
    let mut released = lock.lock().unwrap_or_else(|poison| poison.into_inner());
    while !*released {
        let (next, timeout) = cvar
            .wait_timeout(released, std::time::Duration::from_secs(20))
            .unwrap_or_else(|poison| poison.into_inner());
        released = next;
        if timeout.timed_out() {
            *released = true;
        }
    }
}

/// r39 item 2: serializes every test that uses the single-process
/// CERT_TEMP_HOLD slot, from arming through writer completion (the guard is
/// kept alive until after the writer threads join). The two barrier tests
/// would otherwise overwrite each other's slot when run in the same binary.
#[cfg(test)]
static CERT_BARRIER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Arm the cert-temp barrier AND acquire the process-wide test serialization
/// lock. The returned [`CertTempHoldGuard`] disarms the slot, releases parked
/// writers and frees the serialization lock on drop — hold it until after the
/// spawned writers have joined.
#[cfg(test)]
fn arm_cert_temp_hold(dir: std::path::PathBuf, jobs: Vec<String>) -> CertTempHoldGuard {
    // Acquire the serialization lock BEFORE touching the shared slot so two
    // barrier tests can never arm concurrently.
    let serial = CERT_BARRIER_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let hold = std::sync::Arc::new(CountedHold {
        entered: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        release: std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new())),
        dir,
        jobs,
    });
    *CERT_TEMP_HOLD
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(hold.clone());
    CertTempHoldGuard {
        hold,
        _serial: serial,
    }
}

/// r37 item 2 / r39 item 2 TEST-ONLY: RAII scope guard for the cert-temp
/// barrier. Dropping it disarms the process-global hold, releases every parked
/// writer and frees the cross-test serialization lock, so an assertion failure
/// or normal completion can never leave another test's writer blocked, the
/// static armed, or the slot occupied.
#[cfg(test)]
struct CertTempHoldGuard {
    hold: std::sync::Arc<CountedHold>,
    /// Held to the end of the test; serializes all CERT_TEMP_HOLD users.
    _serial: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl CertTempHoldGuard {
    /// Number of scoped writers parked at the barrier.
    fn entered(&self) -> usize {
        self.hold.entered.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Release (and wake) every writer parked on this barrier.
    fn release(&self) {
        let (lock, cvar) = &*self.hold.release;
        let mut released = lock.lock().unwrap_or_else(|poison| poison.into_inner());
        *released = true;
        cvar.notify_all();
    }
}

#[cfg(test)]
impl Drop for CertTempHoldGuard {
    fn drop(&mut self) {
        *CERT_TEMP_HOLD
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
        self.release();
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
    // certificate or a record missing provenance. r35 item 2: the granted
    // merge sha is mandatory provenance — an unqualified snapshot can never
    // become a certificate.
    if observed.get("pgid").and_then(Value::as_i64) != Some(i64::from(observed_pgid))
        || observed.get("generation").and_then(Value::as_u64).is_none()
        || observed
            .get("mergeSha")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || LeaderIdentity::from_record(observed).is_none()
        || observed.get("diedAt").is_some()
        || observed.get("cause").is_some()
    {
        return Ok(false);
    }
    let generation = observed
        .get("generation")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .expect("validated above");
    let fence_path = land_fence_path(dir, job_id, generation);
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
    // r34 item 1: test seam parks after the compare, before cert write/unlink.
    #[cfg(test)]
    park_certify_after_compare();
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
            let _ = std::fs::remove_file(land_dead_cert_path(dir, job_id, generation));
            Err(error)
        }
    }
}

/// Settle a verified-gone land group using ONLY the attempt's exact identity:
/// open the single immutable fence path for `(job_id, generation)` and certify
/// only when its on-disk snapshot still names THIS attempt — the same merge
/// sha, the same pgid the caller observed ESRCH for, and a valid leader
/// identity. There is deliberately NO directory scan: a recyclable pgid can
/// never select another attempt's file (r35 item 2). A missing/mismatched
/// fence keeps everything on disk (Unknown); certification errors likewise
/// keep the fence.
#[cfg(unix)]
fn settle_fence_for_exact_attempt(
    dir: &std::path::Path,
    job_id: &str,
    generation: u32,
    merge_sha: &str,
    pgid: i32,
) {
    if merge_sha.is_empty() {
        tracing::warn!(
            job_id,
            generation,
            "settling land group without the exact granted merge sha; refusing to certify"
        );
        return;
    }
    let fence_path = land_fence_path(dir, job_id, generation);
    let text = match std::fs::read_to_string(&fence_path) {
        Ok(text) => text,
        // The exact attempt's fence is absent: never manufacture evidence and
        // never look for another attempt via the recyclable pgid.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(
                %error,
                job_id,
                generation,
                "reading the exact-attempt land fence failed; keeping the fence"
            );
            return;
        }
    };
    let Ok(record) = serde_json::from_str::<Value>(&text) else {
        tracing::warn!(
            job_id,
            generation,
            "the exact-attempt land fence is unreadable; keeping the fence"
        );
        return;
    };
    // The on-disk record must be THIS attempt's full live snapshot. Any
    // divergence (wrong merge, a recycled pgid, another attempt's contents at
    // the path) refuses certification rather than touch the file.
    if record.get("jobId").and_then(Value::as_str) != Some(job_id)
        || record.get("generation").and_then(Value::as_u64) != Some(u64::from(generation))
        || record.get("mergeSha").and_then(Value::as_str) != Some(merge_sha)
        || record.get("pgid").and_then(Value::as_i64) != Some(i64::from(pgid))
        || LeaderIdentity::from_record(&record).is_none()
        || record.get("diedAt").is_some()
        || record.get("cause").is_some()
    {
        tracing::warn!(
            job_id,
            generation,
            pgid,
            "the exact-attempt land fence does not match the observed attempt identity; refusing \
             to certify"
        );
        return;
    }
    if let Err(error) = certify_dead_and_remove_fence(dir, &record, pgid) {
        tracing::warn!(
            %error,
            job_id,
            "certifying the settled land group failed; keeping the fence"
        );
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

/// r14 R1 / r15 R5, r20 item 2 / R8(a) startup fence: every leftover record
/// names a granted land group the PREVIOUS Node process may have orphaned
/// mid-push.
///
/// R8(a): after a restart the Node does NOT OWN the orphaned groups, so it
/// NEVER signals them — no TERM, no KILL, no "sampled identity" pinning. Even
/// an exact durable start-identity match is not an ownership pin: the original
/// parent/PID1 may reap the leader between the sample and the signal, its pid
/// can be recycled, and the unconditional post-ESRCH KILL would then strike an
/// unrelated group. Every fence that names a possibly-live group is therefore
/// KEPT; the Landing reads Unknown/uncertain and goes to operator resolution
/// (R5 fail closed). The ONLY autonomous action is when death is established
/// by the kernel for the WHOLE group:
/// * record unreadable/unparseable/missing fields/missing identity → Unknown,
///   KEEP (never delete the only proof, never signal);
/// * leader pid ALIVE (matched, mismatched, or unreadable identity) → KEEP
///   without signalling;
/// * leader pid ESRCH and the group ESRCH → established dead → certify+remove;
/// * leader pid ESRCH but the group still exists (orphaned members, no leader)
///   → Unknown → KEEP;
/// * `kill(pid, 0)` EPERM/other → Unknown → keep.
///
/// r35 item 3 (upgrade): a leftover record can be in the CURRENT
/// attempt-qualified format (`land-<job>-g<gen>.json` with a `mergeSha`) or the
/// LEGACY pre-r34 single-file format (`land-<job>.json`, contents-qualified, no
/// merge sha). A dead legacy fence is migrated EXPLICITLY from its observed
/// source path into the qualified certificate name, stamped
/// `provenance: "legacy-fence"` and WITHOUT a merge sha; the missing merge is
/// bound later, only through the Hub's authoritative generation-pinned probe
/// (`gate.resolveLanding` carrying the grant's merge sha). Such a certificate
/// is never accepted as generic death evidence (no merge authority → Unknown).
///
/// Explicit operator recovery route: a legacy record that cannot be bound to an
/// exact validated attempt (missing/unparseable generation or leader identity,
/// or a read failure) is KEPT and the Landing stays outcome-unknown. The
/// operator then resolves it through the Hub's `POST
/// /v1/projects/{id}/gate/jobs/{jobId}/resolve` route (Human/Bot operator,
/// pinning the exact `expectedAttempts`/`expectedSessionEpoch`/
/// `expectedMergeSha` tuple with a persisted rationale); unlike
/// `/resolve-evidence`, that legacy route lets the explicit operator decision
/// stand when the read-only probe can only answer Unknown.
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
        // The durable identity is required only to certify a proven-dead group;
        // R8(a) removed the live-leader identity match that used to authorize a
        // signal after restart.
        if LeaderIdentity::from_record(&record).is_none() {
            continue;
        }
        let Some(job_id) = record.get("jobId").and_then(Value::as_str) else {
            continue;
        };
        // r35 item 3: every bindable record must name its attempt generation
        // (both formats do); a record without one stays Unknown for the
        // documented operator route.
        let Some(generation) = record
            .get("generation")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
        else {
            tracing::warn!(
                pgid,
                "leftover land fence has no valid attempt generation; keeping it (operator \
                 resolution via the Hub /resolve route)"
            );
            continue;
        };
        // r35 item 3: content decides the format. New records carry the
        // granted merge sha; pre-r35 records never did. The observed file name
        // must agree with the content:
        // * qualified name + merge sha → current format;
        // * legacy unqualified name, no merge → pre-r34 single-file format;
        // * qualified name but NO merge → an r34-era attempt-qualified record
        //   (the attempt-generation naming existed one round before the merge
        //   binding); migrated like legacy evidence on ESRCH.
        let merge_sha = record.get("mergeSha").and_then(Value::as_str);
        let file_name = path.file_name();
        let qualified_name = file_name == land_fence_path(dir, job_id, generation).file_name();
        let legacy_name = file_name == legacy_land_fence_path(dir, job_id).file_name();
        let is_qualified = merge_sha.is_some_and(|sha| !sha.is_empty()) && qualified_name;
        let is_merge_less_legacy = merge_sha.is_none() && (legacy_name || qualified_name);
        if !is_qualified && !is_merge_less_legacy {
            tracing::warn!(
                pgid,
                job_id,
                generation,
                "leftover land fence has an unbindable format/name; keeping it (operator resolution \
                 via the Hub /resolve route)"
            );
            continue;
        }
        match kill(Pid::from_raw(pgid), None) {
            Ok(()) => {
                // R8(a): a process holds the leader pid — matched identity,
                // recycled pid, or unreadable sample. The restarted Node does
                // not own this group: NEVER signal (TERM/KILL can strike a
                // recycled group) and KEEP the fence so the Landing stays
                // Unknown for operator resolution.
                tracing::warn!(
                    pgid,
                    "orphaned land group leader is still alive after a Node restart; the Node \
                     does not own it (no signal sent); keeping the fence record for operator \
                     resolution"
                );
            }
            Err(nix::errno::Errno::ESRCH) => {
                // No process at the leader pid. With no leader, a still-present
                // group can no longer be positively distinguished from a
                // same-session process that joined it: only an authoritative
                // group ESRCH settles it; anything else keeps the record.
                if !land_group_exists(pgid) {
                    if is_merge_less_legacy {
                        // r35 item 3: explicit legacy migration FROM THE
                        // OBSERVED SOURCE PATH (pre-r34 single file or an r34
                        // qualified record written before the merge binding).
                        // The validated attempt identity is (job, generation,
                        // pgid, leader identity); the missing merge is NOT
                        // reconstructed here — the migrated certificate carries
                        // the legacy marker and no merge sha, and binds it only
                        // when the Hub later probes with its authoritative
                        // granted merge.
                        migrate_legacy_fence_to_cert(dir, &path, &record, job_id, generation);
                    } else {
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
/// `/proc` enumeration). It is NEVER consulted to prove death or to skip a
/// signal — production termination claims death only on the kernel's
/// authoritative `killpg(pgid, 0)` ESRCH, so an empty scan (or a platform
/// without `/proc`) can never be read as "cleared" (r15 R5). r19 item 3 adds
/// one TIMING-only use: after a normally-exited but unreaped land leader
/// signalled its group, the owner waits for the OTHER members to vanish before
/// reaping; the zombie leader itself keeps `killpg` reporting "exists", so the
/// post-reap ESRCH remains the authoritative proof.
#[cfg(unix)]
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

/// Test-only PORTABLE group-existence probe (`killpg(pgid, 0)`, r24 item 5):
/// the kernel's authoritative answer works on every Unix (including macOS,
/// which has no `/proc`). True while the group is reported present (including
/// EPERM, which means present but not ours to signal); false only on ESRCH.
#[cfg(unix)]
#[doc(hidden)]
#[cfg(any(test, feature = "test-faults"))]
pub fn gate_test_group_exists(pgid: i32) -> bool {
    land_group_exists(pgid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// r35 item 2: synthetic granted merge shas for fence/certificate fixture
    /// records (40-hex like a real sha). Fence evidence is bound to generation
    /// AND merge, so tests pass the same value the attempt was "granted".
    const MERGE_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const MERGE_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

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
    /// host ambient temp dir. The guard at the top panics unless the outer
    /// test exported that TMPDIR, so a direct `--ignored`/`--include-ignored`
    /// invocation can never plant the foreign root in the ambient temp dir.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "re-exec'd by a_foreign_runs_gate_report_is_not_streamed under a private TMPDIR"]
    async fn foreign_gate_report_is_not_streamed_inner() {
        let tmpdir = std::env::var_os("TMPDIR").expect("outer test must export TMPDIR");
        assert_ne!(
            std::path::Path::new(&tmpdir),
            std::path::Path::new("/tmp"),
            "the planted foreign root needs a TMPDIR that is not /tmp"
        );
        assert_eq!(
            std::env::temp_dir(),
            std::path::PathBuf::from(&tmpdir),
            "the foreign root is planted under $TMPDIR"
        );

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

    /// The fake CLI speaks the production child-start barrier protocol EXACTLY
    /// like `remuda merge`'s `await_start_barrier`: the verify child must NOT see
    /// the barrier variable; the LAND child must see REMUDA_START_BARRIER_FD=0
    /// and block on one stdin byte before any git work (exiting 4 on EOF). If the
    /// fd variable is ABSENT the real barrier call is a no-op, so a faithful
    /// fake PROCEEDS immediately (writes `bypassed.marker`, emits landed) — that
    /// is the dangerous path the env-clear-ordering bug exposed, and the
    /// held-child regression asserts it never happens.
    /// Land phase parks on the barrier then SLEEPS (a long in-flight push), so
    /// an abort message delivered while parked deterministically wins the
    /// owner's select rather than racing a quick natural exit.
    const ABORT_LAND_SCRIPT: &str = r#"
dir=$(dirname "$0")
echo "$*" >> "$dir/args.txt"
base=$(git rev-parse main 2>/dev/null || echo "1111111111111111111111111111111111111111")
head=$(git rev-parse wt/fake/task 2>/dev/null || echo "2222222222222222222222222222222222222222")
if ! printf '%s\n' "$*" | grep -q -- '--land'; then
  echo "{\"exitCode\":0,\"status\":\"verified\",\"base\":\"$base\",\"head\":\"$head\",\"merged\":\"$head\",\"steps\":[]}"
  exit 0
fi
if [ "${REMUDA_START_BARRIER_FD-}" != "0" ]; then
  exit 0
fi
byte=""
IFS= read -r -n 1 -t 30 byte || exit 4
[ -n "$byte" ] || exit 4
echo released > "$dir/abort-released.marker"
sleep 30
"#;

    /// r19 item 3: the land leader (this script) exits 0 with a valid `landed`
    /// report, but BEFORE exiting it forks a descendant in the SAME process
    /// group whose output is DETACHED to /dev/null. The descendant outlives
    /// the leader; the land owner must kill it BEFORE reaping, so the group
    /// settles (death certificate written) and nothing survives.
    const SURVIVOR_LAND_SCRIPT: &str = r#"
dir=$(dirname "$0")
echo "$*" >> "$dir/args.txt"
base=$(git rev-parse main 2>/dev/null || echo "1111111111111111111111111111111111111111")
head=$(git rev-parse wt/fake/task 2>/dev/null || echo "2222222222222222222222222222222222222222")
if ! printf '%s\n' "$*" | grep -q -- '--land'; then
  echo "{\"exitCode\":0,\"status\":\"verified\",\"base\":\"$base\",\"head\":\"$head\",\"merged\":\"$head\",\"steps\":[]}"
  exit 0
fi
if [ "${REMUDA_START_BARRIER_FD-}" != "0" ]; then
  exit 0
fi
byte=""
IFS= read -r -n 1 -t 30 byte || exit 4
[ -n "$byte" ] || exit 4
# Same-group grandchild with fully detached output: stdout/stderr close in the
# leader does NOT take it down, and nothing waits for it.
(sleep 47.91 >/dev/null 2>&1 &)
echo "{\"exitCode\":0,\"status\":\"landed\",\"merged\":\"$head\",\"currentMain\":\"$head\",\"steps\":[]}"
exit 0
"#;

    /// r20 item 3 / R8(c): once released, the land leader CLOSES its captured
    /// stdout AND stderr (both reads reach EOF) but keeps WORKING — sleeping
    /// well past the land budget. Closing the pipes is NOT an exit. The owner
    /// must keep its non-reaping waitid observer running and kill the group
    /// only at the real land timeout, never when read_to_end returns.
    const CLOSED_OUTPUT_LAND_SCRIPT: &str = r#"
dir=$(dirname "$0")
echo "$*" >> "$dir/args.txt"
base=$(git rev-parse main 2>/dev/null || echo "1111111111111111111111111111111111111111")
head=$(git rev-parse wt/fake/task 2>/dev/null || echo "2222222222222222222222222222222222222222")
if ! printf '%s\n' "$*" | grep -q -- '--land'; then
  echo "{\"exitCode\":0,\"status\":\"verified\",\"base\":\"$base\",\"head\":\"$head\",\"merged\":\"$head\",\"steps\":[]}"
  exit 0
fi
if [ "${REMUDA_START_BARRIER_FD-}" != "0" ]; then
  exit 0
fi
byte=""
IFS= read -r -n 1 -t 30 byte || exit 4
[ -n "$byte" ] || exit 4
# Detach BOTH captured pipes: every Node reader sees EOF while this leader is
# still alive and still inside its land group.
exec >/dev/null 2>&1
sleep 47.37
"#;

    /// r21 item 1 / R8(b): the released land leader prints its valid final
    /// JSON and exits 0 IMMEDIATELY, while a SAME-GROUP descendant it forked
    /// keeps the captured STDOUT (and stderr) open. The old owner drained
    /// stdout to EOF BEFORE its first waitid poll, so it never observed the
    /// exit and discarded the valid verdict at the soft land timeout. The
    /// concurrent owner observes the exit independently of the pipes, KILLs the
    /// survivor while the leader is unreaped, then joins the stdout reader to
    /// EOF and honors the landed verdict.
    const STDOUT_RETAINED_LAND_SCRIPT: &str = r#"
dir=$(dirname "$0")
echo "$*" >> "$dir/args.txt"
base=$(git rev-parse main 2>/dev/null || echo "1111111111111111111111111111111111111111")
head=$(git rev-parse wt/fake/task 2>/dev/null || echo "2222222222222222222222222222222222222222")
if ! printf '%s\n' "$*" | grep -q -- '--land'; then
  echo "{\"exitCode\":0,\"status\":\"verified\",\"base\":\"$base\",\"head\":\"$head\",\"merged\":\"$head\",\"steps\":[]}"
  exit 0
fi
if [ "${REMUDA_START_BARRIER_FD-}" != "0" ]; then
  exit 0
fi
byte=""
IFS= read -r -n 1 -t 30 byte || exit 4
[ -n "$byte" ] || exit 4
# Same-group grandchild INHERITS both captured pipes: stdout (and stderr) stay
# open after the leader exits; the valid report above is already buffered.
(sleep 47.21) &
echo "{\"exitCode\":0,\"status\":\"landed\",\"merged\":\"$head\",\"currentMain\":\"$head\",\"steps\":[]}"
exit 0
"#;

    /// r22 item 3: the released land leader writes a line of INVALID UTF-8 to
    /// stderr (a byte sequence `lines()` cannot decode), then a valid `landed`
    /// report on stdout, and exits 0. Portable — no `setsid`, no descendant,
    /// both pipes close at exit. Used two ways: with the injected stderr read
    /// fault the failed drain must make the land UNCERTAIN; without it the raw
    /// bytes drain to EOF (the decode error is presentation-only) and the
    /// valid verdict still lands.
    const RAW_STDERR_LAND_SCRIPT: &str = r#"
dir=$(dirname "$0")
echo "$*" >> "$dir/args.txt"
base=$(git rev-parse main 2>/dev/null || echo "1111111111111111111111111111111111111111")
head=$(git rev-parse wt/fake/task 2>/dev/null || echo "2222222222222222222222222222222222222222")
if ! printf '%s\n' "$*" | grep -q -- '--land'; then
  echo "{\"exitCode\":0,\"status\":\"verified\",\"base\":\"$base\",\"head\":\"$head\",\"merged\":\"$head\",\"steps\":[]}"
  exit 0
fi
if [ "${REMUDA_START_BARRIER_FD-}" != "0" ]; then
  exit 0
fi
byte=""
IFS= read -r -n 1 -t 30 byte || exit 4
[ -n "$byte" ] || exit 4
# One line containing an invalid UTF-8 lead byte: a `lines()` decoder errors on
# it, but the raw-byte drain must not.
printf 'land-remote-note-\377-end\n' >&2
echo "{\"exitCode\":0,\"status\":\"landed\",\"merged\":\"$head\",\"currentMain\":\"$head\",\"steps\":[]}"
exit 0
"#;

    /// r23 item 3: passes the verify invocation instantly, then blocks in the
    /// granted --land phase (after consuming the start-barrier byte) until the
    /// owner's soft timeout kills it — exercises the timeout/abort arm.
    const SLOW_LAND_SCRIPT: &str = r#"
base=$(git rev-parse main 2>/dev/null || echo "1111111111111111111111111111111111111111")
head=$(git rev-parse wt/fake/task 2>/dev/null || echo "2222222222222222222222222222222222222222")
if ! printf '%s\n' "$*" | grep -q -- '--land'; then
  echo "{\"exitCode\":0,\"status\":\"verified\",\"base\":\"$base\",\"head\":\"$head\",\"merged\":\"$head\",\"steps\":[]}"
  exit 0
fi
if [ "${REMUDA_START_BARRIER_FD-}" != "0" ]; then
  exit 0
fi
IFS= read -r -n 1 -t 30 byte || exit 4
sleep 30
"#;

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
  # Faithful reproduction of the real CLI with no fd var: the start-barrier
  # wait is skipped entirely, so the land proceeds WITHOUT a durable fence.
  echo bypassed > "$dir/bypassed.marker"
  echo "{\"exitCode\":0,\"status\":\"landed\",\"merged\":\"$head\",\"currentMain\":\"$head\",\"steps\":[]}"
  exit 0
fi
# r18 item 10: a SINGLE one-byte read that returns nonzero/empty on EOF and 0
# only when a real release byte arrives — mirroring merge.rs
# await_start_barrier (read()==0 -> exit 4). `head -c1` alone returns 0 even
# on EOF and would falsely release; read -n1 distinguishes the two.
byte=""
IFS= read -r -n 1 -t 30 byte || { echo "start barrier: no release byte (EOF/zero)" >&2; exit 4; }
if [ -z "$byte" ]; then
  echo "start barrier released a zero byte" >&2
  exit 4
fi
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
        // Give the (unblocked) child a real window to misbehave: if the barrier
        // variable had been wiped by env_clear, the faithful fake proceeds past
        // the barrier immediately and writes bypassed.marker. It must not.
        std::thread::sleep(Duration::from_millis(300));
        let marker_dir = fixture.bin.parent().unwrap();
        assert!(
            !marker_dir.join("released.marker").exists(),
            "the child must not pass the barrier while the fence is unpersisted"
        );
        assert!(
            !marker_dir.join("bypassed.marker").exists(),
            "the child must not run past a missing barrier (the env var was wiped by env_clear)"
        );
        // The child stays ALIVE and parked: the run must still be in flight.
        assert!(
            !run.is_finished(),
            "the blocked land child keeps the run in flight while the fence latch is held"
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
        assert!(
            !fixture
                .bin
                .parent()
                .unwrap()
                .join("bypassed.marker")
                .exists(),
            "the child never bypassed the barrier (the env var survived env_clear)"
        );
        assert_eq!(
            origin_main(&fixture),
            base_main,
            "a fenced-off child never pushes"
        );
    }

    /// r19 item 1 / R6: abortLanding is a MESSAGE to the land child's unique
    /// owner, never an external signal of a saved pgid. With the child parked
    /// behind the start barrier (fence persisted, hold latched), the abort
    /// message latches; releasing the hold starts the child and the owner's
    /// select observes the abort and is the SOLE thing that kills+reaps the
    /// group. Result is uncertain; nothing is pushed; no process survives.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_landing_message_makes_the_owner_kill_and_reap_uncertain() {
        let fixture = fixture(ABORT_LAND_SCRIPT);
        let base_main = origin_main(&fixture);
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        // fail:false — the fence DOES persist; the hold still parks the run so
        // we can interleave the abort before the child is released.
        fixture
            .node
            .gate_registry()
            .set_land_fence_fault(LandFenceFault {
                entered: entered.clone(),
                release: release.clone(),
                fail: false,
            });
        fixture
            .node
            .gate_test_set_land_timeout(Duration::from_secs(2))
            .await;
        let mut p = params(&fixture, "land");
        p.push = false;
        p.job_id = "gjb_abort_msg".into();
        p.generation = 1;
        let node = fixture.node.clone();
        let run = tokio::spawn(async move { node.run_gate_typed(p).await });
        tokio::time::timeout(Duration::from_secs(15), entered.notified())
            .await
            .expect("fence persistence entered (child parked)");
        // Wait until the land argv line is present (child spawned).
        let argv_path = fixture.bin.parent().unwrap().join("args.txt");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let text = std::fs::read_to_string(&argv_path).unwrap_or_default();
            if text.lines().any(|l| l.contains("--land")) {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        // Deliver the abort as a MESSAGE (the handler under test) while the
        // child is parked and the run is still in flight.
        let abort_params = serde_json::json!({
            "jobId": "gjb_abort_msg",
            "generation": 1u32,
        });
        fixture
            .node
            .abort_gate_landing(&abort_params)
            .await
            .expect("abortLanding accepted");
        assert!(
            !run.is_finished(),
            "the parked child keeps the run alive until released"
        );
        // Release: fence persists, barrier opens; the owner select sees the
        // latched abort and kills+reaps its own group.
        {
            let (lock, cvar) = &*release;
            let mut done = lock.lock().unwrap();
            *done = true;
            cvar.notify_one();
        }
        let result = tokio::time::timeout(Duration::from_secs(30), run)
            .await
            .expect("run finished")
            .expect("joined")
            .expect("run_gate returned");
        assert_eq!(
            result.status, "uncertain",
            "an owner-aborted land is uncertain, never failed: {result:?}"
        );
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|t| t.contains("aborted")),
            "the error names the abort: {:?}",
            result.error
        );
        assert_eq!(origin_main(&fixture), base_main, "nothing was pushed");
        // The land run is gone and its pgid is no longer live.
        assert!(
            fixture
                .node
                .gate_registry()
                .land_push_liveness("gjb_abort_msg", Some(1), result.merge_sha.as_deref())
                .await
                != LandPushLiveness::MayPush,
            "the aborted land group is not live afterwards"
        );
    }

    /// r19 item 3 / R6 production-path regression: the land leader exits 0
    /// (a valid landed report) while a SAME-GROUP child with detached output is
    /// still running. The owner observes the exit WITHOUT reaping (waitid
    /// WNOWAIT), kills the surviving group member BEFORE reaping the leader,
    /// and the fence settles (authoritative post-reap ESRCH → death
    /// certificate): the verdict is `landed`, no descendant survives, and a
    /// later liveness probe attests VerifiedDead rather than keeping the fence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_surviving_same_group_child_is_killed_before_the_land_leader_is_reaped() {
        let fixture = fixture(SURVIVOR_LAND_SCRIPT);
        let base_main = origin_main(&fixture);
        let mut p = params(&fixture, "land");
        p.push = false;
        p.job_id = "gjb_survivor".into();
        p.generation = 1;
        // Baseline box-wide count of the marker, so a concurrent run's sleep
        // never masks this run's own surviving descendant.
        let before = live_marker_processes("sleep 47.91");
        let started = std::time::Instant::now();
        let result = fixture.node.run_gate_typed(p).await.unwrap();
        assert_eq!(
            result.status, "landed",
            "the leader's valid landed verdict is honored: {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "owner cleanup is bounded, not the descendant's 47 s sleep: {:?}",
            started.elapsed()
        );
        assert_eq!(origin_main(&fixture), base_main, "push=false moves nothing");
        // No live detached descendant lingers (a killed-and-reparented sleep
        // may briefly be a zombie; only non-zombie marks count).
        // Poll to the baseline, then assert: the owner's group cleanup kills
        // exactly this run's forked descendant (zombies are excluded).
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let left = live_marker_processes("sleep 47.91");
            if left <= before || std::time::Instant::now() >= deadline {
                assert_eq!(
                    left, before,
                    "a same-group descendant survived the land owner's pre-reap cleanup"
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // The fence settled through the authoritative post-reap ESRCH: a later
        // probe attests VerifiedDead, instead of keeping the fence (the old
        // warn-only behavior while the descendant lived).
        assert_eq!(
            fixture
                .node
                .gate_registry()
                .land_push_liveness("gjb_survivor", Some(1), result.merge_sha.as_deref())
                .await,
            LandPushLiveness::VerifiedDead,
            "the land group must settle (death certificate) after pre-reap cleanup"
        );
    }

    /// r20 item 3 / R8(c): closing the captured stdout/stderr is not an exit.
    /// The released land leader closes BOTH pipes and keeps working past the
    /// land budget. The owner must NOT observe an early "exit" (the old
    /// waitid flags returned EINVAL, which was wrongly mapped to exited, so the
    /// still-live group got TERM/KILLed): the leader stays alive until the real
    /// land timeout elapses, and only then is the group terminated → uncertain.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_land_leader_that_closed_its_output_is_not_killed_before_its_timeout() {
        let fixture = fixture(CLOSED_OUTPUT_LAND_SCRIPT);
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let entered = Arc::new(tokio::sync::Notify::new());
        fixture
            .node
            .gate_registry()
            .set_land_fence_fault(LandFenceFault {
                entered: entered.clone(),
                release: release.clone(),
                fail: false,
            });
        // A short but observable budget: the child sleeps ~47 s.
        let budget = Duration::from_secs(2);
        fixture.node.gate_test_set_land_timeout(budget).await;
        let mut p = params(&fixture, "land");
        p.push = false;
        p.job_id = "gjb_closed_output".into();
        p.generation = 1;
        let node = fixture.node.clone();
        let run = tokio::spawn(async move { node.run_gate_typed(p).await });
        tokio::time::timeout(Duration::from_secs(15), entered.notified())
            .await
            .expect("fence persistence entered (child parked)");
        // Release the leader; it closes both pipes and keeps sleeping.
        let released = std::time::Instant::now();
        {
            let (lock, cvar) = &*release;
            let mut done = lock.lock().unwrap();
            *done = true;
            cvar.notify_one();
        }
        // Half a budget in, the "exited on EOF" bug would already have
        // TERM/KILLed the group: the marker sleep must still be running.
        tokio::time::sleep(budget / 2).await;
        assert!(
            !run.is_finished(),
            "the run must still own the pipe-closed, still-alive land at half the budget"
        );
        assert!(
            live_marker_processes("sleep 47.37") >= 1,
            "the pipe-closed leader was killed before its land timeout"
        );
        let result = tokio::time::timeout(Duration::from_secs(30), run)
            .await
            .expect("run finished")
            .expect("joined")
            .expect("run_gate returned");
        assert_eq!(
            result.status, "uncertain",
            "a budget kill of a released land is uncertain: {result:?}"
        );
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|t| t.contains("exceeded")),
            "the uncertainty is the land timeout, not an early EOF-exit: {:?}",
            result.error
        );
        // From release to verdict the run lasts essentially the WHOLE budget
        // (half-budget sleep above plus the remaining wait): the EOF at the
        // pipes never shortened it. Generous slack for a loaded shared box.
        assert!(
            released.elapsed() >= budget - Duration::from_millis(500),
            "the pipe-closed land was settled before its timeout: {:?} (budget {budget:?})",
            released.elapsed()
        );
        // No marker process survives the real budget kill.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let left = live_marker_processes("sleep 47.37");
            if left == 0 || std::time::Instant::now() >= deadline {
                assert_eq!(left, 0, "the budget-killed leader survived its owner");
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// r21 item 1 / R8(b): the land leader EXITS FIRST with a valid landed
    /// report while a same-group descendant still holds stdout (and stderr)
    /// open. The owner drains the pipes CONCURRENTLY with its non-reaping exit
    /// observation, so it observes the exit at once, KILLs the surviving
    /// descendant while the leader is unreaped, joins the stdout reader to EOF
    /// and returns `landed` — bounded by the owner cleanup allowance, never by
    /// the descendant's sleep or the soft land timeout. The old owner blocked
    /// in `read_to_end` before its first waitid poll and returned an uncertain
    /// timeout instead of the valid verdict.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_land_leader_whose_descendant_keeps_stdout_is_observed_and_lands() {
        let fixture = fixture(STDOUT_RETAINED_LAND_SCRIPT);
        let base_main = origin_main(&fixture);
        let mut p = params(&fixture, "land");
        p.push = false;
        p.job_id = "gjb_stdout_retained".into();
        p.generation = 1;
        let before = live_marker_processes("sleep 47.21");
        // The soft budget is short; the owner must not need it.
        fixture
            .node
            .gate_test_set_land_timeout(Duration::from_secs(2))
            .await;
        let started = std::time::Instant::now();
        let result = fixture.node.run_gate_typed(p).await.unwrap();
        assert_eq!(
            result.status, "landed",
            "the valid verdict emitted before the pipe closed is honored: {result:?}"
        );
        // Exit observation + survivor kill + reap + pipe join must fit inside
        // the absolute deadline (budget + cleanup allowance), not the sleep.
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "owner cleanup is bounded by the absolute deadline, not the holder's sleep: {:?}",
            started.elapsed()
        );
        assert_eq!(origin_main(&fixture), base_main, "push=false moves nothing");
        // The stdout-holding same-group descendant was KILLed during normal
        // completion; no live marker outlives the owner (baseline for parallel
        // runs; zombies do not count).
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let left = live_marker_processes("sleep 47.21");
            if left <= before || std::time::Instant::now() >= deadline {
                assert_eq!(
                    left, before,
                    "a same-group stdout holder survived the owner's normal-completion cleanup"
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// r22 item 6 (was r21 item 2): the leader exits 0 with a valid landed
    /// report while a survivor in ANOTHER session keeps the captured STDERR
    /// open. The owner must retain BOTH reader handles through cleanup and
    /// reap, await them with the remaining absolute budget, and answer
    /// UNCERTAIN when the stderr drain never completes — never `landed`.
    ///
    /// PORTABLE replacement for the external-`setsid` test: the surviving
    /// holder is the injected reader-completion seam — the raw stderr reader
    /// parks at EOF instead of finishing — so there is no `setsid` dependency
    /// (absent on macOS), no unsynchronised descendant that cleanup could kill
    /// first, and no `/proc` timing. The reader parks entirely inside the
    /// Node's own task and is aborted with the test runtime.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_blocked_stderr_drain_after_a_clean_exit_makes_the_land_uncertain() {
        let fixture = fixture(RAW_STDERR_LAND_SCRIPT);
        let base_main = origin_main(&fixture);
        let mut p = params(&fixture, "land");
        p.push = false;
        p.job_id = "gjb_stderr_eof_hold".into();
        p.generation = 1;
        // Soft budget 2 s; the absolute deadline (budget + the fixed owner
        // cleanup allowance) bounds the blocked drain.
        fixture
            .node
            .gate_test_set_land_timeout(Duration::from_secs(2))
            .await;
        // The portable other-session holder: the stderr reader parks at EOF.
        fixture.node.gate_test_set_land_stderr_eof_hold(true).await;
        let started = std::time::Instant::now();
        let result = fixture.node.run_gate_typed(p).await.unwrap();
        fixture.node.gate_test_set_land_stderr_eof_hold(false).await;
        assert_eq!(
            result.status, "uncertain",
            "a blocked stderr drain after the leader exited is uncertainty, got: {result:?}"
        );
        assert!(
            result
                .reason
                .as_deref()
                .is_some_and(|t| t.contains("drain")),
            "the uncertainty names the incomplete pipe drain: {:?}",
            result.reason
        );
        // The verdict waits only for the absolute deadline, not a real
        // long-lived holder's sleep.
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "the absolute deadline bounds the blocked drain: {:?}",
            started.elapsed()
        );
        assert_eq!(origin_main(&fixture), base_main, "push=false moves nothing");
    }

    /// r22 item 3: a FAILED stderr drain makes even a clean, valid land
    /// UNCERTAIN. The child writes one stderr line (here with invalid UTF-8),
    /// prints a valid `landed` report and exits 0; the injected reader seam
    /// makes the raw stderr reader return a read ERROR after its first chunk.
    /// The old `BufReader::lines()` loop swallowed both a UTF-8 decoding error
    /// and any read error as a successful empty drain, so the join accepted
    /// `landed`. Deterministic and portable (no descendant / `setsid`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_stderr_drain_after_a_clean_exit_makes_the_land_uncertain() {
        let fixture = fixture(RAW_STDERR_LAND_SCRIPT);
        let base_main = origin_main(&fixture);
        let mut p = params(&fixture, "land");
        p.push = false;
        p.job_id = "gjb_raw_stderr_fault".into();
        p.generation = 1;
        fixture
            .node
            .gate_test_set_land_timeout(Duration::from_secs(2))
            .await;
        // Arm the synthetic raw-read failure AFTER the stderr reader's first
        // non-empty read.
        fixture
            .node
            .gate_test_set_land_stderr_read_fault(true)
            .await;
        let result = fixture.node.run_gate_typed(p).await.unwrap();
        fixture
            .node
            .gate_test_set_land_stderr_read_fault(false)
            .await;
        assert_eq!(
            result.status, "uncertain",
            "a failed stderr drain is uncertainty even on a clean exit, got: {result:?}"
        );
        assert!(
            result
                .reason
                .as_deref()
                .is_some_and(|t| t.contains("drain")),
            "the uncertainty names the failed pipe drain: {:?}",
            result.reason
        );
        assert_eq!(origin_main(&fixture), base_main, "push=false moves nothing");
    }

    /// r22 item 3: invalid UTF-8 on stderr is a PRESENTATION concern only. The
    /// readers drain raw bytes to EOF, so a clean exit with a decode-hostile
    /// stderr line and both pipes closed still yields the valid `landed`
    /// verdict (the old `lines()` decoder would have aborted the line stream,
    /// but draining raw bytes does not). Portable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_utf8_on_stderr_drains_raw_and_still_lands() {
        let fixture = fixture(RAW_STDERR_LAND_SCRIPT);
        let base_main = origin_main(&fixture);
        let mut p = params(&fixture, "land");
        p.push = false;
        p.job_id = "gjb_raw_stderr_utf8".into();
        p.generation = 1;
        fixture
            .node
            .gate_test_set_land_timeout(Duration::from_secs(2))
            .await;
        // No read fault: the invalid-UTF-8 line drains raw and EOF closes both
        // pipes at the leader's exit.
        let result = fixture.node.run_gate_typed(p).await.unwrap();
        assert_eq!(
            result.status, "landed",
            "a full raw-byte drain tolerates invalid UTF-8 and keeps the verdict: {result:?}"
        );
        assert_eq!(origin_main(&fixture), base_main, "push=false moves nothing");
    }

    /// r22 item 4: the absolute deadline is LATCHED at final drain acceptance.
    /// The leader exits 0 with a valid landed report and both pipes close at
    /// exit, so BOTH reader joins complete successfully — but the owner is
    /// parked at the injected final-acceptance seam until AFTER the original
    /// absolute deadline has expired. Tokio's `timeout` polls the (already
    /// complete) inner join first and would return Ok regardless; without the
    /// explicit post-join clock latch the owner would accept a definitive
    /// `landed`. The verdict must be UNCERTAIN, and it must return promptly
    /// after release (not wait out anything else).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_drain_completed_only_after_the_deadline_is_latched_uncertain() {
        let fixture = fixture(RAW_STDERR_LAND_SCRIPT);
        let base_main = origin_main(&fixture);
        let mut p = params(&fixture, "land");
        p.push = false;
        p.job_id = "gjb_late_drain_latch".into();
        p.generation = 1;
        // Soft budget 0.5 s + the fixed 3.25 s owner cleanup allowance = a
        // 3.75 s absolute deadline.
        fixture
            .node
            .gate_test_set_land_timeout(Duration::from_millis(500))
            .await;

        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        fixture
            .node
            .gate_test_arm_final_drain_accept_hold(Arc::new(FinalDrainHold {
                entered: entered.clone(),
                release: release.clone(),
            }))
            .await;

        let node = fixture.node.clone();
        let run = tokio::spawn(async move { node.run_gate_typed(p).await });
        // Both joins complete once the leader exits; wait for the owner to
        // reach final acceptance, then let the ORIGINAL absolute deadline
        // expire while it is parked.
        entered.notified().await;
        tokio::time::sleep(Duration::from_millis(4_200)).await;
        let before_release = std::time::Instant::now();
        release.notify_one();
        let result = run.await.expect("run task panicked").unwrap();

        assert_eq!(
            result.status, "uncertain",
            "a complete drain accepted only after the absolute deadline must be latched \
             uncertain, got: {result:?}"
        );
        assert!(
            result
                .reason
                .as_deref()
                .is_some_and(|t| t.contains("drain")),
            "the uncertainty names the late/incomplete drain: {:?}",
            result.reason
        );
        assert!(
            before_release.elapsed() < Duration::from_secs(2),
            "the latched owner must return promptly after release"
        );
        assert_eq!(origin_main(&fixture), base_main, "push=false moves nothing");
    }

    /// r22 item 5: cleanup QUEUEING/running time is inside the original
    /// deadline. The leader exits 0 with no surviving descendant; the
    /// survivor-cleanup blocking task is held (modelling blocking-pool
    /// contention) BEFORE it signals the group. The response must stop waiting
    /// at the ORIGINAL absolute deadline and return uncertain WHILE the hold
    /// is still engaged — the old code awaited the cleanup JoinHandle with no
    /// timeout, so this run could not return at all until the hold released.
    /// Releasing afterwards lets the SAME detached owner finish cleanup and
    /// the single reap (no independent response-path reap).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_queued_cleanup_past_the_deadline_detaches_one_cleanup_reap_owner() {
        let fixture = fixture(RAW_STDERR_LAND_SCRIPT);
        let base_main = origin_main(&fixture);
        let mut p = params(&fixture, "land");
        p.push = false;
        p.job_id = "gjb_queued_cleanup".into();
        p.generation = 1;
        // 0.5 s soft budget + 3.25 s owner allowance = 3.75 s absolute deadline.
        fixture
            .node
            .gate_test_set_land_timeout(Duration::from_millis(500))
            .await;

        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        fixture
            .node
            .gate_test_arm_cleanup_hold(Arc::new(CleanupHold {
                entered: entered.clone(),
                release: release.clone(),
            }));

        let started = std::time::Instant::now();
        let node = fixture.node.clone();
        let run = tokio::spawn(async move { node.run_gate_typed(p).await });

        // The blocking cleanup task queued, ran and parked before signalling.
        // Poll the atomic flag ASYNCHRONOUSLY — never block a tokio worker on a
        // std wait while the gate run needs it.
        let entered_poll = async {
            while !entered.load(std::sync::atomic::Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), entered_poll)
            .await
            .expect("the survivor-cleanup task never entered its hold");

        // The response must return on its own, bounded by the absolute
        // deadline, while the cleanup hold is STILL engaged (never released).
        let result = tokio::time::timeout(Duration::from_secs(7), run)
            .await
            .expect("the response blocked on the queued cleanup past the deadline")
            .expect("run task panicked")
            .unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(5_500),
            "the response waited on the held cleanup for {elapsed:?}, beyond the original deadline"
        );
        assert_eq!(
            result.status, "uncertain",
            "a deadline-expired cleanup/reap continuation yields uncertainty, got: {result:?}"
        );
        assert!(
            result
                .reason
                .as_deref()
                .is_some_and(|t| t.contains("uncertain")),
            "the uncertainty is the owner-deadline outcome: {:?}",
            result.reason
        );

        // Release the parked cleanup: the SAME detached owner runs group
        // cleanup and the single reap to completion (it must not hang).
        {
            let (lock, cvar) = &*release;
            let mut released_guard = lock.lock().unwrap();
            *released_guard = true;
            cvar.notify_all();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(origin_main(&fixture), base_main, "push=false moves nothing");
    }

    /// r23 item 3: REAL blocking-pool saturation on the timeout/abort arm.
    /// A granted land runs on a runtime whose ENTIRE blocking pool is held by
    /// latch tasks once the child is running, so the ordered
    /// terminate_group spawn_blocking (and the fence finalizer) stay QUEUED —
    /// they never even start — past the absolute land deadline. The response
    /// must still return at the deadline, uncertain, with NO unbounded
    /// blocking-task join; after the response the SAME detached owner runs the
    /// queued terminate and reaps the leader, and the group reaches ESRCH.
    #[cfg(unix)]
    #[test]
    fn a_saturated_blocking_pool_keeps_the_timeout_response_bounded_and_the_detached_owner_finishes()
     {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            // Small, fully-saturatable pool: every thread below is held by a
            // latch task for the whole deadline window.
            .max_blocking_threads(3)
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async move {
            let fixture = fixture(SLOW_LAND_SCRIPT);
            let base_main = origin_main(&fixture);
            let node = fixture.node.clone();
            let mut p = params(&fixture, "land");
            p.push = false;
            p.job_id = "gjb_blocking_pool_saturated".into();
            p.generation = 1;
            // 0.5 s soft budget + 3.25 s owner allowance = 3.75 s deadline.
            fixture
                .node
                .gate_test_set_land_timeout(Duration::from_millis(500))
                .await;

            let node_for_run = node.clone();
            let run = tokio::spawn(async move { node_for_run.run_gate_typed(p).await });

            // Wait until the granted child is running and its pgid/fence are
            // recorded (the fence write is synchronous before child release).
            let pgid = {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                loop {
                    if let Some(pgid) = node
                        .gate_test_live_land_pgid("gjb_blocking_pool_saturated")
                        .await
                    {
                        break pgid;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the land child never recorded a pgid"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            };
            // Tiny grace so the run reaches its select (fence durable).
            tokio::time::sleep(Duration::from_millis(100)).await;

            // Saturate ALL blocking threads until the latch is released. The
            // terminate/fence closures queue behind these and cannot run.
            let latch = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
            let mut saturators = Vec::new();
            for _ in 0..3 {
                let latch = latch.clone();
                saturators.push(tokio::task::spawn_blocking(move || {
                    let (lock, cvar) = &*latch;
                    let guard = lock.lock().unwrap();
                    // Bounded self-release so a failure that never unblocks the
                    // response cannot park a blocking thread for the whole
                    // binary; the happy path notifies well before this.
                    let (mut guard, _) = cvar
                        .wait_timeout(guard, Duration::from_secs(20))
                        .unwrap_or_else(|poison| poison.into_inner());
                    *guard = true;
                }));
            }

            // Response bounded by the ORIGINAL deadline (~3.75s), even though
            // the terminate/fence blocking tasks never ran while we hold the
            // pool. No unbounded join: it cannot wait for the saturators.
            let started = std::time::Instant::now();
            let result = tokio::time::timeout(Duration::from_secs(8), run)
                .await
                .expect("the response never returned while the pool was saturated")
                .expect("run task panicked")
                .expect("run_gate_typed returned an error");
            let elapsed = started.elapsed();
            assert!(
                elapsed < Duration::from_millis(6_000),
                "the response blocked on the saturated pool for {elapsed:?}"
            );
            assert_eq!(
                result.status, "uncertain",
                "a deadline-expired timeout owner yields uncertainty, got: {result:?}"
            );
            assert!(
                result
                    .reason
                    .as_deref()
                    .is_some_and(|t| t.contains("uncertain")),
                "{:?}",
                result.reason
            );
            // r24 item 7: at response time the group is provably still alive,
            // so the verdict must not claim it "was terminated"; it reports
            // cleanup pending in the owner.
            assert!(
                result
                    .reason
                    .as_deref()
                    .is_some_and(|t| t.contains("cleanup pending in owner")),
                "the detached-cleanup verdict must not claim the group was terminated: {:?}",
                result.reason
            );
            assert!(
                result
                    .error
                    .as_deref()
                    .is_some_and(|t| t.contains("not yet confirmed terminated")),
                "{:?}",
                result.error
            );
            // The group is STILL ALIVE at response time: its terminate was
            // queued behind the saturated pool and has not run. The kernel's
            // portable killpg(pgid, 0) is authoritative here (no Linux /proc,
            // r24 item 5).
            assert!(
                super::gate_test_group_exists(pgid),
                "the terminate closure must still be queued when the uncertain response returns"
            );

            // Release the pool: the SAME detached owner's queued terminate
            // runs (TERM/KILL/ESRCH) and reaps the leader; the bounded fence
            // finalizer then certifies.
            {
                let (lock, cvar) = &*latch;
                let mut done = lock.lock().unwrap();
                *done = true;
                cvar.notify_all();
            }
            for handle in saturators {
                let _ = handle.await;
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(8);
            loop {
                // Portable eventual ESRCH (killpg(pgid, 0)); valid on macOS.
                if !super::gate_test_group_exists(pgid) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the detached owner never terminated/reaped the queued land group {pgid}"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert_eq!(origin_main(&fixture), base_main, "push=false moves nothing");
        });

        rt.shutdown_timeout(Duration::from_secs(5));
    }

    /// r24 item 4: a NORMAL-EXIT land returns a recognized definitive `landed`
    /// report and every ordered owner step (survivor cleanup, the single leader
    /// reap, both pipe drains) finishes inside the deadline — but the fence
    /// FINALIZER's blocking closure stalls (pool contention or fence I/O) past
    /// the absolute deadline. The response must LATCH uncertainty rather than
    /// return the earlier definitive verdict; the detached finalizer still
    /// completes after release and certifies the already-reaped group.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_finalizer_stall_after_a_normal_exit_latches_uncertainty_not_the_landed_verdict() {
        let fixture = fixture(RAW_STDERR_LAND_SCRIPT);
        let base_main = origin_main(&fixture);
        let mut p = params(&fixture, "land");
        p.push = false;
        p.job_id = "gjb_finalize_stall".into();
        p.generation = 1;
        // 0.5 s soft budget + 3.25 s owner allowance = 3.75 s absolute deadline.
        fixture
            .node
            .gate_test_set_land_timeout(Duration::from_millis(500))
            .await;

        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        fixture
            .node
            .gate_test_arm_finalize_hold(Arc::new(CleanupHold {
                entered: entered.clone(),
                release: release.clone(),
            }));

        let node = fixture.node.clone();
        let node_for_run = node.clone();
        let run = tokio::spawn(async move { node_for_run.run_gate_typed(p).await });

        // Wait for the granted child and its recorded pgid.
        let pgid = {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(pgid) = node.gate_test_live_land_pgid("gjb_finalize_stall").await {
                    break pgid;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "no land pgid recorded"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };

        // The finalizer closure ran and parked; every earlier owner step has
        // already completed by the time the response returns below.
        let entered_poll = async {
            while !entered.load(std::sync::atomic::Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), entered_poll)
            .await
            .expect("the fence finalizer never entered its hold");

        let started = std::time::Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(8), run)
            .await
            .expect("the response never returned while the finalizer was held")
            .expect("run task panicked")
            .expect("run_gate_typed returned an error");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(6_000),
            "the response waited on the held finalizer for {elapsed:?}"
        );
        assert_eq!(
            result.status, "uncertain",
            "a definitive verdict is unsafe once fence finalization crosses the deadline, got: \
             {result:?}"
        );
        assert!(
            result
                .reason
                .as_deref()
                .is_some_and(|t| t.contains("finalization")),
            "the uncertainty names the finalizer deadline: {:?}",
            result.reason
        );

        // The owner's ordered cleanup+reap already finished BEFORE the
        // finalizer parked: the group is gone even while the finalizer is still
        // held (proven via the portable killpg existence check).
        assert!(
            !super::gate_test_group_exists(pgid),
            "the owner must have cleaned and reaped the group before finalization"
        );

        // Release the parked finalizer: the detached closure completes its
        // observation and certifies/removes the fence without hanging.
        {
            let (lock, cvar) = &*release;
            let mut done = lock.lock().unwrap();
            *done = true;
            cvar.notify_all();
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!super::gate_test_group_exists(pgid));
        assert_eq!(origin_main(&fixture), base_main, "push=false moves nothing");
    }

    /// r24 item 5: the saturation regression used to prove group liveness by
    /// enumerating `/proc`; ordinary macOS has no `/proc`, so the scan returns
    /// an EMPTY vec for a group that is definitely alive and the test failed
    /// despite correct runtime behavior. With `/proc` made unreadable (the
    /// macOS condition), the portable kernel probe `killpg(pgid, 0)` must still
    /// prove initial existence and eventual ESRCH after the group is reaped.
    #[cfg(unix)]
    #[test]
    fn group_liveness_is_proven_with_portable_killpg_not_proc_enumeration() {
        let _seq = proc_lock();
        let child = spawn_grouped("sh", &["-c", "sleep 5"]);
        let pgid = i32::try_from(child.id()).unwrap();

        // The portable killpg(pgid, 0) probe proves initial existence on
        // EVERY Unix platform (macOS has no /proc at all).
        assert!(super::gate_test_group_exists(pgid), "killpg sees it");

        // Linux-only baseline: there the /proc scan happens to work, and it
        // must agree with the portable probe while the group is live. This
        // assertion can never run on macOS (its /proc cannot be enumerated),
        // so it is compiled on Linux alone; the portable assertions below are
        // the contract on every Unix.
        #[cfg(target_os = "linux")]
        assert!(
            !super::alive_pids_in_group(pgid).is_empty(),
            "Linux baseline: /proc enumerates the live group"
        );

        // Simulate macOS: /proc cannot be read.
        {
            let _missing = MissingProc::set();
            assert!(
                super::alive_pids_in_group(pgid).is_empty(),
                "the /proc scan returns empty for a LIVE group — exactly the macOS condition that \
                 made the old saturation assertions fail"
            );
            assert!(
                super::gate_test_group_exists(pgid),
                "the portable killpg(pgid, 0) probe still proves initial existence without /proc"
            );
        }

        // Eventual ESRCH after KILL and the leader reap.
        {
            use nix::sys::signal::{Signal, killpg};
            use nix::unistd::Pid;
            let _ = killpg(Pid::from_raw(pgid), Some(Signal::SIGKILL));
        }
        reap(child);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if !super::gate_test_group_exists(pgid) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "killpg never reached ESRCH for {pgid}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// r18 item 10: the barrier fake must reject a ZERO-byte read (EOF), not
    /// treat it like a one-byte release — the real `remuda merge`
    /// await_start_barrier exits 4 on `read()==0`. A byte released proceeds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_barrier_fake_rejects_a_zero_byte_release_but_accepts_a_byte() {
        let fixture = fixture(BARRIER_LAND_SCRIPT);
        let dir = fixture.bin.parent().unwrap();
        std::fs::remove_file(dir.join("released.marker")).ok();

        // EOF (zero-byte read): must exit 4 and NOT release.
        let mut eof = std::process::Command::new(&fixture.bin);
        eof.current_dir(dir)
            .env("REMUDA_START_BARRIER_FD", "0")
            .args(["merge", "--land", "--onto", "main"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = eof.output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(4),
            "a zero-byte/EOF barrier read must exit 4 like the real CLI: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !dir.join("released.marker").exists(),
            "EOF must never release the land phase"
        );

        // One release byte: proceeds and writes the marker.
        std::fs::remove_file(dir.join("released.marker")).ok();
        let mut ok = std::process::Command::new(&fixture.bin);
        ok.current_dir(dir)
            .env("REMUDA_START_BARRIER_FD", "0")
            .args(["merge", "--land", "--onto", "main"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = ok.spawn().unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(&[1u8]).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "a real release byte must run the land phase: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            dir.join("released.marker").exists(),
            "a one-byte release must pass the barrier"
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

    /// r23 item 2: a subprocess writes `gate: ` + 600 KiB of letters and is
    /// then killed by the gate timeout. The over-long token must NOT be parsed
    /// as a step name (no 600 KiB RunTrace key / GateRunLog.step) and every
    /// retained field — step, headline, each tail/summary line — stays
    /// bounded, so the whole verdict serializes well under the evidence cap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_oversized_marker_line_on_a_timeout_cannot_escape_the_field_bounds() {
        let huge = 600 * 1024;
        let script = format!(
            "printf 'gate: ' >&2\nhead -c {huge} /dev/zero | tr '\\0' 'a' >&2\nprintf '\\n' >&2\nsleep 30\n"
        );
        let fixture = fixture(&script);
        let mut p = params(&fixture, "verify");
        p.gate_timeout_secs = 1;
        let result = fixture.node.run_gate_typed(p).await.unwrap();
        assert_eq!(result.status, "failed", "{:?}", result.error);
        let log = result
            .run_log
            .as_ref()
            .expect("a timeout attaches bounded failure evidence");
        assert!(
            log.step.len() <= super::MAX_STEP_NAME_BYTES,
            "the 600 KiB marker never became a step name; got {} bytes: {log:?}",
            log.step.len()
        );
        assert_eq!(
            log.step, "gate",
            "the rejected marker leaves the runner-fallback step, not the huge letters"
        );
        assert!(
            log.headline.len() <= super::MAX_HEADLINE_BYTES,
            "headline {} bytes",
            log.headline.len()
        );
        for line in log.tail.iter().chain(log.summary.iter()) {
            assert!(
                line.len() <= super::MAX_LINE_CHARS,
                "a retained line is {} bytes: {line:?}",
                line.len()
            );
        }
        assert!(
            result
                .failed_step
                .as_deref()
                .is_none_or(|step| step.len() <= super::MAX_STEP_NAME_BYTES),
            "failedStep is bounded: {:?}",
            result.failed_step
        );
        let encoded = serde_json::to_vec(&result).unwrap();
        assert!(
            encoded.len() < 512 * 1024,
            "the whole verdict serialized to {} bytes",
            encoded.len()
        );
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

    /// r23 item 2: a marker with an over-long name is NOT a step marker, so a
    /// subprocess emitting `gate: ` + hundreds of KiB of letters cannot create
    /// an unbounded RunTrace key / GateRunLog.step.
    #[test]
    fn an_overlong_step_marker_name_is_rejected() {
        let at_cap = "a".repeat(super::MAX_STEP_NAME_BYTES);
        assert_eq!(
            super::parse_step_marker(&format!("gate: {at_cap}")),
            Some((at_cap.clone(), false)),
            "a name exactly at the cap is accepted"
        );
        let over = "a".repeat(super::MAX_STEP_NAME_BYTES + 1);
        assert!(
            super::parse_step_marker(&format!("gate: {over}")).is_none(),
            "a name one byte over the cap is rejected"
        );
        // The exact regression shape: 600 KiB of letters.
        let huge = "a".repeat(600 * 1024);
        assert!(super::parse_step_marker(&format!("gate: {huge}")).is_none());
    }

    /// r23 item 2: the field clipper keeps in-cap text verbatim and bounds
    /// over-cap text on a UTF-8 boundary with a cut marker.
    #[test]
    fn clip_field_bounds_on_a_utf8_boundary() {
        assert_eq!(super::clip_field("short".into(), 16), "short");
        let clipped = super::clip_field("a".repeat(100), 10);
        assert!(clipped.len() <= 10, "{clipped:?}");
        assert!(clipped.ends_with('…'));
        // A multibyte boundary is never split.
        let clipped = super::clip_field("€".repeat(20), 7);
        assert!(clipped.len() <= 7, "{clipped:?}");
        assert!(std::str::from_utf8(clipped.as_bytes()).is_ok());
    }

    /// r24 item 6: clipping a captured line to the field byte cap IS
    /// truncation. One stderr line of 2,000 '€' (6,000 bytes) fits the
    /// character-count and ring budgets, so the ring's own drop marker stays
    /// false; the field clipper removes bytes and BOTH the failure and kept
    /// log builders must surface that in `GateRunLog.truncated`.
    #[test]
    fn byte_clipping_a_captured_multibyte_line_marks_the_log_truncated() {
        let euro_line = "€".repeat(2_000);
        assert_eq!(euro_line.chars().count(), 2_000);
        assert_eq!(euro_line.len(), 6_000);
        let mut trace = super::RunTrace::new();
        trace.push("gate: cargo-test".to_owned());
        trace.push(euro_line.clone());

        let failure = super::build_failure_log(&trace, "cargo-test", 1, None, None);
        assert!(
            failure.truncated,
            "the failure log lost bytes to field clipping but claims untruncated"
        );
        assert!(
            failure
                .tail
                .iter()
                .all(|line| line.len() <= super::MAX_STEP_FIELD_BYTES),
            "every tail line stays within the field cap"
        );
        assert!(
            failure.tail.iter().any(|line| line.ends_with('…')),
            "the clipped line carries the cut marker: {:?}",
            failure.tail
        );

        let kept = super::build_kept_log(&trace, "passed; log retained by --keep-logs");
        assert!(
            kept.truncated,
            "the kept log lost bytes to field clipping but claims untruncated"
        );
        assert!(kept.tail.iter().any(|line| line.ends_with('…')));

        // An in-cap line leaves the marker false.
        let mut clean = super::RunTrace::new();
        clean.push("gate: cargo-test".to_owned());
        clean.push("all good".to_owned());
        let clean_log = super::build_failure_log(&clean, "cargo-test", 1, None, None);
        assert!(!clean_log.truncated, "{clean_log:?}");
    }

    /// r23 item 4: a valid multi-byte character split ACROSS reads must not be
    /// corrupted into replacement characters in the streamed live log.
    #[test]
    fn lossy_framer_keeps_a_multibyte_char_split_across_reads_intact() {
        // "€" is E2 82 AC; read 1 returns only E2, read 2 returns 82 AC 0A.
        let mut lines = Vec::new();
        let mut framer = super::LossyLineFramer::new();
        framer.push(&[0xE2], |l| lines.push(l));
        assert!(
            lines.is_empty(),
            "a lone lead byte emits no line: {lines:?}"
        );
        framer.push(&[0x82, 0xAC, b'\n'], |l| lines.push(l));
        assert_eq!(
            lines,
            vec!["€".to_owned()],
            "split char corrupted: {lines:?}"
        );
        framer.finish(|l| lines.push(l));
        assert_eq!(
            lines,
            vec!["€".to_owned()],
            "no spurious EOF line: {lines:?}"
        );
    }

    #[test]
    fn lossy_framer_handles_multiple_lines_cr_and_unterminated_tail() {
        let mut lines = Vec::new();
        let mut framer = super::LossyLineFramer::new();
        // Several lines in one chunk, CRLF, and an unterminated final line.
        framer.push(b"one\r\ntwo\nth", |l| lines.push(l));
        framer.finish(|l| lines.push(l));
        assert_eq!(
            lines,
            vec!["one".to_owned(), "two".to_owned(), "th".to_owned()],
            "{lines:?}"
        );
    }

    #[test]
    fn lossy_framer_lossily_decodes_a_genuinely_invalid_completed_line() {
        // The r22 RAW_STDERR shape: an invalid lead byte inside a COMPLETE
        // line becomes U+FFFD, but it does not terminate framing or corrupt
        // the following valid line.
        let mut lines = Vec::new();
        let mut framer = super::LossyLineFramer::new();
        framer.push(b"bad-\xFF-end\nok\n", |l| lines.push(l));
        framer.finish(|_| {});
        assert_eq!(lines[0], "bad-\u{FFFD}-end", "{lines:?}");
        assert_eq!(lines[1], "ok", "{lines:?}");
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

    /// r20 item 1 / R8(c): the portable non-reaping observer distinguishes the
    /// three cases the old nix form conflated:
    ///  - a LIVE leader -> StillRunning, and WNOWAIT leaves it unreaped;
    ///  - an exited leader -> Exited while its zombie is STILL unreaped (the
    ///    owner's own try_wait then collects it — proving it was left behind);
    ///  - after that reap -> ECHILD -> None, NEVER "exited" (an observation
    ///    error must not authorize a signal against a possibly-reused pid).
    #[cfg(unix)]
    #[test]
    fn waitid_non_reaping_distinguishes_live_exited_and_error() {
        let mut live = spawn_grouped("sleep", &["47.42"]);
        let pid = live.id() as i32;
        for _ in 0..5 {
            match super::leader_exit_observed_unreaped(pid) {
                Some(super::LeaderExitObservation::StillRunning) => {}
                other => panic!("a live leader must read StillRunning, got {other:?}"),
            }
            assert!(
                live.try_wait().unwrap().is_none(),
                "WNOWAIT must never consume the live leader"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = live.kill();
        let mut saw_exit = false;
        let kill_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < kill_deadline {
            if matches!(
                super::leader_exit_observed_unreaped(pid),
                Some(super::LeaderExitObservation::Exited)
            ) {
                saw_exit = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(saw_exit, "the killed leader must eventually report Exited");
        // The Exited observation was WNOWAIT: the zombie remains until THIS
        // owner reaps it.
        let status = live
            .try_wait()
            .expect("try_wait after the WNOWAIT observation")
            .expect("the exited leader is still unreaped, waiting for its owner");
        assert!(!status.success(), "the leader died from the test's SIGKILL");
        // Reaped: waitid now answers ECHILD, which is an observation ERROR
        // (None), never an exit.
        assert!(
            super::leader_exit_observed_unreaped(pid).is_none(),
            "ECHILD after the reap must be None, not an exit observation"
        );
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
        let path = super::land_fence_path(dir.path(), "ghost", 1);
        std::fs::write(
            &path,
            serde_json::json!({
                "jobId": "ghost",
                "generation": 1,
                "pgid": pgid,
                "startTick": 42,
                "mergeSha": MERGE_A,
            })
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
            super::land_fence_path(fence, "job-x", 1),
            serde_json::json!({
                "jobId": "job-x",
                "generation": 1,
                "pgid": pgid,
                "startTick": recorded,
                "mergeSha": MERGE_A,
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
        assert!(super::land_fence_path(fence, "job-x", 1).is_file());
        let _ = stranger.kill();
        reap(stranger);
    }

    /// r20 item 2 / R8(a): after a restart the Node does NOT OWN an orphaned
    /// land group even when the live leader's durable start identity EXACTLY
    /// matches the fence record. It must send no TERM and no KILL (a signal
    /// there could strike a recycled group after the leader is reaped), KEEP
    /// the fence, and leave the Landing Unknown for operator resolution.
    #[test]
    fn startup_fence_never_signals_a_live_orphan_even_with_a_matching_identity() {
        let _seq = proc_lock();
        let mut child = spawn_grouped("sleep", &["47.31"]);
        let pgid = child.id() as i32;
        let identity = {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                if let Some(value) = super::sample_leader_identity(pgid) {
                    break value;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "never sampled the leader"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let dir = tempfile::TempDir::new().unwrap();
        let fence = dir.path();
        let mut record = serde_json::json!({
            "jobId": "job-orphan-live",
            "generation": 1,
            "pgid": pgid,
            "mergeSha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        });
        identity.write(&mut record);
        std::fs::write(
            super::land_fence_path(fence, "job-orphan-live", 1),
            record.to_string(),
        )
        .unwrap();

        super::fence_orphaned_land_groups(fence);

        // The orphan is alive AFTER the scan — the old code TERM/KILLed it on
        // the exact identity match.
        assert!(
            child.try_wait().unwrap().is_none(),
            "a live orphan with a matching identity must never be signalled after a restart"
        );
        assert!(
            super::land_group_exists(pgid),
            "the group was signalled despite R8(a)"
        );
        // The fence is retained: the Landing stays Unknown for the operator.
        assert!(
            super::land_fence_path(fence, "job-orphan-live", 1).is_file(),
            "the live orphan's fence must be kept, never certified dead"
        );
        assert!(
            super::read_land_group_dead_cert(
                fence,
                "job-orphan-live",
                1,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            )
            .is_none(),
            "no death certificate may be written for a live group"
        );
        let _ = child.kill();
        reap(child);
    }

    #[test]
    fn startup_fence_removes_the_record_only_when_death_is_esrch() {
        let _seq = proc_lock();
        // Established dead: leader killed+reaped and the group gone.
        let mut child = spawn_grouped("sleep", &["1"]);
        let pgid = child.id() as i32;
        let identity = super::sample_leader_identity(pgid).expect("identity");
        let dir = tempfile::TempDir::new().unwrap();
        let path = super::land_fence_path(dir.path(), "dead", 1);
        let mut record = serde_json::json!({
            "jobId": "dead",
            "generation": 1,
            "pgid": pgid,
            "mergeSha": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        });
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
            super::read_land_group_dead_cert(
                dir.path(),
                "dead",
                1,
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            )
            .is_some(),
            "authoritative ESRCH at startup must leave a death certificate"
        );
        assert_eq!(
            super::land_ledger_liveness(
                dir.path(),
                "dead",
                Some(1),
                Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
            ),
            super::LandPushLiveness::VerifiedDead
        );
        // A different merge sha never matches the certificate.
        assert_eq!(
            super::land_ledger_liveness(
                dir.path(),
                "dead",
                Some(1),
                Some("cccccccccccccccccccccccccccccccccccccccc")
            ),
            super::LandPushLiveness::Unknown,
            "the certificate is scoped to the exact granted merge"
        );
        assert_eq!(
            super::land_ledger_liveness(
                dir.path(),
                "other-job",
                Some(1),
                Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
            ),
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
        let path = super::land_fence_path(dir.path(), "orphan", 1);
        let mut record = serde_json::json!({
            "jobId": "orphan",
            "generation": 1,
            "pgid": pgid,
            "mergeSha": MERGE_A,
        });
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
        super::write_land_fence(
            dir,
            job,
            2,
            live_pgid,
            &super::LeaderIdentity::StartTick(1),
            MERGE_B,
        )
        .unwrap();
        // The leftover attempt-1 certificate for the DEAD pgid.
        let stale = {
            let mut v = serde_json::json!({
                "jobId": job,
                "generation": 1,
                "pgid": dead_pgid,
                "mergeSha": MERGE_A,
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
            super::land_fence_path(dir, job, 2).exists(),
            "the live replacement fence must not be unlinked"
        );
        // No certificate was derived for the replacement snapshot.
        let cert =
            super::read_land_group_dead_cert(dir, job, 1, MERGE_A).expect("cert still present");
        assert_eq!(cert["pgid"], json!(dead_pgid), "{cert}");
        assert_eq!(cert["generation"], json!(1), "{cert}");

        let _ = live.kill();
        reap(live);
    }

    /// r36 item 1: two finalizers writing certificates CONCURRENTLY (different
    /// jobs, same Node) park with their temp fully written, then both rename.
    /// Each certificate must land under its own qualified path with ITS OWN
    /// identity; a shared temp name let the later write's contents reach the
    /// earlier job's certificate and fail the later rename, leaving one
    /// attempt with invalid evidence and its fence removed.
    #[test]
    fn concurrent_certificate_writes_never_share_a_temp_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();

        let cert = |job: &'static str, pgid: i32, merge: &'static str| {
            let mut v = serde_json::json!({
                "jobId": job,
                "generation": 1,
                "pgid": pgid,
                "mergeSha": merge,
                "diedAt": 1_700_000_000_000u64,
                "cause": "killpg-esrch",
            });
            super::LeaderIdentity::StartTick(pgid as u64).write(&mut v);
            v
        };
        let cert_a = cert("gjb_r36_jobA", 4141, MERGE_A);
        let cert_b = cert("gjb_r36_jobB", 4242, MERGE_B);

        // r37 item 2: the barrier is scoped to THIS unique ledger dir and the
        // two intended jobs, so an unrelated parallel test's certificate write
        // cannot count as an arrival; the RAII guard disarms/releases even on
        // assertion failure. r39 item 2: arming also takes the cross-test
        // serialization lock, held through the writer joins.
        let jobs = vec!["gjb_r36_jobA".to_owned(), "gjb_r36_jobB".to_owned()];
        let hold = arm_cert_temp_hold(dir.clone(), jobs);
        let dir_a = dir.clone();
        let ca = cert_a.clone();
        let writer_a = std::thread::spawn(move || {
            super::write_qualified_dead_cert_bytes(&dir_a, &ca, "gjb_r36_jobA", 1)
        });
        let dir_b = dir.clone();
        let cb = cert_b.clone();
        let writer_b = std::thread::spawn(move || {
            super::write_qualified_dead_cert_bytes(&dir_b, &cb, "gjb_r36_jobB", 1)
        });

        // r37 item 2: a FOREIGN writer (different ledger dir AND job) must pass
        // the scoped barrier without parking: it completes on its own while A/B
        // are held and never increments the barrier's arrival count.
        let foreign_dir = tempfile::TempDir::new().unwrap();
        let foreign_cert = cert("gjb_r36_foreign", 4343, MERGE_A);
        let fdir = foreign_dir.path().to_path_buf();
        let writer_foreign = std::thread::spawn(move || {
            super::write_qualified_dead_cert_bytes(&fdir, &foreign_cert, "gjb_r36_foreign", 1)
        });

        // Deterministic barrier: wait until BOTH scoped temps exist and BOTH
        // scoped writers are parked between write and rename.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while hold.entered() < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            hold.entered(),
            2,
            "both scoped writers must park before either rename"
        );
        // The foreign writer did NOT park: it finished (its qualified cert
        // exists) while A/B are still held, and the arrival count is exactly 2.
        writer_foreign
            .join()
            .unwrap()
            .expect("foreign writer completes without parking");
        assert!(
            super::land_dead_cert_path(foreign_dir.path(), "gjb_r36_foreign", 1).exists(),
            "the foreign write completed independently while A/B were parked"
        );
        assert_eq!(
            hold.entered(),
            2,
            "a foreign-dir write must not count as a barrier arrival"
        );
        let temps: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".dead-") && name.ends_with(".tmp"))
            .collect();
        assert_eq!(
            temps.len(),
            2,
            "each writer must own a distinct temp file, found {temps:?}"
        );
        // Release both writers.
        hold.release();
        writer_a.join().unwrap().expect("writer A succeeds");
        writer_b.join().unwrap().expect("writer B succeeds");
        // `hold` drops here: disarms the global barrier and releases the
        // cross-test serialization lock (also on an early assertion failure).

        // Each qualified certificate carries the correct attempt identity —
        // the old shared-temp bug put B's contents at A's path (or vice-versa).
        let got_a = super::read_land_group_dead_cert(&dir, "gjb_r36_jobA", 1, MERGE_A)
            .expect("A's certificate must be its own identity");
        let got_b = super::read_land_group_dead_cert(&dir, "gjb_r36_jobB", 1, MERGE_B)
            .expect("B's certificate must be its own identity");
        assert_eq!(got_a["pgid"], json!(4141), "{got_a}");
        assert_eq!(got_a["jobId"], json!("gjb_r36_jobA"), "{got_a}");
        assert_eq!(got_b["pgid"], json!(4242), "{got_b}");
        assert_eq!(got_b["jobId"], json!("gjb_r36_jobB"), "{got_b}");
        assert_eq!(
            super::land_ledger_liveness(&dir, "gjb_r36_jobA", Some(1), Some(MERGE_A)),
            super::LandPushLiveness::VerifiedDead
        );
        assert_eq!(
            super::land_ledger_liveness(&dir, "gjb_r36_jobB", Some(1), Some(MERGE_B)),
            super::LandPushLiveness::VerifiedDead
        );
        // No temp files leak after successful writes.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".dead-") && name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files leaked: {leftovers:?}");
    }

    /// r39 item 2: arming the cert-temp barrier takes a process-wide
    /// serialization lock held until the returned guard drops, so the two
    /// barrier tests can never occupy the single CERT_TEMP_HOLD slot at once.
    /// A second arming from another thread blocks while the first guard lives.
    #[test]
    fn cert_temp_hold_arming_is_serialized_across_tests() {
        let d1 = tempfile::TempDir::new().unwrap();
        let d2 = tempfile::TempDir::new().unwrap();

        // A latch telling the first guard holder when to release.
        struct Gate {
            acquired: std::sync::atomic::AtomicBool,
            release: std::sync::Mutex<bool>,
            cvar: std::sync::Condvar,
        }
        let gate = std::sync::Arc::new(Gate {
            acquired: std::sync::atomic::AtomicBool::new(false),
            release: std::sync::Mutex::new(false),
            cvar: std::sync::Condvar::new(),
        });

        // First test arms and holds until signalled.
        let g = gate.clone();
        let first = std::thread::spawn(move || {
            let _guard = arm_cert_temp_hold(d1.path().to_path_buf(), vec!["gjb_first".to_owned()]);
            g.acquired.store(true, std::sync::atomic::Ordering::Release);
            let mut released = g.release.lock().unwrap();
            while !*released {
                released = g.cvar.wait(released).unwrap();
            }
        });
        // Wait until the first guard is held.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !gate.acquired.load(std::sync::atomic::Ordering::Acquire)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(gate.acquired.load(std::sync::atomic::Ordering::Acquire));

        // A second arming must BLOCK while the first guard is alive.
        let second_started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let second_acquired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ss = second_started.clone();
        let sa = second_acquired.clone();
        let d2p = d2.path().to_path_buf();
        let second = std::thread::spawn(move || {
            ss.store(true, std::sync::atomic::Ordering::Release);
            let _guard = arm_cert_temp_hold(d2p, vec!["gjb_second".to_owned()]);
            sa.store(true, std::sync::atomic::Ordering::Release);
            // keep it held briefly so the test can observe completion
            std::thread::sleep(Duration::from_millis(100));
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !second_started.load(std::sync::atomic::Ordering::Acquire)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        // Give the second thread a clear chance to arm if it were NOT serialized.
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !second_acquired.load(std::sync::atomic::Ordering::Acquire),
            "a second cert-temp arming must block while the first guard is held"
        );

        // Release the first guard; the second now arms.
        {
            let mut released = gate.release.lock().unwrap();
            *released = true;
            gate.cvar.notify_all();
        }
        first.join().unwrap();
        second.join().unwrap();
        assert!(
            second_acquired.load(std::sync::atomic::Ordering::Acquire),
            "the second arming proceeds once the first guard drops"
        );
    }

    /// r37 item 4: crash-abandoned certificate temp files are reclaimed at
    /// startup under exclusive ledger ownership, while real certificates and
    /// live fences are kept and the temp was never readable as evidence.
    #[test]
    fn startup_reclaims_abandoned_certificate_temps_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        let job = "gjb_r37_tmpjob";

        // Two abandoned temps: the current unique scheme and the older
        // per-pid/pgid scheme (a Node crashed after sync, before rename).
        let tmp_new = dir.join(format!(
            ".dead-{}-g1.{}-{:09x}-1.tmp",
            job,
            std::process::id(),
            123u32
        ));
        let tmp_old = dir.join(".dead-g1-999-4242.tmp");
        std::fs::write(&tmp_new, b"{\"jobId\":\"crashed\"}").unwrap();
        std::fs::write(&tmp_old, b"{\"jobId\":\"crashed-old\"}").unwrap();

        // A real qualified certificate and a live fence must survive.
        let mut record = serde_json::json!({
            "jobId": job,
            "generation": 1,
            "pgid": 4001,
            "mergeSha": MERGE_A,
        });
        super::LeaderIdentity::StartTick(7).write(&mut record);
        super::write_land_group_dead_cert(dir, &record, 4001).unwrap();
        super::write_land_fence(
            dir,
            "gjb_r37_live",
            1,
            5001,
            &super::LeaderIdentity::StartTick(8),
            MERGE_B,
        )
        .unwrap();

        // An unrelated dotfile / non-temp must not be touched.
        let unrelated = dir.join(".keep");
        std::fs::write(&unrelated, b"x").unwrap();

        // The decisive startup path: opening a GateRegistry (exclusive ledger
        // ownership, before any writer starts) reclaims the leftovers.
        let _registry = super::GateRegistry::open(dir.to_path_buf());
        assert!(!tmp_new.exists(), "new-scheme temp removed on startup");
        assert!(!tmp_old.exists(), "old-scheme temp removed on startup");
        assert!(unrelated.exists(), "unrelated dotfile kept");

        // The direct helper reports the count on a second call (idempotent:
        // nothing remains to remove).
        assert_eq!(
            super::reclaim_abandoned_cert_temps(dir),
            0,
            "startup already reclaimed the leftovers"
        );

        // The real certificate is intact and readable; the abandoned temp was
        // never evidence for any job (no temp-prefixed cert reader exists).
        assert!(
            super::read_land_group_dead_cert(dir, job, 1, MERGE_A).is_some(),
            "real certificate preserved"
        );
        assert_eq!(
            super::land_ledger_liveness(dir, "crashed", Some(1), Some(MERGE_A)),
            super::LandPushLiveness::Unknown,
            "an abandoned temp never attests death"
        );
        assert_eq!(
            super::land_ledger_liveness(dir, job, Some(1), Some(MERGE_A)),
            super::LandPushLiveness::VerifiedDead,
            "the qualified certificate still attests death"
        );
    }

    /// r38 item 1: a certificate writer that is still alive (a detached
    /// finalizer parked before rename) holds the ledger SHARED flock; startup
    /// reclamation must NOT take EXCLUSIVE ownership and must NOT unlink its
    /// temp. Once the writer finishes and installs its certificate, the next
    /// startup pass acquires EXCLUSIVE and reclaims unrelated crash leftovers.
    #[test]
    fn startup_does_not_reclaim_an_active_writers_cert_temp() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        let job = "gjb_r38_active";

        let mut record = serde_json::json!({
            "jobId": job,
            "generation": 1,
            "pgid": 6001,
            "mergeSha": MERGE_A,
            "diedAt": 1_700_000_000_000u64,
            "cause": "killpg-esrch",
        });
        super::LeaderIdentity::StartTick(11).write(&mut record);

        // A crash-leftover temp a PREVIOUS process abandoned (must be reclaimed
        // only once no writer is active).
        let leftover = dir.join(format!(
            ".dead-{}-g1.{}-{:09x}-9.tmp",
            job,
            std::process::id(),
            456u32
        ));
        std::fs::write(&leftover, b"{\"jobId\":\"crashed\"}").unwrap();

        // Park the writer after its temp is written (it is holding the ledger
        // SHARED flock the whole time). r39 item 2: arming also serializes
        // against the other cert-barrier test for the whole parked window.
        let hold = arm_cert_temp_hold(dir.clone(), vec![job.to_owned()]);
        let writer_dir = dir.clone();
        let writer_record = record.clone();
        let writer = std::thread::spawn(move || {
            super::write_qualified_dead_cert_bytes(&writer_dir, &writer_record, job, 1)
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while hold.entered() < 1 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            hold.entered(),
            1,
            "the writer must park while holding the shared ledger flock"
        );

        // Simulated restart while the detached writer is still active. Startup
        // tries the EXCLUSIVE non-blocking flock, is refused, and must skip
        // reconciliation entirely.
        let _registry = super::GateRegistry::open(dir.clone());

        // Both the active writer's own temp AND the old leftover survive: the
        // active temp would otherwise be unlinked under its pending rename.
        let active_temps: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".dead-") && name.ends_with(".tmp"))
            .collect();
        assert!(
            active_temps
                .iter()
                .any(|name| name != &leftover.file_name().unwrap().to_string_lossy()),
            "the active writer's temp must remain: {active_temps:?}"
        );
        assert!(
            leftover.exists(),
            "the leftover is kept while a writer is active"
        );
        // No certificate was installed for the crashed leftover.
        assert_eq!(
            super::land_ledger_liveness(&dir, "crashed", Some(1), Some(MERGE_A)),
            super::LandPushLiveness::Unknown
        );

        // Let the writer finish: its rename installs the qualified certificate.
        hold.release();
        writer
            .join()
            .unwrap()
            .expect("active writer renames successfully");
        assert!(
            super::read_land_group_dead_cert(&dir, job, 1, MERGE_A).is_some(),
            "the active writer's certificate is installed"
        );

        // Next restart: no writer holds the ledger, EXCLUSIVE is acquired and
        // the leftover is reclaimed; the freshly installed certificate stays.
        let _registry2 = super::GateRegistry::open(dir.clone());
        assert!(
            !leftover.exists(),
            "the leftover is reclaimed once the ledger is free"
        );
        let temps_left: usize = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.starts_with(".dead-") && n.ends_with(".tmp")
            })
            .count();
        assert_eq!(temps_left, 0, "no temp files remain after reclamation");
        assert!(
            super::read_land_group_dead_cert(&dir, job, 1, MERGE_A).is_some(),
            "the installed certificate survives reclamation"
        );
    }

    /// r39 item 1: a RUNTIME legacy-cert migration (read-only probe lazily
    /// promoting a `dead-<job>.json` to the qualified name) holds the ledger
    /// SHARED flock for its rename. Reopening the ledger while that migration is
    /// parked must NOT reclaim its temp; after release the qualified cert is
    /// installed and the legacy source is removed.
    #[test]
    fn runtime_legacy_cert_migration_holds_ledger_flock_across_reopen() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        let job = "gjb_r39_legacymig";

        // A valid pre-r34 legacy certificate (no mergeSha, legacy-fence provenance
        // not required for the read-side migration).
        let legacy_record = serde_json::json!({
            "jobId": job,
            "generation": 1,
            "pgid": 6101,
            "startTick": 13,
            "diedAt": 1_700_000_000_000u64,
            "cause": "killpg-esrch",
        });
        std::fs::write(
            super::legacy_land_dead_cert_path(&dir, job),
            legacy_record.to_string(),
        )
        .unwrap();
        // An unrelated crash leftover that EXCLUSIVE startup would reclaim once
        // no writer is active — it must survive while the migration is parked.
        let leftover = dir.join(format!(
            ".dead-other-g1.{}-{:09x}-2.tmp",
            std::process::id(),
            789u32
        ));
        std::fs::write(&leftover, b"{}").unwrap();

        // Park the migration mid-rename (the barrier matches the migrated job).
        let hold = arm_cert_temp_hold(dir.clone(), vec![job.to_owned()]);
        let read_dir = dir.clone();
        let reader = std::thread::spawn(move || {
            // Triggers migrate_legacy_dead_cert under the SHARED flock.
            super::read_land_group_dead_cert(&read_dir, job, 1, MERGE_A)
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while hold.entered() < 1 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            hold.entered(),
            1,
            "the legacy migration parks holding SHARED"
        );

        // Reopen while the migration is mid-rename: EXCLUSIVE is refused and
        // reclamation must not touch its temp (or the leftover).
        let _registry = super::GateRegistry::open(dir.clone());
        let temps_during: usize = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.starts_with(".dead-") && n.ends_with(".tmp")
            })
            .count();
        assert!(
            temps_during >= 1,
            "the parked migration's temp must survive reopen (found {temps_during})"
        );
        assert!(leftover.exists(), "reclamation skipped; leftover kept");
        assert!(
            !super::land_dead_cert_path(&dir, job, 1).exists(),
            "qualified cert not installed while migration is parked"
        );

        // Release → migration finishes: qualified cert installed, legacy gone.
        hold.release();
        let cert = reader
            .join()
            .unwrap()
            .expect("migration returns the migrated certificate");
        assert_eq!(cert["jobId"], json!(job));
        assert!(
            super::land_dead_cert_path(&dir, job, 1).exists(),
            "qualified cert installed"
        );
        assert!(
            !super::legacy_land_dead_cert_path(&dir, job).exists(),
            "legacy source removed after migration"
        );

        // Next reopen (no active migration) reclaims the leftover.
        let _registry2 = super::GateRegistry::open(dir.clone());
        assert!(
            !leftover.exists(),
            "leftover reclaimed once the ledger is free"
        );
    }

    /// r17 item 1: `certify_dead_and_remove_fence` refuses when the fence on
    /// disk is no longer the observed snapshot — it never certifies a different
    /// r18 item 9: an error removing the PREVIOUS death certificate other than
    /// NotFound (here the cert path is a non-empty directory → IsADirectory)
    /// must propagate; the new fence is not written (no silent stale cert).
    #[test]
    fn write_fence_propagates_a_non_notfound_old_cert_removal_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        let job = "gjb_certunlink";
        let cert = super::land_dead_cert_path(dir, job, 1);
        // The cert path is a non-empty DIRECTORY: remove_file fails with
        // IsADirectory/EISDIR, not NotFound.
        std::fs::create_dir_all(cert.join("inside")).unwrap();
        let error = super::write_land_fence(
            dir,
            job,
            1,
            1234,
            &super::LeaderIdentity::StartTick(7),
            MERGE_A,
        )
        .expect_err("a non-NotFound cert removal error must propagate");
        assert_ne!(
            error.kind(),
            std::io::ErrorKind::NotFound,
            "the induced failure is a directory, not a missing file: {error}"
        );
        // The new fence was NOT written, so the grant failure is honest rather
        // than leaving the stale directory cert in place behind a new fence.
        assert!(
            !super::land_fence_path(dir, job, 2).exists(),
            "the fence must not be written when the old cert cannot be removed"
        );
    }

    /// A refused/changed snapshot: a later fence for a DIFFERENT
    /// (replacement) attempt must never be certified from a stale observation.
    #[test]
    fn certify_refuses_when_the_on_disk_fence_is_a_different_snapshot() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        let job = "gjb_snapshot";
        // On disk: attempt 2 at pgid 4242 …
        super::write_land_fence(
            dir,
            job,
            2,
            4242,
            &super::LeaderIdentity::StartTick(22),
            MERGE_B,
        )
        .unwrap();
        // … plus attempt 1's OWN qualified fence, but with a snapshot the
        // observation no longer matches (recycled pgid/identity).
        super::write_land_fence(
            dir,
            job,
            1,
            9999,
            &super::LeaderIdentity::StartTick(33),
            MERGE_A,
        )
        .unwrap();
        // Observed (stale): attempt 1 at a different pgid/identity.
        let observed = {
            let mut v = serde_json::json!({
                "jobId": job,
                "generation": 1,
                "pgid": 4141,
                "mergeSha": MERGE_A,
            });
            super::LeaderIdentity::StartTick(11).write(&mut v);
            v
        };
        let certified = super::certify_dead_and_remove_fence(dir, &observed, 4141).unwrap();
        assert!(
            !certified,
            "a mismatched on-disk snapshot must not be certified"
        );
        assert!(
            super::land_fence_path(dir, job, 2).exists(),
            "the current fence stays"
        );
        assert!(
            super::land_fence_path(dir, job, 1).exists(),
            "the mismatched same-generation fence is kept too"
        );
        assert!(
            super::read_land_group_dead_cert(dir, job, 1, MERGE_A).is_none(),
            "no certificate may be written for the wrong snapshot"
        );
    }

    /// r34 item 1 (HIGH): attempt-qualified IMMUTABLE fence/cert paths. Attempt
    /// A's finalizer passes its own snapshot comparison and parks; attempt B
    /// then installs its own fence for the same job (generation 2). When A
    /// resumes it must write ONLY A's certificate (`dead-<job>-g1.json`),
    /// unlink ONLY A's fence (`land-<job>-g1.json`), and leave B's LIVE
    /// `land-<job>-g2.json` untouched. A's certificate must never be accepted
    /// as death evidence for B's generation.
    #[test]
    fn a_detached_finalizer_cannot_touch_a_newer_attempts_fence_or_certify_it() {
        let _seq = proc_lock();
        let dir = tempfile::TempDir::new().unwrap();
        let dirp = dir.path();
        let job = "gjb_r34_attempts";
        let pgid_a = 4141;
        let pgid_b = 4242;
        // Attempt A fence (gen 1).
        super::write_land_fence(
            dirp,
            job,
            1,
            pgid_a,
            &super::LeaderIdentity::StartTick(7),
            MERGE_A,
        )
        .unwrap();
        let observed_a = serde_json::json!({
            "jobId": job, "generation": 1, "pgid": pgid_a, "startTick": 7,
            "mergeSha": MERGE_A,
        });

        // Arm the post-compare park and start A's finalizer in a thread.
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        super::arm_certify_compare_hold(Arc::new(CleanupHold {
            entered: entered.clone(),
            release: release.clone(),
        }));
        let dir_owned = dirp.to_path_buf();
        let observed_owned = observed_a.clone();
        let handle = std::thread::spawn(move || {
            super::certify_dead_and_remove_fence(&dir_owned, &observed_owned, pgid_a)
        });
        // Wait until A passed the compare and is parked pre-unlink.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !entered.load(std::sync::atomic::Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "A never parked");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Attempt B installs its own attempt-qualified fence while A is parked.
        super::write_land_fence(
            dirp,
            job,
            2,
            pgid_b,
            &super::LeaderIdentity::StartTick(22),
            MERGE_B,
        )
        .unwrap();
        let b_fence = super::land_fence_path(dirp, job, 2);
        let b_fence_before = std::fs::read_to_string(&b_fence).unwrap();
        assert!(b_fence_before.contains("\"generation\":2"));

        // Resume A.
        {
            let (lock, cvar) = &*release;
            let mut done = lock.lock().unwrap();
            *done = true;
            cvar.notify_all();
        }
        let certified = handle.join().expect("finalizer thread panicked").unwrap();
        assert!(certified, "A certifies its own (gen1) attempt");

        // B's LIVE fence survives untouched.
        assert!(
            b_fence.is_file(),
            "A's finalizer deleted the newer attempt B's live fence"
        );
        assert_eq!(
            std::fs::read_to_string(&b_fence).unwrap(),
            b_fence_before,
            "B's fence content must be byte-identical"
        );
        // A's own fence is removed, its cert written attempt-qualified.
        assert!(
            !super::land_fence_path(dirp, job, 1).is_file(),
            "A's fence removed"
        );
        assert!(
            super::land_dead_cert_path(dirp, job, 1).is_file(),
            "A's gen1 cert written"
        );

        // A's gen1 certificate NEVER certifies attempt B (gen 2) — not with
        // B's generation and not with B's merge under A's generation.
        assert!(
            super::read_land_group_dead_cert(dirp, job, 2, MERGE_B).is_none(),
            "A's certificate must not be accepted as B's death evidence"
        );
        assert!(
            super::read_land_group_dead_cert(dirp, job, 1, MERGE_B).is_none(),
            "A's certificate must not bind a different merge"
        );
        assert!(
            super::read_land_group_dead_cert(dirp, job, 1, MERGE_A).is_some(),
            "A's certificate is valid for A's own generation and merge"
        );
        // Attempt B's ledger liveness still reads MayPush (its fence is live).
        assert_eq!(
            super::land_ledger_liveness(dirp, job, Some(2), Some(MERGE_B)),
            super::LandPushLiveness::MayPush
        );
        // Attempt A reads VerifiedDead from its own cert.
        assert_eq!(
            super::land_ledger_liveness(dirp, job, Some(1), Some(MERGE_A)),
            super::LandPushLiveness::VerifiedDead
        );
        // A legacy unsuffixed probe (no generation/merge pinned) stays Unknown.
        assert_eq!(
            super::land_ledger_liveness(dirp, job, None, None),
            super::LandPushLiveness::Unknown
        );
        assert_eq!(
            super::land_ledger_liveness(dirp, job, Some(1), None),
            super::LandPushLiveness::Unknown,
            "a generation without the granted merge stays Unknown"
        );
    }

    /// r34 item 2 (medium): while the OWNER is still registered (slow
    /// finalization / lane not yet released), an ESRCH-dead land group must
    /// NOT be attested Absent — requeuing then would race the next dispatch
    /// into a held lane (lane-busy -> spurious Failed). Liveness stays Unknown
    /// until the owner unregisters; the attempt-qualified durable certificate
    /// then attests VerifiedDead. A different registered generation also reads
    /// Unknown.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn registered_owner_withholds_absent_until_the_lane_is_released() {
        use super::LandPushLiveness::*;
        let fixture = fixture(PASSING_SCRIPT);
        let registry = fixture.node.gate_registry();
        let dir = registry.land_fence_dir.clone();
        let job = "gjb_r34_owner_hold";
        // A group id that does not exist (ESRCH on killpg).
        let dead_pgid = {
            // Spawn and immediately reap a grouped child to obtain a now-dead
            // pid; the id is ESRCH by the time we register.
            use std::os::unix::process::CommandExt;
            let mut child = std::process::Command::new("true")
                .process_group(0)
                .spawn()
                .unwrap();
            let dead_pid = i32::try_from(child.id()).unwrap();
            child.wait().unwrap();
            dead_pid
        };
        assert!(!super::gate_test_group_exists(dead_pgid));

        // No run registered, no cert: Unknown.
        assert_eq!(
            registry
                .land_push_liveness(job, Some(1), Some(MERGE_A))
                .await,
            Unknown
        );

        // Owner registers a live run (gen 1) whose group is already ESRCH —
        // the slow-finalization window. Absent must be withheld: Unknown.
        registry.gate_test_insert_land_run(job, 1, dead_pgid).await;
        assert_eq!(
            registry
                .land_push_liveness(job, Some(1), Some(MERGE_A))
                .await,
            Unknown,
            "a registered owner with an ESRCH group must read Unknown, never VerifiedDead"
        );
        // A probe for a DIFFERENT generation while the owner holds the job is
        // also Unknown (the registered run is gen 1).
        assert_eq!(
            registry
                .land_push_liveness(job, Some(2), Some(MERGE_B))
                .await,
            Unknown
        );

        // The finalizer finishes: attempt-qualified cert for gen 1 lands on
        // disk, then the run unregisters (owner released the lane).
        let record = serde_json::json!({
            "jobId": job, "generation": 1, "pgid": dead_pgid, "startTick": 7,
            "mergeSha": MERGE_A,
        });
        super::write_land_group_dead_cert(&dir, &record, dead_pgid).unwrap();
        registry.gate_test_remove_run(job).await;
        assert_eq!(
            registry
                .land_push_liveness(job, Some(1), Some(MERGE_A))
                .await,
            VerifiedDead,
            "after release the durable gen1 certificate attests Absent"
        );
        // The certificate never crosses generations or merges.
        assert_eq!(
            registry
                .land_push_liveness(job, Some(2), Some(MERGE_B))
                .await,
            Unknown
        );
        assert_eq!(
            registry
                .land_push_liveness(job, Some(1), Some(MERGE_B))
                .await,
            Unknown,
            "a wrong merge must not read the gen1 certificate"
        );
    }

    /// r35 item 2 (HIGH): a detached finalizer must never certify a NEWER
    /// attempt after pgid reuse. Attempt A's finalizer observes ESRCH and parks
    /// BEFORE looking up its fence; attempt B is then granted the SAME recycled
    /// pgid (a distinct generation and distinct merge) and installs its own
    /// fence. When A resumes it may open ONLY its own exact
    /// `land-<job>-g1.json` path: B's live fence survives byte-identical, no
    /// certificate is written for B, and A's certificate binds A's merge only.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_detached_finalizer_after_pgid_reuse_cannot_discover_attempt_b() {
        let fixture = fixture(PASSING_SCRIPT);
        let dir = fixture.node.gate_registry().land_fence_dir.clone();
        let job = "gjb_r35_pgid_reuse";
        // One recycled pgid that answers ESRCH for the whole group.
        let pgid = {
            use std::os::unix::process::CommandExt;
            let mut child = std::process::Command::new("true")
                .process_group(0)
                .spawn()
                .unwrap();
            let pgid = i32::try_from(child.id()).unwrap();
            child.wait().unwrap();
            pgid
        };
        assert!(!super::gate_test_group_exists(pgid));

        // Attempt A (gen 1) fenced the group before its finalizer detached.
        super::write_land_fence(
            &dir,
            job,
            1,
            pgid,
            &super::LeaderIdentity::StartTick(7),
            MERGE_A,
        )
        .unwrap();

        // Park A AFTER ESRCH, BEFORE the exact-path lookup.
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        super::arm_settle_esrch_hold(Arc::new(CleanupHold {
            entered: entered.clone(),
            release: release.clone(),
        }));
        // The finalizer runs detached on the runtime, modelling the production
        // deadline-detached spawn_blocking closure.
        let registry_a = fixture.node.inner.gate.clone();
        let job_a = job.to_owned();
        let handle = tokio::spawn(async move {
            registry_a.settle_land_group(&job_a, 1, MERGE_A, pgid).await;
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !entered.load(std::sync::atomic::Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "A never parked");
            std::thread::sleep(Duration::from_millis(10));
        }

        // Attempt B is granted with the SAME recycled pgid and writes its own
        // attempt-qualified fence (gen 2, different merge).
        super::write_land_fence(
            &dir,
            job,
            2,
            pgid,
            &super::LeaderIdentity::StartTick(99),
            MERGE_B,
        )
        .unwrap();
        let b_fence = super::land_fence_path(&dir, job, 2);
        let b_fence_before = std::fs::read_to_string(&b_fence).unwrap();

        // Resume A.
        {
            let (lock, cvar) = &*release;
            let mut done = lock.lock().unwrap();
            *done = true;
            cvar.notify_all();
        }
        handle.await.expect("detached finalizer task panicked");

        // B's live fence is untouched (byte-identical) and has NO certificate.
        assert_eq!(
            std::fs::read_to_string(&b_fence).unwrap(),
            b_fence_before,
            "B's fence must survive A's resumed finalizer untouched"
        );
        assert!(
            !super::land_dead_cert_path(&dir, job, 2).is_file(),
            "no certificate may ever be written for attempt B"
        );
        assert_eq!(
            super::land_ledger_liveness(&dir, job, Some(2), Some(MERGE_B)),
            super::LandPushLiveness::MayPush,
            "B still shows a live fence"
        );
        // A certified only its own exact path.
        assert!(
            !super::land_fence_path(&dir, job, 1).is_file(),
            "A's own fence is removed"
        );
        assert!(
            super::read_land_group_dead_cert(&dir, job, 1, MERGE_A).is_some(),
            "A's certificate exists for A's exact (generation, merge)"
        );
        assert!(
            super::read_land_group_dead_cert(&dir, job, 1, MERGE_B).is_none(),
            "A's certificate never binds B's merge"
        );
    }

    /// r35 item 3 (medium): a LEGACY live fence (`land-<job>.json` from a
    /// pre-r34 Node) whose whole group is ESRCH at startup is migrated EXPLICITLY
    /// to the attempt-qualified certificate. The migrated cert carries the
    /// legacy provenance and NO merge sha, so it attests death ONLY under a
    /// generation-pinned probe supplying the Hub's authoritative granted merge.
    #[cfg(unix)]
    #[test]
    fn startup_migrates_a_legacy_fence_with_a_dead_group_to_a_bound_cert() {
        let _seq = proc_lock();
        // A guaranteed-gone group.
        let pgid = {
            let child = spawn_grouped("sh", &["-c", "exit 0"]);
            let pgid = child.id() as i32;
            reap(child);
            pgid
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while super::land_group_exists(pgid) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!super::land_group_exists(pgid));

        let dir = tempfile::TempDir::new().unwrap();
        let fence = dir.path();
        let job = "gjb_r35_legacy_fence";
        let legacy = super::legacy_land_fence_path(fence, job);
        let record = serde_json::json!({
            "jobId": job,
            "generation": 1,
            "pgid": pgid,
            "startTick": 5,
        });
        // Legacy records have NO merge sha.
        assert!(record.get("mergeSha").is_none());
        std::fs::write(&legacy, record.to_string()).unwrap();

        super::fence_orphaned_land_groups(fence);

        // The legacy live fence is consumed …
        assert!(!legacy.is_file(), "the legacy fence is migrated away");
        // … into the qualified certificate with the legacy marker and no merge.
        let cert_path = super::land_dead_cert_path(fence, job, 1);
        let cert_text = std::fs::read_to_string(&cert_path).expect("qualified cert written");
        let cert: Value = serde_json::from_str(&cert_text).unwrap();
        assert_eq!(cert["provenance"], json!("legacy-fence"), "{cert}");
        assert!(
            cert.get("mergeSha").is_none(),
            "no merge is fabricated: {cert}"
        );
        assert_eq!(cert["pgid"], json!(pgid));

        // The migrated cert is NOT generic death evidence: without a merge
        // authority the answer is Unknown …
        assert_eq!(
            super::land_ledger_liveness(fence, job, Some(1), None),
            super::LandPushLiveness::Unknown
        );
        assert_eq!(
            super::land_ledger_liveness(fence, job, Some(1), Some("")),
            super::LandPushLiveness::Unknown
        );
        // … but the missing merge is bound ONLY through the Hub's authoritative
        // generation-pinned probe: the legacy record cannot verify the merge
        // itself, so every NON-EMPTY merge the authority carries binds it (the
        // Hub always sends its own granted merge for this generation). A
        // new-style cert with an embedded mergeSha is matched exactly instead.
        assert_eq!(
            super::land_ledger_liveness(fence, job, Some(1), Some(MERGE_A)),
            super::LandPushLiveness::VerifiedDead
        );
        assert_eq!(
            super::land_ledger_liveness(fence, job, Some(1), Some(MERGE_B)),
            super::LandPushLiveness::VerifiedDead,
            "legacy evidence binds through the probe's authoritative merge"
        );
    }

    /// r35 item 3 (upgrade continuity): an r34-era attempt-qualified fence
    /// (`land-<job>-g1.json` — generation in the name but no merge sha, as the
    /// immediately previous build wrote) with a verified-dead group is migrated
    /// to a legacy-marked qualified certificate rather than stranded.
    #[cfg(unix)]
    #[test]
    fn startup_migrates_a_merge_less_r34_qualified_fence_with_a_dead_group() {
        let _seq = proc_lock();
        let pgid = {
            let child = spawn_grouped("sh", &["-c", "exit 0"]);
            let pgid = child.id() as i32;
            reap(child);
            pgid
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while super::land_group_exists(pgid) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!super::land_group_exists(pgid));

        let dir = tempfile::TempDir::new().unwrap();
        let fence = dir.path();
        let job = "gjb_r35_r34_fence";
        // r34 wrote the generation-qualified NAME with merge-less CONTENTS.
        let source = super::land_fence_path(fence, job, 1);
        let record = serde_json::json!({
            "jobId": job,
            "generation": 1,
            "pgid": pgid,
            "startTick": 5,
        });
        std::fs::write(&source, record.to_string()).unwrap();

        super::fence_orphaned_land_groups(fence);

        assert!(!source.is_file(), "the r34 merge-less fence is migrated");
        let cert_path = super::land_dead_cert_path(fence, job, 1);
        let cert: Value =
            serde_json::from_str(&std::fs::read_to_string(&cert_path).unwrap()).unwrap();
        assert_eq!(cert["provenance"], json!("legacy-fence"), "{cert}");
        assert!(cert.get("mergeSha").is_none());
        // Binds only via the authoritative probe.
        assert_eq!(
            super::land_ledger_liveness(fence, job, Some(1), None),
            super::LandPushLiveness::Unknown
        );
        assert_eq!(
            super::land_ledger_liveness(fence, job, Some(1), Some(MERGE_A)),
            super::LandPushLiveness::VerifiedDead
        );
    }

    /// r35 item 3 (medium): a LEGACY death certificate (`dead-<job>.json`)
    /// surviving an upgrade is migrated only when a generation-pinned,
    /// merge-bearing probe matches the record's validated attempt identity; an
    /// unqualified read never accepts it as generic death evidence, and a
    /// legacy record without a bindable generation is left for the documented
    /// operator recovery route.
    #[cfg(unix)]
    #[test]
    fn a_legacy_dead_cert_binds_only_through_the_authoritative_probe() {
        let dir = tempfile::TempDir::new().unwrap();
        let fence = dir.path();
        let job = "gjb_r35_legacy_cert";
        let legacy = super::legacy_land_dead_cert_path(fence, job);
        let cert = serde_json::json!({
            "jobId": job,
            "generation": 1,
            "pgid": 4242,
            "startTick": 7,
            "diedAt": 1_700_000_000_000u64,
            "cause": "killpg-esrch",
        });
        std::fs::write(&legacy, cert.to_string()).unwrap();

        // Generic/unqualified reads are refused (no merge authority) …
        assert!(super::read_land_group_dead_cert(fence, job, 1, "").is_none());
        assert_eq!(
            super::land_ledger_liveness(fence, job, Some(1), None),
            super::LandPushLiveness::Unknown
        );
        // … and the legacy file is still in place (nothing bound it yet).
        assert!(
            legacy.is_file(),
            "an unbound legacy cert must be left on disk"
        );

        // The Hub's authoritative generation+merge probe migrates and accepts
        // it once, then serves the qualified file.
        let migrated = super::read_land_group_dead_cert(fence, job, 1, MERGE_A)
            .expect("the generation-pinned probe binds the legacy cert");
        assert_eq!(migrated["provenance"], json!("legacy-cert"), "{migrated}");
        assert!(
            migrated.get("mergeSha").is_none(),
            "the request merge is never stamped into evidence: {migrated}"
        );
        assert!(
            !legacy.is_file(),
            "the legacy source is removed after migration"
        );
        assert!(super::land_dead_cert_path(fence, job, 1).is_file());
        // Legacy evidence binds the missing merge via the probe's authority:
        // every non-empty merge under the pinned generation verifies.
        assert_eq!(
            super::land_ledger_liveness(fence, job, Some(1), Some(MERGE_B)),
            super::LandPushLiveness::VerifiedDead
        );
        assert_eq!(
            super::land_ledger_liveness(fence, job, Some(1), None),
            super::LandPushLiveness::Unknown,
            "no merge authority stays Unknown"
        );

        // An unbindable legacy record (no attempt generation) can never be
        // migrated: it stays Unknown and on disk for the operator route.
        let job2 = "gjb_r35_legacy_unbound";
        let legacy2 = super::legacy_land_dead_cert_path(fence, job2);
        let unbound = serde_json::json!({
            "jobId": job2,
            "pgid": 4242,
            "startTick": 7,
            "diedAt": 1_700_000_000_000u64,
            "cause": "killpg-esrch",
        });
        std::fs::write(&legacy2, unbound.to_string()).unwrap();
        assert!(super::read_land_group_dead_cert(fence, job2, 1, MERGE_A).is_none());
        assert!(
            legacy2.is_file(),
            "an unbindable legacy record is kept (operator resolution via the Hub /resolve route)"
        );
        assert_eq!(
            super::land_ledger_liveness(fence, job2, Some(1), Some(MERGE_A)),
            super::LandPushLiveness::Unknown
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
            .record_land_group("job-fail", 1, std::process::id() as i32, MERGE_A)
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
        let fence = super::land_fence_path(&dir, job, 1);
        let cert = super::land_dead_cert_path(&dir, job, 1);
        let _ = std::fs::remove_file(&fence);
        let _ = std::fs::remove_file(&cert);

        // Nothing on disk: Unknown — never a death attestation.
        assert_eq!(
            registry
                .land_push_liveness(job, Some(1), Some(MERGE_A))
                .await,
            Unknown
        );
        // No merge authority: Unknown even with nothing on disk.
        assert_eq!(
            registry.land_push_liveness(job, Some(1), None).await,
            Unknown
        );

        // A malformed fence ledger is still a PRESENT ledger: may push.
        std::fs::write(&fence, b"not json").unwrap();
        assert_eq!(
            registry
                .land_push_liveness(job, Some(1), Some(MERGE_A))
                .await,
            MayPush
        );

        // Ledger gone, malformed cert: Unknown.
        std::fs::remove_file(&fence).unwrap();
        std::fs::write(&cert, b"not json").unwrap();
        assert_eq!(
            registry
                .land_push_liveness(job, Some(1), Some(MERGE_A))
                .await,
            Unknown
        );

        // Cert present but missing its leader identity: Unknown.
        std::fs::write(
            &cert,
            br#"{"jobId":"gjb_liveness","generation":1,"pgid":12345,"mergeSha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#,
        )
        .unwrap();
        assert_eq!(
            registry
                .land_push_liveness(job, Some(1), Some(MERGE_A))
                .await,
            Unknown
        );

        // Cert naming a different job never proves THIS job dead.
        let foreign = serde_json::json!({
            "jobId": "gjb_other", "generation": 1, "pgid": 1, "startTick": 7,
            "mergeSha": MERGE_A,
        });
        super::write_land_group_dead_cert(&dir, &foreign, 1).unwrap();
        assert_eq!(
            registry
                .land_push_liveness(job, Some(1), Some(MERGE_A))
                .await,
            Unknown
        );

        // A complete certificate for the exact job: VerifiedDead.
        let own = serde_json::json!({
            "jobId": job, "generation": 1, "pgid": 12345, "startTick": 42,
            "mergeSha": MERGE_A,
        });
        super::write_land_group_dead_cert(&dir, &own, 12345).unwrap();
        assert_eq!(
            registry
                .land_push_liveness(job, Some(1), Some(MERGE_A))
                .await,
            VerifiedDead
        );
        // A complete certificate never binds a different merge.
        assert_eq!(
            registry
                .land_push_liveness(job, Some(1), Some(MERGE_B))
                .await,
            Unknown
        );
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

        // r34 item 1: a probe WITHOUT the attempt generation stays Unknown even
        // when an attempt-qualified certificate exists (no cross-attempt
        // evidence).
        // r35 item 2: the certificate binds generation AND the exact granted
        // merge sha (the real orphan commit the fixture probes).
        let dir = fixture.node.gate_registry().land_fence_dir.clone();
        let record = serde_json::json!({
            "jobId": "gjb_deadprobe", "generation": 1, "pgid": 4242, "startTick": 99,
            "mergeSha": orphan,
        });
        super::write_land_group_dead_cert(&dir, &record, 4242).unwrap();
        let reply = fixture
            .node
            .run_gate_resolve_landing(&params)
            .await
            .unwrap();
        assert_eq!(
            reply["status"],
            json!("unknown"),
            "no generation pinned: {reply}"
        );
        // A certificate for a DIFFERENT generation must not attest either.
        let wrong_gen = serde_json::json!({
            "jobId": "gjb_deadprobe",
            "repoPath": fixture.lane.to_string_lossy(),
            "baseBranch": "main",
            "mergeSha": orphan,
            "generation": 7,
        });
        let reply = fixture
            .node
            .run_gate_resolve_landing(&wrong_gen)
            .await
            .unwrap();
        assert_eq!(
            reply["status"],
            json!("unknown"),
            "wrong generation: {reply}"
        );
        // Pinning the exact attempt generation (1) settles absent.
        let params = serde_json::json!({
            "jobId": "gjb_deadprobe",
            "repoPath": fixture.lane.to_string_lossy(),
            "baseBranch": "main",
            "mergeSha": orphan,
            "generation": 1,
        });
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
        let granted = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let gate_failed = &format!(
            r#"{{"exitCode":1,"status":"gate_failed","merged":"{granted}","error":"hook refused"}}"#
        );
        let landed = &format!(r#"{{"exitCode":0,"status":"landed","merged":"{granted}"}}"#);

        // exit 0 with a recognized landed report naming the granted merge →
        // landed.
        assert!(matches!(
            super::classify_released_land_outcome(&output_from(0, landed, ""), "", Some(granted)),
            super::ReleasedLandOutcome::Landed(_)
        ));
        // r17 item 4: a zero exit with an empty/malformed body, a non-landed
        // status, a missing merge, or a MISMATCHED merge is Unknown regardless
        // of the exit code — it never bypasses validation into Landed.
        let other = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        for (stdout, grant) in [
            ("{}", Some(granted)),
            ("", Some(granted)),
            ("not json", Some(granted)),
            (r#"{"status":"landed"}"#, Some(granted)),
            (r#"{"status":"landed","merged":""}"#, Some(granted)),
            (
                &format!(r#"{{"status":"landed","merged":"{other}"}}"#),
                Some(granted),
            ),
            (landed.as_str(), Some(other)),
            (landed.as_str(), None),
            (r#"{"status":"conflict"}"#, Some(granted)),
        ] {
            assert!(
                matches!(
                    super::classify_released_land_outcome(
                        &output_from(0, stdout, "zero"),
                        "",
                        grant
                    ),
                    super::ReleasedLandOutcome::Uncertain { .. }
                ),
                "exit-0 stdout {stdout:?} grant {grant:?} must be Unknown"
            );
        }
        // A structured base_moved riding a zero exit is honored when it names
        // the granted merge (r18 item 1).
        assert!(matches!(
            super::classify_released_land_outcome(
                &output_from(
                    0,
                    &format!(
                        r#"{{"status":"base_moved","merged":"{granted}","currentMain":"abc"}}"#
                    ),
                    ""
                ),
                "",
                Some(granted)
            ),
            super::ReleasedLandOutcome::BaseMoved(_)
        ));
        // The one recognized structured failure (exit 1 + gate_failed).
        assert!(matches!(
            super::classify_released_land_outcome(
                &output_from(1 << 8, gate_failed, ""),
                "",
                Some(granted)
            ),
            super::ReleasedLandOutcome::Failed(message) if message.contains("hook refused")
        ));
        // base_moved keeps its recognized verdict when it names the merge.
        assert!(matches!(
            super::classify_released_land_outcome(
                &output_from(
                    2 << 8,
                    &format!(
                        r#"{{"status":"base_moved","merged":"{granted}","currentMain":"abc"}}"#
                    ),
                    ""
                ),
                "",
                Some(granted)
            ),
            super::ReleasedLandOutcome::BaseMoved(_)
        ));
        // The CLI's own uncertainty stays uncertain.
        assert!(matches!(
            super::classify_released_land_outcome(
                &output_from(4 << 8, r#"{"status":"uncertain"}"#, ""),
                "",
                Some(granted)
            ),
            super::ReleasedLandOutcome::Uncertain { .. }
        ));
        // Missing JSON, malformed JSON and an unrecognized status are all
        // Unknown — never Failed.
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
                        "",
                        Some(granted)
                    ),
                    super::ReleasedLandOutcome::Uncertain { .. }
                ),
                "stdout {stdout:?} must be Unknown"
            );
        }
        // gate_failed on a NON-1 exit is not the recognized failure.
        assert!(matches!(
            super::classify_released_land_outcome(
                &output_from(2 << 8, gate_failed, ""),
                "",
                Some(granted)
            ),
            super::ReleasedLandOutcome::Uncertain { .. }
        ));
        // Killed by SIGKILL/SIGSEGV after release with no exit code: crash-only
        // is Unknown even if a well-formed gate_failed happened to be printed.
        for signal in [9, 11] {
            assert!(
                matches!(
                    super::classify_released_land_outcome(
                        &output_from(signal, gate_failed, ""),
                        "",
                        Some(granted)
                    ),
                    super::ReleasedLandOutcome::Uncertain { .. }
                ),
                "signal {signal} must be Unknown"
            );
        }
    }

    /// r18 item 1: refusal verdicts (base_moved / gate_failed) are accepted
    /// ONLY when they carry a nonempty merged sha equal to the grant; a missing
    /// or different merge makes the refusal Unknown.
    #[cfg(unix)]
    #[test]
    fn refusal_verdicts_require_the_granted_merge_else_unknown() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::{ExitStatus, Output};
        fn output(raw: i32, stdout: &str) -> Output {
            Output {
                status: ExitStatus::from_raw(raw),
                stdout: stdout.as_bytes().to_vec(),
                stderr: b"boom".to_vec(),
            }
        }
        let granted = "cccccccccccccccccccccccccccccccccccccccc";
        let other = "dddddddddddddddddddddddddddddddddddddddd";
        let base_moved = |merge: Option<&str>| match merge {
            Some(sha) => format!(
                r#"{{"exitCode":2,"status":"base_moved","merged":"{sha}","currentMain":"abc"}}"#
            ),
            None => r#"{"exitCode":2,"status":"base_moved","currentMain":"abc"}"#.into(),
        };
        let gate_failed = |merge: Option<&str>| match merge {
            Some(sha) => format!(
                r#"{{"exitCode":1,"status":"gate_failed","merged":"{sha}","error":"rejected"}}"#
            ),
            None => r#"{"exitCode":1,"status":"gate_failed","error":"rejected"}"#.into(),
        };

        // Correct merge on either exit → recognized refusal.
        assert!(matches!(
            super::classify_released_land_outcome(
                &output(2 << 8, &base_moved(Some(granted))),
                "",
                Some(granted)
            ),
            super::ReleasedLandOutcome::BaseMoved(_)
        ));
        assert!(matches!(
            super::classify_released_land_outcome(
                &output(1 << 8, &gate_failed(Some(granted))),
                "",
                Some(granted)
            ),
            super::ReleasedLandOutcome::Failed(_)
        ));
        // base_moved riding exit 0 with the grant is also honored.
        assert!(matches!(
            super::classify_released_land_outcome(
                &output(0, &base_moved(Some(granted))),
                "",
                Some(granted)
            ),
            super::ReleasedLandOutcome::BaseMoved(_)
        ));

        // Missing merge or a different merge → Unknown on every refusal path.
        for (raw, stdout) in [
            (2 << 8, base_moved(None)),
            (2 << 8, base_moved(Some(other))),
            (0, base_moved(None)),
            (0, base_moved(Some(other))),
            (1 << 8, gate_failed(None)),
            (1 << 8, gate_failed(Some(other))),
        ] {
            assert!(
                matches!(
                    super::classify_released_land_outcome(&output(raw, &stdout), "", Some(granted)),
                    super::ReleasedLandOutcome::Uncertain { .. }
                ),
                "refusal {raw} with {stdout} must be Unknown"
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

    /// r17 item 4: a zero exit that does NOT carry a recognized `landed` verdict
    /// for the EXACT granted merge is UNCERTAIN, never a bare "landed". Both a
    /// missing merge identity and a mismatched sha must fail closed through the
    /// production builder.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_zero_exit_with_a_missing_or_wrong_merge_is_uncertain() {
        for bad_report in [
            // Zero exit, no JSON at all.
            "exit 0".to_owned(),
            // Zero exit, landed status but no merged sha.
            r#"echo '{"exitCode":0,"status":"landed"}'; exit 0"#.to_owned(),
            // Zero exit claiming a DIFFERENT merge landed.
            r#"echo '{"exitCode":0,"status":"landed","merged":"ffffffffffffffffffffffffffffffffffffffff"}'; exit 0"#
                .to_owned(),
        ] {
            let script = format!(
                r#"base=$(git rev-parse main 2>/dev/null || echo "1111111111111111111111111111111111111111")
head=$(git rev-parse wt/fake/task 2>/dev/null || echo "$base")
if ! printf '%s\n' "$*" | grep -q -- '--land'; then
  echo "{{\"exitCode\":0,\"status\":\"verified\",\"base\":\"$base\",\"head\":\"$head\",\"merged\":\"$head\",\"steps\":[]}}"
  exit 0
fi
head -c 1 >/dev/null || exit 4
{bad_report}
"#
            );
            let fixture = fixture(&script);
            let mut p = params(&fixture, "land");
            p.push = false;
            let result = fixture.node.run_gate_typed(p).await.unwrap();
            assert_eq!(
                result.status, "uncertain",
                "zero-exit report {bad_report:?} must be Unknown, got {result:?}"
            );
            assert!(
                !result.merge_sha.clone().unwrap_or_default().is_empty(),
                "the granted merge is retained for reconciliation: {result:?}"
            );
            assert_ne!(
                result.merge_sha.as_deref().unwrap_or(""),
                "ffffffffffffffffffffffffffffffffffffffff",
                "the mismatched claimed merge must not replace the granted merge: {result:?}"
            );
        }
    }

    fn fixture_lane_main(fixture: &Fixture) -> String {
        git(&fixture.lane, &["rev-parse", "origin/main"])
    }

    /// R6 single-owner determinism:
    ///  - a child that exits within the deadline yields Completed (exit 0) and
    ///    the leader is reaped exactly once (its pid gone);
    ///  - a child past the deadline yields TimedOut whose status is a SIGKILL
    ///    (not a clean exit), and EVERY member of its group is gone — the owner
    ///    killpg'd while the leader was unreaped and reaped it once.
    ///
    /// Assertions are on the outcome/status/group, not on timing sleeps.
    #[test]
    fn run_owned_completes_reaps_and_timeout_kills_unreaped_group() {
        let keep = tempfile::TempDir::new().unwrap();
        let quick = keep.path().join("quick");
        std::fs::write(&quick, "#!/bin/sh\nexit 0\n").unwrap();
        let long = keep.path().join("long");
        // Leader + a grandchild, both in the script's new group.
        std::fs::write(&long, "#!/bin/sh\n(sleep 47.73 &)\nexec sleep 47.73\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        for bin in [&quick, &long] {
            std::fs::set_permissions(bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // Completed path: a generous deadline, normal exit, reaped once.
        let command = std::process::Command::new(quick.to_str().unwrap());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        match remuda_unix::run_owned_process(command, Duration::from_secs(5), deadline).unwrap() {
            remuda_unix::OwnedRun::Completed(output) => {
                assert!(output.status.success(), "quick exit 0");
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        // TimedOut path: short soft cap; the group (leader + a grandchild) is
        // SIGKILLed while the leader is unreaped. The ABSOLUTE deadline stays
        // generous so the kill/reap/drain sequence has room.
        let started = std::time::Instant::now();
        let command = std::process::Command::new(long.to_str().unwrap());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let outcome =
            remuda_unix::run_owned_process(command, Duration::from_millis(300), deadline).unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "bound applies");
        let output = match outcome {
            remuda_unix::OwnedRun::TimedOut(output) => output,
            other => panic!("expected TimedOut, got {other:?}"),
        };
        // Killed by SIGKILL (signal present) rather than a normal exit.
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            output.status.signal(),
            Some(9),
            "the timed-out leader must be SIGKILLed while unreaped: {:?}",
            output.status
        );
        // No LIVE `sleep 30` from the killed group survives (the group kill
        // reached the grandchild). A killed-and-reparented sleep may be a zombie
        // pending init's reap, which is excluded by its /proc state (not "Z").
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let live_sleeps: usize = std::fs::read_dir(super::proc_root())
                .map(|entries| {
                    entries
                        .flatten()
                        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
                        .filter_map(|name| name.parse::<i32>().ok())
                        .filter(|pid| {
                            let base = super::proc_root().join(pid.to_string());
                            let Ok(cmd) = std::fs::read_to_string(base.join("cmdline")) else {
                                return false;
                            };
                            // /proc cmdline separates argv with NUL bytes.
                            if !cmd.replace('\0', " ").contains("sleep 47.73") {
                                return false;
                            }
                            // Exclude zombies (state field 2 after comm).
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
                .unwrap_or(0);
            if live_sleeps == 0 || std::time::Instant::now() >= deadline {
                assert_eq!(live_sleeps, 0, "a live group grandchild survived the kill");
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// r20 item 5 / R8(b): the leader EXITS FIRST (nonzero) while a same-group
    /// transport grandchild keeps running and HOLDS stderr (its stdout goes to
    /// /dev/null). The owner must observe the exit WITHOUT reaping, TERM/KILL
    /// the surviving grandchild WHILE the unreaped leader pins the pgid, then
    /// reap and return the LEADER's exit status (Completed) — and no sleep
    /// survives. The old r19 owner reaped the leader via try_wait and never
    /// signalled on the normal-exit arm, leaving the grandchild and the
    /// blocked reader.
    #[test]
    fn run_owned_normal_exit_kills_surviving_pipe_holding_grandchild() {
        let keep = tempfile::TempDir::new().unwrap();
        let wrapper = keep.path().join("wrapper");
        // Same-group backgrounded transport: stdout detached, stderr inherited
        // (the captured pipe). The leader exits unsuccessfully immediately.
        std::fs::write(
            &wrapper,
            "#!/bin/sh\n(sleep 47.55 >/dev/null) &\necho transport-line >&2\nexit 7\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = std::time::Instant::now();
        let command = std::process::Command::new(wrapper.to_str().unwrap());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let run =
            remuda_unix::run_owned_process(command, Duration::from_secs(2), deadline).unwrap();
        // Completed with the LEADER's exit code (7), not a timeout and not an
        // error; bounded well under either deadline.
        let output = match run {
            remuda_unix::OwnedRun::Completed(output) => output,
            other => panic!("expected Completed after survivor cleanup, got {other:?}"),
        };
        assert_eq!(output.status.code(), Some(7), "the leader's status is kept");
        assert!(started.elapsed() < Duration::from_secs(4));
        // The grandchild was KILLed as part of normal completion.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let left = live_marker_processes("sleep 47.55");
            if left == 0 || std::time::Instant::now() >= deadline {
                assert_eq!(left, 0, "the surviving transport grandchild was not killed");
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    // r21 item 7: the blocked-pipe/deadline regression no longer lives here.
    // The old test assumed Linux /proc enumeration makes a TERM-ignoring
    // descendant consume the tight deadline; without /proc (macOS) the owner
    // signals, KILLs and reaps before the deadline and correctly returns
    // Completed, so the mandatory Unknown assertion failed under ordinary
    // scheduling. Platform-independent coverage now runs against the shared
    // owner itself in remuda-unix:
    // `owner::tests::completion_after_the_deadline_is_latched_unknown` uses
    // the injected post-exit-observation seam to hold the whole
    // cleanup/reap/drain sequence beyond the absolute deadline and asserts
    // the latched Unknown on every platform. This module keeps the portable
    // owner-sequence tests above (soft-cap kill, normal-exit survivor
    // cleanup) and the Node-specific async land-path tests.

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

    /// r19 item 2 / R7: gate recovery NEVER deepens shallow history. Even with
    /// the granted merge genuinely absent and a death certificate proving the
    /// land group dead, a depth-1 clone makes ancestry undecidable locally, so
    /// the production read-only resolver answers Unknown with an operator
    /// message naming the repository. The clone stays shallow — no
    /// `git fetch --deepen` is attempted.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_landing_shallow_repo_is_unknown_and_never_deepens() {
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
            "jobId": "gjb_shallow_r7", "generation": 1, "pgid": 5353, "startTick": 7,
            "mergeSha": orphan,
        });
        super::write_land_group_dead_cert(&dir, &record, 5353).unwrap();
        let reply = fixture
            .node
            .run_gate_resolve_landing(&serde_json::json!({
                "jobId": "gjb_shallow_r7",
                "repoPath": lane.to_string_lossy(),
                "baseBranch": "main",
                "mergeSha": orphan,
            }))
            .await
            .unwrap();
        assert_eq!(reply["status"], json!("unknown"), "{reply}");
        let error = reply["error"].as_str().unwrap_or("");
        assert!(
            error.contains("shallow"),
            "the operator message names the shallow condition: {reply}"
        );
        assert!(
            error.contains(&*lane.to_string_lossy()),
            "the operator message names the repository: {reply}"
        );
        assert_eq!(
            git(&lane, &["rev-parse", "--is-shallow-repository"]),
            "true",
            "R7 recovery never deepens: the lane must stay shallow"
        );
    }

    /// Build a full-history bare origin plus a lane clone whose default
    /// branch is `branch`, optionally in SHA-256 object format. Returns the
    /// keepalive tempdirs (bare dir owns origin.git), the lane checkout and
    /// the initial base B.
    #[cfg(unix)]
    fn recovery_branch_fixture(
        branch: &str,
        object_format: Option<&str>,
    ) -> (TempDir, TempDir, PathBuf, String) {
        let dir = TempDir::new().unwrap();
        let bare = TempDir::new().unwrap();
        let origin = bare.path().join("origin.git");
        let seed = dir.path().join("seed");
        let lane = dir.path().join("lane");
        std::fs::create_dir_all(&seed).unwrap();
        let mut init = std::process::Command::new("git");
        init.args(["init", "--bare", "-q", "-b", branch]);
        if let Some(format) = object_format {
            init.args(["--object-format", format]);
        }
        let output = init.arg(&origin).output().unwrap();
        assert!(
            output.status.success(),
            "bare init: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut seed_init = std::process::Command::new("git");
        seed_init.args(["init", "-q", "-b", branch]);
        if let Some(format) = object_format {
            seed_init.args(["--object-format", format]);
        }
        assert!(seed_init.current_dir(&seed).status().unwrap().success());
        git(&seed, &["config", "user.email", "t@example.com"]);
        git(&seed, &["config", "user.name", "T"]);
        std::fs::write(seed.join("file.txt"), "base\n").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-q", "-m", "init"]);
        git(
            &seed,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&seed, &["push", "-q", "origin", branch]);
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
        let base = git(&lane, &["rev-parse", "origin/HEAD"]);
        (dir, bare, lane, base)
    }

    /// R10: the FULL for-each-ref listing before and after a recovery must be
    /// byte-identical — recovery writes no refs anywhere (heads, tags, notes,
    /// recovery namespace, nothing).
    #[cfg(unix)]
    fn refs_listing(repo: &Path) -> String {
        git(repo, &["for-each-ref", "--format=%(refname) %(objectname)"])
    }

    /// r31 item 2: recovery fetch must NEVER pass --no-write-fetch-head (older
    /// git rejects it and FETCH_HEAD is not a ref recovery reads).
    #[cfg(unix)]
    #[test]
    fn recovery_fetch_flags_never_write_fetch_head() {
        assert!(
            remuda_unix::RECOVERY_FETCH_FLAGS
                .iter()
                .all(|flag| *flag != "--no-write-fetch-head")
        );
        assert!(remuda_unix::RECOVERY_FETCH_FLAGS.contains(&"--refmap="));
        assert!(remuda_unix::RECOVERY_FETCH_FLAGS.contains(&"--no-prune"));
        assert!(remuda_unix::RECOVERY_FETCH_FLAGS.contains(&"--no-tags"));
    }

    /// R10: ls-remote parsing accepts exactly one well-formed line for the
    /// requested ref and rejects everything else as an unknown tip.
    #[cfg(unix)]
    #[test]
    fn ls_remote_base_parsing() {
        let sha1 = "0123456789abcdef0123456789abcdef01234567";
        let sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let one = format!("{sha1}\trefs/heads/main\n");
        assert_eq!(
            super::parse_ls_remote_base(one.as_bytes(), "main").unwrap(),
            sha1
        );
        assert_eq!(
            super::parse_ls_remote_base(format!("{sha256}\trefs/heads/x\n").as_bytes(), "x")
                .unwrap(),
            sha256
        );
        // Absent base: no lines.
        assert!(super::parse_ls_remote_base(b"", "main").is_err());
        // Another ref advertised, but not the requested one.
        assert!(
            super::parse_ls_remote_base(format!("{sha1}\trefs/heads/other\n").as_bytes(), "main")
                .is_err()
        );
        // Duplicate lines, malformed lines, bad hex are all errors.
        assert!(
            super::parse_ls_remote_base(
                format!("{sha1}\trefs/heads/main\n{sha1}\trefs/heads/main\n").as_bytes(),
                "main"
            )
            .is_err()
        );
        assert!(super::parse_ls_remote_base(b"garbage line\n", "main").is_err());
        assert!(super::parse_ls_remote_base(b"zz\trefs/heads/main\n", "main").is_err());
        // Uppercase A-F (40 and 64 chars) is not a git-advertised ID.
        assert!(
            super::parse_ls_remote_base(
                b"ABCDEF0123456789ABCDEF0123456789ABCDEF01\trefs/heads/main\n",
                "main"
            )
            .is_err()
        );
        assert!(
            super::parse_ls_remote_base(
                b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789ABCDEF\trefs/heads/x\n",
                "x"
            )
            .is_err()
        );

        assert!(
            super::parse_ls_remote_base(
                format!("{}\trefs/heads/main\n", &sha1[..39]).as_bytes(),
                "main"
            )
            .is_err()
        );
        // A peeled tag line beside the head line is not a second match.
        assert_eq!(
            super::parse_ls_remote_base(
                format!("{sha1}\trefs/heads/main\n{sha1}\trefs/tags/v1\n").as_bytes(),
                "main"
            )
            .unwrap(),
            sha1
        );
    }

    /// R10: two overlapping recoveries never swap snapshots — recovery reads
    /// the authoritative remote tip with ls-remote and writes no refs, so a
    /// remote move between A's read and its object fetch leaves each probe with
    /// its own honest SHA: B (M) certifies Landed; A (B, no death evidence)
    /// stays Unknown.
    #[cfg(unix)]
    #[test]
    fn overlapping_recoveries_read_distinct_tips_without_writing_refs() {
        use super::recovery_read_seam as seam;
        let (_dir, _bare, lane, base_b) = recovery_branch_fixture("main", None);
        std::fs::write(lane.join("overlap.txt"), "M\n").unwrap();
        git(&lane, &["add", "overlap.txt"]);
        git(&lane, &["commit", "-q", "-m", "granted merge M"]);
        let merge_m = git(&lane, &["rev-parse", "main"]);
        assert_ne!(base_b, merge_m);

        let params = |job: &'static str, dead: bool| {
            let lane = lane.clone();
            let merge_m = merge_m.clone();
            std::thread::spawn(move || {
                let request = remuda_protocol::GateResolveLandingParams {
                    generation: None,
                    job_id: job.into(),
                    repo_path: lane.to_string_lossy().into_owned(),
                    base_branch: "main".into(),
                    merge_sha: merge_m,
                };
                super::resolve_landing_after_restart(&request, dead)
            })
        };

        seam::arm(&lane, 2);
        let a = params("gjb_r30_ov_a", false);
        seam::wait_arrived(&lane, 0);
        // M reaches the remote between A's read (B) and its object fetch.
        git(&lane, &["push", "-q", "origin", "main"]);
        // Baseline AFTER the test's own push (which legitimately moves the
        // lane's tracking refs); parked A has not fetched anything yet.
        let before = refs_listing(&lane);
        let b = params("gjb_r30_ov_b", true);
        seam::wait_arrived(&lane, 1);
        seam::release(&lane, 1);
        let reply_b = b.join().expect("probe B joins");
        seam::release(&lane, 0);
        let reply_a = a.join().expect("probe A joins");
        seam::disarm(&lane);

        assert_eq!(
            reply_b.status, "landed",
            "B read M, must stay Landed: {reply_b:?}"
        );
        assert_eq!(
            reply_b.base_sha.as_deref(),
            Some(merge_m.as_str()),
            "{reply_b:?}"
        );
        assert_eq!(
            reply_a.status, "unknown",
            "A read B with no death evidence: {reply_a:?}"
        );
        assert_eq!(
            reply_a.base_sha, None,
            "an Unknown (non-ancestry without death evidence) carries no base SHA: {reply_a:?}"
        );
        assert!(
            reply_a
                .error
                .as_deref()
                .unwrap_or("")
                .contains(&base_b[..12]),
            "A's classification is against its OWN read tip B: {reply_a:?}"
        );
        assert_eq!(
            refs_listing(&lane),
            before,
            "overlapping recoveries created, updated or deleted no refs"
        );
    }

    /// R10: recovering `release`, then after a remote migration recovering
    /// `release/v2`, certifies Landed both times without any ref writes (there
    /// is no branch-shaped local ref whose prefix could collide).
    #[cfg(unix)]
    #[test]
    fn recovery_survives_a_base_branch_rename_and_writes_no_refs() {
        let (_dir, _bare, lane, _base_b) = recovery_branch_fixture("release", None);
        std::fs::write(lane.join("m.txt"), "M\n").unwrap();
        git(&lane, &["add", "m.txt"]);
        git(&lane, &["commit", "-q", "-m", "granted merge M"]);
        let merge_m = git(&lane, &["rev-parse", "HEAD"]);
        git(&lane, &["push", "-q", "origin", "HEAD:release"]);

        let recover = |base: &'static str| {
            let request = remuda_protocol::GateResolveLandingParams {
                generation: None,
                job_id: "gjb_r30_rename".into(),
                repo_path: lane.to_string_lossy().into_owned(),
                base_branch: base.into(),
                merge_sha: merge_m.clone(),
            };
            super::resolve_landing_after_restart(&request, true)
        };
        // Each recovery must leave the repository's refs byte-identical; the
        // remote MIGRATION below legitimately moves the tracking refs, so the
        // snapshots bracket each recovery individually.
        let before_first = refs_listing(&lane);
        let first = recover("release");
        assert_eq!(
            first.status, "landed",
            "first recovery on release: {first:?}"
        );
        assert_eq!(refs_listing(&lane), before_first);

        // Migrate the remote branch release -> release/v2 (delete first; the
        // bare server cannot hold both names).
        let origin = _bare.path().join("origin.git");
        git(&origin, &["config", "receive.denyDeleteCurrent", "ignore"]);
        std::fs::write(lane.join("d.txt"), "D\n").unwrap();
        git(&lane, &["add", "d.txt"]);
        git(&lane, &["commit", "-q", "-m", "rename base to release/v2"]);
        git(&lane, &["push", "-q", "origin", ":release"]);
        git(
            &lane,
            &["push", "-q", "origin", "HEAD:refs/heads/release/v2"],
        );

        let second = {
            let before_second = refs_listing(&lane);
            let reply = recover("release/v2");
            assert_eq!(
                refs_listing(&lane),
                before_second,
                "the second recovery created, updated or deleted no ref"
            );
            reply
        };
        assert_eq!(
            second.status, "landed",
            "M is contained in release/v2 with no local ref collision: {second:?}"
        );
    }

    /// R10 r31 item 1: a configured SHA-source remote.origin.fetch mapping
    /// (`+<sha>:refs/tags/keep`) must NOT write its destination during a
    /// destination-less recovery fetch — the empty --refmap ignores configured
    /// mappings. Covered with the destination (a) absent and (b) already
    /// pointing elsewhere; fetch.prune=true plus a stale tracking ref must also
    /// leave the full ref listing byte-identical.
    #[cfg(unix)]
    #[test]
    fn configured_fetch_mappings_and_prune_write_no_refs_during_recovery() {
        let (_dir, _bare, lane, base_b) = recovery_branch_fixture("main", None);
        let seed = _dir.path().join("seed");
        // Remote main advances to M containing B.
        std::fs::write(seed.join("m.txt"), "M\n").unwrap();
        git(&seed, &["add", "m.txt"]);
        git(&seed, &["commit", "-q", "-m", "M"]);
        let merge_m = git(&seed, &["rev-parse", "main"]);
        git(&seed, &["push", "-q", "origin", "main"]);

        // (a) Destination ABSENT.
        git(
            &lane,
            &[
                "config",
                "--add",
                "remote.origin.fetch",
                &format!("+{merge_m}:refs/tags/keep"),
            ],
        );
        let before = refs_listing(&lane);
        let request = remuda_protocol::GateResolveLandingParams {
            generation: None,
            job_id: "gjb_r31_map_absent".into(),
            repo_path: lane.to_string_lossy().into_owned(),
            base_branch: "main".into(),
            merge_sha: merge_m.clone(),
        };
        let reply = super::resolve_landing_after_restart(&request, true);
        assert_eq!(reply.status, "landed", "{reply:?}");
        assert_eq!(
            refs_listing(&lane),
            before,
            "an absent configured destination must not be created"
        );
        assert!(
            block_git(
                &lane,
                &["rev-parse", "--verify", "--quiet", "refs/tags/keep"]
            )
            .is_err(),
            "refs/tags/keep must not exist"
        );

        // (b) Destination EXISTING at B: must not be force-updated to M.
        git(&lane, &["tag", "keep", &base_b]);
        let before = refs_listing(&lane);
        let request = remuda_protocol::GateResolveLandingParams {
            generation: None,
            job_id: "gjb_r31_map_existing".into(),
            repo_path: lane.to_string_lossy().into_owned(),
            base_branch: "main".into(),
            merge_sha: merge_m.clone(),
        };
        let reply = super::resolve_landing_after_restart(&request, true);
        assert_eq!(reply.status, "landed", "{reply:?}");
        assert_eq!(refs_listing(&lane), before);
        assert_eq!(
            git(&lane, &["rev-parse", "refs/tags/keep"]),
            base_b,
            "the existing tag must not be force-updated"
        );

        // (c) fetch.prune=true with a stale remote-tracking ref: --no-prune
        // keeps it.
        git(&lane, &["config", "fetch.prune", "true"]);
        git(&lane, &["update-ref", "refs/remotes/origin/gone", &base_b]);
        let before = refs_listing(&lane);
        let request = remuda_protocol::GateResolveLandingParams {
            generation: None,
            job_id: "gjb_r31_prune".into(),
            repo_path: lane.to_string_lossy().into_owned(),
            base_branch: "main".into(),
            merge_sha: merge_m.clone(),
        };
        let reply = super::resolve_landing_after_restart(&request, true);
        assert_eq!(reply.status, "landed", "{reply:?}");
        assert_eq!(
            refs_listing(&lane),
            before,
            "a stale tracking ref must survive despite fetch.prune=true"
        );
    }

    /// Build a populated-submodule scenario for R11 item 2: returns
    /// (keep dirs, superproject lane, submodule lane, S0). The superproject
    /// remote main is at B; the submodule remote has advanced to S1 after the
    /// recursive clone (its lane's tracking ref still points at S0). Both lanes
    /// allow file:// transports (test-only) and the super is configured to
    /// recurse submodules on fetch.
    #[cfg(unix)]
    fn populated_submodule_fixture() -> (TempDir, TempDir, PathBuf, PathBuf, String) {
        let dir = TempDir::new().unwrap();
        let bare = TempDir::new().unwrap();
        let root = dir.path();
        let sub_seed = root.join("sub-seed");
        let sub_bare = bare.path().join("sub-origin.git");
        let sup_seed = root.join("sup-seed");
        let sup_bare = bare.path().join("sup-origin.git");
        let lane = root.join("lane");
        let sub_lane = lane.join("sub");

        std::process::Command::new("git")
            .args([
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                sub_bare.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["init", "-q", "-b", "main", sub_seed.to_str().unwrap()])
            .output()
            .unwrap();
        git(&sub_seed, &["config", "user.email", "t@example.com"]);
        git(&sub_seed, &["config", "user.name", "T"]);
        std::fs::write(sub_seed.join("s"), "s0\n").unwrap();
        git(&sub_seed, &["add", "s"]);
        git(&sub_seed, &["commit", "-q", "-m", "S0"]);
        let s0 = git(&sub_seed, &["rev-parse", "main"]);
        git(
            &sub_seed,
            &["remote", "add", "origin", sub_bare.to_str().unwrap()],
        );
        git(&sub_seed, &["push", "-q", "origin", "main"]);

        std::process::Command::new("git")
            .args([
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                sup_bare.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["init", "-q", "-b", "main", sup_seed.to_str().unwrap()])
            .output()
            .unwrap();
        git(&sup_seed, &["config", "user.email", "t@example.com"]);
        git(&sup_seed, &["config", "user.name", "T"]);
        std::fs::write(sup_seed.join("f"), "b\n").unwrap();
        git(&sup_seed, &["add", "f"]);
        git(&sup_seed, &["commit", "-q", "-m", "B"]);
        git(
            &sup_seed,
            &["remote", "add", "origin", sup_bare.to_str().unwrap()],
        );
        // Register the submodule (file:// transport allowed for the test).
        git(
            &sup_seed,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub_bare.to_str().unwrap(),
                "sub",
            ],
        );
        git(&sup_seed, &["commit", "-q", "-m", "add submodule"]);
        git(&sup_seed, &["push", "-q", "origin", "main"]);

        // Recursive clone populates the submodule at S0.
        git(
            root,
            &[
                "-c",
                "protocol.file.allow=always",
                "clone",
                "-q",
                "--recurse-submodules",
                sup_bare.to_str().unwrap(),
                lane.to_str().unwrap(),
            ],
        );
        git(&lane, &["config", "user.email", "t@example.com"]);
        git(&lane, &["config", "user.name", "T"]);
        git(&lane, &["config", "protocol.file.allow", "always"]);
        git(&sub_lane, &["config", "user.email", "t@example.com"]);
        git(&sub_lane, &["config", "user.name", "T"]);
        git(&sub_lane, &["config", "protocol.file.allow", "always"]);
        // Recursion enabled in config; hardening must still suppress it.
        git(&lane, &["config", "fetch.recurseSubmodules", "true"]);
        // Submodule remote advances to S1 after the clone.
        std::fs::write(sub_seed.join("s"), "s1\n").unwrap();
        git(&sub_seed, &["add", "s"]);
        git(&sub_seed, &["commit", "-q", "-m", "S1"]);
        git(&sub_seed, &["push", "-q", "origin", "main"]);

        (dir, bare, lane, sub_lane, s0)
    }

    /// R11 item 2: with fetch.recurseSubmodules=true and an advanced submodule
    /// remote, recovery fetch must NOT recurse: the submodule's tracking refs
    /// and the superproject's refs both stay byte-identical. An unreachable
    /// submodule remote likewise must not fail recovery.
    #[cfg(unix)]
    #[test]
    fn recovery_does_not_recurse_into_submodules_and_survives_dead_sub_remote() {
        let (_dir, _bare, lane, sub_lane, _s0) = populated_submodule_fixture();
        // Superproject remote advances to M (contains the base) so recovery
        // certifies Landed.
        std::fs::write(lane.join("m.txt"), "M\n").unwrap();
        git(&lane, &["add", "m.txt"]);
        git(&lane, &["commit", "-q", "-m", "M"]);
        let merge_m = git(&lane, &["rev-parse", "main"]);
        // Publish via a throwaway working clone of the bare super origin.
        let publish = _dir.path().join("sup-publish");
        let sup_bare = _bare.path().join("sup-origin.git");
        git(
            _dir.path(),
            &[
                "clone",
                "-q",
                sup_bare.to_str().unwrap(),
                publish.to_str().unwrap(),
            ],
        );
        git(&publish, &["config", "user.email", "t@example.com"]);
        git(&publish, &["config", "user.name", "T"]);
        std::fs::write(publish.join("m.txt"), "M\n").unwrap();
        git(&publish, &["add", "m.txt"]);
        git(&publish, &["commit", "-q", "-m", "M"]);
        git(&publish, &["push", "-q", "origin", "main"]);

        let sup_before = refs_listing(&lane);
        let sub_before = refs_listing(&sub_lane);
        // The submodule's remote really is ahead of its lane tracking ref.
        let sub_local = git(&sub_lane, &["rev-parse", "refs/remotes/origin/main"]);
        let sub_remote = git(&sub_lane, &["ls-remote", "origin", "refs/heads/main"]);
        assert_ne!(sub_local, sub_remote);

        let request = remuda_protocol::GateResolveLandingParams {
            generation: None,
            job_id: "gjb_r32_sub".into(),
            repo_path: lane.to_string_lossy().into_owned(),
            base_branch: "main".into(),
            merge_sha: merge_m.clone(),
        };
        let reply = super::resolve_landing_after_restart(&request, true);
        assert_eq!(reply.status, "landed", "{reply:?}");
        assert_eq!(refs_listing(&lane), sup_before, "super refs changed");
        assert_eq!(
            refs_listing(&sub_lane),
            sub_before,
            "recovery recursion updated the submodule refs"
        );

        // An unreachable submodule remote does not fail recovery (no
        // recursion means it is never contacted).
        git(
            &sub_lane,
            &[
                "remote",
                "set-url",
                "origin",
                _dir.path().join("no-such-sub.git").to_str().unwrap(),
            ],
        );
        let reply = super::resolve_landing_after_restart(&request, true);
        assert_eq!(
            reply.status, "landed",
            "dead submodule remote must not fail recovery: {reply:?}"
        );
        assert_eq!(refs_listing(&lane), sup_before);
        assert_eq!(refs_listing(&sub_lane), sub_before);
    }

    /// R11 item 3: recovery fetch must never start automatic gc/maintenance,
    /// which git can DETACH outside the R6 owner group and deadline. A repo
    /// configured with gc.auto=1 / autoPackLimit=1 / maintenance.auto=true
    /// triggers `git maintenance run --auto --detach` on an unhardened fetch;
    /// the hardened recovery trace must contain no such child process.
    #[cfg(unix)]
    #[test]
    fn recovery_fetch_never_starts_gc_or_maintenance() {
        let (dir, _bare, lane, _base) = recovery_branch_fixture("main", None);
        // Aggressive auto-maintenance configuration.
        git(&lane, &["config", "gc.auto", "1"]);
        git(&lane, &["config", "gc.autoPackLimit", "1"]);
        git(&lane, &["config", "maintenance.auto", "true"]);
        // Several loose/pack objects so the threshold is crossed.
        for i in 0..3 {
            let name = format!("extra-{i}.txt");
            let f = lane.join(&name);
            std::fs::write(f, format!("x{i}\n")).unwrap();
            let msg = format!("extra {i}");
            git(&lane, &["add", &name]);
            git(&lane, &["commit", "-q", "-m", &msg]);
        }
        let trace = dir.path().join("recovery-trace.log");
        let hardening = remuda_unix::RecoveryGit::traced(&trace);
        let deadline = std::time::Instant::now() + LANDING_PROBE_DEADLINE;
        let sha = super::recovery_fetch_base_sha(&lane, "main", deadline, &hardening)
            .expect("hardened recovery succeeds");
        assert_eq!(sha.len(), 40);
        let trace_text = std::fs::read_to_string(&trace).unwrap();
        assert!(
            !trace_text.contains("maintenance run"),
            "recovery detached maintenance: {trace_text}"
        );
        assert!(
            !trace_text.contains("git gc ") && !trace_text.contains("git-gc"),
            "recovery started gc: {trace_text}"
        );
    }

    /// r33 R11 clarification: recovery creates NO scratch path and is immune to
    /// a graft planted at the OLD r32 scratch location. Builds genuinely
    /// divergent remote R (does not contain granted M), then plants BOTH
    /// `<repo>/.git-r32-recovery-no-hooks/nonexistent-grafts` (the path r32 used
    /// to trust) and a legacy `.git/info/grafts`, plus a valuable file in that
    /// directory and a reference-transaction hook. Recovery must answer
    /// Absent/Unknown (never Landed), run no hook, touch none of the planted
    /// paths, and leave the working tree / .git (minus fetched objects and
    /// FETCH_HEAD) byte-for-byte unchanged.
    #[cfg(unix)]
    #[test]
    fn planted_grafts_at_old_scratch_path_and_hooks_cannot_affect_recovery() {
        let (dir, _bare, lane, base_b) = recovery_branch_fixture("main", None);
        let seed = dir.path().join("seed");
        // Granted M on a side branch.
        git(&lane, &["checkout", "-q", "-b", "work"]);
        std::fs::write(lane.join("m.txt"), "M\n").unwrap();
        git(&lane, &["add", "m.txt"]);
        git(&lane, &["commit", "-q", "-m", "granted merge M"]);
        let merge_m = git(&lane, &["rev-parse", "HEAD"]);
        // Real remote tip R diverges from M.
        std::fs::write(seed.join("r.txt"), "R\n").unwrap();
        git(&seed, &["add", "r.txt"]);
        git(&seed, &["commit", "-q", "-m", "R"]);
        let remote_r = git(&seed, &["rev-parse", "main"]);
        git(&seed, &["push", "-q", "origin", "main"]);
        run_git(
            &lane,
            &[
                "fetch",
                "--no-tags",
                "--no-prune",
                "--refmap=",
                "origin",
                &remote_r,
            ],
        )
        .unwrap();
        assert!(
            block_git(&lane, &["merge-base", "--is-ancestor", &merge_m, &remote_r]).is_err(),
            "R must genuinely lack M"
        );

        // (a) Plant grafts at BOTH locations the unhardened code would read.
        let old_scratch = lane.join(".git-r32-recovery-no-hooks");
        std::fs::create_dir_all(&old_scratch).unwrap();
        let planted = old_scratch.join("nonexistent-grafts");
        std::fs::write(&planted, format!("{remote_r} {merge_m}\n")).unwrap();
        std::fs::write(
            lane.join(".git/info/grafts"),
            format!("{remote_r} {merge_m}\n"),
        )
        .unwrap();
        git(&lane, &["config", "advice.graftFileDeprecated", "false"]);
        // (b) Valuable pre-existing content recovery must preserve.
        let valuable = old_scratch.join("valuable-file");
        std::fs::write(&valuable, "do not delete me\n").unwrap();
        // (c) A reference-transaction hook that records when it runs.
        let marker = dir.path().join("hook-marker");
        let hook = lane.join(".git/hooks/reference-transaction");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\necho fired >> {}\n", marker.display()),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Positive control: with hooksPath at the real hooks dir and a
        // ref-writing fetch, the hook DOES fire and writes the marker.
        run_git(
            &lane,
            &[
                "-c",
                "core.hooksPath=.git/hooks",
                "fetch",
                "--no-tags",
                "origin",
                &format!("+{remote_r}:refs/heads/probe"),
            ],
        )
        .unwrap();
        assert!(
            marker.exists(),
            "control: reference-transaction hook fires with real hooksPath"
        );
        run_git(
            &lane,
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "update-ref",
                "-d",
                "refs/heads/probe",
            ],
        )
        .unwrap();
        // Snapshot firings after the hardened cleanup.
        let control_firings = std::fs::read_to_string(&marker).unwrap().lines().count();
        // Snapshot the working tree and .git (objects/ and FETCH_HEAD are the
        // only paths a ref-free recovery legitimately touches).
        let work_before = git_dir_snapshot(&lane, &[]);
        let git_excludes = ["objects".to_owned(), "FETCH_HEAD".to_owned()];
        let git_before = git_dir_snapshot(&lane.join(".git"), &git_excludes);

        // With death evidence: Absent, never Landed despite both planted grafts.
        let request = remuda_protocol::GateResolveLandingParams {
            generation: None,
            job_id: "gjb_r33_plant".into(),
            repo_path: lane.to_string_lossy().into_owned(),
            base_branch: "main".into(),
            merge_sha: merge_m.clone(),
        };
        let reply = super::resolve_landing_after_restart(&request, true);
        assert_eq!(
            reply.status, "absent",
            "planted grafts must not fabricate Landed: {reply:?}"
        );
        // Without death evidence: Unknown.
        let reply = super::resolve_landing_after_restart(&request, false);
        assert_eq!(reply.status, "unknown", "{reply:?}");
        // (c) no extra hook firings from recovery.
        let after_firings = std::fs::read_to_string(&marker)
            .map(|t| t.lines().count())
            .unwrap_or(0);
        assert_eq!(
            after_firings, control_firings,
            "recovery must not run any repository hook"
        );
        // (b) planted paths preserved exactly.
        assert_eq!(
            std::fs::read_to_string(&valuable).unwrap(),
            "do not delete me\n",
            "pre-existing valuable file must survive unchanged"
        );
        assert_eq!(
            std::fs::read_to_string(&planted).unwrap(),
            format!("{remote_r} {merge_m}\n"),
            "the planted graft path must be left in place"
        );
        assert_eq!(
            git_dir_snapshot(&lane, &[]),
            work_before,
            "working tree changed"
        );
        assert_eq!(
            git_dir_snapshot(&lane.join(".git"), &git_excludes),
            git_before,
            ".git changed apart from objects/ and FETCH_HEAD"
        );
        let _ = base_b;
    }

    /// Snapshot a directory: relative path -> contents hash for files, with
    /// top-level entries in `excludes` skipped. Used to prove recovery leaves
    /// pre-existing repository content byte-identical.
    #[cfg(unix)]
    fn git_dir_snapshot(root: &Path, excludes: &[String]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let entry = entry.unwrap();
                let rel = entry.path().strip_prefix(root).unwrap().to_path_buf();
                if rel.components().count() == 1 && excludes.iter().any(|e| rel == Path::new(e)) {
                    continue;
                }
                let kind = entry.file_type().unwrap();
                if kind.is_dir() {
                    stack.push(entry.path());
                } else if kind.is_file() {
                    let bytes = std::fs::read(entry.path()).unwrap();
                    let hash = std::collections::hash_map::DefaultHasher::new();
                    use std::hash::{Hash, Hasher};
                    let mut hasher = hash;
                    bytes.hash(&mut hasher);
                    out.push((
                        rel.to_string_lossy().into_owned(),
                        format!("{:x}", hasher.finish()),
                    ));
                }
            }
        }
        out.sort();
        out
    }

    /// r33 item 2: drive the REAL production recovery over success,
    /// object-fetch failure, negative-ancestry and shallow paths, and assert
    /// EVERY git command spawned at the owner boundary carries the full R11
    /// hardening set. A caller bypassing the builder would miss entries here.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_production_recovery_invocation_carries_full_hardening() {
        /// Re-run assertions over one recorded production recovery.
        fn assert_recorded_hardening(log: &[remuda_unix::RecordedGit]) {
            assert!(
                !log.is_empty(),
                "recovery must spawn at least one git command"
            );
            for (index, rec) in log.iter().enumerate() {
                for (key, value) in remuda_unix::RecoveryGit::env_pairs() {
                    assert!(
                        rec.env.iter().any(|(k, v)| k == key && v == value),
                        "recorded command {index} missing env {key}={value}: {rec:?}"
                    );
                }
                // Exactly four leading "-c key=value" pairs, in order.
                let pairs = remuda_unix::RecoveryGit::config_pairs();
                assert!(
                    rec.args.len() >= 2 * pairs.len(),
                    "recorded command {index} too short for hardening prefix: {rec:?}"
                );
                for (pair_index, (key, value)) in pairs.iter().enumerate() {
                    assert_eq!(
                        rec.args[2 * pair_index],
                        "-c",
                        "command {index}: pair {pair_index}"
                    );
                    assert_eq!(
                        rec.args[2 * pair_index + 1],
                        format!("{key}={value}"),
                        "command {index}: pair {pair_index} value"
                    );
                }
                // Fetch carries every fetch-only flag.
                if rec.args[2 * pairs.len()] == "fetch" {
                    for flag in remuda_unix::RECOVERY_FETCH_FLAGS {
                        assert!(
                            rec.args.iter().any(|a| a == flag),
                            "recorded fetch {index} missing {flag}: {rec:?}"
                        );
                    }
                }
            }
        }

        // r34 item 4: one RAII recorder scope for the whole test; each phase
        // asserts only commands whose cwd is its fixture repo (parallel tests'
        // subprocesses in other cwds are ignored; hardening is never filtered).
        let recorder = remuda_unix::RecordingGuard::install();

        // (1) SUCCESS: real divergent-but-contained tip, death held -> Landed.
        let (dir, _bare, lane, _base) = recovery_branch_fixture("main", None);
        let seed = dir.path().join("seed");
        git(&lane, &["checkout", "-q", "-b", "work"]);
        std::fs::write(lane.join("m.txt"), "M\n").unwrap();
        git(&lane, &["add", "m.txt"]);
        git(&lane, &["commit", "-q", "-m", "M"]);
        let merge_m = git(&lane, &["rev-parse", "HEAD"]);
        std::fs::write(seed.join("r.txt"), "R\n").unwrap();
        git(&seed, &["add", "r.txt"]);
        git(&seed, &["commit", "-q", "-m", "R"]);
        git(&seed, &["push", "-q", "origin", "main"]);
        let remote_r = git(&seed, &["rev-parse", "main"]);
        // Merge R into M so the advertised remote tip D contains M.
        git(&seed, &["fetch", "-q", lane.to_str().unwrap(), &merge_m]);
        git(&seed, &["merge", "-q", "--no-ff", "-m", "D", &merge_m]);
        git(&seed, &["push", "-q", "origin", "main"]);
        let remote_d = git(&seed, &["rev-parse", "main"]);
        let request = remuda_protocol::GateResolveLandingParams {
            generation: None,
            job_id: "gjb_r33_rec_ok".into(),
            repo_path: lane.to_string_lossy().into_owned(),
            base_branch: "main".into(),
            merge_sha: merge_m.clone(),
        };
        let reply = super::resolve_landing_after_restart(&request, true);
        assert_eq!(reply.status, "landed", "{reply:?}");
        assert_eq!(reply.base_sha.as_deref(), Some(remote_d.as_str()));
        assert_recorded_hardening(&recorder.for_repo(&lane));

        // (2) NEGATIVE ANCESTRY without death -> Unknown: an orphan commit the
        // advertised D cannot contain (the lane holds its objects locally, so
        // the fetch short-circuits but the real merge-base runs and fails).
        let orphan_tree = git(&lane, &["rev-parse", "HEAD^{tree}"]);
        let orphan = git(&lane, &["commit-tree", &orphan_tree, "-m", "orphan grant"]);
        let negative_request = remuda_protocol::GateResolveLandingParams {
            generation: None,
            job_id: "gjb_r33_rec_negative".into(),
            repo_path: lane.to_string_lossy().into_owned(),
            base_branch: "main".into(),
            merge_sha: orphan,
        };
        let reply = super::resolve_landing_after_restart(&negative_request, false);
        assert_eq!(reply.status, "unknown", "{reply:?}");
        assert_recorded_hardening(&recorder.for_repo(&lane));

        // (3) OBJECT-FETCH FAILURE after successful listing: publish one more
        // tip E the lane genuinely lacks, then at the read seam swap to an
        // empty remote so the fetch fails deterministically ("not our ref").
        use super::recovery_read_seam as seam;
        std::fs::write(
            seed.join("e.txt"),
            "E
",
        )
        .unwrap();
        git(&seed, &["add", "e.txt"]);
        git(&seed, &["commit", "-q", "-m", "E"]);
        git(&seed, &["push", "-q", "origin", "main"]);
        git(
            &lane,
            &[
                "remote",
                "set-url",
                "origin",
                _bare.path().join("origin.git").to_str().unwrap(),
            ],
        );
        let empty = dir.path().join("empty.git");
        std::process::Command::new("git")
            .args([
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                empty.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        seam::arm(&lane, 1);
        let lane_t = lane.clone();
        let request_t = remuda_protocol::GateResolveLandingParams {
            generation: None,
            job_id: "gjb_r33_rec_fetchfail".into(),
            repo_path: lane.to_string_lossy().into_owned(),
            base_branch: "main".into(),
            merge_sha: merge_m.clone(),
        };
        let probe =
            std::thread::spawn(move || super::resolve_landing_after_restart(&request_t, true));
        seam::wait_arrived(&lane, 0);
        git(
            &lane,
            &["remote", "set-url", "origin", empty.to_str().unwrap()],
        );
        seam::release(&lane, 0);
        let reply = probe.join().unwrap();
        seam::disarm(&lane);
        assert_eq!(reply.status, "unknown", "{reply:?}");
        assert!(
            reply
                .error
                .as_deref()
                .unwrap_or("")
                .contains("object fetch"),
            "{reply:?}"
        );
        assert_recorded_hardening(&recorder.for_repo(&lane));

        // (4) SHALLOW: a depth-1 clone, an orphan commit -> Unknown naming the
        // shallow condition; every command (including the shallow check) is
        // hardened.
        let fixture2 = fixture(PASSING_SCRIPT);
        let origin2 = fixture2._bare.path().join("origin.git");
        let lane2 = fixture2._dir.path().join("shallow-r33");
        shallow_clone(&origin2, &lane2, "1");
        git(&lane2, &["config", "user.email", "t@example.com"]);
        git(&lane2, &["config", "user.name", "T"]);
        let tree = git(&lane2, &["rev-parse", "origin/main^{tree}"]);
        let orphan = git(&lane2, &["commit-tree", &tree, "-m", "orphan"]);
        let request2 = remuda_protocol::GateResolveLandingParams {
            generation: None,
            job_id: "gjb_r33_rec_shallow".into(),
            repo_path: lane2.to_string_lossy().into_owned(),
            base_branch: "main".into(),
            merge_sha: orphan,
        };
        let reply = super::resolve_landing_after_restart(&request2, true);
        assert_eq!(reply.status, "unknown", "{reply:?}");
        assert!(
            reply.error.as_deref().unwrap_or("").contains("shallow"),
            "{reply:?}"
        );
        assert_recorded_hardening(&recorder.for_repo(&lane2));

        let _ = remote_r;
        let _ = lane_t;
    }

    /// R11 item 1 (HIGH): a local replacement graft (or legacy
    /// .git/info/grafts) must not fabricate ancestry. Real remote tip R
    /// diverged from the granted M (R does not contain M); with `git replace
    /// --graft R M` an ordinary merge-base falsely reports M an ancestor, so
    /// unhardened recovery would return Landed. Hardened recovery disables
    /// replacements and grafts and returns Absent (death evidence is held),
    /// never Landed.
    #[cfg(unix)]
    #[test]
    fn a_local_replace_or_graft_cannot_fabricate_landed() {
        let (_dir, _bare, lane, base_b) = recovery_branch_fixture("main", None);
        let seed = _dir.path().join("seed");
        // Make sure R's objects are present in the lane (needed for a valid
        // replacement) by publishing R first and object-fetching it, and make
        // the granted M locally on the SAME base so both are full commits.
        // Granted merge M on a side branch, not pushed to the remote.
        git(&lane, &["checkout", "-q", "-b", "work"]);
        std::fs::write(lane.join("m.txt"), "M\n").unwrap();
        git(&lane, &["add", "m.txt"]);
        git(&lane, &["commit", "-q", "-m", "granted merge M"]);
        let merge_m = git(&lane, &["rev-parse", "HEAD"]);
        assert_ne!(merge_m, base_b);
        // Remote main independently advances to R, which does NOT contain M.
        std::fs::write(seed.join("r.txt"), "R\n").unwrap();
        git(&seed, &["add", "r.txt"]);
        git(&seed, &["commit", "-q", "-m", "R"]);
        let remote_r = git(&seed, &["rev-parse", "main"]);
        git(&seed, &["push", "-q", "origin", "main"]);
        // Bring R's real objects into the lane without writing refs.
        run_git(
            &lane,
            &[
                "fetch",
                "--no-tags",
                "--no-prune",
                "--refmap=",
                "origin",
                &remote_r,
            ],
        )
        .unwrap();
        // Recovery fetches R's objects (R is the real tip; M's objects are
        // already local from the lane's own commit).
        let request = remuda_protocol::GateResolveLandingParams {
            generation: None,
            job_id: "gjb_r32_repl".into(),
            repo_path: lane.to_string_lossy().into_owned(),
            base_branch: "main".into(),
            merge_sha: merge_m.clone(),
        };
        // Baseline: R genuinely does not contain M -> Absent (death held).
        assert!(
            block_git(&lane, &["merge-base", "--is-ancestor", &merge_m, &remote_r]).is_err(),
            "real remote R must not contain M before the graft"
        );

        for graft in ["replace", "legacy-graft"] {
            // Attack setup uses raw commands: the replacement may or may not
            // already exist at the top of the loop, and test git() asserts
            // success (it is the strict fixture helper).
            let raw_git = |args: &[&str]| {
                let _ = std::process::Command::new("git")
                    .current_dir(&lane)
                    .args(args)
                    .env("GIT_TERMINAL_PROMPT", "0")
                    .status();
            };
            raw_git(&["replace", "-d", &remote_r]);
            std::fs::remove_file(lane.join(".git/info/grafts")).ok();
            if graft == "replace" {
                // Fabricate ancestry: replace R's parent chain with M.
                git(&lane, &["replace", "--graft", &remote_r, &merge_m]);
            } else {
                // Same attack through the deprecated grafts file.
                let grafts = lane.join(".git/info/grafts");
                std::fs::create_dir_all(grafts.parent().unwrap()).unwrap();
                std::fs::write(&grafts, format!("{remote_r} {merge_m}\n")).unwrap();
                git(&lane, &["config", "advice.graftFileDeprecated", "false"]);
            }
            // The vulnerable command now falsely sees M in R.
            assert!(
                block_git(&lane, &["merge-base", "--is-ancestor", &merge_m, &remote_r]).is_ok(),
                "sanity: {graft} fabricates M as an ancestor of R"
            );
            // Hardened recovery is NOT fooled: Absent with death evidence.
            let reply = super::resolve_landing_after_restart(&request, true);
            assert_eq!(
                reply.status, "absent",
                "hardened recovery must not report Landed through a {graft}: {reply:?}"
            );
            assert_eq!(reply.base_sha.as_deref(), Some(remote_r.as_str()));
        }
        // Clear all attack state for the final Unknown assertion.
        let _ = std::process::Command::new("git")
            .current_dir(&lane)
            .args(["replace", "-d", &remote_r])
            .status();
        std::fs::remove_file(lane.join(".git/info/grafts")).ok();
        git(&lane, &["replace", "--graft", &remote_r, &merge_m]);
        let reply = super::resolve_landing_after_restart(&request, false);
        assert_eq!(reply.status, "unknown", "{reply:?}");
        // The recovery scratch hooks directory must not be left behind.
        assert!(
            !lane.join(".git-r32-recovery-no-hooks").exists(),
            "recovery must clean its hardening scratch dir"
        );
    }

    /// R10: a configured `remote.origin.tagOpt=--tags` with a clobbered local
    /// tag used to make fetch fail — recovery now uses --no-tags, so it still
    /// certifies Landed, and the clobbered tag and every other ref stay
    /// byte-identical.
    #[cfg(unix)]
    #[test]
    fn a_clobbered_tag_with_tagopt_does_not_break_ref_free_recovery() {
        let (dir, _bare, lane, base_b) = recovery_branch_fixture("main", None);
        let seed = dir.path().join("seed");
        // Local lane tag pinned at B; remote tag moved sideways AFTER the
        // clone; remote main advances to M containing B.
        git(&lane, &["tag", "v1", &base_b]);
        let orphan = git(&seed, &["commit-tree", "HEAD^{tree}", "-m", "sideways tag"]);
        git(
            &seed,
            &[
                "push",
                "-q",
                "-f",
                "origin",
                &format!("{orphan}:refs/tags/v1"),
            ],
        );
        std::fs::write(seed.join("m.txt"), "M\n").unwrap();
        git(&seed, &["add", "m.txt"]);
        git(&seed, &["commit", "-q", "-m", "M"]);
        let merge_m = git(&seed, &["rev-parse", "main"]);
        git(&seed, &["push", "-q", "origin", "main"]);
        git(&lane, &["config", "remote.origin.tagOpt", "--tags"]);
        // Sanity: a PLAIN fetch honoring tagOpt still fails on the clobber.
        let status = std::process::Command::new("git")
            .current_dir(&lane)
            .args(["fetch", "-q", "origin"])
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "the plain fetch must still hit the tag clobber"
        );
        let before = refs_listing(&lane);

        let request = remuda_protocol::GateResolveLandingParams {
            generation: None,
            job_id: "gjb_r30_tagclobber".into(),
            repo_path: lane.to_string_lossy().into_owned(),
            base_branch: "main".into(),
            merge_sha: merge_m.clone(),
        };
        let reply = super::resolve_landing_after_restart(&request, true);
        assert_eq!(
            reply.status, "landed",
            "--no-tags bypasses the clobber; M is on remote main: {reply:?}"
        );
        assert_eq!(
            reply.base_sha.as_deref(),
            Some(merge_m.as_str()),
            "{reply:?}"
        );
        assert_eq!(
            refs_listing(&lane),
            before,
            "the tag and every other ref must be untouched"
        );
        // The clobbered tag really is still pinned at B.
        assert_eq!(git(&lane, &["rev-parse", "refs/tags/v1"]), base_b);
    }

    /// R10: a recovery whose base the remote does not advertise (ls-remote
    /// fails / finds nothing) is Unknown and creates, updates or deletes no
    /// ref — the failure path is as ref-free as the success path.
    #[cfg(unix)]
    #[test]
    fn a_failed_ref_free_recovery_writes_no_refs() {
        let (dir, _bare, lane, base_b) = recovery_branch_fixture("main", None);
        let seed = dir.path().join("seed");
        std::fs::write(seed.join("m.txt"), "M\n").unwrap();
        git(&seed, &["add", "m.txt"]);
        git(&seed, &["commit", "-q", "-m", "M"]);
        let merge_m = git(&seed, &["rev-parse", "main"]);
        git(&seed, &["push", "-q", "origin", "main"]);
        let before = refs_listing(&lane);

        // (a) Base not advertised by the remote at all.
        let request = remuda_protocol::GateResolveLandingParams {
            generation: None,
            job_id: "gjb_r30_nobase".into(),
            repo_path: lane.to_string_lossy().into_owned(),
            base_branch: "no-such-base-r30".into(),
            merge_sha: merge_m.clone(),
        };
        let reply = super::resolve_landing_after_restart(&request, true);
        assert_eq!(reply.status, "unknown", "{reply:?}");
        assert!(
            reply
                .error
                .as_deref()
                .unwrap_or("")
                .contains("does not advertise"),
            "the Unknown names the missing advertised base: {reply:?}"
        );
        assert_eq!(refs_listing(&lane), before);

        // (b) Origin unreachable: ls-remote cannot connect at all.
        git(
            &lane,
            &[
                "remote",
                "set-url",
                "origin",
                dir.path().join("no-such-origin-r30.git").to_str().unwrap(),
            ],
        );
        let request = remuda_protocol::GateResolveLandingParams {
            generation: None,
            job_id: "gjb_r30_nohost".into(),
            repo_path: lane.to_string_lossy().into_owned(),
            base_branch: "main".into(),
            merge_sha: merge_m.clone(),
        };
        let reply = super::resolve_landing_after_restart(&request, true);
        assert_eq!(
            reply.status, "unknown",
            "an unreachable remote stays Unknown: {reply:?}"
        );
        assert_eq!(refs_listing(&lane), before, "{}", base_b);
    }

    /// r31 item 4: ls-remote must SUCCEED with a real advertised SHA and the
    /// OBJECT FETCH then fail deterministically — proving the failure path
    /// after listing is ref-free. At the read seam (after ls-remote, before
    /// fetch) point origin at a fresh empty bare repo: the advertised SHA is
    /// not served there, so upload-pack rejects it. Reaching the seam proves
    /// fetch was about to run; the Unknown result and unchanged refs prove it
    /// failed without writing anything.
    #[cfg(unix)]
    #[test]
    fn a_failed_object_fetch_after_successful_ls_remote_writes_no_refs() {
        use super::recovery_read_seam as seam;
        let (dir, _bare, lane, _base_b) = recovery_branch_fixture("main", None);
        let seed = dir.path().join("seed");
        std::fs::write(seed.join("m.txt"), "M\n").unwrap();
        git(&seed, &["add", "m.txt"]);
        git(&seed, &["commit", "-q", "-m", "M"]);
        let merge_m = git(&seed, &["rev-parse", "main"]);
        git(&seed, &["push", "-q", "origin", "main"]);
        // The lane genuinely lacks M (the clone predates the push), so the
        // object fetch cannot short-circuit.
        assert!(
            block_git(&lane, &["cat-file", "-e", &format!("{merge_m}^{{commit}}")]).is_err(),
            "the lane must genuinely lack M for the fetch to run"
        );
        // A second, EMPTY bare remote that will refuse the advertised SHA.
        let empty = dir.path().join("empty.git");
        std::process::Command::new("git")
            .args([
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                empty.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        let before = refs_listing(&lane);

        seam::arm(&lane, 1);
        let lane_t = lane.clone();
        let merge_t = merge_m.clone();
        let probe = std::thread::spawn(move || {
            let request = remuda_protocol::GateResolveLandingParams {
                generation: None,
                job_id: "gjb_r31_fetchfail".into(),
                repo_path: lane_t.to_string_lossy().into_owned(),
                base_branch: "main".into(),
                merge_sha: merge_t,
            };
            super::resolve_landing_after_restart(&request, true)
        });
        // ls-remote against the REAL origin succeeded with M; we are parked
        // right before its object fetch.
        seam::wait_arrived(&lane, 0);
        // Sanity: the empty remote really does not serve M.
        let sanity = std::process::Command::new("git")
            .current_dir(&lane)
            .args(["ls-remote", "origin", "refs/heads/main"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&sanity.stdout).contains(&merge_m));
        // Swap origin to the empty remote, then release: fetch runs and fails.
        git(
            &lane,
            &["remote", "set-url", "origin", empty.to_str().unwrap()],
        );
        seam::release(&lane, 0);
        let reply = probe.join().expect("probe joins");
        seam::disarm(&lane);

        assert_eq!(
            reply.status, "unknown",
            "the failed fetch stays Unknown: {reply:?}"
        );
        assert!(
            reply
                .error
                .as_deref()
                .unwrap_or("")
                .contains("object fetch"),
            "the Unknown names the OBJECT fetch (listing succeeded): {reply:?}"
        );
        assert_eq!(
            refs_listing(&lane),
            before,
            "the failed object fetch created, updated or deleted no ref"
        );
    }

    /// r30 item 2: exact diagnostics that mean "git lacks SHA-256". Pure
    /// classifier with controlled strings, independent of the installed git.
    #[cfg(unix)]
    fn unsupported_sha256_diagnostic(stderr: &str) -> bool {
        super::is_unsupported_sha256_diagnostic(stderr)
    }

    /// r30 item 2 / r31 item 3: recognize ONLY the exact whole-line
    /// unsupported-format forms; a recognized phrase appearing inside a
    /// FILENAME on an unrelated error line must not match.
    #[cfg(unix)]
    #[test]
    fn sha256_unsupported_classifier_is_precise() {
        for stderr in [
            "fatal: unknown hash algorithm 'sha256'\n",
            "fatal: attempt to use sha256 has not been compiled in\n",
            "error: unknown option `object-format'\nusage: git init ...",
            "error: unknown option '--object-format=sha256'\n",
            "fatal: unknown value for --object-format: sha256\n",
        ] {
            assert!(
                unsupported_sha256_diagnostic(stderr),
                "must recognize the unsupported form: {stderr:?}"
            );
        }
        // Unrelated diagnostics — including filenames that embed the whole
        // recognized phrase — must NOT be read as unsupported SHA-256.
        for stderr in [
            "fatal: bad config line 3 in file /home/dev/invalid-sha256-config\n",
            "error: invalid key in /etc/gitconfig: foo.sha256.invalid\n",
            "fatal: bad config line 1 in file /fixtures/fatal: unknown hash algorithm 'sha256'/config\n",
            "error: cannot access /build/error: unknown option '--object-format=sha256'/src: No such file\n",
            "fatal: unknown hash algorithm 'bogus'\n",
            "fatal: unknown hash algorithm 'SHA256'\n",
            // Wrong operand: sha1 is a supported algorithm, bogus is another
            // algorithm name, sha256extra is not sha256.
            "fatal: unknown hash algorithm 'sha1'\n",
            "fatal: unknown value for --object-format: bogus\n",
            "error: unknown value for --object-format: sha1\n",
            "fatal: unknown value for --object-format: sha256extra\n",
            // Trailing text after every accepted whole-line form.
            "  fatal: unknown hash algorithm 'sha256'  extra\n",
            "fatal: unknown hash algorithm 'sha256' extra\n",
            "fatal: attempt to use sha256 has not been compiled in extra\n",
            "fatal: unknown value for --object-format: sha256 extra\n",
            "error: unknown option `object-format' extra\n",
            // A prefix line with the option but not at line start.
            " x error: unknown option `--object-format'\n",
            "",
        ] {
            assert!(
                !unsupported_sha256_diagnostic(stderr),
                "must not skip on an unrelated diagnostic: {stderr:?}"
            );
        }
    }

    /// r30 item 2: a spawn failure of the probe program is a hard test
    /// failure, never a silent skip (uses a nonexistent probe executable, so
    /// it depends on no global PATH state).
    #[cfg(unix)]
    #[test]
    fn a_sha256_probe_spawn_failure_panics() {
        let dir = TempDir::new().unwrap();
        let bogus = dir
            .path()
            .join("no-such-git-dir")
            .join("git")
            .to_string_lossy()
            .into_owned();
        let target = dir.path().join("p");
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            super::probe_sha256_support_with(&bogus, &target)
        }))
        .is_err();
        assert!(panicked, "a spawn failure must panic, not skip");
    }

    /// r28 item 2: a SHA-256 repository reports 64-hex object IDs; the
    /// ref-free recovery reads the advertised 64-hex tip and certifies Landed.
    #[cfg(unix)]
    #[test]
    fn recovery_in_a_sha256_repo_reads_a_64_hex_tip_without_refs() {
        let probe = TempDir::new().unwrap();
        let supported = super::probe_sha256_support("git", &probe.path().join("sha256-probe"));
        drop(probe);
        if !supported {
            return;
        }

        let (_dir, _bare, lane, stale_base) = recovery_branch_fixture("main", Some("sha256"));
        git(&lane, &["branch", "topic", "main"]);
        std::fs::write(lane.join("landed.txt"), "landed\n").unwrap();
        git(&lane, &["add", "landed.txt"]);
        git(&lane, &["commit", "-q", "-m", "granted merge M (sha256)"]);
        let merge_m = git(&lane, &["rev-parse", "main"]);
        assert_eq!(merge_m.len(), 64, "fixture must be SHA-256: {merge_m}");
        git(&lane, &["push", "-q", "origin", "main", "topic"]);
        // Stale post-crash checkout: tracking pinned to B, fetch restricted.
        git(
            &lane,
            &["update-ref", "refs/remotes/origin/main", &stale_base],
        );
        git(
            &lane,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/topic:refs/remotes/origin/topic",
            ],
        );
        let before = refs_listing(&lane);

        let request = remuda_protocol::GateResolveLandingParams {
            generation: None,
            job_id: "gjb_r30_sha256".into(),
            repo_path: lane.to_string_lossy().into_owned(),
            base_branch: "main".into(),
            merge_sha: merge_m.clone(),
        };
        let reply = super::resolve_landing_after_restart(&request, true);
        assert_eq!(
            reply.status, "landed",
            "a 64-hex advertised tip must not be rejected as unreadable: {reply:?}"
        );
        let base_sha = reply
            .base_sha
            .clone()
            .expect("landed reply carries the base sha");
        assert_eq!(base_sha.len(), 64, "{base_sha}");
        assert_eq!(base_sha, merge_m, "{reply:?}");
        assert_eq!(
            refs_listing(&lane),
            before,
            "the sha256 recovery created, updated or deleted no ref"
        );
        assert_eq!(
            git(&lane, &["rev-parse", "refs/remotes/origin/main"]),
            stale_base,
            "the configured tracking ref is never touched"
        );
    }

    /// R5 fail-open with a stale configured origin: a restricted
    /// `remote.origin.fetch` can make a plain `git fetch origin` leave
    /// `origin/main` stale, but recovery reads the remote tip DIRECTLY with
    /// ls-remote and certifies Landed against the advertised SHA — touching no
    /// ref at all. A base the remote does not carry is Unknown.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recovery_reads_the_advertised_base_past_a_restricted_refspec() {
        let fixture = fixture(PASSING_SCRIPT);
        let lane = fixture.lane.clone();
        let stale_base = git(&lane, &["rev-parse", "origin/main"]);
        git(&lane, &["checkout", "-q", "main"]);
        std::fs::write(lane.join("landed.txt"), "landed\n").unwrap();
        git(&lane, &["add", "landed.txt"]);
        git(&lane, &["commit", "-q", "-m", "granted merge M"]);
        let merge_sha = git(&lane, &["rev-parse", "main"]);
        assert_ne!(merge_sha, stale_base);
        git(&lane, &["push", "-q", "origin", "main"]);
        git(
            &lane,
            &["update-ref", "refs/remotes/origin/main", &stale_base],
        );
        git(
            &lane,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/wt/fake/task:refs/remotes/origin/wt/fake/task",
            ],
        );
        // Sanity: the stale-tracking fail-open shape still exists for a plain
        // fetch; recovery itself never uses this configured refspec.
        git(&lane, &["fetch", "-q", "origin"]);
        assert_eq!(
            git(&lane, &["rev-parse", "refs/remotes/origin/main"]),
            stale_base,
            "a restricted fetch must leave the origin/main tracking ref stale"
        );
        let before = refs_listing(&lane);

        // Authoritative death evidence is present; under the OLD stale-tip
        // resolver this combination certified Absent against B.
        let dir = fixture.node.gate_registry().land_fence_dir.clone();
        let record = serde_json::json!({
            "jobId": "gjb_r30_refspec", "generation": 1, "pgid": 6127, "startTick": 11,
            "mergeSha": merge_sha,
        });
        super::write_land_group_dead_cert(&dir, &record, 6127).unwrap();
        let reply = fixture
            .node
            .run_gate_resolve_landing(&serde_json::json!({
                "jobId": "gjb_r30_refspec",
                "repoPath": lane.to_string_lossy(),
                "baseBranch": "main",
                "mergeSha": merge_sha,
            }))
            .await
            .unwrap();
        assert_eq!(
            reply["status"],
            json!("landed"),
            "M is advertised on remote main; recovery must establish Landed, not Absent: {reply}"
        );
        assert_eq!(reply["baseSha"], json!(merge_sha), "{reply}");
        assert_eq!(
            refs_listing(&lane),
            before,
            "a successful probe creates, updates or deletes no ref"
        );
        assert_eq!(
            git(&lane, &["rev-parse", "refs/remotes/origin/main"]),
            stale_base,
            "recovery never touches the configured origin/main tracking ref"
        );

        // A base the remote does not advertise → Unknown, never Absent, still
        // no ref writes.
        let reply = fixture
            .node
            .run_gate_resolve_landing(&serde_json::json!({
                "jobId": "gjb_r30_refspec",
                "repoPath": lane.to_string_lossy(),
                "baseBranch": "no-such-base-r30",
                "mergeSha": merge_sha,
            }))
            .await
            .unwrap();
        assert_eq!(reply["status"], json!("unknown"), "{reply}");
        assert!(
            reply["error"]
                .as_str()
                .unwrap_or("")
                .contains("does not advertise"),
            "the Unknown names the missing advertised base: {reply}"
        );
        assert_eq!(
            refs_listing(&lane),
            before,
            "a failed probe creates, updates or deletes no ref"
        );
    }
    /// r19 item 2 / R7: a recovery fetch whose transport stalls is killed at
    /// the per-command cap (well within the probe deadline and the Hub's 30 s
    /// RPC timeout), the resolver answers Unknown BEFORE the deadline, and no
    /// fetch/transport process survives the owner's group kill.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stalled_recovery_fetch_is_unknown_before_the_deadline_with_no_leftover_process() {
        let fixture = fixture(PASSING_SCRIPT);
        let root = fixture._dir.path();
        let origin = fixture._bare.path().join("origin.git");
        let lane = root.join("stall-fetch");
        shallow_clone(&origin, &lane, "1");
        git(&lane, &["config", "user.email", "t@example.com"]);
        git(&lane, &["config", "user.name", "T"]);
        // Fake transport: git invokes this instead of ssh; it never speaks, so
        // `git fetch origin` blocks until the owner kills the group.
        let hang = root.join("hang-ssh-r19.sh");
        // Answer git's `ssh -G host` settings probe directly; only the real
        // upload-pack connection stalls.
        std::fs::write(
            &hang,
            "#!/bin/sh\ncase \"$1\" in\n  -G) printf 'hostname %s\\nuser git\\nport 22\\n' \"$2\"; exit 0 ;;\nesac\nexec sleep 47.53\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hang, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(
            &lane,
            &["remote", "set-url", "origin", "ssh://example.test/repo.git"],
        );
        git(
            &lane,
            &["config", "core.sshCommand", hang.to_str().unwrap()],
        );
        let tree = git(&lane, &["rev-parse", "origin/main^{tree}"]);
        let orphan = git(&lane, &["commit-tree", &tree, "-m", "orphan"]);

        let started = std::time::Instant::now();
        let reply = fixture
            .node
            .run_gate_resolve_landing(&serde_json::json!({
                "jobId": "gjb_stall_r7",
                "repoPath": lane.to_string_lossy(),
                "baseBranch": "main",
                "mergeSha": orphan,
            }))
            .await
            .unwrap();
        assert_eq!(reply["status"], json!("unknown"), "{reply}");
        let error = reply["error"].as_str().unwrap_or("");
        assert!(
            error.contains("ls-remote") && error.contains("probe bound"),
            "the unknown names the bounded remote read: {reply}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(13),
            "the probe must fail closed at the cap, not hang: {:?}",
            started.elapsed()
        );
        // The transport grandchild was killed with the owner's group; no live
        // marker process lingers (the git leader is reaped by the owner).
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let left = live_marker_processes("sleep 47.53");
            if left == 0 || std::time::Instant::now() >= deadline {
                assert_eq!(left, 0, "the stalled transport survived the owner kill");
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Count LIVE (non-zombie) processes whose /proc/<pid>/cmdline contains the
    /// marker — r19 item 2/3 leftover-process assertion.
    #[cfg(unix)]
    fn live_marker_processes(marker: &str) -> usize {
        std::fs::read_dir(super::proc_root())
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
                    .filter_map(|name| name.parse::<i32>().ok())
                    .filter(|pid| {
                        let base = super::proc_root().join(pid.to_string());
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
