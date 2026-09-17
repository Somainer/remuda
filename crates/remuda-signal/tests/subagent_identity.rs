//! Sub-agent tool hooks stamp their `agent_id` onto the derived tool
//! observations' `source.native_agent_id`, so the web assembler can group a
//! member's tool calls UNDER the parent row instead of flattening them into
//! the main transcript (workflow drill-in / grouping, c-wfdrill).

use remuda_protocol::{Knowledge, ObservationPayload};
use remuda_signal::{BusContext, HookEnvelope, SignalBus};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tokio::sync::mpsc;

fn bus_context() -> BusContext {
    BusContext {
        instance_id: remuda_protocol::InstanceId::new(),
        host_id: remuda_protocol::HostId::new(),
        journal_id: remuda_protocol::Id::new("obj").unwrap(),
        run_id: remuda_protocol::RunId::new(),
        driver_kind: remuda_protocol::DriverKind::ShellPty,
        adapter_version: "test".into(),
    }
}

async fn drain(
    rx: &mut mpsc::Receiver<remuda_protocol::Observation>,
) -> Vec<remuda_protocol::Observation> {
    tokio::task::yield_now().await;
    let mut out = Vec::new();
    while let Ok(observation) = rx.try_recv() {
        out.push(observation);
    }
    out
}

const AGENT: &str = "aae139d44933cefe2";

#[tokio::test]
async fn subagent_tool_call_carries_the_native_agent_id() {
    let (tx, mut rx) = mpsc::channel(128);
    let bus = SignalBus::new(bus_context(), tx, Arc::new(AtomicU64::new(0)));
    bus.handle(HookEnvelope {
        credential: "c".into(),
        event: "SessionStart".into(),
        ppid: 4242,
        payload: serde_json::json!({"session_id":"s","transcript_path":"/w/s.jsonl"}),
    })
    .await;
    drain(&mut rx).await;

    // A workflow member's own Bash PreToolUse — same claude pid, agent_id set.
    bus.handle(HookEnvelope {
        credential: "c".into(),
        event: "PreToolUse".into(),
        ppid: 4242,
        payload: serde_json::json!({
            "tool_use_id": "toolu_sub_bash",
            "tool_name": "Bash",
            "agent_id": AGENT,
            "agent_type": "workflow-subagent",
            "tool_input": {"command": "echo pong"},
        }),
    })
    .await;
    let observations = drain(&mut rx).await;
    let derived = observations
        .iter()
        .find(|o| matches!(o.body, ObservationPayload::ToolCall(_)))
        .expect("the derived tool call is journaled");
    assert_eq!(
        derived.source.native_agent_id,
        Knowledge::Known {
            value: AGENT.to_owned()
        }
    );

    // A main-session tool hook stays unattributed.
    bus.handle(HookEnvelope {
        credential: "c".into(),
        event: "PreToolUse".into(),
        ppid: 4242,
        payload: serde_json::json!({
            "tool_use_id": "toolu_main_bash",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
        }),
    })
    .await;
    let observations = drain(&mut rx).await;
    let main = observations
        .iter()
        .find(|o| matches!(o.body, ObservationPayload::ToolCall(_)))
        .expect("main tool call");
    assert_eq!(main.source.native_agent_id, Knowledge::NotApplicable);
}

#[tokio::test]
async fn subagent_stop_completion_is_stamped_with_the_agent_id() {
    let (tx, mut rx) = mpsc::channel(128);
    let bus = SignalBus::new(bus_context(), tx, Arc::new(AtomicU64::new(0)));
    bus.handle(HookEnvelope {
        credential: "c".into(),
        event: "SessionStart".into(),
        ppid: 4242,
        payload: serde_json::json!({"session_id":"s","transcript_path":"/w/s.jsonl"}),
    })
    .await;
    drain(&mut rx).await;

    // Background Agent launch: the node stays open awaiting SubagentStop.
    bus.handle(HookEnvelope {
        credential: "c".into(),
        event: "PreToolUse".into(),
        ppid: 4242,
        payload: serde_json::json!({
            "tool_use_id": "toolu_task_bg",
            "tool_name": "Task",
            "tool_input": {"prompt": "do the thing"},
        }),
    })
    .await;
    bus.handle(HookEnvelope {
        credential: "c".into(),
        event: "PostToolUse".into(),
        ppid: 4242,
        payload: serde_json::json!({
            "tool_use_id": "toolu_task_bg",
            "tool_name": "Task",
            "tool_response": {"isAsync": true, "status": "async_launched", "agentId": AGENT},
        }),
    })
    .await;
    drain(&mut rx).await;

    // The closing SubagentStop folds a Final result onto the launch node; it
    // comes from the subagent and is stamped accordingly.
    bus.handle(HookEnvelope {
        credential: "c".into(),
        event: "SubagentStop".into(),
        ppid: 4242,
        payload: serde_json::json!({
            "agent_id": AGENT,
            "agent_type": "workflow-subagent",
            "agent_transcript_path": format!("/w/s/subagents/workflows/wf_x/agent-{AGENT}.jsonl"),
            "last_assistant_message": "done",
        }),
    })
    .await;
    let observations = drain(&mut rx).await;
    // The launch's rev-3 partial result was drained above; this drain carries
    // the raw SubagentStop lifecycle plus its rev-4 final ToolResult.
    let completion = observations
        .iter()
        .find(|o| matches!(&o.body, ObservationPayload::ToolResult(r) if r.stage == remuda_protocol::ResultStage::Final))
        .expect("SubagentStop folds the final result");
    assert_eq!(
        completion.source.native_agent_id,
        Knowledge::Known {
            value: AGENT.to_owned()
        }
    );
}
