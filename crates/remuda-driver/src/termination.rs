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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
#[cfg(test)]
use std::sync::{MutexGuard, atomic::AtomicU64};
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::adapters::codex_discovery::LifetimeGuard;
use crate::shell_pty::lifecycle::{
    TERMINATION_WATCH_POLL, termination_identity_alive, termination_watch_bound,
};

/// Process identity a pending termination probes: the signalled process GROUP
/// (which may already be gone while an escaped descendant survives) plus the
/// explicitly named survivor pids.
#[derive(Clone)]
pub(crate) struct TerminationIdentity {
    pub(crate) pgid: Option<i32>,
    pub(crate) survivors: Vec<i32>,
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

/// Maximum entries one discovery-triggered recheck probes. The maintenance
/// sweep has no cap; discovery stays a cheap, bounded operation.
const DISCOVERY_RECHECK_CAP: usize = 16;

struct Supervisor {
    /// Survivors that outlived their per-close bound.
    registry: StdMutex<Vec<PendingEntry>>,
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
            registry: StdMutex::new(Vec::new()),
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
                if !termination_identity_alive(entry.identity.pgid, &entry.identity.survivors) {
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
            if !termination_identity_alive(entry.identity.pgid, &entry.identity.survivors) {
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
                registry.push(entry);
                supervisor.pending.store(registry.len(), Ordering::SeqCst);
            }
            supervisor.ensure_maintenance();
        });
        self.watchers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(task);
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
                supervisor.recheck(None, usize::MAX);
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
    /// sweep. At most `cap` entries are probed per call — bounded and cheap on
    /// the discovery path.
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
            let mut probed = 0;
            registry.retain_mut(|entry| {
                if want_cwd
                    .as_ref()
                    .is_some_and(|want| entry.cwd.as_ref() != Some(want))
                {
                    return true;
                }
                if probed >= cap {
                    return true;
                }
                probed += 1;
                if termination_identity_alive(entry.identity.pgid, &entry.identity.survivors) {
                    return true;
                }
                // Confirmed gone: take the guards out and drop the entry. The
                // close is recorded AFTER releasing the registry lock.
                let taken = std::mem::replace(
                    entry,
                    PendingEntry {
                        guards: Vec::new(),
                        identity: TerminationIdentity {
                            pgid: None,
                            survivors: Vec::new(),
                        },
                        cwd: None,
                    },
                );
                confirmed.push(taken);
                false
            });
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
    #[cfg(test)]
    fn has_pending_for(&self, cwd: &Path) -> bool {
        let want = crate::adapters::codex_discovery::canonical_path(cwd);
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|entry| entry.cwd.as_ref() == Some(&want))
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

#[cfg(test)]
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
