//! `remuda project` CLI wiring and its requests against the Hub.

use anyhow::Result;
use remuda_hub::{HubConfig, spawn};
use serde_json::Value;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_remuda")
}

#[tokio::test]
async fn project_help_lists_subcommands() -> Result<()> {
    let output = Command::new(bin()).args(["project", "--help"]).output()?;
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for sub in [
        "list",
        "show",
        "create",
        "set",
        "add-member",
        "remove-member",
    ] {
        assert!(stdout.contains(sub), "help must list `{sub}`: {stdout}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_create_list_show_set_and_member_cli_against_hub() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
    let token = hub.mint_device_token("project-cli").await?;
    let base = format!("http://{}", hub.addr);

    let output = Command::new(bin())
        .args([
            "project",
            "create",
            "--name",
            "cli-project",
            "--default-effort",
            "high",
            "--hub",
            &base,
            "--token",
            &token,
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let created: Value = serde_json::from_slice(&output.stdout)?;
    let project_id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["name"], "cli-project");
    assert_eq!(created["defaultEffort"], "high");
    assert_eq!(
        created["policy"]["configurable"]["maxDelegationDepth"], 3,
        "§2.5 default policy rides along"
    );

    let output = Command::new(bin())
        .args(["project", "list", "--hub", &base, "--token", &token])
        .output()?;
    assert!(output.status.success());
    let list: Value = serde_json::from_slice(&output.stdout)?;
    assert!(
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == project_id)
    );

    let output = Command::new(bin())
        .args([
            "project",
            "show",
            &project_id,
            "--hub",
            &base,
            "--token",
            &token,
        ])
        .output()?;
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout)?["id"],
        project_id
    );

    let output = Command::new(bin())
        .args([
            "project",
            "set",
            &project_id,
            "--name",
            "renamed",
            "--max-delegation-depth",
            "5",
            "--hub",
            &base,
            "--token",
            &token,
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let patched: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(patched["name"], "renamed");
    assert_eq!(patched["policy"]["configurable"]["maxDelegationDepth"], 5);

    // Unknown project → non-zero exit carrying the Hub's 404.
    let missing = remuda_protocol::ProjectId::new();
    let output = Command::new(bin())
        .args([
            "project",
            "show",
            missing.as_id().as_str(),
            "--hub",
            &base,
            "--token",
            &token,
        ])
        .output()?;
    assert!(!output.status.success());

    // --member shape validation happens client-side.
    let output = Command::new(bin())
        .args([
            "project",
            "create",
            "--name",
            "bad-member",
            "--member",
            "no-colon",
            "--hub",
            &base,
            "--token",
            &token,
        ])
        .output()?;
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("HOST:WORKSPACE")
            || String::from_utf8_lossy(&output.stdout).contains("HOST:WORKSPACE")
    );
    hub.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_member_cli_round_trip_requires_registered_workspace() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
    let token = hub.mint_device_token("project-cli-members").await?;
    let base = format!("http://{}", hub.addr);

    let output = Command::new(bin())
        .args([
            "project", "create", "--name", "members", "--hub", &base, "--token", &token,
        ])
        .output()?;
    assert!(output.status.success());
    let created: Value = serde_json::from_slice(&output.stdout)?;
    let project_id = created["id"].as_str().unwrap().to_string();

    // Hub refuses a member whose workspace is not registered on the host.
    let host = remuda_protocol::HostId::new();
    let workspace = remuda_protocol::WorkspaceId::new();
    let output = Command::new(bin())
        .args([
            "project",
            "add-member",
            &project_id,
            "--host",
            host.as_id().as_str(),
            "--workspace",
            workspace.as_id().as_str(),
            "--role",
            "build",
            "--hub",
            &base,
            "--token",
            &token,
        ])
        .output()?;
    assert!(!output.status.success(), "unknown host must fail");

    // Removing a member that never existed is also a failure.
    let output = Command::new(bin())
        .args([
            "project",
            "remove-member",
            &project_id,
            "--host",
            host.as_id().as_str(),
            "--workspace",
            workspace.as_id().as_str(),
            "--hub",
            &base,
            "--token",
            &token,
        ])
        .output()?;
    assert!(!output.status.success());
    hub.shutdown().await;
    Ok(())
}
