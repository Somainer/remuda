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

/// Compose a native Node. When `hooks` is false the fixture codex must NOT
/// fire SessionStart and the real shell-pty path skips hook materialization,
/// so file attribution can only occur through the prompt tier.
fn compose_native_with(root: &Path, workspace: PathBuf, hooks: bool) -> DevNode {
    let mut native = NativeDriverConfig::new(root.join("node-data"));
    native.pty_hooks = hooks;
    if hooks {
        native.relay_binary = Some(Box::new(relay_bin()));
    }
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

fn compose_native(root: &Path, workspace: PathBuf) -> DevNode {
    compose_native_with(root, workspace, true)
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
        memory_context: None,
        initiator: None,
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

/// Record the matching prompt under `prompt_owner` (the prompt half, kept
/// separate from file writing so a probe can construct its discoverer and open
/// its census lifetime FIRST, then record the prompt, then write the rollout).
fn record_matching_prompt(cwd: &Path, prompt_owner: &str) {
    remuda_driver::adapters::codex_discovery::record_input(cwd, prompt_owner, PROMPT);
}

/// Write an otherwise-eligible rollout (matching session/cwd/prompt) into a
/// shadow codex home under `session`, timestamped at `created_at`.
///
/// c-usagefu r21 items 1/2: callers create the owner's census lifetime and
/// sample its launch floor BEFORE calling this, and pass `created_at >=
/// floor`, so the file passes the strict floor / prompt / claim tiers.
fn write_rollout_into(
    shadow_home: &Path,
    cwd: &Path,
    prompt_owner: &str,
    session: &str,
    created_at: time::OffsetDateTime,
) {
    record_matching_prompt(cwd, prompt_owner);
    write_rollout_file(shadow_home, cwd, session, created_at);
}

/// File-only half of [`write_rollout_into`]: write the session_meta + matching
/// user-message rollout and backdate its mtime to `created_at`.
fn write_rollout_file(
    shadow_home: &Path,
    cwd: &Path,
    session: &str,
    created_at: time::OffsetDateTime,
) {
    let month_dir = shadow_home.join("sessions").join(format!(
        "{:04}/{:02}/{:02}",
        created_at.year(),
        u8::from(created_at.month()),
        created_at.day()
    ));
    std::fs::create_dir_all(&month_dir).unwrap();
    let rfc = created_at
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let path = month_dir.join(format!("rollout-{session}.jsonl"));
    std::fs::write(
        &path,
        format!(
            "{{\"timestamp\":\"{rfc}\",\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session}\",\"session_id\":\"{session}\",\"cwd\":\"{}\",\"timestamp\":\"{rfc}\"}}}}\n             {{\"timestamp\":\"{rfc}\",\"ordinal\":1,\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"id\":\"m\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"{PROMPT}\"}}],\"internal_chat_message_metadata_passthrough\":{{\"content_item_kinds\":[\"user.text\"]}}}}}}\n",
            cwd.display()
        ),
    )
    .unwrap();
    // Backdate mtime to created_at so per-file mtime pre-filters agree.
    let secs = created_at.unix_timestamp().max(0);
    let nsecs = created_at.nanosecond();
    let atime = std::time::UNIX_EPOCH + std::time::Duration::new(secs as u64, nsecs);
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    let _ = file.set_times(
        std::fs::FileTimes::new()
            .set_accessed(atime)
            .set_modified(atime),
    );
}

/// Prompt-only discoverer with an explicit owner (used for B's positive
/// control: the owner is B's real instance id, so the census finds B's
/// registered interval). The rollout is written AT `floor` (which is after
/// B's launch), so the strict floor, prompt-fingerprint and stale-claim
/// tiers all pass; only the census overlap can refuse.
///
/// c-usagefu r22 item 2 ordering: the pending discoverer is constructed and
/// its census lifetime opened (with_instance_key) FIRST; the owner's matching
/// prompt is recorded only THEN; the unique unclaimed rollout at/after the
/// floor is written last. Constructing in any other order lets the proof pass
/// on tiers other than the census overlap it targets.
fn prompt_only_binds_with_owner(shadow_home: &Path, cwd: &Path, owner: &str) -> bool {
    let floor = time::OffsetDateTime::now_utc();
    let session = format!("{SESSION}-probe-{}", std::process::id());
    // 1. Pending discoverer FIRST — this also opens its own census interval at
    //    the floor, so the owner exists in the census with no launch slack.
    let mut adapter = remuda_driver::adapters::codex_adapter::CodexAdapter::new(AdapterHome {
        home: shadow_home.to_path_buf(),
        cwd: cwd.to_path_buf(),
        pid: None,
        launched_at: Some(floor),
        resume_session_id: None,
    })
    .with_instance_key(Some(owner.to_owned()));
    // 2. Record the owner's matching prompt after the lifetime is open.
    record_matching_prompt(cwd, owner);
    // 3. Write the unique, unclaimed rollout at/after the floor last.
    write_rollout_file(shadow_home, cwd, &session, floor);
    let _ = adapter.poll();
    adapter.binding().is_some()
}

/// Negative-control discoverer with its own fresh owner (no registered
/// lifetime): used while A is still live — the ONLY refusal is A's census
/// overlap.
fn prompt_only_binds(shadow_home: &Path, cwd: &Path) -> bool {
    let owner = format!("probe-{}", std::process::id());
    prompt_only_binds_with_owner(shadow_home, cwd, &owner)
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
            initiator: None,
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
            "re-exec child failed ({}): {}",
            output.status.code().unwrap_or(-1),
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
    write_rollout_into(
        &home_a,
        &cwd_a,
        id_a.as_id().as_str(),
        SESSION,
        time::OffsetDateTime::now_utc(),
    );
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
    write_rollout_into(
        &home_c,
        &cwd_c,
        id_c.as_id().as_str(),
        SESSION,
        time::OffsetDateTime::now_utc(),
    );
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

    // ---- In-process recompose Node B in A's SAME cwd (c-usagefu r21 item 2).
    //
    // B is composed with hooks DISABLED and a fixture that never sends
    // SessionStart, so the hook-confirmed tier cannot bind. B builds its OWN
    // real codex driver (which registers a real confirmed lifetime through
    // register_confirmed_launch independent of hooks). Attribution is
    // verified ONLY through the prompt tier: a fresh discoverer with B's
    // owner id.
    let root_b = tempfile::tempdir().unwrap();
    let node_b = compose_native_with(root_b.path(), workspace_a.clone(), false);
    let (created_b, _cwd_b, home_b) = launch_real_codex(&node_b, root_b.path(), &workspace_a).await;
    let id_b = created_b.instance.meta.id.clone();
    // Wait for B's driver (its register_confirmed_launch ran during spawn);
    // confirm B never hook-binds (no SessionStart is ever sent).
    let driver_b = live_driver(&node_b, &id_b).await;
    tokio::time::sleep(std::time::Duration::from_millis(1800)).await;
    assert!(
        !driver_b.test_codex_bound(),
        "B must not bind through hooks: the fixture never sends SessionStart"
    );
    // Fresh, unique rollout in B's shadow home with B's owner prompt
    // recorded; timestamp AFTER B's launch floor (B launched seconds ago) so
    // the prompt tier is satisfied once A's interval is retired.
    write_rollout_into(
        &home_b,
        &cwd_a,
        id_b.as_id().as_str(),
        SESSION,
        time::OffsetDateTime::now_utc(),
    );
    assert!(
        prompt_only_binds_with_owner(&home_b, &cwd_a, id_b.as_id().as_str()),
        "B's cwd attributes through the prompt-only tier after A retired (no SessionStart)"
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
