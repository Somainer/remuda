//! Advisory locks serialising merge/queue runs.
//!
//! A run holds an exclusive `flock(2)` on
//! `<git-common-dir>/remuda/merge.lock` and, for an in-process gate, one on
//! `<target-dir>/.remuda-merge.lock` for its whole duration. A second run on
//! the same repository (or reusing the same target directory) exits 3 and
//! names the holder unless `--wait` is given. The queue parent holds only
//! the repository lock: lane children are marked with [`QUEUE_WORKER`] and
//! each locks its own per-lane target directory (a second flock from a
//! related process would otherwise deadlock against the parent).

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};

/// Env marker on children spawned by `--queue`: the parent already holds the
/// repository lock for the whole run, so lanes (and `--land` helpers) skip
/// it and lock only their target directory.
pub(super) const QUEUE_WORKER: &str = "REMUDA_MERGE_QUEUE_WORKER";

#[derive(Debug, Clone, Copy)]
pub(super) enum LockScope {
    Repository,
    TargetDirectory,
}

impl LockScope {
    fn label(self) -> &'static str {
        match self {
            LockScope::Repository => "on this repo",
            LockScope::TargetDirectory => "on this target directory",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct LockInfo {
    pub(super) scope: LockScope,
    pub(super) pid: u32,
    /// RFC3339 UTC timestamp the holder recorded.
    pub(super) since: String,
}

/// Drop releases all locks (and closes the files) automatically.
pub(super) struct MergeLocks {
    _files: Vec<Flock<File>>,
}

impl MergeLocks {
    /// No locks (dry-run, --list).
    pub(super) fn empty() -> Self {
        MergeLocks { _files: Vec::new() }
    }

    /// Acquire locks for a single (non-queue) merge invocation: always the
    /// repository lock, plus the target-directory lock when the gate runs
    /// in-process.
    pub(super) fn acquire(repo: &Path, target_dir: Option<&Path>, wait: bool) -> Result<Self> {
        let mut files = Vec::new();
        if std::env::var_os(QUEUE_WORKER).is_none() {
            let common = super::reports::common_dir(repo)?;
            files.push(acquire_one(
                common.join("remuda/merge.lock"),
                LockScope::Repository,
                wait,
            )?);
        }
        if let Some(target) = target_dir {
            let dir = if target.is_absolute() {
                target.to_path_buf()
            } else {
                repo.join(target)
            };
            fs::create_dir_all(&dir)
                .with_context(|| format!("create target directory {}", dir.display()))?;
            files.push(acquire_one(
                dir.join(".remuda-merge.lock"),
                LockScope::TargetDirectory,
                wait,
            )?);
        }
        Ok(MergeLocks { _files: files })
    }

    /// Lock only the repository lock — used by the queue parent, whose lane
    /// children lock their own per-lane target directories.
    pub(super) fn acquire_repo_lock(repo: &Path, wait: bool) -> Result<Self> {
        let common = super::reports::common_dir(repo)?;
        let file = acquire_one(
            common.join("remuda/merge.lock"),
            LockScope::Repository,
            wait,
        )?;
        Ok(MergeLocks { _files: vec![file] })
    }
}

fn acquire_one(path: PathBuf, scope: LockScope, wait: bool) -> Result<Flock<File>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create lock directory {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open {} lock {}", scope.label(), path.display()))?;
    let mut locked = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(locked) => locked,
        Err((file, Errno::EWOULDBLOCK)) => {
            if !wait {
                let (pid, since) = read_holder(&path).unwrap_or((0, "unknown".into()));
                return Err(Contended(LockInfo { scope, pid, since }).into());
            }
            if let Some((pid, since)) = read_holder(&path) {
                eprintln!(
                    "waiting for another remuda merge {} (pid {}, since {})",
                    scope.label(),
                    pid,
                    since
                );
            }
            match Flock::lock(file, FlockArg::LockExclusive) {
                Ok(locked) => locked,
                Err((_, error)) => {
                    return Err(error).with_context(|| format!("wait for lock {}", path.display()));
                }
            }
        }
        Err((_, error)) => {
            return Err(error)
                .with_context(|| format!("acquire {} lock {}", scope.label(), path.display()));
        }
    };
    // We hold it: stamp our pid and start time so a contender can name us.
    let payload = format!("pid={}\nsince={}\n", std::process::id(), rfc3339_now());
    locked.set_len(0).ok();
    locked.write_all(payload.as_bytes()).ok();
    locked.sync_all().ok();
    Ok(locked)
}

/// Contention is reported as exit code 3; the message names the holder.
#[derive(Debug)]
pub(super) struct Contended(pub(super) LockInfo);

impl std::fmt::Display for Contended {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let info = &self.0;
        if info.pid == 0 {
            write!(
                formatter,
                "another remuda merge is running {}",
                info.scope.label()
            )
        } else {
            write!(
                formatter,
                "another remuda merge is running {} (pid {}, since {})",
                info.scope.label(),
                info.pid,
                info.since
            )
        }
    }
}

impl std::error::Error for Contended {}

fn read_holder(path: &Path) -> Option<(u32, String)> {
    let contents = fs::read_to_string(path).ok()?;
    let mut pid = 0u32;
    let mut since = String::new();
    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("pid=") {
            pid = value.trim().parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("since=") {
            since = value.trim().to_owned();
        }
    }
    (pid != 0).then_some((pid, since))
}

fn rfc3339_now() -> String {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    OffsetDateTime::from_unix_timestamp(secs)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .format(&Rfc3339)
        .unwrap_or_else(|_| secs.to_string())
}
