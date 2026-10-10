//! c-bootstrap-dev round 7 item 3: the production `remuda hub` binary must
//! REFUSE to start when the configured bootstrap access code is an EMPTY
//! value — an empty `REMUDA_BOOTSTRAP_TOKEN` env var or an empty
//! `bootstrapToken = "file:…"` file — instead of silently minting a hub-owned
//! code (which would also accept empty-string logins). Drives the real
//! binary; a watchdog kills a hub that wrongly starts serving and fails the
//! test.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_remuda")
}

/// Hermetic environment: remove EVERY REMUDA_* the calling shell exported so
/// the spawned hub can never serve against an ambient data dir or pick up an
/// ambient bootstrap source (c-bootstrap-dev r8 item 4). Callers then add back
/// exactly the vars the case needs.
fn strip_remuda_env(cmd: &mut Command) -> &mut Command {
    let keys: Vec<OsString> = std::env::vars_os()
        .filter_map(|(k, _)| {
            if k.to_string_lossy().starts_with("REMUDA_") {
                Some(k)
            } else {
                None
            }
        })
        .collect();
    for k in keys {
        cmd.env_remove(k);
    }
    cmd
}

/// Spawn the hub and wait up to 15 s for it to EXIT. A hub that incorrectly
/// treats the empty source as "no code" binds the listener and serves forever;
/// the watchdog kills it and panics.
fn refuse_or_fail(mut child: std::process::Child, label: &str) -> std::process::Output {
    let deadline = Instant::now() + Duration::from_secs(15);
    let exited = loop {
        if child.try_wait().expect("poll hub process").is_some() {
            break true;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break false;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        exited,
        "{label}: the hub started serving instead of refusing"
    );
    child.wait_with_output().expect("collect hub output")
}

fn assert_refused(output: std::process::Output, data_dir: &Path, label: &str) {
    assert!(
        !output.status.success(),
        "{label}: startup must fail: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("resolved to an empty value"),
        "{label}: stderr names the empty resolved value, not just the test file name: {stderr}"
    );
    assert!(
        !data_dir.join("bootstrap-token").exists(),
        "{label}: a refused start never mints a token"
    );
    assert!(
        !data_dir.join("bootstrap-token-source-explicit").exists(),
        "{label}: a refused start never writes the marker"
    );
}

/// Empty env present: the CLI resolves SecretRef::Env, trims to an empty
/// secret, and refuses before the Hub spawns/mints.
#[test]
fn empty_bootstrap_env_refuses_to_start() {
    let dir = tempfile::tempdir().expect("data tempdir");
    let mut cmd = Command::new(bin());
    strip_remuda_env(&mut cmd);
    let child = cmd
        .args(["hub", "--listen", "127.0.0.1:0"])
        .env("REMUDA_DATA_DIR", dir.path())
        .env("REMUDA_BOOTSTRAP_TOKEN", "")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn remuda hub with empty env");
    let output = refuse_or_fail(child, "empty REMUDA_BOOTSTRAP_TOKEN");
    assert_refused(output, dir.path(), "empty REMUDA_BOOTSTRAP_TOKEN");
}

/// Empty file: `bootstrapToken = "file:…"` pointing at a whitespace-only file
/// refuses at SecretRef::resolve with the file context.
#[test]
fn empty_bootstrap_file_refuses_to_start() {
    let dir = tempfile::tempdir().expect("data tempdir");
    let code_file = dir.path().join("empty-access-code");
    std::fs::write(&code_file, "  \n\t ").expect("write an empty/whitespace code file");
    let config: PathBuf = dir.path().join("hub.toml");
    std::fs::write(
        &config,
        format!(
            "data_dir = {:?}\n[hub]\nlisten = \"127.0.0.1:0\"\nbootstrapToken = \"file:{}\"\n",
            dir.path(),
            code_file.display()
        ),
    )
    .expect("write hub.toml");

    let mut cmd = Command::new(bin());
    strip_remuda_env(&mut cmd);
    let child = cmd
        .args(["hub", "--listen", "127.0.0.1:0"])
        .env("REMUDA_CONFIG", &config)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn remuda hub with an empty code file");
    let output = refuse_or_fail(child, "empty bootstrapToken file");
    assert_refused(output, dir.path(), "empty bootstrapToken file");
}
