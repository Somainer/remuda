//! Node-owned pending-termination supervision (c-usagefu r14 items 3/4,
//! Ruling W1).
//!
//! A stop-incomplete teardown (a survivor outlived the SIGKILL ladder) cannot
//! close the codex census intervals — the stop ladder's contract forbids
//! reporting the tree gone while a named survivor is still running. The r13
//! bounded watcher polled each survivor for a fixed bound and then dropped the
//! guards without a recorded close, which left the intervals live for the rest
//! of the Node lifetime and blocked same-cwd prompt-only discovery.
//!
//! Ruling W1 replaces the drop with a TWO-LEVEL, Node-owned mechanism:
//!
//! 1. **Per-close watcher** — one task per stop-incomplete teardown, bounded
//!    (same 250 ms poll / bound as r13). While it runs it owns the census
//!    guards and closes them ONLY after the survivor identity is confirmed
//!    gone.
//! 2. **Pending-termination registry** — a survivor that outlives the per-close
//!    bound moves, guards and process identity together, into a Node-owned
//!    registry. It is rechecked, bounded and cheaply (a signal-0 / `/proc`
//!    probe per entry, a small cap per call), on later codex discovery in the
//!    same cwd and by a periodic maintenance sweep. The interval closes only
//!    after CONFIRMED exit; after that ordinary last-window pruning applies.
//!
//! All watcher tasks and registry entries are owned by ONE Node-level
//! supervisor (process-global here, exactly like the discovery registry). On
//! Node shutdown [`shutdown_pending_terminations`] abandons the periodic sweep,
//! aborts and AWAITS every watcher, and drops every registry entry — the
//! guards' Drop was suppressed when the watch took them, so none of this
//! records a false close.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
#[cfg(test)]
use std::sync::{MutexGuard, atomic::AtomicU64};
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::adapters::codex_discovery::LifetimeGuard;
use crate::shell_pty::lifecycle::{
    ProcessIdentity, TERMINATION_WATCH_POLL, termination_identity_alive, termination_watch_bound,
};

/// Process identity a pending termination probes: the signalled process GROUP
/// (which may already be gone while an escaped descendant survives) plus the
/// explicitly named survivor (pid, start time) identities (Ruling F).
#[derive(Clone)]
pub(crate) struct TerminationIdentity {
    pub(crate) pgid: Option<i32>,
    pub(crate) survivors: Vec<ProcessIdentity>,
    /// Ruling F' (c-usagefu r16 item 2): the pre-signal process-tree snapshot
    /// could not be read, so no descendant identity exists. Unknown can never
    /// be proven gone — every probe reports alive; the entry leaves only via
    /// conservative eviction or Node shutdown, and the next Node reconciles.
    pub(crate) observation_unknown: bool,
}

/// One stop-incomplete teardown whose intervals have no confirmed close yet.
struct PendingEntry {
    /// Census intervals kept live until the identity is confirmed gone.
    guards: Vec<LifetimeGuard>,
    identity: TerminationIdentity,
    /// cwd the intervals live in, so a discovery recheck probes only entries
    /// its decision could care about.
    cwd: Option<PathBuf>,
}

/// Maximum entries one discovery-triggered recheck probes. Discovery stays a
/// cheap, bounded operation.
const DISCOVERY_RECHECK_CAP: usize = 16;

/// Maximum identity probes one maintenance sweep performs (c-usagefu r15 item
/// 5: every maintenance pass is BOUNDED — never `usize::MAX`). Rotation makes
/// repeated sweeps cover every entry.
const MAINTENANCE_RECHECK_CAP: usize = 32;

/// Hard cap on registry entries (c-usagefu r15 item 5). A stop-incomplete
/// teardown past the cap evicts the OLDEST entry; the eviction is
/// CONSERVATIVE — its guards are dropped with close-on-drop suppressed, so no
/// interval is closed by eviction. The interval stays live in the census and
/// is reconciled by the next Node process; a false close would be worse.
const MAX_PENDING_ENTRIES: usize = 64;

struct Supervisor {
    /// Survivors that outlived their per-close bound. A FIFO deque so every
    /// bounded recheck rotates the oldest probed entries to the back: a dead
    /// entry behind a string of still-alive ones cannot hide forever.
    registry: StdMutex<VecDeque<PendingEntry>>,
    /// Number of registry entries, for a lock-free "nothing pending" fast path
    /// on the discovery hot path.
    pending: AtomicUsize,
    /// Per-close bounded watcher tasks owned by the Node.
    watchers: StdMutex<Vec<JoinHandle<()>>>,
    /// Periodic registry maintenance sweep.
    maintenance: StdMutex<Option<JoinHandle<()>>>,
    /// Set by shutdown: a late watcher expiry drops its guards instead of
    /// pushing into a registry the Node is tearing down.
    shutting_down: AtomicBool,
}

impl Supervisor {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            registry: StdMutex::new(VecDeque::new()),
            pending: AtomicUsize::new(0),
            watchers: StdMutex::new(Vec::new()),
            maintenance: StdMutex::new(None),
            shutting_down: AtomicBool::new(false),
        })
    }

    fn reap_finished_watchers(&self) {
        let mut watchers = self
            .watchers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        watchers.retain(|handle| !handle.is_finished());
    }

    /// Own the census guards of one stop-incomplete teardown: bounded poll
    /// here, then the Node registry after the bound expires.
    fn watch(self: &Arc<Self>, entry: PendingEntry) {
        for guard in &entry.guards {
            guard.suppress_drop_close();
        }
        // Spawning requires a live runtime; watch() is reached from the async
        // close path, but degrade safely if it ever is not.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                "no tokio runtime for the termination watcher; census intervals stay live"
            );
            return;
        };
        self.reap_finished_watchers();
        let supervisor = Arc::clone(self);
        let task = handle.spawn(async move {
            let bound = termination_watch_bound();
            let start = tokio::time::Instant::now();
            loop {
                if !termination_identity_alive(
                    entry.identity.pgid,
                    &entry.identity.survivors,
                    entry.identity.observation_unknown,
                ) {
                    close_guards(&entry.guards);
                    return;
                }
                if start.elapsed() >= bound {
                    break;
                }
                tokio::time::sleep(TERMINATION_WATCH_POLL).await;
            }
            // Bound expired while the survivor is still there: one last probe
            // (it may have exited in the final tick), then hand guards and
            // identity to the Node-owned pending-termination registry.
            if !termination_identity_alive(
                entry.identity.pgid,
                &entry.identity.survivors,
                entry.identity.observation_unknown,
            ) {
                close_guards(&entry.guards);
                return;
            }
            if supervisor.shutting_down.load(Ordering::SeqCst) {
                // Node shutdown: the guard Drop records nothing (suppressed).
                drop(entry);
                return;
            }
            {
                let mut registry = supervisor
                    .registry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // c-usagefu r15 item 5: cap the registry. An over-cap entry
                // evicts the OLDEST one CONSERVATIVELY: the taken entry's
                // guards are dropped with close-on-drop suppressed OUTSIDE the
                // lock below, so the eviction never closes an interval.
                while registry.len() >= MAX_PENDING_ENTRIES {
                    if let Some(evicted) = registry.pop_front() {
                        supervisor.drop_entry_conservatively(evicted);
                    }
                }
                registry.push_back(entry);
                supervisor.pending.store(registry.len(), Ordering::SeqCst);
            }
            supervisor.ensure_maintenance();
        });
        self.watchers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(task);
    }

    /// Drop an evicted registry entry WITHOUT recording a close (c-usagefu r15
    /// item 5). Every guard transferred into the registry had its Drop-close
    /// suppressed in [`Supervisor::watch`], so dropping here keeps the census
    /// interval live — conservative by design. Call while holding the registry
    /// lock; this touches no lock.
    fn drop_entry_conservatively(&self, entry: PendingEntry) {
        let cwd = entry
            .cwd
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<unknown>".to_owned());
        tracing::warn!(
            cwd = %cwd,
            "pending-termination registry full; evicting the oldest entry \
             CONSERVATIVELY — its census intervals stay live"
        );
        drop(entry);
    }

    /// Start the bounded periodic sweep that closes registry entries once
    /// their survivor identity is confirmed gone. Spawned lazily on the first
    /// registry transfer; idempotent.
    fn ensure_maintenance(self: &Arc<Self>) {
        let mut slot = self
            .maintenance
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.as_ref().is_some_and(|handle| !handle.is_finished()) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let supervisor = Arc::clone(self);
        *slot = Some(handle.spawn(async move {
            // No immediate recheck: the watcher just probed this identity
            // alive at the transfer, and a same-instant sweep tick could be
            // delayed by scheduling and land as an unbounded early close. The
            // first sweep is one period later; discovery remains the prompt
            // recheck point.
            loop {
                tokio::time::sleep(maintenance_interval()).await;
                // c-usagefu r15 item 5: BOUNDED sweep. The FIFO rotation in
                // recheck means successive sweeps cover the whole registry;
                // one sweep must never be an unbounded usize::MAX scan.
                supervisor.recheck(None, MAINTENANCE_RECHECK_CAP);
                // Nothing left to wait for: the sweep exits and is restarted
                // lazily by the next expiry transfer.
                if supervisor.pending.load(Ordering::SeqCst) == 0 {
                    return;
                }
            }
        }));
    }

    /// Probe pending entries and close the guards of every entry whose
    /// survivor identity is confirmed gone. `cwd` scopes the probe to one
    /// directory (a discovery recheck); `None` is the unscoped maintenance
    /// sweep. At most `cap` matching entries are PROBED per call — bounded and
    /// cheap on the discovery path.
    ///
    /// c-usagefu r15 item 5 (Ruling F): the registry is a FIFO; every popped
    /// entry rotates to the back whether or not it was probed this pass. A
    /// string of cheaply-alive entries at the front therefore cannot hide a
    /// dead entry behind them — within `ceil(len/cap)` passes every entry is
    /// reached. Out-of-scope entries rotate without spending probe budget.
    fn recheck(self: &Arc<Self>, cwd: Option<&Path>, cap: usize) {
        if self.shutting_down.load(Ordering::SeqCst) || self.pending.load(Ordering::SeqCst) == 0 {
            return;
        }
        let want_cwd = cwd.map(crate::adapters::codex_discovery::canonical_path);
        let mut confirmed: Vec<PendingEntry> = Vec::new();
        {
            let mut registry = self
                .registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let total = registry.len();
            let mut probed = 0;
            // In-scope entries the cap left UNPROBED keep their place at the
            // FRONT so the next pass starts exactly where this one stopped;
            // rotated/probed entries go to the back.
            let mut unprobed_head: VecDeque<PendingEntry> = VecDeque::new();
            for _ in 0..total {
                let Some(entry) = registry.pop_front() else {
                    break;
                };
                let in_scope = want_cwd
                    .as_ref()
                    .is_none_or(|want| entry.cwd.as_ref() == Some(want));
                if !in_scope {
                    // Different cwd: rotate, no probe, no budget spent.
                    registry.push_back(entry);
                    continue;
                }
                if probed >= cap {
                    // Budget spent for this pass: keep this entry at the
                    // front for the next pass.
                    unprobed_head.push_back(entry);
                    continue;
                }
                probed += 1;
                if termination_identity_alive(
                    entry.identity.pgid,
                    &entry.identity.survivors,
                    entry.identity.observation_unknown,
                ) {
                    registry.push_back(entry);
                } else {
                    // Confirmed gone: take the guards and drop the entry. The
                    // close is recorded AFTER releasing the registry lock.
                    confirmed.push(entry);
                }
            }
            while let Some(entry) = unprobed_head.pop_back() {
                registry.push_front(entry);
            }
            self.pending.store(registry.len(), Ordering::SeqCst);
        }
        for entry in confirmed {
            close_guards(&entry.guards);
        }
    }

    /// Node shutdown: stop the maintenance sweep, cancel and AWAIT every
    /// watcher, and drop every registry entry WITHOUT recording a close. The
    /// runtime stays alive while this runs, so aborts are delivered and awaited
    /// here rather than observed on some later drop.
    async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        // Take the sweep handle and drop the std-mutex guard BEFORE awaiting
        // it (never hold a blocking guard across an await).
        let maintenance = self
            .maintenance
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(maintenance) = maintenance {
            maintenance.abort();
            let _ = maintenance.await;
        }
        let watchers: Vec<_> = self
            .watchers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect();
        for watcher in watchers {
            watcher.abort();
            let _ = watcher.await;
        }
        // Dropping the entries drops suppressed guards: no close is recorded.
        let _entries = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect::<Vec<_>>();
        self.pending.store(0, Ordering::SeqCst);
    }

    /// Test-only: whether the registry currently holds an entry for `cwd`.
    #[cfg(any(test, feature = "test-stub"))]
    fn has_pending_for(&self, cwd: &Path) -> bool {
        let want = crate::adapters::codex_discovery::canonical_path(cwd);
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|entry| entry.cwd.as_ref() == Some(&want))
    }

    /// Test-only: current registry length.
    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Test-only: put one entry DIRECTLY into the registry, with the same
    /// drop-close suppression the production transfer applies. Unit tests for
    /// the registry hygiene rules (cap, rotation, Ruling F identity) bypass
    /// the time-bounded per-close watcher; production wiring is covered by the
    /// shell-pty stop-incomplete regressions.
    #[cfg(test)]
    fn test_push(&self, mut entry: PendingEntry) {
        for guard in &entry.guards {
            guard.suppress_drop_close();
        }
        // Avoid an unused-mutation warning for the immutable test entries.
        let _ = &mut entry;
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while registry.len() >= MAX_PENDING_ENTRIES {
            if let Some(evicted) = registry.pop_front() {
                self.drop_entry_conservatively(evicted);
            }
        }
        registry.push_back(entry);
        self.pending.store(registry.len(), Ordering::SeqCst);
    }
}

fn close_guards(guards: &[LifetimeGuard]) {
    for guard in guards {
        guard.close_interval();
    }
}

/// The process-global Node supervisor (its lifetime is the Node process,
/// matching the discovery registry it protects).
static GLOBAL_SUPERVISOR: LazyLock<StdMutex<Option<Arc<Supervisor>>>> =
    LazyLock::new(|| StdMutex::new(None));

fn global_supervisor() -> Arc<Supervisor> {
    let mut slot = GLOBAL_SUPERVISOR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    slot.get_or_insert_with(Supervisor::new).clone()
}

/// Own the census guards of a stop-incomplete teardown (production entry
/// point from the shell-pty close path). Bounded per-close polling here; the
/// Node-owned pending-termination registry after the bound. Every guard's Drop
/// is suppressed: only confirmed-exit closes record.
pub(crate) fn watch_pending_termination(guards: Vec<LifetimeGuard>, identity: TerminationIdentity) {
    let cwd = guards.first().map(|guard| guard.cwd().to_path_buf());
    global_supervisor().watch(PendingEntry {
        guards,
        identity,
        cwd,
    });
}

/// Recheck pending terminations a codex discovery in `cwd` could unblock
/// (Ruling W1): bounded and cheap, and a no-op while the registry is empty.
pub(crate) fn recheck_for_discovery(cwd: &Path) {
    global_supervisor().recheck(Some(cwd), DISCOVERY_RECHECK_CAP);
}

/// Node shutdown: cancel and await every watcher/sweep and drop the pending
/// registry without recording closures (Ruling W1).
pub async fn shutdown_pending_terminations() {
    let supervisor = GLOBAL_SUPERVISOR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(supervisor) = supervisor {
        supervisor.shutdown().await;
    }
}

// -- Cross-crate observation seams (remuda-node integration tests) ----------
//
// Read-only access to census/supervisor state for tests that drive the REAL
// DevNode shutdown wiring. They install no guards and substitute no stop
// outcome — behaviour stays entirely production; these only observe it.
/// Test-stub observation seam: whether `owner`'s census interval is live.
#[cfg(any(test, feature = "test-stub"))]
pub fn census_interval_is_live(cwd: &Path, owner: &str) -> bool {
    crate::adapters::codex_discovery::census_lifetime_is_live(cwd, owner)
}

/// Test-stub observation seam: whether the pending registry holds an entry for `cwd`.
#[cfg(any(test, feature = "test-stub"))]
pub fn pending_entry_exists_for(cwd: &Path) -> bool {
    global_supervisor().has_pending_for(cwd)
}

/// Test-stub hygiene seam: forget `owner`'s census intervals in `cwd`.
#[cfg(any(test, feature = "test-stub"))]
pub fn forget_census_for_test(cwd: &Path, owner: &str) {
    crate::adapters::codex_discovery::forget_lifetime(cwd, owner);
}
// -- Test timing seams -------------------------------------------------------

/// Maintenance sweep interval override (millis), zero means production. The
/// per-close watch bound is owned by the lifecycle module
/// (`lifecycle::set_test_watch_bound`).
#[cfg(test)]
static TEST_MAINTENANCE_INTERVAL_MS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
pub(crate) fn set_test_maintenance_interval(d: Option<Duration>) {
    TEST_MAINTENANCE_INTERVAL_MS.store(
        d.map(|x| x.as_millis().min(u64::MAX as u128) as u64)
            .unwrap_or(0),
        Ordering::SeqCst,
    );
}

#[cfg(test)]
pub(crate) fn test_has_pending_for(cwd: &Path) -> bool {
    global_supervisor().has_pending_for(cwd)
}

/// Supervisor teardown observation seam; the node-level shutdown regression
/// observes teardown through the real `DevNode::shutdown`, so this direct
/// supervisor-state check is kept for focused debugging.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn test_global_is_shut() -> bool {
    GLOBAL_SUPERVISOR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_none()
}

/// Process-global termination tests mutate shared supervisor state; serialize
/// them against one another (each still uses a unique cwd for census data).
#[cfg(test)]
static TEST_LOCK: LazyLock<StdMutex<()>> = LazyLock::new(|| StdMutex::new(()));

#[cfg(test)]
/// The guard is read never and dropped on scope exit — its lifetime IS the
/// critical section.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) struct TestLock(MutexGuard<'static, ()>);

#[cfg(test)]
#[allow(clippy::unused_self)]
pub(crate) fn termination_test_lock() -> TestLock {
    TestLock(
        TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

#[cfg(test)]
fn maintenance_interval() -> Duration {
    match TEST_MAINTENANCE_INTERVAL_MS.load(Ordering::SeqCst) {
        0 => Duration::from_secs(30),
        ms => Duration::from_millis(ms),
    }
}

#[cfg(not(test))]
fn maintenance_interval() -> Duration {
    Duration::from_secs(30)
}

#[cfg(test)]
mod r15_tests {
    use super::*;
    use crate::adapters::codex_discovery::{
        begin_lifetime, census_lifetime_is_live, forget_lifetime,
    };
    use crate::shell_pty::lifecycle::ProcessIdentity;
    #[cfg(target_os = "linux")]
    use crate::shell_pty::lifecycle::process_start_ticks;
    // Every use of process_identity_alive in this module is a Linux-only test
    // (start-time identity is Linux-only); an ungated import is unused on
    // macOS and fails -D warnings (c-usagefu r16 item 1).
    #[cfg(target_os = "linux")]
    use crate::shell_pty::lifecycle::process_identity_alive;
    use std::path::Path;
    use time::OffsetDateTime;

    /// A fresh supervisor: registry-hygiene rules are unit-tested without the
    /// global Node supervisor, its watchers or the time-bounded transfer.
    fn fresh() -> Arc<Supervisor> {
        Supervisor::new()
    }

    fn guard_for(dir: &Path, owner: &str) -> LifetimeGuard {
        begin_lifetime(dir, owner, Some(OffsetDateTime::now_utc()))
    }

    fn entry(dir: &Path, owner: &str, survivors: Vec<ProcessIdentity>) -> PendingEntry {
        PendingEntry {
            guards: vec![guard_for(dir, owner)],
            identity: TerminationIdentity {
                pgid: None,
                survivors,
                observation_unknown: false,
            },
            cwd: Some(crate::adapters::codex_discovery::canonical_path(dir)),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_reused_pid_with_a_different_start_time_is_recognised_as_exited() {
        // c-usagefu r15 item 5 (Ruling F): identity is (pid, start time). A
        // LIVE pid carrying a stale (different) start-time baseline is a
        // reused pid — the original process exited — so the registry closes
        // the interval, while the SAME live pid with its real baseline stays
        // alive.
        let _lock = termination_test_lock();
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exec sleep 60"])
            .spawn()
            .unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let real = process_start_ticks(pid).expect("the live child has a start time");

        // Control: real (pid, start) is alive.
        assert!(process_identity_alive(ProcessIdentity {
            pid,
            start_ticks: Some(real),
        }));
        // The stale-baseline identity is confirmed gone even though the pid
        // is live: the original process exited and the pid was "reused".
        let stale = real.wrapping_add(10_000);
        assert!(!process_identity_alive(ProcessIdentity {
            pid,
            start_ticks: Some(stale),
        }));

        // End-to-end through the registry: one entry per verdict, one cwd each.
        let dir_gone = tempfile::tempdir().unwrap();
        let dir_live = tempfile::tempdir().unwrap();
        let supervisor = fresh();
        supervisor.test_push(entry(
            dir_gone.path(),
            "r15-reuse-gone",
            vec![ProcessIdentity {
                pid,
                start_ticks: Some(stale),
            }],
        ));
        supervisor.test_push(entry(
            dir_live.path(),
            "r15-reuse-live",
            vec![ProcessIdentity {
                pid,
                start_ticks: Some(real),
            }],
        ));
        supervisor.recheck(None, MAINTENANCE_RECHECK_CAP);
        assert!(
            !census_lifetime_is_live(dir_gone.path(), "r15-reuse-gone"),
            "start-time mismatch closes the interval"
        );
        assert!(
            census_lifetime_is_live(dir_live.path(), "r15-reuse-live"),
            "the matching baseline keeps the interval live"
        );
        forget_lifetime(dir_live.path(), "r15-reuse-live");
        child.kill().unwrap();
        let _ = child.wait();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_live_reused_group_number_never_keeps_an_entry_with_dead_identities() {
        // c-usagefu r16 item 4: surviving membership is decided from the
        // CAPTURED identities, never from a matching group number. After the
        // signalled group exits the kernel reuses its pgid for an unrelated
        // session; the OLD `group_alive ||` arm saw that live number and kept
        // the interval alive forever (blocking same-cwd discovery) even though
        // every captured process was already gone. This drives the production
        // `Some(pgid)` shape: the child is its OWN group leader (pgid == pid)
        // and genuinely live, while the captured identity carries a STALE
        // start-time baseline — a reused pid whose original process exited.
        use std::os::unix::process::CommandExt as _;

        let _lock = termination_test_lock();
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exec sleep 60"])
            // setpgid(0,0): the child leads a live group whose NUMBER is its pid.
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = i32::try_from(child.id()).unwrap();
        let real = process_start_ticks(pgid).expect("the live group leader has a start time");
        let stale = real.wrapping_add(10_000);

        assert!(
            crate::shell_pty::lifecycle::group_has_live_member(pgid),
            "the production Some(pgid) names a genuinely LIVE group"
        );

        // The live group number must NOT override the identity decision: a
        // start-time mismatch means the captured process exited even though the
        // number is live again; the matching baseline stays alive.
        assert!(
            !termination_identity_alive(
                Some(pgid),
                &[ProcessIdentity {
                    pid: pgid,
                    start_ticks: Some(stale),
                }],
                false,
            ),
            "a live reused group number never keeps an entry with a dead identity"
        );
        assert!(
            termination_identity_alive(
                Some(pgid),
                &[ProcessIdentity {
                    pid: pgid,
                    start_ticks: Some(real),
                }],
                false,
            ),
            "the matching captured identity stays alive despite the group-number rule"
        );

        // End-to-end through the registry with the production Some(pgid):
        // stale identity + live group -> resolved (interval closes); real
        // baseline + live group -> kept.
        let dir_gone = tempfile::tempdir().unwrap();
        let dir_live = tempfile::tempdir().unwrap();
        let supervisor = fresh();
        let make_entry = |dir: &Path, owner: &'static str, ticks: u64| PendingEntry {
            guards: vec![guard_for(dir, owner)],
            identity: TerminationIdentity {
                pgid: Some(pgid),
                survivors: vec![ProcessIdentity {
                    pid: pgid,
                    start_ticks: Some(ticks),
                }],
                observation_unknown: false,
            },
            cwd: Some(crate::adapters::codex_discovery::canonical_path(dir)),
        };
        supervisor.test_push(make_entry(dir_gone.path(), "r16-pgid-gone", stale));
        supervisor.test_push(make_entry(dir_live.path(), "r16-pgid-live", real));
        supervisor.recheck(None, MAINTENANCE_RECHECK_CAP);
        assert!(
            !census_lifetime_is_live(dir_gone.path(), "r16-pgid-gone"),
            "the stale entry closes on identity even with a live Some(pgid)"
        );
        assert!(
            census_lifetime_is_live(dir_live.path(), "r16-pgid-live"),
            "the matching identity keeps its interval"
        );
        assert!(
            !supervisor.has_pending_for(dir_gone.path()),
            "the stale entry is removed from the registry, not held by the group number"
        );

        forget_lifetime(dir_live.path(), "r16-pgid-live");
        child.kill().unwrap();
        let _ = child.wait();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_unreadable_proc_state_is_unknown_and_keeps_the_entry() {
        // c-usagefu r15 item 5 (Ruling F): with /proc reads failing, even
        // against a LIVE pid the registry must not close — Unknown stays.
        let _lock = termination_test_lock();
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exec sleep 60"])
            .spawn()
            .unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let real = process_start_ticks(pid).expect("live child start time");

        crate::shell_pty::lifecycle::set_test_proc_read_failure(true);
        assert!(
            process_identity_alive(ProcessIdentity {
                pid,
                start_ticks: Some(real),
            }),
            "an unreadable /proc state is Unknown, never death"
        );
        let dir = tempfile::tempdir().unwrap();
        let supervisor = fresh();
        supervisor.test_push(entry(
            dir.path(),
            "r15-unknown",
            vec![ProcessIdentity {
                pid,
                start_ticks: Some(real),
            }],
        ));
        supervisor.recheck(Some(dir.path()), DISCOVERY_RECHECK_CAP);
        assert!(
            census_lifetime_is_live(dir.path(), "r15-unknown"),
            "the Unknown survivor keeps the census interval live"
        );
        assert!(
            supervisor.has_pending_for(dir.path()),
            "the entry stays in the registry for a later recheck"
        );
        crate::shell_pty::lifecycle::set_test_proc_read_failure(false);
        // With /proc readable again the real (pid, start) is still live.
        assert!(
            supervisor.pending_count() == 1,
            "the entry was not dropped by the failed read"
        );
        forget_lifetime(dir.path(), "r15-unknown");
        child.kill().unwrap();
        let _ = child.wait();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_dead_entry_behind_alive_ones_is_reached_by_rotation_and_the_registry_is_capped() {
        // c-usagefu r15 item 5: (a) a bounded pass that probes only the first
        // N alive entries must not hide a dead entry behind them — FIFO
        // rotation reaches it within two passes; (b) the registry caps at
        // MAX_PENDING_ENTRIES and evicts oldest-first WITHOUT closing an
        // interval.
        let _lock = termination_test_lock();
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exec sleep 60"])
            .spawn()
            .unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let real = process_start_ticks(pid).expect("live child start time");

        let root = tempfile::tempdir().unwrap();
        let supervisor = fresh();
        let mut live_dirs = Vec::new();
        // 39 live entries (real baseline), then ONE dead entry with a stale
        // baseline pushed LAST.
        for i in 0..39 {
            let dir = root.path().join(format!("live-{i:02}"));
            std::fs::create_dir_all(&dir).unwrap();
            let owner = Box::leak(format!("r15-rot-live-{i:02}").into_boxed_str());
            supervisor.test_push(entry(
                &dir,
                owner,
                vec![ProcessIdentity {
                    pid,
                    start_ticks: Some(real),
                }],
            ));
            live_dirs.push((dir, owner));
        }
        let dead_dir = root.path().join("dead");
        std::fs::create_dir_all(&dead_dir).unwrap();
        supervisor.test_push(entry(
            &dead_dir,
            "r15-rot-dead",
            vec![ProcessIdentity {
                pid,
                start_ticks: Some(real.wrapping_add(10_000)),
            }],
        ));
        assert_eq!(supervisor.pending_count(), 40);

        // First bounded pass (cap 32): probes 32 live entries and rotates
        // them; the dead entry is not reached yet.
        supervisor.recheck(None, 32);
        assert!(
            census_lifetime_is_live(&dead_dir, "r15-rot-dead"),
            "the dead entry behind the alive ones is not closed in pass 1"
        );
        // Second pass rotates into the remaining entries and confirms it.
        supervisor.recheck(None, 32);
        assert!(
            !census_lifetime_is_live(&dead_dir, "r15-rot-dead"),
            "rotation reaches the hidden dead entry in pass 2"
        );
        for (dir, owner) in &live_dirs {
            assert!(
                census_lifetime_is_live(dir, owner),
                "every rotated-alive entry keeps its interval"
            );
            forget_lifetime(dir, owner);
        }

        // Cap: push past MAX_PENDING_ENTRIES; the oldest entries are evicted
        // CONSERVATIVELY (their intervals stay live), the registry is capped.
        let cap_root = tempfile::tempdir().unwrap();
        let supervisor2 = fresh();
        let mut evicted_dirs = Vec::new();
        for i in 0..(MAX_PENDING_ENTRIES + 4) {
            let dir = cap_root.path().join(format!("e-{i:02}"));
            std::fs::create_dir_all(&dir).unwrap();
            let owner = Box::leak(format!("r15-cap-{i:02}").into_boxed_str());
            supervisor2.test_push(entry(
                &dir,
                owner,
                vec![ProcessIdentity {
                    pid,
                    start_ticks: Some(real),
                }],
            ));
            if i < 4 {
                evicted_dirs.push((dir, owner));
            }
        }
        assert_eq!(supervisor2.pending_count(), MAX_PENDING_ENTRIES);
        for (dir, owner) in &evicted_dirs {
            assert!(
                census_lifetime_is_live(dir, owner),
                "oldest-first eviction never closes an interval"
            );
            forget_lifetime(dir, owner);
        }
        // The retained newest entries are still pending; clean their census.
        for i in 4..(MAX_PENDING_ENTRIES + 4) {
            let dir = cap_root.path().join(format!("e-{i:02}"));
            let owner = format!("r15-cap-{i:02}");
            forget_lifetime(&dir, &owner);
        }

        child.kill().unwrap();
        let _ = child.wait();
    }

    /// Non-Linux/unix guard: the hygiene rules themselves are platform
    /// independent even when only a pid-only identity exists.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn the_registry_caps_entries_conservatively_off_linux() {
        let _lock = termination_test_lock();
        let root = tempfile::tempdir().unwrap();
        let supervisor = fresh();
        for i in 0..(MAX_PENDING_ENTRIES + 2) {
            let dir = root.path().join(format!("e-{i:02}"));
            std::fs::create_dir_all(&dir).unwrap();
            let owner = Box::leak(format!("r15-cap-{i:02}").into_boxed_str());
            supervisor.test_push(entry(
                &dir,
                owner,
                vec![crate::shell_pty::lifecycle::survivor_identity(999_999)],
            ));
        }
        assert_eq!(supervisor.pending_count(), MAX_PENDING_ENTRIES);
    }
}
