//! Round 4 item 2: the `serve` example must refuse an EMPTY
//! REMUDA_BOOTSTRAP_TOKEN instead of tagging it ExplicitEnv (the library would
//! then either accept empty-string logins on a fresh dir or overwrite a
//! previously persisted real code). Drives the built example binary.

use std::path::PathBuf;
use std::process::Command;

/// Locate the built `serve` example next to this test binary
/// (`<target>/<profile>/examples/serve`, test exe lives in `deps/`).
fn serve_bin() -> PathBuf {
    let mut path = std::env::current_exe().expect("test exe");
    path.pop(); // deps/
    path.pop(); // <profile>/
    path.push("examples");
    path.push("serve");
    path
}

#[test]
fn serve_refuses_an_empty_bootstrap_env() {
    let data_dir = tempfile::tempdir().expect("data tempdir");
    let output = Command::new(serve_bin())
        .env("REMUDA_DATA_DIR", data_dir.path())
        .env("REMUDA_LISTEN", "127.0.0.1:0")
        .env("REMUDA_COOKIE_SECURE", "0")
        .env("REMUDA_BOOTSTRAP_TOKEN", "")
        .output()
        .expect("run serve example");
    assert!(
        !output.status.success(),
        "an empty REMUDA_BOOTSTRAP_TOKEN must fail startup"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must not be empty"),
        "stderr explains the refusal: {stderr}"
    );
    // Nothing was written: no token, no stamp, no provenance marker.
    assert!(!data_dir.path().join("bootstrap-token").exists());
    assert!(!data_dir.path().join("bootstrap-issued-at").exists());
    assert!(
        !data_dir
            .path()
            .join("bootstrap-token-source-explicit")
            .exists()
    );
}
