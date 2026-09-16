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
    sent_modes: Mutex<Vec<remuda_protocol::PromptMode>>,
    /// Relative order of delivery events ("cancel"/"send:<text>").
    order: Mutex<Vec<String>>,
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
                DriverRequest::Send {
                    prompt,
                    origin,
                    mode,
                    ..
                } => {
                    self.attempts.fetch_add(1, Ordering::SeqCst);
                    if self.race_unavailable.swap(false, Ordering::SeqCst) {
                        self.ready.store(false, Ordering::SeqCst);
                        return Err(DriverError::ControlUnavailable);
                    }
                    if self.fail_send.swap(false, Ordering::SeqCst) {
                        return Err(DriverError::Failed("lost acknowledgement".into()));
                    }
                    self.order.lock().unwrap().push(format!("send:{prompt}"));
                    self.sent.lock().unwrap().push(prompt);
                    self.sent_origins.lock().unwrap().push(origin);
                    self.sent_modes.lock().unwrap().push(mode);
                    Ok(vec![DriverEmission::Message {
                        role: MessageRole::Assistant,
                        phase: MessagePhase::Final,
                        text: "PONG".into(),
                    }])
                }
                DriverRequest::Cancel => {
                    // A real harness Esc ends the turn and leaves the composer
                    // ready for the steer that follows; model both facts.
                    self.order.lock().unwrap().push("cancel".into());
                    self.ready.store(true, Ordering::SeqCst);
                    Ok(Vec::new())
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
    // Tests run with the crate directory as cwd; the shared helper allows that
    // plus the temp dir, so they do not implicitly depend on the checkout
    // living under $HOME.
    config.workspace_roots = Some(remuda_testing::test_workspace_roots!());
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
        sent_modes: Mutex::new(Vec::new()),
        order: Mutex::new(Vec::new()),
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

/// The CURRENT revision of each user message: the journal keeps every Queued →
/// Complete/Interrupted Replace as its own event, so [`messages`] counts
/// stale revisions. Fold by message id, keeping the highest revision.
fn folded_messages(
    node: &DevNode,
    created: &CreateInstanceResponse,
) -> Vec<MessagePayload> {
    let mut by_id = std::collections::BTreeMap::<_, MessagePayload>::new();
    for event in node.read_journal(&created.instance.journal_id, None, 256).unwrap().events {
        let JournalEvent::Instance(observation) = event else { continue };
        let ObservationPayload::Message(message) = observation.body else { continue };
        if message.role != MessageRole::User {
            continue;
        }
        by_id
            .entry(message.message_id.clone())
            .and_modify(|existing| {
                if message.mutation.revision >= existing.mutation.revision {
                    *existing = (*message).clone();
                }
            })
            .or_insert_with(|| *message);
    }
    by_id.into_values().collect()
}

fn folded_with_status(
    node: &DevNode,
    created: &CreateInstanceResponse,
    status: ContentStatus,
) -> Vec<MessagePayload> {
    folded_messages(node, created)
        .into_iter()
        .filter(|message| message.status == status)
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

/// Fold a native `agent_status` lifecycle through the interaction broker the
/// way the observation pump would after a real hook/screen signal: this is
/// the turn-ended/idle evidence the steer gate waits for.
async fn fold_status(node: &DevNode, created: &CreateInstanceResponse, status: &str) {
    let payload = DriverEmission::NativeLifecycle {
        name: "agent_status".into(),
        status: status.into(),
        severity: remuda_protocol::Severity::Info,
    }
    .into_payload()
    .unwrap();
    let observation = node
        .inner
        .store
        .append_observation(
            &created.instance.meta.id,
            None,
            Completeness::Structured,
            payload,
        )
        .unwrap();
    node.inner.interactions.ingest(&observation).await.unwrap();
}

/// Native lifecycle observations emitted for this instance with a given name.
fn native_lifecycles(
    node: &DevNode,
    created: &CreateInstanceResponse,
    name: &str,
) -> Vec<(String, std::collections::BTreeMap<String, String>)> {
    node.read_journal(&created.instance.journal_id, None, 256)
        .unwrap()
        .events
        .into_iter()
        .filter_map(|event| {
            let JournalEvent::Instance(observation) = event else {
                return None;
            };
            let ObservationPayload::Lifecycle(lifecycle) = observation.body else {
                return None;
            };
            let remuda_protocol::LifecyclePayload::Native(native) = lifecycle.as_ref() else {
                return None;
            };
            (native.native_name == name).then(|| {
                (
                    match &native.status {
                        remuda_protocol::Knowledge::Known { value } => value.clone(),
                        _ => String::new(),
                    },
                    native.related_ids.clone(),
                )
            })
        })
        .collect()
}

fn completed_user_message(
    node: &DevNode,
    created: &CreateInstanceResponse,
    text: &str,
) -> Option<MessagePayload> {
    messages(node, created, ContentStatus::Complete)
        .into_iter()
        .find(|message| {
            message
                .blocks
                .iter()
                .any(|block| format!("{block:?}").contains(text))
        })
}

/// c-steer: a 插队 during a long tool sends Esc, waits for the turn-ended
/// evidence, then writes ahead of the FIFO prompts; the two ordinary queued
/// prompts keep their order behind it, and the ledger records origin+reason.
#[tokio::test]
async fn steer_interrupts_the_running_turn_then_jumps_the_queue() {
    let (node, driver) = node(DriverKind::ClaudePty, 8);
    let created = create(&node, DriverKind::ClaudePty).await;
    driver.ready.store(true, Ordering::SeqCst);
    wait(|| driver.sent.lock().unwrap().len() == 1).await;

    // The "long tool": the composer control is not ready while it runs.
    driver.ready.store(false, Ordering::SeqCst);
    let second = submit(
        &node,
        &created,
        serde_json::json!({"operation": "send", "prompt": "second"}),
    )
    .await;
    let third = submit(
        &node,
        &created,
        serde_json::json!({"operation": "send", "prompt": "third"}),
    )
    .await;
    wait(|| folded_with_status(&node, &created, ContentStatus::Queued).len() == 2).await;
    let jump = submit(
        &node,
        &created,
        serde_json::json!({"operation": "send", "prompt": "jump", "mode": "steer"}),
    )
    .await;
    // c-steer evidence: measure interrupt→delivery latency. The Esc lands
    // immediately; the harness reports turn-ended `idle_delay` later.
    let started = std::time::Instant::now();

    // Esc is dispatched immediately; nothing is written while the turn runs.
    wait(|| driver.order.lock().unwrap().contains(&"cancel".to_string())).await;
    let esc_at = started.elapsed();
    // Give the 200 ms delivery tick one chance to prove it will NOT write
    // while the turn is still Working.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(*driver.sent.lock().unwrap(), ["first"]);
    assert_eq!(folded_with_status(&node, &created, ContentStatus::Queued).len(), 3);
    assert_eq!(
        node.get_instance(&created.instance.meta.id).unwrap().activity,
        Knowledge::Known {
            value: Activity::Working
        }
    );

    // The harness reports the interrupted turn ended.
    let idle_at = started.elapsed();
    fold_status(&node, &created, "idle").await;
    // The STEER itself lands on the first delivery tick after turn-ended;
    // measure that, not the FIFO prompts draining on later ticks.
    wait(|| driver.sent.lock().unwrap().iter().any(|text| text == "jump")).await;
    let steer_delivered = started.elapsed();
    wait(|| driver.sent.lock().unwrap().len() == 4).await;
    let delivered_at = started.elapsed();
    // Printed for docs/design/evidence/steer-1.md (--nocapture); asserted on
    // only loosely so CI jitter never flakes the ordering test.
    println!(
        "steer-latency esc={esc_at:?} turn-ended={idle_at:?} steer-sent={steer_delivered:?} \
         steer-after-turn-ended={:?} queue-drained={delivered_at:?}",
        steer_delivered.saturating_sub(idle_at)
    );
    assert!(delivered_at < Duration::from_secs(3));
    assert_eq!(*driver.sent.lock().unwrap(), ["first", "jump", "second", "third"]);
    assert_eq!(
        *driver.sent_modes.lock().unwrap(),
        [
            remuda_protocol::PromptMode::NewTurn,
            remuda_protocol::PromptMode::Steer,
            remuda_protocol::PromptMode::NewTurn,
            remuda_protocol::PromptMode::NewTurn,
        ]
    );
    assert_eq!(
        *driver.order.lock().unwrap(),
        ["send:first", "cancel", "send:jump", "send:second", "send:third"]
    );

    // Ledger: the interrupted turn, then the steer delivery, both attributed.
    let interrupted = native_lifecycles(&node, &created, "turn-interrupted");
    assert_eq!(interrupted.len(), 1);
    assert_eq!(interrupted[0].0, "esc-dispatched");
    assert_eq!(interrupted[0].1.get("reason").map(String::as_str), Some("user-steer"));
    assert_eq!(
        interrupted[0].1.get("commandId").map(String::as_str),
        Some(jump.command_id.as_id().as_str())
    );
    let delivered = native_lifecycles(&node, &created, "prompt-steer");
    assert_eq!(delivered.len(), 1);
    assert_eq!(
        delivered[0].1.get("reason").map(String::as_str),
        Some("interrupted-current-turn")
    );
    let jump_message = completed_user_message(&node, &created, "jump")
        .expect("steer delivered as a complete user message");
    assert_eq!(
        jump_message.prompt_mode,
        Some(remuda_protocol::PromptMode::Steer)
    );

    assert!(folded_with_status(&node, &created, ContentStatus::Queued).is_empty());
    for command in [second, third, jump] {
        assert_accepted(&node, &command);
    }
    node.shutdown().await.unwrap();
}

/// c-steer: while a question/approval is pending the instance is "blocked on a
/// human", never Working. A steer must not fire Esc into the dialog; it holds
/// at the head until the interaction resolves, then jumps the remaining queue.
#[tokio::test]
async fn steer_waits_behind_a_pending_interaction_without_firing_esc() {
    let (node, driver) = node(DriverKind::ClaudePty, 8);
    let created = create(&node, DriverKind::ClaudePty).await;
    driver.ready.store(true, Ordering::SeqCst);
    wait(|| driver.sent.lock().unwrap().len() == 1).await;

    // Agent parks on a question: a REAL pending interaction entity plus the
    // blocked native status (the gate keys off the broker table, not the
    // activity alone — a queue-induced "blocked" must not behave like a dialog).
    driver.ready.store(false, Ordering::SeqCst);
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
    fold_status(&node, &created, "blocked").await;
    assert_eq!(
        node.get_instance(&created.instance.meta.id).unwrap().activity,
        Knowledge::Known {
            value: Activity::WaitingInteraction
        }
    );

    let second = submit(
        &node,
        &created,
        serde_json::json!({"operation": "send", "prompt": "second"}),
    )
    .await;
    wait(|| folded_with_status(&node, &created, ContentStatus::Queued).len() == 1).await;
    // The queued follow-up must not blur the pending question into "working".
    assert_eq!(
        node.get_instance(&created.instance.meta.id).unwrap().activity,
        Knowledge::Known {
            value: Activity::WaitingInteraction
        }
    );

    let jump = submit(
        &node,
        &created,
        serde_json::json!({"operation": "send", "prompt": "jump", "mode": "steer"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    // No Esc at a dialog; nothing written; both prompts stay queued.
    assert!(!driver.order.lock().unwrap().contains(&"cancel".to_string()));
    assert_eq!(*driver.sent.lock().unwrap(), ["first"]);
    assert_eq!(folded_with_status(&node, &created, ContentStatus::Queued).len(), 2);
    assert!(native_lifecycles(&node, &created, "turn-interrupted").is_empty());

    // The question is answered through the interaction broker: the dialog
    // closes, the turn resumes and ends idle, then the steer (not Esc) goes
    // first and the queued follow-up after it.
    let answer = submit(&node, &created, serde_json::json!({
        "operation": "respond_interaction", "interactionId": interaction_id,
        "answer": {"kind": "approval", "optionId": "allow", "inputDigest": format!("sha256:{:0>64}", "b")}
    })).await;
    driver.ready.store(true, Ordering::SeqCst);
    fold_status(&node, &created, "idle").await;
    wait(|| driver.sent.lock().unwrap().len() == 3).await;
    assert_eq!(*driver.sent.lock().unwrap(), ["first", "jump", "second"]);
    assert!(!driver.order.lock().unwrap().contains(&"cancel".to_string()));
    assert_accepted(&node, &second);
    assert_accepted(&node, &jump);
    assert_accepted(&node, &answer);
    node.shutdown().await.unwrap();
}

/// c-steer: if the turn-ended evidence never arrives, the 2 s budget still
/// delivers the steer once control is ready — it never sits 排队中 forever.
#[tokio::test]
async fn steer_delivers_after_the_interrupt_budget_without_idle_evidence() {
    let (node, driver) = node(DriverKind::ClaudePty, 8);
    let created = create(&node, DriverKind::ClaudePty).await;
    driver.ready.store(true, Ordering::SeqCst);
    wait(|| driver.sent.lock().unwrap().len() == 1).await;
    // Long tool; no idle evidence will ever arrive in this test.
    driver.ready.store(false, Ordering::SeqCst);
    let jump = submit(
        &node,
        &created,
        serde_json::json!({"operation": "send", "prompt": "jump", "mode": "steer"}),
    )
    .await;
    wait(|| driver.order.lock().unwrap().contains(&"cancel".to_string())).await;
    // Tool boundary frees the control shortly after the Esc.
    driver.ready.store(true, Ordering::SeqCst);
    wait(|| driver.sent.lock().unwrap().len() == 2).await;
    assert_eq!(*driver.sent.lock().unwrap(), ["first", "jump"]);
    assert_accepted(&node, &jump);
    assert!(folded_with_status(&node, &created, ContentStatus::Queued).is_empty());
    node.shutdown().await.unwrap();
}

/// c-steer: two 插队 in quick succession each get their own Esc; the second is
/// not left gated forever after the first starts a new turn, and delivery
/// order is jump1, jump2 (each ahead of the ordinary queue).
#[tokio::test]
async fn two_consecutive_steers_each_interrupt_and_keep_order() {
    let (node, driver) = node(DriverKind::ClaudePty, 8);
    let created = create(&node, DriverKind::ClaudePty).await;
    driver.ready.store(true, Ordering::SeqCst);
    wait(|| driver.sent.lock().unwrap().len() == 1).await;
    driver.ready.store(false, Ordering::SeqCst);

    let _queued = submit(
        &node,
        &created,
        serde_json::json!({"operation": "send", "prompt": "after"}),
    )
    .await;
    let j1 = submit(
        &node,
        &created,
        serde_json::json!({"operation": "send", "prompt": "jump1", "mode": "steer"}),
    )
    .await;
    wait(|| driver.order.lock().unwrap().iter().filter(|o| *o == "cancel").count() == 1).await;
    let j2 = submit(
        &node,
        &created,
        serde_json::json!({"operation": "send", "prompt": "jump2", "mode": "steer"}),
    )
    .await;

    // First interrupt lands, first turn ends; jump1 goes out and starts a new
    // turn — the worker must fire the second Esc for jump2 itself.
    fold_status(&node, &created, "idle").await;
    wait(|| driver.sent.lock().unwrap().iter().any(|t| t == "jump1")).await;
    // jump1's send marks Working again; the second Esc follows on its own.
    wait(|| driver.order.lock().unwrap().iter().filter(|o| *o == "cancel").count() == 2).await;
    fold_status(&node, &created, "idle").await;
    wait(|| driver.sent.lock().unwrap().len() == 4).await;
    assert_eq!(*driver.sent.lock().unwrap(), ["first", "jump1", "jump2", "after"]);
    assert_accepted(&node, &j1);
    assert_accepted(&node, &j2);
    assert!(folded_with_status(&node, &created, ContentStatus::Queued).is_empty());
    node.shutdown().await.unwrap();
}
