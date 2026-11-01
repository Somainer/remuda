//! Install per-test stub executables without `ETXTBSY`.
//!
//! Linux refuses `exec` of a file that any process still holds open for
//! writing. Two things make tests trip it:
//!
//! * reusing a destination another child is executing, and
//! * opening the destination for write in this process at all: every `fork`
//!   in any test thread inherits that descriptor, and a descendant that has
//!   not exec'd yet keeps the inode busy — a later `rename` does not help,
//!   because the inherited descriptor and the renamed path are the same inode.
//!
//! So bytes are never written by this process. A short-lived `/bin/sh` child
//! writes a unique `*.part` and sets its mode; when it exits its descriptor
//! is gone, the parent renames the part into place, and the exec that follows
//! has no writer anywhere. `install_executable` additionally never reuses a
//! destination, so an executing child cannot block the install either.

use std::fs;
#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};
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

/// Install `bytes` as the executable at exactly `path`.
///
/// Some callers cannot pick the final name — a git hook must sit at
/// `.git/hooks/pre-receive`, a `core.sshCommand` must be the path the config
/// names. Writing such a file and executing it immediately races `ETXTBSY`
/// (see the module docs): the bytes go to a unique hidden `*.part` in the same
/// directory, written and chmodded by a child process so no descriptor of
/// this process ever refers to the inode, and only then is it renamed over
/// `path`. A concurrent `exec` either sees the previous file or the complete
/// new one, never a half-written one, and no descendant of this process can
/// keep the new file busy.
///
/// # Errors
///
/// Returns the write, chmod, or rename failure; a failed call leaves no part
/// file behind.
pub fn install_at(path: &Path, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "an executable needs a parent directory",
        )
    })?;
    fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("stub");
    let part = dir.join(format!(".{name}-{}.part", unique_token()));
    let result = write_part(&part, bytes.as_ref()).and_then(|()| fs::rename(&part, path));
    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result
}

fn try_install(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let token = unique_token();
    let dest = dir.join(format!("{name}-{token}"));
    let part = dir.join(format!(".{name}-{token}.part"));
    if dest.exists() {
        return Err(io::Error::from(ErrorKind::AlreadyExists));
    }
    write_part(&part, bytes)?;
    if let Err(err) = fs::rename(&part, &dest) {
        let _ = fs::remove_file(&part);
        return Err(err);
    }
    Ok(dest)
}

/// Write `bytes` to `path` as a 0755 file, with the write done by a child.
///
/// The child exists for its descriptor table: an `O_WRONLY` handle opened in
/// this process is inherited by every `fork` any thread performs, and a
/// descendant still holding it makes a later `exec` of that inode fail with
/// `ETXTBSY`. Nothing else in this process can inherit the child's handle.
///
/// The part never outlives a failure: the shell removes it on any non-zero
/// exit until the bytes and the mode are in place (a failed `cat`/`chmod`
/// leaves it behind otherwise), and the caller removes it again if the child
/// could not be fed or reaped. The child is always reaped, including when
/// `write_all` fails with `EPIPE` because the writer already exited.
fn write_part(path: &Path, bytes: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(
                r#"trap 'rm -f "$1"' EXIT; umask 077 && /bin/cat > "$1" && /bin/chmod 755 "$1" && trap - EXIT"#,
            )
            .arg("sh")
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let written = match child.stdin.take() {
            Some(mut stdin) => {
                let written = stdin.write_all(bytes);
                // Closing the pipe lets the writer finish even after EPIPE.
                drop(stdin);
                written
            }
            None => Err(io::Error::other("executable writer has no stdin")),
        };
        let reaped = child.wait();
        // A failed write still has to be reported after the child is reaped,
        // and any state it left behind is removed below.
        let result = match (written, reaped) {
            (Err(err), _) => Err(err),
            (Ok(()), Err(err)) => Err(err),
            (Ok(()), Ok(status)) if status.success() => Ok(()),
            (Ok(()), Ok(status)) => Err(io::Error::other(format!(
                "executable writer for {} exited with {status}",
                path.display()
            ))),
        };
        if result.is_err() {
            let _ = fs::remove_file(path);
        }
        result
    }
    #[cfg(not(unix))]
    {
        let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
        file.write_all(bytes)?;
        file.flush()?;
        let _ = file.sync_all();
        Ok(())
    }
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

    #[test]
    fn install_at_write_rename_never_leaves_a_part_file() {
        let dir = tempfile::tempdir().unwrap();
        let hook = dir.path().join("hooks").join("pre-receive");
        install_at(&hook, b"#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(fs::read(&hook).unwrap(), b"#!/bin/sh\nexit 0\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&hook).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o755);
        }
        let leftover_parts = fs::read_dir(hook.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".part"));
        assert!(!leftover_parts, "part files must be renamed away");
        // A second install replaces the same path in place.
        install_at(&hook, b"#!/bin/sh\nexit 1\n").unwrap();
        assert_eq!(fs::read(&hook).unwrap(), b"#!/bin/sh\nexit 1\n");
    }

    /// A writer that cannot create or finish its part must not leave the
    /// partial file behind for a later test (or a later install) to exec.
    #[cfg(unix)]
    #[test]
    fn a_failed_write_leaves_no_part_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("blocked");
        fs::create_dir(&blocked).unwrap();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o555)).unwrap();
        let result = install_at(&blocked.join("script"), b"#!/bin/sh\nexit 0\n");
        // Restore so the tempdir can be cleaned up.
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();
        if result.is_ok() {
            // Root ignores the directory mode; there is nothing to prove.
            return;
        }
        let leftovers: Vec<_> = fs::read_dir(&blocked)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect();
        assert!(
            leftovers.is_empty(),
            "a failed write left files behind: {leftovers:?}"
        );
    }
}
