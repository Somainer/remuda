//! c-ctxusage RC1: a Claude TUI transcript (claude-pty / promoted shell-pty)
//! drives the SAME `TranscriptMapper` the live pumps use. It must emit one
//! `Usage` observation per finished assistant message group, with Known cache
//! buckets taken from the group's last `message.usage` (never a sum).

use remuda_driver::TranscriptMapper;
use remuda_protocol::{
    DriverKind, HostId, Id, InstanceId, Knowledge, ObservationPayload, RunId, U64,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../remuda-journal/tests/fixtures/effort-21289")
}

fn mapper() -> TranscriptMapper {
    TranscriptMapper::new(
        DriverKind::ShellPty,
        InstanceId::new(),
        RunId::new(),
        Id::new("obj").expect("journal id"),
        HostId::new(),
        "effort-session".into(),
        "2.1.289".into(),
    )
}

fn known(value: &Knowledge<U64>) -> u64 {
    match value {
        Knowledge::Known { value } => value.0,
        other => panic!("cache/token bucket must be Known, got {other:?}"),
    }
}

/// Expected final counters for one message, derived independently from the raw
/// fixture (the test never reads the mapper for its expectations): the LAST
/// non-sidechain assistant record carrying each `message.id`, in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpectedUsage {
    input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
}

fn fixture_files() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(fixture_dir())
        .expect("fixture dir")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();
    paths.sort();
    paths
}

/// Independently parse every fixture file: last `message.usage` seen per
/// assistant message id. Cache write = the 5m + 1h split (2.1.289 dialect).
fn expected_by_message() -> BTreeMap<String, ExpectedUsage> {
    let mut expected: BTreeMap<String, ExpectedUsage> = BTreeMap::new();
    for path in fixture_files() {
        let body = std::fs::read_to_string(&path).expect("read fixture");
        for line in body.lines().filter(|line| !line.trim().is_empty()) {
            let record: Value = serde_json::from_str(line).expect("fixture line is json");
            if record.get("type").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            if record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let Some(message) = record.get("message") else {
                continue;
            };
            let Some(id) = message.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(usage) = message.get("usage") else {
                continue;
            };
            let get = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
            let split_5m = usage
                .pointer("/cache_creation/ephemeral_5m_input_tokens")
                .and_then(Value::as_u64);
            let split_1h = usage
                .pointer("/cache_creation/ephemeral_1h_input_tokens")
                .and_then(Value::as_u64);
            let cache_write = match (split_5m, split_1h) {
                (Some(a), Some(b)) => a + b,
                _ => get("cache_creation_input_tokens"),
            };
            expected.insert(
                id.to_owned(),
                ExpectedUsage {
                    input: get("input_tokens"),
                    cache_read: get("cache_read_input_tokens"),
                    cache_write,
                    output: get("output_tokens"),
                },
            );
        }
    }
    expected
}

/// Replay every fixture record through one mapper EXACTLY as the production
/// pump does — including the `flush()` after each file (the call whose
/// observations used to be discarded). Returns ALL usage payloads in emission
/// order (a message may publish provisional then revised snapshots; the last
/// one for an id is its final state).
fn replay_all_usage() -> Vec<remuda_protocol::UsagePayload> {
    let mut mapper = mapper();
    let mut emitted = Vec::new();
    let mut collect = |observations: Vec<remuda_protocol::Observation>| {
        emitted.extend(observations.into_iter().filter_map(|obs| match obs.body {
            ObservationPayload::Usage(payload) => Some(*payload),
            _ => None,
        }));
    };
    for path in fixture_files() {
        let body = std::fs::read_to_string(&path).expect("read fixture");
        for line in body.lines().filter(|line| !line.trim().is_empty()) {
            collect(mapper.map_line(line).expect("a real 2.1.289 record maps"));
        }
        // Poll boundary — these observations are real and must be kept.
        collect(mapper.flush().expect("flush"));
    }
    emitted
}

/// Final payload per message id (last emission wins), with per-id revision count.
fn final_by_message(
    emitted: &[remuda_protocol::UsagePayload],
) -> (
    BTreeMap<String, remuda_protocol::UsagePayload>,
    BTreeMap<String, usize>,
) {
    let mut final_payload: BTreeMap<String, remuda_protocol::UsagePayload> = BTreeMap::new();
    let mut revisions: BTreeMap<String, usize> = BTreeMap::new();
    for payload in emitted {
        if payload.scope != remuda_protocol::UsageScope::Turn {
            continue;
        }
        *revisions.entry(payload.scope_id.clone()).or_insert(0) += 1;
        final_payload.insert(payload.scope_id.clone(), payload.clone());
    }
    (final_payload, revisions)
}

#[test]
fn one_usage_observation_per_finished_assistant_message_with_known_cache_buckets() {
    let emitted = replay_all_usage();
    assert!(
        emitted.len() >= 24,
        "flush() observations are kept: at least the 24 final snapshots, got {}",
        emitted.len()
    );
    let (by_id, _revisions) = final_by_message(&emitted);
    let expected = expected_by_message();
    assert_eq!(
        by_id.len(),
        expected.len(),
        "the mapper finalises exactly the messages the fixture carries usage for"
    );
    assert_eq!(by_id.len(), 24, "24 distinct assistant message ids");

    // EVERY message id: all four counters Known and equal to the independently
    // parsed LAST record (input / cache_read / cache_write(5m+1h) / output).
    for (id, want) in &expected {
        let p = by_id
            .get(id)
            .unwrap_or_else(|| panic!("usage emitted for {id}"));
        assert_eq!(p.scope, remuda_protocol::UsageScope::Turn, "{id} scope");
        assert_eq!(known(&p.input_tokens), want.input, "{id} input");
        assert_eq!(
            known(&p.cache_read_tokens),
            want.cache_read,
            "{id} cacheRead"
        );
        assert_eq!(
            known(&p.cache_write_tokens),
            want.cache_write,
            "{id} cacheWrite"
        );
        assert_eq!(known(&p.output_tokens), want.output, "{id} output");
    }
    // No payload for a message the fixture never reported usage for.
    let expected_ids: BTreeSet<&String> = expected.keys().collect();
    for id in by_id.keys() {
        assert!(expected_ids.contains(id), "unexpected usage for {id}");
    }
}

#[test]
fn a_fresh_mapper_replaying_the_transcript_re_emits_but_hub_dedupes() {
    // A re-hydrated session opens a NEW mapper over the same bytes and re-emits
    // the same final snapshots (keyed on the message id). The durable guarantee
    // lives in the Hub and is covered in remuda-hub's usage_store tests
    // (`real_fixture_double_replay_is_durable`); here we assert the driver
    // re-emits the SAME final scope ids so the Hub has something to dedupe.
    let (first, _) = final_by_message(&replay_all_usage());
    let (second, _) = final_by_message(&replay_all_usage());
    assert!(!first.is_empty(), "the replay must produce usage payloads");
    for (id, payload) in &first {
        let has_counter = known_opt(&payload.input_tokens).is_some()
            || known_opt(&payload.output_tokens).is_some()
            || known_opt(&payload.cache_read_tokens).is_some()
            || known_opt(&payload.cache_write_tokens).is_some();
        assert!(has_counter, "{id} carries at least one known counter");
    }
    assert_eq!(
        first.keys().collect::<Vec<_>>(),
        second.keys().collect::<Vec<_>>(),
        "re-hydration re-emits one final snapshot per identical message id"
    );
}

fn known_opt(value: &remuda_protocol::Knowledge<U64>) -> Option<u64> {
    match value {
        remuda_protocol::Knowledge::Known { value } => Some(value.0),
        _ => None,
    }
}

/// c-ctxusage r2 item 3: a message split across two poll boundaries must not
/// emit usage until the group finalises (a stop_reason record). The first
/// poll (blocks with usage but NO stop_reason) emits content only; the second
/// poll (stop_reason + final counters) emits the single usage snapshot with
/// the FINAL numbers — never a frozen partial first snapshot.
#[test]
fn usage_finalises_only_when_the_assistant_group_completes() {
    fn assistant(message_id: &str, usage: serde_json::Value, stop: bool) -> String {
        serde_json::json!({
            "type": "assistant",
            "uuid": format!("{message_id}-rec"),
            "sessionId": "effort-session",
            "version": "2.1.289",
            "requestId": "req-1",
            "message": {
                "id": message_id,
                "role": "assistant",
                "type": "message",
                "model": "claude-sonnet-5",
                "stop_reason": if stop {
                    serde_json::json!("end_turn")
                } else {
                    serde_json::Value::Null
                },
                "content": [{"type": "text", "text": "part"}],
                "usage": usage,
            }
        })
        .to_string()
    }
    fn usage(input: u64, output: u64, cache_read: u64) -> serde_json::Value {
        serde_json::json!({
            "input_tokens": input,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": cache_read,
            "output_tokens": output,
        })
    }

    // Poll 1: first block, provisional counters, NO stop_reason yet.
    let mut mapper = mapper();
    let first = mapper
        .map_line(&assistant("msg-split", usage(100, 5, 5_000), false))
        .expect("map poll 1");
    assert!(
        first
            .iter()
            .all(|o| !matches!(o.body, ObservationPayload::Usage(_))),
        "an in-flight (no stop_reason) block must not emit usage"
    );

    // A poll-boundary flush (what the 75 ms pump calls) must also NOT emit.
    let boundary = mapper.flush().expect("poll flush");
    assert!(
        boundary
            .iter()
            .all(|o| !matches!(o.body, ObservationPayload::Usage(_))),
        "a poll-boundary flush finalises content but not usage"
    );

    // Poll 2: continuation block with final counters + stop_reason.
    let second = mapper
        .map_line(&assistant("msg-split", usage(200, 9, 9_000), false))
        .expect("map poll 2 cont");
    assert!(
        second
            .iter()
            .all(|o| !matches!(o.body, ObservationPayload::Usage(_)))
    );
    let done = mapper
        .map_line(&assistant("msg-split", usage(200, 9, 9_000), true))
        .expect("map stop");
    let forced = mapper.flush().expect("final flush");
    let mut payloads: Vec<_> = done
        .into_iter()
        .chain(forced)
        .filter_map(|o| match o.body {
            ObservationPayload::Usage(p) => Some(*p),
            _ => None,
        })
        .collect();
    assert_eq!(
        payloads.len(),
        1,
        "exactly one usage snapshot for the split message"
    );
    let p = payloads.pop().unwrap();
    assert_eq!(
        known(&p.input_tokens),
        200,
        "final input, not the provisional 100"
    );
    assert_eq!(known(&p.output_tokens), 9, "final output");
    assert_eq!(known(&p.cache_read_tokens), 9_000, "final cache read");
    assert_eq!(
        p.scope_id, "msg-split",
        "the durable key is the bare message id (requestId is not namespaced)"
    );
}

/// c-ctxusage r3 item 1: a record carrying a stop_reason, followed on a LATER
/// poll by another record for the SAME message id with different counters, must
/// finalise on the LATER counters — the poll drains content but retains the
/// group, and the second snapshot is a higher revision, not a frozen first row.
#[test]
fn a_later_same_id_record_revises_the_usage_to_the_final_counters() {
    fn assistant(message_id: &str, ts: &str, input: u64, stop: bool) -> String {
        serde_json::json!({
            "type": "assistant",
            "uuid": format!("{message_id}-{ts}"),
            "sessionId": "effort-session",
            "version": "2.1.289",
            "requestId": "req-1",
            "timestamp": ts,
            "message": {
                "id": message_id,
                "role": "assistant",
                "type": "message",
                "model": "claude-sonnet-5",
                "stop_reason": if stop { serde_json::json!("end_turn") } else { serde_json::Value::Null },
                "content": [{"type": "text", "text": "x"}],
                "usage": {
                    "input_tokens": input,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "output_tokens": input,
                },
            }
        })
        .to_string()
    }
    // One poll = map the batch lines then a flush().
    let poll = |mapper: &mut TranscriptMapper, lines: &[String]| {
        let mut out = Vec::new();
        for line in lines {
            out.extend(mapper.map_line(line).expect("map"));
        }
        out.extend(mapper.flush().expect("flush"));
        out.into_iter()
            .filter_map(|o| match o.body {
                ObservationPayload::Usage(p) => Some(*p),
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    let mut mapper = mapper();
    // Poll 1: stop_reason + provisional counters.
    let first = poll(
        &mut mapper,
        &[assistant("msg-rev", "2026-09-01T00:00:00.000Z", 100, true)],
    );
    assert_eq!(first.len(), 1, "the completed turn publishes a snapshot");
    assert_eq!(first[0].metric_revision, U64(1));
    assert_eq!(known(&first[0].input_tokens), 100);

    // Poll 2: a later same-id record with different counters revises it.
    let second = poll(
        &mut mapper,
        &[assistant("msg-rev", "2026-09-01T00:00:05.000Z", 250, true)],
    );
    assert_eq!(second.len(), 1, "the later record revises the same turn");
    assert_eq!(second[0].scope_id, "msg-rev");
    assert_eq!(second[0].metric_revision, U64(2), "revision bumps");
    assert_eq!(known(&second[0].input_tokens), 250, "FINAL counters win");
}

/// c-ctxusage r3 item 1: usage with NO stop_reason survives a poll and is
/// finalised when a user record supersedes the group (an interrupt must not
/// lose the tokens).
#[test]
fn usage_without_stop_reason_is_finalised_by_a_superseding_user_record() {
    fn assistant(ts: &str, input: u64) -> String {
        serde_json::json!({
            "type": "assistant",
            "uuid": "msg-int-rec",
            "sessionId": "effort-session",
            "version": "2.1.289",
            "timestamp": ts,
            "message": {
                "id": "msg-int",
                "role": "assistant",
                "type": "message",
                "model": "m",
                "stop_reason": serde_json::Value::Null,
                "content": [{"type": "text", "text": "partial"}],
                "usage": {
                    "input_tokens": input,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "output_tokens": 7,
                },
            }
        })
        .to_string()
    }
    let user = serde_json::json!({
        "type": "user",
        "uuid": "user-1",
        "timestamp": "2026-09-01T00:00:10.000Z",
        "message": {"role": "user", "content": [{"type": "text", "text": "go"}]}
    })
    .to_string();

    let mut mapper = mapper();
    for obs in mapper
        .map_line(&assistant("2026-09-01T00:00:00.000Z", 333))
        .expect("map")
    {
        assert!(!matches!(obs.body, ObservationPayload::Usage(_)));
    }
    // The poll boundary drains content but cannot finalise (no stop_reason).
    assert!(
        mapper
            .flush()
            .expect("poll")
            .iter()
            .all(|o| !matches!(o.body, ObservationPayload::Usage(_))),
        "no usage before supersede"
    );
    // A superseding user record finalises the retained counters.
    let finalised: Vec<_> = mapper
        .map_line(&user)
        .expect("user")
        .into_iter()
        .filter_map(|o| match o.body {
            ObservationPayload::Usage(p) => Some(*p),
            _ => None,
        })
        .collect();
    assert_eq!(finalised.len(), 1, "the interrupt still reports usage");
    assert_eq!(known(&finalised[0].input_tokens), 333, "tokens not lost");
    assert_eq!(finalised[0].scope_id, "msg-int");
}

/// c-ctxusage r3 item 1: a different message id finalises the previous group
/// even without a stop_reason.
#[test]
fn a_different_message_finalises_the_pending_group_without_stop_reason() {
    fn assistant(id: &str, input: u64) -> String {
        serde_json::json!({
            "type": "assistant",
            "uuid": format!("{id}-rec"),
            "sessionId": "effort-session",
            "version": "2.1.289",
            "timestamp": "2026-09-01T00:00:00.000Z",
            "message": {
                "id": id,
                "role": "assistant",
                "type": "message",
                "model": "m",
                "stop_reason": serde_json::Value::Null,
                "content": [{"type": "text", "text": "t"}],
                "usage": {
                    "input_tokens": input,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "output_tokens": input,
                },
            }
        })
        .to_string()
    }
    let mut mapper = mapper();
    mapper.map_line(&assistant("msg-a", 111)).expect("a");
    // Poll with no stop: nothing finalises yet.
    assert!(
        mapper
            .flush()
            .expect("poll")
            .iter()
            .all(|o| !matches!(o.body, ObservationPayload::Usage(_)))
    );
    // The next assistant message supersedes and finalises msg-a.
    let usages: Vec<_> = mapper
        .map_line(&assistant("msg-b", 222))
        .expect("b")
        .into_iter()
        .filter_map(|o| match o.body {
            ObservationPayload::Usage(p) => Some(*p),
            _ => None,
        })
        .collect();
    assert_eq!(usages.len(), 1, "msg-a finalised once");
    assert_eq!(usages[0].scope_id, "msg-a");
    assert_eq!(known(&usages[0].input_tokens), 111);
}

/// c-ctxusage r3 item 5: the usage observation carries the transcript record's
/// own timestamp as `native_at`, so a historical replay is excluded from the
/// rate windows by the Hub.
#[test]
fn usage_observation_carries_the_record_timestamp_as_native_at() {
    let line = serde_json::json!({
        "type": "assistant",
        "uuid": "msg-ts-rec",
        "sessionId": "effort-session",
        "version": "2.1.289",
        "timestamp": "2020-01-01T00:00:00.000Z",
        "message": {
            "id": "msg-ts",
            "role": "assistant",
            "type": "message",
            "model": "m",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "t"}],
            "usage": {
                "input_tokens": 10,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0,
                "output_tokens": 5,
            },
        }
    })
    .to_string();
    let mut mapper = mapper();
    mapper.map_line(&line).expect("map");
    let obs = mapper
        .flush()
        .expect("flush")
        .into_iter()
        .find(|o| matches!(o.body, ObservationPayload::Usage(_)))
        .expect("usage observation");
    match &obs.native_at {
        remuda_protocol::Knowledge::Known { value } => {
            assert!(String::from(value.clone()).starts_with("2020-01-01T00:00:00"));
        }
        other => panic!("native_at must be Known from the record, got {other:?}"),
    }
}
