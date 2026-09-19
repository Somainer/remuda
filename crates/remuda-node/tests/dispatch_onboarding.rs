//! dispatch-onboarding-1 end-to-end: a worker launched into a Node-provisioned
//! worktree reaches its first tool call with **no keys sent**, because the
//! Node pre-trusted the exact cwd from the provision record and seeded the
//! bypass answers into the scoped config dir before launch.
//!
//! The deterministic fake harness stands in for claude. With
//! `FAKE_HARNESS_TRUST_GATE=1` it consults the same global config the real
//! CLI reads (`projects[<cwd>].hasTrustDialogAccepted`): accepted cwd mounts
//! the composer, anything else parks on the Trust screen. Ground truth is the
//! harness events file — `trust_gate=accepted-from-config` followed by
//! `tool_start_hook`, with no `trust_accepted` keypress anywhere.
//!
//! Re-execs under `REMUDA_PTY_CARRIER=native`, the same pattern as
//! `live_pipeline.rs`: the shell-pty agent arm reads that env at launch and
//! process-wide env is unsafe to mutate in-process.

#![cfg(unix)]

use remuda_node::{
    CreateInstanceRequest, DevNode, DevServerConfig, MemoryStore, NativeDriverConfig, ServeConfig,
};
use remuda_protocol::{AgentKind, DriverKind};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CARRIER_ENV: &str = "REMUDA_PTY_CARRIER";
const RUN_MARKER: &str = "REMUDA_DISPATCH_ONBOARDING_CHILD";
const TOOL_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Clone, Debug)]
struct Anchor {
    event: String,
    decision: Option<String>,
    tool: Option<String>,
}

fn read_anchors(path: &Path) -> Vec<Anchor> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(|row| Anchor {
            event: row
                .get("event")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            decision: row
                .get("decision")
                .and_then(Value::as_str)
                .map(str::to_owned),
            tool: row
                .get("toolName")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
        .collect()
}

fn wait_for<F: Fn(&[Anchor]) -> bool>(path: &Path, pred: F, what: &str) {
    let deadline = Instant::now() + TOOL_TIMEOUT;
    loop {
        if pred(&read_anchors(path)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what} never appeared at {path:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Temp git repo with a fetchable origin (same fixture as worker.provision).
fn init_repo(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    let run = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(&["init", "-q"]);
    run(&["symbolic-ref", "HEAD", "refs/heads/main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "test"]);
    run(&["commit", "--allow-empty", "-m", "init"]);
    run(&["remote", "add", "origin", root.to_str().unwrap()]);
    run(&["fetch", "-q", "origin"]);
    run(&["update-ref", "refs/remotes/origin/main", "refs/heads/main"]);
}

fn install_fake_claude(root: &Path) -> PathBuf {
    let install = root.join("opt/bin");
    std::fs::create_dir_all(&install).unwrap();
    let dest = install.join("claude");
    std::fs::copy(remuda_testing::ensure_workspace_bin("fake-harness"), &dest).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    dest
}

fn run_child() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let repo = root.join("repo");
        let data_dir = root.join("node-data");
        let scoped_home = root.join("scoped-claude-config");
        std::fs::create_dir_all(&scoped_home).unwrap();
        init_repo(&repo);
        let binary = install_fake_claude(&root);

        let scenario = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../remuda-testing/fixtures/fake-harness/scenarios/dispatch-onboarding.json");
        let events_path = root.join("harness-events.jsonl");

        let mut native = NativeDriverConfig::new(data_dir.clone());
        native
            .extra_env
            .insert("FAKE_HARNESS_TRUST_GATE".into(), "1".into());
        native.extra_env.insert(
            "FAKE_HARNESS_SCRIPT".into(),
            scenario.to_string_lossy().into_owned(),
        );
        native.extra_env.insert(
            "FAKE_HARNESS_EVENTS_OUT".into(),
            events_path.to_string_lossy().into_owned(),
        );
        // The workspace and its sibling managed-worktree root are both
        // registered, so the provisioned cwd passes Node cwd containment.
        let config = ServeConfig {
            http: DevServerConfig::loopback(0)
                .with_workspace_root(repo.clone())
                .with_workspace_roots(vec![root.clone()]),
            data_dir: data_dir.clone(),
            drivers: remuda_node::LocalDrivers::Native(native.clone()),
        };
        let registry = remuda_node::native_driver_registry(native).unwrap();
        let node = DevNode::with_parts(
            &config.http,
            std::sync::Arc::new(MemoryStore::new(256)),
            registry,
        )
        .unwrap();

        // worker.provision: the product-assigned worktree the dispatch worker
        // launches in; this is the catalog record provenance is read from.
        let provisioned = remuda_node::dispatch_hub_rpc(
            &node,
            "worker.provision",
            json!({
                "name": "c-onboard-e2e",
                "branch": "wt/c-onboard-e2e/onboarding",
            }),
        )
        .await
        .expect("worker.provision RPC");
        let worktree = PathBuf::from(provisioned["worktreePath"].as_str().expect("worktreePath"));
        assert!(worktree.is_dir(), "provisioned worktree exists");

        let request = CreateInstanceRequest {
            origin: remuda_protocol::InputOrigin::Human,
            agent_credential: None,
            command_id: None,
            instance_id: None,
            host_id: None,
            workspace_id: None,
            kind: AgentKind::Claude,
            driver: DriverKind::ShellPty,
            model: "fake".into(),
            args: Vec::new(),
            binary_path: Some(binary.to_string_lossy().into_owned()),
            binary_sha256: None,
            provider_profile_id: "dev-fake".into(),
            // Dispatch always carries bypassPermissions.
            permission_mode: "bypassPermissions".into(),
            sandbox: None,
            prompt: "Run your first tool unattended".into(),
            cwd: Some(worktree.to_string_lossy().into_owned()),
            delegation: None,
            settings_overlay_path: None,
            claude_config_dir: Some(scoped_home.to_string_lossy().into_owned()),
            max_budget_usd: None,
            provider_overlay: None,
            provider_auth_token: None,
            resume_session_id: None,
            resumed_from: None,
            effort: None,
            tui: None,
            extra_env: std::collections::BTreeMap::new(),

            capabilities: Default::default(),
        };
        let created = node
            .create_instance(request)
            .await
            .expect("create instance");

        // 1) The fake mounts the composer on the strength of the persisted
        // trust flag — the dialog screen is never shown.
        wait_for(
            &events_path,
            |anchors| {
                anchors.iter().any(|a| {
                    a.event == "trust_gate" && a.decision.as_deref() == Some("accepted-from-config")
                })
            },
            "trust_gate accepted-from-config",
        );
        // 2) The queued prompt is delivered (no test ever sends keys) and the
        // turn reaches its first — and only — tool call.
        wait_for(
            &events_path,
            |anchors| {
                anchors
                    .iter()
                    .any(|a| a.event == "tool_start_hook" && a.tool.as_deref() == Some("Bash"))
            },
            "first tool call (tool_start_hook Bash)",
        );
        // 3) The only way past a parked Trust screen is a logged keypress.
        let anchors = read_anchors(&events_path);
        assert!(
            !anchors.iter().any(|a| a.event == "trust_accepted"),
            "the trust dialog must not have been answered by keys: {anchors:?}"
        );
        // 4) The scenario quits after its single turn.
        wait_for(
            &events_path,
            |anchors| anchors.iter().any(|a| a.event == "exit"),
            "harness exit",
        );

        // 5) The scoped config dir carries the exact-cwd trust decision and
        // the bypass outside-reads answer — provenance, not a path guess.
        let global: Value =
            serde_json::from_slice(&std::fs::read(scoped_home.join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(
            global["projects"][worktree.to_string_lossy().as_ref()]["hasTrustDialogAccepted"],
            json!(true),
            "scoped config pre-trusts the exact provisioned cwd: {global}"
        );
        assert_eq!(
            global["hasSeenAutoModeOutsideReadPrompt"],
            json!(true),
            "bypass posture seeded the keep-allowing outside-reads answer"
        );
        assert!(global.get("projects").unwrap().as_object().unwrap().len() == 1);

        // Best-effort cleanup.
        let _ = node
            .submit_command(
                &created.instance.meta.id,
                remuda_node::InstanceCommandRequest {
                    origin: remuda_protocol::InputOrigin::Agent,
                    command_id: None,
                    operation: remuda_node::CommandAction::Close,
                    prompt: None,
                    prompt_mode: None,
                    attachments: Vec::new(),
                    run_id: None,
                    interaction_id: None,
                    answer: None,
                    keys: None,
                    model: None,
                    effort_name: None,
                    effort_index: None,
                    permission_mode: None,
                },
            )
            .await;
    });
}

#[test]
fn dispatched_worker_reaches_first_tool_call_with_no_keys() {
    if std::env::var(RUN_MARKER).is_err() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "dispatched_worker_reaches_first_tool_call_with_no_keys",
                "--nocapture",
            ])
            .env(RUN_MARKER, "1")
            .env(CARRIER_ENV, "native")
            .output()
            .expect("re-exec dispatch-onboarding child");
        if !output.status.success() {
            panic!(
                "child failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        println!("{}", String::from_utf8_lossy(&output.stdout));
        return;
    }
    run_child();
}
