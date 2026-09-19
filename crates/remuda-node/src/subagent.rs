//! Hub→Node `subagent.transcript`: on-demand drill-in read of one subagent's
//! sidechain transcript.
//!
//! Subagents (workflow members and plain Agent/Task tasks) are Claude
//! sub-sessions inside one Remuda session — never instances of their own — so
//! their conversations never enter the live journal. This RPC reads the
//! agent's `agent-<id>.jsonl` through the same transcript pipeline the main
//! session uses, on demand only (the live budget stays hook + main
//! transcript). Read-only, bounded, and path-safe (`remuda_journal`).

use remuda_journal::{MapContext, read_subagent_transcript};
use remuda_protocol::hubnode::METHOD_SUBAGENT_TRANSCRIPT;
use remuda_protocol::{
    Completeness, HostId, InstanceId, Knowledge, Observation, ObservationPayload, SchemaVersion,
    SourceChannel, U64,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use crate::DevNode;
use crate::LocalStore;
use crate::NodeError;

/// `available:false` because the owning session has no bound transcript.
pub const TRANSCRIPT_UNBOUND: &str = "transcript-unbound";

/// `available:false` because this agent's own sidechain file has not landed.
pub const AGENT_PENDING: &str = "agent-transcript-pending";

/// Methods handled by this module.
#[must_use]
pub fn is_subagent_method(method: &str) -> bool {
    method == METHOD_SUBAGENT_TRANSCRIPT
}

/// Dispatch a subagent RPC onto the local runtime.
///
/// # Errors
///
/// Propagates store, identity and read errors.
pub async fn handle_rpc(node: &DevNode, method: &str, params: &Value) -> Result<Value, NodeError> {
    debug_assert_eq!(method, METHOD_SUBAGENT_TRANSCRIPT);
    let instance_id = parse_instance(params)?;
    let agent_id = parse_agent(params)?;
    // The fold parks on the staging bridge while screenshots upload; run the
    // whole blocking transcript read off the async RPC task (D-045 §6.2).
    let stager = node.tool_media_stager();
    let node = node.clone();
    tokio::task::spawn_blocking(move || {
        read_for_store(node.store(), &instance_id, &agent_id, stager)
    })
    .await
    .map_err(|error| NodeError::Driver(format!("subagent read task panicked: {error}")))?
}

fn parse_agent(params: &Value) -> Result<String, NodeError> {
    params
        .get("agentId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| NodeError::InvalidRequest("subagent.transcript requires agentId".into()))
}

/// Core read against any local store (the dev server, carriers, and tests all
/// reach the RPC through here).
///
/// # Errors
///
/// Propagates store, identity and read errors.
pub fn read_for_store(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    agent_id: &str,
    tool_media_stager: Option<Arc<dyn remuda_protocol::ToolMediaStager>>,
) -> Result<Value, NodeError> {
    let instance = store.get_instance(instance_id)?;

    // The session directory is the sibling dir of the main transcript:
    // `<projects>/<enc>/<sid>/` for `<projects>/<enc>/<sid>.jsonl`.
    let transcript_path = match &instance.native_ref.transcript {
        Knowledge::Known { value } => Some(value.source_path.clone()),
        _ => None,
    };
    let Some(session_dir) = transcript_path
        .as_deref()
        .and_then(session_dir_from_transcript)
    else {
        // The session's own transcript is not bound, so there is no session
        // directory to read a sidechain from. That is a fact about the
        // *session*, not about this member — name it, so the row can say the
        // host has not bound the session rather than implying the subagent is
        // still starting (c-wfdrill2 B: a promoted shell-pty whose SessionStart
        // never reached the binding stays here).
        return Ok(json!({
            "available": false,
            "reason": TRANSCRIPT_UNBOUND,
            "events": [],
        }));
    };

    let ctx = MapContext::claude_file(
        instance_id.clone(),
        instance.journal_id.clone(),
        instance.host_id.clone(),
        match &instance.native_ref.session_id {
            Knowledge::Known { value } => value.clone(),
            _ => instance_id.as_id().to_string(),
        },
        SourceChannel::Transcript,
    )
    .with_media_stager(tool_media_stager);

    let Some(read) = read_subagent_transcript(&session_dir, agent_id, ctx).map_err(|error| {
        NodeError::InvalidRequest(format!("subagent transcript read failed: {error}"))
    })?
    else {
        // The agent is registered but its transcript has not landed yet —
        // the UI shows 「启动中」 rather than a dead row.
        return Ok(json!({
            "available": false,
            "reason": AGENT_PENDING,
            "events": [],
        }));
    };

    let host_id = instance.host_id.clone();
    let events: Vec<Observation> = read
        .envelopes
        .into_iter()
        .enumerate()
        .map(|(index, envelope)| stamp(envelope, instance_id, &host_id, (index as u64) + 1))
        .collect();

    let meta = read.meta;
    Ok(json!({
        "available": true,
        "meta": {
            "agentId": meta.agent_id,
            "runId": meta.run_id,
            "prompt": meta.prompt,
            "model": meta.model,
            "tokens": meta.tokens,
            "calls": meta.calls,
            "latestTool": meta.latest_tool,
            "startedAt": meta.started_at.map(String::from),
            "endedAt": meta.ended_at.map(String::from),
            "finalText": meta.final_text,
        },
        "events": events,
    }))
}

fn parse_instance(params: &Value) -> Result<InstanceId, NodeError> {
    let raw = params
        .get("instanceId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            NodeError::InvalidRequest("subagent.transcript requires instanceId".into())
        })?;
    Ok(InstanceId::from_str(raw)?)
}

/// Stamp envelope identity the same way the workflow producer does for
/// journal-synthesized observations: the instance/host/journal are the
/// session's; seqs are local to this on-demand page.
fn stamp(
    envelope: remuda_journal::Envelope,
    instance_id: &InstanceId,
    host_id: &HostId,
    seq: u64,
) -> Observation {
    Observation {
        schema_version: SchemaVersion,
        event_id: envelope.event_id.unwrap_or_default(),
        journal_id: envelope.journal_id,
        instance_id: instance_id.clone(),
        run_id: envelope.run_id,
        host_id: host_id.clone(),
        process_generation: envelope.process_generation,
        run_generation: envelope.run_generation,
        seq: U64(seq),
        observed_at: envelope.observed_at,
        native_at: envelope.native_at,
        source: envelope.source,
        completeness: Completeness::Structured,
        raw_ref: None,
        evidence_event_ids: envelope.evidence_event_ids,
        body: envelope.body as ObservationPayload,
    }
}

/// Sibling session directory for a `<…>/<sid>.jsonl` transcript path.
fn session_dir_from_transcript(transcript: &str) -> Option<PathBuf> {
    let transcript = Path::new(transcript);
    let parent = transcript.parent()?;
    let stem = transcript.file_stem()?;
    Some(parent.join(stem))
}
