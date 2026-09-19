//! End-to-end checks for the per-user runtime directory.
//!
//! Cases that need a specific `XDG_RUNTIME_DIR` run in a child process (the
//! parent supplies the variable through [`std::process::Command::env`]): the
//! workspace forbids unsafe code and `env::set_var` is unsafe on this
//! toolchain.
#![cfg(unix)]

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use remuda_signal::runtime_dir::{
    SUN_PATH_LIMIT, bind_io_error, path_fits_sun_path, per_user_runtime_dir, place_socket,
};

const CHILD_MARKER_ENV: &str = "REMUDA_RTD_MARKER";

/// One subprocess at a time: the children share this uid's fallback tmp
/// directory and its security assumptions.
static CHILD_LOCK: Mutex<()> = Mutex::new(());

fn run_child(name: &str, xdg: Option<&Path>, tmpdir: Option<&Path>, marker: &Path) {
    let _guard = CHILD_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", name, "--nocapture"])
        .env("REMUDA_RTD_CHILD", "1")
        .env(CHILD_MARKER_ENV, marker);
    match xdg {
        Some(xdg) => {
            command.env("XDG_RUNTIME_DIR", xdg);
        }
        None => {
            command.env_remove("XDG_RUNTIME_DIR");
        }
    }
    match tmpdir {
        Some(tmpdir) => {
            command.env("TMPDIR", tmpdir);
        }
        None => {
            command.env_remove("TMPDIR");
        }
    }
    let status = command.status().unwrap();
    assert!(status.success(), "child test {name} failed: {status:?}");
}

fn marker_path() -> PathBuf {
    PathBuf::from(std::env::var_os(CHILD_MARKER_ENV).expect("child marker path"))
}

#[test]
fn xdg_runtime_dir_is_used_private_and_stable() {
    let root = tempfile::tempdir().unwrap();
    let xdg = root.path().join("xdg-runtime");
    std::fs::create_dir_all(&xdg).unwrap();
    let marker = root.path().join("chosen");
    run_child(
        "xdg_runtime_dir_is_used_private_and_stable_child",
        Some(&xdg),
        None,
        &marker,
    );
    let chosen = PathBuf::from(std::fs::read_to_string(&marker).unwrap().trim());
    assert!(chosen.starts_with(&xdg), "{}", chosen.display());
    let metadata = std::fs::symlink_metadata(&chosen).unwrap();
    assert!(metadata.is_dir());
    assert_eq!(metadata.mode() & 0o777, 0o700);
    let own_uid = std::fs::symlink_metadata(root.path()).unwrap().uid();
    assert_eq!(metadata.uid(), own_uid);
}

#[test]
fn xdg_runtime_dir_is_used_private_and_stable_child() {
    if std::env::var_os("REMUDA_RTD_CHILD").is_none() {
        return;
    }
    let first = per_user_runtime_dir().unwrap();
    let second = per_user_runtime_dir().unwrap();
    assert_eq!(first, second);
    let metadata = std::fs::symlink_metadata(&first).unwrap();
    assert_eq!(metadata.mode() & 0o777, 0o700);
    std::fs::write(marker_path(), first.as_os_str().as_encoded_bytes()).unwrap();
}

#[test]
fn an_unwritable_xdg_runtime_dir_falls_back_to_tmp() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let xdg = root.path().join("xdg-runtime");
    std::fs::create_dir_all(&xdg).unwrap();
    // Owner r-x can traverse but cannot create the `remuda` subdirectory.
    std::fs::set_permissions(&xdg, std::fs::Permissions::from_mode(0o500)).unwrap();
    let marker = root.path().join("chosen");
    run_child(
        "an_unwritable_xdg_runtime_dir_falls_back_to_tmp_child",
        Some(&xdg),
        None,
        &marker,
    );
    std::fs::set_permissions(&xdg, std::fs::Permissions::from_mode(0o700)).unwrap();
    let chosen = PathBuf::from(std::fs::read_to_string(&marker).unwrap().trim());
    assert!(
        !chosen.starts_with(&xdg),
        "an unusable XDG_RUNTIME_DIR must fall back: {}",
        chosen.display()
    );
    assert!(chosen.is_dir());
}

#[test]
fn an_unwritable_xdg_runtime_dir_falls_back_to_tmp_child() {
    if std::env::var_os("REMUDA_RTD_CHILD").is_none() {
        return;
    }
    let chosen = per_user_runtime_dir().unwrap();
    std::fs::write(marker_path(), chosen.as_os_str().as_encoded_bytes()).unwrap();
}

#[test]
fn a_runtime_path_that_still_overflows_is_an_error_not_a_silent_bind() {
    let root = tempfile::tempdir().unwrap();
    let xdg = root.path().join("x".repeat(110));
    run_child(
        "a_runtime_path_that_still_overflows_is_an_error_not_a_silent_bind_child",
        Some(&xdg),
        None,
        &root.path().join("unused"),
    );
}

#[test]
fn a_runtime_path_that_still_overflows_is_an_error_not_a_silent_bind_child() {
    if std::env::var_os("REMUDA_RTD_CHILD").is_none() {
        return;
    }
    let xdg = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    let preferred = xdg.join("data/instances/ins_x/hook.sock");
    // Even the fixed /tmp root cannot save a 120-character socket name.
    let impossible_name = "n".repeat(120) + ".sock";
    let error = place_socket(&preferred, &impossible_name).unwrap_err();
    assert!(
        error.to_string().contains("no AF_UNIX placement fits"),
        "{error}"
    );
}

/// The macOS-shaped case: long TMPDIR, no XDG. The temp fallback itself then
/// overflows sun_path, so the fixed `/tmp/remuda-<uid>` last resort must
/// produce a bindable short path.
#[test]
fn a_long_tmpdir_without_xdg_falls_back_to_fixed_tmp_root() {
    // 35 filler chars: /tmp/sockpath-lastresort-<35 x> is 60 bytes, and the
    // TMPDIR fallback (`<TMPDIR>/remuda-<uid>/<uuid>.sock`) lands at ~115
    // bytes — over the Linux limit and far over macOS's 103.
    let long_tmp = PathBuf::from(format!("/tmp/sockpath-lastresort-{}", "x".repeat(35)));
    assert_eq!(long_tmp.as_os_str().len(), 60);
    std::fs::create_dir_all(&long_tmp).unwrap();
    let marker = long_tmp.join("chosen");
    run_child(
        "a_long_tmpdir_without_xdg_falls_back_to_fixed_tmp_root_child",
        None,
        Some(&long_tmp),
        &marker,
    );
    let chosen = PathBuf::from(std::fs::read_to_string(&marker).unwrap().trim());
    assert!(
        chosen.as_os_str().len() <= SUN_PATH_LIMIT,
        "{} exceeds sun_path",
        chosen.display()
    );
    // Component-level check: the socket's parent is `/tmp/remuda-<uid>` (the
    // fixed root), not `<long_tmp>/remuda-<uid>` (the overflowing fallback).
    let parent = chosen.parent().unwrap();
    assert_eq!(
        parent.parent(),
        Some(Path::new("/tmp")),
        "must be under the fixed /tmp root: {}",
        chosen.display()
    );
    assert!(
        parent
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("remuda-")),
        "{parent:?}"
    );
    assert!(
        !parent.starts_with(&long_tmp),
        "must not be the long TMPDIR fallback: {}",
        chosen.display()
    );
    assert!(chosen.is_absolute());
    let _ = std::fs::remove_dir_all(&long_tmp);
}

#[test]
fn a_long_tmpdir_without_xdg_falls_back_to_fixed_tmp_root_child() {
    if std::env::var_os("REMUDA_RTD_CHILD").is_none() {
        return;
    }
    let long_tmp = PathBuf::from(std::env::var_os("TMPDIR").unwrap());
    let preferred = long_tmp
        .join("x".repeat(60))
        .join("instances/ins_x/hook.sock");
    let placement = place_socket(&preferred, "01990000-0000-7000-8000-0000000000aa.sock")
        .expect("the fixed /tmp root must always fit a uuid socket");
    // Prove the path is not just short on paper: it really binds.
    let listener = std::os::unix::net::UnixListener::bind(placement.bind_path()).unwrap();
    drop(listener);
    std::fs::write(
        marker_path(),
        placement.bind_path().as_os_str().as_encoded_bytes(),
    )
    .unwrap();
    let _ = std::fs::remove_file(placement.bind_path());
}

/// The start-up sweep removes dead sockets, keeps live ones, and never touches
/// non-socket files. Runs against the deterministic fixed candidate so no
/// environment mutation is needed.
#[test]
fn sweep_reclaims_dead_sockets_and_keeps_live_ones() {
    use remuda_signal::runtime_dir::{runtime_dir_candidates, sweep_dead_runtime_sockets};
    use std::os::unix::net::UnixListener;

    let probe = tempfile::tempdir().unwrap();
    let own_uid = std::fs::symlink_metadata(probe.path()).unwrap().uid();
    let fixed = runtime_dir_candidates(own_uid)
        .into_iter()
        .find(|path| path.parent() == Some(Path::new("/tmp")))
        .expect("fixed /tmp candidate");
    std::fs::create_dir_all(&fixed).unwrap();
    let unique = std::process::id();
    let dead = fixed.join(format!("sweep-dead-{unique}.sock"));
    let live = fixed.join(format!("sweep-live-{unique}.sock"));
    let note = fixed.join(format!("sweep-note-{unique}.txt"));
    let dead_listener = UnixListener::bind(&dead).unwrap();
    let live_listener = UnixListener::bind(&live).unwrap();
    std::fs::write(&note, b"keep").unwrap();
    drop(dead_listener);

    let removed = sweep_dead_runtime_sockets().unwrap();
    assert!(removed >= 1, "at least the planted dead socket is swept");
    assert!(!dead.exists(), "dead socket is reclaimed");
    assert!(live.exists(), "a live listener's socket is untouched");
    assert!(note.exists(), "non-socket files are untouched");
    drop(live_listener);
    let _ = std::fs::remove_file(&live);
    let _ = std::fs::remove_file(&note);
}

/// Purge unlinks the symlink's target, but only when it resolves inside one
/// of this process's own runtime directories.
#[test]
fn purge_unlink_only_removes_a_trusted_runtime_target() {
    use remuda_signal::runtime_dir::{runtime_dir_candidates, unlink_resolved_socket_link};
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;

    let root = tempfile::tempdir().unwrap();
    let own_uid = std::fs::symlink_metadata(root.path()).unwrap().uid();
    let fixed = runtime_dir_candidates(own_uid)
        .into_iter()
        .find(|path| path.parent() == Some(Path::new("/tmp")))
        .unwrap();
    std::fs::create_dir_all(&fixed).unwrap();
    let unique = std::process::id();

    // Trusted: a socket in the fixed runtime dir, linked from an instance dir.
    // The helper unlinks only the target; purging the instance directory (the
    // caller's job) removes the link itself.
    let target = fixed.join(format!("purge-real-{unique}.sock"));
    let listener = UnixListener::bind(&target).unwrap();
    let instance = root.path().join("instances/ins_1");
    std::fs::create_dir_all(&instance).unwrap();
    let link = instance.join("hook.sock");
    symlink(&target, &link).unwrap();
    unlink_resolved_socket_link(&link).unwrap();
    assert!(!target.exists(), "the real runtime socket is unlinked");
    assert!(
        link.is_symlink(),
        "the link itself is left for the directory purge to remove"
    );
    drop(listener);

    // Untrusted: a symlink pointing outside every runtime candidate is left
    // alone.
    let foreign = root.path().join("elsewhere.sock");
    let _foreign_listener = UnixListener::bind(&foreign).unwrap();
    let foreign_link = instance.join("evil.sock");
    symlink(&foreign, &foreign_link).unwrap();
    unlink_resolved_socket_link(&foreign_link).unwrap();
    assert!(foreign.exists(), "an untrusted target must survive");
    assert!(
        foreign_link.is_symlink(),
        "the untrusted link is left as-is"
    );
}

#[test]
fn short_paths_fit_and_long_paths_do_not() {
    assert!(path_fits_sun_path(Path::new("/tmp/remuda-1001/hook.sock")));
    // Exactly the safe threshold, then one byte past.
    assert!(path_fits_sun_path(Path::new(&"a".repeat(100))));
    assert!(!path_fits_sun_path(Path::new(&"a".repeat(101))));
    let long: PathBuf = Path::new("/tmp").join("x".repeat(110));
    assert!(!path_fits_sun_path(&long));
}

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
    assert!(text.contains(&format!("{} bytes", path.as_os_str().len())));
    assert!(text.contains(&format!("limit {SUN_PATH_LIMIT}")));
    assert!(text.contains("path must be shorter than SUN_LEN"));
}
