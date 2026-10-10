use super::*;
use serde_json::json;
use std::collections::BTreeSet;

fn mapper() -> Mapper {
    Mapper {
        stream: StreamState::default(),
        ids: NativeIds::default(),
        seq: 0,
        instance_id: InstanceId::new(),
        run_id: RunId::new(),
        journal_id: fallback_obj(),
        host_id: fallback_host(),
        session_id: "stream-test-session".into(),
        pin: BinaryPin {
            abs_path: String::new(),
            version: "fixture".into(),
            sha256: dummy_digest(),
        },
        driver_kind: DriverKind::ClaudePrint,
        channel: SourceChannel::Stdout,
        media_stager: None,
    }
}

fn map(mapper: &mut Mapper, value: Value) -> Vec<Observation> {
    map_outbound(mapper, &Outbound::from_value(value)).expect("map fixture frame")
}

fn event(mapper: &mut Mapper, value: Value) -> Vec<Observation> {
    map(mapper, json!({"type": "stream_event", "event": value}))
}

fn nested_event(mapper: &mut Mapper, parent: Option<&str>, value: Value) -> Vec<Observation> {
    map(
        mapper,
        json!({"type": "stream_event", "parent_tool_use_id": parent, "event": value}),
    )
}

fn start(mapper: &mut Mapper, native: &str) {
    assert!(
        event(
            mapper,
            json!({"type": "message_start", "message": {"id": native}})
        )
        .is_empty()
    );
}

fn text_start(mapper: &mut Mapper, index: u32) {
    assert!(
        event(
            mapper,
            json!({"type": "content_block_start", "index": index,
        "content_block": {"type": "text", "text": ""}})
        )
        .is_empty()
    );
}

fn text_delta(mapper: &mut Mapper, index: u32, text: &str) -> Vec<Observation> {
    event(
        mapper,
        json!({"type": "content_block_delta", "index": index,
        "delta": {"type": "text_delta", "text": text}}),
    )
}

fn snapshot(mapper: &mut Mapper, native: &str, uuid: &str, content: Value) -> Vec<Observation> {
    map(
        mapper,
        json!({"type": "assistant", "uuid": uuid,
        "message": {"id": native, "content": content}}),
    )
}

fn messages(observations: &[Observation]) -> Vec<&MessagePayload> {
    observations
        .iter()
        .filter_map(|obs| match &obs.body {
            ObservationPayload::Message(payload) => Some(payload.as_ref()),
            _ => None,
        })
        .collect()
}

fn text(payload: &MessagePayload) -> String {
    payload
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

fn assert_revisions(mutations: &[&NodeMutation], operations: &[MutationOperation]) {
    assert_eq!(mutations.len(), operations.len());
    for (index, (mutation, operation)) in mutations.iter().zip(operations).enumerate() {
        assert_eq!(mutation.node_id, mutations[0].node_id);
        assert_eq!(mutation.operation, *operation);
        assert_eq!(mutation.revision, U64(index as u64 + 1));
        assert_eq!(
            mutation.base_revision,
            (index > 0).then_some(U64(index as u64))
        );
    }
}

#[test]
fn recorded_stream_reuses_one_text_node_through_snapshot_before_stop() {
    let mut mapper = mapper();
    let mut observations = Vec::new();
    for line in include_str!("../tests/fixtures/claude-print-stream.jsonl").lines() {
        observations.extend(map(&mut mapper, serde_json::from_str(line).unwrap()));
    }
    let messages = messages(&observations);
    let full_text =
        "流式回复合并成一个消息可以避免频繁的通知打断，让用户一次性看到完整的响应流程。";
    assert_eq!(messages.len(), 12, "ten deltas, one snapshot, one close");
    let mut operations = vec![MutationOperation::Open];
    operations.extend(std::iter::repeat_n(MutationOperation::Append, 9));
    operations.extend([MutationOperation::Replace, MutationOperation::Close]);
    assert_revisions(
        &messages.iter().map(|p| &p.mutation).collect::<Vec<_>>(),
        &operations,
    );
    for payload in &messages {
        assert_eq!(payload.message_id, messages[0].message_id);
        assert_eq!(payload.message_id, payload.mutation.node_id);
        assert_eq!(payload.target_block, Some(0));
        assert_eq!(
            payload.native_origin,
            Knowledge::Known {
                value: "msg_recorded_stream_1".into()
            }
        );
    }
    assert!(
        messages[..10]
            .iter()
            .all(|p| p.status == ContentStatus::Streaming)
    );
    assert_eq!(
        messages[..10].iter().map(|p| text(p)).collect::<String>(),
        full_text
    );
    assert!(
        messages[10..]
            .iter()
            .all(|p| p.status == ContentStatus::Complete)
    );
    assert_eq!(text(messages[10]), full_text);
    assert_eq!(text(messages[11]), full_text);
    let thoughts: Vec<_> = observations
        .iter()
        .filter_map(|obs| match &obs.body {
            ObservationPayload::Thought(payload) => Some(payload.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(
        thoughts.len(),
        2,
        "empty visible thinking is completed only once"
    );
    assert_revisions(
        &thoughts.iter().map(|p| &p.mutation).collect::<Vec<_>>(),
        &[MutationOperation::Open, MutationOperation::Close],
    );
    assert!(
        thoughts
            .iter()
            .all(|p| p.text.as_deref() == Some("") && p.part_index == 0)
    );
}

#[test]
fn thinking_deltas_and_signature_keep_one_visible_thought() {
    let mut mapper = mapper();
    start(&mut mapper, "msg_thought");
    let mut observations = event(
        &mut mapper,
        json!({"type": "content_block_start", "index": 3,
        "content_block": {"type": "thinking", "thinking": ""}}),
    );
    for delta in ["Consider ", "the request."] {
        observations.extend(event(
            &mut mapper,
            json!({"type": "content_block_delta", "index": 3,
            "delta": {"type": "thinking_delta", "thinking": delta}}),
        ));
    }
    observations.extend(event(
        &mut mapper,
        json!({"type": "content_block_delta", "index": 3,
        "delta": {"type": "signature_delta", "signature": "not-visible-text"}}),
    ));
    observations.extend(snapshot(&mut mapper, "msg_thought", "thought-snapshot", json!([
        {"type": "thinking", "thinking": "Consider the request.", "signature": "not-visible-text"}
    ])));
    observations.extend(event(
        &mut mapper,
        json!({"type": "content_block_stop", "index": 3}),
    ));
    observations.extend(event(&mut mapper, json!({"type": "message_stop"})));
    let thoughts: Vec<_> = observations
        .iter()
        .filter_map(|obs| match &obs.body {
            ObservationPayload::Thought(payload) => Some(payload.as_ref()),
            _ => None,
        })
        .collect();
    assert_revisions(
        &thoughts.iter().map(|p| &p.mutation).collect::<Vec<_>>(),
        &[
            MutationOperation::Open,
            MutationOperation::Append,
            MutationOperation::Replace,
            MutationOperation::Close,
        ],
    );
    assert!(
        thoughts
            .iter()
            .all(|p| p.part_index == 3 && p.thought_id == thoughts[0].thought_id)
    );
    assert_eq!(thoughts[0].text.as_deref(), Some("Consider "));
    assert_eq!(thoughts[1].text.as_deref(), Some("the request."));
    assert_eq!(thoughts[3].text.as_deref(), Some("Consider the request."));
    assert_eq!(thoughts[3].status, ContentStatus::Complete);
}

#[test]
fn tool_json_deltas_reconcile_snapshot_and_result_without_reopening_call() {
    let mut mapper = mapper();
    start(&mut mapper, "msg_tool");
    let mut observations = event(
        &mut mapper,
        json!({"type": "content_block_start", "index": 1,
        "content_block": {"type": "tool_use", "id": "toolu_read", "name": "Read", "input": {}}}),
    );
    for delta in ["{\"file_path\":", "\"README.md\"}"] {
        observations.extend(event(
            &mut mapper,
            json!({"type": "content_block_delta", "index": 1,
            "delta": {"type": "input_json_delta", "partial_json": delta}}),
        ));
    }
    observations.extend(snapshot(&mut mapper, "msg_tool", "tool-snapshot", json!([
        {"type": "tool_use", "id": "toolu_read", "name": "Read", "input": {"file_path": "README.md"}}
    ])));
    observations.extend(event(
        &mut mapper,
        json!({"type": "content_block_stop", "index": 1}),
    ));
    observations.extend(event(&mut mapper, json!({"type": "message_stop"})));
    observations.extend(map(&mut mapper, json!({"type": "user", "uuid": "tool-result-frame",
        "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_read", "content": "Project readme", "is_error": false}
        ]}})));
    let calls: Vec<_> = observations
        .iter()
        .filter_map(|obs| match &obs.body {
            ObservationPayload::ToolCall(payload) => Some(payload.as_ref()),
            _ => None,
        })
        .collect();
    assert_revisions(
        &calls.iter().map(|p| &p.mutation).collect::<Vec<_>>(),
        &[
            MutationOperation::Open,
            MutationOperation::Append,
            MutationOperation::Append,
            MutationOperation::Replace,
            MutationOperation::Close,
        ],
    );
    assert!(
        calls
            .iter()
            .all(|p| p.tool_call_id == calls[0].tool_call_id)
    );
    assert!(matches!(calls[1].input, Knowledge::Unknown { .. }));
    assert_eq!(
        calls[1].input_text_delta.as_deref(),
        Some("{\"file_path\":")
    );
    assert_eq!(calls[2].input_text_delta.as_deref(), Some("\"README.md\"}"));
    assert_eq!(
        calls[4].input,
        Knowledge::Known {
            value: json!({"file_path": "README.md"})
        }
    );
    let results: Vec<_> = observations
        .iter()
        .filter_map(|obs| match &obs.body {
            ObservationPayload::ToolResult(payload) => Some(payload.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1);
    // A tool_result is its own node (joined to the call via `tool_call_id`)
    // and opens exactly once — the live stream and transcript replay share
    // this sequence (promotion parity, D-025/D-028).
    assert_eq!(results[0].tool_call_id, calls[0].tool_call_id);
    assert_ne!(results[0].mutation.node_id, calls[0].mutation.node_id);
    assert_eq!(results[0].mutation.operation, MutationOperation::Open);
    assert_eq!(results[0].mutation.revision, U64(1));
    assert_eq!(results[0].outcome, ToolOutcome::Succeeded);
}

#[test]
fn successive_singleton_text_snapshots_preserve_both_native_block_indices() {
    let mut mapper = mapper();
    start(&mut mapper, "msg_multi_block");
    let mut observations = Vec::new();
    for (index, content) in [(0, "First block."), (1, "Second block.")] {
        text_start(&mut mapper, index);
        observations.extend(text_delta(&mut mapper, index, content));
        observations.extend(snapshot(
            &mut mapper,
            "msg_multi_block",
            &format!("snapshot-{index}"),
            json!([{"type": "text", "text": content}]),
        ));
        observations.extend(event(
            &mut mapper,
            json!({"type": "content_block_stop", "index": index}),
        ));
    }
    observations.extend(event(&mut mapper, json!({"type": "message_stop"})));
    let messages = messages(&observations);
    assert_eq!(messages.len(), 6);
    assert_ne!(messages[0].message_id, messages[3].message_id);
    for (group, content) in messages
        .chunks_exact(3)
        .zip(["First block.", "Second block."])
    {
        assert_revisions(
            &group.iter().map(|p| &p.mutation).collect::<Vec<_>>(),
            &[
                MutationOperation::Open,
                MutationOperation::Replace,
                MutationOperation::Close,
            ],
        );
        assert!(group.iter().all(|p| text(p) == content));
    }
}

#[test]
fn subsequent_turns_do_not_append_to_a_previous_message() {
    let mut mapper = mapper();
    let mut observations = Vec::new();
    for native in ["msg_turn_one", "msg_turn_two"] {
        start(&mut mapper, native);
        text_start(&mut mapper, 0);
        observations.extend(text_delta(&mut mapper, 0, "Same text"));
        observations.extend(event(&mut mapper, json!({"type": "message_stop"})));
    }
    let messages = messages(&observations);
    assert_eq!(messages.len(), 4);
    assert_ne!(messages[0].message_id, messages[2].message_id);
    for group in messages.chunks_exact(2) {
        assert_revisions(
            &group.iter().map(|p| &p.mutation).collect::<Vec<_>>(),
            &[MutationOperation::Open, MutationOperation::Close],
        );
        assert_eq!(group[1].status, ContentStatus::Complete);
        assert_eq!(text(group[1]), "Same text");
    }
}

#[test]
fn nested_parent_streams_do_not_steal_each_others_active_message() {
    let mut mapper = mapper();
    let mut observations = Vec::new();
    for (parent, native, initial) in [
        (None, "msg_parent", "Root"),
        (Some("toolu_parent"), "msg_child", "Child"),
    ] {
        observations.extend(nested_event(
            &mut mapper,
            parent,
            json!({"type": "message_start", "message": {"id": native}}),
        ));
        observations.extend(nested_event(
            &mut mapper,
            parent,
            json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}}),
        ));
        observations.extend(nested_event(
            &mut mapper,
            parent,
            json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": initial}}),
        ));
    }
    observations.extend(text_delta(&mut mapper, 0, " continues"));
    observations.extend(nested_event(
        &mut mapper,
        Some("toolu_parent"),
        json!({"type": "message_stop"}),
    ));
    observations.extend(event(&mut mapper, json!({"type": "message_stop"})));
    let messages = messages(&observations);
    let node_ids: BTreeSet<_> = messages.iter().map(|p| p.message_id.clone()).collect();
    assert_eq!(node_ids.len(), 2);
    let root: Vec<_> = messages
        .iter()
        .filter(|p| p.parent_tool_call_id.is_none())
        .copied()
        .collect();
    let child: Vec<_> = messages
        .iter()
        .filter(|p| p.parent_tool_call_id.is_some())
        .copied()
        .collect();
    assert_eq!(root.len(), 3);
    assert_eq!(child.len(), 2);
    assert_revisions(
        &root.iter().map(|p| &p.mutation).collect::<Vec<_>>(),
        &[
            MutationOperation::Open,
            MutationOperation::Append,
            MutationOperation::Close,
        ],
    );
    assert_eq!(text(root[2]), "Root continues");
    assert_eq!(text(child[1]), "Child");
    assert_eq!(
        child[1].parent_tool_call_id.as_ref(),
        mapper.ids.tools.get("toolu_parent")
    );
}

#[test]
fn duplicate_snapshot_uuid_does_not_create_or_modify_another_node() {
    let mut mapper = mapper();
    start(&mut mapper, "msg_replay");
    text_start(&mut mapper, 0);
    let mut observations = text_delta(&mut mapper, 0, "Once");
    observations.extend(snapshot(
        &mut mapper,
        "msg_replay",
        "same-frame",
        json!([{"type": "text", "text": "Once"}]),
    ));
    assert!(
        snapshot(
            &mut mapper,
            "msg_replay",
            "same-frame",
            json!([{"type": "text", "text": "Once"}])
        )
        .is_empty()
    );
    observations.extend(event(&mut mapper, json!({"type": "message_stop"})));
    assert_eq!(messages(&observations).len(), 3);
}

#[test]
fn unassociated_delta_is_opaque_instead_of_inventing_a_message_identity() {
    let mut mapper = mapper();
    let observations = text_delta(&mut mapper, 0, "No message_start");
    assert_eq!(observations.len(), 1);
    assert!(matches!(
        observations[0].body,
        ObservationPayload::Opaque(_)
    ));
    assert!(messages(&observations).is_empty());
}

#[test]
fn malformed_partial_tool_json_does_not_become_an_empty_final_input() {
    let mut mapper = mapper();
    start(&mut mapper, "msg_bad_json");
    event(
        &mut mapper,
        json!({"type": "content_block_start", "index": 0,
        "content_block": {"type": "tool_use", "id": "toolu_bad_json", "name": "Read", "input": {}}}),
    );
    event(
        &mut mapper,
        json!({"type": "content_block_delta", "index": 0,
        "delta": {"type": "input_json_delta", "partial_json": "{\"file_path\":"}}),
    );
    let observations = event(
        &mut mapper,
        json!({"type": "content_block_stop", "index": 0}),
    );
    let ObservationPayload::ToolCall(call) = &observations[0].body else {
        panic!("expected tool close")
    };
    assert_eq!(call.mutation.operation, MutationOperation::Close);
    assert!(matches!(call.input, Knowledge::Unknown { .. }));
}

#[test]
fn late_complete_snapshot_replaces_the_closed_node_without_duplicating_text() {
    let mut mapper = mapper();
    start(&mut mapper, "msg_late_snapshot");
    text_start(&mut mapper, 0);
    let mut observations = text_delta(&mut mapper, 0, "Hello");
    observations.extend(text_delta(&mut mapper, 0, " world"));
    observations.extend(event(
        &mut mapper,
        json!({"type": "content_block_stop", "index": 0}),
    ));
    observations.extend(event(&mut mapper, json!({"type": "message_stop"})));
    observations.extend(snapshot(
        &mut mapper,
        "msg_late_snapshot",
        "late-snapshot",
        json!([{"type": "text", "text": "Hello world!"}]),
    ));
    let messages = messages(&observations);
    assert!(
        messages
            .iter()
            .all(|p| p.message_id == messages[0].message_id)
    );
    let mut merged = String::new();
    for (index, payload) in messages.iter().enumerate() {
        assert_eq!(payload.mutation.revision, U64(index as u64 + 1));
        assert_eq!(
            payload.mutation.base_revision,
            (index > 0).then_some(U64(index as u64))
        );
        match payload.mutation.operation {
            MutationOperation::Append => merged.push_str(&text(payload)),
            MutationOperation::Open | MutationOperation::Replace | MutationOperation::Close => {
                merged = text(payload);
            }
        }
    }
    assert_eq!(merged, "Hello world!");
    assert_eq!(messages.last().unwrap().status, ContentStatus::Complete);
    assert_eq!(
        messages
            .iter()
            .filter(|p| p.mutation.operation == MutationOperation::Open)
            .count(),
        1
    );
}

#[test]
fn repeated_tool_snapshot_uses_native_tool_id_across_different_frame_uuids() {
    let mut mapper = mapper();
    let content = json!([
        {"type": "tool_use", "id": "toolu_stable", "name": "Read", "input": {"file_path": "README.md"}}
    ]);
    let mut observations = snapshot(
        &mut mapper,
        "msg_tool_snapshot",
        "first-frame",
        content.clone(),
    );
    observations.extend(snapshot(
        &mut mapper,
        "msg_tool_snapshot",
        "second-frame",
        content,
    ));
    let calls: Vec<_> = observations
        .iter()
        .filter_map(|obs| match &obs.body {
            ObservationPayload::ToolCall(payload) => Some(payload.as_ref()),
            _ => None,
        })
        .collect();
    assert_revisions(
        &calls.iter().map(|p| &p.mutation).collect::<Vec<_>>(),
        &[
            MutationOperation::Open,
            MutationOperation::Close,
            MutationOperation::Replace,
            MutationOperation::Close,
        ],
    );
    assert!(
        calls
            .iter()
            .all(|p| p.tool_call_id == calls[0].tool_call_id)
    );
}

#[test]
fn message_delta_revises_provisional_per_call_usage_to_final() {
    let mut mapper = mapper();

    // message_start carries the provisional in-flight usage; it never emits a
    // usage observation, but its model label is remembered for the delta.
    let start_obs = event(
        &mut mapper,
        json!({"type": "message_start", "message": {
            "id": "msg_r2",
            "model": "claude-opus-4-7",
            "usage": {"input_tokens": 1, "cache_read_input_tokens": 100,
                      "cache_creation_input_tokens": 50, "output_tokens": 8}
        }}),
    );
    assert!(start_obs.is_empty(), "message_start never emits usage");

    // The assistant record emits revision 1 with the provisional output count.
    let assistant = map(
        &mut mapper,
        json!({"type": "assistant", "uuid": "u1", "message": {
            "id": "msg_r2", "role": "assistant",
            "content": [{"type": "text", "text": "done"}],
            "model": "claude-opus-4-7",
            "usage": {"input_tokens": 1, "cache_read_input_tokens": 100,
                      "cache_creation_input_tokens": 50, "output_tokens": 8}
        }}),
    );
    // message_delta revises to the FINAL count at revision 2; input/cache
    // buckets are identical, output grows 8 -> 30.
    let delta = event(
        &mut mapper,
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
            "usage": {"input_tokens": 1, "cache_read_input_tokens": 100,
                      "cache_creation_input_tokens": 50, "output_tokens": 30,
                      "output_tokens_details": {"thinking_tokens": 0}}}),
    );

    let mut usage: Vec<_> = assistant
        .into_iter()
        .chain(delta)
        .filter_map(|obs| match obs.body {
            ObservationPayload::Usage(payload) => Some(payload),
            _ => None,
        })
        .collect();
    assert_eq!(usage.len(), 2, "provisional rev 1 + final rev 2");
    let rev2 = usage.pop().unwrap();
    let rev1 = usage.pop().unwrap();
    for payload in [&rev1, &rev2] {
        assert_eq!(payload.scope, UsageScope::Turn);
        assert_eq!(payload.scope_id, "msg_r2");
    }
    assert_eq!(rev1.metric_revision, U64(1));
    assert_eq!(rev1.output_tokens, Knowledge::Known { value: U64(8) });
    assert_eq!(rev2.metric_revision, U64(2));
    assert_eq!(rev2.output_tokens, Knowledge::Known { value: U64(30) });
    // Context buckets are the same blob on both frames — growth, no decrease,
    // so the Hub accepts the revision.
    assert_eq!(rev2.input_tokens, rev1.input_tokens);
    assert_eq!(rev2.cache_read_tokens, rev1.cache_read_tokens);
    assert_eq!(rev2.cache_write_tokens, rev1.cache_write_tokens);

    // message_stop retires the remembered model.
    let stop = event(&mut mapper, json!({"type": "message_stop"}));
    assert!(stop.is_empty());
    assert!(!mapper.stream.message_models.contains_key("msg_r2"));
}

#[test]
fn message_delta_with_only_output_tokens_overlays_the_revision1_buckets() {
    // c-usagefu r3 item 2: Anthropic's documented message_delta shape carries
    // only the changed usage keys; Bedrock/Vertex/gateway profiles commonly
    // send {"output_tokens": n} with NO input/cache buckets. Building revision
    // 2 from that delta alone zeroed the buckets and the Hub rejected the
    // "decrease", leaving the provisional per-call output count frozen.
    let mut mapper = mapper();
    event(
        &mut mapper,
        json!({"type": "message_start", "message": {
            "id": "msg_gw",
            "model": "claude-opus-4-7",
            "usage": {"input_tokens": 1, "cache_read_input_tokens": 100,
                      "cache_creation_input_tokens": 50, "output_tokens": 8}
        }}),
    );
    let assistant = map(
        &mut mapper,
        json!({"type": "assistant", "uuid": "u1", "message": {
            "id": "msg_gw", "role": "assistant",
            "content": [{"type": "text", "text": "done"}],
            "model": "claude-opus-4-7",
            "usage": {"input_tokens": 1, "cache_read_input_tokens": 100,
                      "cache_creation_input_tokens": 50, "output_tokens": 8}
        }}),
    );
    // Sparse gateway delta: output tokens only.
    let delta = event(
        &mut mapper,
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 30}}),
    );

    let mut usage: Vec<_> = assistant
        .into_iter()
        .chain(delta)
        .filter_map(|obs| match obs.body {
            ObservationPayload::Usage(payload) => Some(payload),
            _ => None,
        })
        .collect();
    assert_eq!(
        usage.len(),
        2,
        "rev 1 still emits; sparse delta still revises"
    );
    let rev2 = usage.pop().unwrap();
    let rev1 = usage.pop().unwrap();
    assert_eq!(rev2.metric_revision, U64(2));
    assert_eq!(rev2.output_tokens, Knowledge::Known { value: U64(30) });
    // The omitted buckets are carried over from revision 1, not read as zero.
    assert_eq!(rev2.input_tokens, rev1.input_tokens);
    assert_eq!(rev2.cache_read_tokens, rev1.cache_read_tokens);
    assert_eq!(rev2.cache_write_tokens, rev1.cache_write_tokens);

    // message_stop also retires the remembered revision-1 blob.
    event(&mut mapper, json!({"type": "message_stop"}));
    assert!(!mapper.stream.message_usage.contains_key("msg_gw"));
}

#[test]
fn nested_message_delta_emits_no_usage() {
    let mut mapper = mapper();
    let start = nested_event(
        &mut mapper,
        Some("toolu_sub"),
        json!({"type": "message_start", "message": {
            "id": "msg_sub", "model": "m",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }}),
    );
    assert!(start.is_empty());
    let delta = nested_event(
        &mut mapper,
        Some("toolu_sub"),
        json!({"type": "message_delta",
            "usage": {"input_tokens": 1, "output_tokens": 99}}),
    );
    assert!(delta.is_empty(), "a sub-agent delta is a sidechain");
}

#[test]
fn pty_transcript_carriers_do_not_emit_ungated_stream_usage() {
    // c-usagefu r2 item 5a: the claude-pty (and shell-pty) hydrator rebuilds
    // transcript records through this same mapper, but its authoritative usage
    // comes from the group-gated flush, not the first assistant frame. Emitting
    // here would double-count every pty transcript.
    for carrier in [DriverKind::ClaudePty, DriverKind::ShellPty] {
        let mut mapper = mapper();
        mapper.driver_kind = carrier;
        let frame = json!({"type": "assistant", "uuid": "u", "message": {
            "id": "msg_pty", "role": "assistant",
            "content": [{"type": "text", "text": "hi"}],
            "model": "claude-opus-4-7",
            "usage": {"input_tokens": 10, "output_tokens": 5,
                      "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
        }});
        let obs = map(&mut mapper, frame);
        assert!(
            obs.iter()
                .all(|o| !matches!(o.body, ObservationPayload::Usage(_))),
            "{carrier:?} must not emit usage on the ungated first frame"
        );
    }

    // The stdout carriers keep the emission.
    for carrier in [DriverKind::ClaudePrint, DriverKind::ClaudeSdk] {
        let mut mapper = mapper();
        mapper.driver_kind = carrier;
        let frame = json!({"type": "assistant", "uuid": "u", "message": {
            "id": "msg_stdout", "role": "assistant",
            "content": [{"type": "text", "text": "hi"}],
            "model": "claude-opus-4-7",
            "usage": {"input_tokens": 10, "output_tokens": 5,
                      "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
        }});
        let obs = map(&mut mapper, frame);
        assert!(
            obs.iter()
                .any(|o| matches!(o.body, ObservationPayload::Usage(_))),
            "{carrier:?} emits per-call usage on the stream channel"
        );
    }
}
