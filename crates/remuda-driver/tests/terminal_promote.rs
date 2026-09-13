//! Terminal → agent promotion (D-025): transcript hydration and the
//! transcript locator, against a recorded-shape fixture.
//!
//! Source: `tests/fixtures/claude-transcript.jsonl` (see `fixtures/SOURCES.md`).

use remuda_driver::{
    DriverKind, TranscriptMapper, TranscriptTail, encode_project_dir, locate_transcript,
    project_dir,
};
use remuda_protocol::{
    Completeness, HostId, Id, InstanceId, MutationOperation, ObservationPayload, RunId,
    SourceChannel,
};
use std::path::{Path, PathBuf};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-transcript.jsonl")
}

fn mapper() -> TranscriptMapper {
    TranscriptMapper::new(
        DriverKind::ShellPty,
        InstanceId::new(),
        RunId::new(),
        Id::new("obj").expect("journal id"),
        HostId::new(),
        "11111111-2222-4333-8444-555555555555".into(),
        "promoted".into(),
    )
}

/// Map every line of the fixture, in order, as the poller would.
fn hydrate() -> Vec<remuda_protocol::Observation> {
    let body = std::fs::read_to_string(fixture()).expect("fixture");
    let mut mapper = mapper();
    body.lines()
        .flat_map(|line| mapper.map_line(line).expect("map transcript line"))
        .collect()
}

fn payload_kinds(observations: &[remuda_protocol::Observation]) -> Vec<&'static str> {
    observations
        .iter()
        .map(|observation| match &observation.body {
            ObservationPayload::Message(_) => "message",
            ObservationPayload::Thought(_) => "thought",
            ObservationPayload::ToolCall(_) => "toolCall",
            ObservationPayload::ToolResult(_) => "toolResult",
            ObservationPayload::Lifecycle(_) => "lifecycle",
            _ => "other",
        })
        .collect()
}

/// `kind:operation` per observation. Assistant blocks arrive as an
/// open/replace followed by a close, which is the mutation contract the
/// journal and the web transcript already rely on.
fn mutations(observations: &[remuda_protocol::Observation]) -> Vec<String> {
    observations
        .iter()
        .map(|observation| {
            let (kind, operation) = match &observation.body {
                ObservationPayload::Message(payload) => ("message", payload.mutation.operation),
                ObservationPayload::Thought(payload) => ("thought", payload.mutation.operation),
                ObservationPayload::ToolCall(payload) => ("toolCall", payload.mutation.operation),
                ObservationPayload::ToolResult(payload) => {
                    ("toolResult", payload.mutation.operation)
                }
                _ => ("other", MutationOperation::Open),
            };
            format!("{kind}:{}", operation_name(operation))
        })
        .collect()
}

fn operation_name(operation: MutationOperation) -> &'static str {
    match operation {
        MutationOperation::Open => "open",
        MutationOperation::Append => "append",
        MutationOperation::Replace => "replace",
        MutationOperation::Close => "close",
    }
}

#[test]
fn a_transcript_hydrates_the_conversation_in_order() {
    let observations = hydrate();
    assert_eq!(
        payload_kinds(&observations),
        vec![
            "message",
            "thought",
            "thought",
            "toolCall",
            "toolCall",
            "toolResult",
            "message",
            "message"
        ],
        "hydration must yield the user turn, thinking, tool call, its result, and the reply"
    );
}

#[test]
fn each_hydrated_block_opens_and_closes() {
    assert_eq!(
        mutations(&hydrate()),
        vec![
            // The user turn is complete the moment it is read.
            "message:open",
            "thought:open",
            "thought:close",
            "toolCall:open",
            "toolCall:close",
            "toolResult:open",
            "message:open",
            "message:close",
        ],
        "a transcript replay must produce the same open→close mutations as the live stream"
    );
}

#[test]
fn hydrated_observations_are_structured_transcript_records_on_the_promoted_driver() {
    for observation in hydrate() {
        assert_eq!(observation.completeness, Completeness::Structured);
        assert_eq!(observation.source.channel, SourceChannel::Transcript);
        assert_eq!(
            observation.source.driver_kind,
            DriverKind::ShellPty,
            "a promoted terminal keeps its shell-pty driver"
        );
    }
}

#[test]
fn the_first_message_is_the_users_prompt_and_the_last_is_the_reply() {
    let observations = hydrate();
    let ObservationPayload::Message(first) = &observations[0].body else {
        panic!("expected a user message first");
    };
    assert_eq!(first.role, remuda_protocol::MessageRole::User);
    assert_eq!(
        first.blocks,
        vec![remuda_protocol::ContentBlock::Text(Box::new(
            remuda_protocol::TextBlock {
                text: "say hi in one word".into()
            }
        ))]
    );
    let ObservationPayload::Message(last) = &observations[7].body else {
        panic!("expected an assistant message last");
    };
    assert_eq!(last.role, remuda_protocol::MessageRole::Assistant);
}

#[test]
fn bookkeeping_and_sidechain_records_are_not_journaled() {
    let mut mapper = mapper();
    for line in [
        r#"{"type":"mode","mode":"normal"}"#,
        r#"{"type":"permission-mode","permissionMode":"default"}"#,
        r#"{"type":"atis-latch","atis":"deadbeef"}"#,
        r#"{"type":"file-history-snapshot","messageId":"x","snapshot":{}}"#,
        r#"{"type":"attachment","attachment":{"type":"hook_success"}}"#,
        r#"{"type":"assistant","isSidechain":true,"message":{"id":"m","role":"assistant","content":[{"type":"text","text":"sub"}]}}"#,
        "",
        "   ",
        "{ not json",
    ] {
        assert!(
            mapper.map_line(line).expect("map").is_empty(),
            "{line} must map to nothing"
        );
    }
}

#[test]
fn the_tool_call_and_its_result_share_one_native_tool_id() {
    let observations = hydrate();
    let ObservationPayload::ToolCall(call) = &observations[3].body else {
        panic!("expected a tool call");
    };
    let ObservationPayload::ToolResult(result) = &observations[5].body else {
        panic!("expected a tool result");
    };
    assert_eq!(
        call.tool_call_id, result.tool_call_id,
        "the result must attach to the call the UI already rendered"
    );
    assert_eq!(result.outcome, remuda_protocol::ToolOutcome::Succeeded);
}

#[test]
fn a_promoted_session_is_located_by_encoded_cwd_and_detection_time() {
    let tmp = tempfile::tempdir().expect("tmp");
    let cwd = Path::new("/work/repo");
    let dir = project_dir(tmp.path(), cwd);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let session = "11111111-2222-4333-8444-555555555555";
    let path = dir.join(format!("{session}.jsonl"));
    std::fs::copy(fixture(), &path).expect("copy fixture");

    assert_eq!(encode_project_dir(cwd), "-work-repo");
    // argv gave us the session id, so the lookup is exact.
    assert_eq!(
        locate_transcript(
            tmp.path(),
            cwd,
            Some(session),
            std::time::SystemTime::UNIX_EPOCH
        ),
        Some(path.clone())
    );
    // Without one, the newest file written since detection is adopted.
    assert_eq!(
        locate_transcript(tmp.path(), cwd, None, std::time::SystemTime::UNIX_EPOCH),
        Some(path)
    );
}

#[test]
fn tailing_a_growing_transcript_hydrates_only_the_new_lines() {
    let tmp = tempfile::tempdir().expect("tmp");
    let path = tmp.path().join("live.jsonl");
    let body = std::fs::read_to_string(fixture()).expect("fixture");
    let lines: Vec<&str> = body.lines().collect();
    let split = 5;

    std::fs::write(&path, format!("{}\n", lines[..split].join("\n"))).expect("seed");
    let mut tail = TranscriptTail::new(path.clone());
    let mut mapper = mapper();
    let first: Vec<_> = tail
        .poll()
        .expect("poll")
        .iter()
        .flat_map(|line| mapper.map_line(line).expect("map"))
        .collect();
    assert_eq!(payload_kinds(&first), vec!["message"]);

    // The agent keeps working; the tail picks up only what was appended.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("append");
    use std::io::Write;
    writeln!(file, "{}", lines[split..].join("\n")).expect("write");
    let rest: Vec<_> = tail
        .poll()
        .expect("poll")
        .iter()
        .flat_map(|line| mapper.map_line(line).expect("map"))
        .collect();
    assert_eq!(
        payload_kinds(&rest),
        vec![
            "thought",
            "thought",
            "toolCall",
            "toolCall",
            "toolResult",
            "message",
            "message"
        ]
    );
    // Nothing new means nothing re-emitted.
    assert!(tail.poll().expect("poll").is_empty());
}
