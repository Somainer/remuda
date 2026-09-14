//! D-028 P6 parity: the codex and grok file adapters over `--kind
//! codex|grok` fake-harness runs describe the same journal facts, and each
//! adapter dump is self-equal under `remuda journal diff`.
//!
//! The fake harness runs inside a real portable-pty (it sets raw mode), the
//! same way `crates/remuda-testing/tests/fake_harness.rs` drives it. After it
//! exits, its on-disk artifacts are exactly what a real session writes; the
//! adapters tail those and produce stamped observations.

use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use remuda_driver::adapters::{
    AdapterHome, CodexAdapter, FileSignalAdapter, GrokAdapter, StampCtx, stamp,
};
use remuda_protocol::{AgentKind, HostId, Id, InstanceId, Observation, ObservationPayload, RunId};

/// One short auto-tool turn both harnesses replay.
const SCENARIO: &str = r#"{
  "turns": [{
    "match": "RUN_PARITY",
    "text": "SPIKE_COMPLETE parity",
    "thinking": "Working through the parity check.",
    "tools": [{
      "name": "Bash",
      "input": { "command": "printf PARITY_OK" },
      "approval": "auto",
      "exit_code": 0
    }],
    "usage": { "input_tokens": 100, "output_tokens": 20 }
  }],
  "quit_after_turns": 1
}"#;

#[test]
fn codex_and_grok_adapter_dumps_are_journal_diff_parity() {
    let codex = run_and_adapt(AgentKind::Codex);
    let grok = run_and_adapt(AgentKind::Grok);

    assert_parity_facts(&codex, AgentKind::Codex);
    assert_parity_facts(&grok, AgentKind::Grok);

    let dir = tempfile::tempdir().unwrap();
    let codex_dump = dir.path().join("codex.json");
    let grok_dump = dir.path().join("grok.json");
    write_dump(&codex_dump, &codex);
    write_dump(&grok_dump, &grok);
    // Each dump is journal-diff equal to itself (no duplicate/failed facts).
    for dump in [&codex_dump, &grok_dump] {
        let output = diff(dump, dump);
        assert!(
            output.status.success(),
            "self-diff failed for {}: {}",
            dump.display(),
            output.stderr
        );
    }
}

/// Spawn the fake harness in a PTY, submit the turn, wait for exit, and adapt.
fn run_and_adapt(kind: AgentKind) -> Vec<Observation> {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let cwd = root.path().join("work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let script = root.path().join("scenario.json");
    std::fs::write(&script, SCENARIO).unwrap();
    let session = match kind {
        AgentKind::Codex => "00000000-0000-7000-8000-0000000000c1".to_owned(),
        AgentKind::Grok => "00000000-0000-7000-8000-000000000061".to_owned(),
        _ => unreachable!(),
    };

    let binary = remuda_testing::ensure_workspace_bin("fake-harness");
    let pty_system = NativePtySystem::default();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(binary);
    cmd.cwd(&cwd);
    cmd.arg("--kind");
    cmd.arg(if kind == AgentKind::Codex {
        "codex"
    } else {
        "grok"
    });
    cmd.arg("--home");
    cmd.arg(&home);
    cmd.arg("--cwd");
    cmd.arg(&cwd);
    cmd.arg("--session-id");
    cmd.arg(&session);
    cmd.arg("--script");
    cmd.arg(&script);
    cmd.arg("--no-alt-screen");
    // Isolate from the host environment the fake resolves homes from.
    cmd.env("HOME", root.path());
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let master = pair.master;
    let mut writer = master.take_writer().unwrap();
    // Drain PTY output on a thread so repaints never block.
    let mut reader = master.try_clone_reader().unwrap();
    let drainer = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });

    // Body then Enter are separate writes (the fake swallows a CR that arrives
    // in the same read as the body).
    std::thread::sleep(Duration::from_millis(700));
    writer.write_all(b"RUN_PARITY").expect("write prompt");
    writer.flush().expect("flush");
    std::thread::sleep(Duration::from_millis(220));
    writer.write_all(b"\r").expect("submit");
    writer.flush().expect("flush");

    wait_exit(&mut child, Duration::from_secs(20));
    drop(master);
    let _ = drainer.join();

    match kind {
        AgentKind::Codex => {
            let mut adapter = CodexAdapter::new(AdapterHome {
                home: home.clone(),
                cwd: cwd.clone(),
                pid: None,
            });
            let rollout = remuda_driver::codex_rollout::locate_rollout_in(&home, &session)
                .expect("codex rollout written by the fake harness");
            adapter.bind_rollout(&session, rollout);
            drain(&mut adapter, &session, "codex")
        }
        AgentKind::Grok => {
            let mut adapter = GrokAdapter::new(AdapterHome {
                home: home.clone(),
                cwd: cwd.clone(),
                pid: None,
            });
            // The fake empties active_sessions.json on shutdown, so bind the
            // known session directory directly (as a SessionStart hook resolves
            // it at runtime).
            let dir = home
                .join("sessions")
                .join(remuda_driver::grok_session::encode_session_cwd(&cwd))
                .join(&session);
            assert!(
                dir.join("updates.jsonl").is_file(),
                "grok updates written: {}",
                dir.display()
            );
            adapter.bind_session_dir(&session, dir);
            drain(&mut adapter, &session, "grok")
        }
        _ => unreachable!(),
    }
}

fn wait_exit(child: &mut Box<dyn portable_pty::Child + Send + Sync>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                assert!(status.success(), "fake harness exited {status:?}");
                return;
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("fake harness did not exit within {timeout:?}");
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => panic!("waiting on fake harness: {error}"),
        }
    }
}

fn drain(adapter: &mut dyn FileSignalAdapter, session: &str, who: &str) -> Vec<Observation> {
    let ctx = StampCtx {
        instance_id: InstanceId::new(),
        host_id: HostId::new(),
        journal_id: Id::new("obj").unwrap(),
        run_id: RunId::new(),
        session_id: session.to_owned(),
    };
    let mut out = Vec::new();
    let mut seq = 0u64;
    for _ in 0..20 {
        let observed = adapter.poll().expect("adapter poll");
        for o in observed {
            seq += 1;
            if let Some(observation) = stamp(&ctx, seq, &o) {
                out.push(observation);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !out.is_empty(),
        "{who} adapter produced no observations from the fake scenario"
    );
    out
}

#[derive(Debug, Clone, PartialEq)]
enum Fact {
    Working,
    Idle,
    UserMessage,
    AssistantMessage,
    ToolCall,
    ToolResult,
    Usage,
}

fn fact(o: &Observation) -> Option<Fact> {
    match &o.body {
        ObservationPayload::Lifecycle(lifecycle) => {
            let native = match lifecycle.as_ref() {
                remuda_protocol::LifecyclePayload::Native(n) => n,
                _ => return None,
            };
            if native.topic != remuda_protocol::LifecycleTopic::Turn {
                return None;
            }
            match native.native_name.as_str() {
                "task_started" | "turn_started" => Some(Fact::Working),
                "task_complete" | "turn_aborted" | "turn_ended" => Some(Fact::Idle),
                _ => None,
            }
        }
        ObservationPayload::Message(message) => match message.role {
            remuda_protocol::MessageRole::User => Some(Fact::UserMessage),
            remuda_protocol::MessageRole::Assistant => Some(Fact::AssistantMessage),
            remuda_protocol::MessageRole::System => None,
        },
        ObservationPayload::ToolCall(_) => Some(Fact::ToolCall),
        ObservationPayload::ToolResult(_) => Some(Fact::ToolResult),
        ObservationPayload::Usage(_) => Some(Fact::Usage),
        _ => None,
    }
}

/// Collapse streaming repeats (grok emits one Message per chunk) to the single
/// fact each represents, and keep just one Usage snapshot.
fn collapsed_facts(observations: &[Observation]) -> Vec<Fact> {
    let mut out: Vec<Fact> = Vec::new();
    for observation in observations {
        let Some(fact) = fact(observation) else {
            continue;
        };
        // Adjacent streaming message facts coalesce; tool call/result and
        // lifecycle boundaries never do.
        if (fact == Fact::AssistantMessage || fact == Fact::UserMessage)
            && out.last() == Some(&fact)
        {
            continue;
        }
        out.push(fact);
    }
    // Only the first usage snapshot matters for fact parity (snapshot replaces).
    if let Some(pos) = out.iter().position(|f| *f == Fact::Usage) {
        out.retain(|f| *f != Fact::Usage);
        out.insert(pos, Fact::Usage);
    }
    out
}

fn assert_parity_facts(observations: &[Observation], kind: AgentKind) {
    // The shared scenario must surface the same *facts* from both adapters.
    //
    // We assert the multiset rather than a strict order: in a post-exit bulk
    // drain, grok reads all of updates.jsonl before events.jsonl, so its
    // `turn_started` (Working) can land after the first content frame even
    // though they interleave correctly during a live turn (the supervisor
    // polls both tails at 250 ms while the process is running). The Node fold
    // is a state machine that treats Working idempotently, so that reorder is
    // not a behavioral difference. Within-order guarantees that do matter
    // (user→tool→assistant, call before result, one idle, one usage) are
    // checked separately.
    let actual = collapsed_facts(observations);
    let mut sorted_actual: Vec<Fact> = actual.clone();
    sorted_actual.sort_by_key(fact_rank);
    let mut expected = vec![
        Fact::Working,
        Fact::UserMessage,
        Fact::ToolCall,
        Fact::ToolResult,
        Fact::AssistantMessage,
        Fact::Idle,
        Fact::Usage,
    ];
    expected.sort_by_key(fact_rank);
    assert_eq!(
        sorted_actual, expected,
        "{kind:?} adapter fact set diverges: {actual:?}"
    );

    // One boundary each; the tool call precedes its result; the assistant
    // answer follows the tool result.
    assert_eq!(actual.iter().filter(|f| **f == Fact::Working).count(), 1);
    assert_eq!(actual.iter().filter(|f| **f == Fact::Idle).count(), 1);
    assert_eq!(actual.iter().filter(|f| **f == Fact::Usage).count(), 1);
    let pos = |fact: &Fact| actual.iter().position(|f| f == fact).unwrap();
    assert!(pos(&Fact::UserMessage) < pos(&Fact::AssistantMessage));
    assert!(pos(&Fact::ToolCall) < pos(&Fact::ToolResult));
    assert!(pos(&Fact::ToolResult) < pos(&Fact::AssistantMessage));
}

fn fact_rank(fact: &Fact) -> u8 {
    match fact {
        Fact::Working => 0,
        Fact::UserMessage => 1,
        Fact::ToolCall => 2,
        Fact::ToolResult => 3,
        Fact::AssistantMessage => 4,
        Fact::Usage => 5,
        Fact::Idle => 6,
    }
}

fn write_dump(path: &Path, observations: &[Observation]) {
    let events = serde_json::json!({ "observations": observations });
    std::fs::write(path, serde_json::to_vec_pretty(&events).unwrap()).unwrap();
}

struct DiffOutput {
    status: std::process::ExitStatus,
    stderr: String,
}

fn diff(left: &Path, right: &Path) -> DiffOutput {
    let binary = remuda_testing::ensure_workspace_bin("remuda");
    let output = std::process::Command::new(binary)
        .arg("journal")
        .arg("diff")
        .arg(left)
        .arg(right)
        .output()
        .expect("run remuda journal diff");
    DiffOutput {
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}
