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
use remuda_protocol::{
    AgentKind, HostId, Id, InstanceId, Observation, ObservationPayload, RunId, ToolCallState,
    ToolCategory,
};

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

    // The grok tool card is a *named, categorized* card with a real lifecycle,
    // which is the whole point of the fake writing real frames: before this,
    // the fake's forced shell shape made every grok call `Unknown` / `Other`.
    assert_grok_named_tool_card(&grok);

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

/// The grok call the fake scripts (`Bash` → `run_terminal_command`) must arrive
/// through the real adapter as a card with the `_meta["x.ai/tool"].name`
/// identity, the human `title` as display, a resolved category, and a
/// Proposed → Running → Final revision chain on one node.
///
/// The whole chain is asserted through `GrokAdapter`, never against the fake's
/// own JSON: a fake that writes a plausible frame the adapter cannot read is
/// exactly the failure this test exists to catch.
fn assert_grok_named_tool_card(observations: &[Observation]) {
    let calls: Vec<&remuda_protocol::ToolCallPayload> = observations
        .iter()
        .filter_map(|o| match &o.body {
            ObservationPayload::ToolCall(payload) => Some(payload.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls.len(),
        2,
        "grok emits one Proposed and one Running call mutation: {calls:?}"
    );

    let proposed = calls[0];
    // Identity comes from `_meta["x.ai/tool"].name` — the scripted `Bash` is
    // only the scenario's spelling; the frame carries the grok name.
    assert_eq!(
        knowledge(&proposed.tool_name),
        Some("run_terminal_command".to_owned()),
        "stable name from x.ai/tool"
    );
    assert_eq!(
        knowledge(&proposed.display_title),
        Some("run_terminal_command".to_owned()),
        "the Pending frame's title is still the bare name"
    );
    assert_eq!(proposed.category, ToolCategory::Shell, "name table → Shell");
    assert_eq!(proposed.state, ToolCallState::Proposed);

    let running = calls[1];
    assert_eq!(running.tool_call_id, proposed.tool_call_id, "same node");
    assert_eq!(running.state, ToolCallState::Running);
    assert_eq!(
        knowledge(&running.display_title),
        Some("Execute `printf PARITY_OK`".to_owned()),
        "the progress frame carries the human sentence"
    );
    assert_eq!(
        running.mutation.revision.0, 2,
        "Running replaces the proposal"
    );
    assert_eq!(
        Some(running.mutation.base_revision.expect("base").0),
        Some(1),
        "…and builds on revision 1"
    );

    // The result closes strictly above the last call revision, or the web
    // timeline drops it (`assemble.ts` `newerMutation`).
    let result = observations
        .iter()
        .find_map(|o| match &o.body {
            ObservationPayload::ToolResult(payload) => Some(payload.as_ref()),
            _ => None,
        })
        .expect("grok tool result");
    assert_eq!(result.stage, remuda_protocol::ResultStage::Final);
    assert_eq!(result.mutation.node_id, proposed.tool_call_id);
    assert!(result.mutation.revision.0 > running.mutation.revision.0);
    assert_eq!(
        Some(result.mutation.base_revision.expect("base").0),
        Some(running.mutation.revision.0)
    );
}

fn knowledge<T: Clone>(value: &remuda_protocol::Knowledge<T>) -> Option<T> {
    match value {
        remuda_protocol::Knowledge::Known { value } => Some(value.clone()),
        _ => None,
    }
}

/// Read a bundled fake-harness scenario by file name.
///
/// The scenario lives once, under `remuda-testing/fixtures/fake-harness/
/// scenarios/`, where the doc's scenario list points at it; this test drives
/// the same bytes rather than re-inlining a copy that could drift from the file
/// (and from the `bundled_grok_scenarios_produce_the_expected_frames` unit test
/// that also parses it).
fn scenario_fixture(name: &str) -> String {
    let path = remuda_testing::fixtures_dir()
        .join("fake-harness/scenarios")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

/// The question frame must survive the whole pipe: the adapter sees the stable
/// name from `_meta["x.ai/tool"]`, a Running mutation from the statusless
/// progress frame, and the scripted answer in the result. This is the live
/// source the `turn.live` / interaction projection (tasks 2 and 6) reads, so a
/// silent shape change here would break them one task later.
#[test]
fn fake_grok_question_frames_reach_the_adapter() {
    let run = run_and_adapt_scenario(
        AgentKind::Grok,
        &scenario_fixture("grok-question.json"),
        "QUESTION",
    );
    let calls: Vec<&remuda_protocol::ToolCallPayload> = run
        .observations
        .iter()
        .filter_map(|o| match &o.body {
            ObservationPayload::ToolCall(payload) => Some(payload.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2, "one Pending and one Running mutation");
    assert_eq!(
        knowledge(&calls[0].tool_name),
        Some("ask_user_question".to_owned()),
        "identity from _meta[\"x.ai/tool\"].name"
    );
    assert_eq!(
        calls[0].category,
        ToolCategory::Other,
        "ask_user_question is not in the name table"
    );
    assert_eq!(calls[1].state, ToolCallState::Running);
    assert_eq!(calls[1].tool_call_id, calls[0].tool_call_id, "same node");
    assert!(calls[1].mutation.revision.0 > calls[0].mutation.revision.0);

    // The scripted answer survives into the result's structured output.
    let result = run
        .observations
        .iter()
        .find_map(|o| match &o.body {
            ObservationPayload::ToolResult(payload) => Some(payload.as_ref()),
            _ => None,
        })
        .expect("tool result");
    let message = match &result.structured_result {
        remuda_protocol::Knowledge::Known { value } => value
            .pointer("/rawOutput/UserAnswered/message")
            .and_then(|value| value.as_str())
            .map(str::to_owned),
        _ => None,
    }
    .expect("UserAnswered message");
    assert!(
        message.contains("\"Choose the probe result.\"=\"Alpha\""),
        "{message}"
    );

    // The statusless progress frame is what makes the card Running; without it
    // the translator never sees a Running state at all.
    let session_dir = run.session_dir.as_ref().expect("grok session dir");
    let updates = read_jsonl(&session_dir.join("updates.jsonl"));
    let progress = updates
        .iter()
        .find(|frame| {
            frame["params"]["update"]["sessionUpdate"] == "tool_call_update"
                && frame["params"]["update"]
                    .get("status")
                    .is_none_or(serde_json::Value::is_null)
        })
        .expect("a statusless progress frame");
    assert_eq!(
        progress["params"]["update"]["_meta"]["x.ai/tool"]["name"],
        "ask_user_question"
    );
    assert_eq!(
        progress["params"]["update"]["rawInput"]["variant"],
        "AskUserQuestion"
    );
}

/// The shell + write turn: the terminal log grows inside the session directory
/// during the run, the completed frame points `output_file` at it, the cards
/// carry real categories, and a scripted diff arrives as a `FileChange`.
#[test]
fn fake_grok_shell_and_write_frames_reach_the_adapter() {
    let run = run_and_adapt_scenario(
        AgentKind::Grok,
        &scenario_fixture("grok-tools.json"),
        "GROK_TOOLS",
    );
    let session_dir = run.session_dir.as_ref().expect("grok session dir");

    // The log lives inside the session tree, not at a capture machine's path.
    let log = session_dir.join("terminal").join("toolu-fake-001-0.log");
    assert!(
        log.is_file(),
        "terminal log written: {}",
        session_dir.display()
    );
    let body = std::fs::read_to_string(&log).unwrap();
    assert!(
        body.starts_with("$ printf 'one\\ntwo\\nthree\\n'"),
        "log opens with the command line: {body:?}"
    );
    assert!(
        body.contains("three"),
        "the command's output landed: {body:?}"
    );
    assert!(
        log.starts_with(&run.home),
        "log path is session-local: {}",
        log.display()
    );

    // The completed shell frame points at that log (the live output channel).
    let updates = read_jsonl(&session_dir.join("updates.jsonl"));
    let output_file = updates
        .iter()
        .filter_map(|frame| frame["params"]["update"]["rawOutput"]["output_file"].as_str())
        .next()
        .expect("rawOutput.output_file on a completed shell frame");
    assert_eq!(output_file, log.to_string_lossy());

    // Two named, categorized cards — not Unknown / Other.
    let calls: Vec<&remuda_protocol::ToolCallPayload> = run
        .observations
        .iter()
        .filter_map(|o| match &o.body {
            ObservationPayload::ToolCall(payload) if payload.state != ToolCallState::Running => {
                Some(payload.as_ref())
            }
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2, "one Proposed card per call: {calls:?}");
    assert_eq!(
        knowledge(&calls[0].tool_name),
        Some("run_terminal_command".to_owned())
    );
    assert_eq!(calls[0].category, ToolCategory::Shell);
    assert_eq!(knowledge(&calls[1].tool_name), Some("write".to_owned()));
    assert_eq!(calls[1].category, ToolCategory::FileWrite);

    // The scripted diff reached the result as a FileChange.
    // The scripted diff reaches the result as a `FileChange`. The `path` is the
    // field the adapter reads directly; the `diff` text is **not** — the
    // adapter's `diff_text` looks for a `diff`/`patch` string and, finding
    // neither, serializes the whole block. That is the honest current
    // behaviour: the real capture's diff carries `oldText`/`newText`, which the
    // adapter does not yet understand (a follow-up on the translator side, not
    // a fake-harness gap — this file may not edit `grok_adapter.rs`).
    //
    // Asserted explicitly rather than via a loose `contains("OK")`, so the day
    // the adapter learns `oldText`/`newText` this test fails loudly and gets
    // tightened instead of silently passing for a different reason.
    let changes: Vec<_> = run
        .observations
        .iter()
        .filter_map(|o| match &o.body {
            ObservationPayload::ToolResult(payload) => Some(payload.changes.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(changes.len(), 1, "one scripted diff: {changes:?}");
    assert_eq!(
        changes[0].path, "/work/grok-out.txt",
        "the diff's path is the field the adapter reads"
    );
    let serialized: serde_json::Value = serde_json::from_str(&changes[0].diff).expect(
        "the diff text is the adapter's serialized fallback for an unrecognized block shape",
    );
    assert_eq!(
        serialized["newText"], "OK",
        "the fake wrote the captured oldText/newText shape, which the adapter \
         serialized wholesale: {:?}",
        changes[0].diff
    );
}

/// A shell call followed by a non-shell call in one turn: the terminal log
/// belongs to the **shell** call only.
///
/// Regression for the log handle outliving its tool. It used to be set for the
/// shell call and never cleared, so the following `read_file` inherited the
/// handle, created a `terminal/<read-call>.log` no frame referenced, and had
/// its result lines written one per tick into the previous call's counter.
/// The observable contract is asserted here through the artifacts and the
/// adapter, not through the engine's private state.
#[test]
fn fake_grok_non_shell_call_after_shell_has_no_terminal_log() {
    let run = run_and_adapt_scenario(
        AgentKind::Grok,
        &scenario_fixture("grok-ordered-tools.json"),
        "GROK_ORDERED",
    );
    let session_dir = run.session_dir.as_ref().expect("grok session dir");
    let terminal = session_dir.join("terminal");

    // Exactly one log, owned by the shell call: the read must not have made one.
    let mut logs: Vec<String> = std::fs::read_dir(&terminal)
        .expect("terminal dir exists")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    logs.sort();
    assert_eq!(
        logs,
        vec!["toolu-fake-001-0.log".to_owned()],
        "only the shell call gets a terminal log"
    );

    // The shell log holds the whole multi-line result **and nothing else**.
    // Asserting the exact body is what catches a stale handle: with the bug,
    // the following read's result text is appended to this log (its lines are
    // released through the previous call's counter), while `alpha`/`bravo`
    // still appear early enough for a `contains` check to pass.
    let body = std::fs::read_to_string(terminal.join("toolu-fake-001-0.log")).unwrap();
    assert_eq!(
        body, "$ printf 'alpha\\nbravo\\n'\nalpha\nbravo\n",
        "the shell log is exactly its own command and output"
    );

    // Only the shell completion frame points at a log; the read's does not.
    let updates = read_jsonl(&session_dir.join("updates.jsonl"));
    let output_files: Vec<&str> = updates
        .iter()
        .filter_map(|frame| frame["params"]["update"]["rawOutput"]["output_file"].as_str())
        .collect();
    assert_eq!(output_files.len(), 1, "one output_file: {output_files:?}");
    assert!(output_files[0].ends_with("toolu-fake-001-0.log"));

    // Both calls still produce a named card with the right category.
    let named: Vec<(String, ToolCategory)> = run
        .observations
        .iter()
        .filter_map(|o| match &o.body {
            ObservationPayload::ToolCall(payload) if payload.state != ToolCallState::Running => {
                knowledge(&payload.tool_name).map(|name| (name, payload.category))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        named,
        vec![
            ("run_terminal_command".to_owned(), ToolCategory::Shell),
            ("read_file".to_owned(), ToolCategory::FileRead),
        ],
        "both calls keep their identity and category"
    );
}

fn read_jsonl(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("valid json"))
        .collect()
}

/// Spawn the fake harness in a PTY, submit the turn, wait for exit, and adapt.
fn run_and_adapt(kind: AgentKind) -> Vec<Observation> {
    run_and_adapt_scenario(kind, SCENARIO, "RUN_PARITY").observations
}

/// One fake-harness run and everything a test needs to inspect it afterwards.
struct ParityRun {
    observations: Vec<Observation>,
    /// Harness home (holds the artifact tree).
    home: std::path::PathBuf,
    /// Grok session directory, for grok runs only — the `terminal/` logs and
    /// `updates.jsonl` live here.
    session_dir: Option<std::path::PathBuf>,
    /// Keeps the run's temp tree alive for as long as the test holds this: the
    /// adapters read session files lazily, so dropping the root early would
    /// pull the artifacts out from under an assertion.
    _root: tempfile::TempDir,
}

/// Spawn the fake harness in a PTY with `scenario`, submit `prompt`, wait for
/// exit, and adapt the artifacts with the kind's production adapter.
///
/// A real PTY (not a mock) matters: the fake sets raw mode and its input
/// semantics are per-read, so a piped stdin would not exercise the same path.
fn run_and_adapt_scenario(kind: AgentKind, scenario: &str, prompt: &str) -> ParityRun {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let cwd = root.path().join("work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let script = root.path().join("scenario.json");
    std::fs::write(&script, scenario).unwrap();
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
    writer.write_all(prompt.as_bytes()).expect("write prompt");
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
            ParityRun {
                observations: drain(&mut adapter, &session, "codex"),
                home,
                session_dir: None,
                _root: root,
            }
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
            adapter.bind_session_dir(&session, dir.clone());
            ParityRun {
                observations: drain(&mut adapter, &session, "grok"),
                home,
                session_dir: Some(dir),
                _root: root,
            }
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

/// One collapsed fact, with the identity needed to collapse *mutations of the
/// same node* without hiding a genuinely repeated call.
#[derive(Debug, Clone, PartialEq)]
struct Collapsed {
    fact: Fact,
    /// Journal node the fact belongs to (`mutation.node_id`), when it has one.
    node: Option<remuda_protocol::Id>,
}

fn collapse(observation: &Observation, fact: Fact) -> Collapsed {
    let node = match &observation.body {
        ObservationPayload::ToolCall(payload) => Some(payload.mutation.node_id.clone()),
        ObservationPayload::ToolResult(payload) => Some(payload.mutation.node_id.clone()),
        ObservationPayload::Message(payload) => Some(payload.mutation.node_id.clone()),
        _ => None,
    };
    Collapsed { fact, node }
}

/// Collapse streaming repeats to the single fact each represents: grok emits
/// one `Message` per chunk, and — once the Running edge and the live stdout
/// channel land — one `ToolCall` per call mutation and one `ToolResult` per
/// partial. Collapsing is keyed on **node identity**, not just adjacency, so a
/// genuinely repeated call (a different node) still shows up as its own fact
/// rather than being absorbed into its predecessor.
///
/// Only the first Usage snapshot matters for fact parity (snapshot replaces).
fn collapsed_facts(observations: &[Observation]) -> Vec<Fact> {
    let mut out: Vec<Collapsed> = Vec::new();
    for observation in observations {
        let Some(fact) = fact(observation) else {
            continue;
        };
        let current = collapse(observation, fact.clone());
        let coalescable = matches!(
            current.fact,
            Fact::AssistantMessage | Fact::UserMessage | Fact::ToolCall | Fact::ToolResult
        );
        // Same fact, same node, adjacent → a mutation of one thing (streamed
        // chunk, Proposed→Running, Partial→Partial), not a new thing.
        if coalescable
            && let Some(last) = out.last()
            && last.fact == current.fact
            && last.node == current.node
        {
            continue;
        }
        out.push(current);
    }
    // Only the first usage snapshot matters for fact parity (snapshot replaces).
    let mut facts: Vec<Fact> = out.into_iter().map(|collapsed| collapsed.fact).collect();
    if let Some(pos) = facts.iter().position(|f| *f == Fact::Usage) {
        facts.retain(|f| *f != Fact::Usage);
        facts.insert(pos, Fact::Usage);
    }
    facts
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
