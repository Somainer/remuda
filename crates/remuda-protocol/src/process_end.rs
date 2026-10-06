//! c-cardsettle r5 addendum (OA6): the SINGLE shared process-end classifier.
//!
//! Every "is this instance process ended?" decision in the Hub and Node goes
//! through [`process_end`], so the two crates (and ma-lineage / ma-sdk-state
//! after cardsettle lands) can never disagree on what counts as a real process
//! end.
//!
//! The rule (owner): ONLY process-end evidence is terminal.
//! - A clean process exit is [`ProcessEndKind::Exited`]; a non-zero exit, a
//!   signal, PTY/EOF loss, a launch that never started, or the Node reporting
//!   the instance gone is [`ProcessEndKind::Failed`].
//! - Turn-level failures (a `result` error, StopFailure, configure/diagnostic
//!   errors, subagent/workflow events) are NOT process end; they stay
//!   retryable and never settle cards.
//!
//! There is deliberately NO name-substring match, NO `severity == error`
//! heuristic, and NO `affectsCompletion` heuristic: those folded clean exit-0
//! into `failed` and folded a live turn error into a process death. The
//! classifier matches the REAL driver event shapes.

use crate::enums::{LifecycleTopic, Severity};
use crate::observation::NativeLifecycle;
use crate::scalar::Knowledge;
use serde_json::Value;

/// Whether a process ended, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessEndKind {
    /// The process exited cleanly (exit 0 / a clean close). Lifecycle becomes
    /// `exited`; cards settle as generation-ended.
    Exited,
    /// The process ended badly (non-zero exit, signal, PTY/EOF loss), never
    /// started, or the Node reports it gone. Lifecycle becomes `failed`; cards
    /// settle as generation-ended.
    Failed,
}

/// Evidence that a process ended, returned by [`process_end`] /
/// [`entity_process_end`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessEnd {
    /// Clean exit vs failed/gone/never-started.
    pub kind: ProcessEndKind,
}

impl ProcessEnd {
    /// The instance lifecycle string this evidence writes.
    #[must_use]
    pub fn lifecycle(&self) -> &'static str {
        match self.kind {
            ProcessEndKind::Exited => "exited",
            ProcessEndKind::Failed => "failed",
        }
    }

    /// True for a clean exit.
    #[must_use]
    pub fn is_exited(&self) -> bool {
        matches!(self.kind, ProcessEndKind::Exited)
    }
}

/// The exact native event name shell-pty emits for a real process exit
/// (crates/remuda-driver/src/shell_pty.rs NATIVE_EXIT).
pub const SHELL_PTY_EXIT_NAME: &str = "native_exit";
/// The exact native event name the print/SDK driver emits from `emit_exit`
/// (crates/remuda-driver/src/claude_print.rs).
pub const PRINT_EXIT_NAME: &str = "session";
/// The generic PTY carrier's pane-exit name (generic_pty failure_lifecycle).
pub const GENERIC_EXIT_NAME: &str = "exit";
/// The generic PTY carrier's startup-failure name (emit_startup_failure).
pub const GENERIC_STARTUP_ERROR_NAME: &str = "error";
/// The reasonCode / name for a launch that never started.
pub const START_FAILED_REASON: &str = "native-driver-start-failed";
/// The native name the Node/PTY layer uses when it reports an unknown/gone
/// instance at reconnection (Node reports the instance gone).
pub const INSTANCE_GONE_NAME: &str = "agent_not_ready";
/// The generic PTY carrier's agent-gone name.
pub const AGENT_GONE_NAME: &str = "agent_gone";

fn status_of(native: &NativeLifecycle) -> Option<&str> {
    match &native.status {
        Knowledge::Known { value } => Some(value.as_str()),
        _ => None,
    }
}

/// Classify a native lifecycle from its JSON representation (the inner
/// `payload` object of a `type=native` lifecycle event, possibly with the
/// `type` field still present). Tolerant of fields omitted by tests (nativeId
/// etc.) — reads only the fields it needs. Use this when you have a
/// `serde_json::Value` rather than a fully-deserialised [`NativeLifecycle`].
#[must_use]
pub fn process_end_value(payload: &Value) -> Option<ProcessEnd> {
    if payload.get("topic").and_then(Value::as_str) != Some("session") {
        return None;
    }
    let name = payload
        .get("nativeName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let status = payload
        .pointer("/status/value")
        .and_then(Value::as_str)
        .or_else(|| payload.get("status").and_then(Value::as_str));
    let severity = match payload
        .get("severity")
        .and_then(Value::as_str)
        .unwrap_or("info")
    {
        "error" => Severity::Error,
        _ => Severity::Info,
    };
    let reason_code = payload
        .pointer("/relatedIds/reasonCode")
        .and_then(Value::as_str)
        .or_else(|| payload.get("reasonCode").and_then(Value::as_str))
        .unwrap_or("");
    classify(&name, status, severity, reason_code)
}

/// Classify a NATIVE lifecycle observation (`type=native`). Returns `Some`
/// only for explicit process-end evidence on `topic=session`; everything else
/// (turn/hook/configure/diagnostic, any subagent event) returns `None`.
#[must_use]
pub fn process_end(native: &NativeLifecycle) -> Option<ProcessEnd> {
    if native.topic != LifecycleTopic::Session {
        return None;
    }
    let name = native.native_name.as_str();
    let status = status_of(native);
    let severity = native.severity;
    let reason_code = native
        .related_ids
        .get("reasonCode")
        .map(String::as_str)
        .unwrap_or("");
    classify(name, status, severity, reason_code)
}

/// Shared classifier body from the extracted fields.
fn classify(
    raw_name: &str,
    status: Option<&str>,
    severity: Severity,
    reason_code: &str,
) -> Option<ProcessEnd> {
    let name = raw_name.to_ascii_lowercase();

    // Launch that never started → Failed.
    if reason_code == START_FAILED_REASON
        || name == START_FAILED_REASON
        || name.contains("start-fail")
    {
        return Some(ProcessEnd {
            kind: ProcessEndKind::Failed,
        });
    }
    // Node reports the instance gone → Failed.
    if name == INSTANCE_GONE_NAME || name == AGENT_GONE_NAME {
        return Some(ProcessEnd {
            kind: ProcessEndKind::Failed,
        });
    }

    // Real process-exit event names.
    let is_exit_name =
        name == SHELL_PTY_EXIT_NAME || name == PRINT_EXIT_NAME || name == GENERIC_EXIT_NAME;
    if !is_exit_name {
        return None;
    }
    if let Some(status) = status {
        return match status {
            "exited" | "clean" => Some(ProcessEnd {
                kind: ProcessEndKind::Exited,
            }),
            "failed" | "error" | "crashed" | "killed" | "terminated" => Some(ProcessEnd {
                kind: ProcessEndKind::Failed,
            }),
            // status is an agent/pane id (generic_pty failure_lifecycle sets
            // it to the pane id): fall back to severity.
            _ => severity_fallback(severity),
        };
    }
    severity_fallback(severity)
}

/// Exit name present but no usable status: severity=info is a clean exit,
/// severity=error a failed one.
fn severity_fallback(severity: Severity) -> Option<ProcessEnd> {
    match severity {
        Severity::Error => Some(ProcessEnd {
            kind: ProcessEndKind::Failed,
        }),
        _ => Some(ProcessEnd {
            kind: ProcessEndKind::Exited,
        }),
    }
}

/// Classify an ENTITY lifecycle observation (`type=entity`,
/// `entityType=instance`) that the Node emitted as an attested instance state.
///
/// A Node entity `state=exited`/`state=failed` for an INSTANCE entity is
/// process-end evidence (it precedes / accompanies the native exit — see
/// remuda-node runtime). `state=failed` maps to [`ProcessEndKind::Failed`],
/// `state=exited` to [`ProcessEndKind::Exited`]. Any other entity (interaction,
/// command, workflow run/phase/member) is NOT a root process end and returns
/// `None`.
#[must_use]
pub fn entity_process_end(entity_type: Option<&str>, state: Option<&str>) -> Option<ProcessEnd> {
    if entity_type != Some("instance") {
        return None;
    }
    match state {
        Some("exited") => Some(ProcessEnd {
            kind: ProcessEndKind::Exited,
        }),
        Some("failed") => Some(ProcessEnd {
            kind: ProcessEndKind::Failed,
        }),
        _ => None,
    }
}

/// True when a native event's severity is an error — exposed for callers that
/// need the raw severity but should NOT treat it as process end on its own.
#[must_use]
pub fn is_error_severity(severity: Severity) -> bool {
    severity == Severity::Error
}
