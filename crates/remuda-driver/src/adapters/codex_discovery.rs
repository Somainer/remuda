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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use time::OffsetDateTime;

/// Identity of the owning Node for census state (c-usagefu r18 items 2/3).
///
/// One [`NodeEpoch`] is minted per composed `DevNode`; drivers built by that
/// Node carry a clone. In-process recompose (a second `compose()` in the same
/// process) is therefore a NEW epoch: when the first Node shuts down its
/// stop-incomplete census blocks retire for ITS epoch only, and the second
/// Node's same-cwd prompt-only discovery is attributable again.
///
/// The epoch also carries the Node's shutdown flag: an incomplete close
/// performed AS PART OF Node shutdown emits only a `tracing::warn!` — its
/// block ends with that Node — while a live Node's incomplete close emits the
/// operator lifecycle/Warning observation (Ruling W0').
#[derive(Clone)]
pub struct NodeEpoch {
    inner: Arc<NodeEpochInner>,
}

impl std::fmt::Debug for NodeEpoch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeEpoch")
            .field("id", &self.inner.id)
            .field(
                "shutting_down",
                &self.inner.shutting_down.load(Ordering::SeqCst),
            )
            .finish()
    }
}

struct NodeEpochInner {
    /// Monotonic per-process id used to scope retained census guards.
    id: u64,
    shutting_down: AtomicBool,
}

static NEXT_NODE_EPOCH: AtomicU64 = AtomicU64::new(1);

impl NodeEpoch {
    /// Mint a fresh epoch for one composed Node.
    #[must_use]
    pub fn mint() -> Self {
        Self {
            inner: Arc::new(NodeEpochInner {
                id: NEXT_NODE_EPOCH.fetch_add(1, Ordering::Relaxed),
                shutting_down: AtomicBool::new(false),
            }),
        }
    }

    /// Monotonic per-process id used to scope retained census guards (used by
    /// the per-epoch retention added in c-usagefu r18 item 3).
    #[allow(dead_code)]
    pub(crate) fn id(&self) -> u64 {
        self.inner.id
    }

    /// Signal this Node is shutting down BEFORE its instances are closed, so
    /// an incomplete stop during the close loop needs no operator observation
    /// (c-usagefu r18 Ruling W0').
    pub fn begin_shutdown(&self) {
        self.inner.shutting_down.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        self.inner.shutting_down.load(Ordering::SeqCst)
    }
}

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

/// Coordination lock between a pending LAUNCH registration and a LAST-WINDOW
/// release (c-usagefu r13 item 1). Registration inserts the lifetime segment
/// and opens the discovery window as ONE critical section; a last-window
/// release (its refcount drop, the prompt-evidence clear and the closed-
/// segment prune) is the opposite critical section. Without it another
/// window's release could land in the gap between the two separate mutex
/// operations, observe itself as the cwd's LAST window while the launching
/// instance had a lifetime but no window yet, and prune a closed interval the
/// launch's later discovery decision still needed. Always the OUTERMOST lock:
/// it is acquired alone, never while one of the fine-grained mutexes below is
/// held.
static LAUNCH_REGISTRATION: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Acquire the LAUNCH_REGISTRATION coordination lock.
///
/// In test builds a waiter that finds the lock ALREADY held first fires a
/// one-shot "release blocked" seam for its cwd (c-usagefu r14 item 6), so a
/// regression can prove deterministically that a window release is queued on
/// the lock before it lets a parked registration proceed — no fixed sleep.
fn acquire_launch_registration(cwd: &Path) -> std::sync::MutexGuard<'static, ()> {
    #[cfg(not(test))]
    {
        let _ = cwd;
        LAUNCH_REGISTRATION
            .lock()
            .expect("codex launch registration")
    }
    #[cfg(test)]
    {
        use std::sync::TryLockError;
        loop {
            match LAUNCH_REGISTRATION.try_lock() {
                Ok(guard) => break guard,
                Err(TryLockError::WouldBlock) => {
                    notify_release_blocked(cwd);
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(TryLockError::Poisoned(poison)) => {
                    panic!("codex launch registration: {poison}")
                }
            }
        }
    }
}

/// One-shot signal installed by a test: fired when a window release for a
/// cwd first finds LAUNCH_REGISTRATION held (r14 item 6).
#[cfg(test)]
type ReleaseBlockedSlot = Option<(PathBuf, std::sync::mpsc::Sender<()>)>;

#[cfg(test)]
static RELEASE_BLOCKED: LazyLock<Mutex<ReleaseBlockedSlot>> = LazyLock::new(|| Mutex::new(None));

/// Test seam: receive a signal once a window release in `cwd` is blocked
/// waiting for LAUNCH_REGISTRATION (r14 item 6).
#[cfg(test)]
pub(crate) fn release_blocked_receiver(cwd: &Path) -> std::sync::mpsc::Receiver<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    *RELEASE_BLOCKED.lock().expect("release blocked seam") = Some((canonical(cwd), tx));
    rx
}

#[cfg(test)]
fn notify_release_blocked(cwd: &Path) {
    let mut slot = RELEASE_BLOCKED.lock().expect("release blocked seam");
    if slot.as_ref().is_some_and(|(key, _)| key == &canonical(cwd)) {
        // Take and fire once.
        if let Some((_, tx)) = slot.take() {
            let _ = tx.send(());
        }
    }
}

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
// Instance LIFETIMES (c-usagefu r9 item 1; r11 Ruling A2)
// ---------------------------------------------------------------------------

/// One interval an instance was live in a cwd.
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
    /// away) records a close ONLY against its own cohort segment, so a resumed
    /// session is never marked closed by its previous spawn (c-usagefu r10
    /// item 1; r11 item 4).
    epoch: u64,
    /// Census epoch of the Node that registered this segment, so a shutdown
    /// can retire exactly its own intervals (c-usagefu r18 item 3). `None`
    /// for segments begun outside a composed Node (direct test calls).
    node_epoch: Option<u64>,
}

/// Every interval one owner has had in a cwd, in registration order
/// (c-usagefu r11 item 4, Ruling A2). A resume APPENDS a fresh segment
/// starting at the resume time; the previous closed segment is never reopened
/// or bridged. Closed segments are retained so later launches cannot become
/// "sole" over a window an earlier segment shared.
struct InstanceSegments {
    segments: Vec<InstanceLifetime>,
}

impl InstanceSegments {
    fn empty() -> Self {
        Self {
            segments: Vec::new(),
        }
    }
    fn live(&self) -> Option<&InstanceLifetime> {
        self.segments
            .iter()
            .rev()
            .find(|segment| segment.closed_at.is_none())
    }

    fn epoch_mut(&mut self, epoch: u64) -> Option<&mut InstanceLifetime> {
        self.segments
            .iter_mut()
            .find(|segment| segment.epoch == epoch)
    }
}

/// cwd -> (instance key -> its intervals).
static INSTANCE_LIFETIMES: LazyLock<Mutex<HashMap<PathBuf, HashMap<String, InstanceSegments>>>> =
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
/// [`begin_lifetime`] (production: before the agent process is launched).
/// Exactly ONE guard per cohort is the OWNER (the one [`begin_lifetime`]
/// returns, held by the live driver slot); every [clone](Self::clone) is a
/// non-closing WITNESS (a file adapter). The interval is closed explicitly by
/// the owner only AFTER the process has stopped — [`LifetimeGuard::close`]
/// (c-usagefu r11 item 3, Ruling A2) — never when close merely begins and
/// never on a witness drop. Owner Drop is the safety net for a teardown that
/// did not reach the stop ladder. Closing is independent of bind: a bound
/// instance stays live for the whole driver run.
pub(crate) struct LifetimeGuard {
    inner: Arc<LifetimeGuardInner>,
    owner: bool,
}

struct LifetimeGuardInner {
    key: PathBuf,
    owner: String,
    /// Registration cohort this guard belongs to. A close is recorded only if
    /// it is still the current cohort (resume bumps the epoch).
    epoch: u64,
    /// Census epoch of the Node that owns this guard (c-usagefu r18 item 3).
    node_epoch: Option<u64>,
    /// Set once the interval has been closed, so an explicit close followed by
    /// owner Drop cannot move the recorded close time later.
    closed: std::sync::atomic::AtomicBool,
    /// Whether owner Drop may record a close. True for a normally-held owner;
    /// set false when the owner guard is retained for a stop-incomplete
    /// teardown (c-usagefu r17 Ruling W0): a teardown that never CONFIRMED the
    /// process group gone must drop the guard WITHOUT recording a false close.
    /// An explicit [`LifetimeGuard::close_interval`] after confirmed exit
    /// bypasses this flag.
    drop_closes: std::sync::atomic::AtomicBool,
    /// Test clock: when set, the close is recorded at this time instead of
    /// wall-clock now, so regressions can place launches/closes on a
    /// deterministic timeline.
    #[cfg(test)]
    closed_at: std::sync::Mutex<Option<OffsetDateTime>>,
}

impl LifetimeGuard {
    /// Canonical cwd this guard's interval belongs to.
    pub(crate) fn cwd(&self) -> &Path {
        &self.inner.key
    }

    /// Owner key of this guard's interval.
    pub(crate) fn owner(&self) -> &str {
        &self.inner.owner
    }

    /// Per-owner cohort id this guard closes.
    pub(crate) fn epoch(&self) -> u64 {
        self.inner.epoch
    }

    /// Census epoch of the Node that owns this guard, when it was built by a
    /// composed Node.
    pub(crate) fn node_epoch(&self) -> Option<u64> {
        self.inner.node_epoch
    }

    /// Witness clone: shares the cohort but its Drop never closes the interval
    /// (file adapters are torn down BEFORE the stop ladder runs).
    pub(crate) fn witness(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            owner: false,
        }
    }

    /// Close the interval NOW: the owner calls this only after the agent
    /// process has actually stopped (Ruling A2, r11 item 3), so a rollout
    /// created while close was in flight is still attributed under the LIVE
    /// census. Idempotent; witnesses calling it would do nothing (there is
    /// exactly one owner per cohort).
    pub(crate) fn close_interval(&self) {
        self.record_close();
    }

    /// Transfer this owner guard to stop-incomplete retention (c-usagefu r17
    /// Ruling W0): owner Drop must then never record a close the stop ladder
    /// did not confirm — draining the bag on Node shutdown drops the guard
    /// without closing. [`Self::close_interval`] stays effective for the
    /// confirmed-exit path.
    pub(crate) fn suppress_drop_close(&self) {
        self.inner
            .drop_closes
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    fn record_close(&self) {
        if !self.owner
            || self
                .inner
                .closed
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        #[cfg(test)]
        let at = self
            .inner
            .closed_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unwrap_or_else(OffsetDateTime::now_utc);
        #[cfg(not(test))]
        let at = OffsetDateTime::now_utc();
        let mut map = INSTANCE_LIFETIMES.lock().expect("codex instance lifetimes");
        if let Some(by_instance) = map.get_mut(&self.inner.key)
            && let Some(segments) = by_instance.get_mut(&self.inner.owner)
            && let Some(lifetime) = segments.epoch_mut(self.inner.epoch)
            && lifetime.closed_at.is_none()
        {
            lifetime.closed_at = Some(at);
        }
    }
}

impl Clone for LifetimeGuard {
    /// Cloning the owner yields a non-closing witness; the original keeps the
    /// sole right to close the cohort.
    fn clone(&self) -> Self {
        self.witness()
    }
}

impl Drop for LifetimeGuard {
    fn drop(&mut self) {
        // Safety net only: a normal teardown already called close_interval()
        // after the stop ladder (the AtomicBool then makes this a no-op). A
        // witness drop never closes, and a guard retained for a stop-incomplete
        // teardown (c-usagefu r17 Ruling W0) closes ONLY via an explicit
        // confirmed-exit close_interval: its drop on Node shutdown records
        // nothing.
        if self
            .inner
            .drop_closes
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            self.record_close();
        }
    }
}

/// c-usagefu r17 COORDINATOR RULING W0 — census owner guards of
/// stop-incomplete teardowns.
///
/// When a close's stop ladder cannot CONFIRM the process group is gone, its
/// codex census intervals must stay OPEN for the rest of this Node's
/// lifetime: same-cwd prompt-only discovery then keeps refusing exactly as
/// while the agent was live, and a survivor that exits later (or a pid the
/// kernel reused) can never produce a false close on this Node. Deliberately
/// there is NO watcher, NO periodic sweep and NO process-identity tracking —
/// this is only a bag of suppressed owner guards.
///
/// c-usagefu r18 item 3: the bag is keyed by the OWNING NODE's census epoch.
/// On that Node's shutdown [`shutdown_unconfirmed_guards`] retires exactly
/// ITS intervals from the census (removes the segments without recording a
/// close) and drops its guards; another still-live Node's epoch is untouched.
/// Guards whose driver carried no epoch (constructed outside a composed Node)
/// live in the `None` detached bucket and are dropped unclosed as before.
static UNCONFIRMED_GUARDS: LazyLock<Mutex<HashMap<Option<u64>, Vec<LifetimeGuard>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Retain the owner census guards of one stop-incomplete teardown for the
/// rest of its Node's lifetime (Ruling W0). Drop-close is suppressed on
/// every guard before it enters the bag; an explicit confirmed-exit close
/// stays impossible here because the close path already classified the group
/// as not gone.
pub(crate) fn retain_unconfirmed_guards(guards: Vec<LifetimeGuard>) {
    if guards.is_empty() {
        return;
    }
    let node_epoch = guards.first().and_then(LifetimeGuard::node_epoch);
    for guard in &guards {
        guard.suppress_drop_close();
    }
    UNCONFIRMED_GUARDS
        .lock()
        .expect("unconfirmed codex census guards")
        .entry(node_epoch)
        .or_default()
        .extend(guards);
}

/// Node shutdown seam (called from `DevNode::shutdown`, c-usagefu r18 Ruling
/// W0' / item 3): retire THIS Node's stop-incomplete census blocks.
///
/// The retained guards are dropped WITHOUT recording a close (their
/// Drop-close was suppressed), and their still-open census SEGMENTS are then
/// removed — not marked closed — so an in-process recompose (Node B, fresh
/// epoch, same process) can attribute a same-cwd prompt-only discovery again.
/// Other Nodes' epochs (including the detached `None` bucket of drivers
/// built outside a composed Node) are left strictly alone. Synchronous: the
/// guards own no tasks.
pub fn shutdown_unconfirmed_guards(epoch: Option<&NodeEpoch>) {
    let epoch_id = epoch.map(NodeEpoch::id);
    let guards = UNCONFIRMED_GUARDS
        .lock()
        .expect("unconfirmed codex census guards")
        .remove(&epoch_id)
        .unwrap_or_default();
    let locations: Vec<(PathBuf, String, u64)> = guards
        .iter()
        .map(|guard| {
            (
                guard.cwd().to_path_buf(),
                guard.owner().to_owned(),
                guard.epoch(),
            )
        })
        .collect();
    // Drop the suppressed guards first; they record no closure.
    drop(guards);
    // Then forget their (still live) segments so the census no longer blocks
    // the next Node epoch in those cwds. This is a RETIREMENT, not a close.
    if !locations.is_empty() {
        let mut map = INSTANCE_LIFETIMES.lock().expect("codex instance lifetimes");
        for (key, owner, cohort) in locations {
            if let Some(by_instance) = map.get_mut(&key) {
                if let Some(segments) = by_instance.get_mut(&owner) {
                    segments.segments.retain(|segment| {
                        // Defense in depth: remove only segments this Node
                        // actually registered (a same-owner segment a newer
                        // Node already appended must survive).
                        !(segment.epoch == cohort && segment.node_epoch == epoch_id)
                    });
                }
                if by_instance
                    .get(&owner)
                    .is_some_and(|segments| segments.segments.is_empty())
                {
                    by_instance.remove(&owner);
                }
            }
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
/// the stable instance key) does NOT reopen the old interval: it APPENDS a new
/// segment whose launch boundary is the RESUME time, with a fresh cohort
/// epoch. A stale guard from the earlier cohort can close only its own
/// segment, never bridge the gap into the resumed one (c-usagefu r11 item 4,
/// Ruling A2).
pub(crate) fn begin_lifetime(
    cwd: &Path,
    owner: &str,
    launched_at: Option<OffsetDateTime>,
) -> LifetimeGuard {
    begin_lifetime_for_epoch(cwd, owner, launched_at, None)
}

/// Production variant scoped to the owning Node's census epoch
/// (c-usagefu r18 item 3).
pub(crate) fn begin_lifetime_for_epoch(
    cwd: &Path,
    owner: &str,
    launched_at: Option<OffsetDateTime>,
    node_epoch: Option<u64>,
) -> LifetimeGuard {
    let key = canonical(cwd);
    let epoch = {
        let mut map = INSTANCE_LIFETIMES.lock().expect("codex instance lifetimes");
        let by_instance = map.entry(key.clone()).or_default();
        let segments = by_instance
            .entry(owner.to_owned())
            .or_insert_with(InstanceSegments::empty);
        // A resume starts a NEW interval: never mutate an existing segment
        // (no floor min, no reopening, no bridging). The cohort epoch is one
        // past the newest segment so a stale guard's close lands on its own
        // segment alone. Registration does NOT prune by time: closed segments
        // are retained while a discovery decision is pending and dropped only
        // when the cwd's last discovery window closes (r11 item 5, Ruling A2).
        let epoch = segments
            .segments
            .last()
            .map_or(1, |segment| segment.epoch.wrapping_add(1));
        segments.segments.push(InstanceLifetime {
            launched_at,
            closed_at: None,
            epoch,
            node_epoch,
        });
        epoch
    };
    LifetimeGuard {
        inner: Arc::new(LifetimeGuardInner {
            key,
            owner: owner.to_owned(),
            epoch,
            node_epoch,
            closed: std::sync::atomic::AtomicBool::new(false),
            drop_closes: std::sync::atomic::AtomicBool::new(true),
            #[cfg(test)]
            closed_at: std::sync::Mutex::new(None),
        }),
        owner: true,
    }
}

/// Pending-discovery ownership for ONE launch, registered TOGETHER before the
/// agent process starts (c-usagefu r12 item 2, Ruling A2'): the census
/// [`LifetimeGuard`] plus this launch's [`DiscoveryWindow`] slot.
///
/// Generic-pty cannot construct the file adapter until agent settlement (up to
/// 60 s after agent.start). Registering the window only at adapter construction
/// left a gap in which another same-cwd window's release was seen as the
/// cwd's LAST window and pruned closed intervals (and cleared prompt
/// evidence) the not-yet-adapted launch's later discovery decision still
/// needed. The launch site holds this bundle and hands BOTH halves to the
/// adapter builder; a launch failure drops it and closes/releases both.
pub(crate) struct PendingCodexLaunch {
    pub(crate) lifetime: LifetimeGuard,
    pub(crate) window: DiscoveryWindow,
    /// The exact agent.start boundary sampled INSIDE the LAUNCH_REGISTRATION
    /// critical section (c-usagefu r14 item 2, Ruling L1). The launch site and
    /// the adapter MUST use THIS value, never one sampled before registration:
    /// a launch blocked on the registration lock while the world moves (a
    /// rollout is written and closed, the last window released) must make its
    /// discovery floor decision from the moment it actually registered.
    pub(crate) launched_at: OffsetDateTime,
}

/// Register a codex launch's lifetime AND its pending-discovery window as one
/// bundle, at the agent.start boundary BEFORE the child is spawned
/// (Ruling A2', r12 item 2).
///
/// c-usagefu r13 item 1: the bundle is ONE critical section with respect to
/// the last-window prune. r12 performed the lifetime insert and the window
/// open as two separate mutex operations, so another same-cwd window releasing
/// in the gap still saw itself as the cwd's LAST window and pruned closed
/// intervals (and cleared prompt evidence) the not-yet-windowed launch's later
/// discovery decision needed. The coordination lock in [`LAUNCH_REGISTRATION`]
/// is held across both halves and across the test barrier between them.
/// c-usagefu r14 item 2 (Ruling L1): the launch boundary is sampled INSIDE
/// this critical section — after acquiring [`LAUNCH_REGISTRATION`], together
/// with the lifetime insert and window retention — and returned on the bundle
/// for the caller and the adapter. A boundary sampled by the caller before
/// taking the lock could predate a close that completed while this launch was
/// queued for the lock, letting the launch bind a rollout created in that
/// window over a since-closed interval the prune had just discarded.
pub(crate) fn register_pending_launch(
    cwd: &Path,
    owner: &str,
    node_epoch: Option<u64>,
) -> PendingCodexLaunch {
    // Test barrier (cfg(test)): parks BEFORE the coordination lock is taken,
    // so a regression can delay the boundary sample until a rollout was
    // written and closed and the last window released while this launch was
    // queued. A no-op in production builds.
    #[cfg(test)]
    pending_launch_pre_lock_barrier(cwd);
    let _registration = acquire_launch_registration(cwd);
    // L1: the boundary IS the critical-section entry instant. Tests may pin a
    // one-shot boundary for deterministic timelines; production always samples.
    #[cfg(test)]
    let launched_at = take_test_launch_boundary(cwd).unwrap_or_else(OffsetDateTime::now_utc);
    #[cfg(not(test))]
    let launched_at = OffsetDateTime::now_utc();
    let lifetime = begin_lifetime_for_epoch(cwd, owner, Some(launched_at), node_epoch);
    // Test barrier (cfg(test)): parks BETWEEN the lifetime insert and the
    // window open WHILE HOLDING the coordination lock, so a barrier-controlled
    // regression can drive another window's release into exactly this gap. In
    // production builds this is a no-op.
    #[cfg(test)]
    pending_launch_test_barrier(cwd);
    let window = DiscoveryWindow::open(cwd, owner);
    PendingCodexLaunch {
        lifetime,
        window,
        launched_at,
    }
}

/// Register a HOOK-CONFIRMED native launch's census lifetime (the
/// shell-pty shadow-home path): L1 exactly as [`register_pending_launch`],
/// but WITHOUT a pending discovery window — a hook-confirmed launch does no
/// prompt-only discovery and its adapter opens no window.
///
/// c-usagefu r15 item 4: L1 applies to EVERY production registration path.
/// The shell-pty spawn used to sample its boundary with a free-standing
/// `now_utc()` before calling [`begin_lifetime`], outside
/// [`LAUNCH_REGISTRATION`]; a launch queued behind another registration could
/// then carry a floor sampled while the cwd's world had already moved. The
/// boundary is sampled INSIDE the critical section here and returned to the
/// spawn site and the adapter.
pub(crate) fn register_confirmed_launch(
    cwd: &Path,
    owner: &str,
    node_epoch: Option<u64>,
) -> (LifetimeGuard, OffsetDateTime) {
    #[cfg(test)]
    pending_launch_pre_lock_barrier(cwd);
    let _registration = acquire_launch_registration(cwd);
    #[cfg(test)]
    let launched_at = take_test_launch_boundary(cwd).unwrap_or_else(OffsetDateTime::now_utc);
    #[cfg(not(test))]
    let launched_at = OffsetDateTime::now_utc();
    let lifetime = begin_lifetime_for_epoch(cwd, owner, Some(launched_at), node_epoch);
    (lifetime, launched_at)
}

/// Test seam: number of discovery windows currently open in `cwd`.
#[cfg(test)]
pub(crate) fn open_window_count(cwd: &Path) -> usize {
    OPEN_WINDOWS
        .lock()
        .expect("codex discovery windows")
        .get(&canonical(cwd))
        .map_or(0, |entry| entry.count as usize)
}

/// Test seam: number of prompt windows retained for `(cwd, owner)`.
#[cfg(test)]
pub(crate) fn recorded_prompt_window_count(cwd: &Path, owner: &str) -> usize {
    SENT_INPUTS
        .lock()
        .expect("codex sent inputs")
        .get(&canonical(cwd))
        .and_then(|by_owner| by_owner.get(owner))
        .map_or(0, |log| log.windows.len())
}

/// Test seam: retained prompt DISPLAY prefixes are never longer than the
/// storage cap. Matching itself uses the full-text fingerprint, not these.
#[cfg(test)]
pub(crate) fn recorded_prompt_texts(cwd: &Path, owner: &str) -> Vec<String> {
    SENT_INPUTS
        .lock()
        .expect("codex sent inputs")
        .get(&canonical(cwd))
        .and_then(|by_owner| by_owner.get(owner))
        .map(|log| {
            log.windows
                .iter()
                .map(|input| input.display_prefix.clone())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
static PENDING_LAUNCH_BARRIER: std::sync::LazyLock<
    std::sync::Mutex<HashMap<PathBuf, std::sync::Arc<PendingLaunchBarrier>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Test-only held guard over the global barrier slot map (c-usagefu r16
/// item 10): lets a contention regression block another thread from taking the
/// slot lock.
#[cfg(test)]
pub(crate) struct PendingSlotGuard(
    /// Held only for its Drop (mutex release); never read.
    #[allow(dead_code)]
    std::sync::MutexGuard<'static, HashMap<PathBuf, std::sync::Arc<PendingLaunchBarrier>>>,
);

#[cfg(test)]
pub(crate) fn lock_pending_launch_slot_for_test() -> PendingSlotGuard {
    PendingSlotGuard(
        PENDING_LAUNCH_BARRIER
            .lock()
            .expect("pending launch barrier"),
    )
}

/// Test-only: whether the per-cwd barrier slot is currently empty.
#[cfg(test)]
pub(crate) fn pending_launch_barrier_slot_absent(cwd: &Path) -> bool {
    PENDING_LAUNCH_BARRIER
        .lock()
        .expect("pending launch barrier")
        .get(&canonical(cwd))
        .is_none()
}

/// One parked point of [`register_pending_launch`]. A disabled stage passes
/// straight through; an armed one marks arrival and blocks until released OR
/// the barrier is disarmed/dropped.
#[cfg(test)]
struct BarrierStage {
    enabled: bool,
    entered: std::sync::Mutex<bool>,
    entered_cv: std::sync::Condvar,
    proceed: std::sync::Mutex<bool>,
    proceed_cv: std::sync::Condvar,
}

#[cfg(test)]
impl BarrierStage {
    fn armed() -> Self {
        Self {
            enabled: true,
            entered: std::sync::Mutex::new(false),
            entered_cv: std::sync::Condvar::new(),
            proceed: std::sync::Mutex::new(false),
            proceed_cv: std::sync::Condvar::new(),
        }
    }

    fn disabled() -> Self {
        let mut stage = Self::armed();
        stage.enabled = false;
        stage
    }

    /// Registration thread: announce arrival, then block until released or the
    /// barrier is disarmed.
    fn reach(&self, alive: &std::sync::atomic::AtomicBool) {
        if !self.enabled {
            return;
        }
        {
            let mut entered = self
                .entered
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *entered = true;
            self.entered_cv.notify_all();
        }
        let mut proceed = self
            .proceed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !*proceed && alive.load(std::sync::atomic::Ordering::SeqCst) {
            proceed = self
                .proceed_cv
                .wait(proceed)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn wait_entered(&self) {
        let mut entered = self
            .entered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !*entered {
            entered = self
                .entered_cv
                .wait(entered)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn release(&self) {
        let mut proceed = self
            .proceed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *proceed = true;
        self.proceed_cv.notify_all();
    }
}

/// Barrier parking [`register_pending_launch`]. Two independent stages:
/// - the PRE-LOCK stage (arm with [`PendingLaunchBarrierHandle::arm_pre_lock`])
///   fires BEFORE the LAUNCH_REGISTRATION lock is taken and the boundary
///   sampled;
/// - the BETWEEN stage (arm with [`PendingLaunchBarrierHandle::arm`]) fires
///   inside the lock between the lifetime insert and the window open (the
///   r13 item 1 gap).
///
/// Only registrations for the armed canonical cwd park (parallel tests share
/// the process and the static slot).
///
/// c-usagefu r15 item 8(c): the barrier is handed out as an owned
/// [`PendingLaunchBarrierHandle`]. Dropping the CALLER's handle disarms the
/// barrier and clears the per-cwd slot when it still holds THIS barrier
/// ([`Arc::ptr_eq`]) — even while a parked registration thread still holds a
/// clone — so a test scope ending always unblocks its registration and the
/// next launch starts clean without an explicit disarm.
#[cfg(test)]
pub(crate) struct PendingLaunchBarrier {
    /// Only registrations for THIS canonical cwd park (parallel tests share
    /// the process and the static slot).
    cwd: PathBuf,
    /// Set false by handle Drop or a newer arm; parked registrations then
    /// return.
    alive: std::sync::atomic::AtomicBool,
    /// Before LAUNCH_REGISTRATION is acquired (and before the boundary sample).
    before_lock: BarrierStage,
    /// Between the lifetime insert and the window open, holding the lock.
    between: BarrierStage,
}

/// Owned handle returned by [`PendingLaunchBarrier`]'s arm constructors. Its
/// Drop disarms the barrier and clears the slot; method calls deref straight
/// to the barrier (c-usagefu r15 item 8c).
#[cfg(test)]
#[allow(dead_code)]
pub(crate) struct PendingLaunchBarrierHandle(std::sync::Arc<PendingLaunchBarrier>);

#[cfg(test)]
impl PendingLaunchBarrierHandle {
    /// Arm the BETWEEN stage.
    pub(crate) fn arm(cwd: &Path) -> Self {
        PendingLaunchBarrier::arm(cwd)
    }

    /// Arm the PRE-LOCK stage.
    pub(crate) fn arm_pre_lock(cwd: &Path) -> Self {
        PendingLaunchBarrier::arm_pre_lock(cwd)
    }
}

#[cfg(test)]
impl std::ops::Deref for PendingLaunchBarrierHandle {
    type Target = PendingLaunchBarrier;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
impl Drop for PendingLaunchBarrierHandle {
    fn drop(&mut self) {
        // Wake every parked registration FIRST (this barrier must never park
        // anything once the caller is done with it) — this touches only the
        // barrier's own cv mutexes, never the global slot lock.
        self.0.disarm();
        // Clear the slot ONLY when it still holds this exact barrier, so a
        // newer arm for the same cwd is never accidentally removed. Take the
        // slot lock AFTER disarming and with a BLOCKING lock (c-usagefu r16
        // item 10), so the slot is guaranteed absent on return even under
        // contention. This cannot deadlock: a parked registration holds the
        // slot lock only long enough to clone the barrier Arc — it parks on
        // the stage's own cv mutex with the slot lock released — and disarming
        // above already released any parked thread. The old try_lock skipped
        // removal while another thread transiently held the mutex, leaking the
        // stale barrier into the next launch.
        let mut slot = PENDING_LAUNCH_BARRIER
            .lock()
            .expect("pending launch barrier");
        let still_us = slot
            .get(&self.0.cwd)
            .is_some_and(|current| std::sync::Arc::ptr_eq(current, &self.0));
        if still_us {
            slot.remove(&self.0.cwd);
        }
    }
}

#[cfg(test)]
impl PendingLaunchBarrier {
    /// Arm the BETWEEN stage (r13 item 1): registration parks inside the lock
    /// after inserting its lifetime and before opening its window.
    pub(crate) fn arm(cwd: &Path) -> PendingLaunchBarrierHandle {
        PendingLaunchBarrierHandle(Self::arm_stage(cwd, false, true))
    }

    /// Arm the PRE-LOCK stage (r14 item 2, Ruling L1): registration parks
    /// BEFORE taking LAUNCH_REGISTRATION and before sampling its boundary.
    pub(crate) fn arm_pre_lock(cwd: &Path) -> PendingLaunchBarrierHandle {
        PendingLaunchBarrierHandle(Self::arm_stage(cwd, true, false))
    }

    fn arm_stage(cwd: &Path, pre_lock: bool, between: bool) -> std::sync::Arc<Self> {
        let barrier = std::sync::Arc::new(Self {
            cwd: canonical(cwd),
            alive: std::sync::atomic::AtomicBool::new(true),
            before_lock: if pre_lock {
                BarrierStage::armed()
            } else {
                BarrierStage::disabled()
            },
            between: if between {
                BarrierStage::armed()
            } else {
                BarrierStage::disabled()
            },
        });
        // Replace AND disarm a previous barrier for THIS cwd FIRST (r14
        // item 6). The slot is keyed per cwd so parallel tests never disarm
        // one another; a registration parked on the old entry is released
        // before the new barrier arms.
        let previous = {
            let mut slot = PENDING_LAUNCH_BARRIER
                .lock()
                .expect("pending launch barrier");
            slot.insert(barrier.cwd.clone(), std::sync::Arc::clone(&barrier))
        };
        if let Some(previous) = previous {
            previous.disarm();
        }
        barrier
    }

    /// Release parked registrations and mark this barrier dead. Safe to call
    /// from any thread: touches only this barrier's own cv mutexes.
    fn disarm(&self) {
        self.alive.store(false, std::sync::atomic::Ordering::SeqCst);
        self.before_lock.release();
        self.between.release();
    }

    /// Block until a registration thread has entered the PRE-LOCK stage.
    pub(crate) fn wait_before_lock(&self) {
        self.before_lock.wait_entered();
    }

    /// Release the PRE-LOCK stage: the registration then takes the lock and
    /// samples its boundary.
    pub(crate) fn release_pre_lock(&self) {
        self.before_lock.release();
    }

    /// Block until a registration thread has entered the BETWEEN stage.
    pub(crate) fn wait_to_enter(&self) {
        self.between.wait_entered();
    }

    /// Let the parked registration finish (its window opens immediately after).
    pub(crate) fn release(&self) {
        self.between.release();
    }
}

/// Park at one stage only while the armed, still-alive barrier is for `cwd`.
#[cfg(test)]
fn park_at_stage(cwd: &Path, pick: impl Fn(&PendingLaunchBarrier) -> &BarrierStage) {
    let key = canonical(cwd);
    let barrier = {
        let slot = PENDING_LAUNCH_BARRIER
            .lock()
            .expect("pending launch barrier");
        slot.get(&key).map(std::sync::Arc::clone)
    };
    if let Some(barrier) = barrier {
        pick(&barrier).reach(&barrier.alive);
    }
}

#[cfg(test)]
fn pending_launch_pre_lock_barrier(cwd: &Path) {
    park_at_stage(cwd, |barrier| &barrier.before_lock);
}

#[cfg(test)]
fn pending_launch_test_barrier(cwd: &Path) {
    park_at_stage(cwd, |barrier| &barrier.between);
}

/// One-shot deterministic launch boundary for [`register_pending_launch`]
/// tests (r12 item 2 keeps an exact timeline). Keyed by canonical cwd so
/// parallel tests never share it; consumed on read.
#[cfg(test)]
static TEST_LAUNCH_BOUNDARIES: std::sync::LazyLock<
    std::sync::Mutex<HashMap<PathBuf, OffsetDateTime>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

#[cfg(test)]
pub(crate) fn set_test_launch_boundary(cwd: &Path, at: OffsetDateTime) {
    TEST_LAUNCH_BOUNDARIES
        .lock()
        .expect("test launch boundaries")
        .insert(canonical(cwd), at);
}

#[cfg(test)]
fn take_test_launch_boundary(cwd: &Path) -> Option<OffsetDateTime> {
    TEST_LAUNCH_BOUNDARIES
        .lock()
        .expect("test launch boundaries")
        .remove(&canonical(cwd))
}
/// Called when the cwd's LAST discovery window closes: no attribution decision
/// is then pending, and Ruling A2 (c-usagefu r11 item 5) bounds the registry
/// by PENDING DECISIONS rather than by a fixed time horizon. A segment that
/// closed cannot overlap a decision made by a window that opened only after
/// every earlier window was gone, so the discard is sound. A live segment
/// (closed_at None) is always retained.
fn prune_closed_for_cwd(key: &Path) {
    let mut map = INSTANCE_LIFETIMES.lock().expect("codex instance lifetimes");
    if let Some(by_instance) = map.get_mut(key) {
        for segments in by_instance.values_mut() {
            segments
                .segments
                .retain(|segment| segment.closed_at.is_none());
        }
        // Drop owners with no interval left (every segment closed), so the
        // census genuinely forgets them rather than keeping an empty shell.
        by_instance.retain(|_, segments| !segments.segments.is_empty());
    }
}

/// Test seam: record this guard's close at `at` instead of wall-clock time,
/// so a regression can place the close deterministically on its timeline.
#[cfg(test)]
pub(crate) trait LifetimeGuardClock {
    fn close_lifetime_at(&self, at: OffsetDateTime);
}

/// Test seam: move `owner`'s newest segment's recorded close to `at`, as if
/// wall-clock time had advanced to `at` after the interval closed. Lets
/// retention regressions place a closed segment arbitrarily far in the past
/// without sleeping (r11 item 5).
#[cfg(test)]
pub(crate) fn retroactively_close_at(cwd: &Path, owner: &str, at: OffsetDateTime) {
    let key = canonical(cwd);
    let mut map = INSTANCE_LIFETIMES.lock().expect("codex instance lifetimes");
    if let Some(segments) = map
        .get_mut(&key)
        .and_then(|by_instance| by_instance.get_mut(owner))
        && let Some(last) = segments.segments.last_mut()
    {
        last.closed_at = Some(at);
    }
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
        // c-usagefu r13 item 1: the refcount drop and the last-window effects
        // (discoverer removal, prompt clear, closed-segment prune) are ONE
        // critical section against a pending launch's registration. While this
        // lock is held, register_pending_launch cannot sit between its
        // lifetime insert and its window open, so this release is either
        // genuinely the cwd's last window (the launching instance does not
        // exist yet either) or it sees the launch's window and does not prune.
        let _registration = acquire_launch_registration(&self.key);
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
        // r7 item 10 + r11 item 5 (Ruling A2): when this is the LAST open
        // window for the cwd, no discovery lifecycle remains and no
        // attribution decision is pending. Discard all recorded prompts so
        // they cannot authorize a future unrelated rollout, and drop every
        // CLOSED lifetime segment — retained only while a pending decision
        // could overlap them, never on a fixed time horizon. LIVE segments are
        // kept (a bound instance stays live after its window released).
        if last_window {
            if let Some(by_instance) = SENT_INPUTS
                .lock()
                .expect("codex sent inputs")
                .get_mut(&self.key)
            {
                by_instance.clear();
            }
            prune_closed_for_cwd(&self.key);
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

/// Bound on the retained DISPLAY prefix of one prompt window, in chars
/// (c-usagefu r15 item 4). The full prompt is never retained verbatim: matching
/// uses a digest of the COMPLETE trimmed prompt (see [`PromptFingerprint`]),
/// and only this short, human-readable prefix is kept for diagnostics. It never
/// participates in a match (c-usagefu r16 item 6), so two prompts sharing a
/// prefix cannot be confused.
const MAX_PROMPT_CHARS: usize = 8 * 1024;

/// Maximum dispatch windows retained per owner, even while its discovery is
/// still OPEN (c-usagefu r15 item 4). r11 item 10 ruled out a cap to protect
/// a lazily-scanned rollout's original prompt; this round bounds retention
/// absolutely, oldest-first. At a human dispatch cadence 256 windows is far
/// beyond any real discovery need, and a single owner can never make prompt
/// text accumulate without bound.
const MAX_PROMPT_WINDOWS_PER_OWNER: usize = 256;

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
                // Live unbound discoverer: every dispatch window stays
                // eligible regardless of age (a lazily-scanned rollout can be
                // discovered long after the keystroke), but retention is still
                // bounded by MAX_PROMPT_WINDOWS_PER_OWNER (r15 item 4); the
                // length trim happens in put_input on the write lock.
                return !log.windows.is_empty();
            }
            log.windows.retain(|input| input.at >= cutoff);
            !log.windows.is_empty()
        });
        !by_instance.is_empty()
    });
}

/// Collision-resistant identity of one COMPLETE trimmed prompt (c-usagefu r16
/// item 6). The digest is SHA-256 over every byte of the trimmed prompt with NO
/// truncation, and `len_chars` records its full char length: two prompts that
/// share a long prefix but differ anywhere (including only after the old
/// display-prefix bound) get different fingerprints, so a rollout carrying the
/// wrong suffix can never bind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct PromptFingerprint {
    digest: [u8; 32],
    len_chars: usize,
}

/// Compute the [`PromptFingerprint`] of an already-trimmed prompt over its full
/// content without retaining the text.
fn prompt_fingerprint(trimmed: &str) -> PromptFingerprint {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(trimmed.as_bytes());
    PromptFingerprint {
        digest: hasher.finalize().into(),
        len_chars: trimmed.chars().count(),
    }
}

/// Bounded, human-readable prefix kept for diagnostics only; never used to
/// match (c-usagefu r16 item 6).
fn prompt_display_prefix(trimmed: &str) -> String {
    match trimmed.char_indices().nth(MAX_PROMPT_CHARS) {
        Some((idx, _)) => trimmed[..idx].to_owned(),
        None => trimmed.to_owned(),
    }
}

#[derive(Clone)]
struct SentInput {
    /// Identity of the COMPLETE trimmed prompt; this is what matching compares.
    fingerprint: PromptFingerprint,
    /// Bounded display prefix for diagnostics; never part of the match.
    #[cfg_attr(not(test), allow(dead_code))]
    display_prefix: String,
    /// Time this dispatch's evidence window starts: every send gets its own
    /// window, including a resend of an earlier prompt.
    at: OffsetDateTime,
}

/// Per-instance prompt evidence (c-usagefu r11 item 10, Ruling): while a
/// discovery is OPEN every dispatched prompt is retained as its OWN window —
/// a later RESEND never overwrites an earlier window (the old anchor+recent
/// shape drained an unbound discoverer's 33rd distinct prompt and moved a
/// resent prompt's window forward, losing evidence a lazily-scanned rollout
/// still needed). Reclamation is by lifecycle: a bind releases the window and
/// window close clears the cwd's prompts in [`DiscoveryWindow::release`].
///
/// c-usagefu r15 item 4 bounds the accumulation absolutely: the TEXT of every
/// window is capped at [`MAX_PROMPT_CHARS`] and at most
/// [`MAX_PROMPT_WINDOWS_PER_OWNER`] windows are retained (oldest first), live
/// discoverers included.
#[derive(Default)]
struct PromptLog {
    windows: Vec<SentInput>,
}

impl PromptLog {
    fn windows(&self) -> impl Iterator<Item = &SentInput> {
        self.windows.iter()
    }
}

/// cwd -> (instance key -> prompt evidence). Every instance authorizes only
/// its OWN prompts.
type SentInputs = HashMap<PathBuf, HashMap<String, PromptLog>>;
static SENT_INPUTS: LazyLock<Mutex<SentInputs>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Whitespace-trimmed comparison form (codex stores the prompt verbatim in
/// the `input_text` item), bounded to [`MAX_PROMPT_CHARS`] (c-usagefu r15
/// item 4). Both the dispatched prompt and the rollout's first user text pass
/// through this same form in [`evaluate_prompt`], so a real prompt — even one
/// longer than the cap — still matches; only a shared bounded prefix collides.
/// Whitespace-trimmed form of a prompt. The digest is computed over this WHOLE
/// string (c-usagefu r16 item 6), so a prompt longer than the display bound
/// still matches its rollout while two prompts sharing a prefix cannot.
fn trim_prompt(text: &str) -> &str {
    text.trim()
}

fn put_input(cwd: &Path, owner: &str, text: &str, at: OffsetDateTime) {
    let cwd_key = canonical(cwd);
    let trimmed = trim_prompt(text);
    if trimmed.is_empty() {
        return;
    }
    let fingerprint = prompt_fingerprint(trimmed);
    let display_prefix = prompt_display_prefix(trimmed);
    let mut map = SENT_INPUTS.lock().expect("codex sent inputs");
    let log = map
        .entry(cwd_key)
        .or_default()
        .entry(owner.to_owned())
        .or_default();
    // Every dispatch is a distinct window: even an identical resend is pushed,
    // so the earlier window keeps its own send-time back edge.
    log.windows.push(SentInput {
        fingerprint,
        display_prefix,
        at,
    });
    // c-usagefu r15 item 4: hard per-owner window cap, oldest first — prompt
    // evidence never accumulates without bound, even for a live discoverer.
    if log.windows.len() > MAX_PROMPT_WINDOWS_PER_OWNER {
        let excess = log.windows.len() - MAX_PROMPT_WINDOWS_PER_OWNER;
        log.windows.drain(..excess);
    }
}

/// Record that instance `owner` dispatched `text` in `cwd` at `now`.
pub fn record_input(cwd: &Path, owner: &str, text: &str) {
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
    // Digest the COMPLETE trimmed rollout prompt: matching compares the
    // collision-resistant fingerprint (digest + full length), never a truncated
    // prefix, so a different rollout that shares a prefix cannot bind
    // (c-usagefu r16 item 6).
    let wanted = prompt_fingerprint(trim_prompt(rollout_text));
    if rollout_text.trim().is_empty() {
        return PromptVerdict::Ineligible;
    }
    let blocker = {
        let lifetimes = INSTANCE_LIFETIMES.lock().expect("codex instance lifetimes");
        let Some(by_instance) = lifetimes.get(&cwd_key) else {
            return PromptVerdict::Ineligible;
        };
        let Some(own_segments) = by_instance.get(owner) else {
            return PromptVerdict::Ineligible;
        };
        // The owner judges from its CURRENT live interval only: a resume
        // started a NEW segment at the resume time and a closed segment never
        // reopens or bridges (Ruling A2). With no live segment, or with an
        // UNKNOWN launch boundary (a promoted instance), prompt-only
        // attribution is never used.
        let Some(own_live) = own_segments.live() else {
            return PromptVerdict::Ineligible;
        };
        let Some(own_launch) = own_live.launched_at else {
            return PromptVerdict::Ineligible;
        };
        // Strict launch floor: no pre-launch slack for prompt attribution. A
        // rollout created in a pre-resume GAP is therefore never attributable
        // to the resumed segment.
        if session_started < own_launch {
            return PromptVerdict::Ineligible;
        }
        // Complete-census sole-live proof over [launch, creation]. Another
        // instance overlaps when ANY of its registered segments was live at
        // any instant across that interval. A KNOWN-boundary segment overlaps
        // iff it launched at or before creation and was still live at the
        // owner's launch. An UNKNOWN-boundary (promoted) segment cannot be
        // proven to have launched after creation, so it overlaps whenever it
        // was still live at the owner's launch — the cwd census is incomplete
        // and prompt-only attribution is refused (Ruling A; closed exactly at
        // the boundary is not an overlap). Every segment is considered: a
        // resumed instance's retained closed segment still counts for the
        // window it was actually live, and its new segment never reaches back
        // over the gap.
        by_instance
            .iter()
            .filter(|(other, _)| *other != owner)
            .find_map(|(other, segments)| {
                segments.segments.iter().find_map(|segment| {
                    let overlaps = segment
                        .launched_at
                        .is_none_or(|launched| launched <= session_started)
                        && segment.closed_at.is_none_or(|closed| closed > own_launch);
                    overlaps.then(|| other.clone())
                })
            })
    };
    let prompt_fits = {
        let inputs = SENT_INPUTS.lock().expect("codex sent inputs");
        inputs
            .get(&cwd_key)
            .and_then(|by_owner| by_owner.get(owner))
            .is_some_and(|log| {
                log.windows().any(|input| {
                    input.fingerprint == wanted
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
#[cfg(any(test, feature = "test-stub"))]
pub fn forget_lifetime(cwd: &Path, owner: &str) {
    let key = canonical(cwd);
    if let Some(by_instance) = INSTANCE_LIFETIMES
        .lock()
        .expect("codex instance lifetimes")
        .get_mut(&key)
    {
        by_instance.remove(owner);
    }
}

/// Test seam: whether `owner` currently has a registered lifetime in `cwd`
/// (live or retained). Ruling A2 regressions assert the census the moment a
/// promotion/demotion is published (r11 items 2/3).
#[cfg(test)]
pub(crate) fn census_contains(cwd: &Path, owner: &str) -> bool {
    INSTANCE_LIFETIMES
        .lock()
        .expect("codex instance lifetimes")
        .get(&canonical(cwd))
        .is_some_and(|by_instance| by_instance.contains_key(owner))
}

/// Test seam: whether `owner`'s lifetime in `cwd` is still LIVE (its interval
/// has not been closed). A retained closed interval returns false (r11 item 3:
/// the close must follow the process stop, never precede close beginning).
#[cfg(any(test, feature = "test-stub"))]
pub fn census_lifetime_is_live(cwd: &Path, owner: &str) -> bool {
    INSTANCE_LIFETIMES
        .lock()
        .expect("codex instance lifetimes")
        .get(&canonical(cwd))
        .and_then(|by_instance| by_instance.get(owner))
        .is_some_and(|segments| segments.live().is_some())
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
