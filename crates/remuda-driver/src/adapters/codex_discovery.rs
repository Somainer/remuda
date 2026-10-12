//! Process-wide registry for codex generic-pty discovery.
//!
//! Identity model (c-usagefu r6 item 1): every driver launch gets its OWN
//! opaque instance key, shared by the [`crate::generic_pty::GenericPtyDriver`]
//! and the file adapter it spawned. All ownership evidence is scoped to
//! `(cwd, instance key)`:
//!
//! - **Sent inputs** ([`record_input`]) hold the prompts THIS instance
//!   dispatched in THIS cwd. A prompt sent to another pane (a second codex
//!   instance, a grok/claude pane) can never authorize a bind here.
//! - **Claims** ([`claim`]) remember the owning instance key per rollout
//!   path. A path released by a closed owner is never re-bound by a
//!   different instance; the same instance relaunching may reclaim it.
//! - **Launch windows** ([`DiscoveryWindow`]) count the adapters still
//!   discovering in one cwd while they have no evidence yet.
//!
//! The registry lives for the Node process lifetime, which is exactly the
//! scope within which two Remuda launches could race for one file; separate
//! processes/hosts have separate homes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use time::OffsetDateTime;

#[derive(Default)]
struct WindowEntry {
    /// Live adapters currently discovering in this cwd.
    count: u32,
    /// True while another window overlaps (count >= 2).
    tainted: bool,
}

static OPEN_WINDOWS: LazyLock<Mutex<HashMap<PathBuf, WindowEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// cwd -> set of instance keys with an open discovery window (unbound
/// adapters that may still match a new rollout). Used for EXCLUSIVE
/// attribution (c-usagefu r7 item 1): a candidate compatible with more than
/// one live discoverer is ambiguous.
static OPEN_DISCOVERERS: LazyLock<Mutex<HashMap<PathBuf, HashSet<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// rollout path -> owning instance key.
static CLAIMED_ROLLOUTS: LazyLock<Mutex<HashMap<PathBuf, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// (canonical cwd, native session id) -> owning instance key. Lets a
/// poll re-recognize a session it ALREADY claimed (e.g. identical prompt
/// text re-run) without re-scanning the whole file.
static CLAIMED_SESSIONS: LazyLock<Mutex<HashMap<(PathBuf, String), String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Canonical key for one rollout path / cwd (best effort: a non-existent or
/// unreadable path keeps its lexical form).
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

// ---------------------------------------------------------------------------
// Instance LIFETIMES (c-usagefu r9 item 1)
// ---------------------------------------------------------------------------

/// How long a CLOSED instance's lifetime is retained after close. Prompt-only
/// attribution windows span minutes ([`INPUT_MATCH_WINDOW`]); a record older
/// than this can no longer overlap any plausible discovery decision, so it is
/// pruned on the next registration in that cwd. The bound exists because the
/// registry lives for the whole Node process.
const LIFETIME_RETENTION: time::Duration = time::Duration::hours(1);

/// One driver launch's live interval in a cwd.
struct InstanceLifetime {
    /// The agent.start boundary, sampled BEFORE the child was spawned. `None`
    /// means the launch boundary is UNKNOWN: a promoted (hand-typed) instance
    /// the operator started before this Node detected it. Such an interval
    /// makes the cwd's census incomplete, so prompt-only attribution is
    /// refused in the cwd while it could overlap (c-usagefu r10 item 1,
    /// Ruling A).
    launched_at: Option<OffsetDateTime>,
    /// None while the instance is live; Some(close) after it closed. The
    /// record is RETAINED past close (independent of binding) so a later
    /// launch cannot become the "sole live instance" over a window a
    /// since-closed instance actually shared.
    closed_at: Option<OffsetDateTime>,
    /// Bumped on every (re-)registration of this `(cwd, owner)`. A guard from
    /// an older cohort (a close→resume scratch driver whose handle is adopted
    /// away) records a close ONLY while it is still the current cohort, so a
    /// resumed session is never marked closed by its previous spawn
    /// (c-usagefu r10 item 1).
    epoch: u64,
}

/// cwd -> (instance key -> lifetime).
static INSTANCE_LIFETIMES: LazyLock<Mutex<HashMap<PathBuf, HashMap<String, InstanceLifetime>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// (owner, canonical rollout path) -> the OTHER instance whose overlapping
/// lifetime made prompt-only attribution impossible the first time this
/// candidate was judged. A candidate once REJECTED for an owner is rejected
/// PERMANENTLY for that owner (c-usagefu r11 item 1, Ruling A1): neither the
/// blocker merely closing, nor its interval being pruned, nor a resume, nor
/// any later census change ever turns it into a match for that owner. The key
/// is the (owner, rollout identity) pair, so a different owner is unaffected
/// and the same owner may still bind a DIFFERENT rollout.
static REJECTED_CANDIDATES: LazyLock<Mutex<HashMap<(String, PathBuf), String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// RAII owner of one registered instance lifetime. The interval opens in
/// [`begin_lifetime`] (production: before the agent process is launched) and
/// is marked closed when the LAST clone drops — the live driver slot and the
/// file adapter each hold one. Closing is independent of bind: a bound
/// instance stays live for the whole driver run.
pub(crate) struct LifetimeGuard {
    inner: Arc<LifetimeGuardInner>,
}

struct LifetimeGuardInner {
    key: PathBuf,
    owner: String,
    /// Registration cohort this guard belongs to. Drop closes the interval
    /// only if it is still the current cohort (resume bumps the epoch).
    epoch: u64,
    /// Test clock: when set, Drop records this as the close time instead of
    /// wall-clock now, so regressions can place launches/closes on a
    /// deterministic timeline.
    #[cfg(test)]
    closed_at: std::sync::Mutex<Option<OffsetDateTime>>,
}

impl Clone for LifetimeGuard {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for LifetimeGuardInner {
    fn drop(&mut self) {
        #[cfg(test)]
        let at = self
            .closed_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unwrap_or_else(OffsetDateTime::now_utc);
        #[cfg(not(test))]
        let at = OffsetDateTime::now_utc();
        let mut map = INSTANCE_LIFETIMES.lock().expect("codex instance lifetimes");
        if let Some(by_instance) = map.get_mut(&self.key)
            && let Some(lifetime) = by_instance.get_mut(&self.owner)
            && lifetime.epoch == self.epoch
        {
            lifetime.closed_at = Some(at);
        }
    }
}

/// Register instance `owner` as launching in `cwd` at the agent.start boundary
/// `launched_at` and return the lifetime guard. Generic/native/resumed launches
/// pass `Some(boundary)` sampled BEFORE the child was spawned; a promoted
/// (operator-started) instance passes `None` because its real launch boundary
/// is unknown.
///
/// An unknown-boundary interval still appears in every same-cwd census and
/// makes prompt-only attribution unprovable while it could overlap the
/// decision window (c-usagefu r10 item 1, Ruling A).
///
/// Re-registering the same `(cwd, owner)` (a close then resume re-spawn with
/// the stable instance key) reopens the interval, bumps its cohort epoch and
/// keeps the best-known earliest floor; it never erases an earlier segment.
pub(crate) fn begin_lifetime(
    cwd: &Path,
    owner: &str,
    launched_at: Option<OffsetDateTime>,
) -> LifetimeGuard {
    begin_lifetime_at(cwd, owner, launched_at, OffsetDateTime::now_utc())
}

/// Clock-injected core of [`begin_lifetime`] (production passes wall-clock
/// now; regressions advance the retention horizon deterministically).
pub(crate) fn begin_lifetime_at(
    cwd: &Path,
    owner: &str,
    launched_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> LifetimeGuard {
    let key = canonical(cwd);
    let epoch = {
        let mut map = INSTANCE_LIFETIMES.lock().expect("codex instance lifetimes");
        let by_instance = map.entry(key.clone()).or_default();
        prune_closed_intervals(by_instance, now);
        let lifetime = by_instance
            .entry(owner.to_owned())
            .or_insert_with(|| InstanceLifetime {
                launched_at,
                closed_at: None,
                epoch: 0,
            });
        // (Re-)open and claim a fresh cohort so a resume scratch driver's
        // stale guard cannot close the resumed interval on drop.
        lifetime.epoch = lifetime.epoch.wrapping_add(1);
        lifetime.closed_at = None;
        // Best-known earliest floor. An unknown-boundary segment can never be
        // bounded by a later known one, so None stays sticky across cohorts.
        lifetime.launched_at = match (lifetime.launched_at, launched_at) {
            (Some(existing), Some(incoming)) => Some(existing.min(incoming)),
            _ => None,
        };
        lifetime.epoch
    };
    LifetimeGuard {
        inner: Arc::new(LifetimeGuardInner {
            key,
            owner: owner.to_owned(),
            epoch,
            #[cfg(test)]
            closed_at: std::sync::Mutex::new(None),
        }),
    }
}

/// Drop CLOSED intervals whose close predates `now - LIFETIME_RETENTION`: they
/// can no longer overlap any current discovery decision. A live interval
/// (closed_at None) is never pruned. Retained closed intervals are exactly
/// what keep a later launch from becoming "sole" over a window a since-closed
/// instance shared (r9 item 1); the hard horizon bounds that memory (r10
/// item 2: after it, a rejection keyed to that blocker may flip).
fn prune_closed_intervals(
    by_instance: &mut HashMap<String, InstanceLifetime>,
    now: OffsetDateTime,
) {
    let cutoff = now - LIFETIME_RETENTION;
    by_instance.retain(|_, lifetime| lifetime.closed_at.is_none_or(|closed| closed >= cutoff));
}

/// Test seam: expire closed intervals in `cwd` as if retention were judged at
/// `now` (a same-cwd registration triggers the same prune in production).
#[cfg(test)]
pub(crate) fn prune_closed_intervals_at(cwd: &Path, now: OffsetDateTime) {
    let key = canonical(cwd);
    let mut map = INSTANCE_LIFETIMES.lock().expect("codex instance lifetimes");
    if let Some(by_instance) = map.get_mut(&key) {
        prune_closed_intervals(by_instance, now);
    }
}

/// Test seam: record this guard's close at `at` instead of wall-clock time,
/// so a regression can place the close deterministically on its timeline.
#[cfg(test)]
pub(crate) trait LifetimeGuardClock {
    fn close_lifetime_at(&self, at: OffsetDateTime);
}

#[cfg(test)]
impl LifetimeGuardClock for LifetimeGuard {
    fn close_lifetime_at(&self, at: OffsetDateTime) {
        *self
            .inner
            .closed_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(at);
    }
}

/// Whether the rollout is currently claimed by a DIFFERENT instance.
pub(crate) fn is_claimed_by_other(path: &Path, owner: &str) -> bool {
    let map = CLAIMED_ROLLOUTS.lock().expect("claimed codex rollouts");
    matches!(map.get(&canonical(path)), Some(existing) if existing != owner)
}

/// Claim a rollout path, optionally also remembering the native session id
/// scoped to `cwd` (so a later poll re-recognizes the session by id even if
/// its path is identical to another launch's file).
pub(crate) fn claim_session(path: &Path, owner: &str, cwd_session: Option<(&Path, &str)>) -> bool {
    let key = canonical(path);
    let mut map = CLAIMED_ROLLOUTS.lock().expect("claimed codex rollouts");
    match map.get(&key) {
        Some(existing) if existing != owner => false,
        _ => {
            map.insert(key, owner.to_owned());
            drop(map);
            if let Some((cwd, session_id)) = cwd_session {
                CLAIMED_SESSIONS
                    .lock()
                    .expect("claimed codex sessions")
                    .insert((canonical(cwd), session_id.to_owned()), owner.to_owned());
            }
            true
        }
    }
}

/// Whether native session `session_id` in `cwd` is already claimed by THIS
/// owner (lets an adapter re-bind its own session after a tail restart).
pub(crate) fn owned_by(cwd: &Path, owner: &str, session_id: &str) -> bool {
    CLAIMED_SESSIONS
        .lock()
        .expect("claimed codex sessions")
        .get(&(canonical(cwd), session_id.to_owned()))
        .is_some_and(|existing| existing == owner)
}

/// Whether native session `session_id` in `cwd` is claimed by a DIFFERENT
/// owner.
#[allow(dead_code)]
pub(crate) fn session_claimed_by_other(cwd: &Path, owner: &str, session_id: &str) -> bool {
    CLAIMED_SESSIONS
        .lock()
        .expect("claimed codex sessions")
        .get(&(canonical(cwd), session_id.to_owned()))
        .is_some_and(|existing| existing != owner)
}

/// Release a claim using the EXACT key [`claim_session`] inserted (r6 item 7).
/// Callers hold that canonical key (not a path that may have moved or
/// disappeared, nor a symlink whose target canonicalization changes). A
/// foreign/unowned key is untouched.
pub(crate) fn release_claim_key(key: &Path, owner: &str) {
    let mut map = CLAIMED_ROLLOUTS.lock().expect("claimed codex rollouts");
    if map.get(key).is_some_and(|existing| existing == owner) {
        map.remove(key);
    }
}

/// Canonical claim key for a path: the exact PathBuf `claim_session` inserts
/// (r8 item 6). `claim_key_for` is kept as a test alias.
pub(crate) fn canonical_claim_key(path: &Path) -> PathBuf {
    canonical(path)
}

/// Test-only alias for [`canonical_claim_key`].
#[cfg(test)]
pub(crate) fn claim_key_for(path: &Path) -> PathBuf {
    canonical_claim_key(path)
}

/// Test-only: whether the claim map contains ANY owner for the exact key.
#[cfg(test)]
pub(crate) fn claim_is_present_any_owner(key: &Path) -> bool {
    CLAIMED_ROLLOUTS
        .lock()
        .expect("claimed codex rollouts")
        .contains_key(key)
}

/// Test-only: whether the claim map still contains an entry owned by
/// `owner` for the exact key (no re-canonicalization).
#[cfg(test)]
pub(crate) fn is_claim_present_exact(key: &Path, owner: &str) -> bool {
    CLAIMED_ROLLOUTS
        .lock()
        .expect("claimed codex rollouts")
        .get(key)
        .is_some_and(|existing| existing == owner)
}

/// RAII window for one launch still discovering in a cwd. Explicit
/// [`Self::release`] on bind/give-up drops the refcount slot early; the Drop
/// impl is the safety net for an adapter that closes unbound.
pub(crate) struct DiscoveryWindow {
    key: PathBuf,
    owner: String,
    released: bool,
}

impl DiscoveryWindow {
    /// Open (or join) the discovery window for a launch cwd.
    pub(crate) fn open(cwd: &Path, owner: impl Into<String>) -> Self {
        let key = canonical(cwd);
        let owner = owner.into();
        {
            let mut map = OPEN_WINDOWS.lock().expect("codex discovery windows");
            map.entry(key.clone())
                .and_modify(|entry| {
                    entry.count += 1;
                    entry.tainted = true;
                })
                .or_insert(WindowEntry {
                    count: 1,
                    tainted: false,
                });
            OPEN_DISCOVERERS
                .lock()
                .expect("open discoverers")
                .entry(key.clone())
                .or_default()
                .insert(owner.clone());
        }
        Self {
            key,
            owner,
            released: false,
        }
    }

    /// Test-only: whether another window overlaps this cwd.
    #[cfg(test)]
    pub(crate) fn overlapping(&self) -> bool {
        OPEN_WINDOWS
            .lock()
            .expect("codex discovery windows")
            .get(&self.key)
            .is_some_and(|entry| entry.tainted || entry.count >= 2)
    }

    /// Drop this window's refcount slot before the adapter itself goes away
    /// (bind succeeded, or discovery gave up). Idempotent.
    pub(crate) fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let mut map = OPEN_WINDOWS.lock().expect("codex discovery windows");
        let mut last_window = false;
        if let Some(entry) = map.get_mut(&self.key) {
            entry.count = entry.count.saturating_sub(1);
            if entry.count <= 1 {
                // At most one seeker left: the file-level locator (with
                // per-instance claims and prompts) is authoritative again.
                entry.tainted = false;
            }
            if entry.count == 0 {
                map.remove(&self.key);
                last_window = true;
            }
        }
        // Remove this owner from the live-discoverer set regardless of
        // refcount (the window key is per-cwd, the owner is unique).
        if let Some(set) = OPEN_DISCOVERERS
            .lock()
            .expect("open discoverers")
            .get_mut(&self.key)
        {
            set.remove(&self.owner);
        }
        // r7 item 10: when this is the LAST open window for the cwd, no
        // discovery lifecycle remains — discard all recorded prompts so they
        // cannot authorize a future unrelated rollout and do not accumulate.
        // (A specific owner's prompts are also harmless once no window is
        // open; the next launch records fresh inputs.)
        if last_window
            && let Some(by_instance) = SENT_INPUTS
                .lock()
                .expect("codex sent inputs")
                .get_mut(&self.key)
        {
            by_instance.clear();
        }
    }
}

impl Drop for DiscoveryWindow {
    fn drop(&mut self) {
        self.release();
    }
}

// ---------------------------------------------------------------------------
// Sent-input evidence (c-usagefu r5 item 5; r6 items 1/3)
// ---------------------------------------------------------------------------

/// How long after a keystroke a lazily-created rollout may appear and still
/// count as that prompt's session.
pub(crate) const INPUT_MATCH_WINDOW: time::Duration = time::Duration::seconds(300);

/// Small pre-keystroke slack (the session row can precede the recorded send by
/// a scheduler tick).
const INPUT_MATCH_SLACK: time::Duration = time::Duration::seconds(5);

/// How long a recorded prompt stays eligible for attribution after its LATEST
/// dispatch (r7 item 10). Beyond this an input is aged out on write, so a
/// long-lived Node's registry does not grow without bound and an old prompt
/// cannot authorize a much-later rollout.
const INPUT_MAX_AGE: time::Duration = time::Duration::hours(1);

/// Evict all recorded inputs older than `now - INPUT_MAX_AGE` (r7 item 10).
/// Called on every record_input so stale per-instance prompts disappear
/// without a background task.
///
/// Lifecycle first (r8 item 7; r9 item 6): age eviction never removes ANY
/// prompt of an instance that is still a LIVE, unbound discoverer in that
/// cwd. A rollout created inside an EARLIER prompt's match window can be
/// discovered long after the keystroke (a sessions directory that appears
/// or is scanned late); sending a fresh prompt must not strip that earlier
/// evidence, and neither may a sweep fired by an unrelated input in another
/// cwd. The r8 rule protected the owner only when ALL of its prompts were
/// aged, so a live discoverer's initial prompt P was evicted as soon as it
/// sent a fresh Q. Such evidence is reclaimed by the lifecycle instead —
/// bind releases the window and window close clears the prompts in
/// [`DiscoveryWindow::release`].
pub(crate) fn evict_expired_inputs(now: OffsetDateTime) {
    let cutoff = now - INPUT_MAX_AGE;
    let live = OPEN_DISCOVERERS.lock().expect("open discoverers");
    let mut map = SENT_INPUTS.lock().expect("codex sent inputs");
    map.retain(|cwd, by_instance| {
        by_instance.retain(|owner, log| {
            if live.get(cwd).is_some_and(|set| set.contains(owner)) {
                // Live unbound discoverer: keep the anchor and every recent
                // prompt until bind/close (recent is capped at 32).
                return !log.is_empty();
            }
            log.recent.retain(|input| input.at >= cutoff);
            if log.anchor.as_ref().is_some_and(|input| input.at < cutoff) {
                log.anchor = None;
            }
            !log.is_empty()
        });
        !by_instance.is_empty()
    });
}

#[derive(Clone)]
struct SentInput {
    /// Whitespace-trimmed prompt text as the driver sent it.
    text: String,
    /// Time this evidence window starts: the ORIGINAL send for the anchor, the
    /// latest dispatch for a recent entry.
    at: OffsetDateTime,
}

/// Per-instance prompt evidence (c-usagefu r10 item 7). The FIRST prompt is
/// held separately as an immutable ANCHOR carrying its ORIGINAL send-time
/// window: a cap over later prompts must never drain it, and a later RESEND of
/// the same text must not refresh its window (doing so moved the back edge of
/// the window past a rollout that was created inside the ORIGINAL window).
/// Later dispatches live in a bounded recent list; a resend of the anchor text
/// is recorded there too, so the later dispatch still opens its own later
/// window without disturbing the anchor.
#[derive(Default)]
struct PromptLog {
    anchor: Option<SentInput>,
    recent: Vec<SentInput>,
}

impl PromptLog {
    fn is_empty(&self) -> bool {
        self.anchor.is_none() && self.recent.is_empty()
    }

    fn windows(&self) -> impl Iterator<Item = &SentInput> {
        self.anchor.iter().chain(self.recent.iter())
    }
}

/// cwd -> (instance key -> prompt evidence). Every instance authorizes only
/// its OWN prompts.
type SentInputs = HashMap<PathBuf, HashMap<String, PromptLog>>;
static SENT_INPUTS: LazyLock<Mutex<SentInputs>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Whitespace-trimmed comparison form (codex stores the prompt verbatim in
/// the `input_text` item).
fn normalize_input(text: &str) -> String {
    text.trim().to_owned()
}

fn put_input(cwd: &Path, owner: &str, text: &str, at: OffsetDateTime) {
    let cwd_key = canonical(cwd);
    let text = normalize_input(text);
    if text.is_empty() {
        return;
    }
    let mut map = SENT_INPUTS.lock().expect("codex sent inputs");
    let log = map
        .entry(cwd_key)
        .or_default()
        .entry(owner.to_owned())
        .or_default();
    match &log.anchor {
        // First prompt this instance ever sent: keep it as the immutable
        // anchor with its original window.
        None => log.anchor = Some(SentInput { text, at }),
        Some(_) => {
            // Later dispatch (a fresh prompt OR a resend of the anchor). It
            // gets/updates its OWN recent entry and never touches the anchor.
            match log.recent.iter_mut().find(|existing| existing.text == text) {
                Some(existing) => existing.at = at,
                None => {
                    log.recent.push(SentInput { text, at });
                    // Bound retained LATER history only; the anchor is outside
                    // this list and can never be drained by it.
                    if log.recent.len() > 32 {
                        let excess = log.recent.len() - 32;
                        log.recent.drain(0..excess);
                    }
                }
            }
        }
    }
}

/// Record that instance `owner` dispatched `text` in `cwd` at `now`.
pub(crate) fn record_input(cwd: &Path, owner: &str, text: &str) {
    evict_expired_inputs(OffsetDateTime::now_utc());
    put_input(cwd, owner, text, OffsetDateTime::now_utc());
}

/// Test seam: record an input with an explicit timestamp; also runs eviction
/// against that clock so age-out is deterministic.
#[cfg(test)]
pub(crate) fn record_input_at(cwd: &Path, owner: &str, text: &str, at: OffsetDateTime) {
    evict_expired_inputs(at);
    put_input(cwd, owner, text, at);
}

/// Outcome of judging one candidate for prompt-only attribution (Ruling A).
enum PromptVerdict {
    /// Not attributable to this owner on the facts (no lifetime, unknown
    /// floor, pre-launch creation, or no matching sent prompt).
    Ineligible,
    /// Every prompt/launch fact fits but another instance's lifetime overlaps
    /// the census window, so attribution is refused. The string is the
    /// overlapping instance the rejection is keyed to.
    Blocker(String),
    /// Complete census, sole live instance, prompt window fits: may bind.
    Match,
}

/// Pure census judgment shared by the boolean and sticky-rejection paths.
fn evaluate_prompt(
    cwd: &Path,
    owner: &str,
    rollout_text: &str,
    session_started: OffsetDateTime,
) -> PromptVerdict {
    let cwd_key = canonical(cwd);
    let wanted = normalize_input(rollout_text);
    if wanted.is_empty() {
        return PromptVerdict::Ineligible;
    }
    let blocker = {
        let lifetimes = INSTANCE_LIFETIMES.lock().expect("codex instance lifetimes");
        let Some(by_instance) = lifetimes.get(&cwd_key) else {
            return PromptVerdict::Ineligible;
        };
        let Some(own) = by_instance.get(owner) else {
            return PromptVerdict::Ineligible;
        };
        // The owner itself needs a KNOWN launch boundary: a promoted (unknown
        // floor) adapter never uses prompt-only attribution at all.
        let Some(own_launch) = own.launched_at else {
            return PromptVerdict::Ineligible;
        };
        // Strict launch floor: no pre-launch slack for prompt attribution.
        if session_started < own_launch {
            return PromptVerdict::Ineligible;
        }
        // Complete-census sole-live proof over [launch, creation]. Another
        // instance overlaps when it was live at any instant across that
        // interval. A KNOWN-boundary instance overlaps iff it launched at or
        // before creation and was still live at the owner's launch. An
        // UNKNOWN-boundary (promoted) instance cannot be proven to have
        // launched after creation, so it overlaps whenever it was still live at
        // the owner's launch — the cwd census is incomplete and prompt-only
        // attribution is refused (Ruling A; closed exactly at the boundary is
        // not an overlap).
        by_instance
            .iter()
            .find(|(other, lifetime)| {
                *other != owner
                    && lifetime
                        .launched_at
                        .is_none_or(|launched| launched <= session_started)
                    && lifetime.closed_at.is_none_or(|closed| closed > own_launch)
            })
            .map(|(other, _)| other.clone())
    };
    let prompt_fits = {
        let inputs = SENT_INPUTS.lock().expect("codex sent inputs");
        inputs
            .get(&cwd_key)
            .and_then(|by_owner| by_owner.get(owner))
            .is_some_and(|log| {
                log.windows().any(|input| {
                    input.text == wanted
                        && session_started >= input.at - INPUT_MATCH_SLACK
                        && session_started <= input.at + INPUT_MATCH_WINDOW
                })
            })
    };
    if !prompt_fits {
        return PromptVerdict::Ineligible;
    }
    match blocker {
        Some(other) => PromptVerdict::Blocker(other),
        None => PromptVerdict::Match,
    }
}

/// r9 item 1 TIER C boolean: prompt text authorizes a rollout ONLY when the
/// census is complete and the owner was the sole live instance over
/// [launch, creation], with the first prompt in its send-time window.
///
/// Test-only direct read; production discovery goes through
/// [`prompt_candidate_matches`] so the r10 item 2 sticky-rejection memory is
/// always applied.
#[cfg(test)]
pub(crate) fn sole_instance_matches(
    cwd: &Path,
    owner: &str,
    rollout_text: &str,
    session_started: OffsetDateTime,
) -> bool {
    matches!(
        evaluate_prompt(cwd, owner, rollout_text, session_started),
        PromptVerdict::Match
    )
}

/// Production TIER C entry point with PERMANENT sticky rejection (r10 item 2;
/// r11 item 1 tightens it to Ruling A1).
///
/// A candidate judged [`PromptVerdict::Blocker`] is remembered against that
/// blocker. Every LATER judgment of the same `(owner, rollout)` returns false
/// for that owner — the blocker closing, its interval being pruned, a resume
/// or any other census change can never flip a correct first "not yours" into
/// a match. The rejection is keyed per owner, so another owner and another
/// rollout are unaffected.
pub(crate) fn prompt_candidate_matches(
    cwd: &Path,
    owner: &str,
    rollout: &Path,
    rollout_text: &str,
    session_started: OffsetDateTime,
) -> bool {
    let key = (owner.to_owned(), canonical(rollout));
    if REJECTED_CANDIDATES
        .lock()
        .expect("rejected codex candidates")
        .contains_key(&key)
    {
        // Ruling A1: a rejection for (owner, rollout identity) is permanent;
        // do not even re-evaluate a cleaner-looking census.
        return false;
    }
    match evaluate_prompt(cwd, owner, rollout_text, session_started) {
        PromptVerdict::Ineligible => false,
        PromptVerdict::Blocker(blocker) => {
            REJECTED_CANDIDATES
                .lock()
                .expect("rejected codex candidates")
                .insert(key, blocker);
            false
        }
        PromptVerdict::Match => true,
    }
}

/// Test seam: forget every lifetime registered for `owner` in `cwd`, as if the
/// instance had left the cwd census entirely. Lets Ruling A1 regressions prove
/// the rejection outlives the blocker WITHOUT depending on the interval
/// retention policy (r11 item 1).
#[cfg(test)]
pub(crate) fn forget_lifetime(cwd: &Path, owner: &str) {
    let key = canonical(cwd);
    if let Some(by_instance) = INSTANCE_LIFETIMES
        .lock()
        .expect("codex instance lifetimes")
        .get_mut(&key)
    {
        by_instance.remove(owner);
    }
}

// Test-only visibility so unit tests in other modules of this crate can
// build deterministic identities without touching process globals elsewhere.
#[cfg(test)]
pub(crate) fn claimed_owner_of(path: &Path) -> Option<String> {
    CLAIMED_ROLLOUTS
        .lock()
        .expect("claimed codex rollouts")
        .get(&canonical(path))
        .cloned()
}
