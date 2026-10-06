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

    // Launch that never started → Failed. The generic PTY carrier's
    // emit_startup_failure uses the EXACT name "error" with severity=error and
    // a prose status (a transient error the live process survives uses the
    // DISTINCT name transient_runtime_error), so the exact name plus
    // severity=error is attested launch failure.
    if reason_code == START_FAILED_REASON
        || name == START_FAILED_REASON
        || name.contains("start-fail")
        || (name == GENERIC_STARTUP_ERROR_NAME && severity == Severity::Error)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enums::LifecycleTopic;
    use std::collections::BTreeMap;

    fn native(
        topic: LifecycleTopic,
        name: &str,
        status: Option<&str>,
        severity: Severity,
        related: &[(&str, &str)],
    ) -> NativeLifecycle {
        NativeLifecycle {
            topic,
            native_name: name.into(),
            native_id: Knowledge::NotApplicable,
            status: match status {
                Some(value) => Knowledge::Known {
                    value: value.into(),
                },
                None => Knowledge::NotApplicable,
            },
            related_ids: related
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<BTreeMap<_, _>>(),
            data_ref: None,
            severity,
            affects_completion: false,
        }
    }

    #[test]
    fn real_driver_sequences_classify_like_the_events_the_drivers_emit() {
        // (label, native observation, expected process end)
        // Replays the REAL production shapes (addendum C):
        // shell_pty.rs NATIVE_EXIT, claude_print.rs emit_exit, generic_pty.rs
        // failure_lifecycle/emit_startup_failure.
        let cases: Vec<(&str, NativeLifecycle, Option<ProcessEndKind>)> = vec![
            // shell-pty exit 0 (severity info, status "exited") → Exited.
            (
                "shell-pty exit 0",
                native(
                    LifecycleTopic::Session,
                    SHELL_PTY_EXIT_NAME,
                    Some("exited"),
                    Severity::Info,
                    &[("exitCode", "0"), ("reason", "native-exit-code-0")],
                ),
                Some(ProcessEndKind::Exited),
            ),
            // shell-pty non-zero exit (status "failed", severity error) → Failed.
            (
                "shell-pty exit 1",
                native(
                    LifecycleTopic::Session,
                    SHELL_PTY_EXIT_NAME,
                    Some("failed"),
                    Severity::Error,
                    &[("exitCode", "1"), ("reason", "native-exit-code-1")],
                ),
                Some(ProcessEndKind::Failed),
            ),
            // shell-pty signal death (ExitEvidence::Signal.lifecycle() = failed).
            (
                "shell-pty signal",
                native(
                    LifecycleTopic::Session,
                    SHELL_PTY_EXIT_NAME,
                    Some("failed"),
                    Severity::Error,
                    &[
                        ("signal", "SIGKILL"),
                        ("reason", "native-exit-signal-SIGKILL"),
                    ],
                ),
                Some(ProcessEndKind::Failed),
            ),
            // shell-pty EOF with no status (status unknown) → severity error → Failed.
            (
                "shell-pty EOF",
                native(
                    LifecycleTopic::Session,
                    SHELL_PTY_EXIT_NAME,
                    None,
                    Severity::Error,
                    &[("reason", "native-exit-eof")],
                ),
                Some(ProcessEndKind::Failed),
            ),
            // print/SDK emit_exit clean: name "session", status "exited",
            // severity INFO, affectsCompletion=false → Exited.
            (
                "print SDK exited",
                native(
                    LifecycleTopic::Session,
                    PRINT_EXIT_NAME,
                    Some("exited"),
                    Severity::Info,
                    &[],
                ),
                Some(ProcessEndKind::Exited),
            ),
            // print/SDK name "session" carrying status failed (a non-zero
            // one-shot) → Failed.
            (
                "print SDK failed",
                native(
                    LifecycleTopic::Session,
                    PRINT_EXIT_NAME,
                    Some("failed"),
                    Severity::Error,
                    &[],
                ),
                Some(ProcessEndKind::Failed),
            ),
            // generic_pty emit_startup_failure: exact name "error", severity
            // error, status is a prose reason → launch never started → Failed.
            (
                "generic launch failure",
                native(
                    LifecycleTopic::Session,
                    GENERIC_STARTUP_ERROR_NAME,
                    Some("agent process exited during startup"),
                    Severity::Error,
                    &[
                        ("lastError", "agent process exited during startup"),
                        ("paneId", "pane-1"),
                    ],
                ),
                Some(ProcessEndKind::Failed),
            ),
            // attested start-fail reasonCode on any session event → Failed.
            (
                "start-failed reason code",
                native(
                    LifecycleTopic::Session,
                    "somethingelse",
                    None,
                    Severity::Error,
                    &[("reasonCode", START_FAILED_REASON)],
                ),
                Some(ProcessEndKind::Failed),
            ),
            // Node reports the instance gone → Failed.
            (
                "node instance gone",
                native(
                    LifecycleTopic::Session,
                    INSTANCE_GONE_NAME,
                    None,
                    Severity::Error,
                    &[],
                ),
                Some(ProcessEndKind::Failed),
            ),
            (
                "generic agent gone",
                native(
                    LifecycleTopic::Session,
                    AGENT_GONE_NAME,
                    None,
                    Severity::Error,
                    &[],
                ),
                Some(ProcessEndKind::Failed),
            ),
            // generic_pty pane exit: name "exit", status a pane id, severity
            // error → Failed via the severity fallback.
            (
                "generic pane exit",
                native(
                    LifecycleTopic::Session,
                    GENERIC_EXIT_NAME,
                    Some("%pane-1"),
                    Severity::Error,
                    &[("lastError", "pane exited")],
                ),
                Some(ProcessEndKind::Failed),
            ),
            // generic_pty pane clean close: name "exit", pane id, severity info
            // → Exited.
            (
                "generic pane clean",
                native(
                    LifecycleTopic::Session,
                    GENERIC_EXIT_NAME,
                    Some("%pane-1"),
                    Severity::Info,
                    &[],
                ),
                Some(ProcessEndKind::Exited),
            ),
            // ── NOT process end ───────────────────────────────────────────
            // configure error on a live process (addendum B).
            (
                "configure error live",
                native(
                    LifecycleTopic::Configuration,
                    "instance.configure",
                    Some("model-control-unavailable:x"),
                    Severity::Error,
                    &[],
                ),
                None,
            ),
            // root turn result error: ends the turn, not the process (item 4).
            (
                "root result error",
                native(
                    LifecycleTopic::Turn,
                    "result",
                    Some("error"),
                    Severity::Error,
                    &[("resultIndex", "1"), ("numTurns", "1")],
                ),
                None,
            ),
            // root StopFailure outcome=failed.
            (
                "root stop failure",
                native(
                    LifecycleTopic::Turn,
                    "StopFailure",
                    Some("idle"),
                    Severity::Warning,
                    &[("outcome", "failed")],
                ),
                None,
            ),
            // diagnostic severity error.
            (
                "diagnostic error",
                native(
                    LifecycleTopic::Diagnostic,
                    "api_error",
                    Some("rate limited"),
                    Severity::Error,
                    &[],
                ),
                None,
            ),
            // a transient session error with a DISTINCT name survives (the
            // old contains("error")/substring rule must not fire).
            (
                "transient session error",
                native(
                    LifecycleTopic::Session,
                    "transient_runtime_error",
                    Some("transient"),
                    Severity::Error,
                    &[],
                ),
                None,
            ),
            // exact name "error" but severity info is NOT attested launch
            // failure.
            (
                "error name info severity",
                native(
                    LifecycleTopic::Session,
                    "error",
                    Some("hmm"),
                    Severity::Info,
                    &[],
                ),
                None,
            ),
            // severity=error alone on session never ends a process.
            (
                "bare severity error",
                native(
                    LifecycleTopic::Session,
                    "something_happened",
                    None,
                    Severity::Error,
                    &[],
                ),
                None,
            ),
        ];
        for (label, observation, want) in cases {
            let got = process_end(&observation).map(|end| end.kind);
            assert_eq!(got, want, "{label}: classified {got:?}, want {want:?}");
        }
    }

    #[test]
    fn entity_observations_end_only_the_instance_entity() {
        // Node entity state=exited/failed for an INSTANCE is process end and
        // precedes the native exit.
        assert_eq!(
            entity_process_end(Some("instance"), Some("exited")),
            Some(ProcessEnd {
                kind: ProcessEndKind::Exited
            })
        );
        assert_eq!(
            entity_process_end(Some("instance"), Some("failed")),
            Some(ProcessEnd {
                kind: ProcessEndKind::Failed
            })
        );
        // r5 item 1: workflow run/phase/member entities NEVER end the root.
        assert_eq!(
            entity_process_end(Some("workflow.member"), Some("failed")),
            None
        );
        assert_eq!(
            entity_process_end(Some("workflow.run"), Some("failed")),
            None
        );
        assert_eq!(
            entity_process_end(Some("workflow.phase"), Some("failed")),
            None
        );
        // The picker retiring its INTERACTION entity is not a process end.
        assert_eq!(
            entity_process_end(Some("interaction"), Some("invalidated")),
            None
        );
        // Instance non-terminal states.
        assert_eq!(entity_process_end(Some("instance"), Some("ready")), None);
        assert_eq!(entity_process_end(Some("instance"), None), None);
        assert_eq!(entity_process_end(None, Some("failed")), None);
    }

    #[test]
    fn value_classifier_reads_the_wire_shape_the_hub_stores() {
        // The hub stores JSON, not deserialised observations; the exact wire
        // shape (status as {state,value}) and a bare-string status both work.
        let shell = serde_json::json!({
            "type": "native",
            "topic": "session",
            "nativeName": "native_exit",
            "severity": "info",
            "affectsCompletion": true,
            "status": {"state": "known", "value": "exited"},
            "relatedIds": {"exitCode": "0"},
        });
        assert_eq!(
            process_end_value(&shell),
            Some(ProcessEnd {
                kind: ProcessEndKind::Exited
            })
        );
        let print = serde_json::json!({
            "type": "native",
            "topic": "session",
            "nativeName": "session",
            "severity": "info",
            "status": "exited",
        });
        assert_eq!(
            process_end_value(&print),
            Some(ProcessEnd {
                kind: ProcessEndKind::Exited
            })
        );
        let configure = serde_json::json!({
            "type": "native",
            "topic": "configuration",
            "nativeName": "instance.configure",
            "severity": "error",
            "status": {"state": "known", "value": "error"},
        });
        assert_eq!(process_end_value(&configure), None);
    }
}
