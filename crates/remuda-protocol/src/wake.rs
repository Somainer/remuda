//! Wake-ups (cb-wake-engine, D-060 §11): durable rows a lineage writes so the
//! Hub wakes it at a time, at every interval, or when a fact arrives.
//!
//! A wake-up belongs to one lineage. The Hub's writer tick claims each due
//! occurrence once and records a run; a run in `persistent` mode is delivered
//! to the lineage's live chapter as a `wake-due` inbox fact, so it waits for
//! the idle point like any fact, and a run in `fresh` mode makes the Hub
//! create a short-lived child under the lineage instead. The `notify`
//! declaration is the caller's; the Hub never picks one.
//!
//! The text of a wake-up is the lineage's own words (or the owner's, for one
//! the owner added); it is quoted to the lineage as a JSON string.
//!
//! A wake-up may carry a `check` (cb-wake-check, main-agent.md §11.2): a
//! shell command one named Node runs, with a mandatory timeout, before each
//! run. Exit 0 lets the run happen with the check's bounded stdout as its
//! summary, exit 2 skips the run, anything else (the timeout included) fails
//! it. The Hub asks the Node with [`METHOD_WAKE_CHECK`].

use crate::{InboxFactKind, InputOrigin};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Characters a wake-up name may hold (from `[A-Za-z0-9._:-]`, not starting
/// with `wak_`: `rm` takes a name or an id in the URL).
pub const WAKE_NAME_MAX_CHARS: usize = 64;
/// Characters a wake-up text may hold.
pub const WAKE_TEXT_MAX_CHARS: usize = 2000;
/// Shortest `every` interval, in seconds.
pub const WAKE_EVERY_MIN_SECS: i64 = 60;
/// Longest interval, and the furthest ahead an `at` or `startAt` may lie,
/// in seconds (one year).
pub const WAKE_MAX_SECS: i64 = 31_536_000;
/// Wake-ups one lineage may hold.
pub const WAKE_MAX_PER_LINEAGE: usize = 64;
/// Runs `GET /v1/lineages/{id}/wake-ups` lists, newest first.
pub const WAKE_RUNS_LISTED: usize = 50;
/// Characters a check's command may hold.
pub const WAKE_CHECK_COMMAND_MAX_CHARS: usize = 2000;
/// Shortest check timeout, in seconds.
pub const WAKE_CHECK_TIMEOUT_MIN_SECS: i64 = 1;
/// Longest check timeout, in seconds (one hour).
pub const WAKE_CHECK_TIMEOUT_MAX_SECS: i64 = 3600;
/// Bytes of a check's stdout the Node keeps; the rest is read and dropped.
pub const WAKE_CHECK_STDOUT_MAX_BYTES: usize = 4096;
/// The most a Node may take, in seconds, beyond a check's own timeout to
/// answer `wake.check` (its `sh -n` precheck, killing what the check started,
/// draining its stdout). The Hub waits for the answer longer than the timeout
/// plus this, so a check that ran its whole timeout is never taken as lost.
pub const WAKE_CHECK_NODE_OVERHEAD_SECS: u64 = 30;
/// The prefix of every environment variable the Hub may pass to a check.
pub const WAKE_CHECK_ENV_PREFIX: &str = "REMUDA_WAKE_";

/// Hub→Node: run one wake-up check (`sh -n`, then the command under its
/// timeout) and report how it exited.
pub const METHOD_WAKE_CHECK: &str = "wake.check";

wire_enum!(WakeMode, "8.4", {
    Persistent => "persistent",
    Fresh => "fresh",
});

impl WakeMode {
    /// Wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Persistent => "persistent",
            Self::Fresh => "fresh",
        }
    }

    /// Parse a wire spelling.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        [Self::Persistent, Self::Fresh]
            .into_iter()
            .find(|mode| mode.as_str() == raw)
    }
}

wire_enum!(WakeNotify, "8.4", {
    Never => "never",
    Failure => "failure",
    Always => "always",
});

impl WakeNotify {
    /// Wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Failure => "failure",
            Self::Always => "always",
        }
    }

    /// Parse a wire spelling.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        [Self::Never, Self::Failure, Self::Always]
            .into_iter()
            .find(|notify| notify.as_str() == raw)
    }
}

wire_enum!(WakeUpState, "8.4", {
    Active => "active",
    Done => "done",
});

impl WakeUpState {
    /// Wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Done => "done",
        }
    }

    /// Parse a wire spelling.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        [Self::Active, Self::Done]
            .into_iter()
            .find(|state| state.as_str() == raw)
    }
}

wire_enum!(WakeCheckAdmission, "8.4", {
    Owner => "owner",
    OwnHostBypass => "own-host-bypass",
    Approved => "approved",
});

impl WakeCheckAdmission {
    /// Wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::OwnHostBypass => "own-host-bypass",
            Self::Approved => "approved",
        }
    }
}

wire_enum!(WakeCheckState, "8.4", {
    Pending => "pending",
    Running => "running",
    Passed => "passed",
    Skipped => "skipped",
    Failed => "failed",
    Interrupted => "interrupted",
});

impl WakeCheckState {
    /// Every state, in declaration order.
    pub const ALL: [WakeCheckState; 6] = [
        Self::Pending,
        Self::Running,
        Self::Passed,
        Self::Skipped,
        Self::Failed,
        Self::Interrupted,
    ];

    /// Wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Passed => "passed",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    /// Parse a wire spelling.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == raw)
    }
}

wire_enum!(WakeCheckOutcome, "8.4", {
    Exited => "exited",
    TimedOut => "timed-out",
    Syntax => "syntax",
    Unavailable => "unavailable",
});

wire_enum!(WakeRunState, "8.4", {
    Queued => "queued",
    Running => "running",
    Success => "success",
    Failed => "failed",
    Skipped => "skipped",
    Interrupted => "interrupted",
});

impl WakeRunState {
    /// Every state, in declaration order.
    pub const ALL: [WakeRunState; 6] = [
        Self::Queued,
        Self::Running,
        Self::Success,
        Self::Failed,
        Self::Skipped,
        Self::Interrupted,
    ];

    /// Wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Interrupted => "interrupted",
        }
    }

    /// Parse a wire spelling.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == raw)
    }
}

/// When a wake-up fires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind")]
pub enum WakeTrigger {
    /// Once, at a time.
    #[serde(rename = "at")]
    At(WakeAt),
    /// At every interval.
    #[serde(rename = "every")]
    Every(WakeEvery),
    /// Each time a fact of a kind reaches the lineage's inbox.
    #[serde(rename = "when")]
    When(WakeWhen),
}

impl WakeTrigger {
    /// Wire spelling of the trigger kind.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::At(_) => "at",
            Self::Every(_) => "every",
            Self::When(_) => "when",
        }
    }
}

/// `at`: one occurrence. A request gives `at` or `afterSecs`; a stored
/// wake-up always carries `at`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeAt {
    /// The time it fires (UTC RFC3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// Request only: seconds after the Hub received the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_secs: Option<i64>,
}

/// `every`: one occurrence per interval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeEvery {
    /// The interval in seconds, at least [`WAKE_EVERY_MIN_SECS`].
    pub every_secs: i64,
    /// The first occurrence (UTC RFC3339); one interval after the add when
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_at: Option<String>,
}

/// `when`: a condition over the facts the lineage receives. Each fact of
/// `factKind` (about `subjectInstanceId`, when set) recorded after the add is
/// an occurrence. `wake-due` cannot be a condition, and nothing records
/// `context-threshold` facts yet, so that kind is refused at add time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeWhen {
    /// The fact kind that fires it.
    pub fact_kind: InboxFactKind,
    /// Only facts about this instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_instance_id: Option<String>,
}

/// `POST /v1/lineages/{id}/wake-ups`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WakeAddRequest {
    /// Unique in the lineage (`[A-Za-z0-9._:-]`, not `wak_…`); a repeat of
    /// the same add under the same name returns the stored wake-up. The Hub
    /// generates one when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What the wake-up is for: delivered with each run, or the brief of a
    /// fresh-mode child.
    pub text: String,
    /// When it fires.
    pub trigger: WakeTrigger,
    /// The optional check (main-agent.md §11.2): a shell command a Node runs
    /// before each run. Admitted on the OA1 axis: an Agent adds one without
    /// the owner's approval only on its own host while it runs with its
    /// harness's permission control off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<WakeCheckRequest>,
    /// `persistent` (default) or `fresh`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<WakeMode>,
    /// Which runs notify the owner; required, the Hub never picks.
    pub notify: WakeNotify,
}

/// `check` on an add. Every field is optional on the wire so that a missing
/// one is refused with the Hub's own 400, not a body-parse error.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WakeCheckRequest {
    /// The shell command (`sh -c`), at most
    /// [`WAKE_CHECK_COMMAND_MAX_CHARS`] characters; required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The Node it runs on, a host in the lineage's scope. An Agent may omit
    /// it for its own host; the owner names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    /// The timeout in seconds,
    /// [`WAKE_CHECK_TIMEOUT_MIN_SECS`]..=[`WAKE_CHECK_TIMEOUT_MAX_SECS`];
    /// required, there is no default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<i64>,
}

/// A stored wake-up's check, and how it was admitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeCheck {
    /// The shell command.
    pub command: String,
    /// The Node it runs on, fixed when it was added.
    pub host_id: String,
    /// The timeout in seconds.
    pub timeout_secs: i64,
    /// `owner` (the owner added it), `own-host-bypass` (an Agent on its own
    /// host, running with its harness's permission control off) or
    /// `approved` (the owner's one-shot approval).
    pub admission: WakeCheckAdmission,
    /// The approval that admitted it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
}

/// One run's check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeRunCheck {
    /// `pending` (waiting for its host), `running`, `passed` (exit 0),
    /// `skipped` (exit 2), `failed`, or `interrupted` (the Hub or the Node
    /// restarted while it ran; never run again).
    pub state: WakeCheckState,
    /// The Node it ran on.
    pub host_id: String,
    /// How it exited, in a few words (`exit 1`, `timed out after 30 s`,
    /// `syntax error`); never its stderr.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Its stdout, at most [`WAKE_CHECK_STDOUT_MAX_BYTES`] bytes. Untrusted
    /// data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// The stdout was longer and was cut.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub output_truncated: bool,
}

/// Who added a wake-up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeCreator {
    /// `agent` (the lineage itself) or `human` (the owner).
    pub origin: InputOrigin,
    /// The chapter an Agent added it from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
}

/// One wake-up row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeUp {
    /// `wak_…`.
    pub wake_id: String,
    /// The lineage it wakes.
    pub lineage_id: String,
    /// Unique in the lineage.
    pub name: String,
    /// What it is for.
    pub text: String,
    /// When it fires (an `at` carries its resolved time).
    pub trigger: WakeTrigger,
    /// The optional check (main-agent.md §11.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<WakeCheck>,
    /// Delivery mode.
    pub mode: WakeMode,
    /// The declared notify policy.
    pub notify: WakeNotify,
    /// `done` once an `at` has fired.
    pub state: WakeUpState,
    /// The next scheduled occurrence (UTC RFC3339); absent for `when` and
    /// once done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_fire_at: Option<String>,
    /// Occurrences skipped because they came due while an earlier run was
    /// still queued or running: none of them gets a run of its own, not even
    /// once that run ends. The next run reports them and this resets.
    pub missed_count: i64,
    /// When it was added (UTC RFC3339).
    pub created_at: String,
    /// When the Hub last changed it.
    pub updated_at: String,
    /// When its latest run fired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<String>,
    /// When its latest real run fired: one without a check, or whose check
    /// passed. A skipped or failed check leaves it as it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<String>,
    /// Who added it.
    pub creator: WakeCreator,
}

/// One run of a wake-up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeRun {
    /// `wrn_…`.
    pub run_id: String,
    /// The wake-up (it may have been removed since).
    pub wake_id: String,
    /// The lineage it woke.
    pub lineage_id: String,
    /// The wake-up's name when it fired.
    pub name: String,
    /// The wake-up's mode when it fired.
    pub mode: WakeMode,
    /// The occurrence it stands for: the latest one due when it fired.
    pub scheduled_at: String,
    /// When the Hub claimed it.
    pub fired_at: String,
    /// `queued` → `running` → `success`, `failed` or `skipped`; a run that
    /// was running when the Hub restarted is `interrupted` and never
    /// replayed.
    pub state: WakeRunState,
    /// When it started running: its batch was forwarded, or its fresh child
    /// was being created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// When it ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// The check's exit code (main-agent.md §11.2); null without a check,
    /// before it exits, and when it timed out, was refused by `sh -n` or
    /// could not start.
    pub check_exit: Option<i64>,
    /// The run's check, when the wake-up has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<WakeRunCheck>,
    /// The `wake-due` inbox row it was delivered as (persistent mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fact_id: Option<i64>,
    /// The inbox batch that carried it (persistent mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    /// The child the Hub created for it (fresh mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_instance_id: Option<String>,
    /// Occurrences skipped before it (none of them ran): those the Hub was
    /// down for, those due while the previous run was still open, and, for
    /// `when`, the other facts found in the same evaluation.
    pub missed_before: i64,
    /// Why it failed: `not-received`, `not-delivered`, `delivery`,
    /// `turn-failed`, `chapter-ended`, `fresh-refused`, `child-failed`,
    /// `child-ended`, `check-exit`, `check-timeout`, `check-syntax`,
    /// `check-unavailable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<String>,
}

/// `POST /v1/lineages/{id}/wake-ups` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeAddResult {
    /// The stored wake-up.
    pub wake_up: WakeUp,
    /// False when the same add under the same name already stored it.
    pub created: bool,
}

/// `GET /v1/lineages/{id}/wake-ups`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeList {
    /// The lineage.
    pub lineage_id: String,
    /// Its wake-ups, oldest first.
    pub items: Vec<WakeUp>,
    /// Its latest runs, newest first, at most [`WAKE_RUNS_LISTED`].
    pub runs: Vec<WakeRun>,
}

/// `DELETE /v1/lineages/{id}/wake-ups/{wake}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeRemoved {
    /// The removed wake-up.
    pub wake_id: String,
    /// Its name.
    pub name: String,
}

/// `wake.check` params (Hub→Node).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeCheckParams {
    /// The run the check decides.
    pub run_id: String,
    /// The shell command (`sh -c`).
    pub command: String,
    /// The timeout in seconds; the Node refuses 0 and anything over
    /// [`WAKE_CHECK_TIMEOUT_MAX_SECS`].
    pub timeout_secs: u64,
    /// The workspace it runs in (the lineage's on that host); the Node's
    /// first workspace when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The directory inside that workspace (the lineage's cwd on that host).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Variables about the run, each named with [`WAKE_CHECK_ENV_PREFIX`];
    /// the Node drops any other name.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// `wake.check` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeCheckResult {
    /// The run, echoed.
    pub run_id: String,
    /// `exited`, `timed-out` (the process group was killed), `syntax`
    /// (`sh -n` refused it; nothing ran) or `unavailable` (it could not
    /// start).
    pub outcome: WakeCheckOutcome,
    /// The exit code, when it exited with one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The signal that ended it, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// Its stdout, at most [`WAKE_CHECK_STDOUT_MAX_BYTES`] bytes (lossy
    /// UTF-8). Stderr is never kept.
    #[serde(default)]
    pub stdout: String,
    /// The stdout was longer and was cut.
    #[serde(default)]
    pub stdout_truncated: bool,
    /// How it exited, in a few words; never its stderr.
    pub summary: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn triggers_round_trip_with_their_kind_tag() {
        for (wire, trigger) in [
            (
                json!({"kind": "at", "at": "2026-11-04T10:00:00.000Z"}),
                WakeTrigger::At(WakeAt {
                    at: Some("2026-11-04T10:00:00.000Z".into()),
                    after_secs: None,
                }),
            ),
            (
                json!({"kind": "every", "everySecs": 180}),
                WakeTrigger::Every(WakeEvery {
                    every_secs: 180,
                    start_at: None,
                }),
            ),
            (
                json!({"kind": "when", "factKind": "gate"}),
                WakeTrigger::When(WakeWhen {
                    fact_kind: InboxFactKind::Gate,
                    subject_instance_id: None,
                }),
            ),
        ] {
            assert_eq!(serde_json::to_value(&trigger).unwrap(), wire);
            assert_eq!(
                serde_json::from_value::<WakeTrigger>(wire).unwrap(),
                trigger
            );
        }
    }

    #[test]
    fn an_add_must_declare_notify() {
        let missing = json!({"text": "t", "trigger": {"kind": "at", "afterSecs": 60}});
        assert!(serde_json::from_value::<WakeAddRequest>(missing).is_err());
        let declared = json!({"text": "t", "trigger": {"kind": "at", "afterSecs": 60},
                              "notify": "failure"});
        let add: WakeAddRequest = serde_json::from_value(declared).unwrap();
        assert_eq!(add.notify, WakeNotify::Failure);
        assert_eq!(add.mode, None);
    }

    #[test]
    fn a_check_request_takes_only_its_three_fields() {
        let add: WakeAddRequest = serde_json::from_value(json!({
            "text": "t", "trigger": {"kind": "every", "everySecs": 180}, "notify": "failure",
            "check": {"command": "test -s queue", "hostId": "hst_1", "timeoutSecs": 30}
        }))
        .unwrap();
        assert_eq!(
            add.check,
            Some(WakeCheckRequest {
                command: Some("test -s queue".into()),
                host_id: Some("hst_1".into()),
                timeout_secs: Some(30),
            })
        );
        // A missing timeout still parses: the Hub refuses it with its own 400.
        let bare: WakeAddRequest = serde_json::from_value(json!({
            "text": "t", "trigger": {"kind": "at", "afterSecs": 60}, "notify": "never",
            "check": {"command": "true"}
        }))
        .unwrap();
        assert_eq!(bare.check.unwrap().timeout_secs, None);
        assert!(
            serde_json::from_value::<WakeAddRequest>(json!({
                "text": "t", "trigger": {"kind": "at", "afterSecs": 60}, "notify": "never",
                "check": {"command": "true", "timeoutSecs": 5, "admission": "owner"}
            }))
            .is_err(),
            "the admission is the Hub's to record"
        );
    }

    #[test]
    fn states_round_trip_their_wire_spelling() {
        for state in WakeRunState::ALL {
            assert_eq!(WakeRunState::parse(state.as_str()), Some(state));
            assert_eq!(serde_json::to_value(state).unwrap(), json!(state.as_str()));
        }
        assert_eq!(WakeMode::parse("fresh"), Some(WakeMode::Fresh));
        assert_eq!(WakeNotify::parse("always"), Some(WakeNotify::Always));
        assert_eq!(WakeUpState::parse("done"), Some(WakeUpState::Done));
        for state in WakeCheckState::ALL {
            assert_eq!(WakeCheckState::parse(state.as_str()), Some(state));
            assert_eq!(serde_json::to_value(state).unwrap(), json!(state.as_str()));
        }
        assert_eq!(
            serde_json::to_value(WakeCheckOutcome::TimedOut).unwrap(),
            json!("timed-out")
        );
    }
}
