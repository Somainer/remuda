//! D-057 §7.10 (ma-fence), the Node half: a real `DevNode` driven by the
//! Hub's own frames (`dispatch_method`), with a fake PTY driver whose busy
//! turn is a flag — no sleeps decide an outcome; waits only poll for a state
//! the code under test must reach.

use super::*;
use crate::{
    DevNode, Driver, DriverEmission, DriverError, DriverFuture, DriverRegistry, MemoryStore,
};
use remuda_protocol::{
    ContentStatus, DriverKind, InstanceId, MessagePhase, MessageRole, ObservationPayload,
};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const LINEAGE: &str = "ins_0190d8b2-6a1e-7c3b-9f21-4c5d6e7f8a01";
const SEAT: &str = "ins_0190d8b2-6a1e-7c3b-9f21-4c5d6e7f8a02";

fn seat(generation: i64) -> Initiator {
    Initiator {
        instance_id: SEAT.into(),
        lineage_id: LINEAGE.into(),
        generation,
    }
}

/// A PTY-like driver: `ready=false` is a busy turn (control unavailable),
/// `hold_send` parks a send after the write began (a crash mid-write).
struct FakePty {
    ready: AtomicBool,
    hold_send: AtomicBool,
    send_started: AtomicBool,
    sent: std::sync::Mutex<Vec<String>>,
}

impl FakePty {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            ready: AtomicBool::new(false),
            hold_send: AtomicBool::new(false),
            send_started: AtomicBool::new(false),
            sent: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn sent(&self) -> Vec<String> {
        self.sent.lock().unwrap().clone()
    }
}

impl Driver for FakePty {
    fn kind(&self) -> DriverKind {
        DriverKind::ClaudePty
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

    fn execute(&self, request: crate::DriverRequest) -> DriverFuture<'_> {
        Box::pin(async move {
            match request {
                crate::DriverRequest::Send { prompt, .. } => {
                    self.send_started.store(true, Ordering::SeqCst);
                    while self.hold_send.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    self.sent.lock().unwrap().push(prompt);
                    Ok(vec![DriverEmission::Message {
                        role: MessageRole::Assistant,
                        phase: MessagePhase::Final,
                        text: "PONG".into(),
                    }])
                }
                _ => Ok(Vec::new()),
            }
        })
    }
}

fn node_with(
    store: Arc<dyn crate::LocalStore>,
    registry: Option<&Path>,
) -> (DevNode, Arc<FakePty>) {
    let mut config = crate::DevServerConfig::loopback(0)
        .with_workspace_roots(remuda_testing::test_workspace_roots!());
    if let Some(dir) = registry {
        config = config.with_workspace_registry(dir.to_path_buf());
    }
    let driver = FakePty::new();
    let registry = DriverRegistry::default();
    registry.register(driver.clone()).unwrap();
    (
        DevNode::with_parts(&config, store, registry).unwrap(),
        driver,
    )
}

fn node() -> (DevNode, Arc<FakePty>) {
    node_with(Arc::new(MemoryStore::new(128)), None)
}

async fn wait(what: &str, predicate: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never reached: {what}"));
}

async fn dispatch(node: &DevNode, method: &str, params: Value) -> Result<Value, NodeError> {
    crate::transport::hubnode::dispatch_method(node, method, params).await
}

/// A worker instance (not of the fenced lineage) whose create prompt has
/// settled; the driver is then made busy.
async fn worker(node: &DevNode, driver: &FakePty) -> InstanceId {
    driver.ready.store(true, Ordering::SeqCst);
    let created = node
        .create_instance(
            serde_json::from_value(json!({
                "kind": "claude", "driver": "claude-pty", "prompt": "first"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let command_id = created.command.command_id.clone();
    wait("worker create settled", || {
        node.get_command(&command_id).unwrap().state == remuda_protocol::CommandState::Settled
    })
    .await;
    // A PTY create settles at enqueue; wait for the first prompt's actual
    // write before the turn becomes busy.
    wait("the create prompt was written", || {
        driver.sent().iter().any(|text| text == "first")
    })
    .await;
    driver.ready.store(false, Ordering::SeqCst);
    created.instance.meta.id
}

/// The seat's `instance.send` to the worker, as the Hub forwards it.
async fn seat_send(
    node: &DevNode,
    worker: &InstanceId,
    text: &str,
    generation: i64,
) -> Result<Value, NodeError> {
    let command_id = remuda_protocol::CommandId::new();
    dispatch(
        node,
        "instance.send",
        json!({
            "instanceId": worker.as_id().as_str(),
            "commandId": command_id.as_id().as_str(),
            "origin": "agent",
            "initiator": seat(generation),
            "input": {"type": "prompt", "text": text},
        }),
    )
    .await
}

fn fence_params(fence: &str, live: u64) -> Value {
    json!({"fenceId": fence, "lineageId": LINEAGE, "liveGeneration": live.to_string()})
}

fn outcome_of(ack: &Value, op_id: &str) -> Option<String> {
    ack["outcomes"]
        .as_array()?
        .iter()
        .find(|entry| entry["opId"] == op_id)
        .map(|entry| entry["outcome"].as_str().unwrap_or_default().to_owned())
}

fn command_id_of(reply: &Value) -> String {
    reply
        .pointer("/command/commandId")
        .and_then(Value::as_str)
        .expect("command reply")
        .to_owned()
}

fn user_messages(node: &DevNode, worker: &InstanceId, status: ContentStatus) -> Vec<String> {
    let instance = node.get_instance(worker).unwrap();
    node.read_journal(&instance.journal_id, None, 256)
        .unwrap()
        .events
        .into_iter()
        .filter_map(|event| {
            let remuda_protocol::JournalEvent::Instance(observation) = event else {
                return None;
            };
            let ObservationPayload::Message(message) = observation.body else {
                return None;
            };
            (message.role == MessageRole::User && message.status == status)
                .then(|| format!("{:?}", message.blocks))
        })
        .collect()
}

#[test]
fn persisted_fences_reload_and_a_logged_entry_without_outcome_stays_unknown() {
    let dir = tempfile::tempdir().unwrap();
    {
        let fences = NodeFences::open(dir.path()).unwrap();
        let params = json!({"initiator": seat(1), "opId": "op-1"});
        assert_eq!(
            fences.direct_entry("worker.provision", &params).unwrap(),
            Some("op-1".into())
        );
        let params2 = json!({"initiator": seat(1), "opId": "op-2"});
        fences.direct_entry("worker.remove", &params2).unwrap();
        fences.direct_outcome("op-2").unwrap();
        let mut table = fences.lock().unwrap();
        fences.persist(&mut table, "fen_1", LINEAGE, 2).unwrap();
    }
    // A new process over the same dir: the fence applies before anything is
    // served, and the entry logged without an outcome reads unknown.
    let fences = NodeFences::open(dir.path()).unwrap();
    assert!(fences.is_fenced(Some(&seat(1))).unwrap());
    assert!(!fences.is_fenced(Some(&seat(2))).unwrap());
    let table = fences.lock().unwrap();
    let mut outcomes = NodeFences::direct_outcomes(&table, LINEAGE, 2);
    outcomes.sort_by(|a, b| a.op_id.cmp(&b.op_id));
    assert_eq!(outcomes[0].outcome, FenceOutcome::Unknown);
    assert_eq!(outcomes[1].outcome, FenceOutcome::Ran);
}

/// §7.10 "Prompt still queued when the Node reconnects": the prompt waits
/// behind a busy turn; the fence (the first frame after reconnect) cancels
/// it; when the turn ends the worker never receives the text.
#[tokio::test]
async fn a_prompt_queued_behind_a_busy_turn_is_cancelled_by_the_fence_and_never_delivered() {
    let (node, driver) = node();
    let worker = worker(&node, &driver).await;
    let reply = seat_send(&node, &worker, "SEAT-PROMPT", 1).await.unwrap();
    let command = command_id_of(&reply);

    let ack = dispatch(&node, METHOD_FENCE, fence_params("fen_queued", 2))
        .await
        .unwrap();
    assert_eq!(
        outcome_of(&ack, &command).as_deref(),
        Some("cancelled"),
        "{ack}"
    );

    // The busy turn ends: the queue moves on, and the cancelled prompt is
    // consumed without a write.
    driver.ready.store(true, Ordering::SeqCst);
    wait("the cancelled prompt left the queue", || {
        user_messages(&node, &worker, ContentStatus::Interrupted)
            .iter()
            .any(|blocks| blocks.contains("SEAT-PROMPT"))
    })
    .await;
    assert!(
        !driver.sent().iter().any(|text| text == "SEAT-PROMPT"),
        "the worker must never receive a cancelled prompt: {:?}",
        driver.sent()
    );
    let ledger = node.get_command(&command.parse().unwrap()).unwrap();
    assert_eq!(ledger.state, remuda_protocol::CommandState::Settled);
    assert!(settled_fenced(&ledger), "settled rejected(fenced)");
}

/// §7.10 "Turn ends during the partition": the busy turn ends before the
/// Node learns of the fence, so the prompt is delivered; the fence applied
/// at reconnect reports it `ran`, never `cancelled`.
#[tokio::test]
async fn a_prompt_delivered_before_the_fence_applies_is_reported_ran() {
    let (node, driver) = node();
    let worker = worker(&node, &driver).await;
    let reply = seat_send(&node, &worker, "DELIVERED-IN-PARTITION", 1)
        .await
        .unwrap();
    let command: remuda_protocol::CommandId = command_id_of(&reply).parse().unwrap();
    driver.ready.store(true, Ordering::SeqCst);
    wait("the prompt was delivered", || {
        node.get_command(&command).unwrap().state == remuda_protocol::CommandState::Settled
    })
    .await;
    assert!(
        driver
            .sent()
            .iter()
            .any(|text| text == "DELIVERED-IN-PARTITION")
    );

    let ack = dispatch(&node, METHOD_FENCE, fence_params("fen_partition", 2))
        .await
        .unwrap();
    assert_eq!(
        outcome_of(&ack, command.as_id().as_str()).as_deref(),
        Some("ran"),
        "{ack}"
    );
}

/// §7.10 "Link order", Node half: a command whose frame arrives after the
/// fence frame is refused at `insert_command` with `OWNER_FENCED`; nothing
/// is recorded and the worker never receives it.
#[tokio::test]
async fn a_command_arriving_after_the_fence_is_refused_and_never_reaches_the_worker() {
    let (node, driver) = node();
    let worker = worker(&node, &driver).await;
    driver.ready.store(true, Ordering::SeqCst);
    dispatch(&node, METHOD_FENCE, fence_params("fen_link", 2))
        .await
        .unwrap();
    let refused = seat_send(&node, &worker, "AFTER-THE-FENCE", 1).await;
    assert!(matches!(refused, Err(NodeError::Fenced)), "{refused:?}");
    assert_eq!(
        crate::stdio::rpc_code(&NodeError::Fenced),
        i64::from(remuda_protocol::ErrorCode::OwnerFenced.rpc_code())
    );
    // The live generation of the same lineage is not fenced.
    seat_send(&node, &worker, "LIVE-GENERATION", 2)
        .await
        .unwrap();
    wait("the live generation's prompt was delivered", || {
        driver.sent().iter().any(|text| text == "LIVE-GENERATION")
    })
    .await;
    assert!(!driver.sent().iter().any(|text| text == "AFTER-THE-FENCE"));
}

/// §7.10 "Node dispatcher": every §7.5 method with a fenced initiator is
/// refused at entry, before any side effect, and recorded as refused.
#[tokio::test]
async fn every_direct_method_with_a_fenced_initiator_is_refused_at_entry() {
    let (node, _driver) = node();
    dispatch(&node, METHOD_FENCE, fence_params("fen_direct", 2))
        .await
        .unwrap();
    let methods = [
        (
            "interaction.answer",
            json!({"interactionId": "int_x", "commandId": "cmd_x", "answer": {}}),
        ),
        (
            "worker.provision",
            json!({"name": "w-fenced", "branch": "wt/w-fenced", "workspaceId": "wsp_x"}),
        ),
        ("worker.remove", json!({"name": "w-fenced"})),
        (
            "worktree.lease",
            json!({"workspaceId": "wsp_x", "branch": "b"}),
        ),
        (
            "worktree.return",
            json!({"workspaceId": "wsp_x", "leaseId": "wtl_x"}),
        ),
        ("gate.run", json!({"jobId": "gjb_x", "laneId": "lane"})),
    ];
    for (index, (method, mut params)) in methods.into_iter().enumerate() {
        let op_id = format!("op-{index}");
        params["initiator"] = serde_json::to_value(seat(1)).unwrap();
        params["opId"] = json!(op_id);
        // Every carrier hands `interaction.answer` to the interaction runtime
        // before the shared table; the others go through `dispatch_frame`.
        let refused = if method == "interaction.answer" {
            node.dispatch_interaction(method, params).await
        } else {
            dispatch(&node, method, params).await
        };
        assert!(
            matches!(refused, Err(NodeError::Fenced)),
            "{method} must be refused at entry: {refused:?}"
        );
    }
    // The acknowledgement of a re-delivered fence reports each as refused.
    let ack = dispatch(&node, METHOD_FENCE, fence_params("fen_direct", 2))
        .await
        .unwrap();
    for index in 0..6 {
        assert_eq!(
            outcome_of(&ack, &format!("op-{index}")).as_deref(),
            Some("refused"),
            "{ack}"
        );
    }
}

/// §7.10 "Crash ambiguity", Node half: the Node dies after a PTY prompt's
/// write was attempted and before any delivery evidence, with a direct
/// operation's entry logged and no outcome. After a restart over the same
/// data dir and the fence's application, the acknowledgement reports both
/// `unknown` — never `cancelled` — and omits what it holds no record of.
#[tokio::test]
async fn after_a_crash_the_fence_reports_an_attempted_write_and_a_logged_entry_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let command;
    {
        let store = Arc::new(MemoryStore::open_journaled(dir.path(), 128).unwrap());
        let (node, driver) = node_with(store, Some(dir.path()));
        let worker = worker(&node, &driver).await;
        driver.hold_send.store(true, Ordering::SeqCst);
        driver.send_started.store(false, Ordering::SeqCst);
        let reply = seat_send(&node, &worker, "WRITE-ATTEMPTED", 1)
            .await
            .unwrap();
        command = command_id_of(&reply);
        driver.ready.store(true, Ordering::SeqCst);
        wait("the write began", || {
            driver.send_started.load(Ordering::SeqCst)
        })
        .await;
        let claimed = node.get_command(&command.parse().unwrap()).unwrap();
        assert_eq!(
            claimed.dispatch,
            remuda_protocol::DispatchState::TransportWritten,
            "the durable write-attempt marker precedes the write"
        );
        // A direct op whose entry is durable but whose outcome never landed.
        node.inner
            .fences
            .direct_entry(
                "worker.remove",
                &json!({"initiator": seat(1), "opId": "op-crashed"}),
            )
            .unwrap();
        // Hard death: no graceful shutdown.
        drop(node);
    }
    let store = Arc::new(MemoryStore::open_journaled(dir.path(), 128).unwrap());
    let (node, _driver) = node_with(store, Some(dir.path()));
    let ack = dispatch(&node, METHOD_FENCE, fence_params("fen_crash", 2))
        .await
        .unwrap();
    assert_eq!(
        outcome_of(&ack, &command).as_deref(),
        Some("unknown"),
        "{ack}"
    );
    assert_eq!(
        outcome_of(&ack, "op-crashed").as_deref(),
        Some("unknown"),
        "{ack}"
    );
    assert_eq!(outcome_of(&ack, "op-never-seen"), None);
    // The persisted fence refuses the old generation after the restart.
    assert!(matches!(
        node.inner.fences.refuse_fenced(Some(&seat(1))),
        Err(NodeError::Fenced)
    ));
}

/// §7.6: a fence closes this Node's instances of the lineage below the live
/// generation (known from create's `lineage` param) and leaves the live
/// chapter and other lineages alone.
#[tokio::test]
async fn a_fence_closes_the_fenced_chapter_and_refuses_its_stale_recreate() {
    let (node, driver) = node();
    driver.ready.store(true, Ordering::SeqCst);
    let chapter = remuda_protocol::InstanceId::new();
    dispatch(
        &node,
        "instance.create",
        json!({
            "instanceId": chapter.as_id().as_str(),
            "commandId": remuda_protocol::CommandId::new().as_id().as_str(),
            "origin": "human",
            "lineage": {"id": LINEAGE, "generation": "1"},
            "spec": {"kind": "claude", "driver": "claude-pty"},
        }),
    )
    .await
    .unwrap();
    wait("chapter ready", || {
        node.get_instance(&chapter)
            .is_ok_and(|instance| instance.lifecycle == remuda_protocol::InstanceLifecycle::Ready)
    })
    .await;
    let ack = dispatch(&node, METHOD_FENCE, fence_params("fen_close", 2))
        .await
        .unwrap();
    assert_eq!(
        ack["closedInstanceIds"],
        json!([chapter.as_id().as_str()]),
        "{ack}"
    );
    wait("the fenced chapter exited", || {
        node.get_instance(&chapter)
            .is_ok_and(|instance| instance.lifecycle == remuda_protocol::InstanceLifecycle::Exited)
    })
    .await;
    // A create replayed for the fenced generation is refused.
    let stale = dispatch(
        &node,
        "instance.create",
        json!({
            "instanceId": remuda_protocol::InstanceId::new().as_id().as_str(),
            "commandId": remuda_protocol::CommandId::new().as_id().as_str(),
            "origin": "human",
            "lineage": {"id": LINEAGE, "generation": "1"},
            "spec": {"kind": "claude", "driver": "claude-pty"},
        }),
    )
    .await;
    assert!(matches!(stale, Err(NodeError::Fenced)), "{stale:?}");
}

const METHOD_FENCE: &str = remuda_protocol::METHOD_LINEAGE_FENCE;
