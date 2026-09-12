//! Locate workspace test binaries under `CARGO_TARGET_DIR`.
//!
//! Isolated worktrees (`git worktree add /tmp/v HEAD` + `CARGO_TARGET_DIR=<repo>/target`)
//! build into the pointed-at target dir. Lookup must not assume `./target`.
//!
//! On-demand `cargo build` of `fake-claude` / `fake-herdr` is serialized with
//! an exclusive flock on `<target>/remuda-bin-locator.lock`. It is skipped when
//! the binary is already on disk or `REMUDA_<NAME>_BIN` is set, and it never
//! builds into a target dir owned by a parent `cargo test`. After a build (or
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

/// Env override for a stub binary (`REMUDA_FAKE_HERDR_BIN`, `REMUDA_FAKE_CLAUDE_BIN`, …).
pub fn env_bin_override(name: &str) -> Option<PathBuf> {
    let key = env_bin_key(name);
    let path = PathBuf::from(std::env::var_os(key)?);
    path.is_file().then_some(path)
}

fn env_bin_key(name: &str) -> String {
    format!("REMUDA_{}_BIN", name.replace('-', "_").to_ascii_uppercase())
}

/// Locate a remuda-testing binary, honoring env override, cargo bin-exe, and target dir.
pub fn locate_workspace_bin(name: &str) -> Option<PathBuf> {
    env_bin_override(name)
        .or_else(|| cargo_bin_exe(name))
        .or_else(|| locate_bin_in(&cargo_target_dir(), name))
}

/// Conventional fallback path used in error messages when the binary is not on disk yet.
pub fn fallback_bin_path(name: &str) -> PathBuf {
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    cargo_target_dir().join(profile).join(name)
}

/// Return `{name}` built for this workspace, without racing a parent `cargo test`.
///
/// Lookup order: `REMUDA_<NAME>_BIN` (hyphens → underscores), `CARGO_BIN_EXE_*`,
/// then `{debug,release}/<name>` under [`cargo_target_dir`]. If the binary is
/// missing, a nested `cargo build -p remuda-testing --bin <name>` runs **only**
/// when this process is not already inside a parent cargo that owns the same
/// target dir. Under `cargo test` the fallback uses a sidecar target
/// (`<target>/remuda-test-bins`) so it never takes the parent `.cargo-lock`.
///
/// Concurrent callers share an exclusive flock on `<target>/remuda-bin-locator.lock`.
///
/// # Panics
///
/// Panics if `cargo build` fails or the binary is still missing / not exec-able
/// afterwards.
pub fn ensure_workspace_bin(name: &str) -> PathBuf {
    if let Some(path) = env_bin_override(name).or_else(|| cargo_bin_exe(name)) {
        wait_until_runnable(&path);
        return path;
    }
    let target_dir = cargo_target_dir();
    let sidecar = target_dir.join("remuda-test-bins");
    let _lock = lock_bin_locator(&target_dir);
    if let Some(path) = locate_bin_in(&target_dir, name).or_else(|| locate_bin_in(&sidecar, name)) {
        wait_until_runnable(&path);
        return path;
    }
    let build_dir = if parent_cargo_owns_target(&target_dir) {
        sidecar
    } else {
        target_dir.clone()
    };
    cargo_build_testing_bin(name, &build_dir);
    let path = locate_bin_in(&build_dir, name).unwrap_or_else(|| {
        panic!(
            "{name} binary not found under {}/{{debug,release}}",
            build_dir.display()
        )
    });
    wait_until_runnable(&path);
    path
}

/// `CARGO` is set for `cargo test` / `cargo build` children. Nested cargo into
/// the same `--target-dir` then fights the parent's `.cargo-lock`.
fn parent_cargo_owns_target(target_dir: &Path) -> bool {
    let Some(cargo) = std::env::var_os("CARGO") else {
        return false;
    };
    if cargo.is_empty() {
        return false;
    }
    let parent_target = std::env::var_os("CARGO_TARGET_DIR")
        .or_else(|| std::env::var_os("CARGO_BUILD_TARGET_DIR"))
        .map(PathBuf::from);
    match parent_target {
        Some(parent) => {
            let parent = if parent.is_absolute() {
                parent
            } else {
                workspace_root().join(parent)
            };
            paths_match(&parent, target_dir)
        }
        None => true,
    }
}

fn paths_match(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn cargo_build_testing_bin(name: &str, target_dir: &Path) {
    std::fs::create_dir_all(target_dir)
        .unwrap_or_else(|err| panic!("create cargo target dir {}: {err}", target_dir.display()));
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
        .arg(target_dir)
        .env("CARGO_TARGET_DIR", target_dir)
        .env("CARGO_BUILD_TARGET_DIR", target_dir)
        .status()
        .unwrap_or_else(|err| panic!("cargo build -p remuda-testing --bin {name}: {err}"));
    assert!(
        status.success(),
        "cargo build -p remuda-testing --bin {name} failed with {status}"
    );
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

    #[test]
    fn fake_herdr_env_override_key() {
        assert_eq!(env_bin_key("fake-herdr"), "REMUDA_FAKE_HERDR_BIN");
        assert_eq!(env_bin_key("fake-claude"), "REMUDA_FAKE_CLAUDE_BIN");
    }
}
