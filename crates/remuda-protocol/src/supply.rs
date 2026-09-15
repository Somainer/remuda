//! Model supply declarations, observed rate-limit windows, and the dispatch
//! `TaskSpec`; coordinator design §4.2–§4.4.
//!
//! Three data categories, three sources (§4.1): capability lives in the Hub's
//! built-in catalog, **supply must be declared by the user**, and observations
//! are merged back onto the same window objects without clearing declared
//! values. `family` is the rate-limit bucket — it is deliberately not the model
//! id: an account-level window (`appliesTo: ["*"]`) follows a model switch, a
//! family-level window does not.

use crate::ProjectId;
use crate::TaskId;
use serde::{Deserialize, Serialize};

/// Where a window's numbers came from; §4.1.
///
/// Observed frames update `observed` fields but never erase a `declared`
/// limit; `inferred` is filled by Hub-side usage aggregation (§4.5).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
pub enum WindowSource {
    /// User-declared via `remuda profile declare` / REST.
    #[default]
    #[serde(rename = "declared")]
    Declared,
    /// Harness frame, probe, or screen text (429 / Codex rateLimits).
    #[serde(rename = "observed")]
    Observed,
    /// Hub usage aggregation inferred fill level.
    #[serde(rename = "inferred")]
    Inferred,
}

/// Account-level supply state; §4.2.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
pub enum SupplyState {
    /// Usable on the last evidence.
    #[default]
    #[serde(rename = "available")]
    Available,
    /// A family-scoped window is cooling; sibling families stay usable.
    #[serde(rename = "degraded")]
    Degraded,
    /// An account-scoped window is cooling until `cooldownUntil`/reset.
    #[serde(rename = "cooling")]
    Cooling,
    /// Declared/explicitly observed as fully spent.
    #[serde(rename = "exhausted")]
    Exhausted,
    /// Gateway-style supply with no windows the Hub can read; used only when
    /// explicitly prioritised or once every windowed supply is exhausted
    /// (design §8.3 open question 4).
    #[serde(rename = "unknown")]
    Unknown,
}

/// Who may spend a supply; §4.2 (`reserve`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
pub enum SupplyReserve {
    /// Ordinary worker tasks may use it.
    #[default]
    #[serde(rename = "none")]
    None,
    /// Only coordinator seats may spend it (e.g. Fable leading a loop).
    #[serde(rename = "coordinator-only")]
    CoordinatorOnly,
}

/// Model capability class used by `TaskSpec.minClass` and the catalog; §4.2.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
pub enum ModelClass {
    /// Cheapest tier; never auto-selected above `minClass`.
    #[serde(rename = "cheap")]
    Cheap,
    /// Default worker tier.
    #[default]
    #[serde(rename = "workhorse")]
    Workhorse,
    /// Strongest tier; review/design-heavy work.
    #[serde(rename = "frontier")]
    Frontier,
}

impl ModelClass {
    /// Parse a class wire name, accepting the plural/alias spellings seen in
    /// briefs (`frontier` / `workhorse` / `cheap`).
    pub fn from_wire(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "frontier" | "strong" => Some(Self::Frontier),
            "workhorse" | "standard" | "default" => Some(Self::Workhorse),
            "cheap" | "light" => Some(Self::Cheap),
            _ => None,
        }
    }

    /// Numeric order used by the capability filter; `minClass` rejects anything
    /// below it and nothing is ever silently downgraded (§4.4 step 8).
    pub fn rank(self) -> u8 {
        self as u8
    }
}

/// One rate-limit window — the Codex `RateLimitWindow` vocabulary (§4.2).
///
/// `appliesTo` is the field that makes fallback correct: `["*"]` is an
/// account/session/weekly bucket a model switch cannot escape; `["<family>"]`
/// is a family bucket a sibling family can dodge (§4.6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitWindow {
    /// Stable window id (`primary`, `weekly`, `model`, …).
    pub id: String,
    /// Families this window covers; `["*"]` = account-level.
    #[serde(default = "default_applies_to")]
    pub applies_to: Vec<String>,
    /// Declared request/token budget when the user knows one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<crate::U64>,
    /// Window length in minutes (Codex `windowDurationMins`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_duration_mins: Option<u64>,
    /// Fill percentage 0..=100 from the latest evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    /// Unix-epoch **seconds** at which the window resets; null = unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
    /// Declared vs observed vs inferred.
    #[serde(default)]
    pub source: WindowSource,
    /// Unix-epoch seconds of the last observed fill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<i64>,
    /// Runtime: park admissions on this window until this epoch second.
    /// Known `resetsAt` wins; otherwise exponential backoff sets it (§4.4 ⑦).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<i64>,
    /// Consecutive limit hits without a reset; drives the backoff ladder.
    #[serde(default, skip_serializing_if = "u32_is_zero")]
    pub backoff_attempts: u32,
}

fn default_applies_to() -> Vec<String> {
    vec!["*".to_string()]
}
#[allow(clippy::trivially_copy_pass_by_ref)]
fn u32_is_zero(value: &u32) -> bool {
    *value == 0
}

impl RateLimitWindow {
    /// A declared window with the given id; fill stays unknown until observed.
    pub fn declared(
        id: impl Into<String>,
        applies_to: Vec<String>,
        duration_mins: Option<u64>,
    ) -> Self {
        Self {
            id: id.into(),
            applies_to,
            limit: None,
            window_duration_mins: duration_mins,
            used_percent: None,
            resets_at: None,
            source: WindowSource::Declared,
            observed_at: None,
            cooldown_until: None,
            backoff_attempts: 0,
        }
    }

    /// True when this window governs `family` (or every family).
    pub fn applies_to_family(&self, family: &str) -> bool {
        self.applies_to
            .iter()
            .any(|entry| entry == "*" || entry == family)
    }

    /// True when this is the account-level bucket a model switch cannot dodge.
    pub fn is_account_level(&self) -> bool {
        self.applies_to.iter().any(|entry| entry == "*")
    }

    /// True while a cooldown parks this window at instant `now`.
    pub fn is_cooling_at(&self, now: i64) -> bool {
        self.cooldown_until.is_some_and(|until| until > now)
    }
}

/// Account-level concurrency ceiling; §4.4 step 2 — the primitive that was
/// entirely missing while only host `maxInstances` existed.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SupplyConcurrency {
    /// Max simultaneous in-flight uses across every host; null = undeclared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
}

/// Subscription/credits presence; §4.2.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SupplyCredits {
    /// Account has a credit balance; null = unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_credits: Option<bool>,
    /// Balance is unlimited.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unlimited: bool,
}

/// Spend-control panel numbers (billing-site data, never inferred); §4.2.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SpendControl {
    /// Declared period limit, decimal string in the account currency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<String>,
    /// Spent so far, decimal string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<String>,
    /// Remaining percentage 0..=100 as reported by the billing page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_percent: Option<f64>,
}

/// Declared supply plus observed state for one provider profile; §4.2.
///
/// Every field is optional/defaulted so a user can declare as little as
/// "workhorse: X, scarce: Y" and grow from there. Observed runtime fields
/// (`state`, `cooldownUntil`, `lastError`, window cooldowns) are written by
/// the Hub feedback loop, never by the user.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SupplyProfile {
    /// User preference order: larger wins (changed often — that is the point).
    #[serde(default, skip_serializing_if = "i64_is_zero")]
    pub priority: i64,
    /// Reserve this supply for coordinator seats.
    #[serde(default)]
    pub reserve: SupplyReserve,
    /// Account-level in-flight ceiling.
    #[serde(default)]
    pub concurrency: SupplyConcurrency,
    /// Optional declared aggregate limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_usd: Option<f64>,
    /// Optional declared weekly limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_usd: Option<f64>,
    /// Declared primary reset window length in minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_window_mins: Option<u64>,
    /// Rate-limit windows (declared + observed merged).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<RateLimitWindow>,
    /// Credits information.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<SupplyCredits>,
    /// Billing spend-control panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend_control: Option<SpendControl>,
    /// Whether ordinary use is allowed; null = unknown. Per the Codex contract
    /// this is NEVER inferred from percentages or reset times.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinary_usage_allowed: Option<bool>,
    /// Observed runtime state.
    #[serde(default)]
    pub state: SupplyState,
    /// Account-level cooldown epoch seconds (exponential backoff fallback).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<i64>,
    /// Last classified supply error (429 text, structured frame, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn i64_is_zero(value: &i64) -> bool {
    *value == 0
}

// ── TaskSpec (§4.3) ────────────────────────────────────────────────────────

/// Dispatch task classes; §4.3.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
pub enum TaskClass {
    /// Open-ended investigation / spike.
    #[serde(rename = "research")]
    Research,
    /// Product code change.
    #[default]
    #[serde(rename = "implement")]
    Implement,
    /// Diff review.
    #[serde(rename = "review")]
    Review,
    /// Test work.
    #[serde(rename = "test")]
    Test,
    /// Gate / merge execution.
    #[serde(rename = "merge-gate")]
    MergeGate,
    /// Triage of failures.
    #[serde(rename = "triage")]
    Triage,
    /// Documentation.
    #[serde(rename = "docs")]
    Docs,
}

/// Generic low/normal/high sensitivity knob.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
pub enum Sensitivity {
    /// Prefer the cheap/remote side.
    #[serde(rename = "low")]
    Low,
    /// Default neutrality.
    #[default]
    #[serde(rename = "normal")]
    Normal,
    /// Cost: prefer cheap. Latency: prefer local/warm.
    #[serde(rename = "high")]
    High,
}

/// Context-size requirements of a task; §4.3.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextNeed {
    /// Planner's expected input token volume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_input_tokens: Option<crate::U64>,
    /// Task explicitly needs a long-context model.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_long_context: bool,
    /// `file` | `crate` | `workspace` | `repo` blast radius hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_scope: Option<String>,
}

/// Hard task budget; §4.3. Money is an estimate (§4.5) — the stop band is
/// estimate×1.15, applied by the coordinator loop rather than this type.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskBudget {
    /// Estimated USD cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_usd: Option<f64>,
    /// Turn cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u64>,
    /// Wall-clock cap in minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_wall_mins: Option<u64>,
}

/// Explicit pin that disables automatic supply choice; §4.3.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskPin {
    /// Pinned harness (`claude` / `codex` / …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// Pinned model id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Pinned supply (provider profile id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supply_id: Option<String>,
}

/// What a coordinator submits at dispatch/instance-create; §4.3.
///
/// Carried additively on the instance create body as `taskSpec`. Only
/// `class`/sensitivities are LLM assignments; the rest is planner bookkeeping.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskSpec {
    /// Bound task (`tsk_…`); batch 4's ledger owns the row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// Owning project (`prj_…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// Parent task in the delegation tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<TaskId>,
    /// Task class; defaults to `implement`.
    #[serde(default)]
    pub class: TaskClass,
    /// Minimum model class; admission never goes below this silently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_class: Option<ModelClass>,
    /// Requested effort tier name (validated against catalog levels).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Context requirements.
    #[serde(default)]
    pub context_need: ContextNeed,
    /// Cost sensitivity; `high` folds price into the rank.
    #[serde(default)]
    pub cost_sensitivity: Sensitivity,
    /// Latency sensitivity; `high` favours warm/local supply.
    #[serde(default)]
    pub latency_sensitivity: Sensitivity,
    /// Hard host-label requirements (`toolchain=rust`, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// Budget caps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<TaskBudget>,
    /// Explicit harness/model/supply pin; disables auto-selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<TaskPin>,
    /// Opt into picking a weaker class when nothing satisfies `minClass`.
    /// Default false: unsatisfied admission is `deferred`, never downgraded.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_downgrade: bool,
}

/// One observed Codex `account/rateLimits/updated` frame, normalized; §4.2.
///
/// The Node does not relay these frames yet (r-p6); the REST
/// `…/supply/events` path accepts this shape so the loop is wired end to end.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObservedRateLimits {
    /// Account/window snapshots exactly as Codex reports them.
    #[serde(default)]
    pub windows: Vec<RateLimitWindow>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn class_ordering_and_aliases() {
        assert!(ModelClass::Frontier > ModelClass::Workhorse);
        assert!(ModelClass::Workhorse > ModelClass::Cheap);
        assert_eq!(
            ModelClass::from_wire("FRONTIER"),
            Some(ModelClass::Frontier)
        );
        assert_eq!(ModelClass::from_wire("light"), Some(ModelClass::Cheap));
        assert_eq!(ModelClass::from_wire("mystery"), None);
    }

    #[test]
    fn window_scope_distinguishes_account_and_family() {
        let account = RateLimitWindow::declared("weekly", vec!["*".to_string()], Some(10080));
        let family = RateLimitWindow::declared("model", vec!["es1".to_string()], Some(300));
        assert!(account.is_account_level());
        assert!(!family.is_account_level());
        assert!(account.applies_to_family("es1"));
        assert!(family.applies_to_family("es1"));
        assert!(!family.applies_to_family("seed"));
        let mut cooling = family.clone();
        cooling.cooldown_until = Some(100);
        assert!(cooling.is_cooling_at(99));
        assert!(!cooling.is_cooling_at(100));
    }

    #[test]
    fn sparse_declared_supply_round_trips() {
        let supply = SupplyProfile::default();
        let value = serde_json::to_value(&supply).unwrap();
        // A bare declaration adds almost nothing to the wire.
        assert_eq!(
            value,
            json!({ "reserve": "none", "concurrency": {}, "state": "available" })
        );
        let parsed: SupplyProfile = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, supply);
    }
}
