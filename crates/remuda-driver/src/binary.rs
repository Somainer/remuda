//! Absolute path, `--version`, and SHA-256 pin for a native agent binary.

use crate::error::{DriverError, DriverResult};
use remuda_protocol::Digest;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static PIN_SEQ: AtomicU64 = AtomicU64::new(1);

/// Re-probes of the original path before its bytes are copied elsewhere.
const ETXTBSY_REPROBES: u32 = 6;

/// Base delay between re-probes; the nth wait is `n` times this.
const ETXTBSY_BACKOFF: Duration = Duration::from_millis(2);

/// Pinned native executable recorded in a [`crate::LaunchRecipe`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinaryPin {
    /// Canonical absolute path of the file that will be exec'd.
    pub abs_path: String,
    /// First line of `--version` stdout (stderr fallback), trimmed.
    pub version: String,
    /// SHA-256 of the file contents, `sha256:` prefixed.
    pub sha256: Digest,
}

/// Resolve `command` on `PATH` or accept an absolute path, then pin it.
///
/// `ETXTBSY` has two causes and only one of them justifies a copy. A sibling
/// thread that forked between our `open(O_WRONLY)` and its `exec` holds a
/// write handle for microseconds, so the probe is retried on the original path
/// first — otherwise two pins of the same file would disagree on `abs_path`.
/// Only a writer that survives every re-probe (a real mapped or open-for-write
/// inode) makes the bytes get copied to a unique sibling path, and that copy is
/// pinned instead of overwriting the busy inode.
pub fn pin_binary(command: impl AsRef<Path>) -> DriverResult<BinaryPin> {
    let original = resolve_binary(command)?;
    let (path, version) = probe_version(&original)?;
    let path = path.canonicalize().unwrap_or(path);
    let sha256 = hash_file(&path)?;
    tracing::info!(
        path = %path.display(),
        version = %version,
        digest = %String::from(sha256.clone()),
        "pinned native binary"
    );
    Ok(BinaryPin {
        abs_path: path.to_string_lossy().into_owned(),
        version,
        sha256,
    })
}

/// Read `--version`, preferring `original` and falling back to a fresh copy.
///
/// Returns the path that actually answered, which is what gets pinned.
fn probe_version(original: &Path) -> DriverResult<(PathBuf, String)> {
    let mut last_busy = None;
    for attempt in 0..=ETXTBSY_REPROBES {
        match read_version(original) {
            Ok(line) => return Ok((original.to_path_buf(), line)),
            Err(DriverError::Io(err)) if is_etxtbsy(&err) => {
                last_busy = Some(err);
                // A fork window closes on its own; a real writer does not.
                if attempt < ETXTBSY_REPROBES {
                    std::thread::sleep(ETXTBSY_BACKOFF * (attempt + 1));
                }
            }
            Err(err) => return Err(err),
        }
    }
    for _ in 0..4 {
        let copy = copy_to_fresh_path(original)?;
        tracing::warn!(
            src = %original.display(),
            dest = %copy.display(),
            "pin_binary copied binary to a fresh path after persistent ETXTBSY"
        );
        match read_version(&copy) {
            Ok(line) => return Ok((copy, line)),
            Err(DriverError::Io(err)) if is_etxtbsy(&err) => last_busy = Some(err),
            Err(err) => return Err(err),
        }
    }
    Err(DriverError::Io(last_busy.unwrap_or_else(|| {
        io::Error::new(
            ErrorKind::ExecutableFileBusy,
            format!(
                "pin_binary: {} still busy after copying to a fresh path",
                original.display()
            ),
        )
    })))
}

/// Resolve a command name or path to a canonical absolute file.
pub fn resolve_binary(command: impl AsRef<Path>) -> DriverResult<PathBuf> {
    let command = command.as_ref();
    let candidate = if command.is_absolute() {
        command.to_path_buf()
    } else {
        find_on_path(command).ok_or_else(|| DriverError::BinaryNotFound(command.to_path_buf()))?
    };
    if !candidate.is_file() {
        return Err(DriverError::BinaryNotFound(candidate));
    }
    candidate.canonicalize().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            DriverError::BinaryNotFound(candidate)
        } else {
            DriverError::Io(error)
        }
    })
}

/// SHA-256 a file at `path`.
pub fn hash_file(path: &Path) -> DriverResult<Digest> {
    let mut hasher = Sha256::new();
    let mut file = File::open(path)?;
    let mut buf = [0_u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    digest_from_sha(hasher.finalize())
}

/// SHA-256 in-memory bytes (settings overlay, tests).
pub fn hash_bytes(bytes: &[u8]) -> DriverResult<Digest> {
    digest_from_sha(Sha256::digest(bytes))
}

fn digest_from_sha(hash: impl core::fmt::LowerHex) -> DriverResult<Digest> {
    let encoded = format!("sha256:{hash:x}");
    Digest::try_from(encoded).map_err(DriverError::Protocol)
}

fn read_version(path: &Path) -> DriverResult<String> {
    let output = Command::new(path).arg("--version").output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| {
            DriverError::InvalidLaunchSpec(format!(
                "{} produced no --version output",
                path.display()
            ))
        })?;
    Ok(line.to_string())
}

fn is_etxtbsy(err: &io::Error) -> bool {
    err.kind() == ErrorKind::ExecutableFileBusy || err.raw_os_error() == Some(26)
}

fn unique_token() -> String {
    let seq = PIN_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}-{seq}", std::process::id())
}

/// Copy `src` to a unique path so `exec` is not racing a mapped inode.
fn copy_to_fresh_path(src: &Path) -> DriverResult<PathBuf> {
    let bytes = fs::read(src)?;
    let stem = src
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("pinned-bin");
    let mut dirs = Vec::new();
    if let Some(parent) = src.parent() {
        dirs.push(parent.to_path_buf());
    }
    dirs.push(std::env::temp_dir());
    let mut last_err = None;
    for dir in dirs {
        match install_unique(&dir, stem, &bytes) {
            Ok(path) => return Ok(path),
            Err(err) => last_err = Some(err),
        }
    }
    Err(DriverError::Io(last_err.unwrap_or_else(|| {
        io::Error::other(format!(
            "pin_binary: could not copy {} to a fresh path",
            src.display()
        ))
    })))
}

fn install_unique(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let mut last = None;
    for _ in 0..16 {
        match try_install(dir, name, bytes) {
            Ok(path) => return Ok(path),
            Err(err) if is_etxtbsy(&err) || err.kind() == ErrorKind::AlreadyExists => {
                last = Some(err);
            }
            Err(err) => return Err(err),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::from(ErrorKind::ExecutableFileBusy)))
}

fn try_install(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let token = unique_token();
    let dest = dir.join(format!("{name}-{token}"));
    let part = dir.join(format!(".{name}-{token}.part"));
    if dest.exists() {
        return Err(io::Error::from(ErrorKind::AlreadyExists));
    }
    {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&part)?;
        file.write_all(bytes)?;
        file.flush()?;
        let _ = file.sync_all();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&part, fs::Permissions::from_mode(0o755))?;
    }
    fs::rename(&part, &dest)?;
    Ok(dest)
}

fn find_on_path(name: &Path) -> Option<PathBuf> {
    let file_name = name.file_name()?;
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(file_name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Default executable name for a driver kind.
pub fn default_command(kind: remuda_protocol::DriverKind) -> Option<&'static str> {
    use remuda_protocol::DriverKind::*;
    match kind {
        ClaudePrint | ClaudePty | ClaudeBg => Some("claude"),
        CodexAppserver => Some("codex"),
        GrokAcp => Some("grok"),
        AgyPrint => Some("agy"),
        GenericPty | ShellPty => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_is_idempotent_for_a_stub_binary() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_stub(dir.path(), "stub-9.9.9 (test)");
        let first = pin_binary(&path).unwrap();
        let second = pin_binary(&path).unwrap();
        assert_eq!(first, second);
        assert!(first.abs_path.starts_with('/'));
        assert_eq!(first.version, "stub-9.9.9 (test)");
        assert!(String::from(first.sha256.clone()).starts_with("sha256:"));
    }

    fn write_stub(dir: &Path, version: &str) -> PathBuf {
        install_unique(
            dir,
            "agent",
            format!("#!/bin/sh\necho '{version}'\n").as_bytes(),
        )
        .unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pin_binary_copies_to_a_fresh_path_when_source_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_stub(dir.path(), "stub-busy (test)");
        let _writer = OpenOptions::new().write(true).open(&path).unwrap();
        let pin = pin_binary(&path).unwrap();
        let pinned = PathBuf::from(&pin.abs_path);
        assert_ne!(
            pinned,
            path.canonicalize().unwrap(),
            "busy source must not be overwritten"
        );
        assert_eq!(pin.version, "stub-busy (test)");
        assert!(pinned.is_file());
    }

    /// A writer that closes mid-probe is the fork window between a sibling
    /// thread's `open(O_WRONLY)` and its `exec`. Pinning must wait it out and
    /// keep the original path, or two pins of one file disagree on `abs_path`.
    #[cfg(target_os = "linux")]
    #[test]
    fn pin_is_idempotent_when_a_transient_writer_closes() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_stub(dir.path(), "stub-transient (test)");
        let writer = OpenOptions::new().write(true).open(&path).unwrap();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(8));
            drop(writer);
        });
        let first = pin_binary(&path).unwrap();
        handle.join().unwrap();
        let second = pin_binary(&path).unwrap();
        assert_eq!(first, second, "transient ETXTBSY must not fork the path");
        assert_eq!(
            PathBuf::from(&first.abs_path),
            path.canonicalize().unwrap(),
            "a writer that closes must not trigger a copy"
        );
    }

    /// Concurrent pins of one file must agree — the CI failure mode.
    #[test]
    fn concurrent_pins_of_one_binary_agree() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_stub(dir.path(), "stub-parallel (test)");
        let pins: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| pin_binary(&path).unwrap()))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for pin in &pins {
            assert_eq!(pin, &pins[0], "concurrent pins must be identical");
        }
    }
}
