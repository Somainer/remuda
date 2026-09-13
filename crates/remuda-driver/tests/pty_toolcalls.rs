//! claude-pty tool-call sync: an interleaved `tool_use` / `tool_result`
//! transcript must reach the 结构 view as linked tool nodes.
//!
//! The native TUI writes its tool blocks only to
//! `~/.claude/projects/<encoded cwd>/<session>.jsonl`, so this is the shape the
//! `claude-pty` transcript pump replays. Source:
//! `tests/fixtures/claude-transcript-tools.jsonl` (see `fixtures/SOURCES.md`).

use remuda_driver::{DriverKind, TranscriptMapper};
use remuda_protocol::{
    ContentBlock, HostId, Id, InstanceId, Knowledge, MessageRole, MutationOperation,
    ObservationPayload, ResultStage, RunId, ToolOutcome,
};
use std::path::{Path, PathBuf};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-transcript-tools.jsonl")
}

fn hydrate() -> Vec<remuda_protocol::Observation> {
    let body = std::fs::read_to_string(fixture()).expect("fixture");
    let mut mapper = TranscriptMapper::new(
        DriverKind::ClaudePty,
        InstanceId::new(),
        RunId::new(),
        Id::new("obj").expect("journal id"),
        HostId::new(),
        "22222222-3333-4444-8555-666666666666".into(),
        "pty".into(),
    );
    body.lines()
        .flat_map(|line| mapper.map_line(line).expect("map transcript line"))
        .collect()
}

/// `kind:operation` per observation, the contract the journal and the web
/// assembler both read.
fn steps(observations: &[remuda_protocol::Observation]) -> Vec<String> {
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
            let operation = match operation {
                MutationOperation::Open => "open",
                MutationOperation::Append => "append",
                MutationOperation::Replace => "replace",
                MutationOperation::Close => "close",
            };
            format!("{kind}:{operation}")
        })
        .collect()
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
}

/// A turn that mixes commentary text with two tools must hydrate every block:
/// dropping the text or either tool is what left the 结构 view empty while the
/// terminal showed real work.
#[test]
fn an_interleaved_tool_turn_hydrates_every_block_in_order() {
    assert_eq!(
        steps(&hydrate()),
        vec![
            // The prompt Claude recorded receiving.
            "message:open",
            // Commentary text, then the Bash call it introduces.
            "message:open",
            "message:close",
            "toolCall:open",
            "toolCall:close",
            "toolResult:open",
            // The second tool is a separate assistant record with its own id.
            "toolCall:open",
            "toolCall:close",
            "toolResult:open",
            // A user text block that is not a tool_result is still a message.
            "message:open",
            // The final reply.
            "message:open",
            "message:close",
        ],
    );
}

/// Every tool_result must attach to the call the UI already drew. A result that
/// mints its own id renders as a tool card stuck without output.
#[test]
fn each_tool_result_links_to_its_own_call() {
    let observations = hydrate();
    let calls: Vec<_> = observations
        .iter()
        .filter_map(|observation| match &observation.body {
            ObservationPayload::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect();
    let results: Vec<_> = observations
        .iter()
        .filter_map(|observation| match &observation.body {
            ObservationPayload::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2, "one result per tool call");
    // open + close per call, so each call id appears twice.
    let bash = &calls[0];
    let read = &calls[2];
    assert_eq!(
        bash.tool_name,
        Knowledge::Known {
            value: "Bash".into()
        }
    );
    assert_eq!(
        read.tool_name,
        Knowledge::Known {
            value: "Read".into()
        }
    );
    assert_eq!(results[0].tool_call_id, bash.tool_call_id);
    assert_eq!(results[1].tool_call_id, read.tool_call_id);
    assert_ne!(
        bash.tool_call_id, read.tool_call_id,
        "two native tool ids must not collapse onto one node"
    );
    for result in &results {
        assert_eq!(result.stage, ResultStage::Final);
        assert_eq!(result.outcome, ToolOutcome::Succeeded);
    }
}

/// A closed tool call carries its arguments. `unknown` input renders a card
/// with no command, which is indistinguishable from a call that never ran.
#[test]
fn a_closed_tool_call_carries_its_input() {
    let observations = hydrate();
    for observation in &observations {
        let ObservationPayload::ToolCall(call) = &observation.body else {
            continue;
        };
        assert!(
            matches!(call.input, Knowledge::Known { .. }),
            "transcript tool calls are complete on arrival: {:?}",
            call.input
        );
    }
}

/// A `user` record whose content array holds text rather than a tool_result is
/// still part of the conversation; dropping it loses interrupt notices.
#[test]
fn a_user_text_block_is_not_dropped_for_lacking_a_tool_result() {
    let observations = hydrate();
    let texts: Vec<String> = observations
        .iter()
        .filter_map(|observation| match &observation.body {
            ObservationPayload::Message(message) if message.role == MessageRole::User => {
                Some(text_of(&message.blocks))
            }
            _ => None,
        })
        .collect();
    assert!(
        texts
            .iter()
            .any(|text| text == "[Request interrupted by user for tool use]"),
        "user text blocks must survive hydration: {texts:?}"
    );
}
