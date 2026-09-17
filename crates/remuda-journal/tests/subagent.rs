//! On-demand drill-in reader against the recorded 2.1.221 agent transcripts.

use remuda_journal::{MapContext, SubagentKind, locate_agent_file, read_subagent_transcript};
use remuda_protocol::{HostId, Id, InstanceId, ObservationPayload, SourceChannel, SourceDelivery};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const RUN: &str = "wf_c3422384-cb1";
const AGENT: &str = "aae139d44933cefe2";

fn context() -> MapContext {
    MapContext::claude_file(
        InstanceId::new(),
        Id::new("obj").unwrap(),
        HostId::new(),
        "sess",
        SourceChannel::Transcript,
    )
}

fn copy_session() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_dir = session.join("subagents/workflows").join(RUN);
    fs::create_dir_all(&run_dir).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/workflow/runs-221")
        .join(RUN);
    for entry in fs::read_dir(&fixture).unwrap() {
        let entry = entry.unwrap();
        fs::copy(entry.path(), run_dir.join(entry.file_name())).unwrap();
    }
    (tmp, session)
}

#[test]
fn locates_workflow_agent_and_plain_agent_files() {
    let (_tmp, session) = copy_session();
    let (path, kind) = locate_agent_file(&session, AGENT).expect("workflow agent file");
    assert!(path.ends_with(format!("workflows/{RUN}/agent-{AGENT}.jsonl")));
    assert_eq!(
        kind,
        SubagentKind::Workflow {
            run_id: RUN.to_owned()
        }
    );

    // A plain Agent transcript under subagents/ is the fallback location.
    let plain_dir = session.join("subagents");
    fs::copy(
        plain_dir
            .join("workflows")
            .join(RUN)
            .join(format!("agent-{AGENT}.jsonl")),
        plain_dir.join("agent-cafef00d.jsonl"),
    )
    .unwrap();
    let (_, kind) = locate_agent_file(&session, "cafef00d").expect("plain agent file");
    assert_eq!(kind, SubagentKind::Agent);

    // Path traversal is rejected before any filesystem lookup.
    assert!(locate_agent_file(&session, "../etc/passwd").is_none());
}

#[test]
fn reads_the_recorded_agent_through_the_transcript_pipeline() {
    let (_tmp, session) = copy_session();
    let read = read_subagent_transcript(&session, AGENT, context())
        .unwrap()
        .expect("transcript exists");

    assert_eq!(read.meta.agent_id, AGENT);
    assert_eq!(read.meta.run_id.as_deref(), Some(RUN));
    // The first user record is the script prompt literal.
    assert!(
        read.meta.prompt.is_some(),
        "prompt folded from first user record"
    );
    assert!(
        read.meta.model.is_some(),
        "model folded from assistant records"
    );
    assert!(read.meta.tokens.is_some_and(|n| n > 0));
    assert!(read.meta.calls > 0);
    assert!(read.meta.started_at.is_some() && read.meta.ended_at.is_some());

    // The mapped envelopes carry conversation content, not just metrics.
    let has_message = read
        .envelopes
        .iter()
        .any(|env| matches!(env.body, ObservationPayload::Message(_)));
    let has_tool_call = read
        .envelopes
        .iter()
        .any(|env| matches!(env.body, ObservationPayload::ToolCall(_)));
    assert!(
        has_message,
        "the drill-in renders prompt/final-text messages"
    );
    assert!(
        has_tool_call,
        "the drill-in renders the subagent's own tool rows"
    );
    // On-demand reads are replay evidence on the transcript channel.
    assert!(read.envelopes.iter().all(|env| {
        env.source.channel == SourceChannel::Transcript
            && env.source.delivery == SourceDelivery::Replay
    }));
}

#[test]
fn missing_agent_is_starting_not_an_error() {
    let (_tmp, session) = copy_session();
    assert!(
        read_subagent_transcript(&session, "0000000000000000", context())
            .unwrap()
            .is_none()
    );
}
