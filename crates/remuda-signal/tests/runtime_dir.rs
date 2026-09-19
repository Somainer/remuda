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

fn run_child(name: &str, xdg: Option<&Path>, marker: &Path) {
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
    let error = place_socket(&preferred, "ins-1.sock").unwrap_err();
    assert!(
        error.to_string().contains("no AF_UNIX placement fits"),
        "{error}"
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
