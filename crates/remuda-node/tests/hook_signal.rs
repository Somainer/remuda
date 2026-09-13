//! Hook events folded into the Node's journal and instance state (D-028 P1).
//!
//! The bus turns a hook payload into an Observation; these tests check what
//! the Node then *does* with it. The two folds that matter in P1 are the ones
//! a user can see: a promoted terminal becoming resumable (D-026), and the
//! composer following the real turn rather than a screen guess.
//!
//! Payloads come from the recording of a real `claude` 2.1.270 run, so the
//! fold is exercised against fields the harness actually sends.

use remuda_node::signal::{binds_instance, hooks_enabled, session_evidence};
use remuda_protocol::{
    Activity, HostId, Id, InstanceId, Knowledge, LifecyclePayload, Observation, ObservationPayload,
    RunId, SourceChannel,
};
use remuda_signal::{BusContext, HookEnvelope, SignalBus};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tokio::sync::mpsc;

/// Drive a bus with the recorded session and collect what it journaled.
async fn journal_recorded_session() -> Vec<Observation> {
    let (tx, mut rx) = mpsc::channel(64);
    let bus = SignalBus::new(
        BusContext {
            instance_id: InstanceId::new(),
            host_id: HostId::new(),
            journal_id: Id::new("obj").unwrap(),
            run_id: RunId::new(),
            driver_kind: remuda_protocol::DriverKind::ShellPty,
            adapter_version: "test".into(),
        },
        tx,
        Arc::new(AtomicU64::new(0)),
    );
    for line in remuda_testing::hook_session_fixture().lines() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        bus.handle(HookEnvelope {
            credential: "fixture".into(),
            event: value["event"].as_str().unwrap().to_owned(),
            ppid: i32::try_from(value["ppid"].as_i64().unwrap()).unwrap(),
            payload: value["payload"].clone(),
        })
        .await;
    }
    drop(bus);
    let mut out = Vec::new();
    while let Ok(observation) = rx.try_recv() {
        out.push(observation);
    }
    out
}

fn native_name(observation: &Observation) -> Option<&str> {
    let ObservationPayload::Lifecycle(payload) = &observation.body else {
        return None;
    };
    match payload.as_ref() {
        LifecyclePayload::Native(native) => Some(native.native_name.as_str()),
        LifecyclePayload::Entity(_) => None,
    }
}

/// Replay the journal through the same projection the Hub and UI read.
fn project(observations: &[Observation]) -> Activity {
    let mut activity = Activity::Idle;
    for observation in observations {
        let ObservationPayload::Lifecycle(payload) = &observation.body else {
            continue;
        };
        let LifecyclePayload::Native(native) = payload.as_ref() else {
            continue;
        };
        // The same substring reading `InteractionRuntime::ingest` and the
        // journal projection both do, which is why the status spellings in
        // `remuda_signal::map` are load-bearing rather than decorative.
        if let Knowledge::Known { value } = &native.status {
            match value.as_str() {
                "idle" => activity = Activity::Idle,
                "working" => activity = Activity::Working,
                "waiting" => activity = Activity::WaitingInteraction,
                _ => {}
            }
        }
    }
    activity
}

#[tokio::test]
async fn a_recorded_session_lands_in_the_journal_on_the_hook_channel() {
    let journal = journal_recorded_session().await;
    assert!(!journal.is_empty(), "the recording produced no events");
    for observation in &journal {
        assert_eq!(
            observation.source.channel,
            SourceChannel::Hook,
            "{:?} must be attributable to the hook channel",
            native_name(observation)
        );
    }
    let names: Vec<&str> = journal.iter().filter_map(native_name).collect();
    for required in ["SessionStart", "UserPromptSubmit", "MessageDisplay", "Stop"] {
        assert!(
            names.contains(&required),
            "{required} missing from {names:?}"
        );
    }
}

#[tokio::test]
async fn the_session_start_makes_a_promoted_terminal_resumable() {
    // D-026: `--resume` needs a real native session id. Before the hook path,
    // a promoted shell-pty had none and resume returned 409.
    let journal = journal_recorded_session().await;
    let evidence = journal
        .iter()
        .find_map(session_evidence)
        .expect("SessionStart carries session evidence");
    assert!(
        !evidence.session_id.is_empty(),
        "a resume needs the native session id"
    );
    assert!(
        evidence
            .transcript_path
            .as_deref()
            .is_some_and(|path| path.ends_with(".jsonl")),
        "and the transcript the structured view hydrates from"
    );
    assert_eq!(
        evidence.agent_pid, 4242,
        "bound to the agent that reported it"
    );
}

#[tokio::test]
async fn the_turn_opens_on_the_prompt_and_closes_on_the_stop() {
    // What the composer follows. Folding the recorded session must leave the
    // instance idle, not stuck working, or the user cannot type again.
    let journal = journal_recorded_session().await;
    let upto = |name: &str| {
        let end = journal
            .iter()
            .position(|observation| native_name(observation) == Some(name))
            .unwrap_or_else(|| panic!("{name} missing"));
        project(&journal[..=end])
    };
    assert_eq!(upto("UserPromptSubmit"), Activity::Working);
    assert_eq!(upto("Stop"), Activity::Idle);
    assert_eq!(
        project(&journal),
        Activity::Idle,
        "a finished session must leave the composer usable"
    );
}

#[tokio::test]
async fn a_permission_request_shows_as_waiting_without_being_answered() {
    let journal = journal_recorded_session().await;
    let end = journal
        .iter()
        .position(|observation| native_name(observation) == Some("PermissionRequest"))
        .expect("the recording includes a permission prompt");
    assert_eq!(project(&journal[..=end]), Activity::WaitingInteraction);
}

#[tokio::test]
async fn message_deltas_are_persisted_for_p3_to_map() {
    // P3 turns these into message mutations. Persisting them now means that is
    // a mapper change and not a re-capture.
    let journal = journal_recorded_session().await;
    let delta = journal
        .iter()
        .find(|observation| native_name(observation) == Some("MessageDisplay"))
        .expect("MessageDisplay reached the journal");
    let ObservationPayload::Lifecycle(payload) = &delta.body else {
        panic!("expected a lifecycle payload");
    };
    let LifecyclePayload::Native(native) = payload.as_ref() else {
        panic!("expected a native lifecycle");
    };
    for key in ["turnId", "messageId", "index", "final", "delta"] {
        assert!(
            native.related_ids.contains_key(key),
            "{key} is missing: {:?}",
            native.related_ids
        );
    }
}

#[tokio::test]
async fn two_terminals_running_their_own_agent_do_not_cross_bind() {
    // The failure this prevents is silent: one session's structured view
    // hydrated from another session's conversation.
    let journal = journal_recorded_session().await;
    let evidence = journal.iter().find_map(session_evidence).unwrap();
    assert!(binds_instance(evidence.agent_pid, Some(4242)));
    assert!(!binds_instance(evidence.agent_pid, Some(7777)));
}

#[test]
fn the_hook_path_is_off_until_the_operator_turns_it_on() {
    assert!(!hooks_enabled(None), "P1 default is off");
    assert!(hooks_enabled(Some("1")));
}
