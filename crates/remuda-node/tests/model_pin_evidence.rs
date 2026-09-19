//! Evidence capture for `docs/design/evidence/model-pin-1.md` (not a gate).
//!
//! Runs one real native shell-pty launch with an explicit pin through the Node
//! runtime and prints the launch argv the PTY exec'd plus the harness's own
//! transcript lines. Ignored by default: it is a capture tool, and the
//! assertions that guard the behaviour live in `model_pin_launch.rs`.
//!
//! Run with:
//!   REMUDA_PTY_CARRIER=native REMUDA_PTY_EMULATOR=1 \
//!   cargo test -p remuda-node --test model_pin_evidence -- --ignored --nocapture

#![cfg(unix)]

use remuda_node::{
    CreateInstanceRequest, DevNode, DevServerConfig, MemoryStore, NativeDriverConfig, ServeConfig,
};
use remuda_protocol::{AgentKind, DriverKind};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const PIN: &str = "model_hub/es1_orange_o50[1m]";
const HOST_DEFAULT: &str = "model_hub/es1_orange_o48[1m]";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "evidence capture: prints argv + transcript for the design doc"]
async fn capture_model_pin_evidence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let workspace = root.join("workspace");
    let home = root.join("claude-home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");

    // A host settings layer naming a different model, as the demo host had.
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
    let data_dir = root.join("node-data");
    let mut native = NativeDriverConfig::new(data_dir.clone());
    native.pty_hooks = false;
    native.relay_binary = Some(remuda_relay_bin(&root).into());
    native
        .extra_env
        .insert("FAKE_HARNESS_DIALECT_VERSION".into(), "modern".into());

    let config = ServeConfig {
        http: DevServerConfig::loopback(0)
            .with_workspace_root(workspace.clone())
            .with_workspace_roots(remuda_testing::test_workspace_roots!()),
        data_dir,
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
            model: PIN.into(),
            args: Vec::new(),
            binary_path: Some(binary.to_string_lossy().into_owned()),
            binary_sha256: None,
            tui: None,
            extra_env: std::collections::BTreeMap::new(),
            provider_profile_id: "dev-fake".into(),
            permission_mode: "manual".into(),
            sandbox: None,
            prompt: "model pin evidence".into(),
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

            capabilities: Default::default(),
        })
        .await
        .expect("create instance");

    let transcript = wait_for_transcript(&home, Duration::from_secs(90)).await;

    println!("=== EVIDENCE: launch argv (from the harness's own init record) ===");
    for line in std::fs::read_to_string(&transcript)
        .unwrap_or_default()
        .lines()
    {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("assistant") {
            let model = value
                .get("message")
                .and_then(|m| m.get("model"))
                .and_then(Value::as_str)
                .unwrap_or("?");
            println!("assistant record: message.model = {model}");
        }
    }
    println!("=== EVIDENCE: merged overlay the CLI was handed ===");
    if let Some(overlay) = find_overlay(&root) {
        let text = std::fs::read_to_string(&overlay).unwrap_or_default();
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        println!(
            "model key   = {}",
            value.get("model").unwrap_or(&Value::Null)
        );
        println!(
            "env.ANTHROPIC_MODEL = {}",
            value
                .get("env")
                .and_then(|e| e.get("ANTHROPIC_MODEL"))
                .unwrap_or(&Value::Null)
        );
    }
    let instance = node
        .get_instance(&created.instance.meta.id)
        .expect("instance");
    println!(
        "=== EVIDENCE: instance lifecycle = {:?}",
        instance.lifecycle
    );
    println!("last_error = {:?}", instance.last_error);
}

fn find_overlay(root: &Path) -> Option<PathBuf> {
    fn walk(dir: &Path, out: &mut Option<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.file_name().is_some_and(|n| n == "settings.json")
                && path.to_string_lossy().contains("launch")
            {
                *out = Some(path);
            }
        }
    }
    let mut found = None;
    walk(root, &mut found);
    found
}

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

async fn wait_for_transcript(home: &Path, budget: Duration) -> PathBuf {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Some(path) = find_transcript(&home.join("projects"))
            && !transcript_models(&path).is_empty()
        {
            return path;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no harness transcript appeared"
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
