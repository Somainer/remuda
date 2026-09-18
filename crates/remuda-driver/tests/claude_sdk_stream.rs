//! Stream-assembly units for the `claude-sdk` carrier over recorded NDJSON.
//!
//! These replay fixtures through the same `map_outbound` the driver's reader
//! task calls, so an assertion here is an assertion about production behaviour.
//! The properties under test are `print-replacement.md` §1.7 and §2.5 plus
//! D-028a item 3: one message node across every delta, `Partial` completeness
//! while a block is open and `Structured` once the final block closes it,
//! thought / tool_call / tool_result ordering, and usage from the terminal
//! `result`.

use remuda_driver::StdoutMapper;
use remuda_protocol::{
    Completeness, ContentBlock, ContentStatus, DriverKind, Knowledge, LifecyclePayload,
    MutationOperation, Observation, ObservationPayload, SourceChannel, U64,
};
use serde_json::Value;

const SESSION: &str = "66666666-6666-4666-8666-666666666666";

fn replay(source: &str, driver: DriverKind) -> (StdoutMapper, Vec<Observation>) {
    let mut mapper = StdoutMapper::new(driver, "");
    let mut out = Vec::new();
    for line in source.lines().filter(|l| !l.trim().is_empty()) {
        let value: Value = serde_json::from_str(line).expect("fixture line");
        out.extend(mapper.map(value).expect("map frame"));
    }
    (mapper, out)
}

fn partial_then_final() -> (StdoutMapper, Vec<Observation>) {
    replay(
        include_str!("fixtures/claude-sdk-partial-then-final.jsonl"),
        DriverKind::ClaudeSdk,
    )
}

fn kind_name(obs: &Observation) -> &'static str {
    match &obs.body {
        ObservationPayload::Message(_) => "message",
        ObservationPayload::Thought(_) => "thought",
        ObservationPayload::ToolCall(_) => "tool_call",
        ObservationPayload::ToolResult(_) => "tool_result",
        ObservationPayload::Usage(_) => "usage",
        ObservationPayload::Lifecycle(_) => "lifecycle",
        ObservationPayload::Opaque(_) => "opaque",
        _ => "other",
    }
}

fn text_of(obs: &Observation) -> String {
    match &obs.body {
        ObservationPayload::Message(payload) => payload
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect(),
        _ => String::new(),
    }
}

fn first_index(obs: &[Observation], name: &str) -> usize {
    obs.iter()
        .position(|o| kind_name(o) == name)
        .unwrap_or_else(|| {
            panic!(
                "no {name} in {:?}",
                obs.iter().map(kind_name).collect::<Vec<_>>()
            )
        })
}

/// Every delta of one native message folds onto **one** node, and the final
/// assistant block is authoritative (§1.7, D-028a item 3).
#[test]
fn stream_deltas_share_one_message_node_id() {
    let (_mapper, obs) = partial_then_final();
    let messages: Vec<&Observation> = obs.iter().filter(|o| kind_name(o) == "message").collect();
    assert!(
        messages.len() >= 4,
        "expected three deltas plus a final block, got {}",
        messages.len()
    );

    let node_ids: Vec<_> = messages
        .iter()
        .filter_map(|o| match &o.body {
            ObservationPayload::Message(p) => Some(p.mutation.node_id.clone()),
            _ => None,
        })
        .collect();
    assert!(
        node_ids.windows(2).all(|w| w[0] == w[1]),
        "stream deltas drew more than one card: {node_ids:?}"
    );

    // The id is the message id, and the first mutation opens it.
    if let ObservationPayload::Message(first) = &messages[0].body {
        assert_eq!(first.message_id, first.mutation.node_id);
        assert_eq!(first.mutation.operation, MutationOperation::Open);
        assert_eq!(
            first.native_origin,
            Knowledge::Known {
                value: "msg_recorded_sdk_1".into()
            }
        );
    }

    // Deltas concatenate to the text the final block carries.
    let streamed: String = messages
        .iter()
        .filter(|o| {
            matches!(&o.body, ObservationPayload::Message(p) if p.status == ContentStatus::Streaming)
        })
        .map(|o| text_of(o))
        .collect();
    assert_eq!(streamed, "Reading the notes file now.");
    let final_text = text_of(messages.last().expect("final message"));
    assert_eq!(final_text, "Reading the notes file now.");
}

/// §2.5: a stream delta is `Partial`; the closing block is `Structured`. Control
/// frames are always `Structured`.
#[test]
fn deltas_are_partial_and_the_final_block_is_structured() {
    let (_mapper, obs) = partial_then_final();

    for o in obs.iter().filter(|o| kind_name(o) == "message") {
        let ObservationPayload::Message(payload) = &o.body else {
            unreachable!()
        };
        match payload.status {
            ContentStatus::Streaming => assert_eq!(
                o.completeness,
                Completeness::Partial,
                "an open block must be Partial"
            ),
            _ => assert_eq!(
                o.completeness,
                Completeness::Structured,
                "a closed block must be Structured"
            ),
        }
    }
    // At least one of each, or the assertion above is vacuous.
    let partials = obs
        .iter()
        .filter(|o| o.completeness == Completeness::Partial)
        .count();
    assert!(partials >= 3, "expected the text deltas to be Partial");
    assert!(
        obs.iter()
            .any(|o| kind_name(o) == "message" && o.completeness == Completeness::Structured),
        "expected a Structured final block"
    );

    // Lifecycle, usage and tool_result are control frames, never Partial.
    for o in obs
        .iter()
        .filter(|o| matches!(kind_name(o), "lifecycle" | "usage" | "tool_result"))
    {
        assert_eq!(
            o.completeness,
            Completeness::Structured,
            "{} must be Structured",
            kind_name(o)
        );
    }
}

/// `claude-print` is unchanged by the parameter: it keeps every content
/// observation `Structured`, which is what its consumers were built against.
#[test]
fn print_keeps_structured_completeness_on_the_same_frames() {
    let (_mapper, obs) = replay(
        include_str!("fixtures/claude-sdk-partial-then-final.jsonl"),
        DriverKind::ClaudePrint,
    );
    assert!(
        obs.iter().all(|o| o.completeness != Completeness::Partial),
        "print must not start reporting Partial"
    );
    for o in &obs {
        assert_eq!(o.source.driver_kind, DriverKind::ClaudePrint);
    }
}

/// §2.5 ordering: the thought precedes the message, the tool call precedes its
/// result, and the result is its own node joined by `tool_call_id`.
#[test]
fn thought_tool_call_and_tool_result_arrive_in_order() {
    let (_mapper, obs) = partial_then_final();
    let thought = first_index(&obs, "thought");
    let message = first_index(&obs, "message");
    let call = first_index(&obs, "tool_call");
    let result = first_index(&obs, "tool_result");
    assert!(
        thought < message && message < call && call < result,
        "order was thought={thought} message={message} call={call} result={result}"
    );

    // The tool result is its own node, joined to the call rather than mutating
    // it (`NativeIds::tool_result_*`).
    let call_id = obs
        .iter()
        .find_map(|o| match &o.body {
            ObservationPayload::ToolCall(p) => Some(p.tool_call_id.clone()),
            _ => None,
        })
        .expect("tool call id");
    let (result_node, joined) = obs
        .iter()
        .find_map(|o| match &o.body {
            ObservationPayload::ToolResult(p) => {
                Some((p.mutation.node_id.clone(), p.tool_call_id.clone()))
            }
            _ => None,
        })
        .expect("tool result");
    assert_eq!(joined, call_id, "result must join the call by tool_call_id");
    assert_ne!(result_node, call_id, "result must be its own node");
}

/// The session id comes from `system/init` (§2.2 item 1) and is stamped on
/// every observation, which is what the Node lifts into `nativeRef`.
#[test]
fn session_identity_comes_from_system_init() {
    let (mapper, obs) = partial_then_final();
    assert_eq!(mapper.session_id(), SESSION);

    let started = obs
        .iter()
        .find(|o| match &o.body {
            ObservationPayload::Lifecycle(p) => match p.as_ref() {
                LifecyclePayload::Native(n) => n.native_name == "session",
                _ => false,
            },
            _ => false,
        })
        .expect("session lifecycle");
    let ObservationPayload::Lifecycle(payload) = &started.body else {
        unreachable!()
    };
    let LifecyclePayload::Native(native) = payload.as_ref() else {
        unreachable!()
    };
    assert_eq!(
        native.native_id,
        Knowledge::Known {
            value: SESSION.into()
        }
    );
    assert_eq!(
        native.status,
        Knowledge::Known {
            value: "started".into()
        }
    );

    for o in &obs {
        assert_eq!(o.source.driver_kind, DriverKind::ClaudeSdk);
        assert_eq!(o.source.channel, SourceChannel::Stdout);
    }
}

/// Usage rides the terminal `result` (§2.5, `usage_from_result`); the usage page
/// regresses if this stops arriving on the new carrier (§2.8).
#[test]
fn usage_comes_from_the_terminal_result() {
    let (_mapper, obs) = partial_then_final();
    let usage = obs
        .iter()
        .find_map(|o| match &o.body {
            ObservationPayload::Usage(p) => Some(p.as_ref()),
            _ => None,
        })
        .expect("usage observation");
    assert_eq!(usage.input_tokens, Knowledge::Known { value: U64(11) });
    assert_eq!(usage.output_tokens, Knowledge::Known { value: U64(7) });
    assert_eq!(usage.total_tokens, Knowledge::Known { value: U64(18) });
    match &usage.cost {
        Knowledge::Known { value } => assert_eq!(value.currency, "USD"),
        other => panic!("expected a reported cost, got {other:?}"),
    }

    // The turn lifecycle precedes its usage, and a terminal result affects
    // completion (`map_result`).
    let turn_done = obs
        .iter()
        .position(|o| match &o.body {
            ObservationPayload::Lifecycle(p) => match p.as_ref() {
                LifecyclePayload::Native(n) => {
                    n.status
                        == Knowledge::Known {
                            value: "turn_done".into(),
                        }
                        && n.affects_completion
                }
                _ => false,
            },
            _ => false,
        })
        .expect("terminal turn_done");
    let usage_at = first_index(&obs, "usage");
    assert!(turn_done < usage_at, "usage must follow its result");
}

/// The recorded print fixture still assembles on the sdk carrier: the two
/// carriers share one assembler, so one stream shape cannot regress the other.
#[test]
fn the_recorded_print_stream_still_assembles_on_sdk() {
    let (_mapper, obs) = replay(
        include_str!("fixtures/claude-print-stream.jsonl"),
        DriverKind::ClaudeSdk,
    );
    let messages: Vec<&Observation> = obs.iter().filter(|o| kind_name(o) == "message").collect();
    assert_eq!(messages.len(), 12, "ten deltas, one snapshot, one close");
    let ids: Vec<_> = messages
        .iter()
        .filter_map(|o| match &o.body {
            ObservationPayload::Message(p) => Some(p.mutation.node_id.clone()),
            _ => None,
        })
        .collect();
    assert!(ids.windows(2).all(|w| w[0] == w[1]), "{ids:?}");
    assert!(
        obs.iter().any(|o| o.completeness == Completeness::Partial),
        "sdk must report Partial on this fixture's deltas"
    );
}
