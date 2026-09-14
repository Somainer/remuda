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
    )
    // The recording contains a `PermissionRequest`, and since P5 that parks the
    // hook until somebody answers or the wait expires. Nobody answers here —
    // this fixture is about the *fold*, not about adjudication — so the wait is
    // shortened from the broker-aligned 15 minutes to something a test can
    // spend. It still ends in a deny, which is the point of §4.4.
    .with_blocking_wait(std::time::Duration::from_millis(50));
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
async fn a_permission_request_holds_the_instance_waiting_on_a_human() {
    // Since P5 the hook is parked on this and the instance really is blocked
    // on a person, so `WaitingInteraction` is now a statement of fact rather
    // than an observation about a prompt someone else will answer.
    let journal = journal_recorded_session().await;
    let end = journal
        .iter()
        .position(|observation| native_name(observation) == Some("PermissionRequest"))
        .expect("the recording includes a permission prompt");
    assert_eq!(project(&journal[..=end]), Activity::WaitingInteraction);
}

#[tokio::test]
async fn a_permission_request_opens_a_card_a_device_can_answer() {
    // The fold is only half the story: without a journaled
    // `interaction.requested` there is nothing for a human to answer, and the
    // hook would wait out its whole deadline for a card nobody ever saw.
    let journal = journal_recorded_session().await;
    let card = journal
        .iter()
        .find_map(|observation| match &observation.body {
            ObservationPayload::InteractionRequested(payload) => Some(&payload.interaction),
            _ => None,
        })
        .expect("the permission request opened an interaction");
    assert_eq!(
        card.carrier,
        remuda_protocol::InteractionCarrier::HarnessHook
    );
    assert!(card.blocking, "the agent is parked on this answer");
    assert!(card.answerable);
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

/// Drive the bus with the multi-chunk recording and fold it the way the
/// observation pump does, returning the journal in order.
async fn journal_streamed_session() -> Vec<Observation> {
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
    for line in remuda_testing::hook_message_stream_fixture().lines() {
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
    // Mirror `spawn_observation_pump`: journal the hook, then the message it
    // yields, so the two stay interleaved in arrival order.
    let mut assembler = remuda_node::MessageAssembler::new();
    let mut out = Vec::new();
    while let Ok(observation) = rx.try_recv() {
        let folded = remuda_node::message_delta(&observation)
            .and_then(|delta| assembler.fold(&delta))
            .map(|payload| remuda_node::MessageAssembler::observation(&observation, payload));
        out.push(observation);
        out.extend(folded);
    }
    out
}

/// The P3 acceptance: «结构视图行级增量出现». Text must reach the journal as an
/// open/append chain while the turn runs, not as one payload at the end.
#[tokio::test]
async fn streamed_deltas_reach_the_journal_as_incremental_message_mutations() {
    use remuda_protocol::{ContentStatus, MutationOperation};
    let journal = journal_streamed_session().await;
    let messages: Vec<_> = journal
        .iter()
        .filter_map(|observation| match &observation.body {
            ObservationPayload::Message(payload) => Some(payload),
            _ => None,
        })
        .collect();
    assert_eq!(
        messages.len(),
        2,
        "each recorded chunk becomes its own visible mutation"
    );
    assert_eq!(messages[0].mutation.operation, MutationOperation::Open);
    assert_eq!(messages[0].status, ContentStatus::Streaming);
    assert_eq!(messages[1].mutation.operation, MutationOperation::Append);
    assert_eq!(messages[1].status, ContentStatus::Complete);
    assert_eq!(
        messages[0].mutation.node_id, messages[1].mutation.node_id,
        "both chunks are one message, not two bubbles"
    );
}

/// A streamed message must appear *before* the turn ends, or the user is still
/// staring at an empty pane for the whole turn — the thing they complained
/// about.
#[tokio::test]
async fn the_first_text_is_journaled_before_the_turn_stops() {
    let journal = journal_streamed_session().await;
    let first_text = journal
        .iter()
        .position(|observation| matches!(observation.body, ObservationPayload::Message(_)))
        .expect("text reached the journal");
    let stop = journal
        .iter()
        .position(|observation| native_name(observation) == Some("Stop"))
        .expect("the turn ended");
    assert!(
        first_text < stop,
        "text must be visible mid-turn, not only once the turn is over"
    );
}

/// The hook event itself stays in the journal beside the message it produced:
/// it is the evidence, and dropping it would make the message unexplainable.
#[tokio::test]
async fn the_hook_evidence_survives_beside_the_derived_message() {
    let journal = journal_streamed_session().await;
    let deltas = journal
        .iter()
        .filter(|observation| native_name(observation) == Some("MessageDisplay"))
        .count();
    assert_eq!(deltas, 2, "both hook events are still journaled");
}
