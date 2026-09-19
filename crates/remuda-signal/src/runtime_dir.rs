//! AF_UNIX socket placement that survives a long data directory.
//!
//! `sockaddr_un.sun_path` is tiny — 107 usable bytes on Linux, 103 on macOS —
//! and the limit applies to the *address argument* of both [`bind(2)`] and
//! [`connect(2)`], before any symlink resolution. A socket that lives at
//! `<data dir>/instances/<40-char id>/hook.sock` can therefore be both
//! un-bindable and un-connectable when the data directory sits under a long
//! home path. Shortening the data directory is not an option the program gets
//! to take.
//!
//! The rule this module implements:
//!
//! 1. When the preferred (under-instance / under-data-dir) path is short
//!    enough, use it exactly as before.
//! 2. Otherwise bind a short-named socket in the first usable per-user
//!    runtime directory, and leave a symlink with the preferred name pointing
//!    at the real socket. Candidates, in order:
//!    1. `$XDG_RUNTIME_DIR/remuda/` when `XDG_RUNTIME_DIR` is set;
//!    2. `${TMPDIR:-/tmp}/remuda-<uid>/`;
//!    3. `/tmp/remuda-<uid>/` as a fixed last resort.
//!
//! The third candidate exists because the limit is measured against the
//! *whole* address: on macOS `std::env::temp_dir()` is the ~48-byte
//! `/var/folders/xx/yyyy/T/` with `XDG_RUNTIME_DIR` normally unset, and on any
//! platform a long `TMPDIR` makes candidate 2 overflow even though it is the
//! "short" branch. A per-user 0700 directory holding only socket inodes is the
//! one place a fixed `/tmp` root is correct. A candidate counts as usable only
//! when it (a) passes the security checks and (b) yields a bind path at or
//! under [`SUN_PATH_LIMIT`]; an error is returned only when none fits.
//!
//! The symlink is *discovery*, not a route: connecting through the long link
//! path hits the same `sun_path` rejection (verified empirically), so every
//! wire client must connect to [`SocketPlacement::bind_path`]. Anything that
//! only inspects the instance directory (operators, tooling, purge) still sees
//! `hook.sock` there.
//!
//! [`bind(2)`]: https://man7.org/linux/man-pages/man2/bind.2.html
//! [`connect(2)`]: https://man7.org/linux/man-pages/man2/connect.2.html

use std::io;
use std::path::{Path, PathBuf};

/// Bytes of path at or below which a socket is bound at its preferred path.
///
/// 100 leaves headroom under both the Linux limit (107) and the macOS limit
/// (103): filesystem layout on top of the bare socket name must not be able to
/// push the path over either platform's edge.
pub const SAFE_SOCKET_PATH_BYTES: usize = 100;

/// Largest usable `sockaddr_un.sun_path` on this platform, in bytes.
///
/// The kernel arrays hold one more byte (`sun_path` is 108 on Linux, 104 on
/// macOS/BSD) which the terminating NUL occupies.
#[cfg(target_os = "linux")]
pub const SUN_PATH_LIMIT: usize = 107;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub const SUN_PATH_LIMIT: usize = 103;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios")))]
pub const SUN_PATH_LIMIT: usize = 103;

/// Subdirectory of `$XDG_RUNTIME_DIR` remuda owns.
const XDG_SUBDIR: &str = "remuda";

/// Where one socket must be bound, and the discovery link (if any) left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketPlacement {
    /// The short path passed to `UnixListener::bind` and to every client.
    bind_path: PathBuf,
    /// The preferred long path, holding a symlink to `bind_path` when
    /// redirected. [`None`] when the preferred path is used directly.
    link_path: Option<PathBuf>,
}

impl SocketPlacement {
    /// The path to bind and to hand to every client that connects.
    #[must_use]
    pub fn bind_path(&self) -> &Path {
        &self.bind_path
    }

    /// The under-instance/under-data-dir path that symlinks to
    /// [`bind_path`](Self::bind_path) when the preferred path was too long.
    #[must_use]
    pub fn link_path(&self) -> Option<&Path> {
        self.link_path.as_deref()
    }

    /// Whether placement redirected the socket into the per-user runtime dir.
    #[must_use]
    pub fn redirected(&self) -> bool {
        self.link_path.is_some()
    }

    /// Create the discovery symlink after the socket has been bound.
    ///
    /// Called only on a redirected placement. A pre-existing entry at the link
    /// path is tolerated when it is a dangling/live symlink or a stale socket
    /// (a socket from an earlier release that bound this path directly); any
    /// other file type is refused rather than displaced.
    pub fn install_link(&self) -> io::Result<()> {
        let Some((link, target)) = self
            .link_path
            .as_ref()
            .map(|path| (path.as_path(), self.bind_path.as_path()))
        else {
            return Ok(());
        };
        install_socket_symlink(link, target)
    }

    /// Remove the discovery symlink. The bound socket is unlinked by its own
    /// server's drop; this touches the link only.
    ///
    /// Deliberately an explicit call rather than a `Drop`: placement is a
    /// plain description handed to different owners (a hook session, a daemon
    /// socket guard), and dropping an intermediate copy must never silently
    /// unlink a socket another owner still serves.
    pub fn remove_link(&self) {
        if let Some(link) = &self.link_path {
            let _ = std::fs::remove_file(link);
        }
    }
}

/// Choose where a socket should live, without binding it.
///
/// `preferred` is the conventional, discoverable path (under the data or
/// instance directory). `runtime_name` is the plain file name to use inside
/// the per-user runtime directory when `preferred` is too long; it must itself
/// contain no path separators.
///
/// This only computes the placement and, on redirection, prepares the runtime
/// directory (0700, this uid, never a symlink). The caller binds
/// [`SocketPlacement::bind_path`] and then calls
/// [`SocketPlacement::install_link`]; creating the link only after a
/// successful bind means it never points at a socket that failed to come up.
pub fn place_socket(preferred: &Path, runtime_name: &str) -> io::Result<SocketPlacement> {
    debug_assert!(
        Path::new(runtime_name)
            .parent()
            .is_none_or(|parent| parent.as_os_str().is_empty()),
        "runtime_name must be a plain file name, got {runtime_name:?}"
    );
    if path_byte_len(preferred) <= SAFE_SOCKET_PATH_BYTES {
        return Ok(SocketPlacement {
            bind_path: preferred.to_path_buf(),
            link_path: None,
        });
    }
    let uid = current_uid();
    let mut attempts = Vec::new();
    for candidate in runtime_dir_candidates(uid) {
        let bind_path = candidate.join(runtime_name);
        let len = path_byte_len(&bind_path);
        if len > SUN_PATH_LIMIT {
            // Too long to ever bind here; do not even create the directory.
            attempts.push(format!("{} ({len} bytes)", bind_path.display()));
            continue;
        }
        match secure_runtime_dir(&candidate, uid) {
            Ok(()) => {
                return Ok(SocketPlacement {
                    bind_path,
                    link_path: Some(preferred.to_path_buf()),
                });
            }
            Err(error) => {
                tracing::warn!(
                    path = %candidate.display(),
                    %error,
                    "runtime dir candidate unusable for remuda sockets"
                );
                attempts.push(format!("{} ({error})", bind_path.display()));
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "no AF_UNIX placement fits for {}: {}; platform sun_path limit is {SUN_PATH_LIMIT}",
            preferred.display(),
            attempts.join("; ")
        ),
    ))
}

/// True when `path` is short enough to bind directly at its preferred name.
#[must_use]
pub fn path_fits_sun_path(path: &Path) -> bool {
    path_byte_len(path) <= SAFE_SOCKET_PATH_BYTES
}

/// Point `link` at `target`, replacing a stale link/socket but nothing else.
#[cfg(unix)]
fn install_socket_symlink(link: &Path, target: &Path) -> io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, symlink};

    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::symlink_metadata(link) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                // A previous run's link. Keep it only if it already names the
                // right socket; otherwise the old placement is stale.
                if std::fs::read_link(link)? == target {
                    return Ok(());
                }
                std::fs::remove_file(link)?;
            } else if file_type.is_socket() {
                // Leftover from a release/configuration that bound this path
                // directly. A live listener would make the bind below fail
                // loudly anyway; a dead socket is safe to replace.
                std::fs::remove_file(link)?;
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} exists and is neither a socket nor a symlink",
                        link.display()
                    ),
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    symlink(target, link)?;
    Ok(())
}

#[cfg(not(unix))]
fn install_socket_symlink(_link: &Path, _target: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "AF_UNIX sockets are only supported on unix",
    ))
}

/// Per-user runtime directory candidates in preference order.
///
/// Pure: it reads the environment but neither creates nor validates anything.
/// The last element is the fixed `/tmp/remuda-<uid>` last resort, which is
/// omitted when it is identical to the `TMPDIR` fallback (i.e. TMPDIR is unset
/// or is itself `/tmp`).
#[must_use]
pub fn runtime_dir_candidates(uid: u32) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        candidates.push(xdg.join(XDG_SUBDIR));
    }
    let temp_fallback = std::env::temp_dir().join(format!("remuda-{uid}"));
    candidates.push(temp_fallback.clone());
    // Last resort: a fixed short root. It is the only placement that can stay
    // under a 103-byte macOS sun_path when TMPDIR is the ~48-byte per-user
    // Darwin temp dir (or any other long TMPDIR).
    let fixed = PathBuf::from("/tmp").join(format!("remuda-{uid}"));
    if fixed != temp_fallback {
        candidates.push(fixed);
    }
    candidates
}

/// Locate (creating if needed) the first usable 0700 per-user remuda runtime
/// directory, regardless of the socket name it will hold.
///
/// Callers that know the socket file name should use [`place_socket`]
/// instead: it additionally rejects candidates whose join with the name would
/// exceed `sun_path` and moves on to the next (fixed) root.
pub fn per_user_runtime_dir() -> io::Result<PathBuf> {
    let uid = current_uid();
    let mut last_error = None;
    for candidate in runtime_dir_candidates(uid) {
        match secure_runtime_dir(&candidate, uid) {
            Ok(()) => return Ok(candidate),
            Err(error) => {
                tracing::warn!(
                    path = %candidate.display(),
                    %error,
                    "runtime dir candidate unusable for remuda sockets"
                );
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no per-user runtime directory candidate was available",
        )
    }))
}

/// Verify `dir` is a real, own directory and make it 0700.
///
/// Order matters: the symlink and owner checks run *before* any chmod, so a
/// path an attacker planted (a symlink into their tree, or a directory they
/// own) is never written to.
///
/// A directory we create is born `0700` (recursive `DirBuilder` mode), so it
/// is never momentarily group/world accessible. A pre-existing directory keeps
/// whatever mode it had through the checks and is tightened afterwards through
/// an `O_NOFOLLOW|O_DIRECTORY` fd with `fchmod`, which can never follow a link
/// swapped in after the lstat.
fn secure_runtime_dir(dir: &Path, uid: u32) -> io::Result<()> {
    let (metadata, pre_existing) = match std::fs::symlink_metadata(dir) {
        Ok(metadata) => (metadata, true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_private_dir_all(dir)?;
            (std::fs::symlink_metadata(dir)?, false)
        }
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "runtime dir {} is a symlink; refusing an untrusted path",
                    dir.display()
                ),
            ));
        }
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a directory", dir.display()),
            ));
        }
        if metadata.uid() != uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "runtime dir {} is owned by uid {} but this process is uid {}",
                    dir.display(),
                    metadata.uid(),
                    uid
                ),
            ));
        }
        if pre_existing {
            tighten_to_0700_nofollow(dir)?;
        }
    }
    Ok(())
}

/// Recursively create `dir` with every new component born 0700.
#[cfg(unix)]
fn create_private_dir_all(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir_all(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// `fchmod(0700)` on a directory opened with `O_NOFOLLOW|O_DIRECTORY`.
///
/// `O_NOFOLLOW` fails with `ELOOP` if the path became a symlink after the
/// lstat; `O_DIRECTORY` fails if it stopped being a directory. Neither window
/// can therefore reach a file an attacker controls.
#[cfg(unix)]
fn tighten_to_0700_nofollow(dir: &Path) -> io::Result<()> {
    use nix::fcntl::OFlag;
    use nix::sys::stat::{Mode, fchmod};
    use std::os::fd::AsRawFd;
    let fd = nix::fcntl::open(
        dir,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?;
    let result = fchmod(fd.as_raw_fd(), Mode::from_bits_truncate(0o700));
    result?;
    Ok(())
}

#[cfg(not(unix))]
fn tighten_to_0700_nofollow(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Remove dead socket inodes left in the runtime directories by crashed Nodes.
///
/// Every directory here is remuda-private (0700, this uid), so any `*.sock`
/// socket inode is one of ours. A socket with a live listener answers a
/// connection (it is left alone); a `ECONNREFUSED` means no process holds it
/// — e.g. after a SIGKILL — and it is unlinked. Returns the number removed.
#[cfg(unix)]
pub fn sweep_dead_runtime_sockets() -> io::Result<usize> {
    use std::os::unix::fs::FileTypeExt;
    let uid = current_uid();
    let mut removed = 0;
    for dir in runtime_dir_candidates(uid) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.file_type().is_socket()
                || path.extension().and_then(|ext| ext.to_str()) != Some("sock")
            {
                continue;
            }
            match std::os::unix::net::UnixStream::connect(&path) {
                // A live listener accepted the probe; never touch it.
                Ok(_stream) => {}
                // Dead inode (SIGKILL, power loss): reclaim the name. The
                // kernel reports ECONNREFUSED for a socket nobody listens on;
                // std maps that to the portable `ConnectionRefused`.
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    match std::fs::remove_file(&path) {
                        Ok(()) => removed += 1,
                        Err(error) => tracing::debug!(
                            path = %path.display(),
                            %error,
                            "could not sweep dead runtime socket"
                        ),
                    }
                }
                Err(error) => tracing::debug!(
                    path = %path.display(),
                    %error,
                    "skipping runtime socket during sweep"
                ),
            }
        }
    }
    Ok(removed)
}

/// Unlink the real socket an instance's `hook.sock` symlink points at.
///
/// Called when an instance is purged. Only the symlink itself sits in the
/// (about-to-be-removed) instance directory; the live socket inode lives in a
/// per-user runtime dir and would otherwise leak. The target is unlinked only
/// when its parent is one of this process's own runtime candidates, so a link
/// an attacker crafted at an arbitrary path can never direct a removal.
///
/// A direct (non-symlink) socket is left to the directory removal that
/// follows; a missing file is not an error.
#[cfg(unix)]
pub fn unlink_resolved_socket_link(link: &Path) -> io::Result<()> {
    let metadata = match std::fs::symlink_metadata(link) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_symlink() {
        return Ok(());
    }
    let target = std::fs::read_link(link)?;
    let uid = current_uid();
    let trusted = runtime_dir_candidates(uid)
        .iter()
        .any(|candidate| target.parent() == Some(candidate.as_path()));
    if !trusted {
        tracing::warn!(
            link = %link.display(),
            target = %target.display(),
            "hook.sock symlink resolves outside remuda runtime dirs; leaving target in place"
        );
        return Ok(());
    }
    match std::fs::remove_file(&target) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

#[cfg(unix)]
fn current_uid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

fn path_byte_len(path: &Path) -> usize {
    path.as_os_str().len()
}

/// Enrich a failed `UnixListener::bind` with the facts an operator needs:
/// the offending path, its byte length, and this platform's `sun_path` limit.
pub fn bind_io_error(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!(
            "AF_UNIX bind failed at {} ({} bytes; sun_path limit {SUN_PATH_LIMIT} on {}): {error}",
            path.display(),
            path_byte_len(path),
            std::env::consts::OS
        ),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn tmp_root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_short_preferred_path_is_used_directly() {
        let dir = tmp_root();
        let preferred = dir.path().join("hook.sock");
        let placement = place_socket(&preferred, "x.sock").unwrap();
        assert_eq!(placement.bind_path(), preferred);
        assert_eq!(placement.link_path(), None);
        assert!(!placement.redirected());
    }

    #[test]
    fn a_long_preferred_path_is_redirected_under_an_explicit_runtime_root() {
        // Exercise the pure placement math without process-global env vars:
        // a long preferred path names a short file under the secure runtime
        // dir the caller prepared.
        let root = tmp_root();
        let long = root
            .path()
            .join("x".repeat(80))
            .join("instances/ins_01990000-0000-7000-8000-000000000000");
        std::fs::create_dir_all(&long).unwrap();
        let preferred = long.join("hook.sock");
        assert!(path_byte_len(&preferred) > SAFE_SOCKET_PATH_BYTES);

        let runtime = root.path().join("runtime");
        secure_runtime_dir(&runtime, current_uid()).unwrap();
        let bind = runtime.join("ins-1.sock");
        let placement = SocketPlacement {
            bind_path: bind.clone(),
            link_path: Some(preferred.clone()),
        };
        // Simulate the bind, then install the discovery link.
        std::fs::write(&bind, b"").unwrap(); // stand-in for the socket inode
        placement.install_link().unwrap();
        let meta = std::fs::symlink_metadata(&preferred).unwrap();
        assert!(meta.file_type().is_symlink());
        assert_eq!(std::fs::read_link(&preferred).unwrap(), bind);
        // Idempotent: re-installing the same placement is a no-op.
        placement.install_link().unwrap();
        assert_eq!(std::fs::read_link(&preferred).unwrap(), bind);
        // Cleanup removes the link but never the real socket.
        placement.remove_link();
        assert!(std::fs::symlink_metadata(&preferred).is_err());
        assert!(bind.exists());
    }

    #[test]
    fn two_instances_get_distinct_runtime_files() {
        let root = tmp_root();
        let runtime = root.path().join("runtime");
        secure_runtime_dir(&runtime, current_uid()).unwrap();
        let first = SocketPlacement {
            bind_path: runtime.join("0199aaaa.sock"),
            link_path: Some(root.path().join("long-a").join("hook.sock")),
        };
        let second = SocketPlacement {
            bind_path: runtime.join("0199bbbb.sock"),
            link_path: Some(root.path().join("long-b").join("hook.sock")),
        };
        assert_ne!(first.bind_path(), second.bind_path());
        std::fs::create_dir_all(root.path().join("long-a")).unwrap();
        std::fs::create_dir_all(root.path().join("long-b")).unwrap();
        std::fs::write(first.bind_path(), b"a").unwrap();
        std::fs::write(second.bind_path(), b"b").unwrap();
        first.install_link().unwrap();
        second.install_link().unwrap();
        assert_ne!(
            std::fs::read_link(first.link_path().unwrap()).unwrap(),
            std::fs::read_link(second.link_path().unwrap()).unwrap()
        );
    }

    #[test]
    fn a_foreign_owned_runtime_dir_is_refused() {
        let root = tmp_root();
        let dir = root.path().join("remuda-999999");
        std::fs::create_dir_all(&dir).unwrap();
        // The directory is owned by this process; claiming a *different*
        // expected uid is exactly the comparison production makes against an
        // attacker-prepared directory in a shared tmp.
        let error = secure_runtime_dir(&dir, current_uid().wrapping_add(1)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("owned by uid"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_runtime_dir_is_refused() {
        let root = tmp_root();
        let target = root.path().join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        let link = root.path().join("remuda-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        // create_dir_all + chmod follow the symlink and make the *target*
        // private; the lstat uid check still sees this uid, so the refusal has
        // to come from an explicit symlink check — which is what we verify
        // here by proving the final path resolves through a link.
        let metadata = std::fs::symlink_metadata(&link).unwrap();
        assert!(metadata.file_type().is_symlink());
        // And production's guarantee: secure_runtime_dir refuses it.
        assert!(secure_runtime_dir(&link, current_uid()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn an_owned_runtime_dir_with_relaxed_mode_is_tightened_to_0700() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let root = tmp_root();
        let dir = root.path().join("remuda-open");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        // Our own directory (e.g. an older release left 0777) is repaired,
        // not refused; a *foreign* 0777 dir is stopped by the uid check above.
        secure_runtime_dir(&dir, current_uid()).unwrap();
        let metadata = std::fs::symlink_metadata(&dir).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o700);
    }

    #[test]
    fn candidates_end_in_the_fixed_tmp_root_and_a_uuid_name_always_fits() {
        let candidates = runtime_dir_candidates(current_uid());
        assert!(!candidates.is_empty());
        // The last candidate is always the fixed `/tmp/remuda-<uid>` root
        // (possibly the same as the temp fallback when TMPDIR is unset/`/tmp`,
        // in which case it is listed exactly once).
        let last = candidates.last().unwrap();
        assert_eq!(last.parent(), Some(Path::new("/tmp")));
        assert!(
            last.file_name()
                .unwrap()
                .to_str()
                .is_some_and(|name| name.starts_with("remuda-"))
        );
        // No duplicates (the fixed root is dropped when TMPDIR is /tmp).
        let mut unique = candidates.clone();
        unique.dedup();
        assert_eq!(unique.len(), candidates.len());
        // The product invariant: the 41-byte `<uuid>.sock` name fits at the
        // fixed root on every supported platform (58 bytes ≤ 103).
        let bound = last.join("01990000-0000-7000-8000-000000000001.sock");
        assert!(path_byte_len(&bound) <= SUN_PATH_LIMIT);
        // A TMPDIR other than /tmp adds a distinct fallback ahead of the
        // fixed root.
        if std::env::temp_dir() != *Path::new("/tmp") {
            assert!(candidates.len() >= 2, "{candidates:?}");
        }
    }

    #[test]
    fn a_newly_created_runtime_dir_is_born_private() {
        let root = tmp_root();
        let dir = root.path().join("a/b/remuda-new");
        secure_runtime_dir(&dir, current_uid()).unwrap();
        use std::os::unix::fs::MetadataExt;
        let parent = std::fs::symlink_metadata(root.path().join("a/b")).unwrap();
        let leaf = std::fs::symlink_metadata(&dir).unwrap();
        // Both created components are 0700, independent of the process umask.
        assert_eq!(parent.mode() & 0o777, 0o700);
        assert_eq!(leaf.mode() & 0o777, 0o700);
        assert_eq!(leaf.uid(), current_uid());
    }

    #[cfg(unix)]
    // Environment-mutating tests live in tests/runtime_dir.rs (a separate
    // crate): this crate forbids unsafe code, and `env::set_var` is unsafe on
    // the toolchain's edition.
    #[test]
    fn bind_error_text_carries_path_length_and_limit() {
        let path = Path::new("/tmp").join("z".repeat(115));
        let error = bind_io_error(
            &path,
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "path must be shorter than SUN_LEN",
            ),
        );
        let text = error.to_string();
        assert!(text.contains(&path.display().to_string()));
        assert!(text.contains(&format!("{} bytes", path_byte_len(&path))));
        assert!(text.contains(&format!("limit {SUN_PATH_LIMIT}")));
        assert!(text.contains("path must be shorter than SUN_LEN"));
    }
}
