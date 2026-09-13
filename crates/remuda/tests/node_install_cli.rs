//! Installer diagnostics terminate before service mutation with an invalid Hub URL.
#![cfg(target_os = "macos")]

use std::time::Duration;
use tokio::{process::Command, time::timeout};

#[tokio::test]
async fn macos_install_warns_for_protected_workspaces_before_starting_a_service() {
    let fixture = tempfile::tempdir().unwrap();
    let home = fixture.path().join("home");
    let data = fixture.path().join("data");
    std::fs::create_dir(&home).unwrap();
    for folder in ["Documents", "Desktop", "Downloads"] {
        let output = timeout(
            Duration::from_secs(10),
            Command::new(env!("CARGO_BIN_EXE_remuda"))
                .current_dir(fixture.path())
                .env_clear()
                .env("HOME", &home)
                .env("PATH", "/usr/bin:/bin")
                .arg("--data-dir")
                .arg(&data)
                .args(["node", "install", "--launchd", "--workspace"])
                .arg(home.join(folder).join("workspace"))
                .args(["--hub", "invalid://fixture"])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("grant Full Disk Access"), "{stderr}");
        assert!(stderr.contains("System Settings → Privacy & Security"));
        assert!(stderr.contains(env!("CARGO_BIN_EXE_remuda")));
        assert!(stderr.contains("hub_url must use wss://"), "{stderr}");
    }
    assert!(!data.exists());
    assert!(!home.join("Library/LaunchAgents").exists());
}
