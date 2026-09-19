//! Node integration for the on-demand `subagent.transcript` RPC (c-wfdrill).
//!
//! Uses the same in-memory store + synthetic session-dir harness shape as
//! `workflow_producer.rs`, but drives the public RPC entry the Hub calls, so
//! the assertions cover instance/transcript resolution, the bounded
//! `remuda-journal` sidechain reader, and observation stamping end to end.

use remuda_node::{LocalStore, MemoryStore};
use remuda_protocol::{
    Activity, AgentKind, ClaudeRef, Connectivity, DriverKind, EntityMeta, HostId, Instance,
    InstanceId, InstanceLifecycle, InstanceMode, Knowledge, LaunchedBy, NativeRef, Ownership,
    ProcessRef, TranscriptRef, WorkspaceId,
};
use std::fs;
use std::path::PathBuf;

const RUN: &str = "wf_c3422384-cb1";
const AGENT: &str = "aae139d44933cefe2";

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../remuda-journal/tests/fixtures/workflow/runs-221")
        .join(RUN)
}

#[allow(clippy::too_many_lines)]
fn instance_with_transcript(instance_id: InstanceId, transcript_path: &str) -> Instance {
    let now = remuda_protocol::Timestamp::try_from("2026-09-15T00:00:00.000Z".to_string()).unwrap();
    Instance {
        meta: EntityMeta {
            id: instance_id,
            revision: remuda_protocol::U64(1),
            created_at: now.clone(),
            updated_at: now,
        },
        host_id: HostId::new(),
        workspace_id: WorkspaceId::new(),
        kind: AgentKind::Claude,
        driver: DriverKind::ShellPty,
        lifecycle: InstanceLifecycle::Ready,
        activity: Knowledge::Known {
            value: Activity::Idle,
        },
        activity_evidence_event_ids: Vec::new(),
        connectivity: Connectivity::Disconnected,
        ownership: Ownership::Managed,
        native_ref: NativeRef {
            host_id: HostId::new(),
            native_store_id: remuda_protocol::Id::new("obj").unwrap(),
            kind: AgentKind::Claude,
            session_id: Knowledge::Known {
                value: "sess".to_owned(),
            },
            transcript: Knowledge::Known {
                value: TranscriptRef {
                    object_id: remuda_protocol::Id::new("obj").unwrap(),
                    source_path: transcript_path.to_owned(),
                },
            },
            signal_tier: None,
            capabilities: Vec::new(),
            codex: None,
            acp: None,
            claude: Some(ClaudeRef {
                session_id: "sess".to_owned(),
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
        capabilities: remuda_node::driver_capability_snapshot(DriverKind::ShellPty),
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

#[test]
fn rpc_serves_the_recorded_sidechain_transcript() {
    let tmp = tempfile::tempdir().unwrap();
    let session_id = "sess_abc";
    let project = tmp.path().join("home/.claude/projects/-tmp-subagent-it");
    let session_dir = project.join(session_id);
    let run_dir = session_dir.join("subagents/workflows").join(RUN);
    fs::create_dir_all(&run_dir).unwrap();
    for entry in fs::read_dir(fixtures()).unwrap() {
        let entry = entry.unwrap();
        fs::copy(entry.path(), run_dir.join(entry.file_name())).unwrap();
    }
    let transcript = project.join(format!("{session_id}.jsonl"));
    fs::write(&transcript, b"{\"type\":\"user\"}\n").unwrap();

    let store = MemoryStore::open_journaled(tmp.path().join("node"), 256).unwrap();
    let instance_id = InstanceId::new();
    store
        .insert_instance(instance_with_transcript(
            instance_id.clone(),
            &transcript.to_string_lossy(),
        ))
        .unwrap();

    let result = remuda_node::subagent::read_for_store(&store, &instance_id, AGENT, None).unwrap();
    assert_eq!(result["available"], serde_json::json!(true));
    assert_eq!(result["meta"]["agentId"], serde_json::json!(AGENT));
    assert_eq!(result["meta"]["runId"], serde_json::json!(RUN));
    let events = result["events"].as_array().unwrap();
    assert!(
        events.iter().any(|e| e["kind"] == "tool_call"),
        "sidechain tool rows come back as observations"
    );
    assert!(
        events.iter().any(|e| e["kind"] == "message"),
        "prompt/final text come back as messages"
    );
    assert!(
        events
            .iter()
            .all(|e| e["source"]["channel"] == "transcript" && e["source"]["delivery"] == "replay")
    );

    // A registered-but-unstarted agent reads as 启动中, not an error.
    let starting =
        remuda_node::subagent::read_for_store(&store, &instance_id, "0000000000000000", None)
            .unwrap();
    assert_eq!(starting["available"], serde_json::json!(false));

    // Path-traversal agent ids are rejected before touching the filesystem.
    let err =
        remuda_node::subagent::read_for_store(&store, &instance_id, "../etc", None).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("agent"),
        "unexpected error: {err}"
    );
}

#[test]
fn unknown_instance_is_a_store_error() {
    let tmp = tempfile::tempdir().unwrap();
    let store = MemoryStore::open_journaled(tmp.path().join("node"), 256).unwrap();
    assert!(
        remuda_node::subagent::read_for_store(&store, &InstanceId::new(), AGENT, None).is_err()
    );
}
