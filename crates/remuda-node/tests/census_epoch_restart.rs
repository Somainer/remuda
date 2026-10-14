//! c-usagefu r19 item 1 — stop-incomplete census blocks are scoped to the Node
//! that built the driver, exercised ONLY through the REAL native construction
//! path (`native_driver_registry` + `compose`; no manual `node_epoch`
//! injection).
//!
//! 1. Node A creates a real codex shell-pty instance; after its forced
//!    stop-incomplete close, an otherwise-eligible same-cwd rollout stays
//!    refused to prompt-only discovery while A runs.
//! 2. A's real shutdown retires ITS block without recording a close; a
//!    second live Node C (its own registry, another cwd) is unaffected.
//! 3. An in-process recompose Node B launches its OWN codex driver from its
//!    OWN registry in the same cwd; B's own file adapter binds an eligible
//!    rollout (the census A blocked is gone). C retires only with C.
#![cfg(unix)]
// The node test binary only enters the native shell-pty agent arm when
// REMUDA_PTY_CARRIER=native is set in its process env, so the test re-execs
// itself under that env (the unsafe std::env::set_var is forbidden in this
// crate).

use remuda_driver::adapters::codex_discovery::census_lifetime_is_live;
use remuda_driver::adapters::{AdapterHome, FileSignalAdapter};
use remuda_node::{
    CreateInstanceRequest, CreateInstanceResponse, DevNode, DevServerConfig, LocalDrivers,
    NativeDriverConfig, ServeConfig,
};
use remuda_protocol::{AgentKind, DriverKind};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const SESSION: &str = "r19epoch";
const PROMPT: &str = "r19 shared epoch restart prompt";
const RUN_MARKER: &str = "REMUDA_R19_CENSUS_CHILD";
const CARRIER_ENV: &str = "REMUDA_PTY_CARRIER";
const EMULATOR_ENV: &str = "REMUDA_PTY_EMULATOR";
const HOOKS_ENV: &str = "REMUDA_PTY_HOOKS";

/// Fake codex on PATH: answers version probes, fires a real SessionStart hook
/// through the materialized relay (so the driver's file adapter gets the
/// hook-confirmed session), then idles until the real stop ladder kills its
/// group.
fn install_fake_codex(root: &Path) -> PathBuf {
    let install = root.join("opt/bin");
    std::fs::create_dir_all(&install).unwrap();
    let dest = install.join("codex");
    std::fs::write(
        &dest,
        r##"#!/bin/sh
case "$1" in --version|version) echo 'codex 0.154.0'; exit 0;; esac
# Real codex fires SessionStart at startup through the hook relay the Node
# materialized into $CODEX_HOME/hooks.json. The first registered command IS
# SessionStart. Fire it inline (the relay acks {} and exits); this sh process
# is the Node-spawned child, so the relay's ppid matches the agent pid.
if [ -n "$CODEX_HOME" ] && [ -f "$CODEX_HOME/hooks.json" ]; then
    cmd=$(sed -n 's/^[[:space:]]*"command": "\(.*\)",$/\1/p' "$CODEX_HOME/hooks.json" | head -1)
    if [ -n "$cmd" ]; then
        # The command is single-quote tokenized by the materializer; word
        # split it (no glob chars) into argv and exec with the payload on
        # stdin — a direct exec keeps the relay's ppid equal to ours.
        eval "set -- $cmd"
        printf '{"session_id":"r19epoch","cwd":"%s"}' "$PWD" | exec "$@" >"$PWD/r19-fake.log" 2>&1
    fi
fi
sleep 60
"##,
    )
    .unwrap();
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    dest
}

/// The real remuda binary the materialized hook relay invokes.
fn relay_bin() -> PathBuf {
    if let Ok(path) = std::env::var("REMUDA_NATIVE_LIVE_RELAY") {
        return PathBuf::from(path);
    }
    let deps = std::env::current_exe().unwrap();
    let deps = deps.parent().unwrap();
    for candidate in [deps.join("remuda"), deps.join("../remuda")] {
        if candidate.is_file() {
            return candidate;
        }
    }
    remuda_testing::workspace_root().join("target/debug/remuda")
}

fn compose_native(root: &Path, workspace: PathBuf) -> DevNode {
    let mut native = NativeDriverConfig::new(root.join("node-data"));
    // Hooks on with a no-op relay so the real shell-pty codex path runs its
    // confirmed-launch registration and file-adapter spawn; the relay binary
    // itself is never executed by the fixture.
    native.pty_hooks = true;
    native.relay_binary = Some(Box::new(relay_bin()));
    let mut http = DevServerConfig::loopback(0);
    http.workspace_root = workspace.clone();
    http.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    std::fs::create_dir_all(&http.workspace_root).unwrap();
    remuda_node::compose(&ServeConfig {
        http,
        data_dir: root.join("node-data"),
        drivers: LocalDrivers::Native(native),
    })
    .expect("compose native Node")
}

#[tokio::test(flavor = "current_thread")]
async fn one_registry_backs_only_one_node_and_fresh_registries_get_distinct_epochs() {
    // c-usagefu r20 item 3: a registry (including a clone of it) may be
    // adopted by exactly ONE DevNode. Building two Nodes from the same
    // registry object graph would make the second share the first's shutdown
    // flag and census-retirement bucket.
    let root_a = tempfile::tempdir().unwrap();
    let root_c = tempfile::tempdir().unwrap();
    let config = || {
        let mut http = DevServerConfig::loopback(0);
        http.workspace_roots = Some(remuda_testing::test_workspace_roots!());
        http
    };
    let store = |root: &Path| {
        Arc::new(remuda_node::MemoryStore::open_journaled(root.join("node-data"), 64).unwrap())
    };

    let registry = remuda_node::native_driver_registry(NativeDriverConfig::new(
        root_a.path().join("node-data"),
    ))
    .expect("registry");
    let epoch_clone = registry.census_epoch();

    // The CLONE backs Node A.
    let _node_a =
        remuda_node::DevNode::with_parts(&config(), store(root_a.path()), registry.clone())
            .expect("first adoption (registry clone) succeeds");

    // The ORIGINAL cannot back Node C: same adopted flag (shared Arc).
    let second = remuda_node::DevNode::with_parts(&config(), store(root_c.path()), registry);
    assert!(
        second.is_err(),
        "a cloned registry must not be adopted by a second Node"
    );

    // Independent registries mint independent epochs (so properly built Nodes
    // can never share shutdown/retirement ownership).
    let other = remuda_node::native_driver_registry(NativeDriverConfig::new(
        root_c.path().join("node-data"),
    ))
    .expect("second registry");
    assert_ne!(
        format!("{epoch_clone:?}"),
        format!("{:?}", other.census_epoch()),
        "fresh registries must mint distinct epochs"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn failed_node_construction_rolls_back_registry_adoption() {
    // c-usagefu r21 item 4: a constructor failure must not consume the
    // registry's single-adoption slot; the corrected retry with the SAME
    // registry succeeds.
    let root = tempfile::tempdir().unwrap();
    let registry =
        remuda_node::native_driver_registry(NativeDriverConfig::new(root.path().join("node-data")))
            .expect("registry");
    let store = || {
        Arc::new(
            remuda_node::MemoryStore::open_journaled(root.path().join("node-data"), 64).unwrap(),
        )
    };

    // A config with no workspace root: workspace construction fails after the
    // adoption reservation.
    let mut bad = DevServerConfig::loopback(0);
    bad.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    bad.workspace_root = root.path().join("does-not-matter");
    bad.workspace_roots = Some(vec![]);
    let bad_result = remuda_node::DevNode::with_parts(&bad, store(), registry.clone());
    assert!(
        bad_result.is_err(),
        "empty workspace roots must fail Node construction"
    );

    // Corrected config with the retained ORIGINAL registry now succeeds.
    let mut good = DevServerConfig::loopback(0);
    good.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    std::fs::create_dir_all(&good.workspace_root).unwrap();
    let _retry = remuda_node::DevNode::with_parts(&good, store(), registry)
        .expect("the registry must still be adoptable after the failed attempt");
}

fn codex_request(binary: PathBuf) -> CreateInstanceRequest {
    CreateInstanceRequest {
        origin: remuda_protocol::InputOrigin::Human,
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
        cwd: None,
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
        extra_env: std::collections::BTreeMap::new(),
        capabilities: Default::default(),
        api_route: None,
        api_relay_endpoint: None,
    }
}

/// The file-adapter shadow home for an instance:
/// `<data_dir>/instances/<id>/launch/codex-home`.
fn shadow_home(root: &Path, id: &remuda_protocol::InstanceId) -> PathBuf {
    root.join("node-data")
        .join("instances")
        .join(id.as_id().as_str())
        .join("launch")
        .join("codex-home")
}

/// Write an otherwise-eligible rollout (matching session/cwd/prompt) into a
/// shadow codex home under `session`.
fn write_rollout_into(shadow_home: &Path, cwd: &Path, prompt_owner: &str, session: &str) {
    remuda_driver::adapters::codex_discovery::record_input(cwd, prompt_owner, PROMPT);
    let started = time::OffsetDateTime::now_utc();
    let month_dir = shadow_home.join("sessions").join(format!(
        "{:04}/{:02}/{:02}",
        started.year(),
        u8::from(started.month()),
        started.day()
    ));
    std::fs::create_dir_all(&month_dir).unwrap();
    let rfc = started
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    std::fs::write(
        month_dir.join(format!("rollout-{session}.jsonl")),
        format!(
            "{{\"timestamp\":\"{rfc}\",\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session}\",\"session_id\":\"{session}\",\"cwd\":\"{}\",\"timestamp\":\"{rfc}\"}}}}\n\
             {{\"timestamp\":\"{rfc}\",\"ordinal\":1,\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"id\":\"m\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"{PROMPT}\"}}],\"internal_chat_message_metadata_passthrough\":{{\"content_item_kinds\":[\"user.text\"]}}}}}}\n",
            cwd.display()
        ),
    )
    .unwrap();
}

/// A fresh prompt-only discoverer with its OWN owner and a launch floor
/// sampled AFTER the rollout was written: the strict floor and owner-prompt
/// checks pass, so the ONLY thing that can refuse it is the census overlap.
fn prompt_only_binds(shadow_home: &Path, cwd: &Path) -> bool {
    // Owner and rollout are created FIRST (owner's prompt evidence + a fresh,
    // unclaimed rollout); the discoverer is constructed AFTER with a floor in
    // the tiny window between creation and now, so the ONLY possible rejection
    // is the census sole-live overlap — never the launch floor, owner prompt,
    // stale claim or index tier.
    let owner = format!("probe-{}", std::process::id());
    write_rollout_into(shadow_home, cwd, &owner, SESSION);
    let floor = time::OffsetDateTime::now_utc() + time::Duration::milliseconds(100);
    let mut adapter = remuda_driver::adapters::codex_adapter::CodexAdapter::new(AdapterHome {
        home: shadow_home.to_path_buf(),
        cwd: cwd.to_path_buf(),
        pid: None,
        launched_at: Some(floor),
        resume_session_id: None,
    })
    .with_instance_key(Some(owner));
    // Wait past the floor so the rollout (created just before it) is eligible
    // on every check EXCEPT the census overlap.
    std::thread::sleep(std::time::Duration::from_millis(150));
    let _ = adapter.poll();
    adapter.binding().is_some()
}

async fn launch_real_codex(
    node: &DevNode,
    root: &Path,
    workspace: &Path,
) -> (CreateInstanceResponse, PathBuf, PathBuf) {
    let binary = install_fake_codex(root);
    let created = node
        .create_instance(codex_request(binary))
        .await
        .expect("create codex instance");
    let cwd = std::fs::canonicalize(workspace).unwrap();
    let home = shadow_home(root, &created.instance.meta.id);
    (created, cwd, home)
}

async fn live_driver(
    node: &DevNode,
    id: &remuda_protocol::InstanceId,
) -> Arc<dyn remuda_node::Driver> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(driver) = node.test_instance_driver(id).await {
            return driver;
        }
        assert!(tokio::time::Instant::now() < deadline, "driver registers");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Wait until the live driver is registered, then until its real file adapter
/// reports a bound session.
async fn wait_driver_bound(node: &DevNode, id: &remuda_protocol::InstanceId, what: &str) {
    let driver = live_driver(node, id).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !driver.test_codex_bound() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        driver.test_codex_bound(),
        "{what}: the real codex adapter binds"
    );
}

async fn close_instance(node: &DevNode, id: &remuda_protocol::InstanceId) {
    node.submit_command(
        id,
        remuda_node::InstanceCommandRequest {
            origin: remuda_protocol::InputOrigin::Human,
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
            effort_ultracode: None,
            effort_axis: None,
            effort_request_id: None,
            permission_mode: None,
        },
    )
    .await
    .expect("close command settles");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_native_epoch_retirement_unblocks_recomposed_native_discovery() {
    // Re-exec under the native-carrier env before touching any driver.
    if std::env::var(RUN_MARKER).is_err() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "shared_native_epoch_retirement_unblocks_recomposed_native_discovery",
                "--nocapture",
            ])
            .env(RUN_MARKER, "1")
            .env(CARRIER_ENV, "native")
            .env(EMULATOR_ENV, "1")
            .env(HOOKS_ENV, "1")
            .output()
            .expect("re-exec native-carrier child");
        assert!(
            output.status.success(),
            "re-exec child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    // ---- Node A: a REAL codex instance from the real native registry. ----
    let root_a = tempfile::tempdir().unwrap();
    let workspace_a = root_a.path().join("workspace");
    let node_a = compose_native(root_a.path(), workspace_a.clone());
    let (created_a, cwd_a, home_a) = launch_real_codex(&node_a, root_a.path(), &workspace_a).await;
    let id_a = created_a.instance.meta.id.clone();
    // The driver registers before its file adapter can bind; write the
    // eligible rollout into the shadow home FIRST, then wait for the bind.
    write_rollout_into(&home_a, &cwd_a, id_a.as_id().as_str(), SESSION);
    wait_driver_bound(&node_a, &id_a, "Node A").await;

    // Forced incomplete close on the real driver.
    live_driver(&node_a, &id_a)
        .await
        .test_force_stop_incomplete();
    close_instance(&node_a, &id_a).await;
    assert!(
        census_lifetime_is_live(&cwd_a, id_a.as_id().as_str()),
        "A's interval stays open after the incomplete close"
    );
    // Give the adapter a poll cycle to bind the eligible rollout; the census
    // owned by A still makes an independent prompt-only launch refuse.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(
        !prompt_only_binds(&home_a, &cwd_a),
        "an otherwise-eligible rollout is refused while A's block is live"
    );

    // ---- Node C in a different root/cwd, still live. ----
    let root_c = tempfile::tempdir().unwrap();
    let workspace_c = root_c.path().join("workspace");
    let node_c = compose_native(root_c.path(), workspace_c.clone());
    let (created_c, cwd_c, home_c) = launch_real_codex(&node_c, root_c.path(), &workspace_c).await;
    let id_c = created_c.instance.meta.id.clone();
    write_rollout_into(&home_c, &cwd_c, id_c.as_id().as_str(), SESSION);
    wait_driver_bound(&node_c, &id_c, "Node C").await;
    live_driver(&node_c, &id_c)
        .await
        .test_force_stop_incomplete();
    close_instance(&node_c, &id_c).await;
    assert!(census_lifetime_is_live(&cwd_c, id_c.as_id().as_str()));

    // ---- A's real shutdown retires A only. ----
    node_a.shutdown().await.expect("A shuts down");
    assert!(
        !census_lifetime_is_live(&cwd_a, id_a.as_id().as_str()),
        "A's block retires with A's shutdown"
    );
    assert!(
        census_lifetime_is_live(&cwd_c, id_c.as_id().as_str()),
        "a second live Node's block is unaffected by A's shutdown"
    );

    // ---- In-process recompose Node B in A's SAME cwd. ----
    //
    // B is composed from a fresh registry (fresh epoch) but its workspace is
    // A's directory. B builds its OWN codex driver; the rollout is otherwise
    // eligible and A's stale interval is gone, so B's own file adapter must
    // bind — proving the unblock through B's driver, not a hand-built probe.
    let root_b = tempfile::tempdir().unwrap();
    let node_b = compose_native(root_b.path(), workspace_a.clone());
    let (created_b, _cwd_b, home_b) = launch_real_codex(&node_b, root_b.path(), &workspace_a).await;
    let id_b = created_b.instance.meta.id.clone();
    // B's own launch floor was sampled before the rollout; write the eligible
    // rollout into B's fresh shadow home and let B's OWN adapter poll it.
    write_rollout_into(&home_b, &cwd_a, id_b.as_id().as_str(), SESSION);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let driver_b = live_driver(&node_b, &id_b).await;
    while !driver_b.test_codex_bound() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        driver_b.test_codex_bound(),
        "the replacement Node's OWN driver binds in A's cwd after A retired"
    );
    node_b.shutdown().await.expect("B shuts down");

    // C's own shutdown finally retires C.
    node_c.shutdown().await.expect("C shuts down");
    assert!(
        !census_lifetime_is_live(&cwd_c, id_c.as_id().as_str()),
        "C's block retires with C"
    );

    // Cleanup detached test census state.
    remuda_driver::adapters::codex_discovery::forget_lifetime(&cwd_c, id_c.as_id().as_str());
    remuda_driver::adapters::codex_discovery::forget_lifetime(&cwd_a, id_b.as_id().as_str());
}
