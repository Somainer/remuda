//! Built-in model **capability** table; coordinator design §4.2.
//!
//! Capability is the only one of the three profiling categories the
//! coordinator is allowed to know without asking: family (the rate-limit
//! bucket), class tier, context window, effort levels, feature support.
//! **Supply** (quotas, priority, concurrency) is never carried here — only
//! users declare that, in [`crate::supply`].
//!
//! Discipline mirrors `remuda-driver`'s price table: static data with an
//! explicit [`CATALOG_REVISION`] and a [`CapabilityStatus`] provenance mark.
//! Gateway models the table cannot know (`<gateway-model-A>[1m]`, family
//! `es1`) fall back to the per-profile declared `family`/`role` fields; an
//! unknown id is not an admission error on its own.

use remuda_driver::usage::prices::{self, PriceStatus};
use remuda_protocol::ModelClass;

/// Bumped whenever a row is added or changed so placement ledgers stay
/// attributable to the catalog revision they were solved against.
pub const CATALOG_REVISION: u32 = 2;

/// ISO date (YYYY-MM-DD) of the last catalog refresh.
pub const CATALOG_UPDATED: &str = "2026-09-15";

/// Provenance of a row, same vocabulary as the price table.
#[allow(dead_code)] // surfaced on capability responses when probes land
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityStatus {
    /// Id and limits verified against a provider listing on/before
    /// [`CATALOG_UPDATED`].
    Published,
    /// Carried over from a previous generation; limits are best-effort.
    Provisional,
}

/// Feature support bits used by the capability filter; §4.2.
#[allow(dead_code)] // fields read through `supports_all` once tasks declare features
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModelSupports {
    /// Allows `tool_choice: any` (compact tool loops).
    pub tool_choice_any: bool,
    /// Guaranteed structured/JSON output.
    pub structured_output: bool,
    /// Image inputs.
    pub vision: bool,
    /// Prompt caching (so warm-cache affinity means something).
    pub prompt_caching: bool,
    /// Assistant prefill.
    pub assistant_prefill: bool,
}

/// One static capability row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityProfile {
    /// Canonical model id (also a price-table prefix family name).
    pub id: &'static str,
    /// Rate-limit bucket; deliberately distinct from `id` (§4.2).
    #[allow(dead_code)]
    pub family: &'static str,
    /// Ids/aliases this model answers to (incl. `[1m]` long-context spellings).
    pub aliases: &'static [&'static str],
    /// Tier class.
    pub class: ModelClass,
    /// Context window in tokens.
    pub context_window: u64,
    /// Maximum output tokens.
    #[allow(dead_code)] // surfaced in capability responses once probe reads it
    pub max_output_tokens: u64,
    /// Supported native effort tier names (aligns with the web effort tables).
    pub effort_levels: &'static [&'static str],
    /// Feature support.
    pub supports: ModelSupports,
    /// Task shapes this model is suited for (hints, never hard filters).
    #[allow(dead_code)]
    pub suited_for: &'static [&'static str],
    /// Card provenance.
    #[allow(dead_code)]
    pub status: CapabilityStatus,
}

impl CapabilityProfile {
    /// Estimated USD per million **input** tokens from the driver price table,
    /// if a card exists. Cost is an estimate everywhere (§4.5).
    #[must_use]
    pub fn input_price_per_mtok(&self) -> Option<f64> {
        prices::lookup(self.id).map(|price| price.input_usd_per_mtok)
    }

    /// True when the price card backing cost ranking is provisional.
    #[must_use]
    pub fn price_is_provisional(&self) -> bool {
        prices::lookup(self.id).is_none_or(|price| price.status == PriceStatus::Provisional)
    }

    /// True when this row satisfies every required feature.
    #[allow(dead_code)] // §4.4 capability feature filter; tasks declare supports in a later wave
    #[must_use]
    pub fn supports_all(&self, required: &ModelSupports) -> bool {
        (!required.tool_choice_any || self.supports.tool_choice_any)
            && (!required.structured_output || self.supports.structured_output)
            && (!required.vision || self.supports.vision)
            && (!required.prompt_caching || self.supports.prompt_caching)
            && (!required.assistant_prefill || self.supports.assistant_prefill)
    }
}

const CLAUDE_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const CODEX_LEVELS: &[&str] = remuda_driver::effort::CODEX_REASONING_EFFORTS;

const CLAUDE_SUPPORTS: ModelSupports = ModelSupports {
    tool_choice_any: true,
    structured_output: true,
    vision: true,
    prompt_caching: true,
    assistant_prefill: true,
};

/// The built-in table. Matching is by longest id/alias, never position.
static CATALOG: &[CapabilityProfile] = &[
    // ── Anthropic Claude ──────────────────────────────────────────────
    CapabilityProfile {
        id: "claude-fable-5",
        family: "fable",
        aliases: &["fable", "claude-fable-5[1m]"],
        class: ModelClass::Frontier,
        context_window: 1_000_000,
        max_output_tokens: 64_000,
        effort_levels: CLAUDE_LEVELS,
        supports: CLAUDE_SUPPORTS,
        suited_for: &["design-heavy", "coordination", "review"],
        status: CapabilityStatus::Published,
    },
    CapabilityProfile {
        id: "claude-opus-5",
        family: "opus",
        aliases: &["opus", "claude-opus-5[1m]"],
        class: ModelClass::Frontier,
        context_window: 1_048_576,
        max_output_tokens: 64_000,
        effort_levels: CLAUDE_LEVELS,
        supports: CLAUDE_SUPPORTS,
        suited_for: &["design-heavy", "rebase", "review"],
        status: CapabilityStatus::Provisional,
    },
    CapabilityProfile {
        id: "claude-sonnet-5",
        family: "sonnet",
        aliases: &["sonnet", "claude-sonnet-5[1m]"],
        class: ModelClass::Workhorse,
        context_window: 1_000_000,
        max_output_tokens: 64_000,
        effort_levels: CLAUDE_LEVELS,
        supports: CLAUDE_SUPPORTS,
        suited_for: &["implement", "test", "docs"],
        status: CapabilityStatus::Provisional,
    },
    CapabilityProfile {
        id: "claude-haiku-4-5",
        family: "haiku",
        aliases: &["haiku", "claude-haiku-4-5[1m]"],
        class: ModelClass::Cheap,
        context_window: 200_000,
        max_output_tokens: 8_192,
        effort_levels: CLAUDE_LEVELS,
        supports: CLAUDE_SUPPORTS,
        suited_for: &["triage", "docs", "cheap"],
        status: CapabilityStatus::Published,
    },
    // ── OpenAI GPT-5 (Codex) ─────────────────────────────────────────
    CapabilityProfile {
        id: "gpt-5",
        family: "gpt-5",
        aliases: &["gpt-5[1m]"],
        class: ModelClass::Frontier,
        context_window: 400_000,
        max_output_tokens: 128_000,
        effort_levels: CODEX_LEVELS,
        supports: ModelSupports {
            tool_choice_any: true,
            structured_output: true,
            vision: true,
            prompt_caching: true,
            assistant_prefill: false,
        },
        suited_for: &["implement", "review"],
        status: CapabilityStatus::Provisional,
    },
    CapabilityProfile {
        id: "gpt-5-mini",
        family: "gpt-5-mini",
        aliases: &["gpt-5-mini[1m]"],
        class: ModelClass::Workhorse,
        context_window: 400_000,
        max_output_tokens: 128_000,
        effort_levels: CODEX_LEVELS,
        supports: ModelSupports {
            tool_choice_any: true,
            structured_output: true,
            vision: false,
            prompt_caching: true,
            assistant_prefill: false,
        },
        suited_for: &["implement", "test"],
        status: CapabilityStatus::Provisional,
    },
    CapabilityProfile {
        id: "gpt-5-nano",
        family: "gpt-5-nano",
        aliases: &["gpt-5-nano[1m]"],
        class: ModelClass::Cheap,
        context_window: 400_000,
        max_output_tokens: 128_000,
        effort_levels: CODEX_LEVELS,
        supports: ModelSupports {
            tool_choice_any: true,
            structured_output: true,
            vision: false,
            prompt_caching: true,
            assistant_prefill: false,
        },
        suited_for: &["triage", "docs", "cheap"],
        status: CapabilityStatus::Provisional,
    },
    // ── xAI Grok ─────────────────────────────────────────────────────
    CapabilityProfile {
        id: "grok-4",
        family: "grok-4",
        aliases: &["grok"],
        class: ModelClass::Frontier,
        context_window: 256_000,
        max_output_tokens: 32_000,
        effort_levels: &["quick", "standard", "max"],
        supports: ModelSupports {
            tool_choice_any: false,
            structured_output: true,
            vision: true,
            prompt_caching: true,
            assistant_prefill: false,
        },
        suited_for: &["implement", "research"],
        status: CapabilityStatus::Provisional,
    },
    CapabilityProfile {
        id: "grok-4-fast",
        family: "grok-4-fast",
        aliases: &["grok-fast"],
        class: ModelClass::Workhorse,
        context_window: 256_000,
        max_output_tokens: 16_000,
        effort_levels: &["quick", "standard", "max"],
        supports: ModelSupports {
            tool_choice_any: false,
            structured_output: true,
            vision: false,
            prompt_caching: true,
            assistant_prefill: false,
        },
        suited_for: &["implement", "test"],
        status: CapabilityStatus::Provisional,
    },
    CapabilityProfile {
        id: "grok-3-mini",
        family: "grok-3-mini",
        aliases: &[],
        class: ModelClass::Cheap,
        context_window: 131_072,
        max_output_tokens: 8_192,
        effort_levels: &["quick", "standard", "max"],
        supports: ModelSupports {
            tool_choice_any: false,
            structured_output: true,
            vision: true,
            prompt_caching: true,
            assistant_prefill: false,
        },
        suited_for: &["triage", "cheap"],
        status: CapabilityStatus::Provisional,
    },
];

/// All rows (tests iterate this for revision/alias consistency).
#[must_use]
pub fn catalog() -> &'static [CapabilityProfile] {
    CATALOG
}

/// Normalize a model id for lookup: trim, lowercase, strip a dated snapshot
/// suffix (`-20251001`), and drop a trailing `[1m]` long-context tag.
fn normalize(model_id: &str) -> String {
    let mut id = model_id.trim().to_ascii_lowercase();
    if let Some(stripped) = id.strip_suffix("[1m]") {
        id = stripped.to_string();
    }
    let bytes = id.as_bytes();
    if bytes.len() > 9
        && bytes[bytes.len() - 9] == b'-'
        && bytes[bytes.len() - 8..].iter().all(u8::is_ascii_digit)
    {
        id.truncate(id.len() - 9);
    }
    id
}

/// Look up the capability row for a model id by canonical id or alias.
///
/// Longest token wins, and an unknown id returns `None` — gateway-renamed
/// models then rely on the profile's declared `family`/`role`.
#[must_use]
pub fn lookup(model_id: &str) -> Option<&'static CapabilityProfile> {
    let id = normalize(model_id);
    if id.is_empty() {
        return None;
    }
    let mut best: Option<(usize, &'static CapabilityProfile)> = None;
    for row in CATALOG {
        let candidates = std::iter::once(&row.id).chain(row.aliases.iter());
        for candidate in candidates {
            let candidate = candidate.to_ascii_lowercase();
            if (id == candidate || id.starts_with(&format!("{candidate}-")))
                && best.is_none_or(|(len, _)| candidate.len() > len)
            {
                best = Some((candidate.len(), row));
            }
        }
    }
    best.map(|(_, row)| row)
}

/// Resolve the effective class of a model: built-in catalog first, then a
/// declared role wire name, then `None` (treated as workhorse at admission,
/// but the ledger records the missing capability).
#[must_use]
pub fn class_of(model_id: &str, declared_role: Option<&str>) -> Option<ModelClass> {
    lookup(model_id)
        .map(|row| row.class)
        .or_else(|| declared_role.and_then(ModelClass::from_wire))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_and_suffixes_resolve() {
        assert_eq!(lookup("opus").unwrap().family, "opus");
        assert_eq!(lookup("Claude-Opus-5[1m]").unwrap().id, "claude-opus-5");
        assert_eq!(
            lookup("claude-haiku-4-5-20251001").unwrap().id,
            "claude-haiku-4-5"
        );
        assert_eq!(lookup("gpt-5-nano").unwrap().class, ModelClass::Cheap);
        assert_eq!(lookup("grok-fast").unwrap().class, ModelClass::Workhorse);
        assert!(lookup("passthrough/ark/es1_orange_o50[1m]").is_none());
    }

    #[test]
    fn declared_role_fills_unknown_gateway_models() {
        assert_eq!(
            class_of("gw/seed-evolving[1m]", Some("workhorse")),
            Some(ModelClass::Workhorse)
        );
        assert_eq!(class_of("gw/seed-evolving[1m]", None), None);
        // The catalog always wins over a mis-declared role.
        assert_eq!(
            class_of("claude-opus-5", Some("cheap")),
            Some(ModelClass::Frontier)
        );
    }

    #[test]
    fn feature_gates_and_prices() {
        let opus = lookup("claude-opus-5").unwrap();
        assert!(opus.supports_all(&ModelSupports {
            vision: true,
            ..Default::default()
        }));
        let grok = lookup("grok-4").unwrap();
        assert!(!grok.supports_all(&ModelSupports {
            tool_choice_any: true,
            ..Default::default()
        }));
        // Opus input price is carried from the (provisional) driver card.
        assert_eq!(opus.input_price_per_mtok(), Some(15.0));
        assert!(opus.price_is_provisional());
        assert_eq!(CATALOG_REVISION, 2);
    }

    #[test]
    fn codex_capability_rows_offer_the_six_native_tiers() {
        for model in ["gpt-5", "gpt-5-mini", "gpt-5-nano"] {
            assert_eq!(
                lookup(model).unwrap().effort_levels,
                &["low", "medium", "high", "xhigh", "max", "ultra"],
                "{model}"
            );
        }
    }

    #[test]
    fn every_row_is_self_consistent() {
        for row in CATALOG {
            assert!(row.context_window > 0);
            assert!(row.max_output_tokens > 0);
            assert!(!row.effort_levels.is_empty());
            assert_eq!(lookup(row.id).unwrap().id, row.id);
            for alias in row.aliases {
                assert!(lookup(alias).is_some(), "alias {alias} unresolved");
            }
        }
    }
}
