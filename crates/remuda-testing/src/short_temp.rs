//! Short-lived temp roots for AF_UNIX sockets in tests.
//!
//! `tempfile::tempdir()` honours `TMPDIR`, and a long `TMPDIR` (or the macOS
//! per-user temp dir) pushes a socket path like `<tmp>/herdr/herdr.sock` past
//! the 107/103-byte `sun_path` limit. Tests that bind a unix socket therefore
//! create the socket directory here, under the same secure per-user runtime
//! root production uses, where every `<root>/.tmpXXXXXX/herdr/herdr.sock`
//! address stays well under 50 bytes. Other fixture files (cwd, home, launch
//! dirs) are unaffected and may keep using `tempfile`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A unique private directory under the per-user remuda runtime root.
///
/// Removed recursively on drop, like `tempfile::TempDir`.
#[must_use]
pub struct ShortTempDir {
    path: PathBuf,
}

impl ShortTempDir {
    /// Create a fresh directory (0700, this uid).
    pub fn new() -> io::Result<Self> {
        let root = remuda_driver::runtime_socket_root()?;
        std::fs::create_dir_all(&root)?;
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
                    return Ok(Self { path });
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
