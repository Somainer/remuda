//! Grok usage extraction from native session files.
//!
//! Three native shapes are accepted, all observed in the P6 spike evidence
//! (`docs/design/evidence/grok-signals-1.md`, `grok-headless-streaming-json`
//! fixtures):
//!
//! 1. **`usage.json`** — per-model counters, camelCase
//!    (`inputTokens`, `outputTokens`, `cacheReadInputTokens`,
//!    `cacheCreationInputTokens`, `reasoningTokens`, `modelCalls`,
//!    `costUSD`), either as a single object or as a `{modelId: object}` map.
//!    The spike captured the filename but not the file body, so this shape is
//!    inferred from the headless `end.modelUsage` object and documented as
//!    such in `docs/design/usage-adapter.md`.
//! 2. **`updates.jsonl`** — ACP `session/update` frames whose update or
//!    `_meta` carries a `usage` object. Per protocol §5.5 its `inputTokens`
//!    **includes cache**, so cached buckets are subtracted back out.
//! 3. **headless `usage` / `end` frames** — snake_case
//!    (`input_tokens`, …) where `input_tokens` is **uncached**, plus
//!    `modelUsage` per model and a native `total_cost_usd`.
//!
//! The committed TUI fixtures contain no usage objects (only `_meta.totalTokens`
//! hints); synthetic fixtures under `tests/fixtures/usage/` cover all shapes.

use serde_json::Value;

use super::prices::TokenCounters;
use super::{UsageEvent, UsageSource};

/// Parse one `usage.json` document into zero or more per-model events.
///
/// A document is either a single usage object (model taken from its
/// `modelId`/`model_id` field) or an object mapping model ids to usage objects
/// (the `end.modelUsage` shape). Unknown/unmapped models still produce events
/// with token counters and no cost.
#[must_use]
pub fn usage_from_usage_json(doc: &Value) -> Vec<UsageEvent> {
    let object = match doc {
        Value::Object(map) => map,
        _ => return Vec::new(),
    };
    // Single-object form: it carries counters on itself.
    if has_counter_field(doc) {
        let model = string(doc, "modelId").or_else(|| string(doc, "model_id"));
        return event_if_counters(
            UsageSource::GrokUsageFile,
            None,
            model,
            doc,
            CamelShape::TotalIncludingCache,
            None,
        )
        .into_iter()
        .collect();
    }
    // Per-model map form.
    object
        .iter()
        .filter_map(|(model, value)| {
            value.as_object().and_then(|_| {
                event_if_counters(
                    UsageSource::GrokUsageFile,
                    None,
                    Some(model.clone()),
                    value,
                    CamelShape::TotalIncludingCache,
                    value.get("costUSD").and_then(Value::as_f64),
                )
            })
        })
        .collect()
}

/// Extract usage from one parsed `updates.jsonl` frame (the full JSON object).
///
/// Yields an event only for frames that actually carry a usage object;
/// ordinary `agent_message_chunk` / `turn_completed` frames yield `None`.
#[must_use]
pub fn usage_from_update_frame(frame: &Value) -> Option<UsageEvent> {
    if !matches!(
        frame.get("method").and_then(Value::as_str),
        Some("session/update" | "_x.ai/session/update")
    ) {
        return None;
    }
    let params = frame.get("params")?;
    let update = params.get("update")?;
    // Turn/prompt correlation: update.prompt_id first, then _meta.promptId.
    let turn_id = string(update, "prompt_id")
        .or_else(|| string(&update["_meta"], "promptId"))
        .or_else(|| string(&params["_meta"], "promptId"));
    let session_id = string(params, "sessionId");
    let usage = update
        .get("usage")
        .filter(|value| value.is_object())
        .or_else(|| update.get("_meta")?.get("usage").filter(|v| v.is_object()))
        .or_else(|| params.get("_meta")?.get("usage").filter(|v| v.is_object()))?;
    // ACP usage is per turn; the session id is the best stable key available.
    event_if_counters(
        UsageSource::GrokUpdates,
        turn_id.or(session_id),
        string(usage, "modelId")
            .or_else(|| string(&update["_meta"], "modelId"))
            .or_else(|| string(update, "model")),
        usage,
        CamelShape::TotalIncludingCache,
        None,
    )
}

/// Parse one headless stream frame (`type:"usage"` or `type:"end"`).
///
/// `end` can also carry `modelUsage`; when present, one event per model is
/// returned after the aggregate event.
#[must_use]
pub fn usage_from_headless_frame(frame: &Value) -> Vec<UsageEvent> {
    match frame.get("type").and_then(Value::as_str) {
        Some("usage" | "end") => {}
        _ => return Vec::new(),
    }
    let mut events = Vec::new();
    if let Some(usage) = frame.get("usage").filter(|value| value.is_object()) {
        let model = string(frame, "model").or_else(|| string(usage, "model"));
        if let Some(event) = event_if_counters(
            UsageSource::GrokHeadless,
            string(frame, "requestId"),
            model,
            usage,
            // Headless input_tokens is the uncached count (protocol §5.5).
            CamelShape::SnakeUncached,
            frame.get("total_cost_usd").and_then(Value::as_f64),
        ) {
            events.push(event);
        }
    }
    if let Some(Value::Object(map)) = frame.get("modelUsage") {
        for (model, value) in map {
            if let Some(event) = event_if_counters(
                UsageSource::GrokHeadless,
                string(frame, "requestId"),
                Some(model.clone()),
                value,
                CamelShape::CamelUncached,
                value.get("costUSD").and_then(Value::as_f64),
            ) {
                events.push(event);
            }
        }
    }
    events
}

/// Which field convention a source object uses, and whether its input count
/// includes cache.
#[derive(Debug, Clone, Copy)]
enum CamelShape {
    /// camelCase keys; `inputTokens` includes cached tokens.
    TotalIncludingCache,
    /// camelCase keys; `inputTokens` excludes cached tokens (headless
    /// `modelUsage`, whose value equals the uncached aggregate).
    CamelUncached,
    /// snake_case keys; `input_tokens` excludes cached tokens.
    SnakeUncached,
}

fn camel_counters(usage: &Value, shape: CamelShape) -> TokenCounters {
    let camel = matches!(
        shape,
        CamelShape::TotalIncludingCache | CamelShape::CamelUncached
    );
    let (input_key, output_key, read_key, write_key, reasoning_key) = if camel {
        (
            "inputTokens",
            "outputTokens",
            "cacheReadInputTokens",
            "cacheCreationInputTokens",
            "reasoningTokens",
        )
    } else {
        (
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
            "reasoning_tokens",
        )
    };
    let input = usage[input_key].as_u64().unwrap_or(0);
    let output = usage[output_key].as_u64().unwrap_or(0);
    let cache_read = usage[read_key].as_u64().unwrap_or(0);
    let cache_write = usage[write_key].as_u64().unwrap_or(0);
    let uncached = match shape {
        CamelShape::TotalIncludingCache => {
            input.saturating_sub(cache_read).saturating_sub(cache_write)
        }
        CamelShape::CamelUncached | CamelShape::SnakeUncached => input,
    };
    let _ = reasoning_key;
    TokenCounters {
        uncached_input: uncached,
        output,
        cache_read,
        // Grok exposes no cache-write TTL split; price at the single tier
        // (the table sets both TTL rates equal for grok families).
        cache_write_5m: cache_write,
        cache_write_1h: 0,
    }
}

fn reasoning(usage: &Value, shape: CamelShape) -> Option<u64> {
    let key = match shape {
        CamelShape::TotalIncludingCache | CamelShape::CamelUncached => "reasoningTokens",
        CamelShape::SnakeUncached => "reasoning_tokens",
    };
    usage.get(key).and_then(Value::as_u64)
}

fn has_counter_field(value: &Value) -> bool {
    [
        "inputTokens",
        "input_tokens",
        "outputTokens",
        "output_tokens",
    ]
    .iter()
    .any(|key| value.get(*key).is_some())
}

fn event_if_counters(
    source: UsageSource,
    turn_id: Option<String>,
    model: Option<String>,
    usage: &Value,
    shape: CamelShape,
    reported_cost: Option<f64>,
) -> Option<UsageEvent> {
    if !has_counter_field(usage) {
        return None;
    }
    Some(UsageEvent::new(
        source,
        turn_id,
        None,
        model,
        camel_counters(usage, shape),
        reasoning(usage, shape),
        reported_cost,
    ))
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_json_map_form_emits_one_event_per_model() {
        let doc = json!({
            "grok-4.6-build": {
                "inputTokens": 26512,
                "outputTokens": 35,
                "cacheReadInputTokens": 640,
                "cacheCreationInputTokens": 0,
                "reasoningTokens": 30,
                "modelCalls": 1,
                "costUSD": 0.00910418
            }
        });
        let events = usage_from_usage_json(&doc);
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.model.as_deref(), Some("grok-4.6-build"));
        // input includes cache: 26512 - 640 - 0 = 25872 uncached.
        assert_eq!(event.tokens.uncached_input, 25872);
        assert_eq!(event.tokens.cache_read, 640);
        assert_eq!(event.tokens.output, 35);
        assert_eq!(event.reasoning_tokens, Some(30));
        assert_eq!(event.reported_cost_usd, Some(0.00910418));
        assert!(event.cost_usd.is_some());
        assert_eq!(event.source, UsageSource::GrokUsageFile);
    }

    #[test]
    fn usage_json_single_object_form_uses_its_model_id() {
        let doc = json!({
            "modelId": "grok-4-fast",
            "inputTokens": 100,
            "outputTokens": 10,
            "cacheReadInputTokens": 0,
            "cacheCreationInputTokens": 0
        });
        let events = usage_from_usage_json(&doc);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].model.as_deref(), Some("grok-4-fast"));
    }

    #[test]
    fn update_frame_with_usage_subtracts_cache_from_total_input() {
        let frame = json!({
            "timestamp": 1789326032,
            "method": "_x.ai/session/update",
            "params": {
                "sessionId": "sess-1",
                "update": {
                    "sessionUpdate": "turn_completed",
                    "prompt_id": "prompt-9",
                    "usage": {
                        "modelId": "grok-4.6-build",
                        "inputTokens": 1000,
                        "outputTokens": 40,
                        "cacheReadInputTokens": 200,
                        "cacheCreationInputTokens": 50
                    }
                }
            }
        });
        let event = usage_from_update_frame(&frame).expect("event");
        assert_eq!(event.turn_id.as_deref(), Some("prompt-9"));
        assert_eq!(event.tokens.uncached_input, 750);
        assert_eq!(event.tokens.cache_read, 200);
        assert_eq!(event.tokens.cache_write_5m, 50);
        assert_eq!(event.source, UsageSource::GrokUpdates);
    }

    #[test]
    fn update_frame_without_usage_yields_nothing() {
        let frame = json!({
            "method": "session/update",
            "params": {"sessionId": "s", "update": {"sessionUpdate": "agent_message_chunk",
                      "content": {"type": "text", "text": "ok"}}}
        });
        assert!(usage_from_update_frame(&frame).is_none());
        let not_update = json!({"method": "session/request_permission", "params": {}});
        assert!(usage_from_update_frame(&not_update).is_none());
    }

    #[test]
    fn headless_end_frame_is_uncached_and_expands_model_usage() {
        let frame = json!({
            "type": "end",
            "requestId": "req-1",
            "usage": {
                "input_tokens": 26512,
                "cache_read_input_tokens": 640,
                "cache_creation_input_tokens": 0,
                "output_tokens": 35,
                "reasoning_tokens": 30,
                "total_tokens": 27187
            },
            "total_cost_usd": 0.00910418,
            "modelUsage": {
                "grok-4.6-build": {
                    "inputTokens": 26512,
                    "outputTokens": 35,
                    "cacheReadInputTokens": 640,
                    "cacheCreationInputTokens": 0,
                    "modelCalls": 1,
                    "costUSD": 0.00910418
                }
            }
        });
        let events = usage_from_headless_frame(&frame);
        assert_eq!(events.len(), 2);
        let aggregate = &events[0];
        assert_eq!(aggregate.source, UsageSource::GrokHeadless);
        assert_eq!(aggregate.tokens.uncached_input, 26512);
        assert_eq!(aggregate.reported_cost_usd, Some(0.00910418));
        assert_eq!(events[1].model.as_deref(), Some("grok-4.6-build"));
        // The per-model camelCase object is still treated under headless
        // uncached semantics: its inputTokens matches the uncached aggregate.
        assert_eq!(events[1].tokens.uncached_input, 26512);
    }

    #[test]
    fn unknown_models_keep_tokens_without_cost() {
        let doc = json!({"spike": {"inputTokens": 10, "outputTokens": 2,
            "cacheReadInputTokens": 0, "cacheCreationInputTokens": 0}});
        let event = &usage_from_usage_json(&doc)[0];
        assert_eq!(event.model.as_deref(), Some("spike"));
        assert_eq!(event.cost_usd, None);
    }
}
