use super::*;
use crate::{DriverFuture, DriverRegistry, MemoryStore};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

struct FakePty {
    kind: DriverKind,
    ready: AtomicBool,
    race_unavailable: AtomicBool,
    fail_send: AtomicBool,
    fail_close: AtomicBool,
    hold_close: AtomicBool,
    close_started: AtomicBool,
    attempts: AtomicUsize,
    sent: Mutex<Vec<String>>,
    sent_origins: Mutex<Vec<remuda_protocol::InputOrigin>>,
}

impl Driver for FakePty {
    fn kind(&self) -> DriverKind {
        self.kind
    }

    fn wait_control(&self) -> DriverFuture<'_> {
        Box::pin(async {
            if self.ready.load(Ordering::SeqCst) {
                Ok(Vec::new())
            } else {
                Err(DriverError::ControlUnavailable)
            }
        })
    }

    fn execute(&self, request: DriverRequest) -> DriverFuture<'_> {
        Box::pin(async move {
            match request {
                DriverRequest::Send { prompt, origin, .. } => {
                    self.attempts.fetch_add(1, Ordering::SeqCst);
                    if self.race_unavailable.swap(false, Ordering::SeqCst) {
                        self.ready.store(false, Ordering::SeqCst);
                        return Err(DriverError::ControlUnavailable);
                    }
                    if self.fail_send.swap(false, Ordering::SeqCst) {
                        return Err(DriverError::Failed("lost acknowledgement".into()));
                    }
                    self.sent.lock().unwrap().push(prompt);
                    self.sent_origins.lock().unwrap().push(origin);
                    Ok(vec![DriverEmission::Message {
                        role: MessageRole::Assistant,
                        phase: MessagePhase::Final,
                        text: "PONG".into(),
                    }])
                }
                DriverRequest::SendKeys { .. } | DriverRequest::RespondInteraction { .. } => {
                    self.ready.store(true, Ordering::SeqCst);
                    Ok(Vec::new())
                }
                DriverRequest::Close => {
                    self.close_started.store(true, Ordering::SeqCst);
                    while self.hold_close.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    if self.fail_close.swap(false, Ordering::SeqCst) {
                        Err(DriverError::Failed("close temporarily unavailable".into()))
                    } else {
                        Ok(Vec::new())
                    }
                }
                _ => Ok(Vec::new()),
            }
        })
    }
}

fn node(kind: DriverKind, capacity: usize) -> (DevNode, Arc<FakePty>) {
    let mut config = crate::DevServerConfig::loopback(0);
    config.instance_queue_capacity = capacity;
    // Tests run with the crate directory as cwd; pin the allowed root there so
    // they do not implicitly depend on the checkout living under $HOME.
    config.workspace_roots = Some(vec![std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))]);
    let driver = Arc::new(FakePty {
        kind,
        ready: AtomicBool::new(false),
        race_unavailable: AtomicBool::new(false),
        fail_send: AtomicBool::new(false),
        fail_close: AtomicBool::new(false),
        hold_close: AtomicBool::new(false),
        close_started: AtomicBool::new(false),
        attempts: AtomicUsize::new(0),
        sent: Mutex::new(Vec::new()),
        sent_origins: Mutex::new(Vec::new()),
    });
    let registry = DriverRegistry::default();
    registry.register(driver.clone()).unwrap();
    (
        DevNode::with_parts(&config, Arc::new(MemoryStore::new(128)), registry).unwrap(),
        driver,
    )
}

async fn create(node: &DevNode, kind: DriverKind) -> CreateInstanceResponse {
    let agent = if kind == DriverKind::ShellPty {
        "terminal"
    } else {
        "claude"
    };
    let created = node
        .create_instance(
            serde_json::from_value(serde_json::json!({
            "kind": agent, "driver": kind, "prompt": "first"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    wait(|| node.get_command(&created.command.command_id).unwrap().state == CommandState::Settled)
        .await;
    created
}

async fn wait(predicate: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("PTY condition");
}

fn messages(
    node: &DevNode,
    created: &CreateInstanceResponse,
    status: ContentStatus,
) -> Vec<MessagePayload> {
    node.read_journal(&created.instance.journal_id, None, 128)
        .unwrap()
        .events
        .into_iter()
        .filter_map(|event| {
            let JournalEvent::Instance(observation) = event else {
                return None;
            };
            let ObservationPayload::Message(message) = observation.body else {
                return None;
            };
            (message.role == MessageRole::User && message.status == status).then_some(*message)
        })
        .collect()
}

async fn submit(
    node: &DevNode,
    created: &CreateInstanceResponse,
    json: serde_json::Value,
) -> Command {
    node.submit_command(
        &created.instance.meta.id,
        serde_json::from_value(json).unwrap(),
    )
    .await
    .unwrap()
    .command
}

fn assert_accepted(node: &DevNode, command: &Command) {
    let command = node.get_command(&command.command_id).unwrap();
    assert_eq!(command.state, CommandState::Settled);
    assert!(
        matches!(command.settlement, Knowledge::Known { value } if value.outcome == SettlementOutcome::Completed)
    );
}

#[tokio::test]
async fn pty_create_accepts_queued_input_then_delivers_fifo_after_control_keys() {
    for kind in [
        DriverKind::ClaudePty,
        DriverKind::GenericPty,
        DriverKind::ShellPty,
    ] {
        let (node, driver) = node(kind, 4);
        let created = create(&node, kind).await;
        assert_accepted(&node, &created.command);
        let instance = node.get_instance(&created.instance.meta.id).unwrap();
        assert_eq!(instance.lifecycle, InstanceLifecycle::Ready);
        assert_eq!(instance.connectivity, Connectivity::Connected);
        assert_eq!(
            instance.activity,
            Knowledge::Known {
                value: Activity::WaitingInteraction
            }
        );
        assert!(instance.last_error.is_none());
        assert_eq!(messages(&node, &created, ContentStatus::Queued).len(), 1);
        assert_eq!(driver.attempts.load(Ordering::SeqCst), 0);
        let second = submit(
            &node,
            &created,
            serde_json::json!({"operation": "send", "prompt": "second"}),
        )
        .await;
        wait(|| messages(&node, &created, ContentStatus::Queued).len() == 2).await;
        assert_eq!(
            node.get_command(&second.command_id).unwrap().state,
            CommandState::Accepted
        );
        submit(
            &node,
            &created,
            serde_json::json!({"operation": "tty.write", "keys": ["enter"]}),
        )
        .await;
        wait(|| messages(&node, &created, ContentStatus::Complete).len() == 2).await;
        assert_eq!(*driver.sent.lock().unwrap(), ["first", "second"]);
        assert_accepted(&node, &created.command);
        assert_accepted(&node, &second);
        for (queued, sent) in messages(&node, &created, ContentStatus::Queued)
            .iter()
            .zip(messages(&node, &created, ContentStatus::Complete))
        {
            assert_eq!(queued.message_id, sent.message_id);
            assert_eq!(sent.mutation.operation, MutationOperation::Replace);
            assert_eq!(sent.mutation.base_revision, Some(U64(1)));
            assert_eq!(sent.mutation.revision, U64(2));
        }
        node.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn control_unavailable_after_readiness_does_not_reject_or_duplicate_input() {
    let (node, driver) = node(DriverKind::ClaudePty, 4);
    driver.ready.store(true, Ordering::SeqCst);
    driver.race_unavailable.store(true, Ordering::SeqCst);
    let created = create(&node, DriverKind::ClaudePty).await;
    wait(|| driver.attempts.load(Ordering::SeqCst) == 1).await;
    assert_accepted(&node, &created.command);
    assert_eq!(messages(&node, &created, ContentStatus::Queued).len(), 1);
    assert!(messages(&node, &created, ContentStatus::Complete).is_empty());
    driver.ready.store(true, Ordering::SeqCst);
    wait(|| messages(&node, &created, ContentStatus::Complete).len() == 1).await;
    assert_eq!(*driver.sent.lock().unwrap(), ["first"]);
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn queued_prompt_keeps_native_interaction_pending_until_user_response() {
    let (node, driver) = node(DriverKind::ClaudePty, 4);
    let created = create(&node, DriverKind::ClaudePty).await;
    let mut interaction = crate::interactions::fake_can_use_tool(&created.instance).unwrap();
    interaction.carrier = remuda_protocol::InteractionCarrier::NativeTty;
    let interaction_id = interaction.meta.id.clone();
    let event = node
        .inner
        .store
        .append_observation(
            &created.instance.meta.id,
            None,
            Completeness::ScreenDerived,
            DriverEmission::InteractionRequested {
                interaction: Box::new(interaction),
            }
            .into_payload()
            .unwrap(),
        )
        .unwrap();
    node.inner.interactions.ingest(&event).await.unwrap();
    tokio::time::sleep(Duration::from_millis(450)).await;
    let pending = node
        .dispatch_interaction(
            "interaction.list",
            serde_json::json!({"instanceId": created.instance.meta.id}),
        )
        .await
        .unwrap();
    assert_eq!(pending["items"].as_array().unwrap().len(), 1);
    assert_eq!(driver.attempts.load(Ordering::SeqCst), 0);
    let response = submit(&node, &created, serde_json::json!({
        "operation": "respond_interaction", "interactionId": interaction_id,
        "answer": {"kind": "approval", "optionId": "allow", "inputDigest": format!("sha256:{:0>64}", "b")}
    })).await;
    wait(|| messages(&node, &created, ContentStatus::Complete).len() == 1).await;
    assert_accepted(&node, &response);
    assert_eq!(*driver.sent.lock().unwrap(), ["first"]);
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn pty_cancel_and_close_interrupt_queued_input_without_sending() {
    for operation in ["cancel", "close"] {
        let (node, driver) = node(DriverKind::ClaudePty, 4);
        let created = create(&node, DriverKind::ClaudePty).await;
        let second = submit(
            &node,
            &created,
            serde_json::json!({"operation": "send", "prompt": "second"}),
        )
        .await;
        wait(|| messages(&node, &created, ContentStatus::Queued).len() == 2).await;
        let control = submit(&node, &created, serde_json::json!({"operation": operation})).await;
        wait(|| node.get_command(&control.command_id).unwrap().state == CommandState::Settled)
            .await;
        assert_eq!(
            messages(&node, &created, ContentStatus::Interrupted).len(),
            2
        );
        assert_accepted(&node, &created.command);
        assert!(
            matches!(node.get_command(&second.command_id).unwrap().settlement, Knowledge::Known { value } if value.outcome == SettlementOutcome::Rejected)
        );
        assert!(driver.sent.lock().unwrap().is_empty());
        node.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn pty_uncertain_send_is_not_replayed_and_does_not_fail_created_runtime() {
    let (node, driver) = node(DriverKind::ClaudePty, 4);
    driver.ready.store(true, Ordering::SeqCst);
    driver.fail_send.store(true, Ordering::SeqCst);
    let created = create(&node, DriverKind::ClaudePty).await;
    wait(|| messages(&node, &created, ContentStatus::Interrupted).len() == 1).await;
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert_eq!(driver.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        node.get_instance(&created.instance.meta.id)
            .unwrap()
            .lifecycle,
        InstanceLifecycle::Ready
    );
    assert_accepted(&node, &created.command);
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn pty_pending_bound_preserves_control_access() {
    let (node, driver) = node(DriverKind::ClaudePty, 1);
    let created = create(&node, DriverKind::ClaudePty).await;
    let overflow = submit(
        &node,
        &created,
        serde_json::json!({"operation": "send", "prompt": "overflow"}),
    )
    .await;
    wait(|| node.get_command(&overflow.command_id).unwrap().state == CommandState::Settled).await;
    assert!(
        matches!(node.get_command(&overflow.command_id).unwrap().settlement, Knowledge::Known { value } if value.outcome == SettlementOutcome::Rejected)
    );
    let keys = submit(
        &node,
        &created,
        serde_json::json!({"operation": "tty.write", "keys": ["enter"]}),
    )
    .await;
    wait(|| messages(&node, &created, ContentStatus::Complete).len() == 1).await;
    assert_accepted(&node, &keys);
    assert_eq!(*driver.sent.lock().unwrap(), ["first"]);
    node.shutdown().await.unwrap();
}

/// DEFECT B: twelve idle `rmd-*` panes survived instances stopped through
/// `POST /v1/instances/{id}/stop`. A driver only reclaims the Herdr resources
/// it holds **in memory**, and the durable `pty_resources` row — the only
/// record that outlives a rebuilt or adopted driver — was consulted at startup
/// and shutdown but never on a stop. So the stop settled, the Instance went
/// `exited`, and the pane stayed in the operator's session until the Node was
/// restarted. A carrier whose driver has no in-memory record stands in here
/// for the rebuilt-driver case.
#[tokio::test]
async fn stopping_a_pty_instance_reclaims_its_durable_carrier_ownership() {
    use remuda_driver::PtyResource;
    use remuda_testing::{FakeHerdrOptions, FakeHerdrServer};

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("herdr.sock");
    let _fake = FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket)).unwrap();
    let client = remuda_herdr::Client::connect(&socket);
    let (node, driver) = node(DriverKind::ClaudePty, 4);
    let created = create(&node, DriverKind::ClaudePty).await;

    // A pane this Instance owns, recorded durably exactly as a real launch
    // does — and unknown to the driver's own in-memory resource list.
    let workspace = client
        .workspace_create(remuda_herdr::WorkspaceCreateParams {
            label: Some("remuda-stop-probe".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    node.inner
        .store
        .put_pty_resource(&PtyResource {
            instance_id: Some(created.instance.meta.id.clone()),
            socket_path: socket.clone(),
            session: "remuda-stop-test".into(),
            workspace_id: workspace.workspace.workspace_id.clone(),
            workspace_label: workspace.workspace.label.clone(),
            tab_id: workspace.tab.tab_id.clone(),
            pane_id: workspace.root_pane.pane_id.clone(),
        })
        .unwrap();
    assert_eq!(client.session_snapshot().await.unwrap().panes.len(), 1);

    let close = submit(&node, &created, serde_json::json!({"operation": "close"})).await;
    wait(|| node.get_command(&close.command_id).unwrap().state == CommandState::Settled).await;
    assert_accepted(&node, &close);
    assert_eq!(
        node.get_instance(&created.instance.meta.id)
            .unwrap()
            .lifecycle,
        InstanceLifecycle::Exited
    );

    // Reclamation follows the settlement rather than gating it: the stop is
    // already durable, and a carrier that is slow to die must not hold the
    // command open. It is prompt, not deferred to the next Node restart.
    wait(|| node.inner.store.pty_resources().unwrap().is_empty()).await;
    let snapshot = client.session_snapshot().await.unwrap();
    assert!(
        snapshot.panes.is_empty() && snapshot.workspaces.is_empty(),
        "a settled stop must leave no pane behind: {snapshot:?}"
    );
    let _ = driver;
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_pty_close_keeps_worker_available_for_send_and_close_retry() {
    let (node, driver) = node(DriverKind::ClaudePty, 4);
    let created = create(&node, DriverKind::ClaudePty).await;
    driver.fail_close.store(true, Ordering::SeqCst);
    let close = submit(&node, &created, serde_json::json!({"operation": "close"})).await;
    wait(|| node.get_command(&close.command_id).unwrap().state == CommandState::Settled).await;
    assert!(
        matches!(node.get_command(&close.command_id).unwrap().settlement, Knowledge::Known { value } if value.outcome == SettlementOutcome::Rejected)
    );
    assert_eq!(
        node.get_instance(&created.instance.meta.id)
            .unwrap()
            .lifecycle,
        InstanceLifecycle::Ready
    );
    driver.ready.store(true, Ordering::SeqCst);
    let sent = submit(
        &node,
        &created,
        serde_json::json!({"operation": "send", "prompt": "after close failure"}),
    )
    .await;
    wait(|| node.get_command(&sent.command_id).unwrap().state == CommandState::Settled).await;
    assert_accepted(&node, &sent);
    assert_eq!(*driver.sent.lock().unwrap(), ["after close failure"]);
    let retry = submit(&node, &created, serde_json::json!({"operation": "close"})).await;
    wait(|| node.get_command(&retry.command_id).unwrap().state == CommandState::Settled).await;
    assert_accepted(&node, &retry);
    assert_eq!(
        node.get_instance(&created.instance.meta.id)
            .unwrap()
            .lifecycle,
        InstanceLifecycle::Exited
    );
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn successful_pty_close_rejects_commands_accepted_while_close_was_in_flight() {
    let (node, driver) = node(DriverKind::ClaudePty, 4);
    let created = create(&node, DriverKind::ClaudePty).await;
    driver.hold_close.store(true, Ordering::SeqCst);
    let close = submit(&node, &created, serde_json::json!({"operation": "close"})).await;
    wait(|| driver.close_started.load(Ordering::SeqCst)).await;
    let queued = submit(
        &node,
        &created,
        serde_json::json!({"operation": "send", "prompt": "behind close"}),
    )
    .await;
    assert_eq!(queued.state, CommandState::Accepted);
    driver.hold_close.store(false, Ordering::SeqCst);
    wait(|| node.get_command(&queued.command_id).unwrap().state == CommandState::Settled).await;
    assert!(
        matches!(node.get_command(&queued.command_id).unwrap().settlement, Knowledge::Known { value } if value.outcome == SettlementOutcome::Rejected)
    );
    assert_accepted(&node, &close);
    assert!(driver.sent.lock().unwrap().is_empty());
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn queued_inputs_preserve_each_origin_independently_of_the_instance_creator() {
    use remuda_protocol::InputOrigin;

    for creator in [InputOrigin::Human, InputOrigin::Bot, InputOrigin::Agent] {
        let (node, driver) = node(DriverKind::ClaudePty, 4);
        let created = node
            .create_instance(
                serde_json::from_value(serde_json::json!({
                    "kind": "claude", "driver": "claude-pty", "origin": creator, "prompt": "initial"
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        wait(|| {
            node.get_command(&created.command.command_id).unwrap().state == CommandState::Settled
        })
        .await;
        for origin in [
            serde_json::json!("human"),
            serde_json::json!("bot"),
            serde_json::Value::Null,
        ] {
            let mut request = serde_json::json!({"operation": "send", "prompt": "follow-up"});
            if !origin.is_null() {
                request["origin"] = origin;
            }
            submit(&node, &created, request).await;
        }
        wait(|| messages(&node, &created, ContentStatus::Queued).len() == 4).await;
        driver.ready.store(true, Ordering::SeqCst);
        wait(|| driver.sent_origins.lock().unwrap().len() == 4).await;
        assert_eq!(
            *driver.sent_origins.lock().unwrap(),
            [
                creator,
                InputOrigin::Human,
                InputOrigin::Bot,
                InputOrigin::Agent
            ]
        );
        node.shutdown().await.unwrap();
    }
}
