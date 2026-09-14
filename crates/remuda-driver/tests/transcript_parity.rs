//! Parity between the two Claude paths on one real transcript (D-028 §12).
//!
//! §13 P3's acceptance is that «两条路径的 journal diff 为空». The two paths
//! that P3 actually changes are the stdout mapper (`claude-print`) and the
//! transcript mapper (`shell-pty` / `claude-pty` hydration). This test feeds
//! the *same* conversation through both and compares the conversation they
//! produce, so a regrouping change that fixes one path and breaks the other
//! cannot pass.
//!
//! Envelope identity is deliberately not compared — ids, timestamps, driver
//! and channel all differ by construction, and `remuda journal diff` strips
//! exactly those. What must match is the conversation: same messages, same
//! text, same tools linked to the same results, in the same order.

use remuda_driver::claude_print::review::StdoutMapper;
use remuda_driver::{DriverKind, TranscriptMapper};
use remuda_protocol::{
    ContentBlock, HostId, Id, InstanceId, MessageRole, Observation, ObservationPayload, RunId,
};
use serde_json::{Value, json};

/// The conversation an observation stream amounts to, with identity stripped.
///
/// Mutation bookkeeping (`open`/`append`/`close`, revisions) is folded away
/// the way `journal diff` folds it: the two carriers are allowed to *arrive*
/// at the text differently — that is the whitelisted `granularity` difference
/// — but they must arrive at the same text.
fn conversation(observations: &[Observation]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut tools: Vec<String> = Vec::new();
    for observation in observations {
        match &observation.body {
            ObservationPayload::Message(payload) => {
                let text = text_of(&payload.blocks);
                if text.trim().is_empty() {
                    continue;
                }
                let role = match payload.role {
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::System => "system",
                };
                let line = format!("{role}: {text}");
                // Fold the mutation chain: a carrier that opens then closes the
                // same text must not count twice.
                if out.last() != Some(&line) {
                    out.push(line);
                }
            }
            ObservationPayload::Thought(payload) => {
                let Some(text) = payload.text.as_ref().filter(|text| !text.trim().is_empty())
                else {
                    continue;
                };
                let line = format!("thought: {text}");
                if out.last() != Some(&line) {
                    out.push(line);
                }
            }
            ObservationPayload::ToolCall(payload) => {
                let name = match &payload.tool_name {
                    remuda_protocol::Knowledge::Known { value } => value.clone(),
                    _ => "?".into(),
                };
                // Alias native ids to per-side ordinals: the ids differ, the
                // order they appear in does not.
                let alias = match tools
                    .iter()
                    .position(|id| id == payload.tool_call_id.as_str())
                {
                    Some(index) => index,
                    None => {
                        tools.push(payload.tool_call_id.as_str().to_owned());
                        tools.len() - 1
                    }
                };
                let line = format!("tool#{alias}: {name}");
                if out.last() != Some(&line) {
                    out.push(line);
                }
            }
            ObservationPayload::ToolResult(payload) => {
                let alias = tools
                    .iter()
                    .position(|id| id == payload.tool_call_id.as_str());
                let line = format!(
                    "result#{}: {}",
                    alias.map_or("unlinked".to_owned(), |index| index.to_string()),
                    text_of(&payload.blocks).trim()
                );
                if out.last() != Some(&line) {
                    out.push(line);
                }
            }
            _ => {}
        }
    }
    out
}

fn text_of(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
        .trim()
        .to_owned()
}

/// One scenario, declared once: a prompt, commentary, a tool and its result,
/// and a closing reply. Both emitters below render *this*, so the two sides
/// cannot silently drift apart (same discipline as `journal-parity/generate.py`).
struct Scenario {
    prompt: &'static str,
    commentary: &'static str,
    tool_name: &'static str,
    tool_output: &'static str,
    reply: &'static str,
}

const SCENARIO: Scenario = Scenario {
    prompt: "Read README.md and tell me the tagline.",
    commentary: "Let me read it.",
    tool_name: "Read",
    tool_output: "# Remuda\n\nUnified remote agent runtime.",
    reply: "The tagline is “unified remote agent runtime”.",
};

fn tool_input() -> Value {
    json!({ "file_path": "README.md" })
}

/// Render the scenario the way `claude -p --output-format stream-json` does:
/// whole `assistant` frames, each carrying every block of one message.
fn print_frames() -> Vec<Value> {
    vec![
        json!({ "type": "user", "uuid": "u-1",
                "message": { "role": "user", "content": SCENARIO.prompt } }),
        json!({ "type": "assistant", "uuid": "a-1", "message": {
            "id": "msg_print_1", "role": "assistant", "stop_reason": "tool_use",
            "content": [
                { "type": "text", "text": SCENARIO.commentary },
                { "type": "tool_use", "id": "toolu_print_1",
                  "name": SCENARIO.tool_name, "input": tool_input() },
            ],
        }}),
        json!({ "type": "user", "uuid": "u-2", "message": { "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "toolu_print_1",
              "content": SCENARIO.tool_output },
        ]}}),
        json!({ "type": "assistant", "uuid": "a-2", "message": {
            "id": "msg_print_2", "role": "assistant", "stop_reason": "end_turn",
            "content": [{ "type": "text", "text": SCENARIO.reply }],
        }}),
    ]
}

/// Render the same scenario the way the native TUI writes its transcript:
/// **one content block per record**, with consecutive records sharing a
/// `message.id`. This split is the whole reason the regrouping fix exists.
fn transcript_records() -> Vec<Value> {
    vec![
        json!({ "type": "user", "uuid": "tu-1", "promptSource": "typed",
                "message": { "role": "user", "content": SCENARIO.prompt } }),
        json!({ "type": "assistant", "uuid": "ta-1", "message": {
            "id": "msg_pty_1", "role": "assistant", "stop_reason": "tool_use",
            "content": [{ "type": "text", "text": SCENARIO.commentary }],
        }}),
        json!({ "type": "assistant", "uuid": "ta-2", "message": {
            "id": "msg_pty_1", "role": "assistant", "stop_reason": "tool_use",
            "content": [{ "type": "tool_use", "id": "toolu_pty_1",
                          "name": SCENARIO.tool_name, "input": tool_input() }],
        }}),
        json!({ "type": "user", "uuid": "tu-2",
        "sourceToolUseID": "toolu_pty_1",
        "message": { "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "toolu_pty_1",
              "content": SCENARIO.tool_output },
        ]}}),
        json!({ "type": "assistant", "uuid": "ta-3", "message": {
            "id": "msg_pty_2", "role": "assistant", "stop_reason": "end_turn",
            "content": [{ "type": "text", "text": SCENARIO.reply }],
        }}),
    ]
}

fn via_print() -> Vec<Observation> {
    // One mapper for the whole scenario: a `tool_result` can only find the
    // `tool_use` it belongs to if the native ids correlate across frames.
    let mut mapper = StdoutMapper::new();
    print_frames()
        .into_iter()
        .flat_map(|frame| mapper.map(frame).expect("map stdout frame"))
        .collect()
}

fn via_transcript() -> Vec<Observation> {
    let mut mapper = TranscriptMapper::new(
        DriverKind::ShellPty,
        InstanceId::new(),
        RunId::new(),
        Id::new("obj").expect("journal id"),
        HostId::new(),
        "88888888-9999-4aaa-8bbb-cccccccccccc".into(),
        "shell-pty".into(),
    );
    let mut out: Vec<Observation> = transcript_records()
        .into_iter()
        .flat_map(|record| mapper.map_record(record).expect("map transcript record"))
        .collect();
    out.extend(mapper.flush().expect("flush"));
    out
}

/// The P3 acceptance: the same conversation, whichever carrier observed it.
///
/// Before the regrouping fix the transcript side produced a different shape
/// here — each record became its own "message", so the tool bookkeeping slid
/// and the sides did not line up.
#[test]
fn both_carriers_yield_the_same_conversation() {
    assert_eq!(
        conversation(&via_transcript()),
        conversation(&via_print()),
        "the same conversation must survive both carriers; \
         only arrival granularity may differ (§12 whitelist)"
    );
}

/// Spelled out, so a failure says *what* the conversation should be rather
/// than only that two vectors differ.
#[test]
fn the_shared_conversation_is_the_scenario_as_written() {
    assert_eq!(
        conversation(&via_print()),
        vec![
            format!("user: {}", SCENARIO.prompt),
            format!("assistant: {}", SCENARIO.commentary),
            format!("tool#0: {}", SCENARIO.tool_name),
            format!("result#0: {}", SCENARIO.tool_output),
            format!("assistant: {}", SCENARIO.reply),
        ],
    );
}

/// A tool result must link to the call on both sides. An unlinked result is a
/// tool card stuck without output — the P3 bug this pairing guards.
#[test]
fn the_tool_result_links_to_its_call_on_both_sides() {
    for (label, observations) in [("print", via_print()), ("transcript", via_transcript())] {
        let lines = conversation(&observations);
        assert!(
            lines.iter().any(|line| line.starts_with("result#0:")),
            "{label}: the result must attach to tool#0, got {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("result#unlinked")),
            "{label}: no result may float free of its call"
        );
    }
}
