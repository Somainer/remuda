//! End-to-end tests for the D-028 usage adapter.
//!
//! Fixtures live under `tests/fixtures/usage/`:
//! * `claude-transcript-usage.jsonl` — hand-built transcript with two model
//!   messages spread across content-block records and a sidechain record;
//! * `claude-result-cost.json` — a real print `result` frame (derived from
//!   the committed `claude-askuser.jsonl`) carrying `total_cost_usd`, used for
//!   the print cost parity gate;
//! * `codex-interactive-usage.jsonl` — the real 0.154.0 rollout fixture;
//! * `grok-usage.json` / `grok-updates-usage.jsonl` /
//!   `grok-headless-usage.jsonl` — the three grok shapes.

use std::collections::BTreeMap;

use remuda_driver::codex_rollout::{CodexRolloutEvent, parse_rollout_line};
use remuda_driver::usage::claude::{usage_from_result_frame, usage_from_transcript_line};
use remuda_driver::usage::codex::CodexUsage;
use remuda_driver::usage::grok::{
    usage_from_headless_frame, usage_from_update_frame, usage_from_usage_json,
};
use remuda_driver::usage::{Harness, UsageAggregator, UsageSource, to_usage_payload};
use remuda_protocol::{Accounting, Knowledge, UsageScope};
use serde_json::Value;

const CLAUDE_TRANSCRIPT: &str = include_str!("fixtures/usage/claude-transcript-usage.jsonl");
const CLAUDE_RESULT: &str = include_str!("fixtures/usage/claude-result-cost.json");
const CODEX_ROLLOUT: &str = include_str!("fixtures/codex/interactive-0.154.0.jsonl");
const GROK_USAGE_FILE: &str = include_str!("fixtures/usage/grok-usage.json");
const GROK_UPDATES: &str = include_str!("fixtures/usage/grok-updates-usage.jsonl");
const GROK_HEADLESS: &str = include_str!("fixtures/usage/grok-headless-usage.jsonl");

// ── Claude ────────────────────────────────────────────────────────────────

#[test]
fn claude_extracts_one_event_per_content_block_and_dedupes_per_message() {
    let mut agg = UsageAggregator::new();
    let mut raw_events = 0;
    for line in CLAUDE_TRANSCRIPT.lines() {
        if let Some(event) = usage_from_transcript_line(line) {
            raw_events += 1;
            agg.push(&event);
        }
    }
    // Three records for msg_parity_alpha (thinking/text/tool blocks), two for
    // msg_parity_beta (thinking/text); the sidechain record is skipped.
    assert_eq!(raw_events, 5);
    assert_eq!(agg.session().events, 2);
    assert_eq!(agg.turns().len(), 0, "transcript records have no requestId");
    let totals = agg.session();
    assert_eq!(totals.tokens.uncached_input, 18);
    assert_eq!(totals.tokens.output, 261);
    assert_eq!(totals.tokens.cache_read, 40627);
    assert_eq!(totals.tokens.cache_write_1h, 3874);
    assert_eq!(totals.tokens.cache_write_5m, 0);
    assert_eq!(totals.reasoning_tokens, 123);
    assert!(totals.cost_usd().is_some());
}

#[test]
fn claude_session_payload_is_estimated_turn_scoped_snapshots_per_model_call() {
    let mut agg = UsageAggregator::new();
    for line in CLAUDE_TRANSCRIPT.lines() {
        if let Some(event) = usage_from_transcript_line(line) {
            agg.push(&event);
        }
    }
    let payload = to_usage_payload(UsageScope::Session, "sess-parity", 1, agg.session());
    assert_eq!(payload.accounting, Accounting::Estimated);
    assert!(matches!(
        payload.input_accounting,
        remuda_protocol::InputAccounting::Uncached
    ));
    assert_eq!(payload.mode, remuda_protocol::UsageMode::Snapshot);
    let Knowledge::Known { value } = payload.cost else {
        panic!("cost must be known for priced models");
    };
    assert_eq!(value.currency, "USD");
}

/// The print-retirement parity gate: reading the same token counters the print
/// path reads (the `result` frame) through the local table must land within
/// 10 % of the native `total_cost_usd`. The observed gap and its causes are
/// documented in `docs/design/usage-adapter.md` ("Known deviations").
#[test]
fn claude_print_cost_parity_within_10_percent() {
    let frame: Value = serde_json::from_str(CLAUDE_RESULT).unwrap();
    let event =
        usage_from_result_frame(&frame, Some("claude-haiku-4-5-20251001")).expect("result event");
    let estimated = event.cost_usd.expect("priced");
    let reported = event.reported_cost_usd.expect("reported cost present");
    let diff = (estimated - reported).abs() / reported;
    assert!(
        diff <= 0.10,
        "estimate {estimated} vs reported {reported}: {:.1}% gap exceeds the parity gate",
        diff * 100.0
    );
    // Concrete anchor so a price-table change silently drifting past the gate
    // is visible even when still inside 10 %.
    assert!(
        (estimated - 0.0131337).abs() < 1e-6,
        "estimate was {estimated}"
    );
    assert!((reported - 0.0141267).abs() < 1e-9);
    assert!(event.estimated);
}

#[test]
fn claude_transcript_and_result_paths_price_identically_for_the_same_counters() {
    // The synthetic transcript totals exactly match the real result fixture's
    // counters — a transcript-driven estimate must equal the result-driven one.
    let frame: Value = serde_json::from_str(CLAUDE_RESULT).unwrap();
    let result_event = usage_from_result_frame(&frame, Some("claude-haiku-4-5-20251001")).unwrap();
    let mut agg = UsageAggregator::new();
    for line in CLAUDE_TRANSCRIPT.lines() {
        if let Some(event) = usage_from_transcript_line(line) {
            agg.push(&event);
        }
    }
    assert!(
        (agg.session().cost_usd().unwrap() - result_event.cost_usd.unwrap()).abs() < 1e-12,
        "transcript and result estimates must agree for identical counters"
    );
}

// ── Codex ─────────────────────────────────────────────────────────────────

#[test]
fn codex_aggregates_per_response_records_without_the_cumulative_snapshots() {
    let mut extractor = CodexUsage::new();
    let mut agg = UsageAggregator::new();
    let mut per_response = 0;
    let mut snapshots = 0;
    let mut turn_models: BTreeMap<String, String> = BTreeMap::new();
    for line in CODEX_ROLLOUT.lines() {
        let Ok(record) = parse_rollout_line(line) else {
            continue;
        };
        if let CodexRolloutEvent::TurnContext {
            turn_id: Some(turn),
            model: Some(model),
            ..
        } = &record.event
        {
            turn_models.insert(turn.clone(), model.clone());
        }
        if let Some(event) = extractor.on_record(&record) {
            match event.source {
                UsageSource::CodexTokenUsageRecord => {
                    per_response += 1;
                    agg.push(&event);
                }
                UsageSource::CodexTokenCount => snapshots += 1,
                _ => {}
            }
        }
    }
    assert_eq!(per_response, 10);
    assert_eq!(snapshots, 10);
    assert!(!turn_models.is_empty());
    // Every per-response event is priced against the turn's gpt-5.4 card.
    assert_eq!(agg.session().events, 10);
    assert!(agg.session().cost_usd().is_some(), "all responses priced");
    // 10 per-response records × 100 input / 20 output (10 cached each).
    assert_eq!(agg.session().tokens.uncached_input, 900);
    assert_eq!(agg.session().tokens.cache_read, 100);
    assert_eq!(agg.session().tokens.output, 200);
    assert_eq!(agg.ignored_cumulative(), 0, "caller filters snapshots out");
    // Turn totals exist for every turn with usage.
    assert!(agg.turns().len() >= 5);
}

#[test]
fn codex_cumulative_snapshots_do_not_inflate_session_totals() {
    let mut extractor = CodexUsage::new();
    let mut agg = UsageAggregator::new();
    for line in CODEX_ROLLOUT.lines() {
        let Ok(record) = parse_rollout_line(line) else {
            continue;
        };
        if let Some(event) = extractor.on_record(&record) {
            agg.push(&event);
        }
    }
    // With snapshots folded in via push(), they are refused: output stays 200
    // (the per-response sum), never the 200 cumulative-final double count.
    assert_eq!(agg.session().tokens.output, 200);
    assert_eq!(agg.ignored_cumulative(), 10);
}

// ── Grok ──────────────────────────────────────────────────────────────────

#[test]
fn grok_usage_file_yields_priced_event_with_subtracted_cache() {
    let doc: Value = serde_json::from_str(GROK_USAGE_FILE).unwrap();
    let events = usage_from_usage_json(&doc);
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.harness(), Harness::Grok);
    assert_eq!(event.tokens.uncached_input, 25872);
    assert_eq!(event.tokens.output, 35);
    assert_eq!(event.tokens.cache_read, 640);
    assert_eq!(event.reported_cost_usd, Some(0.00910418));
    assert!(event.cost_usd.is_some());
}

#[test]
fn grok_updates_fixture_has_two_priced_turn_completed_frames_and_a_chunk() {
    let mut agg = UsageAggregator::new();
    let mut priced = 0;
    for line in GROK_UPDATES.lines() {
        let frame: Value = serde_json::from_str(line).unwrap();
        if let Some(event) = usage_from_update_frame(&frame) {
            assert_eq!(event.source, UsageSource::GrokUpdates);
            assert!(agg.push(&event));
            priced += 1;
        }
    }
    assert_eq!(priced, 2);
    assert_eq!(agg.session().events, 2);
    // turn 1: 1000 total input - 200 read - 50 write = 750 uncached;
    // turn 2: 2000 - 300 - 0 = 1700 uncached.
    assert_eq!(agg.session().tokens.uncached_input, 2450);
    assert_eq!(agg.session().tokens.cache_read, 500);
    assert_eq!(agg.session().tokens.cache_write_5m, 50);
    assert_eq!(agg.session().tokens.output, 100);
    assert_eq!(agg.turns().len(), 2);
    assert!(agg.session().cost_usd().is_some());
}

#[test]
fn grok_headless_fixture_extracts_usage_and_end_frames() {
    let mut count = 0;
    for line in GROK_HEADLESS.lines() {
        let frame: Value = serde_json::from_str(line).unwrap();
        let events = usage_from_headless_frame(&frame);
        count += events.len();
    }
    assert_eq!(count, 3, "usage frame + end aggregate + end.modelUsage");
}
