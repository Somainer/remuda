//! c-ctxusage RC1: a Claude TUI transcript (claude-pty / promoted shell-pty)
//! drives the SAME `TranscriptMapper` the live pumps use. It must emit one
//! `Usage` observation per finished assistant message group, with Known cache
//! buckets taken from the group's last `message.usage` (never a sum).

use remuda_driver::TranscriptMapper;
use remuda_protocol::{
    DriverKind, HostId, Id, InstanceId, Knowledge, ObservationPayload, RunId, U64,
};
use std::collections::BTreeMap;
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

/// Map every 2.1.289 fixture record and collect the usage observations keyed by
/// scope id (the assistant message id).
fn usage_by_message() -> BTreeMap<String, remuda_protocol::UsagePayload> {
    let mut mapper = mapper();
    let mut by_id = BTreeMap::new();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(fixture_dir())
        .expect("fixture dir")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();
    paths.sort();
    for path in paths {
        let body = std::fs::read_to_string(&path).expect("read fixture");
        for line in body.lines() {
            if line.trim().is_empty() {
                continue;
            }
            for obs in mapper.map_line(line).expect("a real 2.1.289 record maps") {
                if let ObservationPayload::Usage(payload) = obs.body {
                    // scope_id is "<requestId>:<messageId>" when a requestId
                    // is present, else the bare message id; key assertions on
                    // the message id.
                    let id = payload.scope_id.rsplit(':').next().unwrap().to_owned();
                    assert!(
                        by_id.insert(id, *payload).is_none(),
                        "one usage observation per message id (got a duplicate)"
                    );
                }
            }
        }
        mapper.flush().expect("flush");
    }
    by_id
}

#[test]
fn one_usage_observation_per_finished_assistant_message_with_known_cache_buckets() {
    let by_id = usage_by_message();
    // 24 distinct assistant message ids carry usage in the 2.1.289 fixtures.
    assert_eq!(
        by_id.len(),
        24,
        "one usage snapshot per finished message group"
    );

    // Spot-check the recorded counters for a few messages, taken from the
    // group's LAST record (input / cache_read / cache_write(5m+1h) / output).
    let expect = |id: &str, input: u64, cache_read: u64, cache_write: u64, output: u64| {
        let p = by_id.get(id).unwrap_or_else(|| panic!("usage for {id}"));
        assert_eq!(p.scope, remuda_protocol::UsageScope::Turn, "{id}");
        assert_eq!(known(&p.input_tokens), input, "{id} input");
        assert_eq!(known(&p.cache_read_tokens), cache_read, "{id} cacheRead");
        assert_eq!(known(&p.cache_write_tokens), cache_write, "{id} cacheWrite");
        assert_eq!(known(&p.output_tokens), output, "{id} output");
    };
    expect("msg_recorded_effort4_walk_01", 2, 24376, 5741, 4);
    expect("msg_recorded_effort4_walk_04", 2, 26368, 10654, 130);
    expect("msg_recorded_effort4_d_01", 2, 0, 28233, 4);
    expect("msg_recorded_effort4_g_03", 3, 34403, 181, 4);
}

#[test]
fn a_fresh_mapper_replaying_the_transcript_re_emits_but_hub_dedupes() {
    // A re-hydrated session opens a NEW mapper over the same bytes and re-emits
    // the same usage snapshots (each snapshot is keyed on the message id). The
    // durable guarantee lives in the Hub: project + insert twice and only one
    // row survives. That is covered in remuda-hub's usage_store tests; here we
    // assert the driver re-emits the SAME scope ids so the Hub has something to
    // dedupe (it must not silently drop them).
    let first = usage_by_message();
    let second = usage_by_message();
    assert_eq!(first.len(), second.len());
    assert!(!first.is_empty(), "the replay must produce usage payloads");
    // c-ctxusage r2 item 5: the regression previously passed with zero usage.
    // Every emitted snapshot must carry a non-empty counter set (this is a
    // 2.1.289 transcript; the Hub store projection + dedupe of both passes is
    // asserted in remuda-hub's usage_store tests).
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
        "re-hydration re-emits one snapshot per identical message id"
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
        p.scope_id, "req-1:msg-split",
        "scope id namespaces requestId"
    );
}
