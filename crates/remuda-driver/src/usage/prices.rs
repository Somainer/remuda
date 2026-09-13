//! Versioned local price table for the D-028 [`UsageAdapter`](super) cost estimate.
//!
//! Every cost produced through this table is an **estimate** and the UI must
//! label it 「估算」 — the harnesses emit token counters, not bills. The table
//! is plain data with an explicit revision and last-updated date; when a
//! provider publishes a new card, bump [`PRICE_TABLE_REVISION`], update
//! [`PRICE_TABLE_UPDATED`], and record the change in `docs/design/usage-adapter.md`.
//!
//! All rates are USD per million tokens (MTok). Cache-write rates distinguish
//! the Anthropic 5-minute / 1-hour TTL tiers; providers that do not bill cache
//! writes separately set their write rate equal to the uncached input rate.

/// Bumped on every price change so journaled observations stay attributable.
pub const PRICE_TABLE_REVISION: u32 = 1;

/// ISO date (YYYY-MM-DD) of the last rate-card refresh.
pub const PRICE_TABLE_UPDATED: &str = "2026-09-14";

/// Whether the card came from a provider listing or is carried over from the
/// previous generation pending a fresh listing. Provisional prices still
/// produce estimates, but callers may surface them with lower confidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceStatus {
    /// Taken from the provider's public price listing on/before
    /// [`PRICE_TABLE_UPDATED`].
    Published,
    /// No separate listing for this model id; the previous generation's card
    /// is used as a stand-in. See the adapter doc's deviation table.
    Provisional,
}

/// One model family's rate card.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    /// Stable family name used in tests and logs.
    pub family: &'static str,
    /// Uncached input tokens.
    pub input_usd_per_mtok: f64,
    /// Output tokens (reasoning/thinking tokens are output tokens).
    pub output_usd_per_mtok: f64,
    /// Cache-read (prompt-cache hit) tokens.
    pub cache_read_usd_per_mtok: f64,
    /// Cache-write tokens with the short (5-minute) TTL.
    pub cache_write_5m_usd_per_mtok: f64,
    /// Cache-write tokens with the long (1-hour) TTL.
    pub cache_write_1h_usd_per_mtok: f64,
    /// Provenance of the card.
    pub status: PriceStatus,
}

impl ModelPrice {
    /// Estimate the USD cost of `tokens` against this card.
    #[must_use]
    pub fn estimate(&self, tokens: &TokenCounters) -> f64 {
        (tokens.uncached_input as f64 * self.input_usd_per_mtok
            + tokens.output as f64 * self.output_usd_per_mtok
            + tokens.cache_read as f64 * self.cache_read_usd_per_mtok
            + tokens.cache_write_5m as f64 * self.cache_write_5m_usd_per_mtok
            + tokens.cache_write_1h as f64 * self.cache_write_1h_usd_per_mtok)
            / 1_000_000.0
    }
}

/// Token counters normalized to the four billing buckets plus the write TTL
/// split. Extractors are responsible for mapping provider-specific shapes
/// (for example subtracting cache from a total-including-cache input) onto
/// this struct before estimation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCounters {
    /// Fresh (uncached) input tokens.
    pub uncached_input: u64,
    /// Generated output tokens, including reasoning/thinking tokens.
    pub output: u64,
    /// Prompt-cache read tokens.
    pub cache_read: u64,
    /// Cache-write tokens billed at the short-TTL rate.
    pub cache_write_5m: u64,
    /// Cache-write tokens billed at the long-TTL rate.
    pub cache_write_1h: u64,
}

impl TokenCounters {
    /// All cache writes regardless of TTL.
    #[must_use]
    pub fn cache_write(&self) -> u64 {
        self.cache_write_5m + self.cache_write_1h
    }

    /// Input-side tokens across every bucket.
    #[must_use]
    pub fn total_input(&self) -> u64 {
        self.uncached_input + self.cache_read + self.cache_write()
    }

    /// All tokens counted by the table.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.total_input() + self.output
    }
}

/// One table row: a rate card plus the model-id prefixes that select it.
///
/// Prefixes match the way ids actually arrive (`claude-haiku-4-5-20251001`,
/// `gpt-5.4`, `grok-4.6-build`). The matcher strips a trailing dated snapshot
/// suffix (`-YYYYMMDD`) and then chooses the **longest** prefix, so the more
/// specific `grok-4-fast` row wins over `grok-4`.
struct PriceEntry {
    price: ModelPrice,
    prefixes: &'static [&'static str],
}

/// The table itself. Ordered loosely by vendor; matching is prefix-length
/// based, never positional.
static PRICE_TABLE: &[PriceEntry] = &[
    // ── Anthropic Claude ──────────────────────────────────────────────
    PriceEntry {
        price: ModelPrice {
            family: "claude-opus-4",
            input_usd_per_mtok: 15.0,
            output_usd_per_mtok: 75.0,
            cache_read_usd_per_mtok: 1.50,
            cache_write_5m_usd_per_mtok: 18.75,
            cache_write_1h_usd_per_mtok: 30.00,
            status: PriceStatus::Published,
        },
        prefixes: &["claude-opus-4"],
    },
    PriceEntry {
        // No separate Opus 5 listing at the table date; carried over from
        // Opus 4. See usage-adapter.md "Known deviations".
        price: ModelPrice {
            family: "claude-opus-5",
            input_usd_per_mtok: 15.0,
            output_usd_per_mtok: 75.0,
            cache_read_usd_per_mtok: 1.50,
            cache_write_5m_usd_per_mtok: 18.75,
            cache_write_1h_usd_per_mtok: 30.00,
            status: PriceStatus::Provisional,
        },
        prefixes: &["claude-opus-5", "claude-opus-latest"],
    },
    PriceEntry {
        price: ModelPrice {
            family: "claude-sonnet-4",
            input_usd_per_mtok: 3.0,
            output_usd_per_mtok: 15.0,
            cache_read_usd_per_mtok: 0.30,
            cache_write_5m_usd_per_mtok: 3.75,
            cache_write_1h_usd_per_mtok: 6.00,
            status: PriceStatus::Published,
        },
        prefixes: &["claude-sonnet-4"],
    },
    PriceEntry {
        // No separate Sonnet 5 listing at the table date; carried over.
        price: ModelPrice {
            family: "claude-sonnet-5",
            input_usd_per_mtok: 3.0,
            output_usd_per_mtok: 15.0,
            cache_read_usd_per_mtok: 0.30,
            cache_write_5m_usd_per_mtok: 3.75,
            cache_write_1h_usd_per_mtok: 6.00,
            status: PriceStatus::Provisional,
        },
        prefixes: &["claude-sonnet-5", "claude-sonnet-latest"],
    },
    PriceEntry {
        price: ModelPrice {
            family: "claude-haiku-4-5",
            input_usd_per_mtok: 1.0,
            output_usd_per_mtok: 5.0,
            cache_read_usd_per_mtok: 0.10,
            cache_write_5m_usd_per_mtok: 1.25,
            cache_write_1h_usd_per_mtok: 2.00,
            status: PriceStatus::Published,
        },
        prefixes: &["claude-haiku-4-5", "claude-haiku-4.5"],
    },
    // ── OpenAI GPT-5 family (Codex) ───────────────────────────────────
    // PROVISIONAL at revision 1: carried over from the families' published
    // cards, not re-verified in-tree. The print-retirement flip is gated on
    // owner verification (see usage-adapter.md). OpenAI bills no cache-creation
    // tier, so both write rates equal the uncached input rate.
    PriceEntry {
        price: ModelPrice {
            family: "gpt-5",
            input_usd_per_mtok: 1.25,
            output_usd_per_mtok: 10.0,
            cache_read_usd_per_mtok: 0.125,
            // OpenAI does not bill cache creation separately.
            cache_write_5m_usd_per_mtok: 1.25,
            cache_write_1h_usd_per_mtok: 1.25,
            status: PriceStatus::Provisional,
        },
        // Generic row must sit behind the specific mini/nano prefixes; the
        // longest-prefix matcher makes ordering irrelevant, but the grouping
        // documents intent.
        prefixes: &["gpt-5"],
    },
    PriceEntry {
        price: ModelPrice {
            family: "gpt-5-mini",
            input_usd_per_mtok: 0.25,
            output_usd_per_mtok: 2.0,
            cache_read_usd_per_mtok: 0.025,
            cache_write_5m_usd_per_mtok: 0.25,
            cache_write_1h_usd_per_mtok: 0.25,
            status: PriceStatus::Provisional,
        },
        prefixes: &["gpt-5-mini"],
    },
    PriceEntry {
        price: ModelPrice {
            family: "gpt-5-nano",
            input_usd_per_mtok: 0.05,
            output_usd_per_mtok: 0.40,
            cache_read_usd_per_mtok: 0.005,
            cache_write_5m_usd_per_mtok: 0.05,
            cache_write_1h_usd_per_mtok: 0.05,
            status: PriceStatus::Provisional,
        },
        prefixes: &["gpt-5-nano"],
    },
    // ── xAI Grok ──────────────────────────────────────────────────────
    // PROVISIONAL at revision 1 (same caveat as GPT-5). xAI exposes one
    // cached-input rate (~10 % of input) and no separate cache-creation tier,
    // so both write TTL rates equal the input rate.
    PriceEntry {
        price: ModelPrice {
            family: "grok-4-fast",
            input_usd_per_mtok: 0.20,
            output_usd_per_mtok: 0.80,
            // xAI cached input is billed at ~10% of the input rate.
            cache_read_usd_per_mtok: 0.02,
            // No separately billed cache-creation tier.
            cache_write_5m_usd_per_mtok: 0.20,
            cache_write_1h_usd_per_mtok: 0.20,
            status: PriceStatus::Provisional,
        },
        prefixes: &["grok-4-fast", "grok-fast"],
    },
    PriceEntry {
        price: ModelPrice {
            family: "grok-4",
            input_usd_per_mtok: 3.0,
            output_usd_per_mtok: 15.0,
            cache_read_usd_per_mtok: 0.30,
            cache_write_5m_usd_per_mtok: 3.0,
            cache_write_1h_usd_per_mtok: 3.0,
            status: PriceStatus::Provisional,
        },
        // Matches grok-4, grok-4.6-build, …; grok-4-fast is a longer prefix
        // on a different row and wins for those ids.
        prefixes: &["grok-4"],
    },
    PriceEntry {
        price: ModelPrice {
            family: "grok-3-mini",
            input_usd_per_mtok: 0.30,
            output_usd_per_mtok: 0.50,
            cache_read_usd_per_mtok: 0.03,
            cache_write_5m_usd_per_mtok: 0.30,
            cache_write_1h_usd_per_mtok: 0.30,
            status: PriceStatus::Provisional,
        },
        prefixes: &["grok-3-mini"],
    },
    PriceEntry {
        price: ModelPrice {
            family: "grok-3",
            input_usd_per_mtok: 3.0,
            output_usd_per_mtok: 15.0,
            cache_read_usd_per_mtok: 0.30,
            cache_write_5m_usd_per_mtok: 3.0,
            cache_write_1h_usd_per_mtok: 3.0,
            status: PriceStatus::Provisional,
        },
        prefixes: &["grok-3"],
    },
];

/// Normalize a model id for lookup: trim and drop a trailing dated snapshot
/// suffix (`-20251001`). Other vendor suffixes (`grok-4.6-build`, `gpt-5.4`)
/// are prefix-matched untouched. Case folding happens in [`lookup`].
fn normalize(model_id: &str) -> &str {
    let id = model_id.trim();
    let bytes = id.as_bytes();
    if bytes.len() > 9
        && bytes[bytes.len() - 9] == b'-'
        && bytes[bytes.len() - 8..].iter().all(u8::is_ascii_digit)
    {
        return &id[..id.len() - 9];
    }
    id
}

/// Look up the rate card for a model id by longest prefix.
///
/// `None`, empty ids, `"unknown"`, and any id without a matching row (for
/// example Grok's spike build `"spike"`) return `None`; the caller then emits
/// token counters with **no cost** rather than guessing a family.
#[must_use]
pub fn lookup(model_id: &str) -> Option<&'static ModelPrice> {
    let id = normalize(model_id).to_ascii_lowercase();
    if id.is_empty() || id == "unknown" {
        return None;
    }
    PRICE_TABLE
        .iter()
        .filter_map(|entry| {
            entry
                .prefixes
                .iter()
                .find(|prefix| id.starts_with(*prefix))
                .map(|prefix| (prefix.len(), &entry.price))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, price)| price)
}

/// Estimate cost for `model_id`; `None` when the family is not in the table.
#[must_use]
pub fn estimate_cost(model_id: &str, tokens: &TokenCounters) -> Option<f64> {
    lookup(model_id).map(|price| price.estimate(tokens))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dated_snapshot_suffixes_still_resolve() {
        assert_eq!(
            lookup("claude-haiku-4-5-20251001").unwrap().family,
            "claude-haiku-4-5"
        );
        assert_eq!(
            lookup("claude-sonnet-4-20250514").unwrap().family,
            "claude-sonnet-4"
        );
    }

    #[test]
    fn point_and_dotted_versions_match_their_family() {
        assert_eq!(lookup("claude-opus-4-8").unwrap().family, "claude-opus-4");
        assert_eq!(lookup("claude-opus-5").unwrap().family, "claude-opus-5");
        assert_eq!(lookup("gpt-5.4").unwrap().family, "gpt-5");
        assert_eq!(lookup("gpt-5.1-20250807").unwrap().family, "gpt-5");
        assert_eq!(lookup("grok-4.6-build").unwrap().family, "grok-4");
        assert_eq!(lookup("grok-4-fast").unwrap().family, "grok-4-fast");
        assert_eq!(lookup("grok-3-mini-2025").unwrap().family, "grok-3-mini");
    }

    #[test]
    fn matching_is_case_and_whitespace_insensitive() {
        assert!(lookup("  Claude-Sonnet-5 ").is_some());
        assert_eq!(
            lookup("CLAUDE-HAIKU-4-5-20251001").unwrap().family,
            "claude-haiku-4-5"
        );
    }

    #[test]
    fn unknown_and_unmapped_models_have_no_price() {
        assert!(lookup("unknown").is_none());
        assert!(lookup("").is_none());
        assert!(lookup("   ").is_none());
        assert!(lookup("spike").is_none());
        assert!(lookup("agy-local-1").is_none());
        assert!(estimate_cost("unknown", &TokenCounters::default()).is_none());
    }

    #[test]
    fn longest_prefix_wins() {
        // grok-4-fast must not fall through to the generic grok-4 row.
        assert_eq!(lookup("grok-4-fast-preview").unwrap().family, "grok-4-fast");
        assert_eq!(lookup("gpt-5-nano-xyz").unwrap().family, "gpt-5-nano");
    }

    #[test]
    fn cost_is_the_weighted_sum_over_mtok() {
        // 1e6 uncached input + 1e6 output on Haiku = $1 + $5 = $6.
        let price = lookup("claude-haiku-4-5").unwrap();
        let tokens = TokenCounters {
            uncached_input: 1_000_000,
            output: 1_000_000,
            ..TokenCounters::default()
        };
        assert!((price.estimate(&tokens) - 6.0).abs() < 1e-9);

        // TTL split is billed at different rates.
        let tokens = TokenCounters {
            cache_write_5m: 1_000_000,
            ..TokenCounters::default()
        };
        assert!((price.estimate(&tokens) - 1.25).abs() < 1e-9);
        let tokens = TokenCounters {
            cache_write_1h: 1_000_000,
            ..TokenCounters::default()
        };
        assert!((price.estimate(&tokens) - 2.00).abs() < 1e-9);
    }

    #[test]
    fn table_metadata_is_present_and_every_row_resolves_itself() {
        assert_eq!(PRICE_TABLE_REVISION, 1);
        assert!(PRICE_TABLE_UPDATED.starts_with("20"));
        for entry in PRICE_TABLE {
            for prefix in entry.prefixes {
                assert_eq!(lookup(prefix).unwrap().family, entry.price.family);
            }
            assert!(entry.price.input_usd_per_mtok > 0.0);
            assert!(entry.price.output_usd_per_mtok >= entry.price.input_usd_per_mtok);
        }
    }

    #[test]
    fn counters_aggregate_buckets() {
        let t = TokenCounters {
            uncached_input: 1,
            output: 2,
            cache_read: 3,
            cache_write_5m: 4,
            cache_write_1h: 5,
        };
        assert_eq!(t.cache_write(), 9);
        assert_eq!(t.total_input(), 13);
        assert_eq!(t.total(), 15);
    }
}
