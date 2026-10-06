//! c-resumehome, terminal half: a native shell-pty agent resumed through
//! 「继续」launches `claude --resume <id>` in a NEW instance whose pinned
//! native home starts empty. The Node must stage the predecessor transcript
//! there before the PTY process starts.
//!
//! The fake harness enforces the real lookup: on `--resume` it calls
//! `open_resume`, which exits with "resume: transcript not found" when
//! `<home>/projects/<encoded cwd>/<id>.jsonl` is absent. So a resumed child
//! that stays running with its transcript present is the end-to-end proof.
//!
//! Re-execs itself under the native-carrier env for the same reason
//! `model_pin_launch.rs` does: process-wide env is unsafe to mutate in-process
//! and the shell-pty agent arm reads `REMUDA_PTY_CARRIER` at launch.

#![cfg(unix)]

use remuda_node::{
    CreateInstanceRequest, DevNode, DevServerConfig, LocalDrivers, NativeDriverConfig, ServeConfig,
};
use remuda_protocol::{AgentKind, DriverKind, InstanceId};
use std::path::{Path, PathBuf};
use std::time::Duration;

const CARRIER_ENV: &str = "REMUDA_PTY_CARRIER";
const EMULATOR_ENV: &str = "REMUDA_PTY_EMULATOR";
const RUN_MARKER: &str = "REMUDA_RESUME_HOME_PTY_CHILD";
const HOOKS_ENV: &str = "REMUDA_PTY_HOOKS";
/// The fake harness's session id when argv does not pin one (as a native
/// agent-pty launch does — the id comes back from the harness).
const SESSION: &str = "00000000-0000-4000-8000-000000000001";

#[test]
fn a_native_shell_pty_resume_chain_runs_from_staged_conversations() {
    if std::env::var(RUN_MARKER).is_err() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "a_native_shell_pty_resume_chain_runs_from_staged_conversations",
                "--nocapture",
            ])
            .env(RUN_MARKER, "1")
            .env(CARRIER_ENV, "native")
            .env(EMULATOR_ENV, "1")
            .env(HOOKS_ENV, "1")
            .output()
            .expect("re-exec resume-home pty child");
        if !output.status.success() {
            panic!(
                "resume-home pty child failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(assert_resume_chain());
}

async fn assert_resume_chain() {
    let root_dir = tempfile::tempdir().expect("tempdir");
    let root = root_dir.path();
    let data = root.join("data");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");

    let binary = install_fake_harness(root);
    let mut native = NativeDriverConfig::new(data.clone()).with_claude_binary(binary.clone());
    // The modern dialect (claude 2.1.270) PTY input path exercised by the
    // model-pin launch test; the built-in catch-all turn answers after one
    // "approve" keypress, which `approve` sends.
    native
        .extra_env
        .insert("FAKE_HARNESS_DIALECT_VERSION".into(), "modern".into());
    // Hooks on: this is the promoted native shell-pty shape from the bug
    // report, and the queue's control-readiness probe needs the hook-driven
    // activity evidence to deliver the create prompt.
    native.pty_hooks = true;
    native.relay_binary = Some(remuda_relay_bin(root).into());

    let mut http = DevServerConfig::loopback(0).with_workspace_root(workspace.clone());
    http.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    let config = ServeConfig {
        http,
        data_dir: data.clone(),
        drivers: LocalDrivers::Native(native),
    };
    let node = remuda_node::compose(&config).expect("compose");

    let home0 = root.join("homes/gen-0");
    let home1 = root.join("homes/gen-1");
    let home2 = root.join("homes/gen-2");
    std::fs::create_dir_all(&home0).expect("home0");

    // Generation 0: a native shell-pty agent. The harness creates the
    // conversation inside its pinned config home at startup. The first turn is
    // the create-time prompt — the delivery path model_pin_launch proves —
    // gated on the built-in catch-all tool call, approved with "1".
    let marker0 = "RESUME_HOME_PARENT_TURN";
    let parent = node
        .create_instance(req(&binary, &home0, marker0, None))
        .await
        .expect("parent create");
    let parent_id = parent.instance.meta.id.clone();
    wait_ready(&node, &parent_id).await;
    let transcript0 = transcript_in(&home0, &workspace, SESSION);
    wait_file(&transcript0, Duration::from_secs(30)).await;
    approve(&node, &parent_id, &parent.command.command_id).await;
    wait_text(&transcript0, marker0, Duration::from_secs(60)).await;
    let gen0 = std::fs::read(&transcript0).expect("parent transcript after turn");
    close_and_wait_exited(&node, &parent_id).await;

    // Generation 1: resume in a fresh home (「继续（终端）」). Before the fix the
    // harness died instantly with "resume: transcript not found" and the
    // instance failed within a second.
    std::fs::create_dir_all(&home1).expect("home1");
    let marker1 = "RESUME_HOME_CHILD_TURN";
    let child1 = node
        .create_instance(req(&binary, &home1, marker1, Some((&parent_id, SESSION))))
        .await
        .expect("resume create accepted");
    let child1_id = child1.instance.meta.id.clone();
    wait_ready(&node, &child1_id).await;
    assert_stays_ready(&node, &child1_id).await;
    let transcript1 = transcript_in(&home1, &workspace, SESSION);
    wait_file(&transcript1, Duration::from_secs(30)).await;
    let staged1 = std::fs::read(&transcript1).expect("staged child transcript");
    assert!(
        String::from_utf8_lossy(&staged1).contains(marker0),
        "the child home received the predecessor conversation"
    );
    // The continued turn writes into the child home only.
    approve(&node, &child1_id, &child1.command.command_id).await;
    wait_text(&transcript1, marker1, Duration::from_secs(60)).await;
    assert_eq!(
        std::fs::read(&transcript0).expect("frozen parent"),
        gen0,
        "resuming appends in the child home, never in the predecessor's"
    );
    close_and_wait_exited(&node, &child1_id).await;
    let gen1 = std::fs::read(&transcript1).expect("child transcript");

    // Generation 2: resume of a resume. Source is generation 1's home.
    std::fs::create_dir_all(&home2).expect("home2");
    let marker2 = "RESUME_HOME_GRANDCHILD_TURN";
    let child2 = node
        .create_instance(req(&binary, &home2, marker2, Some((&child1_id, SESSION))))
        .await
        .expect("second-generation resume accepted");
    let child2_id = child2.instance.meta.id.clone();
    wait_ready(&node, &child2_id).await;
    assert_stays_ready(&node, &child2_id).await;
    let transcript2 = transcript_in(&home2, &workspace, SESSION);
    wait_file(&transcript2, Duration::from_secs(30)).await;
    let staged2 = std::fs::read_to_string(&transcript2).expect("grandchild transcript");
    assert!(
        staged2.contains(marker1),
        "grandchild continues generation 1"
    );
    assert!(
        staged2.contains(marker0),
        "grandchild still has the original turn"
    );
    approve(&node, &child2_id, &child2.command.command_id).await;
    wait_text(&transcript2, marker2, Duration::from_secs(60)).await;
    close_and_wait_exited(&node, &child2_id).await;

    // Both predecessor homes stay byte-frozen across the chain.
    assert_eq!(
        std::fs::read(&transcript0).expect("parent final"),
        gen0,
        "original instance untouched"
    );
    assert_eq!(
        std::fs::read(&transcript1).expect("child1 final"),
        gen1,
        "generation 1 untouched by generation 2"
    );
}

fn req(
    binary: &Path,
    home: &Path,
    prompt: &str,
    resume: Option<(&InstanceId, &str)>,
) -> CreateInstanceRequest {
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
        binary_path: Some(binary.to_string_lossy().into_owned()),
        binary_sha256: None,
        provider_profile_id: "dev-fake".to_owned(),
        permission_mode: "manual".to_owned(),
        sandbox: None,
        prompt: prompt.to_owned(),
        cwd: None,
        delegation: None,
        settings_overlay_path: None,
        claude_config_dir: Some(home.to_string_lossy().into_owned()),
        max_budget_usd: None,
        provider_overlay: None,
        provider_auth_token: None,
        resume_session_id: resume.map(|(_, session)| session.to_owned()),
        resumed_from: resume.map(|(parent, _)| (*parent).clone()),
        effort: None,
        tui: None,
        extra_env: std::collections::BTreeMap::new(),
        capabilities: Default::default(),
        api_route: None,
        api_relay_endpoint: None,
    }
}

fn install_fake_harness(root: &Path) -> PathBuf {
    remuda_testing::sandbox::TempHome::adopt(root).expect("adopt per-test fake root");

    let install = root.join("opt/bin");
    std::fs::create_dir_all(&install).expect("bin dir");
    let source = remuda_testing::ensure_workspace_bin("fake-harness");
    let dest = install.join("claude");
    // Copy, never a wrapper: the harness renders its TUI on stdout, which must
    // stay attached to the PTY for the emulator and the queue's
    // control-readiness probe.
    std::fs::copy(&source, &dest).expect("copy fake harness");
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

fn transcript_in(home: &Path, workspace: &Path, session: &str) -> PathBuf {
    remuda_driver::claude_transcript::project_dir(home, workspace).join(format!("{session}.jsonl"))
}

async fn wait_ready(node: &DevNode, id: &InstanceId) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let instance = node.get_instance(id).expect("instance");
        if instance.lifecycle == remuda_protocol::InstanceLifecycle::Ready {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "instance never became ready: lifecycle={:?} last_error={:?}",
            instance.lifecycle,
            instance.last_error
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A missing-transcript failure shows up as a process that exits within about a
/// second. Ready-then-stable is the proof the resumed harness really resumed.
async fn assert_stays_ready(node: &DevNode, id: &InstanceId) {
    for _ in 0..20 {
        let instance = node.get_instance(id).expect("instance");
        assert_ne!(
            instance.lifecycle,
            remuda_protocol::InstanceLifecycle::Failed,
            "resumed instance failed (harness could not find the conversation?): {:?}",
            instance.last_error
        );
        assert_ne!(
            instance.lifecycle,
            remuda_protocol::InstanceLifecycle::Exited,
            "resumed harness exited: {:?}",
            instance.last_error
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_file(path: &Path, budget: Duration) {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if path.is_file() && std::fs::metadata(path).is_ok_and(|m| m.len() > 0) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no file {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_text(path: &Path, needle: &str, budget: Duration) {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if std::fs::read_to_string(path)
            .unwrap_or_default()
            .contains(needle)
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{needle:?} never written"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Approve the built-in catch-all turn's one tool call (the harness accepts
/// "1"), the interaction model_pin_launch proves. Waits for the approval
/// dialog to render — the prompt is still being typed character by character
/// until then, so an earlier "1" lands in the composer.
async fn approve(node: &DevNode, id: &InstanceId, command_id: &remuda_protocol::CommandId) {
    wait_settled(node, command_id).await;
    wait_for_approval_gate(node, id).await;
    node.submit_command(
        id,
        serde_json::from_value(serde_json::json!({
            "operation": "tty.write",
            "keys": ["1"],
        }))
        .expect("key request"),
    )
    .await
    .expect("approval accepted");
}

async fn wait_for_approval_gate(node: &DevNode, id: &InstanceId) {
    // The fake harness renders "❯ 1. Yes, proceed" on its approval dialog;
    // the emulator-backed screen is where that gate is observable (the same
    // screen the node's own approval interaction reads from).
    let mut last_screen = serde_json::Value::Null;
    let appeared = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let screen = node.screen_read(id).await.expect("screen read");
            last_screen = screen;
            let text = serde_json::to_string(&last_screen).unwrap_or_default();
            if text.contains("Yes, proceed") {
                return true;
            }
            let current = node.get_instance(id).expect("instance");
            assert!(
                current.last_error.as_deref().unwrap_or_default().is_empty(),
                "instance failed while waiting for the approval gate: {:?}",
                current.last_error
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();
    assert!(
        appeared,
        "approval gate never appeared on screen for {id:?}\n{last_screen}"
    );
}

async fn wait_settled(node: &DevNode, command_id: &remuda_protocol::CommandId) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if node.get_command(command_id).expect("command").state
                == remuda_protocol::CommandState::Settled
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("command settles");
}

async fn close_and_wait_exited(node: &DevNode, id: &InstanceId) {
    node.submit_command(
        id,
        serde_json::from_value(serde_json::json!({ "operation": "close" })).expect("close request"),
    )
    .await
    .expect("close accepted");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let instance = node.get_instance(id).expect("instance");
        if instance.lifecycle == remuda_protocol::InstanceLifecycle::Exited {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never exited: {instance:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
