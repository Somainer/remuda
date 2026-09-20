//! Node-side registration of the `claude-sdk` carrier (`print-replacement.md`
//! §2.6, §3 batch 3).
//!
//! Three properties, and the third is the one that keeps D-035 intact:
//!
//! 1. `(claude, claude-sdk)` is accepted, because the matrix arm and the factory
//!    are both present;
//! 2. `(claude, shell-pty)` is unchanged — adding a carrier must not disturb the
//!    default one;
//! 3. an omitted driver resolves to **neither** print nor sdk. sdk is explicit
//!    only; no default or fallback path may reach it.

use remuda_node::{
    CreateInstanceRequest, DevServerConfig, LocalDrivers, NativeDriverConfig, ServeConfig, compose,
};
use remuda_protocol::{AgentKind, DriverKind};

fn loopback_config(root: &std::path::Path) -> DevServerConfig {
    let mut http = DevServerConfig::loopback(0);
    http.workspace_root = root.join("workspace");
    http.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    std::fs::create_dir_all(&http.workspace_root).expect("workspace");
    http
}

fn native_config(data: &std::path::Path) -> ServeConfig {
    ServeConfig {
        http: loopback_config(data),
        data_dir: data.to_path_buf(),
        drivers: LocalDrivers::Native(NativeDriverConfig::new(data.to_path_buf())),
    }
}

fn create_req(kind: AgentKind, driver: DriverKind) -> CreateInstanceRequest {
    CreateInstanceRequest {
        origin: remuda_protocol::InputOrigin::Human,
        agent_credential: None,
        command_id: None,
        instance_id: None,
        host_id: None,
        workspace_id: None,
        kind,
        driver,
        model: "fake".to_owned(),
        args: Vec::new(),
        provider_profile_id: "dev-fake".to_owned(),
        permission_mode: "manual".to_owned(),
        sandbox: None,
        prompt: String::new(),
        cwd: None,
        delegation: None,
        settings_overlay_path: None,
        claude_config_dir: None,
        binary_path: None,
        binary_sha256: None,
        max_budget_usd: None,
        provider_overlay: None,
        provider_auth_token: None,
        resume_session_id: None,
        resumed_from: None,
        effort: None,
        tui: None,
        extra_env: std::collections::BTreeMap::new(),
        capabilities: Default::default(),
        api_route: None,
        api_relay_endpoint: None,
    }
}

/// The kind/driver matrix takes the new pair, keeps the default one, and still
/// refuses a driver that speaks a different native product.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_sdk_is_accepted_and_shell_pty_is_unchanged() {
    let data = tempfile::tempdir().expect("data dir");
    let node = compose(&native_config(data.path())).expect("compose");

    let sdk = node
        .create_instance(create_req(AgentKind::Claude, DriverKind::ClaudeSdk))
        .await
        .expect("(claude, claude-sdk) must be creatable");
    assert_eq!(sdk.instance.driver, DriverKind::ClaudeSdk);
    node.submit_command(
        &sdk.instance.meta.id,
        serde_json::from_value(serde_json::json!({"operation": "close"})).expect("close"),
    )
    .await
    .expect("close sdk");

    // Adding a carrier must not disturb the default one (D-028).
    let pty = node
        .create_instance(create_req(AgentKind::Claude, DriverKind::ShellPty))
        .await
        .expect("(claude, shell-pty) must stay creatable");
    assert_eq!(pty.instance.driver, DriverKind::ShellPty);
    node.submit_command(
        &pty.instance.meta.id,
        serde_json::from_value(serde_json::json!({"operation": "close"})).expect("close"),
    )
    .await
    .expect("close pty");

    // The invariant the matrix exists for: sdk is a Claude carrier only.
    for kind in [AgentKind::Codex, AgentKind::Grok, AgentKind::Terminal] {
        let mismatched = node
            .create_instance(create_req(kind, DriverKind::ClaudeSdk))
            .await;
        assert!(
            mismatched.is_err(),
            "{kind:?} on claude-sdk is not one native product and must be refused"
        );
    }
}

/// D-035: `claude-sdk` is explicit only. Registering the factory must not put
/// sdk on any path a caller can reach without naming it.
///
/// The Node's own `driver` default is `claude-print` (`model.rs`), and the
/// carrier that answers an omitted driver on the Hub side is `shell-pty` then
/// herdr `claude-pty` (`workers::select_carrier`, which never selects print or
/// sdk). This test pins the half this batch could have broken: the request
/// default is untouched, so nothing silently became sdk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_omitted_driver_never_resolves_to_sdk() {
    // No `driver` key: whatever answers, it must not be the new carrier.
    let defaulted: CreateInstanceRequest = serde_json::from_value(serde_json::json!({
        "kind": "claude",
        "model": "fake",
        "permissionMode": "manual",
        "prompt": ""
    }))
    .expect("request");
    assert_ne!(
        defaulted.driver,
        DriverKind::ClaudeSdk,
        "an omitted driver must never silently become claude-sdk"
    );
    // And the pre-existing default is unchanged by this batch.
    assert_eq!(defaulted.driver, DriverKind::ClaudePrint);

    // Naming it is the only way to get it, and that path works.
    let explicit: CreateInstanceRequest = serde_json::from_value(serde_json::json!({
        "kind": "claude",
        "driver": "claude-sdk",
        "model": "fake",
        "permissionMode": "manual",
        "prompt": ""
    }))
    .expect("request");
    assert_eq!(explicit.driver, DriverKind::ClaudeSdk);

    let data = tempfile::tempdir().expect("data dir");
    let node = compose(&native_config(data.path())).expect("compose");
    let created = node
        .create_instance(explicit)
        .await
        .expect("an explicit --driver claude-sdk must create");
    assert_eq!(created.instance.driver, DriverKind::ClaudeSdk);
    node.submit_command(
        &created.instance.meta.id,
        serde_json::from_value(serde_json::json!({"operation": "close"})).expect("close"),
    )
    .await
    .expect("close");
}
