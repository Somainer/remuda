//! Node integration for the Workflow producer.
//!
//! Drives the **real** [`WorkflowProducer`] — the same fold
//! `spawn_observation_pump` runs — with:
//! - real `SignalBus` hook folds (recorded 2.1.221 payload shapes, like
//!   `live_latency.rs`'s in-process harness) committed through the real
//!   `MemoryStore` append path,
//! - a synthetic run directory built from the spike's *real* agent
//!   transcripts (`remuda-journal/tests/fixtures/workflow`),
//! - asserted observation sequence and the launch/member latency budgets.
//!
//! This mirrors the pump exactly: each committed hook observation goes to
//! `producer.on_observation(..)` synchronously, and file growth is drained by
//! `producer.poll()` on the 250 ms cadence.

use remuda_node::workflow_producer::WorkflowProducer;
use remuda_node::{LocalStore, MemoryStore};
use remuda_protocol::{
    Activity, AgentKind, ClaudeRef, Connectivity, DriverKind, EntityMeta, HostId, Instance,
    InstanceId, InstanceLifecycle, InstanceMode, Knowledge, LaunchedBy, NativeRef, Observation,
    ObservationPayload, Ownership, ProcessRef, WorkflowState, WorkspaceId,
};
use remuda_signal::{HookEnvelope, SignalBus};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;
use tokio::sync::mpsc;

/// Budgets from the brief.
const FIRST_RUN_BUDGET_MS: u128 = 250;
const MEMBER_START_BUDGET_MS: u128 = 250;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../remuda-journal/tests/fixtures/workflow")
}

/// Minimal instance row for store appends; mirrors the node's test fixture.
#[allow(clippy::too_many_lines)]
fn fixture_instance(
    instance_id: InstanceId,
    host_id: HostId,
    workspace_id: WorkspaceId,
    driver: DriverKind,
) -> Instance {
    let now = remuda_protocol::Timestamp::try_from("2026-09-15T00:00:00.000Z".to_string()).unwrap();
    Instance {
        meta: EntityMeta {
            id: instance_id,
            revision: remuda_protocol::U64(1),
            created_at: now.clone(),
            updated_at: now,
        },
        host_id: host_id.clone(),
        workspace_id,
        kind: remuda_protocol::AgentKind::Claude,
        driver,
        lifecycle: InstanceLifecycle::Ready,
        activity: Knowledge::Known {
            value: Activity::Idle,
        },
        activity_evidence_event_ids: Vec::new(),
        connectivity: Connectivity::Connected,
        ownership: Ownership::Managed,
        native_ref: NativeRef {
            host_id,
            native_store_id: remuda_protocol::Id::new("obj").unwrap(),
            kind: AgentKind::Claude,
            session_id: Knowledge::Known {
                value: "0199a1f0-0000-7000-8000-aaaaaaaaaaaa".into(),
            },
            transcript: Knowledge::NotApplicable,
            signal_tier: None,
            capabilities: Vec::new(),
            codex: None,
            acp: None,
            claude: Some(ClaudeRef {
                session_id: "0199a1f0-0000-7000-8000-aaaaaaaaaaaa".into(),
            }),
            claude_bg: None,
            agy: None,
            herdr: None,
        },
        process_ref: ProcessRef {
            process_generation: remuda_protocol::U64(1),
            process_identity: Knowledge::NotApplicable,
            connection_epoch: remuda_protocol::Id::new("epoch").unwrap(),
        },
        spec_revision: remuda_protocol::U64(1),
        launch_id: Knowledge::Known {
            value: remuda_protocol::Id::new("launch").unwrap(),
        },
        capabilities: remuda_node::driver_capability_snapshot(driver),
        owner_fence: remuda_protocol::U64(1),
        active_run_ids: Vec::new(),
        parent: None,
        journal_id: remuda_protocol::Id::new("obj").unwrap(),
        durable_seq: remuda_protocol::U64(0),
        exit: Knowledge::NotApplicable,
        last_error: None,
        mode: Some(InstanceMode::Native),
        promoted_at: None,
        launched_by: Some(LaunchedBy::Remuda),
    }
}

struct Harness {
    /// Owns the temp tree for the test's lifetime.
    #[allow(dead_code)]
    tmp: tempfile::TempDir,
    session_id: String,
    session_dir: PathBuf,
    transcript: PathBuf,
    run_dir: PathBuf,
    instance_id: InstanceId,
    bus: Arc<SignalBus>,
    rx: tokio::sync::Mutex<mpsc::Receiver<Observation>>,
    store: Arc<MemoryStore>,
    producer: WorkflowProducer,
}

impl Harness {
    fn new() -> Self {
        let tmp = tempfile::TempDir::new().unwrap();
        let session_id = "0199a1f0-0000-7000-8000-aaaaaaaaaaaa".to_owned();
        let project = tmp.path().join("home/.claude/projects/-tmp-wfprod-it");
        let session_dir = project.join(&session_id);
        let run_dir = session_dir.join("subagents/workflows/wf_it-1");
        fs::create_dir_all(&run_dir).unwrap();
        let transcript = project.join(format!("{session_id}.jsonl"));
        fs::write(&transcript, b"{\"type\":\"user\"}\n").unwrap();
        fs::create_dir_all(session_dir.join("workflows/scripts")).unwrap();
        fs::copy(
            fixtures().join("scripts/spike-wf-1.js"),
            session_dir.join("workflows/scripts/it-wf_it-1.js"),
        )
        .unwrap();

        let instance_id = InstanceId::new();
        let (tx, rx) = mpsc::channel(256);
        // The bus identity comes from the driver's own `promote_ctx`, exactly
        // as `start_hooks_blocking` builds it in production — not from a
        // hand-written `BusContext` that restates the id the producer uses.
        // That is what makes the `toolCallId` assertion below meaningful:
        // before c-wfdrill2 C the driver minted `InstanceId::new()` here, so
        // the hook-derived tool node and the run's `toolCallId` were derived
        // under different scopes and could never match.
        let mut options =
            remuda_driver::shell_pty::ShellPtyOptions::login(tmp.path().to_path_buf());
        options.instance_id = Some(instance_id.clone());
        let bus = Arc::new(SignalBus::new(
            remuda_driver::shell_pty::hook_bus_context(
                &options,
                &tmp.path().to_string_lossy(),
                None,
            )
            .expect("hook bus context"),
            tx,
            Arc::new(AtomicU64::new(0)),
        ));
        let store = Arc::new(MemoryStore::open_journaled(tmp.path().join("node"), 256).unwrap());
        let instance = fixture_instance(
            instance_id.clone(),
            HostId::new(),
            WorkspaceId::new(),
            DriverKind::ShellPty,
        );
        store.insert_instance(instance).unwrap();
        Self {
            tmp,
            session_id,
            session_dir,
            transcript,
            run_dir,
            instance_id: instance_id.clone(),
            bus,
            rx: tokio::sync::Mutex::new(rx),
            store,
            producer: WorkflowProducer::new(instance_id, None),
        }
    }

    /// Fold a hook through the real bus, commit every emitted observation
    /// through the store (the pump's append path), and drive the workflow
    /// producer synchronously on the committed observation as the pump does.
    async fn hook(&mut self, event: &str, payload: serde_json::Value) -> Vec<Observation> {
        self.bus
            .handle(HookEnvelope {
                credential: "fixture".into(),
                event: event.into(),
                ppid: 4242,
                payload,
            })
            .await;
        tokio::task::yield_now().await;
        let mut committed_all = Vec::new();
        let mut derived_all = Vec::new();
        let mut rx = self.rx.lock().await;
        while let Ok(observation) = rx.try_recv() {
            let committed = self
                .store
                .append_driver_observation(&self.instance_id, observation)
                .unwrap();
            // Exactly the pump seam: the workflow fold on each committed hook.
            derived_all.extend(self.producer.on_observation(&committed));
            committed_all.push(committed);
        }
        for derived in derived_all {
            let committed = self
                .store
                .append_driver_observation(&self.instance_id, derived)
                .unwrap();
            committed_all.push(committed);
        }
        committed_all
    }

    /// One 250 ms poll tick: drain file growth and commit derived obs.
    fn poll(&mut self) -> Vec<Observation> {
        self.producer
            .poll()
            .into_iter()
            .map(|observation| {
                self.store
                    .append_driver_observation(&self.instance_id, observation)
                    .unwrap()
            })
            .collect()
    }
}

fn runs(obs: &[Observation]) -> Vec<&remuda_protocol::WorkflowRunPayload> {
    obs.iter()
        .filter_map(|obs| match &obs.body {
            ObservationPayload::WorkflowRun(run) => Some(run.as_ref()),
            _ => None,
        })
        .collect()
}

/// The node id of the `tool_name` tool call, as the hook fold minted it.
fn tool_call_id(obs: &[Observation], tool_name: &str) -> Option<remuda_protocol::Id> {
    obs.iter().find_map(|obs| match &obs.body {
        ObservationPayload::ToolCall(call)
            if matches!(&call.tool_name, Knowledge::Known { value } if value == tool_name) =>
        {
            Some(call.tool_call_id.clone())
        }
        _ => None,
    })
}

fn members(obs: &[Observation]) -> Vec<&remuda_protocol::WorkflowMemberPayload> {
    obs.iter()
        .filter_map(|obs| match &obs.body {
            ObservationPayload::WorkflowMember(member) => Some(member.as_ref()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launch_member_terminal_sequence_and_budgets() {
    let mut h = Harness::new();
    let agent_id = "aae139d44933cefe2";

    // SessionStart: binds the session directory and the main transcript tail.
    h.hook(
        "SessionStart",
        serde_json::json!({
            "session_id": h.session_id,
            "transcript_path": h.transcript.to_string_lossy(),
        }),
    )
    .await;

    // The harness writes the first agent transcript record and the journal
    // before SubagentStart (verified on 2.1.221).
    fs::copy(
        fixtures().join("runs-221/wf_c3422384-cb1/agent-aae139d44933cefe2.jsonl"),
        h.run_dir.join(format!("agent-{agent_id}.jsonl")),
    )
    .unwrap();
    fs::write(
        h.run_dir.join("journal.jsonl"),
        format!("{{\"type\":\"started\",\"key\":\"v2:one\",\"agentId\":\"{agent_id}\"}}\n"),
    )
    .unwrap();

    // The launch's own PreToolUse opens the Workflow tool row (claude fires it
    // before the call runs); the run snapshot below must name exactly that row.
    let opened = h
        .hook(
            "PreToolUse",
            serde_json::json!({
                "tool_name": "Workflow",
                "tool_use_id": "toolu_vrtx_01WF",
                "tool_input": { "script": "export const meta = {}" },
            }),
        )
        .await;
    let launch_call =
        tool_call_id(&opened, "Workflow").expect("PreToolUse opens the Workflow tool row");
    assert_eq!(
        Some(launch_call.clone()),
        remuda_signal::tool_node_id(h.instance_id.as_id().as_str(), "toolu_vrtx_01WF"),
        "the hook fold derives the tool node under the Node's instance scope"
    );

    // PostToolUse(Workflow): the first workflow.run must be emitted
    // synchronously inside the budget.
    let started = Instant::now();
    let committed = h
        .hook(
            "PostToolUse",
            serde_json::json!({
                "tool_name": "Workflow",
                "tool_use_id": "toolu_vrtx_01WF",
                "tool_input": { "script": "export const meta = {}" },
                "tool_response": {
                    "status": "async_launched",
                    "taskId": "wn-it",
                    "taskType": "local_workflow",
                    "runId": "wf_it-1",
                    "transcriptDir": h.run_dir.to_string_lossy(),
                    "scriptPath": h
                        .session_dir
                        .join("workflows/scripts/it-wf_it-1.js")
                        .to_string_lossy(),
                }
            }),
        )
        .await;
    let run_snapshots = runs(&committed);
    assert_eq!(run_snapshots.len(), 1, "exactly one run snapshot on launch");
    assert_eq!(run_snapshots[0].state, WorkflowState::Running);
    // c-wfdrill2 C: the run must name the tool call row the SAME hook fold
    // produced for `toolu_vrtx_01WF`. The web assembler mounts the workflow
    // card on that row by id; two ids derived under different scopes leave
    // every member and subagent row outside the Workflow row forever.
    assert_eq!(
        run_snapshots[0].tool_call_id.as_ref(),
        Some(&launch_call),
        "workflow.run.toolCallId must be the tool call node the hook fold minted"
    );
    assert!(run_snapshots[0].totals.as_ref().unwrap().total_known);
    assert_eq!(run_snapshots[0].totals.as_ref().unwrap().agents_total.0, 4);
    let elapsed = started.elapsed().as_nanos() / 1_000_000;
    assert!(
        elapsed <= FIRST_RUN_BUDGET_MS,
        "first workflow.run took {elapsed} ms; budget {FIRST_RUN_BUDGET_MS}"
    );

    // SubagentStart: member start emitted synchronously, enriched from the
    // real transcript and joined to the script's call site.
    let member_started = Instant::now();
    let committed = h
        .hook(
            "SubagentStart",
            serde_json::json!({
                "agent_id": agent_id,
                "agent_type": "workflow-subagent",
                "session_id": h.session_id,
                "transcript_path": h.transcript.to_string_lossy(),
            }),
        )
        .await;
    let member_starts = members(&committed);
    assert_eq!(member_starts.len(), 1, "one member start observation");
    assert_eq!(member_starts[0].state, WorkflowState::Running);
    let label = match &member_starts[0].label {
        remuda_protocol::Knowledge::Known { value } => value.clone(),
        other => panic!("expected joined label, got {other:?}"),
    };
    assert_eq!(label, "alpha:one", "label joined from the script");
    assert!(member_starts[0].phase_id.is_some());
    assert!(matches!(
        member_starts[0].model_resolved,
        remuda_protocol::Knowledge::Known { .. }
    ));
    assert!(
        member_started.elapsed().as_nanos() / 1_000_000 <= MEMBER_START_BUDGET_MS,
        "member start exceeded the {MEMBER_START_BUDGET_MS} ms budget"
    );

    // Journal growth drains on the poll tick: result completes the member.
    fs::OpenOptions::new()
        .append(true)
        .open(h.run_dir.join("journal.jsonl"))
        .unwrap()
        .write_all(
            format!(
                "{{\"type\":\"result\",\"key\":\"v2:one\",\"agentId\":\"{agent_id}\",\"result\":\"done\"}}\n"
            )
            .as_bytes(),
        )
        .unwrap();
    let committed = h.poll();
    members(&committed)
        .iter()
        .find(|member| member.state == WorkflowState::Completed)
        .expect("member completes from journal result");

    // Terminal marker in the main transcript: completed run + summary.
    let mut main = fs::OpenOptions::new()
        .append(true)
        .open(&h.transcript)
        .unwrap();
    writeln!(
        main,
        "{}",
        serde_json::json!({
            "type": "user",
            "message": { "content": "<task-notification>\n<task-id>wn-it</task-id>\n<tool-use-id>toolu_vrtx_01WF</tool-use-id>\n<status>completed</status>\n<summary>Dynamic workflow \"it\" completed</summary>\n</task-notification>" }
        })
    )
    .unwrap();
    drop(main);
    let committed = h.poll();
    runs(&committed)
        .iter()
        .find(|run| run.state == WorkflowState::Completed)
        .expect("completed run from task-notification");

    // Idle polls emit nothing — no polling chatter.
    assert!(h.poll().is_empty());
    assert!(h.poll().is_empty());

    // Sequence is committed to the real journal.
    let journal_id = h.store.get_instance(&h.instance_id).unwrap().journal_id;
    let page = h.store.read_events(&journal_id, None, 256).unwrap();
    let kinds: Vec<_> = page
        .events
        .iter()
        .filter_map(|event| match event {
            remuda_protocol::JournalEvent::Instance(obs) => Some(obs.body.kind()),
            _ => None,
        })
        .collect();
    assert!(kinds.contains(&remuda_protocol::ObservationKind::WorkflowRun));
    assert!(kinds.contains(&remuda_protocol::ObservationKind::WorkflowMember));
    assert!(kinds.contains(&remuda_protocol::ObservationKind::WorkflowPhase));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_run_marks_live_members_killed() {
    let mut h = Harness::new();
    let agent_id = "aae139d44933cefe2";
    h.hook(
        "SessionStart",
        serde_json::json!({
            "session_id": h.session_id,
            "transcript_path": h.transcript.to_string_lossy(),
        }),
    )
    .await;
    fs::copy(
        fixtures().join("runs-221/wf_c3422384-cb1/agent-aae139d44933cefe2.jsonl"),
        h.run_dir.join(format!("agent-{agent_id}.jsonl")),
    )
    .unwrap();
    fs::write(
        h.run_dir.join("journal.jsonl"),
        format!("{{\"type\":\"started\",\"key\":\"v2:one\",\"agentId\":\"{agent_id}\"}}\n"),
    )
    .unwrap();
    h.hook(
        "PostToolUse",
        serde_json::json!({
            "tool_name": "Workflow",
            "tool_use_id": "toolu_stop",
            "tool_response": {
                "taskId": "wn-kill",
                "runId": "wf_it-1",
                "transcriptDir": h.run_dir.to_string_lossy(),
                "scriptPath": h
                    .session_dir
                    .join("workflows/scripts/it-wf_it-1.js")
                    .to_string_lossy(),
            }
        }),
    )
    .await;
    h.hook(
        "SubagentStart",
        serde_json::json!({
            "agent_id": agent_id,
            "agent_type": "workflow-subagent",
            "session_id": h.session_id,
        }),
    )
    .await;

    // TaskStop terminal decision fed by the hook fold (TaskStop tool_result).
    let committed = h
        .producer
        .terminate("wn-kill", WorkflowState::Cancelled)
        .into_iter()
        .map(|obs| {
            h.store
                .append_driver_observation(&h.instance_id, obs)
                .unwrap()
        })
        .collect::<Vec<_>>();
    let cancelled = committed;
    let cancelled_runs = runs(&cancelled);
    let run = cancelled_runs
        .iter()
        .find(|run| run.state == WorkflowState::Cancelled)
        .expect("cancelled run");
    assert!(run.totals.as_ref().unwrap().agents_killed.0 >= 1);
    let committed = h.poll();
    assert!(
        members(&committed)
            .iter()
            .any(|member| member.state == WorkflowState::Cancelled)
    );
}
