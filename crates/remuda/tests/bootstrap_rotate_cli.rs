//! c-bootstrap-dev round 2 (item 3): the `remuda hub rotate-bootstrap` CLI
//! honours the `dev-hub/` layout and refuses (non-zero, token + stamp
//! untouched) when the persisted token is governed by an explicit access-code
//! file/env. Drives the real binary.

use std::fs;
use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_remuda")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path)
        .expect("read file")
        .trim()
        .to_string()
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// Rotate changes only `dev-hub/bootstrap-token`; the outer dir is untouched.
#[test]
fn rotate_bootstrap_cli_rotates_dev_hub_only() {
    let outer = tempfile::tempdir().unwrap();
    let dev_hub = outer.path().join("dev-hub");
    let token_path = dev_hub.join("bootstrap-token");
    let stamp_path = dev_hub.join("bootstrap-issued-at");
    let outer_token = outer.path().join("bootstrap-token");

    // A hub-generated token: token + stamp present, NO explicit marker.
    write(&token_path, "old-hub-generated-code-12345");
    write(&stamp_path, "2000-01-01T00:00:00.000Z");

    let output = Command::new(bin())
        .args(["hub", "rotate-bootstrap", "--data-dir"])
        .arg(outer.path())
        .output()
        .expect("run rotate-bootstrap");
    assert!(
        output.status.success(),
        "expected success, got: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let new_code = String::from_utf8(output.stdout)
        .expect("utf8 stdout")
        .trim()
        .to_string();
    assert_ne!(new_code, "old-hub-generated-code-12345");
    assert!(new_code.len() >= 16, "new code looks minted: {new_code}");

    // Only dev-hub's token changes…
    assert_eq!(read(&token_path), new_code);
    // …a fresh stamp is written (newer than 2000)…
    assert_ne!(read(&stamp_path), "2000-01-01T00:00:00.000Z");
    // …and the OUTER dir receives nothing.
    assert!(!outer_token.exists(), "outer dir must be untouched");
}

/// Rotation refuses (non-zero) with token AND stamp unchanged when the
/// explicit-source provenance marker is present.
#[test]
fn rotate_bootstrap_cli_refuses_explicit_source_without_touching_token() {
    let outer = tempfile::tempdir().unwrap();
    let dev_hub = outer.path().join("dev-hub");
    let token_path = dev_hub.join("bootstrap-token");
    let stamp_path = dev_hub.join("bootstrap-issued-at");
    let marker = dev_hub.join("bootstrap-token-source-explicit");

    // Deliberately no trailing newline: raw-byte equality must hold exactly.
    write(&token_path, "operator-file-code-abcdef12345");
    write(&stamp_path, "2000-01-01T00:00:00.000Z");
    write(&marker, ""); // explicit-source provenance

    // Snapshot the raw bytes BEFORE the refusal.
    let token_before = fs::read(&token_path).unwrap();
    let stamp_before = fs::read(&stamp_path).unwrap();
    let marker_before = fs::read(&marker).unwrap();

    let output = Command::new(bin())
        .args(["hub", "rotate-bootstrap", "--data-dir"])
        .arg(outer.path())
        .output()
        .expect("run rotate-bootstrap");
    assert!(
        !output.status.success(),
        "rotation of an explicit source must fail, got: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("explicit") || stderr.contains("access-code"),
        "stderr explains the refusal: {stderr}"
    );

    // Token, stamp, and the marker are byte-for-byte unchanged (no trim/re-write
    // hiding a content-preserving rewrite).
    assert_eq!(fs::read(&token_path).unwrap(), token_before);
    assert_eq!(fs::read(&stamp_path).unwrap(), stamp_before);
    assert_eq!(fs::read(&marker).unwrap(), marker_before);
    assert_eq!(token_before, b"operator-file-code-abcdef12345");
    assert_eq!(stamp_before, b"2000-01-01T00:00:00.000Z");
}

/// Rotation also refuses when there is no persisted token at all (a hub
/// running purely off an explicit file leaves nothing to rotate).
#[test]
fn rotate_bootstrap_cli_refuses_without_persisted_token() {
    let outer = tempfile::tempdir().unwrap();
    let dev_hub = outer.path().join("dev-hub");
    fs::create_dir_all(&dev_hub).unwrap();

    let output = Command::new(bin())
        .args(["hub", "rotate-bootstrap", "--data-dir"])
        .arg(outer.path())
        .output()
        .expect("run rotate-bootstrap");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no bootstrap-token"),
        "expected missing-token refusal"
    );
}
