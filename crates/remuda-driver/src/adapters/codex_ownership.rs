//! Process-tree ownership of a codex rollout file (c-usagefu r8 item 1, tier b).
//!
//! Prompt text can never prove WHICH running instance created a rollout when
//! more than one agent has been live in a cwd. The strong non-hook signal is
//! the operating system: the rollout JSONL is created and held open by the
//! Codex child process belonging to THIS instance's pane. [`RolloutOwnership`]
//! answers "is this path currently open by `root` or one of its descendants?"
//! behind a trait so tests inject a deterministic probe instead of poking
//! real `/proc` entries.

use std::collections::{HashMap, HashSet};
#[cfg(not(test))]
use std::path::Path;
#[cfg(test)]
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Probe that decides file ownership from live process state.
pub trait RolloutOwnership: Send + Sync {
    /// True when `path` is currently open (a file descriptor resolves to it)
    /// by the process `root` itself or a descendant of `root`.
    fn file_held_in_tree(&self, path: &Path, root: u32) -> bool;
}

/// Test/driver-injected probe: maps a canonical path to the set of pids that
/// currently hold it open. The caller decides pid-tree membership separately
/// where needed; this fake treats any pid in `holders(path)` as in-tree only
/// when it equals the supplied root (tests pass the exact agent pid).
#[cfg(test)]
pub(crate) struct FakeOwnership {
    holders: Arc<std::sync::Mutex<HashMap<PathBuf, HashSet<u32>>>>,
}

#[cfg(test)]
impl FakeOwnership {
    pub(crate) fn new() -> Self {
        Self {
            holders: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn hold(&self, path: &Path, pid: u32) {
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.holders
            .lock()
            .expect("fake owners")
            .entry(key)
            .or_default()
            .insert(pid);
    }
}

#[cfg(test)]
impl RolloutOwnership for FakeOwnership {
    fn file_held_in_tree(&self, path: &Path, root: u32) -> bool {
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.holders
            .lock()
            .expect("fake owners")
            .get(&key)
            .is_some_and(|pids| pids.contains(&root))
    }
}

/// Linux `/proc` implementation: scans every process's fd directory and
/// resolves open-file symlinks to `path`, accepting a pid only when it is
/// `root` or a descendant (matched via the ppid chain).
#[cfg(target_os = "linux")]
pub(crate) struct ProcFdOwnership;

#[cfg(target_os = "linux")]
impl ProcFdOwnership {
    pub(crate) fn new() -> Self {
        Self
    }

    /// Set of `root` and all its descendant pids, built once per query from
    /// `/proc/<pid>/stat` (field 4 = ppid).
    fn tree(root: u32) -> HashSet<u32> {
        let mut parents: HashMap<u32, u32> = HashMap::new();
        if let Ok(entries) = std::fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|s| s.parse::<u32>().ok())
                else {
                    continue;
                };
                if let Ok(stat) = std::fs::read_to_string(entry.path().join("stat"))
                    && let Some(ppid) = parse_ppid(&stat)
                {
                    parents.insert(pid, ppid);
                }
            }
        }
        let mut tree = HashSet::new();
        tree.insert(root);
        let mut frontier = vec![root];
        while let Some(pid) = frontier.pop() {
            for (child, parent) in &parents {
                if *parent == pid && tree.insert(*child) {
                    frontier.push(*child);
                }
            }
        }
        tree
    }
}

#[cfg(target_os = "linux")]
fn parse_ppid(stat: &str) -> Option<u32> {
    // comm (field 2) is wrapped in parens and may contain spaces/parens: find
    // the LAST ')' then parse whitespace fields; field 4 (ppid) is the 2nd
    // token after ')'.
    let close = stat.rfind(')')?;
    let rest = stat[close + 1..].split_whitespace();
    let mut tokens = rest.skip(1); // field 3: state
    tokens.next()?.parse::<u32>().ok()
}

#[cfg(target_os = "linux")]
impl RolloutOwnership for ProcFdOwnership {
    fn file_held_in_tree(&self, path: &Path, root: u32) -> bool {
        let Ok(target) = std::fs::canonicalize(path) else {
            return false;
        };
        let tree = Self::tree(root);
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return false;
        };
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            if !tree.contains(&pid) {
                continue;
            }
            let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
                continue;
            };
            for fd in fds.flatten() {
                if let Ok(link) = std::fs::read_link(fd.path())
                    && link == target
                {
                    return true;
                }
            }
        }
        false
    }
}

/// Construct the platform ownership probe (None on unsupported hosts).
pub(crate) fn platform_probe() -> Option<Arc<dyn RolloutOwnership>> {
    #[cfg(target_os = "linux")]
    {
        Some(Arc::new(ProcFdOwnership::new()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}
