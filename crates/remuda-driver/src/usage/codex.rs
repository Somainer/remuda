//! Codex usage extraction from parsed rollout records.
//!
//! Codex writes two additive shapes ([`crate::codex_rollout`]):
//! * `token_usage_record.payload.usage` — counters for **one model response**
//!   (additive; what the adapter sums);
//! * `event_msg/token_count.info.total_token_usage` — the **cumulative thread**
//!   snapshot, re-emitted after every response and resent verbatim on resume.
//!   These are tagged [`UsageSource::CodexTokenCount`] and
//!   [`UsageAggregator`](super::UsageAggregator) refuses to add them.
//!
//! The usage payload itself does not name the model; it is reported by
//! `turn_context` for the turn. [`CodexUsage`] keeps that correlation so a
//! tail can simply feed every parsed record in order.
//!
//! Counter semantics: Codex `input_tokens` **includes** cached tokens (its
//! `total_tokens = input_tokens + output_tokens`), so the extractor subtracts
//! `cached_input_tokens` / `cache_write_input_tokens` to recover the uncached
//! bucket. OpenAI's GPT-5 card has no separate cache-creation charge; the
//! price table gives the write bucket the ordinary input rate.

use std::collections::HashMap;

use crate::codex_rollout::{CodexRolloutEvent, CodexRolloutRecord};

use super::prices::TokenCounters;
use super::{UsageEvent, UsageSource};

/// Stateful extractor remembering each turn's effective model from
/// `turn_context` records.
#[derive(Debug, Default)]
pub struct CodexUsage {
    models_by_turn: HashMap<String, String>,
    fallback_model: Option<String>,
}

impl CodexUsage {
    /// Create an empty extractor.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a model used for usage records whose turn never reported one.
    pub fn with_fallback_model(mut self, model: impl Into<String>) -> Self {
        self.fallback_model = Some(model.into());
        self
    }

    /// Feed one parsed rollout record in file order.
    ///
    /// `turn_context` records update the turn→model map and yield no event;
    /// `token_usage_record` yields a per-response event; `token_count` yields
    /// a non-additive cumulative event; everything else yields `None`.
    pub fn on_record(&mut self, record: &CodexRolloutRecord) -> Option<UsageEvent> {
        match &record.event {
            CodexRolloutEvent::TurnContext {
                turn_id: Some(turn),
                model: Some(model),
                ..
            } => {
                self.models_by_turn.insert(turn.clone(), model.clone());
                None
            }
            CodexRolloutEvent::TokenUsage { source_type, data } => {
                if source_type == "token_usage_record" {
                    self.per_response(data)
                } else {
                    self.cumulative_snapshot(data)
                }
            }
            _ => None,
        }
    }

    fn per_response(&self, data: &serde_json::Value) -> Option<UsageEvent> {
        let usage = data.get("usage")?;
        if !usage.is_object() {
            return None;
        }
        let turn_id = data
            .get("turn_id")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let model = turn_id
            .as_ref()
            .and_then(|turn| self.models_by_turn.get(turn))
            .or(self.fallback_model.as_ref())
            .cloned();
        Some(UsageEvent::new(
            UsageSource::CodexTokenUsageRecord,
            turn_id,
            data.get("response_id")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            model,
            counters(usage),
            usage
                .get("reasoning_output_tokens")
                .and_then(|v| v.as_u64()),
            None,
        ))
    }

    fn cumulative_snapshot(&self, data: &serde_json::Value) -> Option<UsageEvent> {
        let usage = data.get("info")?.get("total_token_usage")?;
        if !usage.is_object() {
            return None;
        }
        Some(UsageEvent::new(
            UsageSource::CodexTokenCount,
            None,
            None,
            None,
            counters(usage),
            usage
                .get("reasoning_output_tokens")
                .and_then(|v| v.as_u64()),
            None,
        ))
    }
}

/// Map a Codex usage object onto normalized buckets.
fn counters(usage: &serde_json::Value) -> TokenCounters {
    let input = usage["input_tokens"].as_u64().unwrap_or(0);
    let cached = usage["cached_input_tokens"].as_u64().unwrap_or(0);
    let write = usage["cache_write_input_tokens"].as_u64().unwrap_or(0);
    // input_tokens includes cache; never let a sparse/odd record go negative.
    let uncached = input.saturating_sub(cached).saturating_sub(write);
    TokenCounters {
        uncached_input: uncached,
        output: usage["output_tokens"].as_u64().unwrap_or(0),
        cache_read: cached,
        // Single provider tier; the table prices writes at the input rate.
        cache_write_5m: write,
        cache_write_1h: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_rollout::parse_rollout_line;
    use serde_json::json;

    #[test]
    fn per_response_usage_correlates_the_turn_context_model() {
        let context = parse_rollout_line(
            &json!({
                "type": "turn_context",
                "payload": {"turn_id": "turn-1", "model": "gpt-5.4"}
            })
            .to_string(),
        )
        .unwrap();
        let usage = parse_rollout_line(
            &json!({
                "type": "token_usage_record",
                "payload": {
                    "turn_id": "turn-1",
                    "response_id": "resp_1",
                    "usage": {
                        "input_tokens": 100,
                        "cached_input_tokens": 10,
                        "cache_write_input_tokens": 0,
                        "output_tokens": 20,
                        "reasoning_output_tokens": 5,
                        "total_tokens": 120
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let mut extractor = CodexUsage::new();
        assert!(extractor.on_record(&context).is_none());
        let event = extractor.on_record(&usage).expect("event");
        assert_eq!(event.source, UsageSource::CodexTokenUsageRecord);
        assert_eq!(event.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(event.request_id.as_deref(), Some("resp_1"));
        assert_eq!(event.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(event.tokens.uncached_input, 90);
        assert_eq!(event.tokens.cache_read, 10);
        assert_eq!(event.tokens.output, 20);
        assert_eq!(event.reasoning_tokens, Some(5));
        assert!(event.cost_usd.is_some());
    }

    #[test]
    fn token_count_events_are_tagged_cumulative_and_carry_total_counters() {
        let snapshot = parse_rollout_line(
            &json!({
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 500,
                            "cached_input_tokens": 50,
                            "cache_write_input_tokens": 0,
                            "output_tokens": 100,
                            "reasoning_output_tokens": 25,
                            "total_tokens": 600
                        },
                        "last_token_usage": {}
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let event = CodexUsage::new().on_record(&snapshot).expect("event");
        assert_eq!(event.source, UsageSource::CodexTokenCount);
        assert_eq!(event.tokens.uncached_input, 450);
        assert_eq!(event.tokens.output, 100);
    }

    #[test]
    fn records_without_usage_and_other_events_yield_nothing() {
        let mut extractor = CodexUsage::new().with_fallback_model("gpt-5.4");
        let item = parse_rollout_line(
            &json!({"type": "response_item", "payload": {"type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": "hi"}]}})
            .to_string(),
        )
        .unwrap();
        assert!(extractor.on_record(&item).is_none());
        let empty = parse_rollout_line(
            &json!({"type": "token_usage_record",
                    "payload": {"turn_id": "t", "response_id": "r"}})
            .to_string(),
        )
        .unwrap();
        assert!(extractor.on_record(&empty).is_none());
    }
}
