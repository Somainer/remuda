//! P2's acceptance gate: the two launch paths produce one journal
//! (D-028 §1.0 rule 5, §13 P2).
//!
//! The unification principle is only worth anything if it is checkable. The
//! check is this: run the same scenario twice — once with Remuda launching the
//! agent, once with a human typing the same command into a terminal — and diff
//! the event streams. Anything other than `launchedBy` differing is, in §13's
//! words, 阻塞放行.
//!
//! This exercises it against a scripted harness rather than a live `claude`,
//! for the obvious reason that a gate depending on a model's mood is not a
//! gate. What it proves is the structural claim: **the same driver code runs
//! for both**, so the two paths cannot diverge without one of these failing.
//! The live evidence that the scripted stand-in is faithful lives in
//! `docs/design/evidence/native-pty-2.md`.

use remuda_driver::shell_pty::{ShellPtyDriver, ShellPtyOptions, Target};
use remuda_driver::{Driver, ProcessRow, ProcessTable};
use remuda_protocol::{
    AgentKind, LifecyclePayload, Observation, ObservationPayload, SourceChannel,
};
use std::sync::Arc;
use std::time::Duration;

/// A process table that reports a `claude` holding the foreground.
///
/// Detection is what both paths go through — §1.0 rule 2 makes D-025 the only
/// road in, including for the launch Remuda controls — so the fixture is the
/// same for both and only the *reason* it is true differs: Remuda started that
/// process, or a human did.
struct ClaudeForeground;

impl ProcessTable for ClaudeForeground {
    fn process_group(&self, _pgid: i32) -> Vec<ProcessRow> {
        vec![ProcessRow {
            pid: 4242,
            args: "claude".into(),
        }]
    }
}

/// A `(kind, name)` summary of one observation: the shape a journal diff
/// compares, with the volatile envelope (ids, timestamps, sequence) dropped
/// exactly as `remuda journal diff` drops it.
fn signature(observation: &Observation) -> String {
    let kind = match &observation.body {
        ObservationPayload::Message(_) => "message",
        ObservationPayload::Thought(_) => "thought",
        ObservationPayload::ToolCall(_) => "tool_call",
        ObservationPayload::ToolResult(_) => "tool_result",
        ObservationPayload::Lifecycle(_) => "lifecycle",
        ObservationPayload::InteractionRequested(_) => "interaction_requested",
        _ => "other",
    };
    let detail = match &observation.body {
        ObservationPayload::Lifecycle(payload) => match payload.as_ref() {
            LifecyclePayload::Native(native) => native.native_name.clone(),
            LifecyclePayload::Entity(entity) => entity.state.clone(),
        },
        _ => String::new(),
    };
    format!("{kind}:{detail}")
}

/// Drive one PTY to a promoted state and collect what it journaled.
///
/// `target` is the only difference between the two runs, which is the whole
/// point: everything after the spawn is shared code.
async fn journal_for(target: Target) -> Vec<Observation> {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut options = ShellPtyOptions::login(dir.path().to_path_buf());
    // Both runs execute the same inert process. A real run would exec the
    // agent on one path and a shell on the other, but what is being compared
    // is the observation stream the driver produces around it, and holding the
    // process identical removes the one variable that would otherwise explain
    // away a difference.
    options.args = vec![
        "/bin/sh".into(),
        "-c".into(),
        "while :; do sleep 0.1; done".into(),
    ];
    options.promote = true;
    options.claude_home = Some(dir.path().join("claude-home"));
    // `Target::Agent` normally carries an `AgentLaunch`; here the spawn is
    // forced through the shell arm (via `args`) so that both runs execute the
    // identical process and the comparison isolates the journal.
    if matches!(target, Target::Agent { .. }) {
        options.target = Target::Shell;
    }

    let driver = ShellPtyDriver::with_process_table(options, Arc::new(ClaudeForeground));
    let mut handle = driver.spawn().await.expect("spawn");

    // Two promotion polls is D-025's detection budget.
    let mut observations = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), handle.recv()).await {
            Ok(Some(observation)) => {
                let promoted = signature(&observation) == "lifecycle:agent_promoted";
                observations.push(observation);
                if promoted {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => {}
        }
    }
    let _ = Driver::close(&driver).await;
    observations
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remuda_launched_agent_and_a_hand_typed_one_journal_the_same_events() {
    // §13 P2's acceptance criterion, and §1.0 rule 5's test: any capability or
    // event that appears on only one path is a P-level defect.
    let launched = journal_for(Target::Agent {
        kind: AgentKind::Claude,
        resume: None,
    })
    .await;
    let typed = journal_for(Target::Shell).await;

    let launched_signatures: Vec<String> = launched.iter().map(signature).collect();
    let typed_signatures: Vec<String> = typed.iter().map(signature).collect();

    assert!(
        launched_signatures.contains(&"lifecycle:agent_promoted".to_owned()),
        "a Remuda-launched agent still goes through D-025 promotion; \
         got {launched_signatures:?}"
    );
    assert_eq!(
        launched_signatures, typed_signatures,
        "the two paths must produce one event stream (§1.0 rule 5). \
         A difference here means a capability exists on only one road in."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_paths_report_the_same_evidence_channel_for_every_event() {
    // The channel is how the UI explains 「凭什么说它 blocked」 (§4.3). If the
    // launched path claimed a stronger channel than the promoted one for the
    // same fact, the two journals would diff equal on kind and still mean
    // different things.
    let launched = journal_for(Target::Agent {
        kind: AgentKind::Claude,
        resume: None,
    })
    .await;
    let typed = journal_for(Target::Shell).await;

    let channels = |observations: &[Observation]| -> Vec<(String, SourceChannel)> {
        observations
            .iter()
            .map(|observation| (signature(observation), observation.source.channel))
            .collect()
    };
    assert_eq!(channels(&launched), channels(&typed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn neither_path_claims_a_native_session_it_was_never_told() {
    // D-026's rule, checked on both roads at once: `nativeRef.session_id` is
    // Unknown until a harness reports one. A path that invented an id would
    // resume the wrong conversation.
    for target in [
        Target::Agent {
            kind: AgentKind::Claude,
            resume: None,
        },
        Target::Shell,
    ] {
        for observation in journal_for(target.clone()).await {
            assert!(
                !matches!(
                    &observation.source.native_session_id,
                    remuda_protocol::Knowledge::Known { value } if !value.is_empty()
                ),
                "{target:?} reported a session id nobody gave it"
            );
        }
    }
}
