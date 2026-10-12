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
    /// Sampled at the agent.start boundary, BEFORE the child is spawned.
    launched_at: OffsetDateTime,
    /// None while the instance is live; Some(close) after it closed. The
    /// record is RETAINED past close (independent of binding) so a later
    /// launch cannot become the "sole live instance" over a window a
    /// since-closed instance actually shared.
    closed_at: Option<OffsetDateTime>,
}

/// cwd -> (instance key -> lifetime).
static INSTANCE_LIFETIMES: LazyLock<Mutex<HashMap<PathBuf, HashMap<String, InstanceLifetime>>>> =
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
        {
            lifetime.closed_at = Some(at);
        }
    }
}

/// Register instance `owner` as launching in `cwd` at `launched_at` and
/// return the lifetime guard. MUST be called before the agent process is
/// spawned (the agent.start boundary), so an instance that launches/closes in
/// the gap before the file adapter is constructed is still recorded.
///
/// Re-registering the same `(cwd, owner)` (a close→resume re-spawn with the
/// stable instance key) reopens the interval while keeping the EARLIEST
/// launch floor; it never erases an earlier segment.
pub(crate) fn begin_lifetime(
    cwd: &Path,
    owner: &str,
    launched_at: OffsetDateTime,
) -> LifetimeGuard {
    let key = canonical(cwd);
    {
        let cutoff = OffsetDateTime::now_utc() - LIFETIME_RETENTION;
        let mut map = INSTANCE_LIFETIMES.lock().expect("codex instance lifetimes");
        let by_instance = map.entry(key.clone()).or_default();
        // Bound retained history: drop closed intervals that can no longer
        // overlap a current decision. A live interval (closed_at None) is
        // never pruned.
        by_instance.retain(|_, lifetime| lifetime.closed_at.is_none_or(|closed| closed >= cutoff));
        by_instance
            .entry(owner.to_owned())
            .and_modify(|lifetime| {
                lifetime.launched_at = lifetime.launched_at.min(launched_at);
                lifetime.closed_at = None;
            })
            .or_insert(InstanceLifetime {
                launched_at,
                closed_at: None,
            });
    }
    LifetimeGuard {
        inner: Arc::new(LifetimeGuardInner {
            key,
            owner: owner.to_owned(),
            #[cfg(test)]
            closed_at: std::sync::Mutex::new(None),
        }),
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
/// Lifecycle first (r8 item 7): age eviction never removes the ONLY prompt
/// evidence of an instance that is still a LIVE, unbound discoverer in that
/// cwd. A rollout created inside the prompt's match window can be discovered
/// late (a sessions directory that appears or is scanned well after the
/// keystroke); dropping the prompt would leave that instance unable to bind
/// its own file. Such evidence is reclaimed by the lifecycle instead — bind
/// or window close clears it in [`DiscoveryWindow::release`]. Fresh prompts
/// for the same owner still replace aged ones in the normal retain.
pub(crate) fn evict_expired_inputs(now: OffsetDateTime) {
    let cutoff = now - INPUT_MAX_AGE;
    let live = OPEN_DISCOVERERS.lock().expect("open discoverers");
    let mut map = SENT_INPUTS.lock().expect("codex sent inputs");
    map.retain(|cwd, by_instance| {
        by_instance.retain(|owner, inputs| {
            // Every surviving prompt being aged out means the sweep would
            // strip this owner's only evidence; a live discoverer keeps it
            // until the lifecycle ends.
            let only_evidence_is_aged = !inputs.is_empty()
                && inputs.iter().all(|input| input.at < cutoff)
                && live.get(cwd).is_some_and(|set| set.contains(owner));
            inputs.retain(|input| only_evidence_is_aged || input.at >= cutoff);
            !inputs.is_empty()
        });
        !by_instance.is_empty()
    });
}

#[derive(Clone)]
struct SentInput {
    /// Whitespace-trimmed prompt text as the driver sent it.
    text: String,
    /// Latest time the instance dispatched this text.
    at: OffsetDateTime,
}

/// cwd -> (instance key -> prompts). A HashMap (not a global dedupe list):
/// every instance authorizes only its OWN prompts, and re-sending identical
/// text ("continue") refreshes that instance's timestamp instead of being
/// dropped (r6 item 3).
type SentInputs = HashMap<PathBuf, HashMap<String, Vec<SentInput>>>;
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
    let inputs = map
        .entry(cwd_key)
        .or_default()
        .entry(owner.to_owned())
        .or_default();
    match inputs.iter_mut().find(|existing| existing.text == text) {
        // Identical prompt sent again: refresh the time so a later turn
        // ("continue" minutes/hours later) can still match (r6 item 3).
        Some(existing) => existing.at = at,
        None => {
            inputs.push(SentInput { text, at });
            // Bound retained history per (cwd, instance).
            if inputs.len() > 32 {
                let excess = inputs.len() - 32;
                inputs.drain(0..excess);
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

/// r9 item 1 TIER C: prompt text authorizes a rollout ONLY when ALL hold:
///
/// 1. `owner` registered an instance lifetime in `cwd`;
/// 2. the rollout was created AT OR AFTER the owner's ACTUAL launch floor —
///    the locator's 5 s pre-launch slack is NOT available to prompt-only
///    attribution (only the hook and process-ownership tiers may take an
///    in-slack candidate);
/// 3. no OTHER instance was live in that cwd at any instant over the whole
///    interval [owner launch, rollout creation]. A closed instance's
///    retained interval counts, so an owner closing before the candidate is
///    judged never makes the survivor "sole";
/// 4. the rollout's first prompt is an input THIS owner dispatched, in its
///    send-time window.
pub(crate) fn sole_instance_matches(
    cwd: &Path,
    owner: &str,
    rollout_text: &str,
    session_started: OffsetDateTime,
) -> bool {
    let cwd_key = canonical(cwd);
    let wanted = normalize_input(rollout_text);
    if wanted.is_empty() {
        return false;
    }
    let lifetimes = INSTANCE_LIFETIMES.lock().expect("codex instance lifetimes");
    let Some(by_instance) = lifetimes.get(&cwd_key) else {
        return false;
    };
    let Some(own) = by_instance.get(owner) else {
        return false;
    };
    // Strict launch floor: no pre-launch slack for prompt attribution.
    if session_started < own.launched_at {
        return false;
    }
    // Sole-live proof over [launch, creation]. Another instance overlaps when
    // it launched at or before creation and was still live at the owner's
    // launch (closed exactly at the boundary is not an overlap).
    let shared_window = by_instance.iter().any(|(other, lifetime)| {
        other != owner
            && lifetime.launched_at <= session_started
            && lifetime
                .closed_at
                .is_none_or(|closed| closed > own.launched_at)
    });
    if shared_window {
        return false;
    }
    drop(lifetimes);
    let inputs = SENT_INPUTS.lock().expect("codex sent inputs");
    inputs
        .get(&cwd_key)
        .and_then(|by_owner| by_owner.get(owner))
        .is_some_and(|list| {
            list.iter().any(|input| {
                input.text == wanted
                    && session_started >= input.at - INPUT_MATCH_SLACK
                    && session_started <= input.at + INPUT_MATCH_WINDOW
            })
        })
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
