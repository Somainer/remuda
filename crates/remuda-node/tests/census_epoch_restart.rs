//! c-usagefu r18 items 2/3 — census blocks are scoped to the owning Node epoch.
//!
//! Through the REAL `compose()` -> `DevNode::shutdown()` -> `compose()`
//! in-process restart:
//! 1. a driver carrying Node A's epoch ends a close with a forced
//!    stop-incomplete verdict (the real ladder still runs) -> its codex
//!    census interval stays OPEN and same-cwd prompt-only discovery refuses;
//! 2. Node A shutting down retires ONLY its own intervals — removed without
//!    a recorded close — so Node B (fresh epoch, same process, same cwd) can
//!    attribute a prompt-only discovery again;
//! 3. a second live Node C with a retained block in ANOTHER cwd is unaffected
//!    by A's shutdown, and C's own shutdown then retires its block.
#![cfg(unix)]

use remuda_driver::adapters::codex_adapter::CodexAdapter;
use remuda_driver::adapters::codex_discovery::{
    NodeEpoch, census_lifetime_is_live, forget_lifetime, record_input,
};
use remuda_driver::adapters::{AdapterHome, FileSignalAdapter};
use remuda_driver::shell_pty::StopRung;
use remuda_driver::shell_pty::{AgentLaunch, HookConfig, ShellPtyDriver, ShellPtyOptions};
use remuda_driver::{
    Delegation, Driver, LaunchOrigin, ProviderHealth, ProviderKind, ProviderProfile,
    TestStopOutcome,
};
use remuda_node::{DevNode, DevServerConfig, LocalDrivers, ServeConfig};
use remuda_protocol::{AgentKind, DriverKind, InstanceId, InstanceSpec};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const RUN_MARKER: &str = "r18-node-epoch-child";
const EMULATOR_ENV: &str = "REMUDA_PTY_EMULATOR";

/// Write the fake codex binary the driver launches: answers `--version` and
/// `version`, then idles (the real ladder kills its group; the test only
/// substitutes the ladder VERDICT afterwards).
fn install_fake_codex(root: &Path) -> PathBuf {
    let install = root.join("opt/bin");
    std::fs::create_dir_all(&install).unwrap();
    let dest = install.join("codex");
    std::fs::write(
        &dest,
        "#!/bin/sh
case \"$1\" in --version|version) echo 'codex 0.154.0'; exit 0;; esac
sleep 60
",
    )
    .unwrap();
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    dest
}

fn compose_node(data_dir: PathBuf) -> DevNode {
    let http =
        DevServerConfig::loopback(0).with_workspace_roots(remuda_testing::test_workspace_roots!());
    remuda_node::compose(&ServeConfig {
        http,
        data_dir,
        drivers: LocalDrivers::Fake,
    })
    .expect("compose")
}

fn forced_incomplete(pgid: i32) -> TestStopOutcome {
    TestStopOutcome {
        rung: StopRung::Kill,
        group_gone: false,
        pgid: Some(pgid),
        survivors: vec![pgid],
    }
}

/// Build a REAL codex shell-pty driver carrying `epoch`, spawn it, force the
/// next close to a stop-incomplete verdict, close it, and return the
/// `(cwd, owner)` whose census interval must now be retained open.
async fn retained_incomplete_block(
    root: &Path,
    epoch: NodeEpoch,
    instance_id: &InstanceId,
) -> (PathBuf, String) {
    let cwd = root.join("workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    let instance_dir = root.join(instance_id.as_id().as_str());
    let binary = install_fake_codex(root);
    let mut options = ShellPtyOptions::agent(
        cwd.clone(),
        AgentKind::Codex,
        AgentLaunch {
            profile: Box::new(ProviderProfile {
                id: "pvp_01990000-0000-7000-8000-000000000001".parse().unwrap(),
                kind: ProviderKind::OpenaiResponses,
                base_url: String::new(),
                delegation: Delegation::None,
                secret_ref: None,
                models: vec!["fake".to_owned()],
                health: ProviderHealth::Healthy,
            }),
            launch_dir: instance_dir.join("launch"),
            native_home: root.join("codex-native-home"),
            binary: Some(binary),
            origin: LaunchOrigin::Human,
            native_home_managed: true,
            settings_overlay: None,
        },
    );
    options.instance_id = Some(instance_id.clone());
    options.node_epoch = Some(epoch);
    options.hooks = Some(HookConfig {
        instance_dir,
        relay_binary: PathBuf::from("/bin/true"),
        tui: remuda_driver::TuiMode::Default,
    });
    options.extra_env.insert(RUN_MARKER.into(), "1".to_owned());
    options
        .extra_env
        .insert(EMULATOR_ENV.into(), "1".to_owned());

    let driver = ShellPtyDriver::new(options);
    let mut spec: InstanceSpec = serde_json::from_str(include_str!(
        "../../remuda-driver/tests/fixtures/instance-spec.json"
    ))
    .unwrap();
    spec.driver = DriverKind::ShellPty;
    spec.kind = AgentKind::Codex;
    spec.cwd = cwd.to_string_lossy().into_owned();
    spec.model_id = Some("fake".to_owned());
    let _run = driver
        .start(spec)
        .await
        .expect("the real codex fixture spawns");
    let pgid = driver.test_pgid().expect("the spawned child has a pgid");
    assert!(pgid > 0);
    driver.test_force_stop_outcome(forced_incomplete(pgid));
    Driver::close(&driver).await.expect("close settles");
    (
        std::fs::canonicalize(&cwd).unwrap(),
        instance_id.as_id().to_string(),
    )
}

/// A later same-cwd, prompt-only launch is attributable through the REAL
/// discovery path: one fresh rollout carrying a matching prompt binds.
fn prompt_only_discovery_binds(codex_home: &Path, cwd: &Path, owner: &str, session: &str) {
    let home = codex_home.to_path_buf();
    let floor = time::OffsetDateTime::now_utc() + time::Duration::milliseconds(500);
    let mut adapter = CodexAdapter::new(AdapterHome {
        home,
        cwd: cwd.to_path_buf(),
        pid: None,
        launched_at: Some(floor),
        resume_session_id: None,
    })
    .with_instance_key(Some(owner.to_owned()));
    let prompt = "r18 W0 epoch restart prompt";
    record_input(cwd, owner, prompt);
    let date_dir = format!(
        "{:04}/{:02}/{:02}",
        floor.year(),
        u8::from(floor.month()),
        floor.day()
    );
    let month_dir = codex_home.join("sessions").join(date_dir);
    std::fs::create_dir_all(&month_dir).unwrap();
    let rfc = floor
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    std::fs::write(
        month_dir.join("rollout-r18epoch.jsonl"),
        format!(
            "{{\"timestamp\":\"{rfc}\",\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session}\",\"session_id\":\"{session}\",\"cwd\":\"{}\",\"timestamp\":\"{rfc}\"}}}}\n\
             {{\"timestamp\":\"{rfc}\",\"ordinal\":1,\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"id\":\"m\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"{prompt}\"}}],\"internal_chat_message_metadata_passthrough\":{{\"content_item_kinds\":[\"user.text\"]}}}}}}\n",
            cwd.display()
        ),
    )
    .unwrap();
    let observed = adapter.poll().expect("real discover()");
    assert!(
        adapter.binding().is_some(),
        "prompt-only discovery binds after the dead Node's intervals were retired: {observed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_retires_one_nodes_blocks_and_recompose_discovers_again() {
    let root = tempfile::tempdir().unwrap();
    let codex_home = root.path().join("codex-home");

    // Node A: real incomplete block in cwd A.
    let node_a = compose_node(root.path().join("node-data-a"));
    let id_a = InstanceId::new();
    let (cwd_a, owner_a) =
        retained_incomplete_block(root.path(), node_a.census_epoch(), &id_a).await;
    assert!(
        census_lifetime_is_live(&cwd_a, &owner_a),
        "Node A's stop-incomplete interval stays live while A runs"
    );

    // Node C (also still live) holds its own block in a different cwd.
    let node_c = compose_node(root.path().join("node-data-c"));
    let root_c = root.path().join("node-c");
    let id_c = InstanceId::new();
    let (cwd_c, owner_c) = retained_incomplete_block(&root_c, node_c.census_epoch(), &id_c).await;
    assert!(census_lifetime_is_live(&cwd_c, &owner_c));

    // A prompt-only launch in A's cwd refuses while A still runs.
    let mut refusing = CodexAdapter::new(AdapterHome {
        home: codex_home.clone(),
        cwd: cwd_a.clone(),
        pid: None,
        launched_at: Some(time::OffsetDateTime::now_utc()),
        resume_session_id: None,
    })
    .with_instance_key(Some("r18-competitor-a".to_owned()));
    record_input(&cwd_a, "r18-competitor-a", "anything");
    assert!(
        refusing.poll().unwrap().is_empty() && refusing.binding().is_none(),
        "prompt-only discovery refuses under a live stop-incomplete block"
    );
    drop(refusing);

    // REAL Node A shutdown (the incomplete closes already happened; this is
    // the epoch retirement wiring in DevNode::shutdown).
    node_a.shutdown().await.expect("Node A shuts down");

    // A's intervals are retired (removed, not closed); C's block is untouched.
    assert!(
        !census_lifetime_is_live(&cwd_a, &owner_a),
        "A's block retires with A's shutdown"
    );
    assert!(
        census_lifetime_is_live(&cwd_c, &owner_c),
        "a second live Node's retained block is unaffected"
    );

    // In-process recompose: Node B, same process, fresh epoch. Its same-cwd
    // prompt-only discovery attributes again.
    let _node_b = compose_node(root.path().join("node-data-b"));
    prompt_only_discovery_binds(&codex_home, &cwd_a, "r18-competitor-b", "r18epochb");

    // C shutting down retires its own block too.
    node_c.shutdown().await.expect("Node C shuts down");
    assert!(
        !census_lifetime_is_live(&cwd_c, &owner_c),
        "C's block retires with C"
    );

    // Clean up detached test state.
    forget_lifetime(&cwd_c, &owner_c);
    forget_lifetime(&cwd_a, "r18-competitor-b");
}
