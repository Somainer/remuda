//! Terminal → agent promotion (D-025): transcript hydration and the
//! transcript locator, against a recorded-shape fixture.
//!
//! Source: `tests/fixtures/claude-transcript.jsonl` (see `fixtures/SOURCES.md`).

use remuda_driver::{
    BindingSource, DriverKind, TranscriptMapper, TranscriptTail, bind_by_pid_file,
    bind_by_session_id, encode_project_dir, list_candidates, project_dir,
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
    let mut out: Vec<_> = body
        .lines()
        .flat_map(|line| mapper.map_line(line).expect("map transcript line"))
        .collect();
    // An assistant run stays buffered until superseded, so the closing turn
    // only lands on the end-of-batch flush the tailer performs (D-028 §7).
    out.extend(mapper.flush().expect("flush"));
    out
}

/// The conversation payloads, dropping the bookkeeping lifecycles that D-028
/// §7 now retains (`mode`, `permission-mode`, the queue ledger). Those have
/// their own coverage in `transcript_origin.rs`; these tests are about the
/// conversation shape.
fn conversation(
    observations: &[remuda_protocol::Observation],
) -> Vec<remuda_protocol::Observation> {
    observations
        .iter()
        .filter(|observation| !matches!(observation.body, ObservationPayload::Lifecycle(_)))
        .cloned()
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
    let observations = conversation(&hydrate());
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
        mutations(&conversation(&hydrate())),
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
    let observations = conversation(&hydrate());
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
    // `mode` and `permission-mode` are the exception D-028 §7 calls out: they
    // are not conversation, but dropping them loses the record of a permission
    // change the user can see in the terminal.
    for line in [
        r#"{"type":"mode","mode":"normal"}"#,
        r#"{"type":"permission-mode","permissionMode":"default"}"#,
    ] {
        assert!(
            !mapper.map_line(line).expect("map").is_empty(),
            "{line} must reach the journal as a lifecycle"
        );
    }
}

#[test]
fn the_tool_call_and_its_result_share_one_native_tool_id() {
    let observations = conversation(&hydrate());
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
fn a_promoted_session_is_bound_by_exact_identity_never_by_newest_mtime() {
    let tmp = tempfile::tempdir().expect("tmp");
    // Use a real cwd so canonicalize-based project_dir resolution works.
    let cwd = tmp.path().join("work-repo");
    std::fs::create_dir_all(&cwd).expect("mkdir");
    let dir = project_dir(tmp.path(), &cwd);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let session = "11111111-2222-4333-8444-555555555555";
    let path = dir.join(format!("{session}.jsonl"));
    std::fs::copy(fixture(), &path).expect("copy fixture");

    assert_eq!(encode_project_dir(Path::new("/work/repo")), "-work-repo");

    // Channel A (argv): an explicit session id binds its exact file.
    assert_eq!(
        bind_by_session_id(tmp.path(), &cwd, session)
            .expect("argv binding")
            .path,
        path
    );
    // An unknown id never falls through to "newest file".
    assert!(bind_by_session_id(tmp.path(), &cwd, "missing").is_none());

    // Channel B (pid file): the foreground pid names the exact session.
    let sessions = tmp.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir");
    std::fs::write(
        sessions.join("4242.json"),
        format!(
            r#"{{"pid":4242,"sessionId":"{session}","cwd":{}}}"#,
            serde_json::json!(cwd.to_string_lossy())
        ),
    )
    .expect("write pid session");
    let binding = bind_by_pid_file(tmp.path(), 4242, &cwd).expect("pid binding");
    assert_eq!(binding.path, path);
    assert_eq!(binding.source, BindingSource::PidFile);
    // A different pid with no registry entry is unbound, never "newest file".
    assert!(bind_by_pid_file(tmp.path(), 9999, &cwd).is_none());

    // Channel C (manual): candidates are merely listed; nothing is auto-picked.
    assert_eq!(list_candidates(tmp.path(), &cwd).len(), 1);
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
    assert_eq!(payload_kinds(&conversation(&first)), vec!["message"]);

    // The agent keeps working; the tail picks up only what was appended.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("append");
    use std::io::Write;
    writeln!(file, "{}", lines[split..].join("\n")).expect("write");
    let mut rest: Vec<_> = tail
        .poll()
        .expect("poll")
        .iter()
        .flat_map(|line| mapper.map_line(line).expect("map"))
        .collect();
    // The poller flushes at the end of every batch, which is what releases the
    // closing turn of a finished response instead of holding it until the next
    // record arrives.
    rest.extend(mapper.flush().expect("flush"));
    assert_eq!(
        payload_kinds(&conversation(&rest)),
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

/// The SessionStart hook payload shape (D-028 P1 shim → `remuda hook emit`).
/// Fixture: `tests/fixtures/session-start-hook.json`.
#[test]
fn the_session_start_hook_fixture_has_the_documented_shape() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/session-start-hook.json");
    let body = std::fs::read_to_string(path).expect("hook fixture");
    let report = remuda_driver::SessionStartReport::from_stdin(&body).expect("parse fixture");
    assert_eq!(report.session_id, "04b95a78-e876-4212-aa9c-a6482f30f583");
    assert_eq!(report.ppid, Some(5150));
    assert!(
        report
            .transcript_path
            .ends_with("04b95a78-e876-4212-aa9c-a6482f30f583.jsonl")
    );
    assert_eq!(
        report.cwd.as_deref(),
        Some(Path::new("/Users/dev/work/repo"))
    );
    // The hook binds only when its parent is the detected foreground process.
    assert!(
        report.bind(5150).is_none(),
        "no transcript file on disk here"
    );
    assert!(report.bind(9999).is_none(), "a different pid never binds");
}
