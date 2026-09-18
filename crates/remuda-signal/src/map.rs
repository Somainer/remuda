//! Hook event → [`ObservationPayload`] (D-028 §4.3, §7).
//!
//! Pure: no IO, no clock, no channel. Everything that decides what an event
//! *means* is here so it can be tested against payloads recorded from a real
//! `claude` run rather than against a mock.
//!
//! The rules that are easy to get wrong, and why:
//!
//! - **`SubagentStop` is never turn evidence.** It fires with no subagent at
//!   all (a recap is enough), so treating it as `working` — or as `idle` —
//!   would flip the composer on a non-event (design §3.1 [V]).
//! - **Turn lifecycle comes only from `UserPromptSubmit` / `Stop` /
//!   `StopFailure`.** Tool events say a turn is in progress but they are not
//!   its boundaries, so they stay diagnostics.
//! - **`MessageDisplay` is persisted raw.** P3 maps it to message mutations;
//!   persisting the line deltas now means P3 is a mapper change and not a
//!   re-capture.

use crate::event::HookEvent;
use crate::live::Phase;
use remuda_protocol::{
    Completeness, Knowledge, LifecyclePayload, LifecycleTopic, NativeLifecycle, ObservationPayload,
    Severity,
};
use std::collections::BTreeMap;

/// What an event is, once classified. Drives Node-side side effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappedKind {
    /// `SessionStart`: carries the native session id and transcript path.
    SessionStarted,
    /// `UserPromptSubmit`: a turn opened.
    TurnStarted,
    /// `Stop` / `StopFailure`: the turn ended.
    TurnEnded,
    /// `SessionEnd`: the agent is finished with this session.
    SessionEnded,
    /// `MessageDisplay`: one streamed line delta.
    MessageDelta,
    /// `PermissionRequest` / `Elicitation`: the agent is blocked and wants a
    /// human decision. Raises the `blocked` phase and parks a hook.
    InteractionObserved,
    /// `Notification`: an advisory the harness prints (idle prompt, "waiting
    /// for your input", a permission hint). It is NOT a blocking request — it
    /// carries no decision the Node answers — so it must never raise the turn
    /// phase or move the instance to `waiting`. The idle-prompt Notification in
    /// particular fires *after* the turn already ended; treating it as a wait
    /// stranded the composer (root cause, ins_01a0b3b2 2026-09-18 17:03). It is
    /// journaled so the web can surface it as an in-app notification.
    NotificationObserved,
    /// `PreToolUse`: a tool is about to run.
    ToolStarted,
    /// `PostToolUse` / `PostToolBatch`: a tool finished.
    ToolFinished,
    /// `PostToolUseFailure`: a tool finished with an error.
    ///
    /// Not registered by the overlay yet (design §3.1): the classify arm is
    /// real but inert until c-hookgap registers the event.
    ToolFailed,
    /// Tool activity and anything else: journaled, no state change.
    Diagnostic,
}

/// A classified event plus the payload to journal for it.
#[derive(Debug, Clone)]
pub struct Mapped {
    /// What this event means.
    pub kind: MappedKind,
    /// How much we actually know. Hook payloads are structured by construction.
    pub completeness: Completeness,
    /// Payload to append to the journal.
    pub payload: ObservationPayload,
    /// Native session id, when the event carried one.
    pub session_id: Option<String>,
    /// Transcript path, when the event carried one.
    pub transcript_path: Option<String>,
}

/// Classify one hook event and build its journal payload.
#[must_use]
pub fn map_event(event: &HookEvent) -> Mapped {
    let session_id = event.session_id().map(ToOwned::to_owned);
    let transcript_path = event.transcript_path().map(ToOwned::to_owned);
    let (kind, topic, status, severity) = classify(event);
    let mut related = BTreeMap::new();
    related.insert("ppid".into(), event.ppid.to_string());
    if let Some(path) = &transcript_path {
        related.insert("transcriptPath".into(), path.clone());
    }
    if let Some(cwd) = event.text("cwd") {
        related.insert("cwd".into(), cwd.to_owned());
    }
    if let Some(prompt) = event.text("prompt_id") {
        related.insert("promptId".into(), prompt.to_owned());
    }
    // C2 prompt correlation: the Node joins `UserPromptSubmit` onto the
    // command that delivered the prompt by this text. Untyped payloads simply
    // omit the key and the observation stays unattributed (native typing).
    if event.name == "UserPromptSubmit"
        && let Some(prompt) = event.text("prompt")
    {
        related.insert("prompt".into(), prompt.to_owned());
    }
    for (key, field) in [
        ("toolName", "tool_name"),
        ("toolUseId", "tool_use_id"),
        ("turnId", "turn_id"),
        ("messageId", "message_id"),
        ("reason", "reason"),
        ("source", "source"),
        ("permissionMode", "permission_mode"),
        // r-ux-w: which agent a sub-agent hook belongs to. Main-session hooks
        // omit these; workflow sub-agents send `workflow-subagent`.
        ("agentId", "agent_id"),
        ("agentType", "agent_type"),
    ] {
        if let Some(value) = event.text(field) {
            related.insert(key.into(), value.to_owned());
        }
    }
    // r-ux-w: SubagentStop carries the agent transcript path (model/usage/timing
    // tail) and the background-task snapshot (id/status/name/description).
    if let Some(path) = event.text("agent_transcript_path") {
        related.insert("agentTranscriptPath".into(), path.to_owned());
    }
    if let Some(duration) = event
        .payload
        .get("duration_ms")
        .and_then(serde_json::Value::as_u64)
    {
        related.insert("durationMs".into(), duration.to_string());
    }
    // Structured launch handle on PostToolUse(Workflow): runId / taskId /
    // run directory / script path. Verified on claude 2.1.221 (r-ux-w).
    let response = event.payload.get("tool_response");
    for (key, field) in [
        ("runId", "runId"),
        ("taskId", "taskId"),
        ("workflowName", "workflowName"),
        ("transcriptDir", "transcriptDir"),
        ("scriptPath", "scriptPath"),
        ("taskType", "taskType"),
    ] {
        if let Some(value) = response
            .and_then(|v| v.get(field))
            .and_then(serde_json::Value::as_str)
        {
            related.insert(key.into(), value.to_owned());
        }
    }
    // Background tasks on SubagentStop / Stop: curate the workflow row's
    // id+status+name when exactly one workflow task is present.
    let workflow_tasks: Vec<&serde_json::Value> = event
        .payload
        .get("background_tasks")
        .and_then(serde_json::Value::as_array)
        .map(|tasks| {
            tasks
                .iter()
                .filter(|t| t.get("type").and_then(serde_json::Value::as_str) == Some("workflow"))
                .collect()
        })
        .unwrap_or_default();
    if let [task] = workflow_tasks.as_slice() {
        if let Some(value) = task.get("id").and_then(serde_json::Value::as_str) {
            related.insert("backgroundTaskId".into(), value.to_owned());
        }
        if let Some(value) = task.get("status").and_then(serde_json::Value::as_str) {
            related.insert("backgroundTaskStatus".into(), value.to_owned());
        }
        if let Some(value) = task.get("name").and_then(serde_json::Value::as_str) {
            related.insert("backgroundTaskName".into(), value.to_owned());
        }
    }
    // Notification advisory fields, for the web's in-app notification list.
    // `notification_type` distinguishes an idle prompt from a permission hint;
    // `message` is the human-readable line. Scoped to Notification so no other
    // event's `message`/type field leaks into these keys.
    if event.name == "Notification" {
        for (key, field) in [
            ("notificationType", "notification_type"),
            ("notificationType", "notificationType"),
            ("message", "message"),
        ] {
            if let Some(value) = event.text(field) {
                related
                    .entry(key.into())
                    .or_insert_with(|| value.to_owned());
            }
        }
    }
    // MessageDisplay's delta is the payload P3 needs; index/final say where the
    // line belongs and whether a later block will replace it.
    if let Some(index) = event
        .payload
        .get("index")
        .and_then(serde_json::Value::as_u64)
    {
        related.insert("index".into(), index.to_string());
    }
    if let Some(final_) = event
        .payload
        .get("final")
        .and_then(serde_json::Value::as_bool)
    {
        related.insert("final".into(), final_.to_string());
    }
    if let Some(delta) = event
        .payload
        .get("delta")
        .and_then(serde_json::Value::as_str)
    {
        related.insert("delta".into(), delta.to_owned());
    }

    let payload = ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
        NativeLifecycle {
            topic,
            native_name: event.name.clone(),
            native_id: match &session_id {
                Some(value) => Knowledge::Known {
                    value: value.clone(),
                },
                None => Knowledge::Unknown {
                    reason: "not-emitted".into(),
                    evidence_event_ids: Vec::new(),
                },
            },
            status: Knowledge::Known {
                value: status.to_owned(),
            },
            related_ids: related,
            data_ref: None,
            severity,
            affects_completion: false,
        },
    ))));

    Mapped {
        kind,
        // A hook payload is the harness telling us directly; nothing was
        // inferred from a screen.
        completeness: Completeness::Structured,
        payload,
        session_id,
        transcript_path,
    }
}

/// `(kind, topic, status, severity)` for one event name.
///
/// `status` is read by the journal projection and by
/// `InteractionRuntime::ingest`, both of which match on the substrings
/// `idle` / `working` / `waiting`. Those spellings are therefore load-bearing,
/// not decorative.
fn classify(event: &HookEvent) -> (MappedKind, LifecycleTopic, &'static str, Severity) {
    match event.name.as_str() {
        "SessionStart" => (
            MappedKind::SessionStarted,
            LifecycleTopic::Hook,
            "idle",
            Severity::Info,
        ),
        "UserPromptSubmit" => (
            MappedKind::TurnStarted,
            LifecycleTopic::Turn,
            "working",
            Severity::Info,
        ),
        "Stop" => (
            MappedKind::TurnEnded,
            LifecycleTopic::Turn,
            "idle",
            Severity::Info,
        ),
        // A turn that ended badly still ended: the composer must come back.
        // Severity stays Info because the *session* is healthy — Error here
        // would make the journal projection fold the instance to `failed`.
        "StopFailure" => (
            MappedKind::TurnEnded,
            LifecycleTopic::Turn,
            "idle",
            Severity::Warning,
        ),
        "SessionEnd" => (
            MappedKind::SessionEnded,
            LifecycleTopic::Hook,
            "ended",
            Severity::Info,
        ),
        "MessageDisplay" => (
            MappedKind::MessageDelta,
            LifecycleTopic::Hook,
            "streaming",
            Severity::Info,
        ),
        "Notification" => (
            // A Notification is an advisory, never a blocking request: its
            // status stays inert (`observed`) so the journal projection and
            // `hook_activity` never fold it to `waiting`, and it opens no live
            // phase. The web surfaces it as an in-app notification instead.
            MappedKind::NotificationObserved,
            LifecycleTopic::Hook,
            "observed",
            Severity::Info,
        ),
        "PermissionRequest" | "Elicitation" => (
            MappedKind::InteractionObserved,
            LifecycleTopic::Permission,
            "waiting",
            Severity::Info,
        ),
        // Tool events keep the inert `"observed"` status on purpose: they are
        // intra-turn progress, never turn boundaries (D-6), so `hook_activity`
        // and `InteractionRuntime::ingest` see exactly what they saw before.
        // The live layer gives them real payloads from a second emission.
        "PreToolUse" => (
            MappedKind::ToolStarted,
            LifecycleTopic::Diagnostic,
            "observed",
            Severity::Info,
        ),
        "PostToolUse" | "PostToolBatch" => (
            MappedKind::ToolFinished,
            LifecycleTopic::Diagnostic,
            "observed",
            Severity::Info,
        ),
        "PostToolUseFailure" => (
            MappedKind::ToolFailed,
            LifecycleTopic::Diagnostic,
            "observed",
            Severity::Info,
        ),
        // SubagentStop reaches us only if something registers it; it is never
        // turn evidence, so it lands as an inert diagnostic.
        _ => (
            MappedKind::Diagnostic,
            LifecycleTopic::Diagnostic,
            "observed",
            Severity::Info,
        ),
    }
}

/// The live [`Phase`] a classified event opens, if any (design §2.2).
///
/// `SessionStart` / `SessionEnd` / `SubagentStop` and unknown events have no
/// phase: a phase nobody produces is absent, and the projection latches the
/// previous one instead of collapsing to idle. `thinking` and `tool-output`
/// have no claude channel and are intentionally unreachable here.
#[must_use]
pub fn phase(_event: &HookEvent, kind: MappedKind) -> Option<Phase> {
    Some(match kind {
        MappedKind::TurnStarted => Phase::PromptAccepted,
        MappedKind::TurnEnded => Phase::TurnEnded,
        MappedKind::InteractionObserved => Phase::Blocked,
        MappedKind::MessageDelta => Phase::TextStreaming,
        MappedKind::ToolStarted => Phase::ToolStarted,
        MappedKind::ToolFinished | MappedKind::ToolFailed => Phase::ToolFinished,
        // A Notification never raises a phase: it is an idle-time advisory, not
        // a turn transition, so it leaves the latch untouched (the previous
        // phase holds until a real boundary or the reducer's screen/transcript
        // fold ends the turn).
        MappedKind::NotificationObserved
        | MappedKind::SessionStarted
        | MappedKind::SessionEnded
        | MappedKind::Diagnostic => {
            return None;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &str, payload: serde_json::Value) -> HookEvent {
        HookEvent {
            name: name.into(),
            ppid: 4242,
            payload,
        }
    }

    fn native(mapped: &Mapped) -> &NativeLifecycle {
        let ObservationPayload::Lifecycle(payload) = &mapped.payload else {
            panic!("expected a lifecycle payload");
        };
        let LifecyclePayload::Native(native) = payload.as_ref() else {
            panic!("expected a native lifecycle");
        };
        native
    }

    #[test]
    fn a_prompt_opens_a_turn_and_a_stop_closes_it() {
        let started = map_event(&event("UserPromptSubmit", serde_json::json!({})));
        assert_eq!(started.kind, MappedKind::TurnStarted);
        assert_eq!(native(&started).topic, LifecycleTopic::Turn);
        // The projection reads the status text, so the spelling matters.
        assert_eq!(
            native(&started).status,
            Knowledge::Known {
                value: "working".into()
            }
        );
        let ended = map_event(&event("Stop", serde_json::json!({})));
        assert_eq!(ended.kind, MappedKind::TurnEnded);
        assert_eq!(
            native(&ended).status,
            Knowledge::Known {
                value: "idle".into()
            }
        );
    }

    #[test]
    fn a_failed_stop_still_ends_the_turn() {
        // Otherwise a crashed turn leaves the composer disabled forever.
        let mapped = map_event(&event("StopFailure", serde_json::json!({})));
        assert_eq!(mapped.kind, MappedKind::TurnEnded);
        assert_eq!(
            native(&mapped).status,
            Knowledge::Known {
                value: "idle".into()
            }
        );
        assert_eq!(native(&mapped).severity, Severity::Warning);
    }

    #[test]
    fn subagent_stop_is_never_turn_evidence() {
        // It fires with no subagent at all (design §3.1 [V]).
        let mapped = map_event(&event("SubagentStop", serde_json::json!({})));
        assert_eq!(mapped.kind, MappedKind::Diagnostic);
        assert_eq!(native(&mapped).topic, LifecycleTopic::Diagnostic);
        let status = &native(&mapped).status;
        let Knowledge::Known { value } = status else {
            panic!("expected a known status");
        };
        assert!(
            !value.contains("working") && !value.contains("idle"),
            "SubagentStop must not move the turn state, got {value}"
        );
    }

    #[test]
    fn a_failed_stop_does_not_read_as_an_instance_failure() {
        // `native_lifecycle_failed` folds Severity::Error to `failed`; a turn
        // that stopped badly must not kill the instance.
        assert_ne!(
            native(&map_event(&event("StopFailure", serde_json::json!({})))).severity,
            Severity::Error
        );
    }

    #[test]
    fn session_start_carries_the_session_id_and_transcript() {
        let mapped = map_event(&event(
            "SessionStart",
            serde_json::json!({
                "session_id": "0199a1f0-0000-7000-8000-000000000000",
                "transcript_path": "/w/.claude/projects/p/s.jsonl",
                "cwd": "/w",
                "source": "startup",
            }),
        ));
        assert_eq!(mapped.kind, MappedKind::SessionStarted);
        assert_eq!(
            mapped.session_id.as_deref(),
            Some("0199a1f0-0000-7000-8000-000000000000")
        );
        assert_eq!(
            mapped.transcript_path.as_deref(),
            Some("/w/.claude/projects/p/s.jsonl")
        );
        let native = native(&mapped);
        assert_eq!(
            native.native_id,
            Knowledge::Known {
                value: "0199a1f0-0000-7000-8000-000000000000".into()
            }
        );
        // The Node binds on ppid, so it has to survive into the journal.
        assert_eq!(native.related_ids["ppid"], "4242");
        assert_eq!(
            native.related_ids["transcriptPath"],
            "/w/.claude/projects/p/s.jsonl"
        );
    }

    #[test]
    fn a_message_delta_keeps_everything_p3_needs_to_rebuild_the_message() {
        let mapped = map_event(&event(
            "MessageDisplay",
            serde_json::json!({
                "turn_id": "t-1",
                "message_id": "m-1",
                "index": 0,
                "final": true,
                "delta": "done",
            }),
        ));
        assert_eq!(mapped.kind, MappedKind::MessageDelta);
        let related = &native(&mapped).related_ids;
        assert_eq!(related["turnId"], "t-1");
        assert_eq!(related["messageId"], "m-1");
        assert_eq!(related["index"], "0");
        assert_eq!(related["final"], "true");
        assert_eq!(related["delta"], "done");
    }

    #[test]
    fn a_permission_request_is_observed_as_waiting() {
        let mapped = map_event(&event(
            "PermissionRequest",
            serde_json::json!({"tool_name": "Write"}),
        ));
        assert_eq!(mapped.kind, MappedKind::InteractionObserved);
        assert_eq!(native(&mapped).related_ids["toolName"], "Write");
        assert_eq!(
            native(&mapped).status,
            Knowledge::Known {
                value: "waiting".into()
            }
        );
    }

    #[test]
    fn every_hook_payload_is_structured_not_screen_derived() {
        for name in ["SessionStart", "Stop", "MessageDisplay", "Notification"] {
            assert_eq!(
                map_event(&event(name, serde_json::json!({}))).completeness,
                Completeness::Structured
            );
        }
    }

    #[test]
    fn an_idle_prompt_notification_never_raises_a_phase_or_a_wait() {
        // Root cause (ins_01a0b3b2, 2026-09-18 17:03): a Stop, then pty idle,
        // then a Notification carrying Claude Code's idle prompt was classified
        // as `blocked` / waiting and latched, stranding the composer and the
        // held queue. A Notification is an advisory, so it must map to its own
        // inert kind with an observed status and open no phase.
        let mapped = map_event(&event(
            "Notification",
            serde_json::json!({
                "notification_type": "idle_prompt",
                "message": "Claude is waiting for your input",
            }),
        ));
        assert_eq!(mapped.kind, MappedKind::NotificationObserved);
        assert_eq!(native(&mapped).topic, LifecycleTopic::Hook);
        assert_eq!(
            native(&mapped).status,
            Knowledge::Known {
                value: "observed".into()
            }
        );
        let related = &native(&mapped).related_ids;
        assert_eq!(related["notificationType"], "idle_prompt");
        assert_eq!(related["message"], "Claude is waiting for your input");
        assert!(phase(&event("Notification", serde_json::json!({})), mapped.kind).is_none());
    }

    #[test]
    fn a_permission_notification_is_still_an_advisory_not_a_blocking_wait() {
        // Even a permission_prompt *Notification* is not the blocking
        // PermissionRequest hook: the Node answers nothing for it, so it never
        // moves the instance to waiting — the web links it to the real dialog
        // instead. Only the actual PermissionRequest/Elicitation hooks block.
        let mapped = map_event(&event(
            "Notification",
            serde_json::json!({"notification_type": "permission_prompt"}),
        ));
        assert_eq!(mapped.kind, MappedKind::NotificationObserved);
        assert!(phase(&event("Notification", serde_json::json!({})), mapped.kind).is_none());

        for name in ["PermissionRequest", "Elicitation"] {
            let request = map_event(&event(name, serde_json::json!({})));
            assert_eq!(request.kind, MappedKind::InteractionObserved);
            assert_eq!(
                phase(&event(name, serde_json::json!({})), request.kind),
                Some(Phase::Blocked)
            );
        }
    }
}
