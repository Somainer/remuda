//! Short-lived temp roots for AF_UNIX sockets in tests.
//!
//! `tempfile::tempdir()` honours `TMPDIR`, and a long `TMPDIR` (or the macOS
//! per-user temp dir) pushes a socket path like `<tmp>/herdr/herdr.sock` past
//! the 107/103-byte `sun_path` limit. Tests that bind a unix socket therefore
//! create the socket directory here, choosing the first per-user runtime
//! candidate under which the full socket address fits, like production's
//! chooser does. Other fixture files (cwd, home, launch dirs) are unaffected
//! and may keep using `tempfile`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A unique private directory under a per-user remuda runtime root chosen so
/// that `dir.join(suffix)` always fits `sun_path`.
///
/// Removed recursively on drop, like `tempfile::TempDir`.
#[must_use]
pub struct ShortTempDir {
    path: PathBuf,
}

impl ShortTempDir {
    /// Create a fresh directory (0700, this uid) for a socket that will sit
    /// directly at its root.
    pub fn new() -> io::Result<Self> {
        Self::for_socket_suffix("herdr.sock")
    }

    /// Create a fresh directory whose join with `socket_suffix` stays within
    /// the platform's `sun_path`.
    ///
    /// `socket_suffix` is the relative path from this directory the test will
    /// bind at, e.g. `herdr/herdr.sock`.
    pub fn for_socket_suffix(socket_suffix: &str) -> io::Result<Self> {
        let limit = remuda_driver::runtime_socket_limit();
        // The unique leaf is bounded well under this placeholder width
        // (`test-<pid>-<nanos>-<seq>` ≈ 36 bytes); size the root against the
        // placeholder so no directory is created under a root that cannot fit.
        const PLACEHOLDER_LEAF: &str = "test-2147483647-18446744073709551615-4294967295";
        for root in remuda_driver::runtime_socket_candidates() {
            let fits = root
                .join(PLACEHOLDER_LEAF)
                .join(socket_suffix)
                .as_os_str()
                .len()
                <= limit;
            if !fits {
                continue;
            }
            let dir = Self::unique_under(&root)?;
            return Ok(Self { path: dir });
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no short runtime root fits the requested socket suffix",
        ))
    }

    /// Allocate a unique directory under `root` (created 0700).
    fn unique_under(root: &Path) -> io::Result<PathBuf> {
        std::fs::create_dir_all(root)?;
        for _ in 0..8 {
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let name = format!(
                "test-{}-{}-{seq}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            let path = root.join(name);
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
                    }
                    return Ok(path);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique short test temp dir",
        ))
    }

    /// The directory path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for ShortTempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for ShortTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
