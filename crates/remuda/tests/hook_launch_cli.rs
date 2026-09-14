//! Hand-typed --settings uses the real shim and merger, preserving exec pid.
#![cfg(unix)]

use remuda_driver::{OverlayOptions, TuiMode, materialize_overlay, materialize_shims};
use serde_json::json;
use std::{
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
};

#[test]
fn explicit_settings_are_merged_before_exec_without_losing_user_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let launch = dir.path().join("launch");
    let overlay = materialize_overlay(&OverlayOptions {
        launch_dir: launch.clone(),
        relay_binary: env!("CARGO_BIN_EXE_remuda").into(),
        socket_path: dir.path().join("hook.sock"),
        tui: TuiMode::Default,
        base: None,
    })
    .unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(
        real.join("claude"),
        "#!/bin/sh\nprintf '%s\\n' \"$$\" \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(real.join("claude"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let pinned = real.join("claude-custom");
    std::fs::copy(real.join("claude"), &pinned).unwrap();
    let decoy = dir.path().join("decoy");
    std::fs::create_dir(&decoy).unwrap();
    std::fs::write(decoy.join("claude"), "#!/bin/sh\nexit 97\n").unwrap();
    std::fs::set_permissions(decoy.join("claude"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let user = json!({"env":{"CUSTOM":"keep"},"hooks":{"Stop":[{"hooks":[{"type":"command","command":"user-stop"}]}]}}).to_string();
    let source = dir.path().join("user settings.json");
    std::fs::write(&source, &user).unwrap();
    // The same Rust settings merger must run for PATH resolution and a pin
    // outside PATH. The pinned case puts a failing decoy on PATH deliberately.
    for binary in [None, Some(pinned.as_path())] {
        let shims = materialize_shims(&launch, &overlay.path, "fixture", false, binary).unwrap();
        let path_bin = if binary.is_some() { &decoy } else { &real };
        for settings in [
            vec![
                "--settings".to_owned(),
                source.to_string_lossy().into_owned(),
            ],
            vec![format!("--settings={user}")],
        ] {
            let child = Command::new(shims.bin_dir.join("claude"))
                .env("PATH", format!("{}:/usr/bin:/bin", path_bin.display()))
                .env("REMUDA_HOOK_RELAY", env!("CARGO_BIN_EXE_remuda"))
                .args(settings)
                .args([
                    "--model",
                    "claude-opus-5[1m]",
                    "--setting-sources",
                    "project",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let pid = child.id();
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{output:?}");
            let stdout = String::from_utf8(output.stdout).unwrap();
            let args: Vec<_> = stdout.lines().collect();
            assert_eq!(
                args[0],
                pid.to_string(),
                "the merger must exec, preserving foreground pid"
            );
            assert!(launch.join("shim-pids").join(pid.to_string()).is_file());
            assert_eq!(args[1], "--settings");
            let merged: serde_json::Value =
                serde_json::from_slice(&std::fs::read(args[2]).unwrap()).unwrap();
            assert_eq!(merged["env"]["CUSTOM"], "keep");
            assert_eq!(
                merged["hooks"]["Stop"][0]["hooks"][0]["command"],
                "user-stop"
            );
            for event in remuda_driver::launch::overlay::HOOK_EVENTS {
                assert!(
                    merged["hooks"][event].to_string().contains("hook emit"),
                    "{event}"
                );
            }
            assert_eq!(
                &args[3..],
                &[
                    "--model",
                    "claude-opus-5[1m]",
                    "--setting-sources",
                    "project"
                ]
            );
            assert_eq!(std::fs::read_to_string(&source).unwrap(), user);
        }
    }
}
