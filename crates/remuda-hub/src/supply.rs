//! Model-supply admission, ranking, cooldown, and the 429 feedback loop;
//! coordinator design §4.4–§4.6.
//!
//! Policy shape: **admission control is heavy, failover is light** (§4.4).
//! Everything below is pure given a clock instant, so the 09-14 es1 replay
//! can pin time deterministically. Rules encoded here:
//!
//! * capability filter (class ≥ `minClass`, context window, effort level),
//! * supply filter (state, cooling windows, account-level concurrency, reserve),
//! * rank = priority → remaining declared share → cooldown freshness →
//!   warm-cache affinity → cost (cost-sensitive tasks) → host load,
//! * cooldown unit is `(supplyId, windowId)`; known `resetsAt` parks to it,
//!   otherwise exponential backoff 60s→2m→5m→15m,
//! * HTTP 429 / rate-limit text cools the model's **family** window (siblings
//!   stay admissible); a 529/overloaded is the upstream fleet, never ours, and
//!   does not cool anything,
//! * no admissible candidate ⇒ explicit `deferred`, never a silent downgrade.

use crate::model_catalog;
use crate::store::{HostRecord, InstanceRecord, ProviderRecord};
use remuda_protocol::{
    ModelClass, ObservedRateLimits, RateLimitWindow, Sensitivity, SupplyReserve, SupplyState,
    TaskSpec, WindowSource,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

// ── cooldown ───────────────────────────────────────────────────────────────

/// Backoff ladder (seconds) when a 429 carries no `resetsAt`: 1m, 2m, 5m, 15m.
pub const BACKOFF_LADDER_SECS: &[i64] = &[60, 2 * 60, 5 * 60, 15 * 60];

fn backoff_secs(attempts: u32) -> i64 {
    BACKOFF_LADDER_SECS
        .get(attempts as usize)
        .copied()
        .unwrap_or(*BACKOFF_LADDER_SECS.last().unwrap())
}

// ── textual classification (observed-textual, §4.2 source 2) ───────────────

/// What a piece of screen/journal evidence means for supply state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateSignal {
    /// Our quota: cool the (supply, window). 429 / known limit phrases.
    RateLimited,
    /// Their fleet is overloaded: explicitly NOT a cooldown trigger (§4.4 ⑦).
    FleetOverloaded,
}

/// Classify an HTTP status + journal text into a supply signal.
///
/// 529/overloaded is checked **first**: an upstream overload must never cool
/// one of our windows. The 429 patterns match the fixed strings the design
/// lists (`Request rejected (429)`, `session limit`, `weekly limit`,
/// `Opus limit`, `Budget limit reached`) and share matcher mechanics with
/// `wait --until 'line:'`.
#[must_use]
pub fn classify_rate_signal(status: Option<u16>, text: &str) -> Option<RateSignal> {
    let text = text.to_ascii_lowercase();
    if status == Some(529) || text.contains("529") && text.contains("overload") {
        return Some(RateSignal::FleetOverloaded);
    }
    if text.contains("overloaded") || text.contains("service unavailable") {
        return Some(RateSignal::FleetOverloaded);
    }
    if status == Some(429)
        || text.contains("429")
            && (text.contains("rate")
                || text.contains("limit")
                || text.contains("rejected")
                || text.contains("quota"))
    {
        return Some(RateSignal::RateLimited);
    }
    const PHRASES: &[&str] = &[
        "request rejected (429)",
        "you've hit your session limit",
        "you have hit your session limit",
        "hit your rate limit",
        "rate limit reached",
        "rate_limit",
        "weekly limit",
        "session limit",
        "opus limit",
        "budget limit reached",
        "resource_exhausted",
        "too many requests",
    ];
    PHRASES
        .iter()
        .any(|phrase| text.contains(phrase))
        .then_some(RateSignal::RateLimited)
}

// ── feedback application (§4.5 / §4.6) ─────────────────────────────────────

/// Merge one observed/structured window snapshot without erasing declared
/// fields. A `usedPercent` of 100 parks the window; a known `resetsAt` is the
/// cooldown horizon.
pub fn apply_structured_windows(
    windows: &mut Vec<RateLimitWindow>,
    observed: &ObservedRateLimits,
    now: i64,
) {
    for incoming in &observed.windows {
        apply_window_snapshot(windows, incoming, now);
    }
}

#[allow(clippy::ptr_arg)] // pushes new windows; needs Vec, not slice
fn apply_window_snapshot(windows: &mut Vec<RateLimitWindow>, incoming: &RateLimitWindow, now: i64) {
    let pos = windows.iter().position(|w| w.id == incoming.id);
    let full = incoming.used_percent.is_some_and(|pct| pct >= 100.0);
    let Some(pos) = pos else {
        let mut window = incoming.clone();
        window.source = WindowSource::Observed;
        window.observed_at = Some(now);
        if full {
            park_window(&mut window, now);
        }
        windows.push(window);
        return;
    };
    let window = &mut windows[pos];
    // Observed never clears declared values: fill the metric fields, keep the
    // declared limit/duration when the frame omits them.
    if incoming.used_percent.is_some() {
        window.used_percent = incoming.used_percent;
    }
    if incoming.resets_at.is_some() {
        window.resets_at = incoming.resets_at;
    }
    if incoming.window_duration_mins.is_some() {
        window.window_duration_mins = incoming.window_duration_mins;
    }
    if !incoming.applies_to.is_empty() {
        window.applies_to = incoming.applies_to.clone();
    }
    window.source = WindowSource::Observed;
    window.observed_at = Some(now);
    if full {
        park_window(window, now);
    }
}

fn park_window(window: &mut RateLimitWindow, now: i64) {
    window.used_percent = Some(100.0);
    if window.resets_at.is_none_or(|reset| reset <= now) {
        window.cooldown_until = Some(now + backoff_secs(window.backoff_attempts));
        window.backoff_attempts = window.backoff_attempts.saturating_add(1);
    } else {
        window.cooldown_until = window.resets_at;
        // A later retry past the reset starts the backoff ladder fresh.
        window.backoff_attempts = 0;
    }
}

/// Apply a textual 429 against one model's family: park a family-scoped
/// window so sibling families on the same account stay admissible (§4.6).
///
/// Returns false when the signal is [`RateSignal::FleetOverloaded`] — by
/// contract nothing is cooled in that case.
#[allow(clippy::ptr_arg)] // pushes a new family window
pub fn apply_family_rate_hit(windows: &mut Vec<RateLimitWindow>, family: &str, now: i64) -> bool {
    let observed = RateLimitWindow {
        id: "model".to_string(),
        applies_to: vec![family.to_string()],
        limit: None,
        window_duration_mins: None,
        used_percent: Some(100.0),
        resets_at: None,
        source: WindowSource::Observed,
        observed_at: Some(now),
        cooldown_until: None,
        backoff_attempts: 0,
    };
    apply_window_snapshot(windows, &observed, now);
    true
}

/// Expire passed cooldowns and recompute derived state; call at every solve.
///
/// `has_declared_windows` distinguishes a gateway profile the Hub cannot see
/// (state stays `unknown`) from a fully observable profile (state returns to
/// `available`).
#[must_use]
pub fn refresh_state(
    windows: &mut [RateLimitWindow],
    explicit_state: Option<SupplyState>,
    now: i64,
) -> SupplyState {
    for window in windows.iter_mut() {
        if window.cooldown_until.is_some_and(|until| until <= now) {
            window.cooldown_until = None;
            if window.source == WindowSource::Observed
                && window.used_percent.is_some_and(|pct| pct >= 100.0)
            {
                // The cooldown elapsed: the next request is allowed through to
                // get a fresh frame rather than being pre-rejected on stale
                // fill. A known reset that has passed clears the ladder too;
                // an expired *backoff* keeps the attempt count so a repeated
                // hit escalates 60→2m→5m→15m (§4.4 ⑦).
                window.used_percent = None;
                if window.resets_at.is_some_and(|reset| reset <= now) {
                    window.backoff_attempts = 0;
                    window.resets_at = None;
                }
            }
        }
    }
    let account_cooling = windows
        .iter()
        .any(|w| w.is_account_level() && w.is_cooling_at(now));
    let family_cooling = windows
        .iter()
        .any(|w| !w.is_account_level() && w.is_cooling_at(now));
    let account_full = windows.iter().any(|w| {
        w.is_account_level()
            && !w.is_cooling_at(now)
            && w.used_percent.is_some_and(|pct| pct >= 100.0)
    });
    let derived = if account_cooling {
        SupplyState::Cooling
    } else if account_full {
        SupplyState::Exhausted
    } else if family_cooling {
        SupplyState::Degraded
    } else {
        SupplyState::Available
    };
    match explicit_state {
        Some(SupplyState::Unknown) => SupplyState::Unknown,
        Some(SupplyState::Exhausted) => SupplyState::Exhausted,
        Some(other) if other != SupplyState::Available => other,
        _ => derived,
    }
}

// ── candidates ─────────────────────────────────────────────────────────────

/// A concrete (profile, catalog-model) the solver may admit.
#[derive(Clone, Debug)]
pub struct SupplyCandidate {
    /// Provider profile row (with declared supply).
    pub profile_id: String,
    /// Profile display name (ledger readability).
    pub profile_name: String,
    /// Model id as sent over the wire.
    pub model_id: String,
    /// Resolved rate-limit bucket: declared model family, else catalog family.
    pub family: String,
    /// Resolved capability class.
    pub class: ModelClass,
    /// Resolved context window (catalog/declared), when known.
    pub context_window: Option<u64>,
    /// Catalog effort levels, when known.
    pub effort_levels: Vec<String>,
    /// Declared per-model fallback order.
    pub fallback: Vec<String>,
    /// Profile priority.
    pub profile_priority: i64,
    /// Model-level priority override.
    pub model_priority: Option<i64>,
    /// Profile-level concurrency ceiling.
    pub concurrency_max: Option<i64>,
    /// Model-level concurrency ceiling.
    pub model_concurrency_max: Option<i64>,
    /// Account reserve.
    pub reserve: SupplyReserve,
    /// Live windows (mutated by refresh at solve time).
    pub windows: Vec<RateLimitWindow>,
    /// Declared explicit state.
    pub explicit_state: SupplyState,
    /// True for gateway profiles with no readable windows.
    pub unknown_supply: bool,
    /// Input price USD/MTok for cost ranking.
    pub input_price: Option<f64>,
    /// Hosts this profile's scope allows among the placement-eligible hosts.
    pub host_ids: Vec<String>,
    /// Per-host load ratio in 0..=1.
    pub host_load: HashMap<String, f64>,
    /// Hosts where this model is currently in flight (warm-cache affinity).
    pub warm_host_ids: HashSet<String>,
    /// Explicitly requested via TaskSpec.pin.
    pub pinned: bool,
}

impl SupplyCandidate {
    fn effective_priority(&self) -> i64 {
        self.model_priority.unwrap_or(self.profile_priority)
    }

    /// Remaining declared share 0..=1 across the windows governing `family`.
    /// Windows with no observed fill read as fully available.
    fn remaining_share(&self, now: i64) -> f64 {
        self.windows
            .iter()
            .filter(|w| w.applies_to_family(&self.family))
            .filter(|w| !w.is_cooling_at(now))
            .filter_map(|w| w.used_percent)
            .map(|pct| 1.0 - (pct / 100.0).clamp(0.0, 1.0))
            .fold(1.0_f64, f64::min)
    }

    fn latest_cooldown(&self, now: i64) -> Option<i64> {
        self.windows
            .iter()
            .filter(|w| w.applies_to_family(&self.family))
            .filter_map(|w| w.cooldown_until)
            .filter(|until| *until > now)
            .max()
    }
}

/// Why a candidate was rejected; rendered into `rejected[]` on the ledger.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RejectedSupply {
    /// `pvp_…`.
    pub profile_id: String,
    /// Profile name.
    pub profile_name: String,
    /// Model id.
    pub model_id: String,
    /// Human-readable rejection reasons.
    pub reasons: Vec<String>,
}

/// One ranked candidate in the dry-run / ledger view.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RankedSupply {
    /// `pvp_…`.
    pub profile_id: String,
    /// Model id.
    pub model_id: String,
    /// Rate-limit family bucket.
    pub family: String,
    /// Effective priority.
    pub priority: i64,
    /// Remaining window share 0..=1.
    pub remaining_share: f64,
    /// Resolved class.
    pub class: String,
    /// Supply state at solve time.
    pub state: String,
}

/// The chosen admission.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChosenSupply {
    /// `pvp_…`.
    pub profile_id: String,
    /// Model id to launch with.
    pub model_id: String,
    /// Family bucket.
    pub family: String,
    /// Selected host (when host constraints narrowed to one).
    pub host_id: Option<String>,
    /// Declared fallback chain.
    pub fallback: Vec<String>,
}

/// Result of a supply solve; serialized verbatim into the placement ledger.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupplyDecision {
    /// Admission when one exists.
    pub chosen: Option<ChosenSupply>,
    /// Candidates after filtering, in rank order.
    pub ranked: Vec<RankedSupply>,
    /// Rejected candidates with reasons.
    pub rejected: Vec<RejectedSupply>,
    /// Positive reasons explaining the winner (bot card copy).
    pub reasons: Vec<String>,
    /// True when nothing was admissible; the caller must queue/defer.
    pub deferred: bool,
    /// Earliest cooldown horizon worth retrying at (epoch seconds).
    pub deferred_until: Option<i64>,
}

impl SupplyDecision {
    pub(crate) fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(json!({}))
    }
}

/// In-flight counters and affinity input gathered from live instances.
#[derive(Clone, Debug, Default)]
pub struct InFlight {
    /// Live instances per profile.
    pub per_profile: HashMap<String, i64>,
    /// Live instances per (profile, model).
    pub per_model: HashMap<(String, String), i64>,
    /// Live host per (profile, model) → warm-cache affinity.
    pub warm: HashMap<(String, String), HashSet<String>>,
}

impl InFlight {
    /// Build from live instance rows (same lifecycle set as host counts).
    #[must_use]
    pub fn from_live(instances: &[InstanceRecord]) -> Self {
        let mut out = Self::default();
        for instance in instances {
            let Some(profile_id) = instance.provider_profile_id.as_deref() else {
                continue;
            };
            if !is_live_lifecycle(&instance.lifecycle) {
                continue;
            }
            *out.per_profile.entry(profile_id.to_string()).or_default() += 1;
            if let Some(model) = instance.model.as_deref() {
                *out.per_model
                    .entry((profile_id.to_string(), model.to_string()))
                    .or_default() += 1;
                out.warm
                    .entry((profile_id.to_string(), model.to_string()))
                    .or_default()
                    .insert(instance.host_id.clone());
            }
        }
        out
    }
}

fn is_live_lifecycle(lifecycle: &str) -> bool {
    matches!(
        lifecycle,
        "preparing" | "starting" | "ready" | "running" | "closing" | "reconciling"
    )
}

/// Per-candidate mutable solve state; admission rejects accumulate reasons.
struct Evaluated {
    candidate: SupplyCandidate,
    state: SupplyState,
    min_load_host: Option<(String, f64)>,
}

/// Solve inputs; the Hub layer fills these from the store.
pub struct SolveInput<'a> {
    /// All provider profiles visible to the placement.
    pub profiles: &'a [ProviderRecord],
    /// Placement-eligible live hosts.
    pub hosts: &'a [HostRecord],
    /// Dispatch task spec.
    pub task: &'a TaskSpec,
    /// Live in-flight counters.
    pub in_flight: &'a InFlight,
    /// Is the caller a coordinator seat (unlocks `coordinator-only` reserve)?
    pub caller_is_coordinator: bool,
    /// Clock instant (epoch seconds).
    pub now: i64,
}

type ModelSupplyFields = (
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Vec<String>,
    Vec<RateLimitWindow>,
    bool,
);

fn model_fields(model: &crate::provider_models::ProviderModel) -> ModelSupplyFields {
    (
        model.family.clone(),
        model.role.clone(),
        model.priority,
        model.concurrency_max,
        model.fallback.clone(),
        model.windows.clone(),
        model.workhorse,
    )
}

/// Build the candidate list from stored profiles and eligible hosts.
#[must_use]
pub fn build_candidates(input: &SolveInput<'_>) -> Vec<SupplyCandidate> {
    let host_ids: HashSet<&str> = input.hosts.iter().map(|h| h.host_id.as_str()).collect();
    let mut candidates = Vec::new();
    for profile in input.profiles {
        // With no placement hosts yet (a planning dry-run before any node is
        // enrolled), universal/native profiles still solve — only host-scoped
        // profiles require their bound host to be among the eligible set.
        let no_hosts_yet = host_ids.is_empty();
        let allowed_hosts: Vec<&str> = if no_hosts_yet {
            if profile.scope == "universal" || profile.scope.is_empty() {
                Vec::new()
            } else {
                continue;
            }
        } else {
            host_ids
                .iter()
                .copied()
                .filter(|host_id| {
                    crate::provider_resolve::profile_allowed_on_host(&profile.scope, host_id)
                })
                .collect()
        };
        if !no_hosts_yet && allowed_hosts.is_empty() {
            continue;
        }
        let supply = &profile.supply;
        let host_load: HashMap<String, f64> = input
            .hosts
            .iter()
            .filter(|host| allowed_hosts.contains(&host.host_id.as_str()))
            .map(|host| {
                let cap = host.max_instances.max(1) as f64;
                (host.host_id.clone(), host.instance_count as f64 / cap)
            })
            .collect();
        let host_id_vec: Vec<String> = allowed_hosts.iter().map(|s| (*s).to_string()).collect();
        for model in profile.models.iter().filter(|m| m.enabled) {
            let (family_decl, role, model_priority, model_conc, fallback, model_windows, workhorse) =
                model_fields(model);
            let catalog_row = model_catalog::lookup(&model.id);
            let family = family_decl
                .or_else(|| catalog_row.map(|row| row.family.to_string()))
                .unwrap_or_else(|| model.id.clone());
            let class = model_catalog::class_of(&model.id, role.as_deref())
                .or_else(|| workhorse.then_some(ModelClass::Workhorse))
                .unwrap_or(ModelClass::Workhorse);
            let context_window = model
                .context_window
                .or_else(|| catalog_row.map(|row| row.context_window));
            let effort_levels = catalog_row
                .map(|row| row.effort_levels.iter().map(|s| (*s).to_string()).collect())
                .unwrap_or_default();
            let mut windows = supply.windows.clone();
            windows.extend(model_windows.iter().cloned());
            let unknown_supply = supply.windows.is_empty() && model_windows.is_empty();
            let pinned = input.task.pin.as_ref().is_some_and(|pin| {
                pin.supply_id.as_deref() == Some(profile.id.as_str())
                    && pin.model.as_deref().is_none_or(|id| id == model.id)
            });
            candidates.push(SupplyCandidate {
                profile_id: profile.id.clone(),
                profile_name: profile.name.clone(),
                model_id: model.id.clone(),
                family,
                class,
                context_window,
                effort_levels,
                fallback,
                profile_priority: supply.priority,
                model_priority,
                concurrency_max: supply.concurrency.max,
                model_concurrency_max: model_conc,
                reserve: supply.reserve,
                windows,
                explicit_state: supply.state,
                unknown_supply,
                input_price: catalog_row.and_then(|row| row.input_price_per_mtok()),
                host_ids: host_id_vec.clone(),
                host_load: host_load.clone(),
                warm_host_ids: input
                    .in_flight
                    .warm
                    .get(&(profile.id.clone(), model.id.clone()))
                    .cloned()
                    .unwrap_or_default(),
                pinned,
            });
        }
    }
    candidates
}

// ── admission ──────────────────────────────────────────────────────────────

fn evaluate(mut candidate: SupplyCandidate, input: &SolveInput<'_>) -> Evaluated {
    let state = refresh_state(
        &mut candidate.windows,
        Some(candidate.explicit_state),
        input.now,
    );
    // Pick the least-loaded allowed host for the load tiebreak.
    let min_load_host = candidate
        .host_ids
        .iter()
        .map(|id| {
            (
                id.clone(),
                candidate.host_load.get(id).copied().unwrap_or(0.0),
            )
        })
        .min_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
    Evaluated {
        candidate,
        state,
        min_load_host,
    }
}

/// Admission filter: capability then supply. Returns rejection reasons.
fn reject(ev: &Evaluated, input: &SolveInput<'_>, in_flight: &InFlight) -> Vec<String> {
    let candidate = &ev.candidate;
    let task = input.task;
    let mut reasons = Vec::new();

    // Capability filter (§4.4 step 1). `minClass` defaults to workhorse:
    // cheap models are opt-in for a task, frontier is requested explicitly.
    let min_class = task.min_class.unwrap_or(ModelClass::Workhorse);
    if candidate.class.rank() < min_class.rank() {
        reasons.push(format!(
            "{} class {} below minClass {}",
            candidate.model_id,
            wire_class(candidate.class),
            wire_class(min_class)
        ));
    }
    if let Some(needed) = task
        .context_need
        .expected_input_tokens
        .as_ref()
        .map(|v| v.0)
        .filter(|n| *n > 0)
        && let Some(window) = candidate.context_window
        && window < needed
    {
        reasons.push(format!(
            "{} contextWindow {window} < expectedInputTokens {needed}",
            candidate.model_id
        ));
    }
    if task.context_need.needs_long_context
        && candidate
            .context_window
            .is_some_and(|window| window < 1_000_000)
    {
        reasons.push(format!("{} needs a 1m context window", candidate.model_id));
    }
    if let Some(effort) = task.effort.as_deref()
        && !candidate.effort_levels.is_empty()
        && !candidate.effort_levels.iter().any(|level| level == effort)
    {
        reasons.push(format!(
            "{} does not support effort {effort}",
            candidate.model_id
        ));
    }

    // Pinned supplies skip the supply-state filters: the pin is an explicit
    // override and the ledger reports the state honestly (§4.3 pin semantics).
    if !candidate.pinned {
        // Reserve (§4.4 step 2).
        if candidate.reserve == SupplyReserve::CoordinatorOnly && !input.caller_is_coordinator {
            reasons.push(format!(
                "{} is reserved for coordinators",
                candidate.profile_name
            ));
        }

        // Gateway/unknown supply: only when pinned (handled) or everything
        // else is exhausted — the latter is applied after filtering in solve.
        let unknown = ev.state == SupplyState::Unknown || candidate.unknown_supply;
        if unknown {
            reasons.push(format!(
                "{} supply state unknown (no readable windows)",
                candidate.profile_name
            ));
        } else if matches!(ev.state, SupplyState::Cooling | SupplyState::Exhausted) {
            reasons.push(format!(
                "{} is {} until cooldown/reset",
                candidate.profile_name,
                wire_state(ev.state)
            ));
        }

        // Window-level cooling for this exact family (degraded accounts).
        for window in &candidate.windows {
            if window.applies_to_family(&candidate.family) && window.is_cooling_at(input.now) {
                let until = window.cooldown_until.unwrap_or(input.now);
                reasons.push(format!(
                    "{} family {} window '{}' cooling until {until}",
                    candidate.model_id, candidate.family, window.id
                ));
            }
        }

        // Account-level concurrency ceiling — the missing primitive (§4.4 ②).
        if let Some(max) = candidate.concurrency_max
            && max >= 0
        {
            let count = in_flight
                .per_profile
                .get(&candidate.profile_id)
                .copied()
                .unwrap_or(0);
            if count >= max {
                reasons.push(format!(
                    "{} at account concurrency.max {max} ({count} in flight)",
                    candidate.profile_name
                ));
            }
        }
        if let Some(max) = candidate.model_concurrency_max
            && max >= 0
        {
            let count = in_flight
                .per_model
                .get(&(candidate.profile_id.clone(), candidate.model_id.clone()))
                .copied()
                .unwrap_or(0);
            if count >= max {
                reasons.push(format!(
                    "{} at model concurrency.max {max} ({count} in flight)",
                    candidate.model_id
                ));
            }
        }
    }
    reasons
}

fn wire_class(class: ModelClass) -> &'static str {
    match class {
        ModelClass::Cheap => "cheap",
        ModelClass::Workhorse => "workhorse",
        ModelClass::Frontier => "frontier",
    }
}

fn wire_state(state: SupplyState) -> &'static str {
    match state {
        SupplyState::Available => "available",
        SupplyState::Degraded => "degraded",
        SupplyState::Cooling => "cooling",
        SupplyState::Exhausted => "exhausted",
        SupplyState::Unknown => "unknown",
    }
}

/// Strict rank comparator implementing §4.4 step 4:
/// priority → remaining window share → cooldown freshness → warm affinity →
/// cost (cost-sensitive only) → host load → stable ids.
fn rank_cmp(a: &Evaluated, b: &Evaluated, input: &SolveInput<'_>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let ca = &a.candidate;
    let cb = &b.candidate;

    // 1. User-declared priority is the objective function.
    match cb.effective_priority().cmp(&ca.effective_priority()) {
        Ordering::Equal => {}
        ord => return ord,
    }
    // 2. Remaining declared share (more remaining first).
    let (ra, rb) = (ca.remaining_share(input.now), cb.remaining_share(input.now));
    match rb.partial_cmp(&ra).unwrap_or(Ordering::Equal) {
        Ordering::Equal => {}
        ord => return ord,
    }
    // 3. Cooldown freshness (later cooldown = worse; no cooldown first).
    let (ca_cd, cb_cd) = (ca.latest_cooldown(input.now), cb.latest_cooldown(input.now));
    match ca_cd.unwrap_or(i64::MIN).cmp(&cb_cd.unwrap_or(i64::MIN)) {
        Ordering::Equal => {}
        ord => return ord,
    }
    // 4. Warm-cache affinity for latency-sensitive tasks.
    if input.task.latency_sensitivity == Sensitivity::High {
        let warm_a = a
            .min_load_host
            .as_ref()
            .is_some_and(|(host, _)| ca.warm_host_ids.contains(host));
        let warm_b = b
            .min_load_host
            .as_ref()
            .is_some_and(|(host, _)| cb.warm_host_ids.contains(host));
        match warm_b.cmp(&warm_a) {
            Ordering::Equal => {}
            ord => return ord,
        }
    }
    // 5. Cost only matters when the task asked for it (no inverse-price
    //    weighting — subscription marginal cost is 0 until exhausted, §4.4).
    if input.task.cost_sensitivity == Sensitivity::High {
        let cost_a = ca.input_price.unwrap_or(0.0);
        let cost_b = cb.input_price.unwrap_or(0.0);
        match cost_a.partial_cmp(&cost_b).unwrap_or(Ordering::Equal) {
            Ordering::Equal => {}
            ord => return ord,
        }
    }
    // 6. Existing host load, then stable ids.
    let load_a = a
        .min_load_host
        .as_ref()
        .map(|(_, load)| *load)
        .unwrap_or(0.0);
    let load_b = b
        .min_load_host
        .as_ref()
        .map(|(_, load)| *load)
        .unwrap_or(0.0);
    match load_a.partial_cmp(&load_b).unwrap_or(Ordering::Equal) {
        Ordering::Equal => {}
        ord => return ord,
    }
    ca.profile_id
        .cmp(&cb.profile_id)
        .then_with(|| ca.model_id.cmp(&cb.model_id))
}

/// Run the full §4.4 solve over stored profiles/hosts and a task spec.
#[must_use]
pub fn solve(input: SolveInput<'_>) -> SupplyDecision {
    let raw = build_candidates(&input);
    let mut evaluated: Vec<Evaluated> = raw
        .into_iter()
        .map(|candidate| evaluate(candidate, &input))
        .collect();

    // A hard pin is an explicit model/supply choice: honor it regardless of
    // class filters, but still report other candidates in the ledger.
    if let Some(pin) = input.task.pin.as_ref()
        && (pin.model.is_some() || pin.supply_id.is_some())
        && let Some(pos) = evaluated.iter().position(|ev| ev.candidate.pinned)
    {
        let winner = evaluated.remove(pos);
        let others = evaluated
            .into_iter()
            .map(|ev| RejectedSupply {
                profile_id: ev.candidate.profile_id.clone(),
                profile_name: ev.candidate.profile_name.clone(),
                model_id: ev.candidate.model_id.clone(),
                reasons: vec!["not the pinned harness/model/supply".into()],
            })
            .collect();
        return finish(vec![winner], others, &input, true);
    }

    let mut admitted: Vec<Evaluated> = Vec::new();
    let mut rejected: Vec<RejectedSupply> = Vec::new();
    let mut unknown_pool: Vec<Evaluated> = Vec::new();
    for ev in evaluated {
        let reasons = reject(&ev, &input, input.in_flight);
        let is_unknown = ev.state == SupplyState::Unknown || ev.candidate.unknown_supply;
        if reasons.is_empty() {
            admitted.push(ev);
        } else if is_unknown
            && !reasons.iter().any(|r| r.contains("reserved"))
            && reasons.iter().all(|r| r.contains("unknown"))
        {
            unknown_pool.push(ev);
        } else {
            rejected.push(RejectedSupply {
                profile_id: ev.candidate.profile_id.clone(),
                profile_name: ev.candidate.profile_name.clone(),
                model_id: ev.candidate.model_id.clone(),
                reasons,
            });
        }
    }

    // Gateway profiles without windows: last resort once all windowed supply
    // is out, never silently preferred (design §8.3 #4).
    if admitted.is_empty() && !unknown_pool.is_empty() {
        admitted.append(&mut unknown_pool);
    } else {
        for ev in unknown_pool {
            rejected.push(RejectedSupply {
                profile_id: ev.candidate.profile_id.clone(),
                profile_name: ev.candidate.profile_name.clone(),
                model_id: ev.candidate.model_id.clone(),
                reasons: vec!["supply state unknown; windowed supply is still available".into()],
            });
        }
    }

    finish(admitted, rejected, &input, false)
}

fn finish(
    mut admitted: Vec<Evaluated>,
    rejected: Vec<RejectedSupply>,
    input: &SolveInput<'_>,
    pinned: bool,
) -> SupplyDecision {
    admitted.sort_by(|a, b| rank_cmp(a, b, input));
    let mut reasons = Vec::new();
    let mut deferred_until: Option<i64> = None;
    for rejected in &rejected {
        for reason in &rejected.reasons {
            if let Some(until) = parse_cooling_until(reason) {
                deferred_until = Some(deferred_until.map_or(until, |existing| existing.min(until)));
            }
        }
    }
    let chosen = admitted.first().map(|ev| {
        let candidate = &ev.candidate;
        if !pinned {
            reasons.push(format!(
                "priority {} wins; {}% window headroom",
                candidate.effective_priority(),
                (candidate.remaining_share(input.now) * 100.0).round() as u8
            ));
            if input.task.cost_sensitivity == Sensitivity::High
                && let Some(price) = candidate.input_price
            {
                reasons.push(format!(
                    "cost-sensitive task; ${price}/MTok input (estimated)"
                ));
            }
        }
        ChosenSupply {
            profile_id: candidate.profile_id.clone(),
            model_id: candidate.model_id.clone(),
            family: candidate.family.clone(),
            host_id: ev.min_load_host.as_ref().map(|(id, _)| id.clone()),
            fallback: candidate.fallback.clone(),
        }
    });
    if chosen.is_none() {
        reasons.push(
            "no supply passed capability+admission filters; task is deferred, not downgraded"
                .into(),
        );
    }
    let ranked: Vec<RankedSupply> = admitted
        .iter()
        .map(|ev| RankedSupply {
            profile_id: ev.candidate.profile_id.clone(),
            model_id: ev.candidate.model_id.clone(),
            family: ev.candidate.family.clone(),
            priority: ev.candidate.effective_priority(),
            remaining_share: ev.candidate.remaining_share(input.now),
            class: wire_class(ev.candidate.class).into(),
            state: wire_state(ev.state).into(),
        })
        .collect();
    let deferred = ranked.is_empty();
    SupplyDecision {
        chosen,
        ranked,
        rejected,
        reasons,
        deferred,
        deferred_until,
    }
}

fn parse_cooling_until(reason: &str) -> Option<i64> {
    let marker = "cooling until ";
    let start = reason.find(marker)? + marker.len();
    let rest = &reason[start..];
    let end = rest
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit() && *c != '-')
        .map_or(rest.len(), |(i, _)| i);
    rest[..end].parse().ok()
}

// ── journal feedback (observed-textual, §4.6 Detect) ───────────────────────

/// Classify one observed event and, on a real 429, cool the model's family
/// window on its profile. Returns the classified signal so the caller can
/// record the matching audit/ledger lines. A 529 is recorded but cools
/// nothing.
pub fn observe_textual_event(
    windows: &mut Vec<RateLimitWindow>,
    family: &str,
    status: Option<u16>,
    text: &str,
    now: i64,
) -> Option<RateSignal> {
    let signal = classify_rate_signal(status, text)?;
    match signal {
        RateSignal::RateLimited => {
            apply_family_rate_hit(windows, family, now);
            Some(signal)
        }
        RateSignal::FleetOverloaded => Some(signal),
    }
}

// ── placement ledger migration (batch 4 adds the task-side placements table) ─

/// Ledger of every supply solve; design §5.6. One row per
/// dispatch/resolve, carrying the machine-readable decision (`reasons[]` /
/// `rejected[]`) that is both audit trail and bot card body.
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS placement_ledger (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            instance_id TEXT,
            project_id TEXT,
            profile_id TEXT,
            model_id TEXT,
            host_id TEXT,
            decision_json TEXT NOT NULL,
            created_at TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS placement_ledger_instance ON placement_ledger(instance_id);
         CREATE INDEX IF NOT EXISTS placement_ledger_project ON placement_ledger(project_id);",
    )?;
    Ok(())
}

// ── Hub layer: solve over the store, REST dry-run, journal-text feedback ────

use crate::AppState;
use crate::HubError;
use crate::agent_scope::{caller, require_operator};
use crate::auth::require_origin;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;

fn now_epoch_secs() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Whether a caller may spend `coordinator-only` reserves: human/bot callers
/// always may; agent callers need the `dispatch` grant.
async fn caller_is_coordinator(state: &AppState, headers: &HeaderMap) -> Result<bool, HubError> {
    let device = caller(state, headers).await?;
    if crate::agent_scope::origin(&device) != remuda_protocol::InputOrigin::Agent {
        return Ok(true);
    }
    let Some(instance_id) = device.instance_id.as_deref() else {
        return Ok(false);
    };
    let Some(instance) = state.store.get_instance(instance_id.to_string()).await? else {
        return Ok(false);
    };
    Ok(instance
        .grants
        .iter()
        .any(|verb| verb == "dispatch" || verb == "spend"))
}

/// Gather inputs and run a solve against live Hub state.
pub async fn solve_state(
    state: &AppState,
    task: &TaskSpec,
    hosts: &[HostRecord],
    coordinator: bool,
) -> Result<SupplyDecision, HubError> {
    let profiles = state.store.list_providers(None).await?;
    let live = state.store.list_live_instances().await?;
    let in_flight = InFlight::from_live(&live);
    let now = now_epoch_secs();
    Ok(solve(SolveInput {
        profiles: &profiles,
        hosts,
        task,
        in_flight: &in_flight,
        caller_is_coordinator: coordinator,
        now,
    }))
}

/// Routes for supply solving/dry-runs. Per-profile declaration and observed
/// events live under `/v1/providers/{id}/supply*` (see `providers.rs`).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/supply/resolve", post(resolve_supply_http))
        .route("/v1/supply/catalog", axum::routing::get(catalog_http))
        .route(
            "/v1/instances/{id}/usage",
            axum::routing::get(instance_usage_http),
        )
}

/// `GET /v1/instances/:id/usage` — journaled usage totals for one instance
/// (§4.5 budget reconciliation; all costs are estimates).
async fn instance_usage_http(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, HubError> {
    require_operator(&state, &headers).await?;
    state
        .store
        .get_instance(id.clone())
        .await?
        .ok_or(HubError::NotFound)?;
    let usage_id = id.clone();
    let agg = state
        .store
        .run(move |conn| {
            crate::usage_store::aggregate_instance(conn, &usage_id)
                .map_err(crate::store::StoreError::from)
        })
        .await
        .map_err(crate::http::map_store)?;
    Ok(Json(json!({
        "instanceId": id,
        "events": agg.events,
        "totalTokens": agg.total_tokens,
        "inputTokens": agg.input_tokens,
        "outputTokens": agg.output_tokens,
        "estimatedUsd": agg.cost_usd,
        "estimated": true,
    })))
}

/// `GET /v1/supply/catalog` — the built-in capability table (revisioned).
async fn catalog_http(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, HubError> {
    require_operator(&state, &headers).await?;
    let rows: Vec<Value> = model_catalog::catalog()
        .iter()
        .map(|row| {
            json!({
                "id": row.id,
                "family": row.family,
                "aliases": row.aliases,
                "class": wire_class(row.class),
                "contextWindow": row.context_window,
                "maxOutputTokens": row.max_output_tokens,
                "effortLevels": row.effort_levels,
                "supports": {
                    "toolChoiceAny": row.supports.tool_choice_any,
                    "structuredOutput": row.supports.structured_output,
                    "vision": row.supports.vision,
                    "promptCaching": row.supports.prompt_caching,
                    "assistantPrefill": row.supports.assistant_prefill,
                },
                "suitedFor": row.suited_for,
                "inputPricePerMtok": row.input_price_per_mtok(),
                "priceProvisional": row.price_is_provisional(),
            })
        })
        .collect();
    Ok(Json(json!({
        "revision": model_catalog::CATALOG_REVISION,
        "updated": model_catalog::CATALOG_UPDATED,
        "models": rows,
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResolveSupplyBody {
    /// Dispatch task spec; §4.3.
    task_spec: TaskSpec,
    /// Shorthand placement (`{"kind":"project","projectId":…}` etc.).
    #[serde(default)]
    placement: Option<Value>,
    /// Explicit host id.
    #[serde(default)]
    host_id: Option<String>,
}

/// `POST /v1/supply/resolve` — "what would run where?" dry run. Never spawns,
/// never writes the ledger; returns the full ranked decision.
async fn resolve_supply_http(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ResolveSupplyBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    require_operator(&state, &headers).await?;
    let placement =
        crate::placement::Placement::from_value(body.placement.as_ref(), body.host_id.as_deref())?;
    let hosts =
        crate::placement::pick_hosts(&state, &placement, &crate::placement::PlaceSpec::default())
            .await?;
    let coordinator = caller_is_coordinator(&state, &headers).await?;
    let decision = solve_state(&state, &body.task_spec, &hosts, coordinator).await?;
    let mut value = decision.to_json();
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "catalogRevision".into(),
            json!(model_catalog::CATALOG_REVISION),
        );
        obj.insert(
            "catalogUpdated".into(),
            json!(model_catalog::CATALOG_UPDATED),
        );
        obj.insert(
            "hosts".into(),
            json!(hosts.iter().map(|h| h.host_id.clone()).collect::<Vec<_>>()),
        );
    }
    Ok(Json(value))
}

/// Recursively collect string values from a journal event for line-shape
/// matching. Bounded so a giant transcript blob does not dominate scanning.
fn event_text(value: &Value, out: &mut String) {
    if out.len() > 16_384 {
        return;
    }
    match value {
        Value::String(text) => {
            out.push_str(text);
            out.push('\n');
        }
        Value::Array(items) => items.iter().for_each(|item| event_text(item, out)),
        Value::Object(map) => map.values().for_each(|item| event_text(item, out)),
        _ => {}
    }
}

/// Observe one fresh journal append and cool supply on a real 429 text
/// (observed-textual, §4.2 source 2 / §4.6 Detect). 529/overloaded text is
/// classified but cools nothing; only the audit line records it.
///
/// Structured Codex `account/rateLimits/updated` frames are NOT relayed by the
/// Node yet: `remuda-codex-wire` decodes the notification variant
/// (`notification.rs:106 AccountRateLimitsUpdated`) but zero hubnode frames
/// carry it. The typed hook point when P6 wires that relay is the
/// `report_supply_event` HTTP handler, which calls [`apply_structured_windows`]
/// on the profile's supply; until then the REST
/// `POST /v1/providers/{id}/supply/events {"type":"structured", …}` path feeds
/// the exact same merge function.
pub async fn observe_journal_text(state: &AppState, record: &crate::store::JournalRecord) {
    let kind = record.event.get("kind").and_then(Value::as_str);
    if !matches!(
        kind,
        Some("message") | Some("lifecycle") | Some("opaque") | Some("tool_result")
    ) {
        return;
    }
    let mut text = String::new();
    event_text(&record.event, &mut text);
    let Some(signal) = classify_rate_signal(None, &text) else {
        return;
    };
    let Some(instance) = state
        .store
        .get_instance(record.instance_id.clone())
        .await
        .ok()
        .flatten()
    else {
        return;
    };
    let Some(profile_id) = instance
        .provider_profile_id
        .filter(|id| crate::provider_resolve::is_real_profile_id(id))
    else {
        return;
    };
    let Some(profile) = state.store.get_provider(profile_id).await.ok().flatten() else {
        return;
    };
    let mut supply = profile.supply.clone();
    match signal {
        RateSignal::FleetOverloaded => {
            supply.last_error = Some(format!(
                "upstream fleet overload at {} (529); window not cooled",
                record.observed_at
            ));
        }
        RateSignal::RateLimited => {
            let model_id = instance.model.clone().unwrap_or_default();
            let family = family_of(&profile, &model_id);
            apply_family_rate_hit(&mut supply.windows, &family, now_epoch_secs());
            supply.state = refresh_state(&mut supply.windows, None, now_epoch_secs());
            supply.cooldown_until = supply
                .windows
                .iter()
                .filter(|w| w.is_account_level())
                .filter_map(|w| w.cooldown_until)
                .max();
            supply.last_error = Some(format!("429/rate-limit text for family {family}"));
            if let Err(error) = state
                .store
                .update_provider_supply(profile.id.clone(), supply.clone())
                .await
            {
                tracing::warn!(%error, "supply.cooldown persist failed");
                return;
            }
            let _ = state
                .store
                .append_audit(
                    "hub".into(),
                    "supply.cooldown".into(),
                    Some(profile.id.clone()),
                    json!({
                        "source": "observed-textual",
                        "instanceId": record.instance_id,
                        "family": family,
                        "signal": "rate-limited",
                    }),
                )
                .await;
        }
    }
}

/// Resolve the rate-limit family bucket of a running model: declared model
/// family first, then the capability catalog, then the model id itself.
#[must_use]
pub fn family_of(profile: &ProviderRecord, model_id: &str) -> String {
    profile
        .models
        .iter()
        .find(|m| m.id == model_id)
        .and_then(|m| m.family.clone())
        .or_else(|| model_catalog::lookup(model_id).map(|row| row.family.to_string()))
        .unwrap_or_else(|| model_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_models::ProviderModel;
    use crate::store::ProviderRecord;

    const NOW: i64 = 1_757_900_000;

    fn profile_with(
        id: &str,
        name: &str,
        supply: remuda_protocol::SupplyProfile,
        models: Vec<ProviderModel>,
    ) -> ProviderRecord {
        ProviderRecord {
            id: id.into(),
            name: name.into(),
            kind: "gateway".into(),
            base_url: "http://example.invalid".into(),
            models,
            default_model: None,
            headers: std::collections::BTreeMap::new(),
            default_gateway: false,
            scope: "universal".into(),
            revision: 1,
            secret_name: None,
            secret_present: false,
            secret_last4: None,
            secret_fingerprint: None,
            last_test_ok: None,
            last_test_at: None,
            last_test_message: None,
            created_at: "2026-09-15T00:00:00.000Z".into(),
            updated_at: "2026-09-15T00:00:00.000Z".into(),
            supply,
        }
    }

    fn host(id: &str, max: i64, instance_count: i64) -> HostRecord {
        HostRecord {
            workspaces: Vec::new(),
            workspace_revision: 0,
            ssh: None,
            last_error: None,
            host_id: id.into(),
            label: id.into(),
            state: "online".into(),
            online: true,
            last_seen_at: None,
            node_version: None,
            cli: serde_json::json!([]),
            capabilities: serde_json::json!({}),
            instance_count,
            transport: "outbound-wss".into(),
            labels: Vec::new(),
            herdr: None,
            resources: None,
            max_instances: max,
            hostname: None,
            provider_binding: "auto".into(),
            default_launch_args: None,
            claude_binary_path: None,
        }
    }

    fn model(id: &str, family: &str, role: &str, priority: i64) -> ProviderModel {
        ProviderModel {
            id: id.into(),
            enabled: true,
            label: None,
            context_window: Some(1_048_576),
            tags: vec!["1m".into()],
            surfaces: vec![],
            family: Some(family.into()),
            role: Some(role.into()),
            priority: Some(priority),
            concurrency_max: None,
            fallback: Vec::new(),
            windows: Vec::new(),
            workhorse: role == "workhorse",
        }
    }

    fn task(min_class: Option<ModelClass>) -> TaskSpec {
        TaskSpec {
            min_class,
            ..Default::default()
        }
    }

    fn observed_zero_window() -> Vec<RateLimitWindow> {
        vec![RateLimitWindow {
            id: "primary".into(),
            applies_to: vec!["*".into()],
            limit: None,
            window_duration_mins: Some(300),
            used_percent: Some(0.0),
            resets_at: None,
            source: WindowSource::Observed,
            observed_at: Some(NOW),
            cooldown_until: None,
            backoff_attempts: 0,
        }]
    }

    fn input<'a>(
        profiles: &'a [ProviderRecord],
        hosts: &'a [HostRecord],
        task: &'a TaskSpec,
        in_flight: &'a InFlight,
    ) -> SolveInput<'a> {
        SolveInput {
            profiles,
            hosts,
            task,
            in_flight,
            caller_is_coordinator: false,
            now: NOW,
        }
    }

    #[test]
    fn classifies_429_and_529_oppositely() {
        assert_eq!(
            classify_rate_signal(Some(429), "Request rejected (429) {\"error_code\":-2001}"),
            Some(RateSignal::RateLimited)
        );
        assert_eq!(
            classify_rate_signal(None, "You've hit your session limit"),
            Some(RateSignal::RateLimited)
        );
        assert_eq!(
            classify_rate_signal(Some(529), "overloaded"),
            Some(RateSignal::FleetOverloaded)
        );
        assert_eq!(
            classify_rate_signal(Some(529), "random 529 text"),
            Some(RateSignal::FleetOverloaded)
        );
        assert_eq!(classify_rate_signal(Some(200), "all good"), None);
    }

    #[test]
    fn family_429_parks_family_but_siblings_stay_open_then_backoff_ladder() {
        let mut windows = Vec::new();
        apply_family_rate_hit(&mut windows, "es1", NOW);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].applies_to, vec!["es1".to_string()]);
        assert_eq!(windows[0].cooldown_until, Some(NOW + 60));
        assert!(windows[0].is_cooling_at(NOW + 30));
        // A sibling family is not governed by the window.
        assert!(!windows[0].applies_to_family("seed"));
        // Repeated hits without a reset climb the ladder 60→120→300→900→900.
        for (at, wait) in [(120, 120), (300, 300), (900, 900), (1800, 900)] {
            let t = NOW + at;
            let state = refresh_state(&mut windows, None, t);
            assert_eq!(state, SupplyState::Available, "cooldown expired at {at}");
            apply_family_rate_hit(&mut windows, "es1", t);
            assert_eq!(
                windows[0].cooldown_until,
                Some(t + wait),
                "at {at} wait {wait}"
            );
        }
    }

    #[test]
    fn known_reset_time_parks_until_reset_not_backoff() {
        let observed = ObservedRateLimits {
            windows: vec![RateLimitWindow {
                id: "primary".into(),
                applies_to: vec!["es1".into()],
                limit: None,
                window_duration_mins: Some(300),
                used_percent: Some(100.0),
                resets_at: Some(NOW + 1800),
                source: WindowSource::Observed,
                observed_at: Some(NOW),
                cooldown_until: None,
                backoff_attempts: 0,
            }],
        };
        let mut windows = vec![RateLimitWindow::declared(
            "primary",
            vec!["es1".into()],
            Some(300),
        )];
        apply_structured_windows(&mut windows, &observed, NOW);
        // Declared window is updated in place, observed does not duplicate it.
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].cooldown_until, Some(NOW + 1800));
        assert_eq!(windows[0].used_percent, Some(100.0));
    }

    #[test]
    fn account_level_window_parks_every_family() {
        let observed = ObservedRateLimits {
            windows: vec![RateLimitWindow {
                id: "weekly".into(),
                applies_to: vec!["*".into()],
                limit: None,
                window_duration_mins: Some(10080),
                used_percent: Some(100.0),
                resets_at: Some(NOW + 3600),
                source: WindowSource::Observed,
                observed_at: Some(NOW),
                cooldown_until: None,
                backoff_attempts: 0,
            }],
        };
        let mut windows = Vec::new();
        apply_structured_windows(&mut windows, &observed, NOW);
        assert_eq!(refresh_state(&mut windows, None, NOW), SupplyState::Cooling);
        assert!(windows[0].applies_to_family("es1"));
        assert!(windows[0].applies_to_family("seed"));
    }

    #[test]
    fn rank_priority_then_share_then_load() {
        let mut high_priority = remuda_protocol::SupplyProfile {
            priority: 20,
            ..Default::default()
        };
        high_priority.windows = vec![RateLimitWindow {
            id: "primary".into(),
            applies_to: vec!["*".into()],
            limit: None,
            window_duration_mins: Some(300),
            used_percent: Some(90.0),
            resets_at: None,
            source: WindowSource::Observed,
            observed_at: Some(NOW),
            cooldown_until: None,
            backoff_attempts: 0,
        }];
        let lower_with_headroom = remuda_protocol::SupplyProfile {
            priority: 10,
            windows: vec![RateLimitWindow {
                id: "primary".into(),
                applies_to: vec!["*".into()],
                limit: None,
                window_duration_mins: Some(300),
                used_percent: Some(10.0),
                resets_at: None,
                source: WindowSource::Observed,
                observed_at: Some(NOW),
                cooldown_until: None,
                backoff_attempts: 0,
            }],
            ..Default::default()
        };
        let profiles = vec![
            profile_with(
                "pvp_busy",
                "busy-high-priority",
                high_priority,
                vec![model("claude-sonnet-5", "sonnet", "workhorse", 20)],
            ),
            profile_with(
                "pvp_fresh",
                "fresh-lower-priority",
                lower_with_headroom,
                vec![model("gpt-5-mini", "gpt-5-mini", "workhorse", 10)],
            ),
        ];
        let hosts = [host("hst_a", 8, 0)];
        let decision = solve(input(&profiles, &hosts, &task(None), &InFlight::default()));
        // Priority beats window headroom: the user's declared order IS the
        // objective function.
        assert_eq!(decision.chosen.as_ref().unwrap().profile_id, "pvp_busy");
        assert_eq!(decision.ranked.len(), 2);
    }

    #[test]
    fn concurrency_max_rejects_even_when_hosts_are_empty() {
        let supply = remuda_protocol::SupplyProfile {
            priority: 20,
            concurrency: remuda_protocol::SupplyConcurrency { max: Some(2) },
            ..Default::default()
        };
        let profiles = vec![profile_with(
            "pvp_cap",
            "capped",
            supply,
            vec![model("claude-sonnet-5", "sonnet", "workhorse", 20)],
        )];
        let hosts = [host("hst_a", 8, 0), host("hst_b", 8, 0)];
        let mut in_flight = InFlight::default();
        in_flight.per_profile.insert("pvp_cap".to_string(), 2);
        let decision = solve(input(&profiles, &hosts, &task(None), &in_flight));
        assert!(decision.deferred);
        assert!(
            decision.rejected[0]
                .reasons
                .iter()
                .any(|r| r.contains("concurrency.max 2"))
        );
        // Drop below the ceiling and the candidate admits despite both hosts
        // being far from their own maxInstances.
        in_flight.per_profile.insert("pvp_cap".to_string(), 1);
        let decision = solve(input(&profiles, &hosts, &task(None), &in_flight));
        assert!(!decision.deferred);
    }

    #[test]
    fn min_class_defers_never_downgrades() {
        let supply = remuda_protocol::SupplyProfile::default();
        let profiles = vec![profile_with(
            "pvp_cheap",
            "cheap-only",
            supply,
            vec![model("claude-haiku-4-5", "haiku", "cheap", 10)],
        )];
        let hosts = [host("hst_a", 8, 0)];
        let decision = solve(input(
            &profiles,
            &hosts,
            &task(Some(ModelClass::Frontier)),
            &InFlight::default(),
        ));
        assert!(decision.deferred);
        assert!(
            decision.rejected[0]
                .reasons
                .iter()
                .any(|r| r.contains("below minClass frontier"))
        );
        assert!(decision.chosen.is_none());
    }

    #[test]
    fn unknown_gateway_supply_is_last_resort_and_pin_wins() {
        // A gateway profile with no declared/observed windows.
        let gw = profile_with(
            "pvp_gw",
            "opaque-gateway",
            remuda_protocol::SupplyProfile::default(),
            vec![model("gw/anything[1m]", "es1", "workhorse", 0)],
        );
        let hosts = [host("hst_a", 8, 0)];
        // On its own: unknown pool is admitted (everything else exhausted).
        let decision = solve(input(
            std::slice::from_ref(&gw),
            &hosts,
            &task(None),
            &InFlight::default(),
        ));
        assert_eq!(decision.chosen.as_ref().unwrap().profile_id, "pvp_gw");

        // Add a windowed profile: the gateway drops to rejected.
        let windowed = profile_with(
            "pvp_visible",
            "windowed",
            remuda_protocol::SupplyProfile {
                priority: 5,
                windows: observed_zero_window(),
                ..Default::default()
            },
            vec![model("claude-sonnet-5", "sonnet", "workhorse", 5)],
        );
        let decision = solve(input(
            &[gw.clone(), windowed],
            &hosts,
            &task(None),
            &InFlight::default(),
        ));
        assert_eq!(decision.chosen.as_ref().unwrap().profile_id, "pvp_visible");
        assert!(decision.rejected.iter().any(|r| r.profile_id == "pvp_gw"));

        // An explicit pin overrides the unknown-state filter.
        let pinned = TaskSpec {
            pin: Some(remuda_protocol::TaskPin {
                harness: None,
                model: Some("gw/anything[1m]".into()),
                supply_id: Some("pvp_gw".into()),
            }),
            ..Default::default()
        };
        let decision = solve(input(
            &[
                gw,
                profile_with(
                    "pvp_visible2",
                    "windowed",
                    remuda_protocol::SupplyProfile {
                        windows: observed_zero_window(),
                        ..Default::default()
                    },
                    vec![model("claude-sonnet-5", "sonnet", "workhorse", 5)],
                ),
            ],
            &hosts,
            &pinned,
            &InFlight::default(),
        ));
        assert_eq!(decision.chosen.as_ref().unwrap().profile_id, "pvp_gw");
    }

    #[test]
    fn es1_incident_shape_family_cool_sibling_wins() {
        // §4.6 replay core: es1 family is 429ing; seed sibling on the SAME
        // account has been fine all along and sits in es1's fallback chain.
        let mut es1_model = model("gw/es1[1m]", "es1", "workhorse", 20);
        es1_model.fallback = vec!["gw/seed[1m]".into()];
        let mut supply = remuda_protocol::SupplyProfile {
            priority: 20,
            ..Default::default()
        };
        apply_family_rate_hit(&mut supply.windows, "es1", NOW);
        let profiles = vec![profile_with(
            "pvp_relay",
            "relay",
            supply,
            vec![es1_model, model("gw/seed[1m]", "seed", "workhorse", 18)],
        )];
        let hosts = [host("hst_a", 8, 0)];
        let decision = solve(input(&profiles, &hosts, &task(None), &InFlight::default()));
        let chosen = decision.chosen.as_ref().unwrap();
        assert_eq!(chosen.model_id, "gw/seed[1m]");
        assert_eq!(chosen.family, "seed");
        assert!(decision.rejected.iter().any(|r| r.model_id == "gw/es1[1m]"
            && r.reasons.iter().any(|reason| reason.contains("cooling"))));
    }

    #[test]
    fn coordinator_only_reserve_rejects_workers() {
        let supply = remuda_protocol::SupplyProfile {
            reserve: remuda_protocol::SupplyReserve::CoordinatorOnly,
            ..Default::default()
        };
        let profiles = vec![profile_with(
            "pvp_fable",
            "fable-account",
            supply,
            vec![model("claude-fable-5", "fable", "frontier", 99)],
        )];
        let hosts = [host("hst_a", 8, 0)];
        let worker = solve(input(&profiles, &hosts, &task(None), &InFlight::default()));
        assert!(worker.deferred, "worker must not reach reserve");
        let no_flight = InFlight::default();
        let worker_task = task(None);
        let coordinator = SolveInput {
            caller_is_coordinator: true,
            ..input(&profiles, &hosts, &worker_task, &no_flight)
        };
        let coordinator = solve(coordinator);
        assert_eq!(coordinator.chosen.as_ref().unwrap().profile_id, "pvp_fable");
    }

    #[test]
    fn textual_observer_does_nothing_on_529() {
        let mut windows = vec![RateLimitWindow::declared(
            "primary",
            vec!["*".to_string()],
            Some(300),
        )];
        let signal = observe_textual_event(&mut windows, "es1", Some(529), "529 overloaded", NOW);
        assert_eq!(signal, Some(RateSignal::FleetOverloaded));
        assert_eq!(
            refresh_state(&mut windows, None, NOW),
            SupplyState::Available
        );
        assert!(windows[0].cooldown_until.is_none());
    }

    #[test]
    fn backoff_ladder_is_capped() {
        let wait = |attempts| {
            BACKOFF_LADDER_SECS
                .get(attempts)
                .copied()
                .unwrap_or(*BACKOFF_LADDER_SECS.last().unwrap())
        };
        assert_eq!(wait(0), 60);
        assert_eq!(wait(1), 120);
        assert_eq!(wait(2), 300);
        assert_eq!(wait(3), 900);
        assert_eq!(wait(99), 900);
    }
}
