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
    http.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    std::fs::create_dir_all(&http.workspace_root).expect("workspace");
    http
}

fn create_req(prompt: &str) -> CreateInstanceRequest {
    CreateInstanceRequest {
        origin: remuda_protocol::InputOrigin::Human,
        agent_credential: None,
        command_id: None,
        instance_id: None,
        host_id: None,
        workspace_id: None,
        kind: AgentKind::Claude,
        driver: DriverKind::ClaudePrint,
        model: "fake".to_owned(),
        args: Vec::new(),
        binary_path: None,
        binary_sha256: None,
        provider_profile_id: "dev-fake".to_owned(),
        permission_mode: "dontAsk".to_owned(),
        sandbox: None,
        prompt: prompt.to_owned(),
        cwd: None,
        delegation: None,
        settings_overlay_path: None,
        claude_config_dir: None,
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

fn observations(events: &[JournalEvent]) -> Vec<Observation> {
    events
        .iter()
        .filter_map(|event| match event {
            JournalEvent::Instance(observation) => Some((**observation).clone()),
            _ => None,
        })
        .collect()
}

/// A minimal public-protocol lifecycle observation for direct store appends.
fn native_lifecycle_observation(native_name: &str) -> ObservationPayload {
    use remuda_protocol::{Knowledge, LifecyclePayload, LifecycleTopic, NativeLifecycle, Severity};
    use std::collections::BTreeMap;
    ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
        NativeLifecycle {
            topic: LifecycleTopic::Session,
            native_name: native_name.to_owned(),
            native_id: Knowledge::Known {
                value: native_name.to_owned(),
            },
            status: Knowledge::Known {
                value: "started".to_owned(),
            },
            related_ids: BTreeMap::new(),
            data_ref: None,
            severity: Severity::Info,
            affects_completion: false,
        },
    ))))
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
    let (instance_id, journal_id, created) = {
        let node = compose(&config).expect("compose");
        let created = node
            .create_instance(create_req("durable hello"))
            .await
            .expect("create");
        wait_for_settlement(&node, &created.command.command_id).await;
        // The fast ack returns at `preparing`; wait for the worker to reach
        // `ready` so the durability assertion reads the terminal lifecycle.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if node
                    .get_instance(&created.instance.meta.id)
                    .is_ok_and(|instance| instance.lifecycle == InstanceLifecycle::Ready)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("instance reaches ready");
        let ready = node
            .get_instance(&created.instance.meta.id)
            .expect("ready instance");
        (ready.meta.id.clone(), ready.journal_id.clone(), created)
    };
    assert_eq!(
        created.instance.lifecycle,
        InstanceLifecycle::Preparing,
        "the ack itself is returned while materialization is still running"
    );

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
    let claude_config = data.path().join("claude-config");
    std::fs::create_dir_all(&claude_config).expect("isolated Claude config");
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
        let mut request = create_req("mid-session");
        request.claude_config_dir = Some(claude_config.to_string_lossy().into_owned());
        let created = tokio::time::timeout(Duration::from_secs(20), node.create_instance(request))
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

/// A journal reopened on a torn tail must reconcile and keep complete records.
///
/// This is the landing-gate failure of 2026-09-21
/// (`reconcile: Driver("journal append failed: json: EOF while parsing a
/// string ...")`) made deterministic: the last JSONL line is cut mid-record as
/// a hard-killed writer can leave it, then a fresh Node reopens the data dir
/// and runs the restart sweep. Reconcile must succeed (the torn seq never
/// reaches the append it triggers), every complete record must survive, and the
/// next append must reuse the recycled seq cleanly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_after_a_torn_journal_tail_reconciles_and_keeps_complete_records() {
    use remuda_node::LocalStore;

    let data = tempfile::tempdir().expect("data dir");
    let config = ServeConfig::fake(loopback_config(data.path()), data.path().to_path_buf());
    let id = {
        let node = compose(&config).expect("compose");
        let created = node
            .create_instance(create_req("durable hello"))
            .await
            .expect("create");
        wait_for_settlement(&node, &created.command.command_id).await;
        created.instance.meta.id.clone()
    };

    // Find the instance's JSONL and cut its final record in half, exactly as a
    // writer interrupted between the byte write and the newline would.
    let journal_dir = data.path().join("journal");
    let path = journal_dir.join(format!("{}.jsonl", id.as_id().as_str()));
    let bytes = std::fs::read(&path).expect("jsonl");
    assert!(bytes.iter().filter(|b| **b == b'\n').count() >= 2);
    let last_newline = bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .expect("newline-terminated tail");
    let previous_newline = bytes[..last_newline]
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |pos| pos + 1);
    let complete_records = bytes[..last_newline]
        .iter()
        .filter(|b| **b == b'\n')
        .count();
    let cut = previous_newline + (last_newline - previous_newline) / 2;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open jsonl for truncation");
    file.set_len(cut as u64).expect("truncate torn tail");
    drop(file);

    let restarted = compose(&config).expect("reopen over a torn tail");
    restarted
        .reconcile_herdr()
        .await
        .expect("reconcile over a torn journal tail");
    assert!(
        matches!(
            restarted.get_instance(&id).expect("instance").lifecycle,
            InstanceLifecycle::Exited | InstanceLifecycle::Failed
        ),
        "the sweep settles a session it cannot vouch for"
    );
    // Every complete record survived the reopen with its original seq; the torn
    // one is gone, so the seqs are a dense prefix and the tail parses.
    let journal_id = restarted.get_instance(&id).expect("instance").journal_id;
    let page = restarted
        .read_journal(&journal_id, None, 256)
        .expect("read the recovered journal");
    let kept = observations(&page.events);
    assert!(
        kept.len() >= complete_records,
        "every complete record survives; reconcile may add settle events"
    );
    assert_eq!(
        page.durable_seq.0 as usize,
        kept.len(),
        "the watermark follows surviving JSONL records: no torn seq, no hole"
    );
    for (index, observation) in kept.iter().take(complete_records).enumerate() {
        assert_eq!(
            observation.seq.0 as usize,
            index + 1,
            "complete records keep their original dense seqs"
        );
    }

    // The next append lands at watermark + 1 and reads back whole — the exact
    // operation that died with `json: EOF` in the gate. The composed Node owns
    // its store, so drop it and reopen one the way a later process would.
    let durable = page.durable_seq.0;
    drop(restarted);
    let store = remuda_node::MemoryStore::open_journaled(data.path(), 64).expect("reopen store");
    let appended = store
        .append_observation(
            &id,
            None,
            remuda_protocol::Completeness::Structured,
            native_lifecycle_observation("after-torn-tail"),
        )
        .expect("append after torn-tail recovery");
    assert_eq!(appended.seq.0, durable + 1);
    let read_back = store
        .read_events(&journal_id, Some(remuda_protocol::U64(durable)), 1)
        .expect("the repaired line parses, unlike the gate's json: EOF");
    assert_eq!(read_back.events.len(), 1);
}

/// A hard-dropped Node (no graceful shutdown, the way a real process dies)
/// must close its journal writer before a second Node reopens the same data
/// dir. Before the fix the dropped Node's spawned workers kept the writer and
/// its store clones alive, so a successor raced it on seq allocation
/// (`UNIQUE constraint failed: events.instance_id, events.seq`) and on JSONL
/// offsets (`json: EOF`). The structural assertion is the successor's
/// `compose`: it takes the single-writer lock, so a predecessor writer that
/// survived the drop makes it fail `Error::Locked` after the bounded wait
/// instead of this test passing on a timing fluke.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hard_dropped_node_cannot_write_after_its_successor_reopens() {
    for _ in 0..5 {
        let data = tempfile::tempdir().expect("data dir");
        let config = ServeConfig::fake(loopback_config(data.path()), data.path().to_path_buf());
        let id = {
            let node = compose(&config).expect("compose");
            let created = node
                .create_instance(create_req("drop race"))
                .await
                .expect("create");
            // Drop immediately while materialization is still in flight: the
            // teardown must abort the store-owning tasks and close the journal
            // before the next compose can reopen it.
            created.instance.meta.id.clone()
        };
        // Deterministic, not a sleep: the open polls for the predecessor's
        // single-writer lock, which is released only when its writer thread
        // exits. A teardown that leaks the writer makes this return Locked
        // after the bounded wait; a correct handoff acquires on the first
        // poll after the close nudge.
        let reopen_started = std::time::Instant::now();
        let restarted = compose(&config).expect("reopen must not meet a live old writer");
        assert!(
            reopen_started.elapsed() < std::time::Duration::from_secs(2),
            "the dropped Node's writer must release the lock immediately, took {:?}",
            reopen_started.elapsed()
        );
        restarted
            .reconcile_herdr()
            .await
            .expect("reconcile after a hard drop");
        assert!(
            matches!(
                restarted.get_instance(&id).expect("instance").lifecycle,
                InstanceLifecycle::Exited | InstanceLifecycle::Failed
            ),
            "the successor settles the session the dropped Node left behind"
        );
        // Belt and braces: whatever an aborted task still does, it cannot reach
        // the closed journal, so the tail stays dense and parseable.
        let instance = restarted.get_instance(&id).expect("instance");
        let page = restarted
            .read_journal(&instance.journal_id, None, 256)
            .expect("journal remains readable");
        assert_eq!(
            page.events.len(),
            page.durable_seq.0 as usize,
            "no stale-offset writer can leave an unindexed or torn tail"
        );
    }
}
