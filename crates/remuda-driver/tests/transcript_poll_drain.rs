//! c-ctxusage r4 item 1 regression: later blocks of ONE assistant message must
//! survive a poll boundary.
//!
//! Claude writes one content block per transcript record and repeats the same
//! `message.id` across the message's records, each with its OWN uuid. A
//! thinking-then-text (or text-then-tool_use) message routinely spans two 75 ms
//! polls (the real `effort-walk-21289.jsonl` message
//! `msg_recorded_effort4_walk_04` is exactly such a pair). Before the fix the
//! content drain assembled later records on the FIRST record's envelope (its
//! uuid), so the stdout mapper dropped them as an already-seen snapshot AFTER
//! the blocks had been taken — the text/tool_use vanished forever.

use remuda_driver::TranscriptMapper;
use remuda_protocol::{
    ContentBlock, DriverKind, HostId, Id, InstanceId, MutationOperation, Observation,
    ObservationKind, ObservationPayload, RunId,
};
use std::path::{Path, PathBuf};

fn mapper() -> TranscriptMapper {
    TranscriptMapper::new(
        DriverKind::ShellPty,
        InstanceId::new(),
        RunId::new(),
        Id::new("obj").expect("journal id"),
        HostId::new(),
        "poll-drain-session".into(),
        "2.1.289".into(),
    )
}

fn assistant(uuid: &str, message_id: &str, content: serde_json::Value) -> String {
    serde_json::json!({
        "type": "assistant",
        "uuid": uuid,
        "timestamp": "2026-10-07T00:00:00.000Z",
        "isSidechain": false,
        "message": {
            "type": "message",
            "role": "assistant",
            "id": message_id,
            "model": "claude-opus-5-5",
            "content": content,
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 1,
                "cache_read_input_tokens": 100,
                "cache_creation_input_tokens": 5,
                "output_tokens": 9,
                "cache_creation": {"ephemeral_5m_input_tokens": 5, "ephemeral_1h_input_tokens": 0}
            }
        }
    })
    .to_string()
}

fn tool_result(tool_use_id: &str, text: &str) -> String {
    serde_json::json!({
        "type": "user",
        "uuid": "uuid-user-result",
        "timestamp": "2026-10-07T00:00:01.000Z",
        "sourceToolUseID": tool_use_id,
        "message": {
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": text}]
        }
    })
    .to_string()
}

/// Content-based signature of one structured observation:
/// (kind, mutation operation, joined block text / tool name).
///
/// Node ids are minted per mapper, so cross-mapper comparisons use content —
/// block loss/duplication changes this sequence deterministically.
fn signature_of(obs: &Observation) -> Option<(ObservationKind, &'static str, String)> {
    let op: &'static str = match &obs.body {
        ObservationPayload::Message(p) => {
            if p.mutation.operation == MutationOperation::Close {
                "close"
            } else {
                "open"
            }
        }
        ObservationPayload::ToolCall(p) => {
            if p.mutation.operation == MutationOperation::Close {
                "close"
            } else {
                "open"
            }
        }
        ObservationPayload::ToolResult(_) => "result",
        _ => return None,
    };
    let text = |blocks: &[ContentBlock]| {
        blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(block) => Some(block.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    };
    match &obs.body {
        ObservationPayload::Message(payload) => Some((
            ObservationKind::Message,
            op,
            format!("message:{}", text(&payload.blocks)),
        )),
        ObservationPayload::ToolCall(payload) => Some((
            ObservationKind::ToolCall,
            op,
            format!(
                "tool:{}",
                match &payload.tool_name {
                    remuda_protocol::Knowledge::Known { value } => value.clone(),
                    _ => "?".to_string(),
                }
            ),
        )),
        ObservationPayload::ToolResult(payload) => {
            Some((ObservationKind::ToolResult, op, text(&payload.blocks)))
        }
        _ => None,
    }
}

/// The close-bearing text nodes in a batch (each block closes exactly once).
fn closed_texts(observations: &[Observation], needle: &str) -> usize {
    observations
        .iter()
        .filter_map(signature_of)
        .filter(|(kind, op, text)| {
            *kind == ObservationKind::Message && *op == "close" && text.contains(needle)
        })
        .count()
}

#[test]
fn later_blocks_of_one_message_survive_the_poll_between_records() {
    // Record A: a text block in the first poll.
    let a = assistant(
        "uuid-a",
        "msg_split",
        serde_json::json!([{"type": "text", "text": "PLAN_TEXT"}]),
    );
    // Record B: same message.id, DIFFERENT uuid, a tool_use block in the next
    // poll.
    let b = assistant(
        "uuid-b",
        "msg_split",
        serde_json::json!([{
            "type": "tool_use",
            "id": "toolu_split_1",
            "name": "Bash",
            "input": {"command": "echo hi"}
        }]),
    );
    let result = tool_result("toolu_split_1", "hi");

    let mut mapper = mapper();

    let first = mapper.map_line(&a).expect("record a");
    let poll_a = mapper.flush().expect("poll after a");
    assert_eq!(
        closed_texts(&first, "PLAN_TEXT") + closed_texts(&poll_a, "PLAN_TEXT"),
        1,
        "record A's text node closes exactly once"
    );

    // The later record maps, the poll drains its tool_use block, and the user
    // tool_result pairs with it.
    let second = mapper.map_line(&b).expect("record b");
    let poll_b = mapper.flush().expect("poll after b");
    let third = mapper.map_line(&result).expect("tool result");
    let tail = mapper.finish().expect("finish");
    let after = [
        second.as_slice(),
        poll_b.as_slice(),
        third.as_slice(),
        tail.as_slice(),
    ]
    .concat();
    let after_sig: Vec<_> = after.iter().filter_map(signature_of).collect();

    assert!(
        after_sig
            .iter()
            .any(|(kind, op, text)| *kind == ObservationKind::ToolCall
                && *op == "close"
                && text == "tool:Bash"),
        "record B's tool_use must OPEN and CLOSE after the poll split: {after_sig:?}"
    );
    assert!(
        after_sig
            .iter()
            .any(|(kind, _, text)| *kind == ObservationKind::ToolResult && text == "hi"),
        "the later tool_result must be mapped and pair with the call"
    );
    assert_eq!(
        closed_texts(&after, "PLAN_TEXT"),
        0,
        "the second drain never re-emits record A: {after_sig:?}"
    );
}

/// Poll-granular replay of the real 2.1.289 fixture: feed ONE record, flush,
/// repeat, and compare the resulting structured observations against a
/// whole-line replay (mapper per line, single finish at the end).
#[test]
fn poll_granular_replay_matches_whole_line_replay() {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../remuda-journal/tests/fixtures/effort-21289/effort-walk-21289.jsonl");
    let body = std::fs::read_to_string(path).expect("fixture");

    // Baseline: once-per-line, one finish at the end.
    let mut whole = mapper();
    let mut whole_out = Vec::new();
    for line in body.lines().filter(|line| !line.trim().is_empty()) {
        whole_out.extend(whole.map_line(line).expect("line"));
    }
    whole_out.extend(whole.finish().expect("finish"));
    let whole_sig: Vec<_> = whole_out.iter().filter_map(signature_of).collect();

    // Poll granularity: a flush after EVERY record.
    let mut granular = mapper();
    let mut gran_out = Vec::new();
    for line in body.lines().filter(|line| !line.trim().is_empty()) {
        gran_out.extend(granular.map_line(line).expect("line"));
        gran_out.extend(granular.flush().expect("poll"));
    }
    gran_out.extend(granular.finish().expect("finish"));
    let gran_sig: Vec<_> = gran_out.iter().filter_map(signature_of).collect();

    assert_eq!(
        gran_sig.len(),
        whole_sig.len(),
        "poll-granular replay emits the same number of structured observations"
    );
    assert_eq!(
        gran_sig, whole_sig,
        "the walk_04 thinking+text pair must survive the poll split identically"
    );
}
