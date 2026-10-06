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
                    let id = payload.scope_id.clone();
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
    assert_eq!(
        first.keys().collect::<Vec<_>>(),
        second.keys().collect::<Vec<_>>(),
        "re-hydration re-emits one snapshot per identical message id"
    );
}
