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
use std::sync::{LazyLock, Mutex};
use time::OffsetDateTime;

#[derive(Default)]
struct WindowEntry {
    /// Live adapters currently discovering in this cwd.
    count: u32,
    /// True while another window overlaps (count >= 2).
    tainted: bool,
    /// r8 item 1 tier c: STICKY — once a second window has been open at any
    /// point in this epoch, prompt text can never attribute a rollout here
    /// (another instance closing does not make the survivor "sole"). Cleared
    /// only when the epoch ends (the last window closes and the entry drops).
    ever_shared: bool,
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

/// Canonical claim key for a path (test seam + adapter Drop).
pub(crate) fn claim_key_for(path: &Path) -> PathBuf {
    canonical(path)
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
                    entry.ever_shared = true;
                })
                .or_insert(WindowEntry {
                    count: 1,
                    tainted: false,
                    ever_shared: false,
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
pub(crate) fn evict_expired_inputs(now: OffsetDateTime) {
    let cutoff = now - INPUT_MAX_AGE;
    let mut map = SENT_INPUTS.lock().expect("codex sent inputs");
    map.retain(|_, by_instance| {
        by_instance.retain(|_, inputs| {
            inputs.retain(|input| input.at >= cutoff);
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

/// r8 item 1 TIER C: prompt text authorizes a rollout ONLY when `owner` has
/// been the single discoverer in `cwd` for the whole window epoch — the
/// `ever_shared` sticky flag means a second instance that was live at ANY
/// point (even if it later closed) disables prompt attribution for the
/// epoch. Repeated sends by the same owner do not relax it.
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
    let windows = OPEN_WINDOWS.lock().expect("codex discovery windows");
    let discoverers = OPEN_DISCOVERERS.lock().expect("open discoverers");
    let inputs = SENT_INPUTS.lock().expect("codex sent inputs");
    // Owner must currently be a live discoverer AND its epoch never shared.
    let sole_epoch = windows
        .get(&cwd_key)
        .is_some_and(|entry| !entry.ever_shared)
        && discoverers
            .get(&cwd_key)
            .is_some_and(|set| set.contains(owner));
    drop((windows, discoverers));
    if !sole_epoch {
        return false;
    }
    inputs
        .get(&cwd_key)
        .and_then(|by_instance| by_instance.get(owner))
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
