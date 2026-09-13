//! Node side of the hook channel (D-028 §4.2, §4.3, P1).
//!
//! The driver owns the socket and the [`SignalBus`] that turns hook events into
//! Observations; the Node's job is the fold — what a hook observation *changes*
//! about an instance once it lands in the journal. That is this module.
//!
//! Two folds matter in P1 and both go through code that already exists rather
//! than a new path beside it (§1.0 rule 2: promotion is the only hydration
//! entry):
//!
//! - **Session binding.** `SessionStart` carries the native session id and
//!   transcript path, which is what `--resume` needs (D-026). The existing
//!   `native_session_evidence` already recognises a `SessionStart` hook
//!   lifecycle, so a promoted shell-pty session becomes resumable with no new
//!   store call — but only if the observation reaches the *right* instance,
//!   which is what [`binds_instance`] decides.
//! - **Turn lifecycle.** `UserPromptSubmit` / `Stop` / `StopFailure` set
//!   activity through the same `agent_status`-shaped status text the screen
//!   path already uses, so hook and screen evidence converge instead of
//!   fighting.

use remuda_protocol::{
    LifecyclePayload, NativeLifecycle, Observation, ObservationPayload, SourceChannel,
};

/// Environment flag gating the whole hook path. Off by default in P1.
pub const HOOKS_ENABLE_ENV: &str = "REMUDA_PTY_HOOKS";

/// True when the operator opted this Node into the hook path.
#[must_use]
pub fn hooks_enabled(value: Option<&str>) -> bool {
    matches!(
        value.map(|raw| raw.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "on" | "yes")
    )
}

/// The native lifecycle inside a hook-channel observation, if this is one.
///
/// Channel is checked, not just shape: a screen-derived guess and a hook
/// payload can carry the same `native_name`, and only one of them is evidence.
#[must_use]
pub fn hook_lifecycle(observation: &Observation) -> Option<&NativeLifecycle> {
    if observation.source.channel != SourceChannel::Hook {
        return None;
    }
    let ObservationPayload::Lifecycle(payload) = &observation.body else {
        return None;
    };
    match payload.as_ref() {
        LifecyclePayload::Native(native) => Some(native),
        LifecyclePayload::Entity(_) => None,
    }
}

/// The agent pid a hook observation came from, when it carried one.
///
/// This is the relay's parent — the `claude` process itself.
#[must_use]
pub fn hook_agent_pid(native: &NativeLifecycle) -> Option<i32> {
    native.related_ids.get("ppid")?.parse().ok()
}

/// Whether a `SessionStart` from `agent_pid` belongs to the instance whose PTY
/// foreground process group leader is `foreground_pid`.
///
/// Binding on the pid rather than on "the most recent SessionStart" is what
/// makes this deterministic: a user with two Remuda terminals, each running
/// their own `claude`, produces two interleaved hook streams, and only the pid
/// says which transcript belongs to which instance. Getting it wrong does not
/// fail loudly — it silently hydrates one session's structured view from
/// another session's conversation.
///
/// `foreground_pid` is the process *group* leader. A shim that `exec`s (as ours
/// does) leaves the agent as that leader, so the two are equal; a shim that
/// spawned a child would not, which is one more reason the shim execs.
///
/// TODO(x-promote-bind): once `bind_by_session(pid, session_id,
/// transcript_path)` lands in `shell_pty/promotion.rs`, the SignalBus should
/// call it directly instead of the Node re-deriving the match from the
/// journal. Until then the binding evidence travels in the observation's
/// `ppid` (see [`session_evidence`]), which is why the relay stamps it on
/// every event.
#[must_use]
pub fn binds_instance(agent_pid: i32, foreground_pid: Option<i32>) -> bool {
    match foreground_pid {
        Some(foreground) => agent_pid == foreground,
        // No foreground reading available (no PTY, or `tcgetpgrp` refused).
        // Refusing to bind is the honest answer: an unverified binding is
        // worse than an unresumable session, because it is wrong silently.
        None => false,
    }
}

/// The activity a hook observation proves, if it proves one.
///
/// This is the §4.3 priority made real: without it the hook events are
/// journaled but the *instance* still follows `agent_status`, which is a screen
/// guess. A hook is the harness saying what it is doing, so it outranks the
/// screen and must be what moves the composer.
///
/// Only the three turn-boundary events and the interaction events qualify.
/// Tool events say a turn is in progress but are not its boundaries, and
/// `SubagentStop` fires with no subagent at all (design §3.1 [V]) — treating
/// either as evidence would flip the composer on a non-event.
#[must_use]
pub fn hook_activity(observation: &Observation) -> Option<remuda_protocol::Activity> {
    use remuda_protocol::Activity;
    let native = hook_lifecycle(observation)?;
    match native.native_name.as_str() {
        "UserPromptSubmit" => Some(Activity::Working),
        // A turn that ended badly still ended: the composer has to come back,
        // or the user cannot type again after one failed turn.
        "Stop" | "StopFailure" => Some(Activity::Idle),
        "Notification" | "PermissionRequest" | "Elicitation" => Some(Activity::WaitingInteraction),
        _ => None,
    }
}

/// Session identity carried by a hook `SessionStart`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSessionEvidence {
    /// Agent pid the hook ran under.
    pub agent_pid: i32,
    /// Native session id, for `--resume`.
    pub session_id: String,
    /// Transcript the agent is writing, when reported.
    pub transcript_path: Option<String>,
}

/// Read a `SessionStart` hook observation as session evidence.
///
/// Only `SessionStart` is accepted even though every Claude hook payload
/// carries a `session_id`: taking the id from any event would let a
/// `MessageDisplay` arriving during a race rebind the instance.
#[must_use]
pub fn session_evidence(observation: &Observation) -> Option<HookSessionEvidence> {
    let native = hook_lifecycle(observation)?;
    if native.native_name != "SessionStart" {
        return None;
    }
    let session_id = match &native.native_id {
        remuda_protocol::Knowledge::Known { value } if !value.trim().is_empty() => {
            value.trim().to_owned()
        }
        _ => return None,
    };
    Some(HookSessionEvidence {
        agent_pid: hook_agent_pid(native)?,
        session_id,
        transcript_path: native
            .related_ids
            .get("transcriptPath")
            .map(|path| path.trim().to_owned())
            .filter(|path| !path.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_protocol::{
        Completeness, DriverKind, EventId, HostId, Id, InstanceId, Knowledge, LifecycleTopic,
        NativeRequestKey, ObservationSource, RunId, RuntimeCursor, SchemaVersion, Severity,
        SourceCursor, SourceDelivery, Timestamp, U64,
    };
    use std::collections::BTreeMap;

    fn observation(
        channel: SourceChannel,
        name: &str,
        session: Option<&str>,
        related: &[(&str, &str)],
    ) -> Observation {
        // Generic, not a closure: inference would otherwise pin it to whichever
        // `Knowledge<T>` it is first used at.
        fn unknown<T>() -> Knowledge<T> {
            Knowledge::Unknown {
                reason: "not-emitted".into(),
                evidence_event_ids: Vec::new(),
            }
        }
        Observation {
            schema_version: SchemaVersion,
            event_id: EventId::new(),
            journal_id: Id::new("obj").unwrap(),
            instance_id: InstanceId::new(),
            run_id: Some(RunId::new()),
            host_id: HostId::new(),
            process_generation: U64(1),
            run_generation: Some(U64(1)),
            seq: U64(1),
            observed_at: Timestamp::try_from("2026-09-14T00:00:00.000Z".to_owned()).unwrap(),
            native_at: unknown(),
            source: ObservationSource {
                driver_kind: DriverKind::ShellPty,
                driver_version: "shell-pty".into(),
                adapter_version: "test".into(),
                channel,
                delivery: SourceDelivery::Live,
                native_session_id: unknown(),
                native_turn_id: Knowledge::NotApplicable,
                native_agent_id: Knowledge::NotApplicable,
                native_item_id: Knowledge::NotApplicable,
                native_event_id: Knowledge::NotApplicable,
                native_request_id: NativeRequestKey::None,
                source_cursor: SourceCursor::Runtime(Box::new(RuntimeCursor {
                    ledger_revision: U64(1),
                })),
            },
            completeness: Completeness::Structured,
            raw_ref: None,
            evidence_event_ids: Vec::new(),
            body: ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
                NativeLifecycle {
                    topic: LifecycleTopic::Hook,
                    native_name: name.into(),
                    native_id: match session {
                        Some(value) => Knowledge::Known {
                            value: value.into(),
                        },
                        None => unknown(),
                    },
                    status: Knowledge::Known {
                        value: "idle".into(),
                    },
                    related_ids: related
                        .iter()
                        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                        .collect::<BTreeMap<_, _>>(),
                    data_ref: None,
                    severity: Severity::Info,
                    affects_completion: false,
                },
            )))),
        }
    }

    #[test]
    fn the_flag_is_off_unless_it_is_explicitly_on() {
        for value in ["1", "true", "on", "YES"] {
            assert!(hooks_enabled(Some(value)), "{value}");
        }
        for value in ["0", "false", "off", "", "maybe"] {
            assert!(!hooks_enabled(Some(value)), "{value}");
        }
        assert!(!hooks_enabled(None), "P1 default must be off");
    }

    #[test]
    fn the_turn_boundaries_move_the_instance_and_nothing_else_does() {
        use remuda_protocol::Activity;
        let hook =
            |name: &str| observation(SourceChannel::Hook, name, Some("s-1"), &[("ppid", "42")]);
        assert_eq!(
            hook_activity(&hook("UserPromptSubmit")),
            Some(Activity::Working)
        );
        assert_eq!(hook_activity(&hook("Stop")), Some(Activity::Idle));
        // A failed turn still frees the composer.
        assert_eq!(hook_activity(&hook("StopFailure")), Some(Activity::Idle));
        assert_eq!(
            hook_activity(&hook("PermissionRequest")),
            Some(Activity::WaitingInteraction)
        );
        // Tool events are progress, not boundaries; SubagentStop is not
        // evidence at all.
        for name in [
            "PreToolUse",
            "PostToolUse",
            "PostToolBatch",
            "MessageDisplay",
            "SubagentStop",
            "SessionStart",
        ] {
            assert_eq!(hook_activity(&hook(name)), None, "{name} must not move it");
        }
    }

    #[test]
    fn a_screen_guess_cannot_masquerade_as_hook_activity() {
        // Hook outranks screen (§4.3); that only holds if the channel is what
        // decides, not the event name.
        assert_eq!(
            hook_activity(&observation(
                SourceChannel::Pty,
                "UserPromptSubmit",
                Some("s-1"),
                &[("ppid", "42")]
            )),
            None
        );
    }

    #[test]
    fn only_the_hook_channel_counts_as_hook_evidence() {
        // A screen guess can carry the same name; only one of them is proof.
        let screen = observation(
            SourceChannel::Pty,
            "SessionStart",
            Some("s-1"),
            &[("ppid", "42")],
        );
        assert!(hook_lifecycle(&screen).is_none());
        assert!(session_evidence(&screen).is_none());
    }

    #[test]
    fn a_session_start_yields_the_pid_session_and_transcript() {
        let event = observation(
            SourceChannel::Hook,
            "SessionStart",
            Some("0199a1f0-0000-7000-8000-000000000000"),
            &[("ppid", "4242"), ("transcriptPath", "/w/s.jsonl")],
        );
        assert_eq!(
            session_evidence(&event),
            Some(HookSessionEvidence {
                agent_pid: 4242,
                session_id: "0199a1f0-0000-7000-8000-000000000000".into(),
                transcript_path: Some("/w/s.jsonl".into()),
            })
        );
    }

    #[test]
    fn only_session_start_rebinds_the_session() {
        // Every claude hook payload carries a session_id; taking it from any of
        // them would let a mid-turn event rebind the instance.
        for name in ["MessageDisplay", "Stop", "UserPromptSubmit", "Notification"] {
            let event = observation(
                SourceChannel::Hook,
                name,
                Some("s-other"),
                &[("ppid", "4242")],
            );
            assert!(session_evidence(&event).is_none(), "{name} must not bind");
        }
    }

    #[test]
    fn a_session_start_without_a_pid_is_not_bindable() {
        let event = observation(SourceChannel::Hook, "SessionStart", Some("s-1"), &[]);
        assert!(session_evidence(&event).is_none());
    }

    #[test]
    fn binding_requires_the_pid_to_be_the_ptys_foreground_leader() {
        assert!(binds_instance(4242, Some(4242)));
        // Another terminal's claude must not hydrate this instance.
        assert!(!binds_instance(4242, Some(9999)));
    }

    #[test]
    fn an_unreadable_foreground_refuses_to_bind_rather_than_guessing() {
        // An unresumable session is recoverable; a session hydrated from
        // someone else's conversation is silently wrong.
        assert!(!binds_instance(4242, None));
    }
}
