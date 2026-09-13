//! In-process Hub + fake Node: recorded Feishu inbound creates an instance
//! and a tool-boundary journal event becomes a DryRun progress card.
//!
//! Source inbound: `tests/fixtures/im-message-p2p.jsonl` (synthetic consume line).

use futures::{SinkExt, StreamExt};
use remuda_feishu::{
    ConsumeEvent, DispatchAction, Dispatcher, HubInstanceApi, InboundPolicy, LarkCli,
    RouteDefaults, parse_event_line,
};
use remuda_hub::{HubConfig, spawn};
use remuda_hub_client::HubClient;
use remuda_protocol::hubnode::{
    HubNodeRequest, HubNodeResponse, JournalAppendParams, METHOD_JOURNAL_APPEND, METHOD_NODE_HELLO,
    NodeHelloParams, NodeHostInventory,
};
use remuda_protocol::{HostId, JsonRpcVersion, PROTOCOL_VERSION};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

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

fn policy() -> InboundPolicy {
    InboundPolicy {
        owner_open_ids: vec!["ou_owner_aaaaaaaaaaaaaaaaaaaaaaaaaa".into()],
        chat_allowlist: vec!["oc_p2p_aaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()],
        bot_open_id: Some("ou_bot_cccccccccccccccccccccccccccc".into()),
        bot_name: Some("Remuda".into()),
        allow_unaddressed: false,
    }
}

fn consume_event() -> ConsumeEvent {
    ConsumeEvent::Event {
        event_key: "im.message.receive_v1".into(),
        event: Box::new(parse_event_line(&fixture_line()).unwrap()),
    }
}

#[tokio::test]
async fn inbound_fixture_creates_instance_and_posts_progress_card() {
    let dir = tempfile::tempdir().unwrap();
    let config = HubConfig::for_test(dir.path().join("data"));
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

    let client = HubClient::new(format!("http://{addr}"), None, Some(bootstrap)).unwrap();
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

    let api = HubInstanceApi::new(client);
    let mut dispatcher =
        Dispatcher::memory(api, LarkCli::dry_run(), policy(), RouteDefaults::default()).unwrap();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
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
    let key = "feishu:oc_p2p_aaaaaaaaaaaaaaaaaaaaaaaaaaaa:main";
    let binding = dispatcher.sessions().get(key).unwrap().unwrap();
    let instance_id = binding.instance_id.expect("instance mapped");

    let append = HubNodeRequest {
        jsonrpc: JsonRpcVersion::V2,
        id: Some(json!("append")),
        version: Some(PROTOCOL_VERSION),
        method: METHOD_JOURNAL_APPEND.to_string(),
        params: Some(
            serde_json::to_value(JournalAppendParams {
                instance_id: instance_id.as_id().as_str().to_string(),
                event: None,
                events: vec![json!({
                    "kind": "tool",
                    "payload": {
                        "name": "Read",
                        "summary": "README.md",
                        "elapsedSecs": 2
                    }
                })],
                seq: None,
                watermark: None,
            })
            .unwrap(),
        ),
    };
    {
        let mut s = sink.lock().await;
        s.send(Message::Text(
            serde_json::to_string(&append).unwrap().into(),
        ))
        .await
        .unwrap();
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let follow = dispatcher.follow_live(now).await.unwrap();
        if follow.iter().any(|r| {
            matches!(
                &r.action,
                DispatchAction::CardPosted { kind } if kind == "progress"
            )
        }) {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("expected DryRun progress card: {follow:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        dispatcher.outbound().recorded().iter().any(|cmd| cmd
            .argv
            .iter()
            .any(|a| a == "interactive" || a.contains("interactive"))),
        "progress card should be DryRun interactive outbound: {:?}",
        dispatcher.outbound().recorded()
    );
}
