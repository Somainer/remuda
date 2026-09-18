//! Gate-lane queue, lane runner RPC and project gate-job entity; coordinator
//! hierarchy batch 6 (co-lanes), design §3.2/§8 row 6 and D-034.
//!
//! Verify jobs may run in parallel across lanes; land jobs are serialized per
//! project with a compare-and-swap push. The Hub owns the queue; the Node owns
//! the lane checkout and runs the consumed `remuda merge --gate/--land` CLI
//! (the gate step authority stays `scripts/ci/gate.sh` in the merged tree).

use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ── Hub→Node RPC method names ──────────────────────────────────────────────

/// Hub→Node: run one queued gate/land job on a lane.
pub const METHOD_GATE_RUN: &str = "gate.run";
/// Hub→Node: cancel a running lane job (kills the step's process group).
pub const METHOD_GATE_CANCEL: &str = "gate.cancel";
/// Node→Hub: one streamed job event (a finished step, a phase, log output …).
pub const METHOD_GATE_EVENT: &str = "gate.event";
/// Hub→Node (home host): run the optional `remuda land --then` command.
pub const METHOD_GATE_THEN: &str = "gate.then";
/// Hub→Node (home host): fetch a lane's verified merge and CAS-push main.
pub const METHOD_GATE_LAND: &str = "gate.land";
/// Hub→Node (lane host): drop a job's persisted merge refs.
pub const METHOD_GATE_UNPIN: &str = "gate.unpin";

/// Every Hub→Node gate RPC (stdio allowlist predicate helper).
#[must_use]
pub fn is_gate_call(method: &str) -> bool {
    matches!(
        method,
        METHOD_GATE_RUN
            | METHOD_GATE_CANCEL
            | METHOD_GATE_THEN
            | METHOD_GATE_LAND
            | METHOD_GATE_UNPIN
    )
}

/// Ref pinning a passing lane verify's merge commit in the lane repo, so the
/// commit outlives the scratch worktree and can be fetched by the home host
/// (evidence: gate-lane-2).
#[must_use]
pub fn gate_merge_ref(job_id: &str) -> String {
    format!("refs/remuda/gate/{job_id}")
}

/// Companion ref pinning the verified branch tip.
///
/// The suffix is `.branch`, not `/branch`: git refuses a ref that is both a
/// file and a directory, so `refs/remuda/gate/<id>` and
/// `refs/remuda/gate/<id>/branch` cannot both exist ("cannot lock ref …
/// exists; cannot create"). A sibling leaf keeps both refs plus the single
/// `refs/remuda/gate/` prefix sweep used for retention.
#[must_use]
pub fn gate_branch_ref(job_id: &str) -> String {
    format!("refs/remuda/gate/{job_id}.branch")
}

/// Which host pushes `main` for a land job.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum GatePushFrom {
    /// The lane host pushes from its own checkout (needs a push credential).
    #[default]
    Lane,
    /// The lane only verifies; the project home host fetches the merge commit
    /// and pushes it. The policy for lanes holding no project credential.
    Home,
}

impl GatePushFrom {
    /// Wire/CLI spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lane => "lane",
            Self::Home => "home",
        }
    }
}

// ── Job enums ──────────────────────────────────────────────────────────────

/// What the job does: verify only, or verify-then-land.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum GateMode {
    /// Verify the merge onto current main; never advance it.
    Verify,
    /// Verify when needed, then compare-and-swap and push main.
    Land,
}

impl GateMode {
    /// Wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verify => "verify",
            Self::Land => "land",
        }
    }

    /// Parse the wire spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "verify" => Some(Self::Verify),
            "land" => Some(Self::Land),
            _ => None,
        }
    }
}

/// Web gate selection.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum GateWebMode {
    /// Diff-driven: the merge gate includes web checks when the diff touches
    /// `crates/` or `web/`; the live Hub e2e suite is not run.
    #[default]
    Auto,
    /// Force every web check including the live Hub Playwright suite.
    Always,
    /// Never add web-only gates (the Rust gate still runs). The consumed
    /// merge CLI has no suppression flag, so on a web-touching diff this is
    /// equivalent to `auto`; documented rather than silently different.
    Never,
}

impl GateWebMode {
    /// Wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Never => "never",
        }
    }

    /// Parse the wire spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

/// Gate job lifecycle: queued → running → passed|failed|landed, with cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum GateJobState {
    /// Waiting for a lane / the land serial position.
    Queued,
    /// A Node is running the job.
    Running,
    /// Verify finished green; main was not advanced.
    Passed,
    /// Gate or landing failed.
    Failed,
    /// Land finished and pushed main.
    Landed,
    /// Cancel was requested while running; the Node is killing the step.
    Canceling,
    /// Cancel completed (queued cancel or killed run).
    Canceled,
}

impl GateJobState {
    /// Wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Landed => "landed",
            Self::Canceling => "canceling",
            Self::Canceled => "canceled",
        }
    }

    /// Parse the wire spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "passed" => Some(Self::Passed),
            "failed" => Some(Self::Failed),
            "landed" => Some(Self::Landed),
            "canceling" => Some(Self::Canceling),
            "canceled" => Some(Self::Canceled),
            _ => None,
        }
    }

    /// A terminal state the scheduler never re-enters.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Passed | Self::Failed | Self::Landed | Self::Canceled
        )
    }
}

// ── Hub-side persisted entity ──────────────────────────────────────────────

/// One gate step result; same JSON shape as `remuda merge --gate --json`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GateStep {
    /// Step name (`cargo-test`, `web-hub-e2e`, …).
    pub name: String,
    /// `planned` | `ok` | `failed` | `skipped`.
    pub status: String,
    /// Wall duration in milliseconds.
    pub duration_ms: u64,
    /// Attempt ordinal (the gate retries some steps).
    #[serde(default)]
    pub attempts: u32,
    /// Whether the last run was a retry.
    #[serde(default)]
    pub retried: bool,
    /// Failure text / timeout / kill reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `"timeout"` | `"child-lost"` when a supervisor killed the step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Bounded failure evidence for one gate run, stored by the Hub as an
/// `obj_…` log object (never inlined into the job row). For a failed run the
/// Node populates it for the failed step; with `--keep-logs` a green run gets
/// a `kept` log of the whole-run tail.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GateRunLog {
    /// Step the log belongs to (`cargo-test`, `web-hub-e2e`, …), or `*` for a
    /// whole-run `kept` log / a runner-level failure before any step marker.
    pub step: String,
    /// `failed` (a step or the runner failed) | `kept` (`--keep-logs` green).
    pub kind: String,
    /// Attempt ordinal reported for the failed step.
    #[serde(default)]
    pub attempts: u32,
    /// One-line headline, mirrored as the job `reason`.
    pub headline: String,
    /// Extracted summary lines: the libtest `failures:` section /
    /// `test … FAILED` / `panicked at` lines for cargo-test; the numbered
    /// Playwright failure titles and first failure block for web steps.
    #[serde(default)]
    pub summary: Vec<String>,
    /// Last captured raw lines (combined step output), bounded.
    #[serde(default)]
    pub tail: Vec<String>,
    /// Number of lines attributed to the step before bounding.
    #[serde(default)]
    pub captured_lines: usize,
    /// Whether the tail/summary was cut by the bounds.
    #[serde(default)]
    pub truncated: bool,
}

/// One queued gate/land job (Hub document; coordinator-hierarchy.md §8 row 6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GateJob {
    /// `gjb_…` id.
    pub id: GateJobId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Branch under verification.
    pub branch: String,
    /// Verify vs verify-then-land.
    pub mode: GateMode,
    /// Web gate selection.
    pub web: GateWebMode,
    /// Pinned lane (chosen at dispatch when not given).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane_id: Option<String>,
    /// Device that enqueued the job.
    pub requested_by: String,
    /// Lifecycle state.
    pub state: GateJobState,
    /// Step results streamed back from the lane Node.
    #[serde(default)]
    pub steps: Vec<GateStep>,
    /// Host the job runs on (the lane's host).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<HostId>,
    /// Queue ordering key (RFC3339 enqueue time).
    pub queued_at: Timestamp,
    /// When the Node took the job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Timestamp>,
    /// When the job reached a terminal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<Timestamp>,
    /// When a cancel was requested while the job was running. Drives the
    /// bounded grace after which the scheduler finishes a `canceling` job
    /// `canceled` even if the Node never answered the cancel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_requested_at: Option<Timestamp>,
    /// Set while the project home host is running `gate.land` for this job
    /// (`pushFrom: home`). A cancel during this window must not let the bounded
    /// cancel grace finalize the job or drop its pinned merge: the home push
    /// can take minutes and its own terminal write records the honest outcome.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub home_land_in_flight: bool,
    /// Base (`main`) sha the merge was verified onto.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// Pinned branch-tip sha incorporated by the merge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    /// Verified / landed merge commit sha.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_sha: Option<String>,
    /// Ref pinning `mergeSha` in the lane repo after a passing verify, so the
    /// commit survives the scratch worktree and a home-host land can fetch it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_ref: Option<String>,
    /// Current main when a land lost the compare-and-swap (retry signal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_main_sha: Option<String>,
    /// Failure / cancel reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Name of the step that failed the last run (`cargo-test`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_step: Option<String>,
    /// First line of the failure summary (the log headline).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Hub object holding the bounded run log (`obj_…`); absent on green runs
    /// unless `keepLogs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_object_id: Option<String>,
    /// Retain the bounded run log even when the run passes (`--keep-logs`).
    #[serde(default)]
    pub keep_logs: bool,
    /// Number of verify+land attempts (CAS retries).
    #[serde(default)]
    pub attempts: u32,
    /// `land --then "<cmd>"`: post-land command on the project home host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub then_command: Option<String>,
    /// Output of the completed `--then` command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub then_output: Option<String>,
}

/// One streamed Node→Hub job event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(
    rename_all = "camelCase",
    tag = "kind",
    rename_all_fields = "camelCase"
)]
pub enum GateEventKind {
    /// The run started on the lane (`phase`: fetch/ff/gate/land/push).
    #[serde(rename = "phase")]
    Phase {
        /// Phase name.
        phase: String,
    },
    /// One gate step reached a terminal status.
    #[serde(rename = "step")]
    Step {
        /// The finished step.
        step: GateStep,
    },
    /// A line of runner diagnostics.
    #[serde(rename = "log")]
    Log {
        /// Diagnostic text.
        message: String,
    },
    /// The run finished with its full verdict; the gate.run reply mirrors it.
    #[serde(rename = "finished")]
    Finished {
        /// Final verdict.
        result: Box<GateRunResult>,
    },
}

/// Params of the Node-originated `gate.event` notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GateEventParams {
    /// The job this event belongs to.
    pub job_id: String,
    /// Event payload.
    #[serde(flatten)]
    pub kind: GateEventKind,
}

// ── Hub→Node RPC params/results ────────────────────────────────────────────

/// `gate.run` params: everything the lane runner needs, derived from the
/// project's `ProjectGateLane` (set once, never re-typed per run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GateRunParams {
    /// Hub job id (`gjb_…`); key for cancel and for streamed events.
    pub job_id: String,
    /// Lane id (Node-side lane lock key).
    pub lane_id: String,
    /// Lane checkout on this host.
    pub repo_path: String,
    /// Dedicated cargo target dir.
    pub target_dir: String,
    /// Branch to verify/land.
    pub branch: String,
    /// Base branch (default `main`).
    #[serde(default = "default_base_branch")]
    pub base_branch: String,
    /// `verify` | `land`.
    pub mode: String,
    /// `auto` | `always` | `never`.
    pub web: String,
    /// Extra environment for the gate (toolchain, build jobs, …).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Advisory lock for the shared web-hub-e2e browser (`REMUDA_E2E_LOCK`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_path: Option<String>,
    /// Playwright WebSocket endpoint (`PW_TEST_CONNECT_WS_ENDPOINT`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pw_endpoint: Option<String>,
    /// PATH prefix (toolchain bin dirs) applied to the gate environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain_path: Option<String>,
    /// Allocated port block, e.g. `58480-58489`; derives HUB_E2E_LISTEN/WEB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<String>,
    /// Per-step wall-clock budgets, forwarded as REMUDA_GATE_STEP_TIMEOUTS.
    #[serde(default)]
    pub timeouts: BTreeMap<String, u64>,
    /// Overall run wall-clock budget (seconds); 0 disables the runner cap.
    #[serde(default)]
    pub gate_timeout_secs: u64,
    /// Land mode pushes main from the lane host when true.
    #[serde(default)]
    pub push: bool,
    /// Where `main` is pushed from for a land job. With `home` the lane runs a
    /// verify only (never `merge --land`): moving lane-local main here would
    /// report `landed` while nothing reached the remote.
    #[serde(default)]
    pub push_from: GatePushFrom,
    /// Retain the bounded run log even when the run passes (`--keep-logs`).
    #[serde(default)]
    pub keep_logs: bool,
    /// Test seam: override the merge binary (defaults to the Node's own exe).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
}

fn default_base_branch() -> String {
    "main".into()
}

/// `gate.run` verdict, mirrored by the terminal `finished` event.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GateRunResult {
    /// Job id echoed back.
    pub job_id: String,
    /// `passed` | `landed` | `failed` | `canceled` | `base-moved` | `stale-tip` | `lane-busy`.
    pub status: String,
    /// Step results.
    #[serde(default)]
    pub steps: Vec<GateStep>,
    /// Base sha.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// Branch tip sha.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    /// Merge commit sha.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_sha: Option<String>,
    /// Ref the lane runner pinned `mergeSha` under (`refs/remuda/gate/<id>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_ref: Option<String>,
    /// Current main when the land CAS lost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_main_sha: Option<String>,
    /// Failure text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Name of the step that failed (`cargo-test`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_step: Option<String>,
    /// First line of the extracted failure summary (the log headline).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Bounded failure evidence (or a `kept` green-run log). The Hub stores
    /// it as an `obj_…` log object and replaces this field with `logObjectId`
    /// on the persisted job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_log: Option<GateRunLog>,
}

/// `gate.cancel` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GateCancelParams {
    /// Job to cancel.
    pub job_id: String,
}

/// `gate.then` params: post-land command on the project's home host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GateThenParams {
    /// Job the post-land hook belongs to.
    pub job_id: String,
    /// Command line (`bash -lc`).
    pub command: String,
    /// Working directory (default: the Node's primary workspace).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Extra environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Wall-clock budget (seconds); default enforced by the Node.
    #[serde(default)]
    pub timeout_secs: u64,
}

/// `gate.then` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GateThenResult {
    /// Job id echoed back.
    pub job_id: String,
    /// Process exit code.
    pub exit_code: i32,
    /// Captured stdout/stderr (truncated).
    pub output: String,
}

/// `gate.land` params: the home host obtains a lane's verified merge commit
/// and compare-and-swap pushes it to the project's base branch.
///
/// This is the `pushFrom: home` half of D-034: the lane host holds no push
/// credential for the project remote, so a passing verify is landed by the
/// host that does. For this slice the merge commit arrives by `git fetch` of
/// `mergeRef` over `fetchRemote`; a Node RPC streaming the packfile is the
/// better shape later, so `fetchRemote` stays the only lane-reachability
/// input and nothing else assumes ssh.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GateLandParams {
    /// Job being landed.
    pub job_id: String,
    /// Home-host checkout that owns the push credential.
    pub repo_path: String,
    /// Branch being landed (for the log line only).
    pub branch: String,
    /// Base branch to advance (`main`).
    #[serde(default = "default_base_branch")]
    pub base_branch: String,
    /// Remote to push to (`origin`).
    #[serde(default = "default_push_remote")]
    pub push_remote: String,
    /// Operator's existing remote/alias on the home host that reaches the lane
    /// repo, used to fetch `mergeRef`.
    pub fetch_remote: String,
    /// Ref in the lane repo pinning the verified merge commit.
    pub merge_ref: String,
    /// The verified merge commit; the fetched ref must resolve to exactly this.
    pub merge_sha: String,
    /// Base the merge was verified onto. The push is refused unless the
    /// remote's base branch still equals this (it is the merge's first parent).
    pub base_sha: String,
    /// Wall-clock budget (seconds); 0 uses the Node default.
    #[serde(default)]
    pub timeout_secs: u64,
}

fn default_push_remote() -> String {
    "origin".into()
}

/// `gate.land` verdict.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GateLandResult {
    /// Job id echoed back.
    pub job_id: String,
    /// `landed` | `base-moved` | `failed`.
    ///
    /// `base-moved` means the compare-and-swap was refused because the remote
    /// base branch had advanced; nothing was pushed and the Hub re-queues a
    /// verify. There is no partial-push outcome: the push is a single
    /// `--force-with-lease` on the base branch, so the remote either takes the
    /// whole merge commit or stays exactly where it was.
    pub status: String,
    /// Remote base-branch sha observed when the CAS was refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_main_sha: Option<String>,
    /// Sha now at the remote base branch on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_sha: Option<String>,
    /// Failure text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Captured git output (truncated), kept as land evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// `gate.unpin` params: drop a job's persisted merge refs on the lane host.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GateUnpinParams {
    /// Job whose refs are dropped.
    pub job_id: String,
    /// Lane checkout holding the refs.
    pub repo_path: String,
}

/// `gate.unpin` result.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GateUnpinResult {
    /// Job id echoed back.
    pub job_id: String,
    /// Refs actually deleted.
    #[serde(default)]
    pub removed: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_state_web_round_trip_and_terminal_set() {
        for (text, mode) in [("verify", GateMode::Verify), ("land", GateMode::Land)] {
            assert_eq!(GateMode::parse(text), Some(mode));
            assert_eq!(mode.as_str(), text);
        }
        for (text, web) in [
            ("auto", GateWebMode::Auto),
            ("always", GateWebMode::Always),
            ("never", GateWebMode::Never),
        ] {
            assert_eq!(GateWebMode::parse(text), Some(web));
            assert_eq!(web.as_str(), text);
        }
        assert!(GateJobState::parse("nope").is_none());
        assert!(GateJobState::Passed.is_terminal());
        assert!(GateJobState::Landed.is_terminal());
        assert!(!GateJobState::Canceling.is_terminal());
        assert!(is_gate_call(METHOD_GATE_RUN));
        assert!(is_gate_call(METHOD_GATE_CANCEL));
        assert!(!is_gate_call(METHOD_GATE_EVENT));
    }

    #[test]
    fn gate_event_is_internally_tagged_on_the_wire() {
        let event = GateEventParams {
            job_id: "gjb_x".into(),
            kind: GateEventKind::Phase {
                phase: "fetch".into(),
            },
        };
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["kind"], "phase");
        assert_eq!(value["phase"], "fetch");
        assert_eq!(value["jobId"], "gjb_x");
    }
}
