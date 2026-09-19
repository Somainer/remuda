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
//! 2. Otherwise bind a short-named socket in a per-user runtime directory
//!    (`$XDG_RUNTIME_DIR/remuda/`, else `${TMPDIR:-/tmp}/remuda-<uid>/`), and
//!    leave a symlink with the preferred name pointing at the real socket.
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
    let bind_path = per_user_runtime_dir()?.join(runtime_name);
    let len = path_byte_len(&bind_path);
    if len > SUN_PATH_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "no AF_UNIX placement fits: runtime socket path is {len} bytes but the \
                 platform limit is {SUN_PATH_LIMIT}; shorten XDG_RUNTIME_DIR/TMPDIR"
            ),
        ));
    }
    Ok(SocketPlacement {
        bind_path,
        link_path: Some(preferred.to_path_buf()),
    })
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

/// Locate (creating if needed) the 0700 per-user remuda runtime directory.
///
/// Preference order, per the XDG base directory spec:
///
/// 1. `$XDG_RUNTIME_DIR/remuda/` when the variable is set and the directory
///    can be made ours (0700, this uid, not through a symlink).
/// 2. `${TMPDIR:-/tmp}/remuda-<uid>/`, with the same guarantees.
///
/// A pre-existing directory that is a symlink or owned by another uid is
/// refused, never repaired: in a shared tmp that is an attacker-controlled
/// path, not something to chmod into trust.
pub fn per_user_runtime_dir() -> io::Result<PathBuf> {
    let uid = current_uid();
    if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        let candidate = xdg.join(XDG_SUBDIR);
        match secure_runtime_dir(&candidate, uid) {
            Ok(()) => return Ok(candidate),
            Err(error) => tracing::warn!(
                path = %candidate.display(),
                %error,
                "XDG_RUNTIME_DIR unusable for remuda sockets; falling back to tmp"
            ),
        }
    }
    let fallback = std::env::temp_dir().join(format!("remuda-{uid}"));
    secure_runtime_dir(&fallback, uid)?;
    Ok(fallback)
}

/// Verify `dir` is a real, own directory and make it 0700.
///
/// Order matters: the symlink and owner checks run *before* the chmod, so a
/// path an attacker planted (a symlink into their tree, or a directory they
/// own) is never written to.
fn secure_runtime_dir(dir: &Path, uid: u32) -> io::Result<()> {
    let metadata = match std::fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir)?;
            std::fs::symlink_metadata(dir)?
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
    }
    set_dir_mode_0700(dir)?;
    Ok(())
}

#[cfg(unix)]
fn set_dir_mode_0700(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_dir_mode_0700(_dir: &Path) -> io::Result<()> {
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
