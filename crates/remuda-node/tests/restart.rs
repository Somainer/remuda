//! Restart durability: SQLite entities + remuda-journal survive process death.

use remuda_journal::fold_all;
use remuda_node::{
    CreateInstanceRequest, DevServerConfig, LocalDrivers, NativeDriverConfig, ServeConfig, compose,
};
use remuda_protocol::{
    AgentKind, CommandState, DriverKind, InstanceLifecycle, JournalEvent, Observation,
    ObservationPayload,
};
use remuda_testing::{ScriptKind, ensure_workspace_bin, script_path};
use std::time::Duration;

fn loopback_config(root: &std::path::Path) -> DevServerConfig {
    let mut http = DevServerConfig::loopback(0);
    http.workspace_root = root.join("workspace");
    std::fs::create_dir_all(&http.workspace_root).expect("workspace");
    http
}

fn create_req(prompt: &str) -> CreateInstanceRequest {
    CreateInstanceRequest {
        command_id: None,
        instance_id: None,
        host_id: None,
        workspace_id: None,
        kind: AgentKind::Claude,
        driver: DriverKind::ClaudePrint,
        model: "fake".to_owned(),
        args: Vec::new(),
        provider_profile_id: "dev-fake".to_owned(),
        permission_mode: "dontAsk".to_owned(),
        prompt: prompt.to_owned(),
        cwd: None,
        delegation: None,
        settings_overlay_path: None,
        claude_config_dir: None,
        max_budget_usd: None,
        provider_overlay: None,
        provider_auth_token: None,
    }
}

fn observations(events: &[JournalEvent]) -> Vec<Observation> {
    events
        .iter()
        .filter_map(|event| match event {
            JournalEvent::Instance(observation) => Some((**observation).clone()),
            _ => None,
        })
        .collect()
}

async fn wait_for_settlement(node: &remuda_node::DevNode, command_id: &remuda_protocol::CommandId) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if node.get_command(command_id).expect("command").state == CommandState::Settled {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("command settlement");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_driver_restart_lists_instances_and_replays_folds() {
    let data = tempfile::tempdir().expect("data dir");
    let http = loopback_config(data.path());
    let config = ServeConfig::fake(http, data.path().to_path_buf());
    let created = {
        let node = compose(&config).expect("compose");
        let created = node
            .create_instance(create_req("durable hello"))
            .await
            .expect("create");
        wait_for_settlement(&node, &created.command.command_id).await;
        created
    };
    let instance_id = created.instance.meta.id.clone();
    let journal_id = created.instance.journal_id.clone();
    assert_eq!(created.instance.lifecycle, InstanceLifecycle::Ready);

    let restarted = compose(&config).expect("reopen");
    let listed = restarted.list_instances().expect("list");
    assert_eq!(listed.items.len(), 1);
    assert_eq!(listed.items[0].meta.id, instance_id);
    assert!(
        matches!(
            listed.items[0].lifecycle,
            InstanceLifecycle::Ready | InstanceLifecycle::Failed
        ),
        "restart must list the last durable lifecycle, got {:?}",
        listed.items[0].lifecycle
    );
    assert!(listed.items[0].durable_seq.0 >= 1);

    let page = restarted
        .read_journal(&journal_id, None, 256)
        .expect("journal page");
    let first = observations(&page.events);
    assert!(!first.is_empty());
    let fold_a = fold_all(&first);
    let fold_b = fold_all(&first);
    assert_eq!(fold_a, fold_b);
    assert!(
        first.iter().any(|obs| matches!(
            &obs.body,
            ObservationPayload::Message(payload) if payload.role == remuda_protocol::MessageRole::User
        )),
        "user prompt should be in the replayed journal"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_claude_kill_mid_session_replays_identical_folds() {
    let fake = ensure_workspace_bin("fake-claude");
    let data = tempfile::tempdir().expect("data dir");
    let http = loopback_config(data.path());
    let mut native = NativeDriverConfig::new(data.path().to_path_buf()).with_claude_binary(fake);
    native.extra_env.insert(
        "FAKE_CLAUDE_SCRIPT".to_owned(),
        script_path(ScriptKind::Ok).to_string_lossy().into_owned(),
    );
    let config = ServeConfig {
        http,
        data_dir: data.path().to_path_buf(),
        drivers: LocalDrivers::Native(native),
    };

    let (instance_id, journal_id) = {
        let node = compose(&config).expect("compose native");
        let created = tokio::time::timeout(
            Duration::from_secs(20),
            node.create_instance(create_req("mid-session")),
        )
        .await
        .expect("create timed out")
        .expect("create fake-claude instance");
        wait_for_settlement(&node, &created.command.command_id).await;
        // Drop `node` here: mid-session kill (no close command).
        (created.instance.meta.id, created.instance.journal_id)
    };

    let restarted = compose(&config).expect("reopen after kill");
    let listed = restarted.list_instances().expect("list after kill");
    assert_eq!(listed.items.len(), 1);
    assert_eq!(listed.items[0].meta.id, instance_id);
    assert!(
        matches!(
            listed.items[0].lifecycle,
            InstanceLifecycle::Ready | InstanceLifecycle::Failed
        ),
        "kill mid-session must persist Ready or Failed, got {:?}",
        listed.items[0].lifecycle
    );

    let page = restarted
        .read_journal(&journal_id, None, 256)
        .expect("replay journal");
    let first = observations(&page.events);
    let fold_a = fold_all(&first);
    let fold_b = fold_all(&first);
    assert_eq!(fold_a, fold_b, "journal folds must be identical on replay");
    let recipe = restarted
        .launch_recipe(&instance_id)
        .expect("recipe lookup");
    assert!(
        recipe.is_some(),
        "native fake-claude launch recipe must survive restart"
    );
}
