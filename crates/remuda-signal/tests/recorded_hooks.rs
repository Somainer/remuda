//! Replay of hook payloads recorded from a real `claude` run (D-028 §4.2).
//!
//! These are not authored shapes. `crates/remuda-testing/fixtures/hooks/
//! claude-hook-session.jsonl` was captured by registering a recording hook for
//! every event the overlay registers and driving `claude` 2.1.270 through a
//! tool call, a streamed reply, and a permission prompt. Only host paths and
//! session ids were scrubbed; every field name and nesting is verbatim.
//!
//! The point of replaying them is that the mapper's assumptions are checked
//! against what the harness actually sends, so a payload change shows up here
//! rather than as a silently empty structured view.

use remuda_protocol::{Knowledge, LifecyclePayload, NativeLifecycle, ObservationPayload};
use remuda_signal::{HookEnvelope, HookEvent, MappedKind, map_event};

/// One recorded line: the event name, the agent pid, the payload verbatim.
fn recorded() -> Vec<HookEvent> {
    remuda_testing::hook_session_fixture()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: serde_json::Value =
                serde_json::from_str(line).expect("fixture line is JSON");
            HookEvent::from_envelope(HookEnvelope {
                credential: "fixture".into(),
                event: value["event"].as_str().expect("event name").to_owned(),
                ppid: i32::try_from(value["ppid"].as_i64().expect("ppid")).expect("ppid fits"),
                payload: value["payload"].clone(),
            })
        })
        .collect()
}

fn native(payload: &ObservationPayload) -> &NativeLifecycle {
    let ObservationPayload::Lifecycle(lifecycle) = payload else {
        panic!("expected a lifecycle payload");
    };
    let LifecyclePayload::Native(native) = lifecycle.as_ref() else {
        panic!("expected a native lifecycle");
    };
    native
}

fn find(name: &str) -> HookEvent {
    recorded()
        .into_iter()
        .find(|event| event.name == name)
        .unwrap_or_else(|| panic!("fixture has no {name} event"))
}

#[test]
fn the_recording_covers_every_event_the_p1_overlay_depends_on() {
    let names: Vec<String> = recorded().into_iter().map(|event| event.name).collect();
    for required in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PermissionRequest",
        "PostToolUse",
        "PostToolBatch",
        "MessageDisplay",
        "Stop",
        "SessionEnd",
    ] {
        assert!(
            names.iter().any(|name| name == required),
            "the recorded session is missing {required}; re-record before trusting it"
        );
    }
}

#[test]
fn a_recorded_session_start_yields_the_session_id_and_transcript_to_resume_with() {
    // This is the D-026 path: without both of these a promoted terminal has
    // nothing `--resume` will accept.
    let mapped = map_event(&find("SessionStart"));
    assert_eq!(mapped.kind, MappedKind::SessionStarted);
    let session = mapped
        .session_id
        .expect("recorded SessionStart has a session");
    assert!(
        session.len() == 36 && session.contains('-'),
        "session id should be a uuid, got {session}"
    );
    let transcript = mapped
        .transcript_path
        .expect("recorded SessionStart has a transcript path");
    assert!(
        transcript.ends_with(".jsonl"),
        "transcript should be a JSONL path, got {transcript}"
    );
}

#[test]
fn a_recorded_turn_opens_on_the_prompt_and_closes_on_the_stop() {
    assert_eq!(
        map_event(&find("UserPromptSubmit")).kind,
        MappedKind::TurnStarted
    );
    assert_eq!(map_event(&find("Stop")).kind, MappedKind::TurnEnded);
}

#[test]
fn the_recorded_message_display_carries_a_line_delta_not_a_whole_message() {
    // §7 promises line-level, not token-level. The recording is what makes
    // that claim honest rather than aspirational.
    let event = find("MessageDisplay");
    let mapped = map_event(&event);
    assert_eq!(mapped.kind, MappedKind::MessageDelta);
    let related = &native(&mapped.payload).related_ids;
    for key in ["turnId", "messageId", "index", "final", "delta"] {
        assert!(
            related.contains_key(key),
            "MessageDisplay must carry {key} for P3 to rebuild the message; got {related:?}"
        );
    }
    assert!(
        !related["delta"].is_empty(),
        "a recorded delta should carry text"
    );
}

#[test]
fn the_recorded_permission_request_is_observed_with_its_tool_but_not_answered() {
    let mapped = map_event(&find("PermissionRequest"));
    assert_eq!(mapped.kind, MappedKind::InteractionObserved);
    let related = &native(&mapped.payload).related_ids;
    assert!(
        related.contains_key("toolName"),
        "an approval the user has to judge must name its tool; got {related:?}"
    );
}

#[test]
fn every_recorded_event_carries_the_agent_pid_the_node_binds_on() {
    for event in recorded() {
        let mapped = map_event(&event);
        let related = &native(&mapped.payload).related_ids;
        assert_eq!(
            related.get("ppid").map(String::as_str),
            Some("4242"),
            "{} lost the agent pid; the Node binds the session on it",
            event.name
        );
    }
}

#[test]
fn every_recorded_event_reports_the_session_it_belongs_to() {
    // Claude stamps session_id on every hook payload, so an observation that
    // lacks one is a decoding bug rather than a harness limitation.
    for event in recorded() {
        let mapped = map_event(&event);
        assert!(
            matches!(native(&mapped.payload).native_id, Knowledge::Known { .. }),
            "{} should carry its session id",
            event.name
        );
    }
}
