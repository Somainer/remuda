//! c-effortread r13 item 6 (r12 item 4b): missing and invalid JSON ultracode
//! booleans must be refused through the PRODUCTION configure dispatch —
//! web/Hub wire JSON → Node `with_configure` parse → `submit_command` → the
//! PTY worker → `NativeAdapter` → the real shell-pty driver's pre-dispatch
//! gate — not only via JSON parsing (`model.rs`), the classifier (`effort.rs`)
//! or a hand-built `DriverInput` (`shell_pty_fake_send.rs`).
//!
//! An explicit flag-axis configure whose boolean is absent, a string, a
//! number or null reaches the driver as `effort_axis = Flag` +
//! `effort_ultracode = None`; the driver refuses it (CapabilityUnsupported →
//! a Rejected command) on every build, before it could type anything.
#![cfg(unix)]

use remuda_node::{
    CreateInstanceRequest, DevServerConfig, InstanceCommandRequest, LocalDrivers,
    NativeDriverConfig, ServeConfig, compose,
};
use remuda_protocol::{AgentKind, CommandState, DriverKind, Knowledge, SettlementOutcome};
use remuda_testing::ensure_workspace_bin;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

fn native_config(root: &Path, binary: &Path, scenario: &Path) -> ServeConfig {
    let mut http = DevServerConfig::loopback(0);
    http.workspace_root = root.join("workspace");
    http.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    std::fs::create_dir_all(&http.workspace_root).expect("workspace");
    let mut drivers = NativeDriverConfig::new(root.join("node-data"));
    drivers.pty_hooks = true;
    // The relay this test's overlay invokes: reuse the workspace-built remuda
    // instead of triggering an in-test cargo build.
    drivers.relay_binary = Some(ensure_workspace_bin("remuda").into());
    drivers.claude_binary = Some(binary.to_path_buf());
    drivers
        .extra_env
        .insert("FAKE_HARNESS_DIALECT_VERSION".into(), "modern".into());
    drivers.extra_env.insert(
        "FAKE_HARNESS_SCRIPT".into(),
        scenario.to_string_lossy().into_owned(),
    );
    ServeConfig {
        http,
        data_dir: root.join("node-data"),
        drivers: LocalDrivers::Native(drivers),
    }
}

fn create_req(binary: &Path, home: &Path) -> CreateInstanceRequest {
    CreateInstanceRequest {
        origin: remuda_protocol::InputOrigin::Human,
        agent_credential: None,
        command_id: None,
        instance_id: None,
        host_id: None,
        workspace_id: None,
        kind: AgentKind::Claude,
        driver: DriverKind::ShellPty,
        model: "fake".to_owned(),
        args: Vec::new(),
        provider_profile_id: "dev-fake".to_owned(),
        permission_mode: "dontAsk".to_owned(),
        binary_path: Some(binary.to_string_lossy().into_owned()),
        binary_sha256: None,
        sandbox: None,
        prompt: String::new(),
        cwd: None,
        delegation: None,
        settings_overlay_path: None,
        claude_config_dir: Some(home.to_string_lossy().into_owned()),
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

/// Install the scripted fake Claude as `<root>/bin/claude`.
fn install_fake_claude(root: &Path) -> std::path::PathBuf {
    let install = root.join("bin");
    std::fs::create_dir_all(&install).expect("bin dir");
    let dest = install.join("claude");
    std::fs::copy(ensure_workspace_bin("fake-harness"), &dest).expect("copy fake claude");
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    dest
}

/// Wrapper: `unsafe`/`set_var` is forbidden workspace-wide, so re-exec this
/// test binary with the native carrier enabled and run the real assertion
/// (ignored by default) — the same pattern as the gate carrier tests.
#[test]
fn missing_and_invalid_flag_booleans_are_refused_through_the_production_dispatch() {
    let exe = std::env::current_exe().expect("test binary");
    let output = std::process::Command::new(exe)
        .args([
            "--exact",
            "missing_and_invalid_flag_booleans_are_refused_through_the_production_dispatch_impl",
            "--ignored",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env("REMUDA_PTY_CARRIER", "native")
        .output()
        .expect("re-exec under the native carrier");
    assert!(
        output.status.success(),
        "native-carrier configure dispatch failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "the inner dispatch test did not run"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "runs under REMUDA_PTY_CARRIER=native via the wrapper test"]
async fn missing_and_invalid_flag_booleans_are_refused_through_the_production_dispatch_impl() {
    // Every bad JSON spelling the Hub could send for an explicit flag axis.
    let bad_effort_payloads = [
        json!({ "name": "high", "axis": "flag" }),
        json!({ "name": "high", "axis": "flag", "ultracode": "on" }),
        json!({ "name": "high", "axis": "flag", "ultracode": 1 }),
        json!({ "name": "high", "axis": "flag", "ultracode": null }),
    ];

    let root_dir = tempfile::tempdir().expect("data dir");
    let root = root_dir.path();
    let binary = install_fake_claude(root);
    // A turn that never matches anything we send: the scripted composer stays
    // idle, which is all the flag gate needs (it refuses before typing).
    let scenario = root.join("idle.json");
    std::fs::write(
        &scenario,
        r#"{"turns":[{"match_prefix":"NEVER_MATCHED","text":"unused"}]}"#,
    )
    .expect("scenario");
    let home = root.join("claude-home");
    std::fs::create_dir_all(&home).expect("home");

    let node = compose(&native_config(root, &binary, &scenario)).expect("compose node");
    let created = node
        .create_instance(create_req(&binary, &home))
        .await
        .expect("create instance");
    let id = created.instance.meta.id.clone();

    // Wait for the real agent session to come up (the PTY worker only drains
    // control commands once control is ready).
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let instance = node.get_instance(&id).expect("instance");
            if matches!(
                instance.lifecycle,
                remuda_protocol::InstanceLifecycle::Ready
            ) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the scripted Claude reaches ready");

    for effort in bad_effort_payloads.iter() {
        // The exact production entry point of `instance.configure`
        // (transport/hubnode_codec.rs `dispatch_configure`): a wire request
        // parsed from JSON and folded with the wire `effort` object.
        let parsed: InstanceCommandRequest =
            serde_json::from_value(json!({ "operation": "instance.configure" }))
                .expect("operation parses");
        let parsed = parsed.with_configure(&json!({ "effort": effort.clone() }));
        assert_eq!(
            parsed.effort_axis.as_deref(),
            Some("flag"),
            "the axis rides the production parse: {effort}"
        );
        assert_eq!(
            parsed.effort_ultracode, None,
            "the missing/invalid boolean maps to None, never false: {effort}"
        );

        let result = node
            .submit_command(
                &id,
                InstanceCommandRequest {
                    // Let the Node assign the command id, as a Hub configure
                    // without a client-chosen id does.
                    command_id: None,
                    ..parsed
                },
            )
            .await
            .expect("the configure is accepted for the driver");

        // The worker settles asynchronously; poll the durable command.
        let command_id = result.command.command_id.clone();
        let settled = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let command = node.get_command(&command_id).expect("command ledger");
                if command.state == CommandState::Settled {
                    return command;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the driver refusal settles within the budget");

        // The real driver refused it: Rejected with the flag-carrier
        // capability error, before any typing. It never fell through to
        // `from_level`, which would have typed a plain `/effort high`.
        let Knowledge::Known { value: settlement } = settled.settlement else {
            panic!("a rejected configure carries a known settlement: {effort}");
        };
        assert_eq!(
            settlement.outcome,
            SettlementOutcome::Rejected,
            "the bad boolean is refused by the driver, not dispatched: {effort}"
        );
        let message = settlement
            .error
            .as_ref()
            .map(|error| error.message.clone())
            .unwrap_or_default();
        assert!(
            message.contains("ultracode cannot be switched"),
            "the rejection is the flag-axis pre-dispatch gate, not a fall-through: {message}"
        );
    }

    // The rejected control operations never fail the instance.
    let instance = node.get_instance(&id).expect("instance still present");
    assert!(
        !matches!(
            instance.lifecycle,
            remuda_protocol::InstanceLifecycle::Failed
        ),
        "a configure refusal is a control rejection, not an instance failure"
    );

    node.submit_command(
        &id,
        serde_json::from_value(json!({ "operation": "close" })).expect("close"),
    )
    .await
    .expect("close reaps the child");
}
