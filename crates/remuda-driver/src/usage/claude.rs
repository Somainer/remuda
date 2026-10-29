//! Claude usage extraction from transcript `assistant` records.
//!
//! The native TUI writes one transcript record **per finished content block**,
//! so a single model message spans 2–7 records that repeat the identical
//! `message.usage`. [`UsageAggregator`](super::UsageAggregator) folds that by
//! `message.id`; the extractor itself stays stateless and emits one event per
//! record so callers can also inspect raw frequency.
//!
//! Counter semantics (Anthropic transcript shape):
//! * `input_tokens` is the **uncached** input;
//! * `cache_read_input_tokens` and `cache_creation_input_tokens` are separate;
//! * cache writes split into `ephemeral_5m_input_tokens` /
//!   `ephemeral_1h_input_tokens` under `usage.cache_creation` when present —
//!   without the split, the whole write count is billed at the 5-minute rate.

use serde_json::Value;

use super::prices::TokenCounters;
use super::{UsageEvent, UsageSource};

/// Extract usage from one decoded transcript record (the full JSON object,
/// not the lifted `message` sub-object).
///
/// Returns `None` for non-assistant records, sidechain records, and assistant
/// records without a `message.usage` object.
#[must_use]
pub fn usage_from_transcript_record(record: &Value) -> Option<UsageEvent> {
    if record.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    // A sidechain record belongs to a sub-agent; the main conversation is the
    // billed surface the usage page renders.
    if record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let message = record.get("message")?;
    let usage = message.get("usage")?;
    if !usage.is_object() {
        return None;
    }
    let message_id = string(message, "id");
    let model = string(message, "model").or_else(|| string(record, "model"));
    // Newer transcripts correlate a model message with its turn via a
    // top-level requestId; older files simply do not carry it.
    let turn_id = string(record, "requestId");
    let tokens = counters_from_anthropic_usage(usage);
    let reasoning = usage["output_tokens_details"]["thinking_tokens"].as_u64();
    Some(UsageEvent::new(
        UsageSource::ClaudeTranscript,
        turn_id,
        message_id,
        model,
        tokens,
        reasoning,
        None,
    ))
}

/// Extract usage from one `message.usage` object (already lifted off the
/// transcript record), keyed by the message/request ids.
///
/// Used when a message group is assembled from multiple block records: the
/// group keeps the last block's usage and the message id, without the full
/// record envelope.
#[must_use]
pub fn usage_from_message_usage(
    message_id: Option<&str>,
    request_id: Option<&str>,
    model: Option<&str>,
    usage: &Value,
) -> Option<UsageEvent> {
    if !usage.is_object() {
        return None;
    }
    let tokens = counters_from_anthropic_usage(usage);
    let reasoning = usage["output_tokens_details"]["thinking_tokens"].as_u64();
    Some(UsageEvent::new(
        UsageSource::ClaudeTranscript,
        request_id.map(str::to_owned),
        message_id.map(str::to_owned),
        model.map(str::to_owned),
        tokens,
        reasoning,
        None,
    ))
}

/// Parse and extract from one raw transcript JSONL line. Unparseable/blank
/// lines and records without usage yield `None`.
#[must_use]
pub fn usage_from_transcript_line(line: &str) -> Option<UsageEvent> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    usage_from_transcript_record(&value)
}

/// Extract usage from a stream-json `result` frame (the print parity path).
///
/// A result frame's `usage` uses the same Anthropic shape but the frame does
/// not repeat the model id; the caller supplies the session's model when it
/// knows it. `total_cost_usd` is retained as the native reported cost for the
/// parity comparison.
#[must_use]
pub fn usage_from_result_frame(frame: &Value, model: Option<&str>) -> Option<UsageEvent> {
    if frame.get("type").and_then(Value::as_str) != Some("result") {
        return None;
    }
    let usage = frame.get("usage")?;
    if !usage.is_object() {
        return None;
    }
    let tokens = counters_from_anthropic_usage(usage);
    let reasoning = usage["output_tokens_details"]["thinking_tokens"].as_u64();
    let reported = frame
        .get("total_cost_usd")
        .and_then(Value::as_f64)
        .filter(|cost| *cost > 0.0);
    Some(UsageEvent::new(
        UsageSource::ClaudeResult,
        None,
        None,
        model.map(str::to_owned),
        tokens,
        reasoning,
        reported,
    ))
}

/// Map an Anthropic `usage` object onto the normalized billing buckets.
///
/// Field PRESENCE is preserved: a bucket the record omits stays `None`
/// (Unknown), it is never a fabricated zero (c-ctxusage r15 item 5). Only a
/// genuinely reported `0` is `Some(0)`. The cache-write TTL split is applied
/// only when both TTL fields are present and reconcile to the reported
/// total; otherwise a reported whole-write count is priced at the cheaper
/// 5-minute tier, and an omitted write bucket leaves both write buckets
/// Unknown.
fn counters_from_anthropic_usage(usage: &Value) -> TokenCounters {
    let known = |key: &str| {
        if let Some(path) = key.strip_prefix("cache_creation.") {
            usage
                .pointer(&format!("/cache_creation/{path}"))
                .and_then(Value::as_u64)
        } else {
            usage.get(key).and_then(Value::as_u64)
        }
    };
    let cache_write = known("cache_creation_input_tokens");
    let write_5m = known("cache_creation.ephemeral_5m_input_tokens");
    let write_1h = known("cache_creation.ephemeral_1h_input_tokens");
    let (write_5m, write_1h) = match (cache_write, write_5m, write_1h) {
        // A trustworthy TTL split prices each tier separately.
        (Some(total), Some(write_5m), Some(write_1h)) if write_5m + write_1h == total => {
            (Some(write_5m), Some(write_1h))
        }
        // A reported whole without a reconciling split is priced at the
        // cheaper tier rather than overstated; the absent tier is a
        // structural zero FOR THIS RECORD.
        (Some(total), _, _) => (Some(total), Some(0)),
        // No write bucket reported: both stay Unknown.
        (None, _, _) => (None, None),
    };
    TokenCounters {
        uncached_input: known("input_tokens"),
        output: known("output_tokens"),
        cache_read: known("cache_read_input_tokens"),
        cache_write_5m: write_5m,
        cache_write_1h: write_1h,
    }
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_full_assistant_usage_with_ttl_split() {
        let record = json!({
            "type": "assistant",
            "requestId": "req_42",
            "message": {
                "id": "msg_abc",
                "model": "claude-haiku-4-5-20251001",
                "usage": {
                    "input_tokens": 18,
                    "output_tokens": 261,
                    "cache_read_input_tokens": 40627,
                    "cache_creation_input_tokens": 3874,
                    "cache_creation": {
                        "ephemeral_5m_input_tokens": 0,
                        "ephemeral_1h_input_tokens": 3874
                    },
                    "output_tokens_details": {"thinking_tokens": 123}
                }
            }
        });
        let event = usage_from_transcript_record(&record).expect("event");
        assert_eq!(event.turn_id.as_deref(), Some("req_42"));
        assert_eq!(event.request_id.as_deref(), Some("msg_abc"));
        assert_eq!(event.model.as_deref(), Some("claude-haiku-4-5-20251001"));
        assert_eq!(event.tokens.uncached_input, Some(18));
        assert_eq!(event.tokens.output, Some(261));
        assert_eq!(event.tokens.cache_read, Some(40627));
        assert_eq!(event.tokens.cache_write_5m, Some(0));
        assert_eq!(event.tokens.cache_write_1h, Some(3874));
        assert_eq!(event.reasoning_tokens, Some(123));
        assert!(event.cost_usd.is_some());
        assert!(event.estimated);
    }

    #[test]
    fn missing_ttl_split_falls_back_to_the_5m_bucket() {
        let record = json!({
            "type": "assistant",
            "message": {
                "id": "msg_x",
                "model": "claude-sonnet-5",
                "usage": {
                    "input_tokens": 1,
                    "output_tokens": 2,
                    "cache_read_input_tokens": 3,
                    "cache_creation_input_tokens": 40
                }
            }
        });
        let event = usage_from_transcript_record(&record).unwrap();
        assert_eq!(event.tokens.cache_write_5m, Some(40));
        assert_eq!(event.tokens.cache_write_1h, Some(0));
    }

    #[test]
    fn omitted_buckets_stay_unknown_never_zero() {
        // A partial record (a later content block, or an older/newer client
        // shape) that reports only output: every other bucket stays Unknown.
        let record = json!({
            "type": "assistant",
            "message": {
                "id": "m",
                "model": "claude-sonnet-5",
                "usage": {"output_tokens": 15}
            }
        });
        let event = usage_from_transcript_record(&record).unwrap();
        assert_eq!(event.tokens.output, Some(15));
        assert_eq!(event.tokens.uncached_input, None);
        assert_eq!(event.tokens.cache_read, None);
        assert_eq!(event.tokens.cache_write_5m, None);
        assert_eq!(event.tokens.cache_write_1h, None);
        // An omitted bucket is never priced for free.
        assert_eq!(event.cost_usd, None);
    }

    #[test]
    fn a_reported_zero_bucket_is_known_zero() {
        let record = json!({
            "type": "assistant",
            "message": {
                "id": "m",
                "model": "claude-sonnet-5",
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0
                }
            }
        });
        let event = usage_from_transcript_record(&record).unwrap();
        assert_eq!(event.tokens.output, Some(0), "reported zero is known");
        assert_eq!(event.tokens.cache_write_5m, Some(0));
        assert!(
            event.cost_usd.is_some(),
            "a fully-reported record is priced"
        );
    }

    #[test]
    fn non_assistant_sidechain_and_usageless_records_yield_nothing() {
        assert!(usage_from_transcript_record(&json!({"type": "user"})).is_none());
        assert!(usage_from_transcript_record(&json!({"type": "mode"})).is_none());
        let sidechain = json!({
            "type": "assistant",
            "isSidechain": true,
            "message": {"id": "m", "model": "x", "usage": {"input_tokens": 1}}
        });
        assert!(usage_from_transcript_record(&sidechain).is_none());
        let no_usage = json!({"type": "assistant", "message": {"id": "m"}});
        assert!(usage_from_transcript_record(&no_usage).is_none());
    }

    #[test]
    fn unknown_model_still_emits_tokens_without_cost() {
        let record = json!({
            "type": "assistant",
            "message": {"id": "m", "model": "internal-x",
                        "usage": {"input_tokens": 10, "output_tokens": 5}}
        });
        let event = usage_from_transcript_record(&record).unwrap();
        assert_eq!(event.tokens.output, Some(5));
        assert_eq!(event.tokens.uncached_input, Some(10));
        assert_eq!(event.cost_usd, None);
    }

    #[test]
    fn result_frame_keeps_the_reported_cost_for_parity() {
        let frame = json!({
            "type": "result",
            "total_cost_usd": 0.0141267,
            "usage": {
                "input_tokens": 18,
                "output_tokens": 261,
                "cache_read_input_tokens": 40627,
                "cache_creation_input_tokens": 3874,
                "cache_creation": {"ephemeral_5m_input_tokens": 0,
                                   "ephemeral_1h_input_tokens": 3874}
            }
        });
        let event = usage_from_result_frame(&frame, Some("claude-haiku-4-5-20251001")).unwrap();
        assert_eq!(event.reported_cost_usd, Some(0.0141267));
        assert!(event.cost_usd.is_some());
        assert_ne!(event.cost_usd, event.reported_cost_usd);
    }
}
