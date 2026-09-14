//! In-process Hub + fake Node, driven through the real Feishu dispatcher.
//!
//! Covers design §5.1 (batch co-botfix):
//! - real `ObservationKind` journal events (`tool_call`/`tool_result`/`lifecycle`)
//!   drive the golden progress and completion cards;
//! - a Bot device token answers an interaction exactly when the acting
//!   `open_id` is on the Hub-side allowlist and an open card ticket binds the
//!   interaction (403 otherwise), with an audit row naming the `open_id`;
//! - a dispatcher restart rehydrates ticket ↔ instance bindings from the Hub.
//!
//! Source inbound: `tests/fixtures/im-message-p2p.jsonl` (synthetic consume line).
//! No real Feishu network: outbound is DryRun, the Node is an in-test WS fake.

use futures::{SinkExt, StreamExt};
use remuda_feishu::{
    ConsumeEvent, DispatchAction, Dispatcher, HubInstanceApi, HubTicketBackend, InboundPolicy,
    LarkCli, RouteDefaults, TicketBackend, parse_event_line,
};
use remuda_hub::{HubConfig, spawn};
use remuda_hub_client::HubClient;
use remuda_protocol::hubnode::{
    HubNodeRequest, HubNodeResponse, JournalAppendParams, METHOD_JOURNAL_APPEND, METHOD_NODE_HELLO,
    NodeHelloParams, NodeHostInventory,
};
use remuda_protocol::{
    ApprovalAnswer, HostId, Interaction, InteractionAnswer, InteractionId, JsonRpcVersion,
    PROTOCOL_VERSION,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const OWNER_OPEN_ID: &str = "ou_owner_aaaaaaaaaaaaaaaaaaaaaaaaaa";
const CHAT_ID: &str = "oc_p2p_aaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SESSION_KEY: &str = "feishu:oc_p2p_aaaaaaaaaaaaaaaaaaaaaaaaaaaa:main";

type WsSink = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

/// One in-process Hub plus a fake Node connected over WS. The caller owns the
/// returned [`remuda_hub::RunningHub`] so it can mint tokens and query the store.
struct HubNode {
    addr: std::net::SocketAddr,
    bootstrap: String,
    sink: Arc<Mutex<WsSink>>,
}

fn policy() -> InboundPolicy {
    InboundPolicy {
        owner_open_ids: vec![OWNER_OPEN_ID.into()],
        chat_allowlist: vec![CHAT_ID.into()],
        bot_open_id: Some("ou_bot_cccccccccccccccccccccccccccc".into()),
        bot_name: Some("Remuda".into()),
        allow_unaddressed: false,
    }
}

fn fixture_line() -> String {
    let text = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/im-message-p2p.jsonl"),
    )
    .unwrap();
    text.lines()
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap()
        .to_string()
}

fn consume_event() -> ConsumeEvent {
    ConsumeEvent::Event {
        event_key: "im.message.receive_v1".into(),
        event: Box::new(parse_event_line(&fixture_line()).unwrap()),
    }
}

fn load_interaction() -> Interaction {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/interaction-approval.json");
    remuda_protocol::from_json_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn golden(name: &str) -> Value {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/goldens")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Find a DryRun-recorded interactive card whose predicate matches and return
/// its parsed Card JSON 2.0 body.
fn posted_card<F>(dispatcher: &Dispatcher<HubInstanceApi>, pred: F) -> Value
where
    F: Fn(&Value) -> bool,
{
    dispatcher
        .outbound()
        .recorded()
        .iter()
        .filter_map(|cmd| {
            let pos = cmd.argv.iter().position(|a| a == "--content")?;
            serde_json::from_str::<Value>(cmd.argv.get(pos + 1)?).ok()
        })
        .find(pred)
        .expect("matching interactive card was posted")
}

async fn start_hub_node(dir: &std::path::Path) -> (HubNode, remuda_hub::RunningHub) {
    let config = HubConfig::for_test(dir.join("data"));
    let hub = spawn(config).await.unwrap();
    let bootstrap = hub.bootstrap_token.clone();
    let addr = hub.addr;

    // D-018: the access code pairs devices; a Node enrolls with a single-use
    // enroll token. The in-process Hub mints one directly.
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await
        .unwrap();
    let mut req = format!("ws://{addr}/v1/node")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let (sink, mut stream) = ws.split();
    let sink = Arc::new(Mutex::new(sink));
    let reply_sink = Arc::clone(&sink);
    tokio::spawn(async move {
        while let Some(Ok(frame)) = stream.next().await {
            let text = match frame {
                Message::Text(t) => t.to_string(),
                Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                _ => continue,
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            // Fake Node accepts every RPC (journal append, interaction answer).
            if value.get("method").is_some()
                && let Some(id) = value.get("id").cloned()
            {
                let reply = HubNodeResponse {
                    jsonrpc: JsonRpcVersion::V2,
                    id,
                    version: Some(PROTOCOL_VERSION),
                    result: Some(json!({ "ok": true })),
                    error: None,
                };
                let _ = reply_sink
                    .lock()
                    .await
                    .send(Message::Text(serde_json::to_string(&reply).unwrap().into()))
                    .await;
            }
        }
    });

    let host_id = HostId::new();
    let hello = HubNodeRequest {
        jsonrpc: JsonRpcVersion::V2,
        id: Some(json!("hello")),
        version: Some(PROTOCOL_VERSION),
        method: METHOD_NODE_HELLO.to_string(),
        params: Some(
            serde_json::to_value(NodeHelloParams {
                host_id: Some(host_id.as_id().as_str().to_string()),
                label: Some("fake-node".into()),
                node_version: Some("0.1.0-test".into()),
                node_epoch: None,
                enrollment_token: None,
                transport: Some("outbound-wss".into()),
                version: Some(PROTOCOL_VERSION),
                protocol: None,
                host: Some(NodeHostInventory {
                    host_id: Some(host_id.as_id().as_str().to_string()),
                    hostname: Some("fake-node.local".into()),
                    labels: Some(json!({ "role": "canary" })),
                    max_instances: Some(4),
                    cli: None,
                    herdr: None,
                    resources: None,
                    os: None,
                    kernel: None,
                    libc: None,
                    label: Some("fake-node".into()),
                    workspaces: None,
                    workspace_revision: None,
                }),
                capabilities: None,
                cli: None,
            })
            .unwrap(),
        ),
    };
    {
        let mut s = sink.lock().await;
        s.send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
            .await
            .unwrap();
    }

    let client = HubClient::new(format!("http://{addr}"), None, Some(bootstrap.clone())).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let hosts = client.list_hosts().await.unwrap();
        if hosts.iter().any(|h| h.get("online") == Some(&json!(true))) {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("fake node should be online: {hosts:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    (
        HubNode {
            addr,
            bootstrap,
            sink,
        },
        hub,
    )
}

impl HubNode {
    async fn append(&self, instance_id: &str, events: Vec<Value>) {
        let append = HubNodeRequest {
            jsonrpc: JsonRpcVersion::V2,
            id: Some(json!(format!("append-{}", events.len()))),
            version: Some(PROTOCOL_VERSION),
            method: METHOD_JOURNAL_APPEND.to_string(),
            params: Some(
                serde_json::to_value(JournalAppendParams {
                    instance_id: instance_id.to_string(),
                    event: None,
                    events,
                    seq: None,
                    watermark: None,
                })
                .unwrap(),
            ),
        };
        let mut s = self.sink.lock().await;
        s.send(Message::Text(
            serde_json::to_string(&append).unwrap().into(),
        ))
        .await
        .unwrap();
    }

    fn http(&self, token: Option<String>) -> HubClient {
        HubClient::new(format!("http://{}", self.addr), token, None).unwrap()
    }

    fn bootstrap_http(&self) -> HubClient {
        HubClient::new(
            format!("http://{}", self.addr),
            None,
            Some(self.bootstrap.clone()),
        )
        .unwrap()
    }
}

/// Create the session instance through the dispatcher, driven by the recorded
/// p2p message fixture.
async fn create_session(
    dispatcher: &mut Dispatcher<HubInstanceApi>,
    now: SystemTime,
) -> remuda_protocol::InstanceId {
    let reports = dispatcher
        .handle_consume(consume_event(), now)
        .await
        .unwrap();
    assert!(
        reports
            .iter()
            .any(|r| matches!(r.action, DispatchAction::Created { .. })),
        "expected create from inbound fixture: {reports:?}"
    );
    dispatcher
        .sessions()
        .get(SESSION_KEY)
        .unwrap()
        .unwrap()
        .instance_id
        .expect("instance mapped")
}

/// Poll `follow_live` until a report with `kind` appears or the deadline hits.
async fn wait_for_card(dispatcher: &mut Dispatcher<HubInstanceApi>, kind: &str, now: SystemTime) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let follow = dispatcher.follow_live(now).await.unwrap();
        if follow
            .iter()
            .any(|r| matches!(&r.action, DispatchAction::CardPosted { kind: k } if k == kind))
        {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("expected {kind} card: {follow:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Real ObservationKind vocabulary (enums.rs): tool_call drives progress.
#[tokio::test]
async fn real_tool_call_observation_posts_golden_progress_card() {
    let dir = tempfile::tempdir().unwrap();
    let (node, hub) = start_hub_node(dir.path()).await;
    let api = HubInstanceApi::new(node.bootstrap_http());
    let mut dispatcher =
        Dispatcher::memory(api, LarkCli::dry_run(), policy(), RouteDefaults::default()).unwrap();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let instance_id = create_session(&mut dispatcher, now).await;

    node.append(
        instance_id.as_id().as_str(),
        vec![json!({
            "kind": "tool_call",
            "payload": {
                "toolName": "Read",
                "displayTitle": "README.md"
            }
        })],
    )
    .await;
    wait_for_card(&mut dispatcher, "progress", now).await;

    let card = posted_card(&dispatcher, |c| {
        c["header"]["title"]["content"] == json!("Running")
    });
    assert_eq!(card, golden("progress-card.json"));
    drop(hub);
}

/// tool_result refreshes progress; native lifecycle is ignored; terminal run
/// and instance lifecycle events post the green/red completion cards.
#[tokio::test]
async fn tool_result_and_terminal_lifecycle_post_golden_cards() {
    let dir = tempfile::tempdir().unwrap();
    let (node, hub) = start_hub_node(dir.path()).await;
    let api = HubInstanceApi::new(node.bootstrap_http());
    let mut dispatcher =
        Dispatcher::memory(api, LarkCli::dry_run(), policy(), RouteDefaults::default()).unwrap();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let instance_id = create_session(&mut dispatcher, now).await;

    // tool_result is the other real progress trigger.
    node.append(
        instance_id.as_id().as_str(),
        vec![json!({
            "kind": "tool_result",
            "payload": { "stage": "final", "outcome": "succeeded", "blocks": [] }
        })],
    )
    .await;
    wait_for_card(&mut dispatcher, "progress", now).await;

    // A native (non-entity) lifecycle must NOT post a completion card.
    node.append(
        instance_id.as_id().as_str(),
        vec![json!({
            "kind": "lifecycle",
            "payload": { "type": "native", "topic": "session", "nativeName": "model-switch" }
        })],
    )
    .await;
    let before = dispatcher.outbound().recorded().len();
    let _ = dispatcher.follow_live(now).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = dispatcher.follow_live(now).await.unwrap();
    assert_eq!(before, dispatcher.outbound().recorded().len());

    // Terminal run lifecycle -> green completion.
    node.append(
        instance_id.as_id().as_str(),
        vec![json!({
            "kind": "lifecycle",
            "payload": {
                "type": "entity",
                "entityType": "run",
                "entityId": "run_golden",
                "revision": "1",
                "state": "succeeded",
                "reasonCode": "run-complete",
                "evidenceEventIds": []
            }
        })],
    )
    .await;
    wait_for_card(&mut dispatcher, "completion", now).await;
    let ok_card = posted_card(&dispatcher, |c| c["header"]["template"] == json!("green"));
    assert_eq!(ok_card, golden("completion-ok-card.json"));

    // A failed instance lifecycle on the same journal -> red completion card.
    node.append(
        instance_id.as_id().as_str(),
        vec![json!({
            "kind": "lifecycle",
            "payload": {
                "type": "entity",
                "entityType": "instance",
                "entityId": instance_id.as_id().as_str(),
                "revision": "2",
                "state": "failed",
                "reasonCode": "tool-exit-1",
                "evidenceEventIds": []
            }
        })],
    )
    .await;
    wait_for_card(&mut dispatcher, "completion", now).await;
    let failed_card = posted_card(&dispatcher, |c| c["header"]["template"] == json!("red"));
    assert_eq!(failed_card, golden("completion-failed-card.json"));
    drop(hub);
}

/// Bot token relay: 403 without an acting open_id, with a non-allowlisted
/// open_id, or without an open ticket binding; 200 with all three; the audit
/// row names the acting open_id.
#[tokio::test]
async fn bot_token_relay_requires_allowlisted_open_id_and_open_ticket() {
    let dir = tempfile::tempdir().unwrap();
    let (node, hub) = start_hub_node(dir.path()).await;
    let bot_token = hub
        .mint_bot_device_token("remuda-dispatcher")
        .await
        .unwrap();
    let bot = node.http(Some(bot_token));

    // Dispatcher boot: register this bot's owner allowlist.
    remuda_feishu::register_owner_allowlist(&bot, &[OWNER_OPEN_ID.to_string()])
        .await
        .unwrap();

    let interaction = load_interaction();
    let instance_id = interaction.instance_id.as_id().as_str().to_string();
    let interaction_id = interaction.meta.id.as_id().as_str().to_string();
    node.append(
        &instance_id,
        vec![json!({
            "kind": "interaction.requested",
            "payload": { "interaction": serde_json::to_value(&interaction).unwrap() }
        })],
    )
    .await;

    // Wait for the durable interaction row.
    let store = hub.store().expect("store");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if store
            .get_interaction(interaction_id.clone())
            .await
            .unwrap()
            .is_some()
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("interaction row was never persisted");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let input_digest = match &interaction.request {
        remuda_protocol::InteractionRequest::Approval(req) => req.input_digest.clone(),
        _ => panic!("approval fixture"),
    };
    let answer = json!(InteractionAnswer::Approval(Box::new(ApprovalAnswer {
        option_id: "allow".into(),
        input_digest,
    })));
    let answer_path = format!("/v1/interactions/{interaction_id}/answer");

    // No acting open_id -> 403.
    let err = bot
        .post(&answer_path, &json!({ "answer": &answer }))
        .await
        .expect_err("bot answer without acting open_id must be refused");
    assert!(err.to_string().contains("403"), "{err}");

    // Non-allowlisted open_id -> 403.
    let err = bot
        .post(
            &answer_path,
            &json!({ "answer": &answer, "actingOpenId": "ou_stranger_xxxxxxxxxxxxxxxxxxxxxxxxx" }),
        )
        .await
        .expect_err("stranger open_id must be refused");
    assert!(err.to_string().contains("403"), "{err}");

    // Allowlisted open_id but no open card ticket binding -> 403.
    let err = bot
        .post(
            &answer_path,
            &json!({ "answer": &answer, "actingOpenId": OWNER_OPEN_ID }),
        )
        .await
        .expect_err("allowlisted relay without a ticket binding must be refused");
    assert!(err.to_string().contains("403"), "{err}");

    // Dispatcher issue path: open card ticket bound to this bot.
    let now_ms = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    bot.post(
        "/v1/bot/card-tickets",
        &json!({
            "ticketId": "tidgolden01",
            "interactionId": interaction_id,
            "instanceId": instance_id,
            "sessionKey": SESSION_KEY,
            "requestVersion": interaction.request_version.0.to_string(),
            "processGeneration": interaction.request_key.process_generation.0.to_string(),
            "requestJson": serde_json::to_string(&interaction.request).unwrap(),
            "createdAtMs": now_ms,
            "expiresAtMs": now_ms + 12 * 60 * 1000
        }),
    )
    .await
    .unwrap();

    // Allowlisted open_id + open ticket -> the fake Node accepts (200).
    let result = bot
        .post(
            &answer_path,
            &json!({ "answer": &answer, "actingOpenId": OWNER_OPEN_ID }),
        )
        .await
        .expect("allowlisted relay with open ticket must succeed");
    assert_eq!(result.get("ok"), Some(&json!(true)));

    // The audit trail names the acting human open_id, not just the bot device.
    let audit = store.audit_for(interaction_id.clone()).await.unwrap();
    let row = audit
        .iter()
        .find(|row| row["action"] == json!("interaction.bot-answer"))
        .expect("bot-answer audit row");
    assert_eq!(row["detail"]["actingOpenId"], json!(OWNER_OPEN_ID));
    assert_eq!(row["detail"]["relayedBy"], json!("feishu"));
    assert!(
        row["deviceId"]
            .as_str()
            .is_some_and(|id| id.starts_with("dev_"))
    );

    // A second relay for the now-committed answer must not double-write.
    let _ = bot
        .post(
            &answer_path,
            &json!({ "answer": &answer, "actingOpenId": OWNER_OPEN_ID }),
        )
        .await;
    let audit = store.audit_for(interaction_id.clone()).await.unwrap();
    assert_eq!(
        audit
            .iter()
            .filter(|row| row["action"] == json!("interaction.bot-answer"))
            .count(),
        1,
        "exactly one audit row for the winning answer: {audit:?}"
    );
    drop(hub);
}

/// A dispatcher restart keeps ticket ↔ instance bindings: the restarted
/// process hydrates the open ticket and relays the owner's card click end to
/// end against the same (fake) Hub.
#[tokio::test]
async fn dispatcher_restart_keeps_ticket_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let (node, hub) = start_hub_node(dir.path()).await;
    let bot_token = hub
        .mint_bot_device_token("remuda-dispatcher")
        .await
        .unwrap();
    let session_db = dir.path().join("dispatcher/sessions.sqlite");
    let now = SystemTime::now();
    let interaction_id = InteractionId::new();

    // First dispatcher generation: allowlist, session, interaction card.
    {
        let client = node.http(Some(bot_token.clone()));
        remuda_feishu::register_owner_allowlist(&client, &[OWNER_OPEN_ID.to_string()])
            .await
            .unwrap();
        let backend: Arc<dyn TicketBackend> = Arc::new(HubTicketBackend::new(client.clone()));
        let mut dispatcher = Dispatcher::open_with_backend(
            session_db.clone(),
            HubInstanceApi::new(client),
            LarkCli::dry_run(),
            policy(),
            RouteDefaults::default(),
            backend,
        )
        .unwrap();
        dispatcher.hydrate_tickets(now).await.unwrap();
        let instance_id = create_session(&mut dispatcher, now).await;

        let mut interaction = load_interaction();
        interaction.meta.id = interaction_id.clone();
        interaction.instance_id = instance_id;
        node.append(
            interaction.instance_id.as_id().as_str(),
            vec![json!({
                "kind": "interaction.requested",
                "payload": { "interaction": serde_json::to_value(&interaction).unwrap() }
            })],
        )
        .await;
        wait_for_card(&mut dispatcher, "interaction", now).await;
        drop(dispatcher);
    }

    // Second generation: fresh in-memory state, same Hub and session db.
    {
        let client = node.http(Some(bot_token.clone()));
        let backend: Arc<dyn TicketBackend> = Arc::new(HubTicketBackend::new(client.clone()));
        let mut dispatcher = Dispatcher::open_with_backend(
            session_db,
            HubInstanceApi::new(client),
            LarkCli::dry_run(),
            policy(),
            RouteDefaults::default(),
            backend,
        )
        .unwrap();
        let loaded = dispatcher.hydrate_tickets(SystemTime::now()).await.unwrap();
        assert_eq!(loaded, 1, "the open ticket binding survives restart");
        let ticket = dispatcher
            .tickets()
            .open_for_session(SESSION_KEY, SystemTime::now())
            .next()
            .expect("hydrated open ticket for session");

        // Rebuild the owner's card click against the hydrated ticket.
        let fixture = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/card-action-allow.jsonl"),
        )
        .unwrap();
        let line = fixture
            .lines()
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .unwrap();
        let mut raw: Value = serde_json::from_str(line).unwrap();
        raw["action_value"] = json!(json!({ "tid": ticket.ticket_id, "a": "allow" }).to_string());
        let event = parse_event_line(&raw.to_string()).expect("card action line parses");
        let reports = dispatcher
            .handle_consume(
                ConsumeEvent::Event {
                    event_key: remuda_feishu::EVENT_CARD_ACTION.into(),
                    event: Box::new(event),
                },
                SystemTime::now(),
            )
            .await
            .unwrap();
        assert!(
            reports
                .iter()
                .any(|r| matches!(r.action, DispatchAction::Answered { .. })),
            "hydrated ticket accepts the owner click after restart: {reports:?}"
        );

        let store = hub.store().expect("store");
        let audit = store
            .audit_for(interaction_id.as_id().as_str().to_string())
            .await
            .unwrap();
        assert!(
            audit.iter().any(|row| {
                row["action"] == json!("interaction.bot-answer")
                    && row["detail"]["actingOpenId"] == json!(OWNER_OPEN_ID)
            }),
            "post-restart relay audited with the owner open_id: {audit:?}"
        );
    }
    drop(hub);
}
