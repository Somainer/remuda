//! Process-wide registry for codex generic-pty discovery.
//!
//! Two mechanisms close the cross-instance same-cwd binding holes:
//!
//! - **Launch windows** ([`DiscoveryWindow`]) count the adapters in this Node
//!   process that are still discovering in one canonical cwd. While two
//!   windows overlap, a file matched by launch-time alone could belong to
//!   either adapter, so discovery waits for stronger evidence (c-usagefu r5
//!   item 1). A window is released as soon as its adapter BINDS or gives up,
//!   not only when the instance closes: an instance that bound at 10:01 must
//!   not taint a second launch in the same cwd at 11:00.
//! - **Claimed rollouts** ([`claim`]): once an adapter binds a rollout file,
//!   no other adapter in this process may bind it — the file-level locator
//!   excludes claimed paths. This closes the "A binds first" race: B's scan
//!   sees A's rollout as already owned and waits for B's own file. Claims are
//!   released when the owning adapter drops (instance close).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

#[derive(Default)]
struct WindowEntry {
    /// Live adapters currently discovering in this cwd.
    count: u32,
    /// True while another window overlaps (count >= 2).
    tainted: bool,
}

static OPEN_WINDOWS: LazyLock<Mutex<HashMap<PathBuf, WindowEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static CLAIMED_ROLLOUTS: LazyLock<Mutex<HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Canonical key for one rollout path / cwd (best effort: a non-existent or
/// unreadable path keeps its lexical form).
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Whether a rollout file is already claimed by another live adapter.
pub(crate) fn is_claimed(path: &Path) -> bool {
    let map = CLAIMED_ROLLOUTS.lock().expect("claimed codex rollouts");
    map.contains(&canonical(path))
}

/// Claim a rollout file for this adapter. Returns false when another adapter
/// claimed it first (a poll race — the caller must treat the candidate as
/// someone else's and keep discovering).
pub(crate) fn claim(path: &Path) -> bool {
    CLAIMED_ROLLOUTS
        .lock()
        .expect("claimed codex rollouts")
        .insert(canonical(path))
}

/// Release a claim (adapter drop). A double release is a no-op.
pub(crate) fn release_claim(path: &Path) {
    CLAIMED_ROLLOUTS
        .lock()
        .expect("claimed codex rollouts")
        .remove(&canonical(path));
}

/// RAII window for one launch still discovering in a cwd. Explicit
/// [`Self::release`] on bind/give-up drops the refcount slot early; the Drop
/// impl is the safety net for an adapter that closes unbound.
pub(crate) struct DiscoveryWindow {
    key: PathBuf,
    released: bool,
}

impl DiscoveryWindow {
    /// Open (or join) the discovery window for a launch cwd.
    pub(crate) fn open(cwd: &Path) -> Self {
        let key = canonical(cwd);
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
        }
        Self {
            key,
            released: false,
        }
    }

    /// Whether another discovery window currently overlaps this cwd (two or
    /// more unbound/just-launched adapters). Read live every tick.
    pub(crate) fn overlapping(&self) -> bool {
        let map = OPEN_WINDOWS.lock().expect("codex discovery windows");
        map.get(&self.key)
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
        if let Some(entry) = map.get_mut(&self.key) {
            entry.count = entry.count.saturating_sub(1);
            if entry.count <= 1 {
                // At most one seeker left: the file-level locator (with the
                // claimed-rollout set) is authoritative again.
                entry.tainted = false;
            }
            if entry.count == 0 {
                map.remove(&self.key);
            }
        }
    }
}

impl Drop for DiscoveryWindow {
    fn drop(&mut self) {
        self.release();
    }
}
