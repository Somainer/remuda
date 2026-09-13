//! D-028 usage adapter: token/cost evidence for claude, codex, and grok.
//!
//! Today [`crate::claude_print`] is the only emitter of
//! [`remuda_protocol::UsagePayload`], reading the stream-json `result` frame.
//! Once print is retired (D-028 condition 1), usage must come from the native
//! files instead:
//!
//! * **claude** — transcript `assistant` records' `message.usage`
//!   (see [`self::claude`]);
//! * **codex** — rollout `token_usage_record` (see [`self::codex`]);
//! * **grok** — `usage.json` and `updates.jsonl` usage fields
//!   (see [`self::grok`]).
//!
//! Every extractor produces a [`UsageEvent`] with counters normalized to the
//! [`prices::TokenCounters`] buckets. Cost is estimated locally through the
//! versioned [`prices`] table and is always marked estimated; the UI must
//! label it 「估算」. An unknown model yields tokens but no cost.
//!
//! [`UsageAggregator`] folds events into per-turn and per-session totals,
//! handling each source's duplication rules (Claude repeats one message's
//! usage on every content-block record, Codex resends cumulative snapshots,
//! Grok can report the same turn from `turn_completed` and a prompt result).
//! [`to_usage_payload`] maps one totals snapshot to the protocol payload.

pub mod claude;
pub mod codex;
pub mod grok;
pub mod prices;

use std::collections::{BTreeMap, HashSet};

use remuda_protocol::{
    Accounting, Cost, Id, InputAccounting, Knowledge, U64, UsageMode, UsagePayload, UsageScope,
};

use prices::TokenCounters;

/// Which harness produced an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    /// Anthropic Claude CLI.
    Claude,
    /// OpenAI Codex CLI.
    Codex,
    /// xAI Grok CLI.
    Grok,
}

/// Exact native record an event was extracted from, for provenance/debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSource {
    /// Claude transcript `assistant` record (`message.usage`).
    ClaudeTranscript,
    /// Claude stream-json `result` frame. Used by the print-retirement parity
    /// comparison, not by a native session.
    ClaudeResult,
    /// Codex rollout `token_usage_record` — one model response.
    CodexTokenUsageRecord,
    /// Codex rollout `event_msg` / `token_count` — a cumulative snapshot that
    /// must never be added to per-response records.
    CodexTokenCount,
    /// Grok native `usage.json` file.
    GrokUsageFile,
    /// Grok `updates.jsonl` ACP frame carrying a usage object.
    GrokUpdates,
    /// Grok headless `usage`/`end` frame (uncached, snake_case shape).
    GrokHeadless,
}

impl UsageSource {
    /// Native harness for this source.
    #[must_use]
    pub fn harness(self) -> Harness {
        match self {
            UsageSource::ClaudeTranscript | UsageSource::ClaudeResult => Harness::Claude,
            UsageSource::CodexTokenUsageRecord | UsageSource::CodexTokenCount => Harness::Codex,
            UsageSource::GrokUsageFile | UsageSource::GrokUpdates | UsageSource::GrokHeadless => {
                Harness::Grok
            }
        }
    }

    /// Stable machine-readable code used in debug fields.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            UsageSource::ClaudeTranscript => "claude-transcript",
            UsageSource::ClaudeResult => "claude-result",
            UsageSource::CodexTokenUsageRecord => "codex-token-usage-record",
            UsageSource::CodexTokenCount => "codex-token-count",
            UsageSource::GrokUsageFile => "grok-usage-file",
            UsageSource::GrokUpdates => "grok-updates",
            UsageSource::GrokHeadless => "grok-headless",
        }
    }
}

/// One model call's worth of usage, normalized across harnesses.
///
/// Counter fields are `None` when the native record does not emit them — never
/// silently zeroed (protocol §5.5: missing usage is unknown, not 0).
#[derive(Debug, Clone, PartialEq)]
pub struct UsageEvent {
    /// Native turn id, when the source supplies one.
    pub turn_id: Option<String>,
    /// Native request/response/message id, when supplied.
    pub request_id: Option<String>,
    /// Model id exactly as reported (pre price-table normalization).
    pub model: Option<String>,
    /// Normalized token counters.
    pub tokens: TokenCounters,
    /// Reasoning/thinking tokens (already counted inside `tokens.output`);
    /// `None` when the harness does not break them out.
    pub reasoning_tokens: Option<u64>,
    /// Cost estimated from [`prices`]; `None` when the model is not in the
    /// table or token counters are absent.
    pub cost_usd: Option<f64>,
    /// A cost the native source itself reported, retained for parity checks.
    /// Never mapped verbatim by this adapter.
    pub reported_cost_usd: Option<f64>,
    /// Always `true` for table-derived costs; retained on the event so future
    /// reported-cost paths cannot be mistaken for estimates.
    pub estimated: bool,
    /// Exact source record kind.
    pub source: UsageSource,
}

impl UsageEvent {
    /// Build an event from already-extracted counters, pricing against the
    /// table. `model` of `None`, `"unknown"`, or an unmapped id prices as
    /// `None`.
    #[must_use]
    pub fn new(
        source: UsageSource,
        turn_id: Option<String>,
        request_id: Option<String>,
        model: Option<String>,
        tokens: TokenCounters,
        reasoning_tokens: Option<u64>,
        reported_cost_usd: Option<f64>,
    ) -> Self {
        let cost_usd = model
            .as_deref()
            .and_then(|id| prices::estimate_cost(id, &tokens));
        Self {
            turn_id,
            request_id,
            model,
            tokens,
            reasoning_tokens,
            cost_usd,
            reported_cost_usd,
            estimated: true,
            source,
        }
    }

    /// Harness that produced the event.
    #[must_use]
    pub fn harness(&self) -> Harness {
        self.source.harness()
    }
}

/// Folded counters for a turn or a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageTotals {
    /// Number of deduped model calls represented.
    pub events: u64,
    /// Normalized counters summed across calls.
    pub tokens: TokenCounters,
    /// Reasoning/thinking tokens summed when reported.
    pub reasoning_tokens: u64,
    /// Sum of table estimates; `None` when **any** call could not be priced
    /// (unknown model), so the figure is never presented as a complete bill.
    cost_usd: Option<f64>,
}

impl UsageTotals {
    fn add(&mut self, event: &UsageEvent) {
        self.events += 1;
        self.tokens.uncached_input += event.tokens.uncached_input;
        self.tokens.output += event.tokens.output;
        self.tokens.cache_read += event.tokens.cache_read;
        self.tokens.cache_write_5m += event.tokens.cache_write_5m;
        self.tokens.cache_write_1h += event.tokens.cache_write_1h;
        self.reasoning_tokens += event.reasoning_tokens.unwrap_or(0);
        self.cost_usd = match (self.cost_usd, event.cost_usd) {
            (Some(total), Some(cost)) => Some(total + cost),
            (None, Some(cost)) => Some(cost),
            // An unpriced event makes the whole total unpriceable.
            (_, None) => None,
        };
    }

    /// Estimated USD, only when every folded event had a table price.
    #[must_use]
    pub fn cost_usd(&self) -> Option<f64> {
        self.cost_usd
    }
}

/// Folds [`UsageEvent`]s into per-turn and session totals without double
/// counting any native source's repeated records.
///
/// Dedup keys:
/// * Claude — `message.id`; transcript writes one record **per content block**
///   with the same `message.usage` repeated, so only the first record for a
///   message id counts.
/// * Codex per-response records — `response_id`; cumulative `token_count`
///   snapshots are ignored entirely (they are not additive).
/// * Grok — turn/prompt id when present; keyless events are kept individually
///   per protocol §5.5 (no adjacency-based dedup without a stable native id).
#[derive(Debug, Default)]
pub struct UsageAggregator {
    seen: HashSet<String>,
    ignored_cumulative: u64,
    by_turn: BTreeMap<String, UsageTotals>,
    session: UsageTotals,
}

impl UsageAggregator {
    /// Create an empty aggregator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one event. Returns `false` when it was a duplicate or a
    /// non-additive cumulative snapshot that was ignored.
    pub fn push(&mut self, event: &UsageEvent) -> bool {
        if event.source == UsageSource::CodexTokenCount {
            self.ignored_cumulative += 1;
            return false;
        }
        let key = match event.source {
            UsageSource::ClaudeTranscript | UsageSource::ClaudeResult => event
                .request_id
                .clone()
                .map(|id| format!("{}.{id}", event.source.code())),
            UsageSource::CodexTokenUsageRecord => event
                .request_id
                .clone()
                .map(|id| format!("{}.{id}", event.source.code())),
            // Grok: only a native turn/prompt id makes a record dedupable.
            UsageSource::GrokUsageFile | UsageSource::GrokUpdates | UsageSource::GrokHeadless => {
                event
                    .turn_id
                    .clone()
                    .or_else(|| event.request_id.clone())
                    .map(|id| format!("{}.{id}", event.source.code()))
            }
            UsageSource::CodexTokenCount => unreachable!("handled above"),
        };
        if let Some(key) = key
            && !self.seen.insert(key)
        {
            return false;
        }
        if let Some(turn) = event.turn_id.as_deref().filter(|id| !id.is_empty()) {
            self.by_turn.entry(turn.to_owned()).or_default().add(event);
        }
        self.session.add(event);
        true
    }

    /// Number of cumulative snapshot records refused by [`Self::push`].
    #[must_use]
    pub fn ignored_cumulative(&self) -> u64 {
        self.ignored_cumulative
    }

    /// Totals for one native turn id.
    #[must_use]
    pub fn turn(&self, turn_id: &str) -> Option<&UsageTotals> {
        self.by_turn.get(turn_id)
    }

    /// All per-turn totals, keyed by native turn id.
    #[must_use]
    pub fn turns(&self) -> &BTreeMap<String, UsageTotals> {
        &self.by_turn
    }

    /// Whole-session totals across every foldable event seen.
    #[must_use]
    pub fn session(&self) -> &UsageTotals {
        &self.session
    }
}

fn known(value: u64) -> Knowledge<U64> {
    Knowledge::Known { value: U64(value) }
}

fn cost_knowledge(totals: &UsageTotals) -> Knowledge<Cost> {
    match totals.cost_usd() {
        Some(cost) => Knowledge::Known {
            value: Cost {
                amount: format!("{cost:.9}"),
                currency: "USD".into(),
            },
        },
        // A model absent from the price table leaves the cost unknown rather
        // than reporting a partial sum as if it were the complete bill.
        None => Knowledge::Unknown {
            reason: "unpriced-model-or-no-usage".into(),
            evidence_event_ids: Vec::new(),
        },
    }
}

/// Map folded totals onto a protocol `UsagePayload` snapshot.
///
/// `revision` is the metric revision for this scope (protocol §5.5: each
/// snapshot replaces the previous one for that scope). Every payload produced
/// here carries [`Accounting::Estimated`] — that field is the protocol's
/// visibility channel for the 「估算」 label. `input_tokens` is normalized to
/// the **uncached** bucket; cache traffic is always broken out separately, so
/// [`InputAccounting::Uncached`] applies even for sources whose native total
/// includes cache (the extraction subtracts it — see each extractor's docs).
///
/// Note for the protocol owner: this relies on `accounting:"estimated"` being
/// surfaced by the UI as a label. If a richer distinction is wanted later
/// (table revision, native reported cost alongside the estimate), it needs a
/// protocol field; this crate intentionally does not invent one.
#[must_use]
pub fn to_usage_payload(
    scope: UsageScope,
    scope_id: impl Into<String>,
    revision: u64,
    totals: &UsageTotals,
) -> UsagePayload {
    let total = totals.tokens.total();
    UsagePayload {
        usage_id: Id::new("obj").expect("usage id"),
        scope,
        scope_id: scope_id.into(),
        mode: UsageMode::Snapshot,
        metric_revision: U64(revision),
        input_tokens: known(totals.tokens.uncached_input),
        input_accounting: InputAccounting::Uncached,
        output_tokens: known(totals.tokens.output),
        reasoning_tokens: known(totals.reasoning_tokens),
        cache_read_tokens: known(totals.tokens.cache_read),
        cache_write_tokens: known(totals.tokens.cache_write()),
        total_tokens: known(total),
        cost: cost_knowledge(totals),
        accounting: Accounting::Estimated,
        native_fields_ref: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(
        source: UsageSource,
        turn: Option<&str>,
        request: Option<&str>,
        model: Option<&str>,
        tokens: TokenCounters,
    ) -> UsageEvent {
        UsageEvent::new(
            source,
            turn.map(str::to_owned),
            request.map(str::to_owned),
            model.map(str::to_owned),
            tokens,
            None,
            None,
        )
    }

    #[test]
    fn claude_message_usage_is_deduped_per_message_id() {
        let tokens = TokenCounters {
            uncached_input: 10,
            output: 20,
            ..TokenCounters::default()
        };
        let mut agg = UsageAggregator::new();
        // Five content-block records repeat the same message usage.
        let mut results = Vec::new();
        for _ in 0..5 {
            results.push(agg.push(&event(
                UsageSource::ClaudeTranscript,
                Some("turn-1"),
                Some("msg_123"),
                Some("claude-haiku-4-5"),
                tokens,
            )));
        }
        // First fold accepted, the rest duplicates.
        assert_eq!(results, vec![true, false, false, false, false]);
        let accepted = agg.session().events;
        assert_eq!(accepted, 1, "repeated content-block usage must count once");
        assert_eq!(agg.turn("turn-1").unwrap().tokens.output, 20);
    }

    #[test]
    fn codex_cumulative_token_count_snapshots_are_never_added() {
        let per_response = TokenCounters {
            uncached_input: 90,
            cache_read: 10,
            output: 20,
            ..TokenCounters::default()
        };
        let cumulative = TokenCounters {
            uncached_input: 900,
            cache_read: 100,
            output: 200,
            ..TokenCounters::default()
        };
        let mut agg = UsageAggregator::new();
        assert!(agg.push(&event(
            UsageSource::CodexTokenUsageRecord,
            Some("t1"),
            Some("resp_1"),
            Some("gpt-5.4"),
            per_response,
        )));
        assert!(!agg.push(&event(
            UsageSource::CodexTokenCount,
            Some("t1"),
            None,
            Some("gpt-5.4"),
            cumulative,
        )));
        assert_eq!(agg.ignored_cumulative(), 1);
        assert_eq!(agg.session().tokens.output, 20);
    }

    #[test]
    fn per_turn_and_session_totals_sum_independently() {
        let mut agg = UsageAggregator::new();
        agg.push(&event(
            UsageSource::CodexTokenUsageRecord,
            Some("t1"),
            Some("r1"),
            Some("gpt-5.4"),
            TokenCounters {
                uncached_input: 100,
                output: 5,
                ..TokenCounters::default()
            },
        ));
        agg.push(&event(
            UsageSource::CodexTokenUsageRecord,
            Some("t2"),
            Some("r2"),
            Some("gpt-5.4"),
            TokenCounters {
                uncached_input: 200,
                output: 7,
                ..TokenCounters::default()
            },
        ));
        assert_eq!(agg.turns().len(), 2);
        assert_eq!(agg.turn("t1").unwrap().tokens.uncached_input, 100);
        assert_eq!(agg.turn("t2").unwrap().tokens.uncached_input, 200);
        assert_eq!(agg.session().tokens.uncached_input, 300);
        assert_eq!(agg.session().events, 2);
        assert!(agg.session().cost_usd().is_some());
    }

    #[test]
    fn one_unpriced_model_makes_the_total_cost_unknown() {
        let mut agg = UsageAggregator::new();
        agg.push(&event(
            UsageSource::ClaudeTranscript,
            Some("t1"),
            Some("m1"),
            Some("claude-sonnet-5"),
            TokenCounters {
                uncached_input: 10,
                output: 10,
                ..TokenCounters::default()
            },
        ));
        agg.push(&event(
            UsageSource::ClaudeTranscript,
            Some("t1"),
            Some("m2"),
            Some("some-internal-model"),
            TokenCounters {
                uncached_input: 10,
                output: 10,
                ..TokenCounters::default()
            },
        ));
        assert_eq!(agg.session().cost_usd(), None);
    }

    #[test]
    fn payload_carries_estimated_accounting_and_normalized_buckets() {
        let mut agg = UsageAggregator::new();
        agg.push(&event(
            UsageSource::CodexTokenUsageRecord,
            Some("turn-a"),
            Some("resp-a"),
            Some("gpt-5.4"),
            TokenCounters {
                uncached_input: 90,
                cache_read: 10,
                output: 20,
                ..TokenCounters::default()
            },
        ));
        let payload = to_usage_payload(UsageScope::Turn, "turn-a", 1, agg.turn("turn-a").unwrap());
        assert_eq!(payload.accounting, Accounting::Estimated);
        assert_eq!(payload.input_accounting, InputAccounting::Uncached);
        assert_eq!(payload.scope, UsageScope::Turn);
        assert_eq!(payload.total_tokens, Knowledge::Known { value: U64(120) });
        let Knowledge::Known { value: cost } = payload.cost else {
            panic!("expected known estimated cost")
        };
        assert_eq!(cost.currency, "USD");
        assert!(cost.amount.parse::<f64>().unwrap() > 0.0);
    }

    #[test]
    fn empty_totals_serialize_cost_as_unknown_not_zero() {
        let payload = to_usage_payload(UsageScope::Session, "s", 1, &UsageTotals::default());
        assert!(matches!(payload.cost, Knowledge::Unknown { .. }));
    }
}
