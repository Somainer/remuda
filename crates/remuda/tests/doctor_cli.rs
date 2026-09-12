//! Host preflight with synthetic executables and an ephemeral authenticated registry.
#![cfg(unix)]

use axum::{Json, Router, routing::get};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, time::Duration};
use tokio::{process::Command, time::timeout};

#[tokio::test(flavor = "multi_thread")]
async fn local_doctor_uses_config_reports_versions_and_returns_nonzero_on_blockers() {
    let keep = tempfile::tempdir().unwrap();
    let bin = keep.path().join("bin");
    let data = keep.path().join("data");
    let home = keep.path().join("home");
    for dir in [&bin, &data, &home] {
        std::fs::create_dir(dir).unwrap();
    }
    for name in [
        "claude", "codex", "grok", "agy", "herdr", "cargo", "pnpm", "df",
    ] {
        let script = if name == "df" {
            "#!/bin/sh\nprintf 'Filesystem Blocks Used Available Capacity Mounted\\nfixture 9000000 0 8000000 0%% /\\n'\n".into()
        } else {
            format!("#!/bin/sh\nprintf '{name} doctor-fixture-1.0\\n'\n")
        };
        let path = bin.join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new().route(
        "/v1/hosts",
        get(|headers: axum::http::HeaderMap| async move {
            assert_eq!(headers["authorization"], "Bearer fixture-device");
            Json(json!({"items":[]}))
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let config = keep.path().join("remuda.toml");
    std::fs::write(
        &config,
        format!("[hub]\nlisten = '{address}'\n[node]\nlisten = '127.0.0.1:0'\n"),
    )
    .unwrap();
    let command = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_remuda"));
        cmd.env_clear()
            .env("PATH", &bin)
            .env("HOME", &home)
            .env("REMUDA_TOKEN", "fixture-device")
            .arg("--config")
            .arg(&config)
            .arg("--data-dir")
            .arg(&data)
            .args(["doctor", "--local"])
            .kill_on_drop(true);
        cmd
    };
    let output = timeout(Duration::from_secs(15), command().arg("--json").output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["exitCode"], 0);
    for name in [
        "binary.claude",
        "binary.codex",
        "binary.grok",
        "binary.agy",
        "binary.herdr",
        "binary.cargo",
        "binary.pnpm",
        "hub.reachability",
        "port.hub",
    ] {
        assert!(
            report["checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|check| check["name"] == name && check["status"] == "ok"),
            "{name}: {report}"
        );
    }
    assert!(!data.join("enrollment.json").exists());
    let human = timeout(Duration::from_secs(15), command().output())
        .await
        .unwrap()
        .unwrap();
    assert!(human.status.success());
    assert!(String::from_utf8_lossy(&human.stdout).contains("doctor-fixture-1.0"));
    assert!(String::from_utf8_lossy(&human.stdout).contains("doctor: no blockers"));
    std::fs::write(
        data.join("enrollment.json"),
        "malformed-identity-do-not-disclose",
    )
    .unwrap();
    let blocked = timeout(Duration::from_secs(15), command().arg("--json").output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(blocked.status.code(), Some(1));
    assert!(
        !String::from_utf8_lossy(&blocked.stdout).contains("malformed-identity-do-not-disclose")
    );
    let report: Value = serde_json::from_slice(&blocked.stdout).unwrap();
    assert_eq!(report["exitCode"], 1);
    server.abort();
}
