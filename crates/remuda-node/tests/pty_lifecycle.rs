//! The lifecycle five, on the native PTY carrier (D-028 §5, §8).
//!
//! These exercise the Node-side halves of P2: which (kind, driver) pairs are
//! allowed to start at all, what happens to the instance when the PTY's process
//! ends, and what a Node restart does to sessions it can no longer hold.
//!
//! The driver-side halves — the signal ladder, the key table, the paste gating
//! — are unit tested next to the code that implements them. What is testable
//! only from here is the *fold*: whether the Node believes the driver.

use remuda_node::{
    CreateInstanceRequest, DevServerConfig, LocalDrivers, NativeDriverConfig, ServeConfig, compose,
};
use remuda_protocol::{AgentKind, DriverKind, InstanceLifecycle, ObservationPayload};
use std::time::Duration;

fn loopback_config(root: &std::path::Path) -> DevServerConfig {
    let mut http = DevServerConfig::loopback(0);
    http.workspace_root = root.join("workspace");
    // Never a hard-coded root: these must pass from a /tmp checkout and from
    // one under $HOME, and the helper is what keeps the two honest.
    http.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    std::fs::create_dir_all(&http.workspace_root).expect("workspace");
    http
}

fn create_req(kind: AgentKind, driver: DriverKind) -> CreateInstanceRequest {
    CreateInstanceRequest {
        origin: remuda_protocol::InputOrigin::Human,
        agent_credential: None,
        command_id: None,
        instance_id: None,
        host_id: None,
        workspace_id: None,
        kind,
        driver,
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
        binary_path: None,
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
    }
}

/// A Node whose shell-pty instances run a real `portable-pty`.
fn native_config(data: &std::path::Path) -> ServeConfig {
    ServeConfig {
        http: loopback_config(data),
        data_dir: data.to_path_buf(),
        drivers: LocalDrivers::Native(NativeDriverConfig::new(data.to_path_buf())),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_agent_kind_may_run_in_a_shell_pty_and_mismatched_products_may_not() {
    // §5.1's kind/driver matrix. The four agent kinds on `shell-pty` are what
    // D-028 opens up; the negative case is the invariant that keeps a kind from
    // being paired with a driver that speaks a different native product.
    let data = tempfile::tempdir().expect("data dir");
    let node = compose(&native_config(data.path())).expect("compose");

    for kind in [
        AgentKind::Claude,
        AgentKind::Codex,
        AgentKind::Grok,
        AgentKind::Agy,
        AgentKind::Terminal,
        AgentKind::Generic,
    ] {
        let created = node
            .create_instance(create_req(kind, DriverKind::ShellPty))
            .await;
        let created = match created {
            Ok(created) => created,
            Err(error) => panic!("(kind {kind:?}, shell-pty) must be creatable: {error:?}"),
        };
        // Each of these is a real login shell in a real PTY. Closing them is
        // not tidiness — an un-awaited close leaves six shells running for the
        // life of the test binary.
        node.submit_command(
            &created.instance.meta.id,
            serde_json::from_value(serde_json::json!({"operation": "close"})).expect("close"),
        )
        .await
        .expect("close");
    }

    let mismatched = node
        .create_instance(create_req(AgentKind::Codex, DriverKind::ClaudePrint))
        .await;
    assert!(
        mismatched.is_err(),
        "codex on a Claude driver is not one product and must stay rejected"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pty_whose_process_exits_cleanly_stops_being_ready() {
    // §5.5's defect, from the Node's side: EOF used to only break the read
    // loop, so an agent that had exited — crashed or not — was still `ready` in
    // the UI forever. The instance must settle on its own, with nobody having
    // sent a close.
    let data = tempfile::tempdir().expect("data dir");
    let node = compose(&native_config(data.path())).expect("compose");
    let mut request = create_req(AgentKind::Terminal, DriverKind::ShellPty);
    request.args = vec!["/bin/sh".into(), "-c".into(), "exit 0".into()];
    let created = node.create_instance(request).await.expect("create");
    let id = created.instance.meta.id.clone();

    let settled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let instance = node.get_instance(&id).expect("instance");
            // A fast-accepted create starts at `preparing`; wait for a real
            // terminal state rather than racing the preparing → ready ramp.
            if matches!(
                instance.lifecycle,
                InstanceLifecycle::Exited | InstanceLifecycle::Failed
            ) {
                return instance;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("§5.5 budgets 2s from process death to lifecycle event");

    assert_eq!(settled.lifecycle, InstanceLifecycle::Exited);
    let page = node
        .read_journal(&created.instance.journal_id, None, 256)
        .expect("journal");
    assert!(
        page.events.iter().any(|event| {
            let remuda_protocol::JournalEvent::Instance(observation) = event else {
                return false;
            };
            let ObservationPayload::Lifecycle(payload) = &observation.body else {
                return false;
            };
            match payload.as_ref() {
                remuda_protocol::LifecyclePayload::Entity(entity) => entity.state == "exited",
                remuda_protocol::LifecyclePayload::Native(native) => {
                    native.native_name == remuda_driver::shell_pty::NATIVE_EXIT
                }
            }
        }),
        "the exit has to reach the journal, not just the entity: \
         the UI reads the journal to explain why a session ended"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pty_that_dies_badly_is_failed_rather_than_exited() {
    // §5.5: only exit code 0 is `exited`. Folding a crash into the same state
    // as a clean close is how a broken agent looks finished.
    let data = tempfile::tempdir().expect("data dir");
    let node = compose(&native_config(data.path())).expect("compose");
    let mut request = create_req(AgentKind::Terminal, DriverKind::ShellPty);
    request.args = vec!["/bin/sh".into(), "-c".into(), "exit 3".into()];
    let created = node.create_instance(request).await.expect("create");
    let id = created.instance.meta.id.clone();

    let settled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let instance = node.get_instance(&id).expect("instance");
            if matches!(
                instance.lifecycle,
                InstanceLifecycle::Exited | InstanceLifecycle::Failed
            ) {
                return instance;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("exit detection");

    assert_ne!(
        settled.lifecycle,
        InstanceLifecycle::Ready,
        "a non-zero exit must settle the instance"
    );
    let reason = settled.last_error.unwrap_or_default();
    assert!(
        reason.contains("native-exit-code-3"),
        "the reason must name the evidence, got {reason:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_a_pty_leaves_no_process_behind() {
    // §5.3's acceptance: after `instance.close` the foreground pgid is gone and
    // nothing it started survives. The shell here spawns a grandchild, which is
    // the shape the old `child.kill()` orphaned — it SIGKILLed the login shell
    // and left the `claude` inside it running.
    let data = tempfile::tempdir().expect("data dir");
    let node = compose(&native_config(data.path())).expect("compose");
    let marker = data.path().join("grandchild.pid");
    let mut request = create_req(AgentKind::Terminal, DriverKind::ShellPty);
    request.args = vec![
        "/bin/sh".into(),
        "-c".into(),
        format!(
            "sh -c 'echo $$ > {}; while :; do sleep 0.1; done' & while :; do sleep 0.1; done",
            marker.display()
        ),
    ];
    let created = node.create_instance(request).await.expect("create");
    let id = created.instance.meta.id.clone();

    let grandchild = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(&marker)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("grandchild starts");

    node.submit_command(
        &id,
        serde_json::from_value(serde_json::json!({"operation": "close"})).expect("close"),
    )
    .await
    .expect("close");

    let gone = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if !remuda_driver::shell_pty::lifecycle::group_alive(grandchild) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        gone.is_ok(),
        "the grandchild outlived the close; that is the orphan §5.3 names"
    );
}

/// native-carrier-4: a purge that arrives behind a forced delete must stop the
/// instance, not refuse it.
///
/// The Hub only purges behind a delete it has already decided to perform, and
/// it drops its own row regardless of the answer. So refusing here never kept
/// a session alive — it orphaned the process: the Hub logged "node rejected
/// instance.purge" and the Node was left holding a running agent nobody could
/// reach. On macOS that happened on *every* forced delete, because the
/// shell-pty stop ladder outlasts the purge's own grace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn purging_a_live_pty_stops_it_instead_of_orphaning_it() {
    let data = tempfile::tempdir().expect("data dir");
    let node = compose(&native_config(data.path())).expect("compose");
    let marker = data.path().join("child.pid");
    let mut request = create_req(AgentKind::Terminal, DriverKind::ShellPty);
    request.args = vec![
        "/bin/sh".into(),
        "-c".into(),
        format!(
            "echo $$ > {}; while :; do sleep 0.1; done",
            marker.display()
        ),
    ];
    let created = node.create_instance(request).await.expect("create");
    let id = created.instance.meta.id.clone();

    let pid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(&marker)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the shell starts");

    // No `instance.close` first: this is the forced-delete path, where the
    // purge is what has to cope with a still-live instance.
    let purged = node.purge_instance(&id).await.expect("purge must succeed");
    assert_eq!(purged["purged"], serde_json::json!(true));

    let gone = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if !remuda_driver::shell_pty::lifecycle::group_alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        gone.is_ok(),
        "the purge returned while its process was still running"
    );
    // The Node-owned directory goes with it; leaving it behind is the leak the
    // old rejection path produced on every forced delete.
    assert!(
        !data
            .path()
            .join("instances")
            .join(id.as_id().as_str())
            .exists(),
        "the instance directory outlived the purge"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_restart_ends_native_pty_sessions_and_says_why() {
    // §8 plan A. An in-process PTY dies with the Node, and the requirement is
    // not that it survive — it is that the loss be legible. A row still
    // claiming `ready` after the restart is the failure mode: the user waits
    // for an agent that is not there.
    let data = tempfile::tempdir().expect("data dir");
    let config = native_config(data.path());
    let (id, journal_id) = {
        let node = compose(&config).expect("compose");
        let mut request = create_req(AgentKind::Terminal, DriverKind::ShellPty);
        request.args = vec![
            "/bin/sh".into(),
            "-c".into(),
            "while :; do sleep 0.1; done".into(),
        ];
        let created = node.create_instance(request).await.expect("create");
        // The create is durably accepted at `preparing`; wait for the worker to
        // materialize the PTY and reach `ready` before killing the Node.
        let ready = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if node
                    .get_instance(&created.instance.meta.id)
                    .is_ok_and(|instance| instance.lifecycle == InstanceLifecycle::Ready)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await;
        ready.expect("the instance reaches ready after materialization");
        let ids = (
            created.instance.meta.id.clone(),
            created.instance.journal_id.clone(),
        );
        // Kill the PTY without telling the store, which is what a Node dying
        // actually looks like: the process goes, the `ready` row stays. Going
        // through `instance.close` would settle the row and there would be
        // nothing left for reconciliation to find.
        node.shutdown_processes_only().await;
        ids
    };

    let restarted = compose(&config).expect("reopen");
    restarted.reconcile_native_pty().await.expect("reconcile");

    let instance = restarted.get_instance(&id).expect("instance");
    assert_eq!(
        instance.lifecycle,
        InstanceLifecycle::Exited,
        "a PTY the Node can no longer hold must not still read as ready"
    );
    assert_eq!(
        instance.last_error.as_deref(),
        Some(remuda_node::reclaim::NODE_EPOCH_CHANGED),
    );

    let page = restarted
        .read_journal(&journal_id, None, 256)
        .expect("journal");
    let diagnostic = page.events.iter().any(|event| {
        let remuda_protocol::JournalEvent::Instance(observation) = event else {
            return false;
        };
        let ObservationPayload::Lifecycle(payload) = &observation.body else {
            return false;
        };
        let remuda_protocol::LifecyclePayload::Native(native) = payload.as_ref() else {
            return false;
        };
        native.native_name == "node_epoch_changed"
            && native.related_ids.get("resumable").map(String::as_str) == Some("true")
    });
    assert!(
        diagnostic,
        "the web renders 「Node 重启，会话已结束」 plus a Resume affordance from this \
         diagnostic; without it the restart is silent"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconciling_a_restart_twice_does_not_journal_the_loss_twice() {
    // Idempotence matters because reconciliation runs on every startup, and a
    // Node that restarts repeatedly would otherwise accumulate one "your
    // session ended" notice per restart for the same long-dead session.
    let data = tempfile::tempdir().expect("data dir");
    let config = native_config(data.path());
    let journal_id = {
        let node = compose(&config).expect("compose");
        let mut request = create_req(AgentKind::Terminal, DriverKind::ShellPty);
        request.args = vec![
            "/bin/sh".into(),
            "-c".into(),
            "while :; do sleep 0.1; done".into(),
        ];
        let created = node.create_instance(request).await.expect("create");
        let journal_id = created.instance.journal_id.clone();
        node.shutdown_processes_only().await;
        journal_id
    };

    let restarted = compose(&config).expect("reopen");
    restarted.reconcile_native_pty().await.expect("first");
    restarted.reconcile_native_pty().await.expect("second");

    let page = restarted
        .read_journal(&journal_id, None, 256)
        .expect("journal");
    let notices = page
        .events
        .iter()
        .filter(|event| {
            let remuda_protocol::JournalEvent::Instance(observation) = event else {
                return false;
            };
            let ObservationPayload::Lifecycle(payload) = &observation.body else {
                return false;
            };
            matches!(
                payload.as_ref(),
                remuda_protocol::LifecyclePayload::Native(native)
                    if native.native_name == "node_epoch_changed"
            )
        })
        .count();
    assert_eq!(notices, 1, "one restart, one notice");
}

/// Minimal Instance row for a store-seeded restart case; mirrors the Node's
/// own test fixture (`runtime::fixture_instance`) without needing it public.
#[allow(clippy::too_many_lines)]
fn seeded_instance(
    instance_id: remuda_protocol::InstanceId,
    host_id: remuda_protocol::HostId,
    workspace_id: remuda_protocol::WorkspaceId,
    driver: DriverKind,
) -> remuda_protocol::Instance {
    use remuda_protocol::{
        Activity, AgentKind, ClaudeRef, Connectivity, EntityMeta, InstanceMode, Knowledge,
        LaunchedBy, NativeRef, Ownership, ProcessRef, U64,
    };
    let now = remuda_protocol::Timestamp::try_from("2026-09-15T00:00:00.000Z".to_string())
        .expect("timestamp");
    let session_id = "0199a1f0-0000-7000-8000-aaaaaaaaaaaa".to_owned();
    remuda_protocol::Instance {
        meta: EntityMeta {
            id: instance_id,
            revision: U64(1),
            created_at: now.clone(),
            updated_at: now,
        },
        host_id: host_id.clone(),
        workspace_id,
        kind: AgentKind::Claude,
        driver,
        lifecycle: InstanceLifecycle::Ready,
        activity: Knowledge::Known {
            value: Activity::Idle,
        },
        activity_evidence_event_ids: Vec::new(),
        connectivity: Connectivity::Connected,
        ownership: Ownership::Managed,
        native_ref: NativeRef {
            host_id,
            native_store_id: remuda_protocol::Id::new("obj").expect("id"),
            kind: AgentKind::Claude,
            session_id: Knowledge::Known {
                value: session_id.clone(),
            },
            transcript: Knowledge::NotApplicable,
            signal_tier: None,
            capabilities: Vec::new(),
            codex: None,
            acp: None,
            claude: Some(ClaudeRef {
                session_id: session_id.clone(),
            }),
            claude_bg: None,
            agy: None,
            herdr: None,
        },
        process_ref: ProcessRef {
            process_generation: U64(1),
            process_identity: Knowledge::NotApplicable,
            connection_epoch: remuda_protocol::Id::new("epoch").expect("id"),
        },
        spec_revision: U64(1),
        launch_id: Knowledge::Known {
            value: remuda_protocol::Id::new("launch").expect("id"),
        },
        capabilities: remuda_node::driver_capability_snapshot(driver),
        owner_fence: U64(1),
        active_run_ids: Vec::new(),
        parent: None,
        journal_id: remuda_protocol::Id::new("obj").expect("id"),
        durable_seq: U64(0),
        exit: Knowledge::NotApplicable,
        last_error: None,
        mode: Some(InstanceMode::Native),
        promoted_at: None,
        launched_by: Some(LaunchedBy::Remuda),
    }
}

/// The restart sweep must cover every in-process driver, not `shell-pty` alone.
///
/// A `claude-pty` row survived its Node restart reading `running` in
/// node.sqlite, which is how the 2026-09-18 demo ended up with four instances
/// whose processes were dead still counting against `maxInstances` — and with
/// the hello inventory repeating that lie to the Hub, which then had nothing to
/// reconcile against.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_settles_a_claude_pty_row_and_the_inventory_agrees() {
    use remuda_node::{LocalStore, MemoryStore};

    let data = tempfile::tempdir().expect("data dir");
    let config = native_config(data.path());
    let id = remuda_protocol::InstanceId::new();

    // The previous Node's durable row, as it left it: ready, and with no
    // process behind it any more.
    {
        let store = MemoryStore::open_journaled(data.path(), 64).expect("store");
        store
            .insert_instance(seeded_instance(
                id.clone(),
                remuda_protocol::HostId::new(),
                remuda_protocol::WorkspaceId::new(),
                DriverKind::ClaudePty,
            ))
            .expect("seed a ready claude-pty row");
        assert_eq!(
            store.get_instance(&id).expect("read back").lifecycle,
            InstanceLifecycle::Ready,
            "the seed must start life as a row that claims to be running"
        );
    }

    // A fresh process, exactly as a real restart composes one.
    let restarted = compose(&config).expect("reopen");
    restarted.reconcile_herdr().await.expect("reconcile");

    let instance = restarted.get_instance(&id).expect("instance");
    assert_eq!(
        instance.lifecycle,
        InstanceLifecycle::Exited,
        "a carrier the Node can no longer hold must not still read as ready"
    );
    assert_eq!(
        instance.last_error.as_deref(),
        Some(remuda_node::reclaim::NODE_EPOCH_CHANGED),
    );

    // The inventory the hello announces must agree with the row, or the Hub
    // reconciles a Node that is still claiming the session it just lost.
    let inventory = restarted.list_instances().expect("list").items;
    let reported = inventory
        .iter()
        .find(|row| row.meta.id == id)
        .expect("the settled row is still listed");
    assert_eq!(
        reported.lifecycle,
        InstanceLifecycle::Exited,
        "the hello inventory must not re-report a session the sweep just settled"
    );
}

/// A Node that cannot vouch for its instance store must not announce an empty
/// inventory.
///
/// The Hub settles every live row a hello omits, so `[]` is a destructive
/// claim: it says "I hold nothing" and the Hub exits every session on the
/// host. A `--data-dir` pointed at the wrong path enumerates zero rows without
/// that meaning anything, so such a Node reports `None` — no `instances` key
/// at all — and leaves the Hub's rows for a Node that can vouch for itself.
///
/// "Vouch" is specifically *the store file was already there*. A brand-new
/// data dir is indistinguishable from a wrong one: both enumerate zero rows
/// and neither can show that emptiness is real, so both stay silent. That
/// costs nothing, because a host whose store never existed has no rows on the
/// Hub for the silence to leave behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_that_was_never_found_announces_no_inventory() {
    let data = tempfile::tempdir().expect("data dir");
    let config = native_config(data.path());

    // Nothing has created `node.sqlite` here yet.
    let fresh = compose(&config).expect("compose");
    assert!(
        !fresh.found_instance_store(),
        "a data dir this process had to create is not one it can vouch for"
    );
    assert!(
        fresh.list_instances().expect("list").items.is_empty(),
        "and it holds nothing, which is exactly why the claim is unsafe"
    );
    assert_eq!(
        fresh.announceable_inventory().expect("inventory"),
        None,
        "an unwarranted empty inventory must not be announced: the Hub would \
         settle every live row on this host"
    );
}

/// The other half, and the one the demo turned on: a Node that reopens the
/// data dir it was already using *can* vouch, so its inventory rides the hello
/// even when the sweep has just emptied it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reopened_store_vouches_for_an_empty_inventory() {
    use serde_json::json;

    let data = tempfile::tempdir().expect("data dir");
    let config = native_config(data.path());

    // First run creates the store; the second finds it.
    let first = compose(&config).expect("compose");
    let id = {
        let mut request = create_req(AgentKind::Terminal, DriverKind::ShellPty);
        request.args = vec![
            "/bin/sh".into(),
            "-c".into(),
            "while :; do sleep 0.1; done".into(),
        ];
        let created = first.create_instance(request).await.expect("create");
        created.instance.meta.id.clone()
    };
    drop(first);

    let restarted = compose(&config).expect("reopen");
    assert!(
        restarted.found_instance_store(),
        "a data dir that already held instance rows is one this process vouches for"
    );
    // The restart sweep settles the session, and the inventory the hello
    // announces must say so rather than re-reporting it as held.
    restarted.reconcile_herdr().await.expect("reconcile");
    assert_eq!(
        restarted.get_instance(&id).expect("instance").lifecycle,
        InstanceLifecycle::Exited
    );
    let announced = restarted
        .announceable_inventory()
        .expect("inventory")
        .expect("a vouched store announces its rows");
    let rows = announced.as_array().expect("array");
    let reported = rows
        .iter()
        .find(|row| row["id"] == json!(id.as_id().as_str()))
        .expect("the settled row is still listed");
    assert_eq!(
        reported["lifecycle"],
        json!("exited"),
        "the hello must report the swept row as exited, not as a session it still holds"
    );
}

/// An in-memory store holds nothing and can attest to nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_in_memory_store_never_vouches() {
    use remuda_node::{DevNode, DriverRegistry, LocalStore, MemoryStore};
    use std::sync::Arc;

    let store = Arc::new(MemoryStore::new(64));
    assert!(
        !store.instance_store_is_durable(),
        "an in-memory store must not claim durable instance rows"
    );
    let data = tempfile::tempdir().expect("data dir");
    let config = native_config(data.path());
    let node = DevNode::with_parts(
        &config.http,
        store,
        DriverRegistry::with_fake().expect("drivers"),
    )
    .expect("compose node");
    assert_eq!(
        node.announceable_inventory().expect("inventory"),
        None,
        "with nothing to vouch for, the hello carries no inventory key"
    );
}
