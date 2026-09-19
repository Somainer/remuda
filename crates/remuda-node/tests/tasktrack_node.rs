//! Node integration test for subagent task-row lifecycle (c-tasktrack).
//!
//! The fake harness stands in for claude with the real per-session hook
//! overlay + native transcript (`REMUDA_PTY_EMULATOR=1`, `REMUDA_PTY_HOOKS=1`),
//! exactly the demo's carrier. It scripts:
//!  - a synchronous foreground `Agent` call → one Final result;
//!  - a backgrounded `Agent` call → immediate Partial launch result, then an
//!    injected `<task-notification>` delivered after the turn ends → a Final
//!    result folded onto the same node.
//!
//! The assertions read the committed journal, so they cover the whole path:
//! shell-pty transcript hydration → driver mapper → Node → journal.

#![cfg(unix)]

use remuda_node::{
    CommandAction, CreateInstanceRequest, DevNode, DevServerConfig, InstanceCommandRequest,
    MemoryStore, NativeDriverConfig, ServeConfig,
};
use remuda_protocol::AgentKind;
use remuda_protocol::{
    DriverKind, InputOrigin, Instance, InstanceLifecycle, ObservationPayload, ResultStage,
    ToolOutcome,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const CARRIER_ENV: &str = "REMUDA_PTY_CARRIER";
const EMULATOR_ENV: &str = "REMUDA_PTY_EMULATOR";
const HOOKS_ENV: &str = "REMUDA_PTY_HOOKS";
const RUN_MARKER: &str = "REMUDA_TASKTRACK_CHILD";

/// Install a 0755 fake binary outside the cwd.
fn install_fake(root: &Path) -> PathBuf {
    let install = root.join("opt/bin");
    std::fs::create_dir_all(&install).expect("bin dir");
    let source = remuda_testing::ensure_workspace_bin("fake-harness");
    let dest = install.join("claude");
    std::fs::copy(&source, &dest).expect("copy fake claude");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    dest
}

fn relay_bin(root: &Path) -> PathBuf {
    if let Ok(path) = std::env::var("REMUDA_RELAY_BIN") {
        return PathBuf::from(path);
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        && let Some(candidate) = [dir.join("remuda"), dir.join("remuda.exe")]
            .into_iter()
            .find(|p| p.is_file())
    {
        return candidate;
    }
    let out = root.join("relay-target");
    let status = std::process::Command::new(env!("CARGO"))
        .current_dir(remuda_testing::workspace_root())
        .args(["build", "-p", "remuda", "--bin", "remuda", "--quiet"])
        .arg("--target-dir")
        .arg(&out)
        .status()
        .expect("build remuda relay");
    assert!(status.success());
    out.join("debug/remuda")
}

struct Run {
    node: DevNode,
    instance: Instance,
    #[allow(dead_code)]
    root: PathBuf,
    _dir: Option<tempfile::TempDir>,
}

async fn start() -> Run {
    // Keep artifacts on disk under a known dir when debugging the fake's
    // transcript; otherwise use an auto-cleaned tempdir.
    let (root, dir) = if let Ok(dir) = std::env::var("REMUDA_TT_DIR") {
        let root = PathBuf::from(dir);
        std::fs::create_dir_all(&root).unwrap();
        (root, None)
    } else {
        // Pinned to /tmp on purpose: a long $TMPDIR pushes the per-instance
        // hook.sock past the AF_UNIX path limit; c-sockpath fixes that in the
        // product. start_in adds this run root's parent to workspace_roots so
        // the pin still holds when TMPDIR points elsewhere.
        let dir = tempfile::tempdir_in("/tmp").expect("tempdir");
        (dir.path().to_path_buf(), Some(dir))
    };
    start_in(root, dir).await
}

async fn start_in(root: PathBuf, _dir: Option<tempfile::TempDir>) -> Run {
    let data_dir = root.join("node-data");
    let workspace = root.join("workspace");
    let home = root.join("claude-home");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let binary = install_fake(&root);
    let scenario = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../remuda-testing/fixtures/fake-harness/scenarios/tasktrack.json");

    let mut native = NativeDriverConfig::new(data_dir.clone());
    native.pty_hooks = true;
    native.relay_binary = Some(relay_bin(&root).into());
    native
        .extra_env
        .insert("FAKE_HARNESS_DIALECT_VERSION".into(), "modern".into());
    native.extra_env.insert(
        "FAKE_HARNESS_SCRIPT".into(),
        scenario.to_string_lossy().into_owned(),
    );

    // The run root is pinned under /tmp for socket-length reasons (see start);
    // allow its parent too, otherwise the workspace is rejected whenever
    // TMPDIR points at a different directory. The REMUDA_TT_DIR debug root is
    // arbitrary and gets the same treatment via its own parent.
    let mut workspace_roots = remuda_testing::test_workspace_roots!();
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.clone());
    if let Some(parent) = canonical_root.parent()
        && !workspace_roots.iter().any(|held| parent.starts_with(held))
    {
        workspace_roots.push(parent.to_path_buf());
    }

    let config = ServeConfig {
        http: DevServerConfig::loopback(0)
            .with_workspace_root(workspace.clone())
            .with_workspace_roots(workspace_roots),
        data_dir: data_dir.clone(),
        drivers: remuda_node::LocalDrivers::Native(native.clone()),
    };
    let registry = remuda_node::native_driver_registry(native).expect("registry");
    let node = DevNode::with_parts(&config.http, Arc::new(MemoryStore::new(512)), registry)
        .expect("dev node");

    let request = CreateInstanceRequest {
        origin: InputOrigin::Human,
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
        tui: None,
        extra_env: std::collections::BTreeMap::new(),
        provider_profile_id: "dev-fake".into(),
        permission_mode: "manual".into(),
        sandbox: None,
        prompt: "FGAGENT foreground".into(),
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
    };

    let created = node
        .create_instance(request)
        .await
        .expect("create instance");
    Run {
        node,
        instance: created.instance,
        root,
        _dir,
    }
}

impl Run {
    async fn observations(&self) -> Vec<remuda_protocol::Observation> {
        self.node
            .read_journal(&self.instance.journal_id, None, 4096)
            .expect("read journal")
            .events
            .into_iter()
            .filter_map(|event| match event {
                remuda_protocol::JournalEvent::Instance(o) => Some(*o),
                _ => None,
            })
            .collect()
    }

    async fn send(&self, prompt: &str) {
        self.node
            .submit_command(
                &self.instance.meta.id,
                InstanceCommandRequest {
                    origin: InputOrigin::Human,
                    command_id: None,
                    operation: CommandAction::Send,
                    prompt: Some(prompt.into()),
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
            .await
            .expect("send prompt");
    }

    /// One tool node's result stages in journal order.
    async fn tool_result_stages(&self) -> Vec<(String, ResultStage, ToolOutcome)> {
        self.observations()
            .await
            .into_iter()
            .filter_map(|obs| match obs.body {
                ObservationPayload::ToolResult(result) => Some((
                    result.tool_call_id.to_string(),
                    result.stage,
                    result.outcome,
                )),
                _ => None,
            })
            .collect()
    }

    async fn wait_until<F>(&self, timeout: Duration, pred: F) -> Option<()>
    where
        F: Fn(&[(String, ResultStage, ToolOutcome)]) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            let stages = self.tool_result_stages().await;
            if pred(&stages) {
                return Some(());
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Group result stages by tool node.
fn by_node(stages: &[(String, ResultStage, ToolOutcome)]) -> Vec<Vec<(ResultStage, ToolOutcome)>> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::BTreeMap<String, Vec<(ResultStage, ToolOutcome)>> =
        std::collections::BTreeMap::new();
    for (id, stage, outcome) in stages {
        if !groups.contains_key(id) {
            order.push(id.clone());
        }
        groups
            .entry(id.clone())
            .or_default()
            .push((*stage, *outcome));
    }
    order
        .into_iter()
        .map(|id| groups.remove(&id).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreground_and_background_subagent_rows_close_correctly() {
    if std::env::var(RUN_MARKER).is_err() {
        re_exec_child();
        return;
    }

    let run = start().await;

    // Foreground Agent turn settles to one Final result.
    run.wait_until(Duration::from_secs(40), |stages| {
        by_node(stages)
            .iter()
            .any(|g| g.len() == 1 && g[0].0 == ResultStage::Final)
    })
    .await
    .expect("foreground Final result");

    // Launch the backgrounded Agent.
    run.send("BGAGENT background").await;

    // An immediate Partial launch result appears (running in background).
    run.wait_until(Duration::from_secs(40), |stages| {
        by_node(stages)
            .iter()
            .any(|g| g.iter().any(|(stage, _)| *stage == ResultStage::Partial))
    })
    .await
    .expect("Partial launch result");

    // The post-Stop <task-notification> folds a Final result onto that node.
    run.wait_until(Duration::from_secs(40), |stages| {
        by_node(stages).iter().any(|g| {
            g.iter().any(|(stage, _)| *stage == ResultStage::Partial)
                && g.last().is_some_and(|(stage, outcome)| {
                    *stage == ResultStage::Final && *outcome == ToolOutcome::Succeeded
                })
        })
    })
    .await
    .expect("background completion Final result");

    let groups = by_node(&run.tool_result_stages().await);
    // Both channels (hook relay + transcript tailer) can each emit results for
    // the same deterministic node, so assert the folded semantics: the
    // foreground node ends Final without a Partial; the background node passes
    // through Partial and ends Final.
    let mut saw_fg = false;
    let mut saw_bg = false;
    for group in &groups {
        let stages: Vec<_> = group.iter().map(|(stage, _)| *stage).collect();
        let (last_stage, last_outcome) = group.last().unwrap();
        if *last_stage == ResultStage::Final && *last_outcome == ToolOutcome::Succeeded {
            if stages.contains(&ResultStage::Partial) {
                saw_bg = true;
            } else {
                saw_fg = true;
            }
        }
    }
    assert!(saw_fg, "foreground Agent closed Final: {groups:?}");
    assert!(
        saw_bg,
        "background Agent Partial then Final on one node: {groups:?}"
    );
}

fn re_exec_child() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "foreground_and_background_subagent_rows_close_correctly",
            "--nocapture",
        ])
        .env(RUN_MARKER, "1")
        .env(CARRIER_ENV, "native")
        .env(EMULATOR_ENV, "1")
        .env(HOOKS_ENV, "1")
        .output()
        .expect("re-exec tasktrack child");
    if !output.status.success() {
        panic!(
            "child failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    println!("{}", String::from_utf8_lossy(&output.stdout));

    // Let the launched instance exit cleanly before the tempdir is dropped.
    let _ = ();
    let _ = InstanceLifecycle::Exited;
}
