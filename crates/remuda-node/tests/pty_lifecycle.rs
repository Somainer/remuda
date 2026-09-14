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
            if instance.lifecycle != InstanceLifecycle::Ready {
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
            if instance.lifecycle != InstanceLifecycle::Ready {
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
        assert_eq!(created.instance.lifecycle, InstanceLifecycle::Ready);
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
