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
//!
//! c-usagefu r16 item 9: the exact survivor pid comes from the LAUNCHED FIXTURE
//! over a test-private channel (a file under this run's tempdir), never from a
//! host-wide `/proc` scan that could match an unrelated `sleep`. The test
//! captures the process identity (pid + start time + group) and re-verifies it
//! immediately before signalling, and a drop guard reaps the survivor even when
//! an assertion fails — it can never signal a recycled/unrelated pid.

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
/// Test-private channel: the fixture writes its escaped descendant's exact pid
/// here (a path unique to this run's tempdir). Deliberately NOT `REMUDA_`-
/// prefixed — the driver scrubs that prefix from child env (token hygiene).
const SURVIVOR_PID_FILE_ENV: &str = "R16_SURVIVOR_PID_FILE";
/// Env the fake codex reads to know how long the escaped sleeper lives.
/// Unique survivor lifetime marker: `setsid … sleep 177`.
const SURVIVOR_SECONDS: u32 = 177;

/// Captured identity of the fixture's escaped survivor. `start_ticks` is the
/// `/proc` field-22 start time: a pid reused by an unrelated process never
/// matches, so the test only ever signals the exact process it launched.
#[derive(Clone, Copy, Debug)]
struct SurvivorIdentity {
    pid: i32,
    start_ticks: u64,
    pgrp: i32,
}

impl SurvivorIdentity {
    /// True only if this pid is still alive, still our `setsid sleep 177`, and
    /// still at the captured start time (not a recycled pid). Read entirely from
    /// `/proc` — no signal is sent here.
    fn still_ours(self) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", self.pid)) else {
            return false;
        };
        let Some(after_comm) = stat.rsplit_once(')').map(|(_, rest)| rest.trim_start()) else {
            return false;
        };
        let mut fields = after_comm.split_whitespace();
        // after comm: state(0) ppid(1) pgrp(2) … starttime(19).
        let Some(pgrp) = fields.nth(2).and_then(|v| v.parse::<i32>().ok()) else {
            return false;
        };
        let Some(start_ticks) = fields.nth(16).and_then(|v| v.parse::<u64>().ok()) else {
            return false;
        };
        if start_ticks != self.start_ticks || pgrp != self.pgrp {
            return false;
        }
        std::fs::read_to_string(format!("/proc/{}/cmdline", self.pid)).is_ok_and(|cmdline| {
            let mut parts = cmdline
                .split('\0')
                .map(str::as_bytes)
                .filter(|p| !p.is_empty());
            let argv0 = parts.next().unwrap_or(&[]);
            let seconds = parts.next().unwrap_or(&[]);
            argv0.ends_with(b"/sleep") && seconds == SURVIVOR_SECONDS.to_string().as_bytes()
        })
    }
}

/// Reaps the verified survivor on drop so a panic never leaks the `sleep`, and
/// only signals when the identity still matches the launched fixture.
struct SurvivorGuard {
    identity: SurvivorIdentity,
    signalled: bool,
}

impl SurvivorGuard {
    /// Verify ownership, then signal the exact survivor. Refuses (and leaves it
    /// for the OS / next Node) if the pid was recycled — never a blind kill.
    fn signal_verified(&mut self) {
        assert!(
            self.identity.still_ours(),
            "refusing to signal: pid {} no longer matches the launched fixture's identity",
            self.identity.pid
        );
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(self.identity.pid),
            Some(nix::sys::signal::Signal::SIGKILL),
        )
        .expect("signal the verified fixture survivor");
        self.signalled = true;
    }
}

impl Drop for SurvivorGuard {
    fn drop(&mut self) {
        if !self.signalled && self.identity.still_ours() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(self.identity.pid),
                Some(nix::sys::signal::Signal::SIGKILL),
            );
        }
    }
}

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
    // A channel path private to THIS run: only this fixture writes it.
    let survivor_pid_file = root.join(format!("survivor-{}.pid", std::process::id()));
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
        .create_instance(create_req(
            binary,
            workspace.clone(),
            fake_path,
            &survivor_pid_file,
        ))
        .await
        .expect("create the real shell-pty codex instance");
    let id = created.instance.meta.id.clone();
    let owner = id.as_id().as_str().to_owned();

    // The EXACT escaped survivor, reported by the launched fixture over its
    // private pid file — not a host-wide /proc scan.
    let pid = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        loop {
            if let Ok(text) = std::fs::read_to_string(&survivor_pid_file)
                && let Ok(parsed) = text.trim().parse::<i32>()
                && parsed > 0
            {
                break parsed;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the fixture reports its escaped survivor pid"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };

    // Capture and verify the identity BEFORE anything signals it.
    let identity = capture_identity(pid);
    assert!(
        identity.still_ours(),
        "the reported pid is our live setsid sleep {SURVIVOR_SECONDS}"
    );
    assert_eq!(
        identity.pgrp, identity.pid,
        "the survivor is its own setsid session/group leader"
    );
    // Ownership is the start-time identity, not mere liveness: the SAME live
    // pid with a stale start tick (what a recycled pid would present) must be
    // rejected, so the test never signals a process it did not launch.
    assert!(
        !SurvivorIdentity {
            pid: identity.pid,
            start_ticks: identity.start_ticks.wrapping_add(1),
            pgrp: identity.pgrp,
        }
        .still_ours(),
        "a live pid with the wrong start time is not ours"
    );
    assert!(
        !SurvivorIdentity {
            pid: identity.pid,
            start_ticks: identity.start_ticks,
            pgrp: identity.pgrp + 1,
        }
        .still_ours(),
        "a live pid in the wrong group is not ours"
    );
    let mut survivor = SurvivorGuard {
        identity,
        signalled: false,
    };

    // The REAL Node close command: production stop ladder -> the escaped
    // survivor is named stop-incomplete -> the bounded watcher keeps the
    // census interval live (the full SIGINT/SIGHUP/SIGKILL ladder can take a
    // few seconds).
    node.submit_command(&id, close_request())
        .await
        .expect("the production close command settles");
    tokio::time::sleep(Duration::from_secs(6)).await;

    assert!(identity.still_ours(), "the survivor outlives the ladder");
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

    // The survivor exits afterwards: re-verify identity first, then signal the
    // exact process. Nothing is left watching to close it.
    survivor.signal_verified();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        !identity.still_ours(),
        "the verified survivor actually exited"
    );
    assert!(
        remuda_driver::termination::census_interval_is_live(&workspace, &owner),
        "a post-shutdown exit is never turned into a recorded close"
    );

    remuda_driver::termination::forget_census_for_test(&workspace, &owner);
}

/// Read the `/proc/<pid>/stat` identity (start time field 22, pgrp field 5) of
/// the fixture-reported survivor.
fn capture_identity(pid: i32) -> SurvivorIdentity {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .unwrap_or_else(|err| panic!("the reported survivor {pid} exists: {err}"));
    let after_comm = stat
        .rsplit_once(')')
        .map(|(_, rest)| rest.trim_start())
        .expect("parse /proc stat");
    let mut fields = after_comm.split_whitespace();
    let pgrp = fields
        .nth(2)
        .and_then(|v| v.parse::<i32>().ok())
        .expect("pgrp field");
    let start_ticks = fields
        .nth(16)
        .and_then(|v| v.parse::<u64>().ok())
        .expect("starttime field");
    SurvivorIdentity {
        pid,
        start_ticks,
        pgrp,
    }
}

fn loopback_config(root: &Path) -> DevServerConfig {
    let mut http = DevServerConfig::loopback(0);
    http.workspace_root = root.join("workspace");
    http.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    std::fs::create_dir_all(&http.workspace_root).expect("workspace");
    http
}

/// A fake codex that answers the version pin and forks an escaped
/// `setsid sleep` the production ladder must name stop-incomplete. The outer
/// shell writes the new session leader's pid (`$!`, which is the setsid/sleep
/// leader because setsid execs in place) to the test-private
/// `$R16_SURVIVOR_PID_FILE`, so the test learns the exact descendant pid
/// without scanning the host (c-usagefu r16 item 9). Returns the binary path
/// and a PATH that lets the per-session launch shim resolve it (the shim
/// searches PATH with its own directory excluded).
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
         printf '%s' \"$!\" > \"$R16_SURVIVOR_PID_FILE\"\n\
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
        effort_axis: None,
        effort_request_id: None,
        effort_ultracode: None,
        permission_mode: None,
    }
}

fn create_req(
    binary: PathBuf,
    cwd: PathBuf,
    path: String,
    survivor_pid_file: &Path,
) -> CreateInstanceRequest {
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
        extra_env: std::collections::BTreeMap::from([
            ("PATH".to_owned(), path),
            (
                SURVIVOR_PID_FILE_ENV.to_owned(),
                survivor_pid_file.to_string_lossy().into_owned(),
            ),
        ]),
        capabilities: Default::default(),
        api_route: None,
        api_relay_endpoint: None,
    }
}
