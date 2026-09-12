//! Locate workspace test binaries under `CARGO_TARGET_DIR`.
//!
//! Isolated worktrees (`git worktree add /tmp/v HEAD` + `CARGO_TARGET_DIR=<repo>/target`)
//! build into the pointed-at target dir. Lookup must not assume `./target`.
//!
//! On-demand `cargo build` of `fake-claude` / `fake-herdr` is serialized with
//! an exclusive flock on `<target>/remuda-bin-locator.lock`. After a build (or
//! when another process may still be writing the file), the locator waits until
//! the binary can be exec'd, retrying `ETXTBSY` / [`std::io::ErrorKind::ExecutableFileBusy`].

use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// Workspace root (`crates/remuda-testing/../..`).
pub fn workspace_root() -> PathBuf {
    let nested = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    nested.canonicalize().unwrap_or(nested)
}

/// Cargo target directory: `CARGO_TARGET_DIR`, then `CARGO_BUILD_TARGET_DIR`, else `<workspace>/target`.
pub fn cargo_target_dir() -> PathBuf {
    for key in ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"] {
        if let Ok(dir) = std::env::var(key) {
            let path = PathBuf::from(dir);
            if path.as_os_str().is_empty() {
                continue;
            }
            if path.is_absolute() {
                return path;
            }
            return workspace_root().join(path);
        }
    }
    workspace_root().join("target")
}

/// `CARGO_BIN_EXE_<name>` (hyphen or underscore) when cargo set it for this package.
pub fn cargo_bin_exe(name: &str) -> Option<PathBuf> {
    let hyphen = format!("CARGO_BIN_EXE_{name}");
    let underscore = format!("CARGO_BIN_EXE_{}", name.replace('-', "_"));
    for key in [hyphen, underscore] {
        if let Ok(path) = std::env::var(key) {
            let path = PathBuf::from(path);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

/// Search `<target_dir>/{<triple>/,}{debug,release}/<name>` including `.exe`.
pub fn locate_bin_in(target_dir: &Path, name: &str) -> Option<PathBuf> {
    let mut dirs = vec![target_dir.to_path_buf()];
    for key in ["CARGO_BUILD_TARGET", "TARGET"] {
        if let Ok(triple) = std::env::var(key)
            && !triple.is_empty()
        {
            dirs.push(target_dir.join(triple));
        }
    }
    let exe = format!("{name}.exe");
    let names = [name, exe.as_str()];
    let profiles: &[&str] = match std::env::var("PROFILE").as_deref() {
        Ok("release") => &["release", "debug"],
        _ => &["debug", "release"],
    };
    for dir in dirs {
        for profile in profiles {
            for candidate_name in names {
                let candidate = dir.join(profile).join(candidate_name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// Locate a remuda-testing binary, honoring cargo bin-exe env and target dir.
pub fn locate_workspace_bin(name: &str) -> Option<PathBuf> {
    cargo_bin_exe(name).or_else(|| locate_bin_in(&cargo_target_dir(), name))
}

/// Conventional fallback path used in error messages when the binary is not on disk yet.
pub fn fallback_bin_path(name: &str) -> PathBuf {
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    cargo_target_dir().join(profile).join(name)
}

/// Build `-p remuda-testing --bin <name>` into [`cargo_target_dir`] and return the path.
///
/// Concurrent callers share an exclusive flock so only one `cargo build` runs
/// at a time. If the binary is already present it is not rebuilt (CI prebuilds
/// with `cargo build -p remuda-testing --bins` for this reason).
///
/// # Panics
///
/// Panics if `cargo build` fails or the binary is still missing / not exec-able
/// afterwards.
pub fn ensure_workspace_bin(name: &str) -> PathBuf {
    if let Some(path) = cargo_bin_exe(name) {
        wait_until_runnable(&path);
        return path;
    }
    let target_dir = cargo_target_dir();
    let _lock = lock_bin_locator(&target_dir);
    if let Some(path) = locate_bin_in(&target_dir, name) {
        wait_until_runnable(&path);
        return path;
    }
    let status = Command::new(env!("CARGO"))
        .current_dir(workspace_root())
        .args([
            "build",
            "-p",
            "remuda-testing",
            "--bin",
            name,
            "--quiet",
            "--target-dir",
        ])
        .arg(&target_dir)
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("CARGO_BUILD_TARGET_DIR", &target_dir)
        .status()
        .unwrap_or_else(|err| panic!("cargo build -p remuda-testing --bin {name}: {err}"));
    assert!(
        status.success(),
        "cargo build -p remuda-testing --bin {name} failed with {status}"
    );
    let path = locate_bin_in(&target_dir, name).unwrap_or_else(|| {
        panic!(
            "{name} binary not found under {}/{{debug,release}}",
            target_dir.display()
        )
    });
    wait_until_runnable(&path);
    path
}

struct BinLocatorLock {
    _file: File,
}

fn lock_bin_locator(target_dir: &Path) -> BinLocatorLock {
    std::fs::create_dir_all(target_dir)
        .unwrap_or_else(|err| panic!("create cargo target dir {}: {err}", target_dir.display()));
    let lock_path = target_dir.join("remuda-bin-locator.lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap_or_else(|err| panic!("open {}: {err}", lock_path.display()));
    file.lock_exclusive()
        .unwrap_or_else(|err| panic!("flock {}: {err}", lock_path.display()));
    BinLocatorLock { _file: file }
}

fn is_etxtbsy(err: &io::Error) -> bool {
    err.kind() == ErrorKind::ExecutableFileBusy || err.raw_os_error() == Some(26)
}

fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn probe_exec(path: &Path) -> io::Result<()> {
    Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| ())
}

fn wait_until_runnable(path: &Path) {
    let mut delay = Duration::from_millis(10);
    for _ in 0..40 {
        if is_executable(path) {
            match probe_exec(path) {
                Ok(()) => return,
                Err(err) if is_etxtbsy(&err) => {}
                Err(_) => return,
            }
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(200));
    }
    panic!(
        "{} is not executable (ETXTBSY or still being written)",
        path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_target_dir_is_absolute() {
        let dir = cargo_target_dir();
        assert!(dir.is_absolute(), "{}", dir.display());
    }

    #[test]
    fn ensure_fake_herdr_is_runnable() {
        let path = ensure_workspace_bin("fake-herdr");
        assert!(path.is_file(), "{}", path.display());
        assert!(is_executable(&path), "{}", path.display());
    }
}
