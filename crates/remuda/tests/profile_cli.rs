//! `remuda profile` CLI golden wiring: catalog, declare, event, probe.
//!
//! No real models — a secret-less gateway profile is created, supply is
//! declared, a synthetic 429 is reported, and `probe` returns the ranked
//! "what would run where" JSON without spawning anything.

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use serde_json::Value;
use std::process::Command;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

async fn enroll_fake_node(
    hub: &remuda_hub::RunningHub,
    host_id: &str,
    max_instances: u64,
) -> Result<tokio::task::JoinHandle<()>> {
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await?;
    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    node.send(Message::Text(
        serde_json::json!({
            "jsonrpc": "2.0", "id": "hello", "method": "runtime.hello",
            "params": {
                "hostId": host_id, "nodeVersion": "0.1.0-test", "label": "fake-node",
                "host": { "hostname": format!("{host_id}.local"), "maxInstances": max_instances }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    Ok(tokio::spawn(async move {
        while let Some(Ok(msg)) = node.next().await {
            if let Message::Text(text) = msg
                && let Ok(frame) = serde_json::from_str::<Value>(&text)
                && let Some(id) = frame.get("id").cloned()
            {
                let _ = node
                    .send(Message::Text(
                        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } })
                            .to_string()
                            .into(),
                    ))
                    .await;
            }
        }
    }))
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_remuda")
}

#[tokio::test]
async fn profile_help_lists_subcommands() -> Result<()> {
    let output = Command::new(bin()).args(["profile", "--help"]).output()?;
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for sub in [
        "catalog", "list", "show", "declare", "event", "usage", "probe",
    ] {
        assert!(stdout.contains(sub), "help must list `{sub}`: {stdout}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_declare_event_and_probe_dry_run_against_hub() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
    let token = hub.mint_device_token("profile-cli").await?;
    let base = format!("http://{}", hub.addr);
    let run = |args: &[&str]| Command::new(bin()).args(args).output();

    // Seed an existing profile over REST: `profile declare` only edits a
    // profile's supply, and profile creation stays on the providers surface.
    let body = serde_json::json!({
        "name": "cli-native",
        "kind": "native",
        "models": [
            {"id":"gw/es1[1m]","family":"es1","role":"workhorse","priority":20,
             "fallback":["gw/seed[1m]"]},
            {"id":"gw/seed[1m]","family":"seed","role":"workhorse","priority":18}
        ]
    });
    let client = remuda_hub_client::HubClient::new(&base, Some(token.clone()), None)?;
    let profile = client.post("/v1/providers", &body).await?;
    let profile_id = profile["id"].as_str().unwrap().to_string();

    // Enroll one fake node so host placement passes; the supply solver picks
    // the model regardless of which host.
    let host_id = remuda_protocol::HostId::new();
    let _node = enroll_fake_node(&hub, host_id.as_id().as_str(), 8).await?;

    // 1. catalog
    let out = run(&["profile", "catalog", "--hub", &base, "--token", &token])?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let catalog: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(
        catalog["revision"],
        remuda_hub::CATALOG_REVISION,
        "catalog revision must match the built-in catalog"
    );

    // 2. declare priority + concurrency + a family window
    let out = run(&[
        "profile",
        "declare",
        &profile_id,
        "--priority",
        "20",
        "--concurrency-max",
        "4",
        "--window",
        "primary:*:300",
        "--hub",
        &base,
        "--token",
        &token,
    ])?;
    assert!(
        out.status.success(),
        "declare: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let declared: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(declared["supply"]["priority"], 20);
    assert_eq!(declared["supply"]["concurrency"]["max"], 4);
    assert_eq!(
        declared["supply"]["windows"][0]["appliesTo"][0], "*",
        "declared window shape: {}",
        declared
    );

    // 3. show reflects the declaration
    let out = run(&[
        "profile",
        "show",
        &profile_id,
        "--hub",
        &base,
        "--token",
        &token,
    ])?;
    assert!(out.status.success());
    let shown: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(shown["supply"]["priority"], 20);

    // 4. report the 09-14 text: es1 429
    let out = run(&[
        "profile",
        "event",
        &profile_id,
        "--type",
        "textual",
        "--model",
        "gw/es1[1m]",
        "--text",
        "Request rejected (429) {\"error_code\":-2001}",
        "--hub",
        &base,
        "--token",
        &token,
    ])?;
    assert!(
        out.status.success(),
        "event: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(after["supply"]["state"], "degraded");
    let windows = after["supply"]["windows"].as_array().unwrap();
    assert!(
        windows
            .iter()
            .any(|w| w["appliesTo"][0] == "es1" && w["cooldownUntil"].is_number()),
        "es1 family window cooling: {after}"
    );

    // 5. a 529 leaves that family window in place and adds no new cooldown
    let before_count = windows.len();
    let out = run(&[
        "profile",
        "event",
        &profile_id,
        "--type",
        "textual",
        "--http-status",
        "529",
        "--text",
        "529 overloaded",
        "--hub",
        &base,
        "--token",
        &token,
    ])?;
    assert!(out.status.success());
    let after529: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(
        after529["supply"]["windows"].as_array().unwrap().len(),
        before_count
    );
    assert_eq!(after529["supply"]["state"], "degraded");

    // 6. probe dry-run: es1 cooled, seed sibling wins, deferred=false
    let out = run(&[
        "profile",
        "probe",
        "--min-class",
        "workhorse",
        "--hub",
        &base,
        "--token",
        &token,
    ])?;
    assert!(
        out.status.success(),
        "probe: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let decision: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(decision["chosen"]["modelId"], "gw/seed[1m]");
    assert_eq!(decision["chosen"]["family"], "seed");
    assert_eq!(decision["deferred"], false);
    assert!(decision["rejected"].as_array().unwrap().iter().any(|r| {
        r["modelId"] == "gw/es1[1m]"
            && r["reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x.as_str().unwrap_or_default().contains("cooling"))
    }));

    // 7. frontier probe is an EXPLICIT deferred decision (dry-run still 200):
    // no silent downgrade — chosen is null, deferred=true with reasons.
    let out = run(&[
        "profile",
        "probe",
        "--min-class",
        "frontier",
        "--hub",
        &base,
        "--token",
        &token,
    ])?;
    assert!(
        out.status.success(),
        "probe stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let deferred: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(deferred["deferred"], true);
    assert!(deferred["chosen"].is_null());
    assert!(
        deferred["reasons"].as_array().unwrap().iter().any(|r| r
            .as_str()
            .unwrap_or_default()
            .contains("deferred, not downgraded")),
        "{deferred}"
    );

    // 8. list shows the profile with its supply envelope attached.
    let out = run(&["profile", "list", "--hub", &base, "--token", &token])?;
    assert!(out.status.success());
    let list: Value = serde_json::from_slice(&out.stdout)?;
    assert!(
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == profile_id)
    );

    hub.shutdown().await;
    Ok(())
}
