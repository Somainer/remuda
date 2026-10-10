//! Process-wide registry of open codex launch-discovery windows.
//!
//! c-usagefu r4 item 3: two generic-pty codex instances launched in the same
//! cwd within seconds share one real `$CODEX_HOME`. When instance A is
//! prompted first, only A's rollout exists for a while; instance B's poll
//! would see exactly one same-cwd post-launch match and bind A's thread —
//! journaling A's transcript, usage and cost under B. Timestamps cannot tell
//! the two apart (the files genuinely start at nearly the same instant), so
//! the driver keeps a process-wide registry of open discovery windows keyed
//! by canonical cwd: while two windows overlap, discovery is Ambiguous for
//! BOTH (fail closed), regardless of what files happen to exist. The
//! registry covers every generic-pty launch in this Node process; it cannot
//! (and does not pretend to) coordinate separate Node processes — those
//! land on different operator homes or hosts in the deployed topology.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

#[derive(Default)]
struct WindowEntry {
    /// Live adapters currently discovering in this cwd.
    count: u32,
    /// Set once a second window opened while this one was active; makes the
    /// ambiguity visible to every still-unbound adapter for this cwd.
    tainted: bool,
}

static OPEN_WINDOWS: LazyLock<Mutex<HashMap<PathBuf, WindowEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// RAII handle for one open launch-discovery window. Dropping it releases the
/// process's slot for the cwd when this was the last window.
pub(crate) struct DiscoveryWindow {
    key: PathBuf,
}

impl DiscoveryWindow {
    /// Open (or join) the window for a launch cwd. The returned guard reports
    /// [`Self::tainted`] live: the first opener starts clean and becomes
    /// tainted the moment another window overlaps.
    pub(crate) fn open(cwd: &Path) -> Self {
        let key = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
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
        Self { key }
    }

    /// Whether another launch window overlaps this cwd while the adapter is
    /// still unbound — the cross-instance same-cwd ambiguity signal.
    pub(crate) fn tainted(&self) -> bool {
        let map = OPEN_WINDOWS.lock().expect("codex discovery windows");
        map.get(&self.key).is_some_and(|entry| entry.tainted)
    }
}

impl Drop for DiscoveryWindow {
    fn drop(&mut self) {
        let mut map = OPEN_WINDOWS.lock().expect("codex discovery windows");
        if let Some(entry) = map.get_mut(&self.key) {
            entry.count = entry.count.saturating_sub(1);
            if entry.count == 0 {
                map.remove(&self.key);
            } else {
                // The overlapping partner is gone: the survivor may trust the
                // file-level locator again (unique match binds; two files are
                // still Ambiguous there).
                entry.tainted = false;
            }
        }
    }
}
