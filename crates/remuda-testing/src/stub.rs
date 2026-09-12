//! Install per-test stub executables without `ETXTBSY`.
//!
//! Linux refuses `exec` of a file that is still open for write, and refuses
//! `open(O_WRONLY)` / rename-over of a file a child still has mapped. Tests
//! therefore never reuse a destination: bytes go to a unique `*.part` name,
//! then `rename` installs a unique final path.

use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SEQ: AtomicU64 = AtomicU64::new(1);

/// Write `bytes` as a new executable under `dir`.
///
/// The returned path is unique for this process (never `dir/name` reused).
/// The file is closed before it is renamed into place so a later `--version`
/// probe is not racing a still-open write handle.
///
/// # Panics
///
/// Panics if the directory cannot be created or every unique name hits
/// [`ErrorKind::ExecutableFileBusy`].
pub fn install_executable(dir: &Path, name: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
    fs::create_dir_all(dir)
        .unwrap_or_else(|err| panic!("create stub dir {}: {err}", dir.display()));
    let bytes = bytes.as_ref();
    let mut last = None;
    for _ in 0..16 {
        match try_install(dir, name, bytes) {
            Ok(path) => return path,
            Err(err) if is_etxtbsy(&err) || err.kind() == ErrorKind::AlreadyExists => {
                last = Some(err);
            }
            Err(err) => panic!("install executable {name} in {}: {err}", dir.display()),
        }
    }
    panic!(
        "install executable {name} in {}: {}",
        dir.display(),
        last.map(|err| err.to_string())
            .unwrap_or_else(|| "ETXTBSY".into())
    );
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

fn unique_token() -> String {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}-{seq}", std::process::id())
}

fn is_etxtbsy(err: &io::Error) -> bool {
    err.kind() == ErrorKind::ExecutableFileBusy || err.raw_os_error() == Some(26)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_executable_never_reuses_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let first = install_executable(dir.path(), "claude", b"#!/bin/sh\necho one\n");
        let second = install_executable(dir.path(), "claude", b"#!/bin/sh\necho two\n");
        assert_ne!(first, second);
        assert!(
            first
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("claude-"))
        );
        let leftover_parts = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".part"));
        assert!(!leftover_parts, "part files must be renamed away");
    }
}
