//! c-usagefu (a): print/SDK per-call usage.
//!
//! The REAL stream-json session `claude-exit-plan-mode-allow.jsonl` is fed
//! through the same `StdoutMapper` the live `claude-print` / `claude-sdk`
//! readers use. Per-call token counters must come from each `assistant`
//! frame's `message.usage` — never the `result` frame, whose `usage` is summed
//! over every call in the turn. The result keeps only the cumulative reported
//! cost and carries `modelUsage.<model>.contextWindow` as a new optional
//! field. All-zero results emit nothing.

use remuda_driver::claude_print::review::StdoutMapper;
use remuda_protocol::{Knowledge, ObservationPayload, U64, UsageScope};
use serde_json::{Value, json};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../remuda-testing/fixtures/claude/claude-exit-plan-mode-allow.jsonl"
);

fn known(value: &Knowledge<U64>) -> u64 {
    match value {
        Knowledge::Known { value } => value.0,
        other => panic!("a Known counter was required, got {other:?}"),
    }
}

fn usages(mapper: &mut StdoutMapper, frames: &[Value]) -> Vec<remuda_protocol::UsagePayload> {
    let mut out = Vec::new();
    for frame in frames {
        for observation in mapper.map(frame.clone()).expect("frame maps") {
            if let ObservationPayload::Usage(payload) = observation.body {
                out.push(*payload);
            }
        }
    }
    out
}

#[test]
fn per_call_usage_comes_from_assistant_frames_and_last_call_context_is_33718() {
    let frames: Vec<Value> = std::fs::read_to_string(FIXTURE)
        .expect("fixture")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("real stream-json frame"))
        .collect();

    let mut mapper = StdoutMapper::new();
    let payloads = usages(&mut mapper, &frames);

    // Four distinct top-level model messages (msg_replay_02/03/04/05); the
    // two frames repeating msg_replay_02 and msg_replay_04 are content-block
    // duplicates and must NOT produce a second usage payload each.
    let turn: Vec<_> = payloads
        .iter()
        .filter(|p| p.scope == UsageScope::Turn)
        .collect();
    assert_eq!(
        turn.iter().map(|p| p.scope_id.as_str()).collect::<Vec<_>>(),
        vec![
            "msg_replay_02",
            "msg_replay_03",
            "msg_replay_04",
            "msg_replay_05"
        ]
    );

    // The last call (msg_replay_05): 1 uncached input + 33,598 cache read +
    // 119 cache write = 33,718 tokens of context — the number the chip
    // displays. The summed result.usage would be ~134k and is never used.
    let last = turn.last().expect("last turn usage");
    let context = known(&last.input_tokens)
        + known(&last.cache_read_tokens)
        + known(&last.cache_write_tokens);
    assert_eq!(context, 33_718);
    assert_eq!(known(&last.input_tokens), 1);
    assert_eq!(known(&last.cache_read_tokens), 33_598);
    assert_eq!(known(&last.cache_write_tokens), 119);
    assert_eq!(known(&last.output_tokens), 9);

    // The result frame is one token-less SESSION snapshot: cost only, with the
    // native per-model context window attached. Its summed counters are gone.
    let session: Vec<_> = payloads
        .iter()
        .filter(|p| p.scope == UsageScope::Session)
        .collect();
    assert_eq!(session.len(), 1, "one result frame in the fixture");
    let session = session[0];
    assert!(
        matches!(session.input_tokens, Knowledge::Unknown { .. }),
        "result.usage tokens must not be carried: {:?}",
        session.input_tokens
    );
    let cost = match &session.cost {
        Knowledge::Known { value } => value.amount.parse::<f64>().unwrap(),
        other => panic!("reported cost expected, got {other:?}"),
    };
    assert!((cost - 0.4658525).abs() < 1e-9);
    assert_eq!(
        session.context_window.as_ref().map(U64::to_owned),
        Some(U64(1_000_000)),
        "modelUsage.test-model.contextWindow is carried"
    );
}

#[test]
fn all_zero_results_and_assistant_calls_emit_no_usage() {
    let mut mapper = StdoutMapper::new();

    // A result with no cost and no modelUsage produces nothing.
    let zero_result = json!({
        "type": "result",
        "subtype": "success",
        "session_id": "s",
        "total_cost_usd": null,
        "usage": {"input_tokens": 0, "output_tokens": 0}
    });
    assert!(usages(&mut mapper, &[zero_result]).is_empty());

    // An all-zero assistant call (empty/error response shape) likewise
    // produces no usage payload, but its content blocks still map.
    let zero_assistant = json!({
        "type": "assistant",
        "session_id": "s",
        "uuid": "u-0",
        "message": {
            "id": "msg_zero",
            "role": "assistant",
            "content": [{"type": "text", "text": ""}],
            "model": "claude-opus-4-7",
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0
            }
        }
    });
    assert!(usages(&mut mapper, &[zero_assistant]).is_empty());

    // The message id is still remembered as consumed: a repeated frame with
    // the same id must not suddenly emit usage.
    let repeated = json!({
        "type": "assistant",
        "session_id": "s",
        "uuid": "u-1",
        "message": {
            "id": "msg_zero",
            "role": "assistant",
            "content": [{"type": "text", "text": ""}],
            "model": "claude-opus-4-7",
            "usage": {
                "input_tokens": 5,
                "output_tokens": 5,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0
            }
        }
    });
    assert!(
        usages(&mut mapper, &[repeated]).is_empty(),
        "a later block of the same zero call cannot mint usage"
    );
}

#[test]
fn nested_subagent_assistant_frames_emit_no_usage() {
    let mut mapper = StdoutMapper::new();
    let nested = json!({
        "type": "assistant",
        "session_id": "s",
        "uuid": "u-2",
        "parent_tool_use_id": "toolu_subagent",
        "message": {
            "id": "msg_sub",
            "role": "assistant",
            "content": [{"type": "text", "text": "working"}],
            "model": "claude-opus-4-7",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
                "cache_read_input_tokens": 200,
                "cache_creation_input_tokens": 0
            }
        }
    });
    assert!(
        usages(&mut mapper, &[nested]).is_empty(),
        "sub-agent usage stays out of the parent session's rollup"
    );
}
