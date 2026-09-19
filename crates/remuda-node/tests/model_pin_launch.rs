//! model-pin-1: an explicit model pin survives a real native launch.
//!
//! The 2026-09-18 demo dispatched every worker with `--model <pinned id>` on
//! driver `shell-pty` and ran all of them on the host's default: the
//! materializer's agent arm emitted no model token, and the settings merge left
//! the host's own `model` key in place. Every audit record and roster row still
//! said the pin had been honoured, so nothing in the system contradicted it.
//!
//! This test closes that loop end to end through the real Node runtime and a
//! real PTY. The fake harness parses `--model` (`remuda-testing/src/flags.rs`)
//! and echoes it into the records it writes, so what comes back out of the
//! transcript is proof of what the process actually received — not of what we
//! asked for.
//!
//! Re-execs itself under the native-carrier env for the same reason
//! `live_pipeline.rs` does: process-wide env is unsafe to mutate in-process and
//! the shell-pty agent arm reads `REMUDA_PTY_CARRIER` at launch.

#![cfg(unix)]

use remuda_node::{
    CreateInstanceRequest, DevNode, DevServerConfig, MemoryStore, NativeDriverConfig, ServeConfig,
};
use remuda_protocol::{AgentKind, DriverKind};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const CARRIER_ENV: &str = "REMUDA_PTY_CARRIER";
const EMULATOR_ENV: &str = "REMUDA_PTY_EMULATOR";
const RUN_MARKER: &str = "REMUDA_MODEL_PIN_CHILD";
const HOOKS_ENV: &str = "REMUDA_PTY_HOOKS";
/// Which case the re-exec'd child runs.
const CASE_ENV: &str = "REMUDA_MODEL_PIN_CASE";
/// Makes the fake harness report a model other than the one it was given, to
/// stand in for a harness that ignores `--model` (the 2026-09-18 substitution).
const REPORT_MODEL_ENV: &str = "FAKE_HARNESS_REPORT_MODEL";

/// A synthetic pin in the shape a gateway catalog really lists: namespaced, with
/// a `[1m]` context-window variant. The suffix is load-bearing — it selects a
/// different model — so it is asserted byte-identical throughout.
const PIN: &str = "model_hub/es1_orange_o50[1m]";
/// A different id standing in for the launching host's own default, to prove the
/// pin is what reached the process rather than whatever the host preferred.
const HOST_DEFAULT: &str = "model_hub/es1_orange_o48[1m]";

#[test]
fn a_pinned_model_reaches_the_process_on_a_native_shell_pty_launch() {
    if std::env::var(RUN_MARKER).is_err() {
        re_exec_child("honoured");
        re_exec_child("mismatch");
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let mismatch = std::env::var(CASE_ENV).as_deref() == Ok("mismatch");
    runtime.block_on(async { assert_pin_reaches_the_harness(mismatch).await });
}

/// `mismatch`: the harness is told to report a model other than the one it was
/// given, standing in for a harness that ignores `--model`. The pin still
/// reaches its argv, so this isolates "honoured" from "sent".
async fn assert_pin_reaches_the_harness(mismatch: bool) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let data_dir = root.join("node-data");
    let workspace = root.join("workspace");
    let home = root.join("claude-home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");

    // A host settings layer that names a *different* model, the way the demo
    // host did. Under delegation `none` this used to answer for the session.
    std::fs::write(
        home.join("settings.json"),
        serde_json::json!({
            "model": HOST_DEFAULT,
            "env": {"ANTHROPIC_MODEL": HOST_DEFAULT},
        })
        .to_string(),
    )
    .expect("host settings");

    let binary = install_fake_claude(&root);
    let mut native = NativeDriverConfig::new(data_dir.clone());
    // Hooks on: the deterministic transcript binding comes from the
    // SessionStart hook, and without a bound transcript there is no model
    // read-back channel at all (the session reports `transcript_unbound`).
    native.pty_hooks = true;
    native.relay_binary = Some(remuda_relay_bin(&root).into());
    native
        .extra_env
        .insert("FAKE_HARNESS_DIALECT_VERSION".into(), "modern".into());
    if mismatch {
        // Ignore the pin and answer on the host's default instead — the
        // 2026-09-18 substitution, in the pin's own namespace so it is
        // decidable (a bare upstream name would not be; model-pin-1 §3).
        native
            .extra_env
            .insert(REPORT_MODEL_ENV.into(), HOST_DEFAULT.into());
    }

    let config = ServeConfig {
        http: DevServerConfig::loopback(0)
            .with_workspace_root(workspace.clone())
            .with_workspace_roots(remuda_testing::test_workspace_roots!()),
        data_dir: data_dir.clone(),
        drivers: remuda_node::LocalDrivers::Native(native.clone()),
    };
    let registry = remuda_node::native_driver_registry(native).expect("registry");
    let node =
        DevNode::with_parts(&config.http, Arc::new(MemoryStore::new(256)), registry).expect("node");

    let created = node
        .create_instance(CreateInstanceRequest {
            origin: remuda_protocol::InputOrigin::Human,
            agent_credential: None,
            command_id: None,
            instance_id: None,
            host_id: None,
            workspace_id: None,
            kind: AgentKind::Claude,
            driver: DriverKind::ShellPty,
            // The pin under test.
            model: PIN.into(),
            args: Vec::new(),
            binary_path: Some(binary.to_string_lossy().into_owned()),
            binary_sha256: None,
            tui: None,
            extra_env: std::collections::BTreeMap::new(),
            api_route: None,
            api_relay_endpoint: None,
            provider_profile_id: "dev-fake".into(),
            permission_mode: "manual".into(),
            sandbox: None,
            prompt: "model pin probe".into(),
            cwd: None,
            // Delegation `none` — the exact profile kind the demo used, and the
            // one where the host's own settings used to win.
            delegation: None,
            settings_overlay_path: None,
            claude_config_dir: Some(home.to_string_lossy().into_owned()),
            max_budget_usd: None,
            provider_overlay: None,
            provider_auth_token: None,
            resume_session_id: None,
            resumed_from: None,
            effort: None,

            capabilities: Default::default(),
        })
        .await
        .expect("create instance");

    // What the harness actually received/reported: the half the Node cannot
    // fake. The fake harness parses `--model` (`remuda-testing/src/flags.rs`)
    // and echoes the session model into the transcript it writes.
    let transcript = wait_for_transcript(&home, Duration::from_secs(60)).await;
    let models = transcript_models(&transcript);

    if mismatch {
        // The substitution: the process answered on the host's default. The
        // read-back gate must have stopped the launch, naming both ids.
        assert!(
            models.iter().any(|model| model == HOST_DEFAULT),
            "the mismatch case must report the host default: {models:?}"
        );
        let failure =
            wait_for_model_mismatch(&node, &created.instance.meta.id, Duration::from_secs(60))
                .await;
        assert!(
            failure.contains(PIN),
            "must name the requested id: {failure}"
        );
        assert!(
            failure.contains(HOST_DEFAULT),
            "must name the observed id: {failure}"
        );
        // And the instance must not be left running on the wrong model.
        let instance = node
            .get_instance(&created.instance.meta.id)
            .expect("instance row");
        assert_eq!(
            instance.lifecycle,
            remuda_protocol::InstanceLifecycle::Failed,
            "a refused launch must not stay working"
        );
    } else {
        // The pin answered, so the gate stays silent and nothing is failed.
        assert!(
            models.contains(&PIN.to_owned()),
            "the harness recorded {models:?}, expected the pin {PIN}"
        );
        assert!(
            !models.iter().any(|model| model == HOST_DEFAULT),
            "the host's default answered instead of the pin: {models:?}"
        );
        let instance = node
            .get_instance(&created.instance.meta.id)
            .expect("instance row");
        assert!(
            !instance
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("model-mismatch"),
            "an honoured pin must not be refused: {:?}",
            instance.last_error
        );
    }

    let _ = node;
}

/// Wait for the Node to record the model-mismatch refusal, returning its reason.
async fn wait_for_model_mismatch(
    node: &DevNode,
    instance_id: &remuda_protocol::InstanceId,
    budget: Duration,
) -> String {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Ok(instance) = node.get_instance(instance_id)
            && let Some(error) = instance.last_error.as_deref()
            && error.contains("model-mismatch")
        {
            return error.to_owned();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the node never recorded a model-mismatch refusal"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Every distinct `message.model` the harness wrote.
fn transcript_models(path: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut seen = Vec::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(model) = value
            .get("message")
            .and_then(|m| m.get("model"))
            .and_then(Value::as_str)
            && !seen.iter().any(|existing| existing == model)
        {
            seen.push(model.to_owned());
        }
    }
    seen
}

/// Wait for the harness to write its session transcript under `<home>/projects`.
async fn wait_for_transcript(home: &Path, budget: Duration) -> PathBuf {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Some(path) = find_transcript(&home.join("projects"))
            && std::fs::metadata(&path)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
            && !transcript_models(&path).is_empty()
        {
            return path;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no harness transcript with a model under {}",
            home.display()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn find_transcript(projects: &Path) -> Option<PathBuf> {
    for project in std::fs::read_dir(projects).ok()?.flatten() {
        for entry in std::fs::read_dir(project.path()).ok()?.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                return Some(path);
            }
        }
    }
    None
}

/// Install a 0755 fake binary outside the workspace cwd (the override guard
/// rejects group-writable target-dir files and anything inside the cwd).
fn install_fake_claude(root: &Path) -> PathBuf {
    let install = root.join("opt/bin");
    std::fs::create_dir_all(&install).expect("bin dir");
    let source = remuda_testing::ensure_workspace_bin("fake-harness");
    let dest = install.join("claude");
    std::fs::copy(&source, &dest).expect("copy fake claude");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    dest
}

/// Locate or build the real `remuda` CLI the hook overlay invokes.
fn remuda_relay_bin(root: &Path) -> PathBuf {
    if let Ok(path) = std::env::var("REMUDA_RELAY_BIN") {
        return PathBuf::from(path);
    }
    let out = root.join("relay-target");
    let status = std::process::Command::new(env!("CARGO"))
        .current_dir(remuda_testing::workspace_root())
        .args(["build", "-p", "remuda", "--bin", "remuda", "--quiet"])
        .arg("--target-dir")
        .arg(&out)
        .status()
        .expect("build remuda relay");
    assert!(status.success(), "building the remuda relay failed");
    out.join("debug/remuda")
}

fn re_exec_child(case: &str) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_pinned_model_reaches_the_process_on_a_native_shell_pty_launch",
            "--nocapture",
        ])
        .env(RUN_MARKER, "1")
        .env(CASE_ENV, case)
        .env(CARRIER_ENV, "native")
        .env(EMULATOR_ENV, "1")
        .env(HOOKS_ENV, "1")
        .output()
        .expect("re-exec model pin child");
    if !output.status.success() {
        panic!(
            "{case} child failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    println!("[{case}] {}", String::from_utf8_lossy(&output.stdout));
}
