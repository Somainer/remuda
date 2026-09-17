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

/// Every Hub→Node gate RPC (stdio allowlist predicate helper).
#[must_use]
pub fn is_gate_call(method: &str) -> bool {
    matches!(
        method,
        METHOD_GATE_RUN | METHOD_GATE_CANCEL | METHOD_GATE_THEN
    )
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
    /// Base (`main`) sha the merge was verified onto.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// Pinned branch-tip sha incorporated by the merge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    /// Verified / landed merge commit sha.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_sha: Option<String>,
    /// Current main when a land lost the compare-and-swap (retry signal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_main_sha: Option<String>,
    /// Failure / cancel reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
        result: GateRunResult,
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
    /// Current main when the land CAS lost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_main_sha: Option<String>,
    /// Failure text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
