//! c-usagefu r15 item 8(a): the stop-incomplete shutdown guarantee driven
//! through the NODE's real shutdown wiring — `DevNode::shutdown` (the exact
//! call `reclaim.rs::shutdown` makes after closing every instance), not a
//! direct `shutdown_pending_terminations` helper.
//!
//! A real shell-pty codex instance launches through the Node with a binary
//! that forks a `setsid sleep` descendant (nothing hand-installed, no
//! substituted stop outcome). Closing the instance runs the production stop
//! ladder, names the escaped survivor stop-incomplete, and arms the Node's
//! bounded termination watcher. The REAL Node shutdown must cancel/await the
//! watcher and drop the suppressed guards WITHOUT closing the census
//! interval — even after the survivor later exits.
//!
//! The shell-pty native carrier is selected by `REMUDA_PTY_CARRIER=native`
//! read from the Node's own process environment, so (like the other native
//! shell-pty node tests) the test re-execs itself once with the carrier env
//! rather than mutating the shared test process environment.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use remuda_node::{
    CommandAction, CreateInstanceRequest, DevServerConfig, InstanceCommandRequest, LocalDrivers,
    NativeDriverConfig, ServeConfig, compose,
};
use remuda_protocol::{AgentKind, DriverKind, InputOrigin};

const RUN_MARKER: &str = "REMUDA_R15_ITEM8_CHILD";
const CARRIER_ENV: &str = "REMUDA_PTY_CARRIER";
const EMULATOR_ENV: &str = "REMUDA_PTY_EMULATOR";
/// Unique survivor lifetime marker: `setsid sleep 177`.
const SURVIVOR_SECONDS: u32 = 177;

#[test]
fn node_shutdown_drops_a_stop_incomplete_watcher_without_closing_the_interval() {
    if std::env::var(RUN_MARKER).is_err() {
        // Re-exec with the native shell-pty carrier selected.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "node_shutdown_drops_a_stop_incomplete_watcher_without_closing_the_interval",
                "--nocapture",
            ])
            .env(RUN_MARKER, "1")
            .env(CARRIER_ENV, "native")
            .env(EMULATOR_ENV, "1")
            .output()
            .expect("re-exec native carrier child");
        if !output.status.success() {
            panic!(
                "native carrier child failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
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
    runtime.block_on(run());
}

async fn run() {
    let root_dir = tempfile::tempdir().expect("tempdir");
    let root = root_dir.path();
    let data_dir = root.join("node-data");
    let workspace = root.join("workspace");
    let (binary, fake_path) = install_fake_codex(root);

    let mut native = NativeDriverConfig::new(data_dir.clone());
    native.pty_hooks = false;
    let config = ServeConfig {
        http: loopback_config(root),
        data_dir,
        drivers: LocalDrivers::Native(native),
    };
    let node = compose(&config).expect("compose DevNode");

    let created = node
        .create_instance(create_req(binary, workspace.clone(), fake_path))
        .await
        .expect("create the real shell-pty codex instance");
    let id = created.instance.meta.id.clone();
    let owner = id.as_id().as_str().to_owned();

    // The child forks the setsid survivor shortly after spawn.
    let survivor = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(pid) = survivor_pid(SURVIVOR_SECONDS) {
                break pid;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the setsid survivor appears"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };

    // The REAL Node close command: production stop ladder -> the escaped
    // survivor is named stop-incomplete -> the bounded watcher keeps the
    // census interval live (the full SIGINT/SIGHUP/SIGKILL ladder can take a
    // few seconds).
    node.submit_command(&id, close_request())
        .await
        .expect("the production close command settles");
    tokio::time::sleep(Duration::from_secs(6)).await;

    assert!(
        remuda_driver::termination::census_interval_is_live(&workspace, &owner),
        "the stop-incomplete survivor keeps the production interval live before shutdown"
    );

    // The exact reclaim.rs shutdown call: workers/pumps stop, every instance
    // closes, and the termination supervisor is shut down while the runtime is
    // alive.
    node.shutdown().await.expect("the real DevNode shutdown");

    assert!(
        remuda_driver::termination::census_interval_is_live(&workspace, &owner),
        "Node shutdown drops the suppressed guards WITHOUT recording a close"
    );

    // The survivor exits afterwards: nothing is left watching to close it.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(survivor),
        Some(nix::sys::signal::Signal::SIGKILL),
    )
    .expect("kill the survivor");
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        remuda_driver::termination::census_interval_is_live(&workspace, &owner),
        "a post-shutdown exit is never turned into a recorded close"
    );

    remuda_driver::termination::forget_census_for_test(&workspace, &owner);
}

fn loopback_config(root: &Path) -> DevServerConfig {
    let mut http = DevServerConfig::loopback(0);
    http.workspace_root = root.join("workspace");
    http.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    std::fs::create_dir_all(&http.workspace_root).expect("workspace");
    http
}

/// A fake codex that answers the version pin and forks an escaped
/// `setsid sleep` the production ladder must name stop-incomplete. Returns
/// the binary path and a PATH that lets the per-session launch shim resolve
/// it (the shim searches PATH with its own directory excluded).
fn install_fake_codex(root: &Path) -> (PathBuf, String) {
    use std::os::unix::fs::PermissionsExt;
    let install = root.join("opt/bin");
    std::fs::create_dir_all(&install).expect("bin dir");
    let dest = install.join("codex");
    std::fs::write(
        &dest,
        "#!/bin/sh\n\
         case \"$1\" in --version|version) echo 'codex 0.154.0'; exit 0;; esac\n\
         /usr/bin/setsid /usr/bin/sleep 177 &\n\
         wait\n",
    )
    .expect("write fake codex");
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let path = format!(
        "{}:{}",
        install.display(),
        std::env::var("PATH").expect("the child process PATH")
    );
    (dest, path)
}

/// Find the escaped survivor by its unique sleep-marker in /proc cmdlines.
fn survivor_pid(marker_seconds: u32) -> Option<i32> {
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let mut parts = cmdline.split(|byte| *byte == 0);
        let argv0 = parts.next().unwrap_or(&[]);
        let seconds = parts.next().unwrap_or(&[]);
        if argv0.ends_with(b"/sleep") && seconds == marker_seconds.to_string().as_bytes() {
            return Some(pid);
        }
    }
    None
}

fn close_request() -> InstanceCommandRequest {
    InstanceCommandRequest {
        origin: InputOrigin::Human,
        command_id: None,
        operation: CommandAction::Close,
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
    }
}

fn create_req(binary: PathBuf, cwd: PathBuf, path: String) -> CreateInstanceRequest {
    CreateInstanceRequest {
        origin: InputOrigin::Human,
        agent_credential: None,
        command_id: None,
        instance_id: None,
        host_id: None,
        workspace_id: None,
        kind: AgentKind::Codex,
        driver: DriverKind::ShellPty,
        model: "fake".to_owned(),
        args: Vec::new(),
        provider_profile_id: "dev-fake".to_owned(),
        permission_mode: "dontAsk".to_owned(),
        sandbox: None,
        prompt: String::new(),
        cwd: Some(cwd.to_string_lossy().into_owned()),
        delegation: None,
        settings_overlay_path: None,
        claude_config_dir: None,
        binary_path: Some(binary.to_string_lossy().into_owned()),
        binary_sha256: None,
        max_budget_usd: None,
        provider_overlay: None,
        provider_auth_token: None,
        resume_session_id: None,
        resumed_from: None,
        effort: None,
        tui: None,
        extra_env: std::collections::BTreeMap::from([("PATH".to_owned(), path)]),
        capabilities: Default::default(),
        api_route: None,
        api_relay_endpoint: None,
    }
}
