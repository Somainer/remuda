//! Prompt→command correlation against the real `fake-harness` (workbench C2).
//!
//! The harness is spawned under a PTY the way the production `claude-pty`
//! driver spawns the real CLI: it fires its `UserPromptSubmit` hook through a
//! settings overlay (fixture `hook.sh`, which logs the payload) and writes the
//! Claude-shaped transcript JSONL under its home. Both artifacts are then
//! replayed through the *production* mappers — `SignalBus`/`map_event` for the
//! hook, `TranscriptMapper` for the transcript — and attributed by the same
//! [`PromptCorrelator`] the Node's observation pump runs.
//!
//! Replay (rather than driving the correlator live as bytes are typed) is
//! deliberate: it makes the assertions deterministic and lets one capture be
//! correlated twice — once "as if the first prompt was a command", once with
//! no registration (the natively-typed case).
//!
//! No real model is ever invoked: `fake-harness` plays an echo turn in
//! milliseconds and exits.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, NativePtySystem, PtySize, PtySystem};
use remuda_driver::{TranscriptMapper, encode_project_dir};
use remuda_node::prompt_correlation::PromptCorrelator;
use remuda_protocol::{
    CommandId, DriverKind, HostId, Id, InstanceId, MessageOrigin, MessageRole, Observation,
    ObservationPayload, RunId,
};
use remuda_signal::{BusContext, HookEnvelope, SignalBus};
use remuda_testing::{ensure_workspace_bin, fixtures_dir};
use tokio::sync::mpsc;

const SESSION_ID: &str = "01993ab0-0000-7000-8000-000000000c02";

/// One captured harness run: the hook log lines and the transcript JSONL.
struct Capture {
    hook_events: Vec<serde_json::Value>,
    transcript: String,
    _child: Box<dyn Child + Send + Sync>,
}

/// Spawn fake-harness under a real PTY, submit `prompts` as separate turns
/// (body and Enter in distinct writes, the recipe the input parser requires),
/// wait for its `quit_after_turns` exit, and collect both evidence channels.
fn run_harness(turns: u32, prompts: &[&str]) -> Capture {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&cwd).expect("cwd");

    // Round-trip hook: appends every payload as one JSON line.
    let hook_sh = fixtures_dir().join("fake-harness/hooks/hook.sh");
    let hook_log = dir.path().join("hooks.jsonl");
    let overlay = dir.path().join("settings.json");
    std::fs::write(
        &overlay,
        serde_json::json!({
            "hooks": {
                "SessionStart": [{"hooks": [{"type": "command", "command": hook_sh.to_string_lossy()}]}],
                "UserPromptSubmit": [{"hooks": [{"type": "command", "command": hook_sh.to_string_lossy()}]}]
            }
        })
        .to_string(),
    )
    .expect("overlay");
    let script = dir.path().join("scenario.json");
    std::fs::write(
        &script,
        serde_json::json!({ "quit_after_turns": turns }).to_string(),
    )
    .expect("script");
    let events = dir.path().join("events.jsonl");

    let pty_system = NativePtySystem::default();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new(ensure_workspace_bin("fake-harness"));
    cmd.arg("--kind");
    cmd.arg("claude");
    cmd.arg("--home");
    cmd.arg(&home);
    cmd.arg("--cwd");
    cmd.arg(&cwd);
    cmd.arg("--script");
    cmd.arg(&script);
    cmd.arg("--settings");
    cmd.arg(&overlay);
    cmd.arg("--session-id");
    cmd.arg(SESSION_ID);
    cmd.arg("--events-out");
    cmd.arg(&events);
    cmd.env("HOME", &home);
    cmd.env("TERM", "xterm-256color");
    cmd.env("FAKE_HARNESS_HOOK_LOG", &hook_log);
    let mut child = pair.slave.spawn_command(cmd).expect("spawn");
    drop(pair.slave);
    let master = pair.master;
    let mut writer = master.take_writer().expect("writer");
    let mut reader = master.try_clone_reader().expect("reader");
    // Drain screen output for the whole run so repaints cannot stall on a full
    // PTY buffer.
    let drain = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while matches!(reader.read(&mut buf), Ok(n) if n > 0) {}
    });

    // Let the harness paint the idle composer before typing.
    std::thread::sleep(Duration::from_millis(400));
    for prompt in prompts {
        writer.write_all(prompt.as_bytes()).expect("write body");
        writer.flush().expect("flush");
        std::thread::sleep(Duration::from_millis(120));
        // The verified safe submit: Enter must be its own read.
        writer.write_all(b"\r").expect("write enter");
        writer.flush().expect("flush");
        // Give the turn room to run before the next submission.
        std::thread::sleep(Duration::from_millis(300));
    }

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if child.try_wait().expect("try_wait").is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fake-harness did not quit after {turns} turns"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = drain.join();
    drop(master);

    let hook_events = std::fs::read_to_string(&hook_log)
        .expect("hook log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("hook log line"))
        .collect();
    let transcript_path = home
        .join("projects")
        .join(encode_project_dir(&cwd))
        .join(format!("{SESSION_ID}.jsonl"));
    let transcript = std::fs::read_to_string(transcript_path).expect("transcript jsonl");

    Capture {
        hook_events,
        transcript,
        _child: child,
    }
}

/// Replay the hook capture through the production bus, correlating each
/// `UserPromptSubmit` with `correlator`. Returns the resulting observations in
/// arrival order.
async fn correlated_hook_observations(
    capture: &Capture,
    correlator: &PromptCorrelator,
) -> Vec<Observation> {
    let (tx, mut rx) = mpsc::channel(64);
    let bus = SignalBus::new(
        BusContext {
            instance_id: InstanceId::new(),
            host_id: HostId::new(),
            journal_id: Id::new("obj").unwrap(),
            run_id: RunId::new(),
            driver_kind: DriverKind::ClaudePty,
            adapter_version: "test".into(),
        },
        tx,
        Arc::new(AtomicU64::new(0)),
    );
    for line in &capture.hook_events {
        let event = line["event"].as_str().expect("event name").to_owned();
        bus.handle(HookEnvelope {
            credential: String::new(),
            event,
            ppid: 4242,
            payload: line["payload"].clone(),
        })
        .await;
    }
    drop(bus);
    let mut out = Vec::new();
    while let Ok(mut observation) = rx.try_recv() {
        correlator.correlate(&mut observation);
        out.push(observation);
    }
    out
}

/// The `commandId` the correlator stamped on a `UserPromptSubmit`, if any.
fn hook_command_id(observation: &Observation) -> Option<&str> {
    let ObservationPayload::Lifecycle(lifecycle) = &observation.body else {
        return None;
    };
    let remuda_protocol::LifecyclePayload::Native(native) = lifecycle.as_ref() else {
        return None;
    };
    if native.native_name != "UserPromptSubmit" {
        return None;
    }
    native.related_ids.get("commandId").map(String::as_str)
}

/// Replay transcript lines through the production mapper, correlate each, and
/// return the human user-message observations in file order.
fn correlated_user_messages(
    capture: &Capture,
    correlator: &PromptCorrelator,
) -> Vec<(Id, Option<CommandId>, String)> {
    let mut mapper = TranscriptMapper::new(
        DriverKind::ClaudePty,
        InstanceId::new(),
        RunId::new(),
        Id::new("obj").unwrap(),
        HostId::new(),
        SESSION_ID.to_owned(),
        "2.1.270".into(),
    );
    let mut observations: Vec<Observation> = Vec::new();
    for line in capture.transcript.lines() {
        if line.trim().is_empty() {
            continue;
        }
        observations.extend(mapper.map_line(line).expect("map transcript line"));
    }
    observations
        .into_iter()
        .filter_map(|mut observation| {
            let ObservationPayload::Message(payload) = &observation.body else {
                return None;
            };
            if payload.role != MessageRole::User || payload.origin != Some(MessageOrigin::Human) {
                return None;
            }
            correlator.correlate(&mut observation);
            let ObservationPayload::Message(payload) = observation.body else {
                unreachable!()
            };
            let text = payload
                .blocks
                .iter()
                .filter_map(|block| match block {
                    remuda_protocol::ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            Some((payload.message_id.clone(), payload.command_id.clone(), text))
        })
        .collect()
}

#[test]
fn a_command_prompt_becomes_one_user_node_after_hook_and_transcript_replay() {
    let prompt = "via command one";
    let capture = run_harness(1, &[prompt]);

    let command = CommandId::new();
    // The node id the queued/delivered synthesized message was emitted with.
    let queued_node = Id::new("obj").expect("node id");
    let correlator = PromptCorrelator::new();
    correlator.register(command.clone(), queued_node.clone(), prompt.to_owned());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let hooks = runtime.block_on(correlated_hook_observations(&capture, &correlator));
    let submitted: Vec<_> = hooks.iter().filter_map(hook_command_id).collect();
    assert_eq!(
        submitted.as_slice(),
        [command.as_id().as_str()],
        "the hook turn event must carry the delivering command id"
    );

    let users = correlated_user_messages(&capture, &correlator);
    assert_eq!(users.len(), 1, "exactly one human user record was captured");
    let (node_id, stamped, text) = &users[0];
    assert_eq!(text, prompt);
    assert_eq!(stamped.as_ref(), Some(&command));
    assert_eq!(
        node_id, &queued_node,
        "the transcript record joins the queued node instead of minting a second node"
    );
}

#[test]
fn a_prompt_typed_into_the_pty_has_no_command_anywhere() {
    let prompt = "typed by a human only";
    let capture = run_harness(1, &[prompt]);

    // No registration: nobody delivered this through a command.
    let correlator = PromptCorrelator::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let hooks = runtime.block_on(correlated_hook_observations(&capture, &correlator));
    let submitted: Vec<_> = hooks.iter().filter_map(hook_command_id).collect();
    assert!(
        submitted.is_empty(),
        "a natively typed prompt's hook event must stay commandId-less"
    );

    let users = correlated_user_messages(&capture, &correlator);
    assert_eq!(users.len(), 1);
    let (_, stamped, text) = &users[0];
    assert_eq!(text, prompt);
    assert!(
        stamped.is_none(),
        "native typing is never attributed to a command"
    );
}

#[test]
fn two_identical_texts_one_command_one_native_are_two_distinct_attributed_nodes() {
    let prompt = "same text both ways";
    let capture = run_harness(2, &[prompt, prompt]);

    let command = CommandId::new();
    let queued_node = Id::new("obj").expect("queued node");
    let correlator = PromptCorrelator::new();
    // Only the first submission is a Remuda command.
    correlator.register(command.clone(), queued_node.clone(), prompt.to_owned());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let hooks = runtime.block_on(correlated_hook_observations(&capture, &correlator));
    let submitted: Vec<_> = hooks.iter().filter_map(hook_command_id).collect();
    assert_eq!(
        submitted.len(),
        1,
        "only the first submit went through a command"
    );
    assert_eq!(submitted[0], command.as_id().as_str());

    let users = correlated_user_messages(&capture, &correlator);
    assert_eq!(users.len(), 2, "two submissions, two human user records");

    let nodes: HashSet<&Id> = users.iter().map(|(node_id, _, _)| node_id).collect();
    assert_eq!(
        nodes.len(),
        2,
        "the two records must not collapse into one node"
    );

    assert_eq!(users[0].1.as_ref(), Some(&command));
    assert_eq!(
        users[0].0, queued_node,
        "the command prompt joins its queued node"
    );
    assert!(
        users[1].1.is_none(),
        "the second identical submission was typed natively"
    );
    assert_ne!(users[1].0, queued_node);
}
