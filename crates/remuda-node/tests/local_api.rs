//! End-to-end loopback API fixture tests. No native CLI or model is invoked.

use futures::{SinkExt, StreamExt};
use remuda_node::{DevNode, DevServerConfig, dev_router};
use serde_json::{Value, json};
use std::{collections::BTreeSet, net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    task::JoinHandle,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

struct TestServer {
    address: SocketAddr,
    task: JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn spawn_server(config: DevServerConfig) -> TestServer {
    let node = DevNode::new(&config).expect("compose local node");
    let router = dev_router(node, &config);
    let listener = tokio::net::TcpListener::bind(config.bind_addr)
        .await
        .expect("bind test listener");
    let address = listener.local_addr().expect("listener address");
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("serve local router");
    });
    TestServer { address, task }
}

struct HttpResponse {
    status: u16,
    headers: String,
    body: Value,
}

async fn http_json(
    address: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
    headers: &[(&str, &str)],
) -> HttpResponse {
    let body = body.unwrap_or_default();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if !body.is_empty() {
        request.push_str("Content-Type: application/json\r\n");
    }
    for (name, value) in headers {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    request.push_str(body);

    let mut stream = TcpStream::connect(address).await.expect("connect HTTP");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write HTTP request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read HTTP response");
    let response = String::from_utf8(response).expect("UTF-8 HTTP response");
    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("HTTP header delimiter");
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .expect("HTTP status");
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body).expect("JSON HTTP body")
    };
    HttpResponse {
        status,
        headers: head.to_owned(),
        body,
    }
}

type TestSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn next_json(socket: &mut TestSocket) -> Value {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(3), socket.next())
            .await
            .expect("WebSocket message timeout")
            .expect("open WebSocket")
            .expect("valid WebSocket frame");
        if let Message::Text(text) = message {
            return serde_json::from_str(&text).expect("JSON WebSocket frame");
        }
    }
}

async fn next_batch(socket: &mut TestSocket) -> Value {
    loop {
        let frame = next_json(socket).await;
        if frame["method"] == "events.batch" {
            return frame;
        }
    }
}

async fn rpc(socket: &mut TestSocket, id: &str, method: &str, params: Value) -> Value {
    socket
        .send(Message::Text(
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
                .to_string()
                .into(),
        ))
        .await
        .expect("send RPC request");
    loop {
        let response = next_json(socket).await;
        if response["id"] == id {
            assert!(response.get("error").is_none(), "RPC failed: {response}");
            return response["result"].clone();
        }
    }
}

fn seq(value: &Value) -> u64 {
    value
        .as_str()
        .expect("wire U64 string")
        .parse()
        .expect("wire U64 number")
}

async fn wait_for_settled(address: SocketAddr, command_id: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let response = http_json(
                address,
                "GET",
                &format!("/v1/commands/{command_id}"),
                None,
                &[],
            )
            .await;
            assert_eq!(response.status, 200);
            if response.body["state"] == "settled" {
                return response.body;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("command settled")
}

#[tokio::test]
async fn create_send_follow_cancel_and_reconnect_catch_up_without_duplicates() {
    let server = spawn_server(DevServerConfig::loopback(0)).await;
    let fixture = include_str!("fixtures/create-instance.json");
    let created = http_json(server.address, "POST", "/v1/instances", Some(fixture), &[]).await;
    assert_eq!(created.status, 201);
    assert_eq!(created.body["command"]["state"], "settled");
    let instance_id = created.body["instance"]["id"]
        .as_str()
        .expect("instance id")
        .to_owned();

    let follow_url = format!("ws://{}/v1/instances/{instance_id}/follow", server.address);
    let (mut follow, _) = connect_async(&follow_url).await.expect("connect follow");
    let snapshot = next_json(&mut follow).await;
    assert_eq!(snapshot["type"], "snapshot");
    let snapshot_seq = seq(&snapshot["result"]["durableSeq"]);

    let sent = http_json(
        server.address,
        "POST",
        &format!("/v1/instances/{instance_id}/commands"),
        Some(&json!({"operation": "send", "prompt": "hello"}).to_string()),
        &[],
    )
    .await;
    assert_eq!(sent.status, 200);
    assert_eq!(sent.body["command"]["state"], "accepted");
    let sent_command_id = sent.body["command"]["commandId"]
        .as_str()
        .expect("send command id")
        .to_owned();
    let first_batch = next_batch(&mut follow).await;
    let last_live_seq = seq(&first_batch["params"]["toSeq"]);
    assert!(last_live_seq > snapshot_seq);
    let settled = wait_for_settled(server.address, &sent_command_id).await;
    assert_eq!(settled["settlement"]["state"], "known");

    let cancelled = http_json(
        server.address,
        "POST",
        &format!("/v1/instances/{instance_id}/commands"),
        Some(&json!({"operation": "cancel"}).to_string()),
        &[],
    )
    .await;
    assert_eq!(cancelled.status, 200);
    assert_eq!(cancelled.body["command"]["operation"], "instance.cancel");
    follow.close(None).await.expect("close first follow");

    let offline = http_json(
        server.address,
        "POST",
        &format!("/v1/instances/{instance_id}/commands"),
        Some(&json!({"operation": "send", "prompt": "offline"}).to_string()),
        &[],
    )
    .await;
    assert_eq!(offline.status, 200);
    tokio::time::sleep(Duration::from_millis(25)).await;

    let catch_up = http_json(
        server.address,
        "GET",
        &format!("/v1/instances/{instance_id}/journal?from_seq={last_live_seq}"),
        None,
        &[],
    )
    .await;
    assert_eq!(catch_up.status, 200);
    let events = catch_up.body["events"].as_array().expect("catch-up events");
    assert!(!events.is_empty());
    let sequences: Vec<_> = events.iter().map(|event| seq(&event["seq"])).collect();
    assert!(sequences.iter().all(|sequence| *sequence > last_live_seq));
    assert_eq!(
        sequences.iter().copied().collect::<BTreeSet<_>>().len(),
        sequences.len()
    );
    let caught_up_through = seq(&catch_up.body["durableSeq"]);

    let reconnect_url = format!("{follow_url}?from_seq={caught_up_through}");
    let (mut reconnected, _) = connect_async(reconnect_url)
        .await
        .expect("reconnect follow");
    let reconnected_snapshot = next_json(&mut reconnected).await;
    assert_eq!(reconnected_snapshot["type"], "snapshot");
    let reconnect_watermark = seq(&reconnected_snapshot["result"]["durableSeq"]);
    assert!(reconnect_watermark >= caught_up_through);

    let cancel_again = http_json(
        server.address,
        "POST",
        &format!("/v1/instances/{instance_id}/commands"),
        Some(&json!({"operation": "cancel"}).to_string()),
        &[],
    )
    .await;
    assert_eq!(cancel_again.status, 200);
    let next = next_batch(&mut reconnected).await;
    assert!(seq(&next["params"]["fromSeq"]) > reconnect_watermark);
}

#[tokio::test]
async fn access_code_cookie_and_exact_origin_are_enforced() {
    let config = DevServerConfig::loopback(0)
        .with_access_code("correct horse".to_owned())
        .expect("access code")
        .with_allowed_origins(vec!["http://localhost:5173".to_owned()])
        .expect("origin");
    let server = spawn_server(config).await;

    let missing = http_json(server.address, "GET", "/healthz", None, &[]).await;
    assert_eq!(missing.status, 401);
    let bad_origin = http_json(
        server.address,
        "GET",
        "/healthz",
        None,
        &[
            ("Origin", "http://evil.invalid"),
            ("x-remuda-access-code", "correct horse"),
        ],
    )
    .await;
    assert_eq!(bad_origin.status, 403);

    let session = http_json(
        server.address,
        "POST",
        "/v1/dev/session",
        None,
        &[
            ("Origin", "http://localhost:5173"),
            ("x-remuda-access-code", "correct horse"),
        ],
    )
    .await;
    assert_eq!(session.status, 204);
    assert!(
        session
            .headers
            .contains("access-control-allow-origin: http://localhost:5173")
    );
    let cookie = session
        .headers
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("set-cookie:"))
        .and_then(|line| line.split_once(':'))
        .map(|(_, value)| value.trim())
        .and_then(|value| value.split_once(';'))
        .map(|(cookie, _)| cookie.to_owned())
        .expect("session cookie");
    let authenticated = http_json(
        server.address,
        "GET",
        "/healthz",
        None,
        &[("Cookie", &cookie)],
    )
    .await;
    assert_eq!(authenticated.status, 200);
}

#[tokio::test]
async fn interaction_response_uses_the_bounded_instance_command_queue() {
    let server = spawn_server(DevServerConfig::loopback(0)).await;
    let created = http_json(
        server.address,
        "POST",
        "/v1/instances",
        Some(include_str!("fixtures/create-instance.json")),
        &[],
    )
    .await;
    assert_eq!(created.status, 201);
    let instance_id = created.body["instance"]["id"]
        .as_str()
        .expect("instance id");
    let interaction_id = remuda_protocol::InteractionId::new();
    let response = http_json(
        server.address,
        "POST",
        &format!("/v1/instances/{instance_id}/commands"),
        Some(
            &json!({
                "operation": "respond_interaction",
                "interactionId": interaction_id,
                "answer": {"choice": "allow-once"}
            })
            .to_string(),
        ),
        &[],
    )
    .await;
    assert_eq!(response.status, 200);
    assert_eq!(response.body["command"]["operation"], "interaction.respond");
    assert_eq!(response.body["command"]["state"], "accepted");
    let command_id = response.body["command"]["commandId"]
        .as_str()
        .expect("command id");
    let settled = wait_for_settled(server.address, command_id).await;
    assert_eq!(settled["state"], "settled");
}

#[tokio::test]
async fn one_follow_socket_multiplexes_two_instances() {
    let server = spawn_server(DevServerConfig::loopback(0)).await;
    let fixture = include_str!("fixtures/create-instance.json");
    let first = http_json(server.address, "POST", "/v1/instances", Some(fixture), &[]).await;
    let second = http_json(server.address, "POST", "/v1/instances", Some(fixture), &[]).await;
    let first_id = first.body["instance"]["id"].as_str().expect("first id");
    let second_id = second.body["instance"]["id"].as_str().expect("second id");
    let first_journal = first.body["instance"]["journalId"]
        .as_str()
        .expect("first journal");
    let second_journal = second.body["instance"]["journalId"]
        .as_str()
        .expect("second journal");

    let (mut follow, _) = connect_async(format!(
        "ws://{}/v1/instances/{first_id}/follow",
        server.address
    ))
    .await
    .expect("connect multiplex follow");
    let first_snapshot = next_json(&mut follow).await;
    assert_eq!(
        first_snapshot["result"]["snapshot"]["instance"]["id"],
        first_id
    );
    follow
        .send(Message::Text(
            json!({"type": "subscribe", "instanceId": second_id, "fromSeq": null})
                .to_string()
                .into(),
        ))
        .await
        .expect("subscribe second instance");
    let second_snapshot = next_json(&mut follow).await;
    assert_eq!(
        second_snapshot["result"]["snapshot"]["instance"]["id"],
        second_id
    );
    assert_ne!(
        first_snapshot["result"]["subscriptionId"],
        second_snapshot["result"]["subscriptionId"]
    );
    assert_eq!(
        first_snapshot["result"]["connectionId"],
        second_snapshot["result"]["connectionId"]
    );

    for (instance_id, prompt) in [(first_id, "one"), (second_id, "two")] {
        let response = http_json(
            server.address,
            "POST",
            &format!("/v1/instances/{instance_id}/commands"),
            Some(&json!({"operation": "send", "prompt": prompt}).to_string()),
            &[],
        )
        .await;
        assert_eq!(response.status, 200);
    }

    let mut seen = BTreeSet::new();
    while seen.len() < 2 {
        let batch = next_batch(&mut follow).await;
        let journal = batch["params"]["journalId"]
            .as_str()
            .expect("batch journal");
        if journal == first_journal || journal == second_journal {
            seen.insert(journal.to_owned());
        }
    }
    assert_eq!(
        seen,
        BTreeSet::from([first_journal.to_owned(), second_journal.to_owned()])
    );
}

#[tokio::test]
async fn web_jsonrpc_shape_can_create_and_follow_a_live_fixture() {
    let server = spawn_server(DevServerConfig::loopback(0)).await;
    let (mut client, _) = connect_async(format!("ws://{}/v1/client", server.address))
        .await
        .expect("connect JSON-RPC client");
    let hello = rpc(
        &mut client,
        "hello",
        "runtime.hello",
        json!({
            "protocol": {"major": 1, "minMinor": 0, "maxMinor": 0},
            "observationSchemaMajors": [1],
            "features": ["snapshot-follow-v1"]
        }),
    )
    .await;
    assert_eq!(hello["protocol"]["major"], 1);
    let connection_id = hello["connectionId"].clone();

    let hosts = rpc(&mut client, "hosts", "host.list", json!({})).await;
    let host_id = hosts["items"][0]["id"].as_str().expect("host id");
    let workspaces = rpc(
        &mut client,
        "workspaces",
        "workspace.list",
        json!({"hostId": host_id}),
    )
    .await;
    let workspace_id = workspaces["items"][0]["id"].as_str().expect("workspace id");
    let created = rpc(
        &mut client,
        "create",
        "instance.create",
        json!({
            "spec": {
                "hostId": host_id,
                "workspaceId": workspace_id,
                "kind": "claude",
                "driver": "claude-print",
                "model": "fake",
                "providerProfileId": "dev-fake",
                "permissionMode": "dontAsk",
                "prompt": ""
            },
            "initialInput": {"type": "prompt", "text": ""}
        }),
    )
    .await;
    let instance_id = created["instance"]["id"].as_str().expect("instance id");
    let journal_id = created["instance"]["journalId"]
        .as_str()
        .expect("journal id");
    let subscription = rpc(
        &mut client,
        "subscribe",
        "events.subscribe",
        json!({
            "journalId": journal_id,
            "afterSeq": null,
            "snapshot": "required",
            "projectionVersion": "v1",
            "batchLimit": 128
        }),
    )
    .await;
    assert_eq!(subscription["journalId"], journal_id);
    assert_eq!(subscription["connectionId"], connection_id);

    client
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": "send",
                "method": "instance.send",
                "params": {
                    "instanceId": instance_id,
                    "input": {"type": "prompt", "text": "from web"},
                    "completionScope": "native-turn"
                }
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send live prompt");
    let mut saw_response = false;
    let mut saw_batch = false;
    while !saw_response || !saw_batch {
        let frame = next_json(&mut client).await;
        saw_response |= frame["id"] == "send" && frame.get("result").is_some();
        saw_batch |= frame["method"] == "events.batch";
    }
}

#[tokio::test]
async fn tty_endpoint_emits_protocol_v1_binary_fixture() {
    let server = spawn_server(DevServerConfig::loopback(0)).await;
    let created = http_json(
        server.address,
        "POST",
        "/v1/instances",
        Some(include_str!("fixtures/create-instance.json")),
        &[],
    )
    .await;
    let instance_id = created.body["instance"]["id"]
        .as_str()
        .expect("instance id");
    let (mut tty, _) = connect_async(format!(
        "ws://{}/v1/instances/{instance_id}/tty",
        server.address
    ))
    .await
    .expect("connect TTY fixture");
    let frame = tokio::time::timeout(Duration::from_secs(3), tty.next())
        .await
        .expect("TTY timeout")
        .expect("TTY socket open")
        .expect("TTY frame valid");
    let Message::Binary(bytes) = frame else {
        panic!("expected binary TTY frame");
    };
    assert!(bytes.len() > 32);
    assert_eq!(&bytes[..4], &[1, 1, 0, 0]);
    assert_eq!(
        u32::from_be_bytes(bytes[28..32].try_into().expect("payload length")) as usize,
        bytes.len() - 32
    );
}
