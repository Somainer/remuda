//! Cross-batch interlock: the `computer-use` row this task *produces* is the
//! row `c-cua-launch`'s host gate *consumes* (D-045 §2/§4).
//!
//! Each batch tests its own half against a hand-written fixture: the launch
//! gate is fed a literal `CliEntry`, and the probe is asserted on its own
//! fields. Neither proves the two agree at the seam, which is the failure this
//! file exists to catch — a row that looks right in isolation but is refused
//! by the gate (or, worse, silently accepted) once it travels between them.
//!
//! So these tests drive the real producer: a fake `CODEX_HOME` is laid down on
//! disk, the probe runs over it, and its output is handed straight to the
//! gate. Both are the production functions, not restatements of them.

use remuda_node::{
    CliEntry, CollectRequest, Collector, ProbeEnv, computer_use_evaluate, computer_use_plist_path,
};
use remuda_protocol::AgentKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Lay down a vendor bundle under `root`, with `body` as the client script.
///
/// The script exits non-zero and writes nothing: if the probe ever executed
/// it, `installed` would still be true (the stat is what matters) but the
/// version would be missing — the no-exec property itself is pinned by the
/// probe's own unit test.
fn fake_bundle(root: &Path, version: &str) {
    let client = remuda_node::computer_use_client_path(&probe_env(root));
    std::fs::create_dir_all(client.parent().unwrap()).unwrap();
    std::fs::write(&client, "#!/bin/sh\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&client, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let plist = computer_use_plist_path(&probe_env(root));
    std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
    std::fs::write(
        &plist,
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleShortVersionString</key><string>{version}</string>
</dict></plist>
"#
        ),
    )
    .unwrap();
}

/// A probe env whose `CODEX_HOME` is `root` and which resolves no PATH CLIs.
fn probe_env(root: &Path) -> ProbeEnv {
    let empty_path: PathBuf = root.join("no-binaries");
    ProbeEnv {
        path: empty_path.into_os_string(),
        home: root.join("home"),
        hostname: Some("interop".to_owned()),
        herdr_socket_env: None,
        xdg_config_home: None,
        codex_home: Some(root.to_path_buf()),
    }
}

/// The probe's real `computer-use` row for a host with `root` as `CODEX_HOME`.
fn probed_row(root: &Path) -> CliEntry {
    let snapshot = Collector::new(probe_env(root), Duration::from_secs(30))
        .snapshot(&CollectRequest::default());
    snapshot
        .cli
        .into_iter()
        .find(|entry| entry.kind == "computer-use")
        .expect("the probe always reports a computer-use row")
}

/// The producer's row, unchanged, satisfies the consumer — the interlock.
///
/// This is the assertion neither batch could make alone: it fails if the probe
/// stops emitting the row, if the row's `kind` string drifts from the gate's
/// constant, if `installed` stops being set for a present bundle, or if the
/// gate's macOS spelling stops matching `std::env::consts::OS`.
#[test]
fn the_probed_row_satisfies_the_launch_gate() {
    let root = tempfile::tempdir().unwrap();
    fake_bundle(root.path(), "2.7.0");
    let row = probed_row(root.path());
    assert!(row.installed, "a present bundle must report installed");

    for kind in [AgentKind::Claude, AgentKind::Codex] {
        let verdict = computer_use_evaluate(&kind, "macos", Some(&row));
        assert!(
            verdict.is_ok(),
            "the probe's own row must pass the launch gate for {kind:?}: {verdict:?}"
        );
    }
}

/// The absence path travels too: the same row on this (non-macOS) host is
/// refused by the os check before the row is even consulted, and a row the
/// probe marks absent is refused by the installed check naming its path.
#[test]
fn the_probed_absence_states_are_refused_by_the_gate() {
    let empty = tempfile::tempdir().unwrap();
    let absent = probed_row(empty.path());
    assert!(!absent.installed, "no bundle means not installed");
    assert!(absent.path.is_none(), "and no path is offered");

    // On this build host the gate refuses on os first; the refusal is a named
    // error either way, never a silent pass.
    let here = std::env::consts::OS;
    let verdict = computer_use_evaluate(&AgentKind::Claude, here, Some(&absent));
    assert!(verdict.is_err(), "a non-macOS host must refuse");

    // On a (simulated) Mac, the same absent row is refused for absence, and
    // the message is the one the contract's refusal table requires.
    let on_mac = computer_use_evaluate(&AgentKind::Claude, "macos", Some(&absent));
    let message = format!("{on_mac:?}");
    assert!(
        message.contains("not installed"),
        "absence must be named as such: {message}"
    );
}

/// A host whose Node predates this row reports nothing: the gate refuses with
/// "not reported" rather than mistaking silence for capability.
#[test]
fn no_row_is_not_reported_rather_than_unsupported() {
    let verdict = computer_use_evaluate(&AgentKind::Claude, "macos", None);
    let message = format!("{verdict:?}");
    assert!(
        message.contains("has not reported"),
        "an unreported row must read as unreported: {message}"
    );
}
