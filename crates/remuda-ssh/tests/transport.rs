//! Length-prefixed stdio + reconnect + local WebSocket hello.

use std::path::PathBuf;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use remuda_ssh::{Backoff, NodeTransport, StdioTransport, WssTransport};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

fn python() -> PathBuf {
    PathBuf::from("python3")
}

fn fake_node() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join("fake-node-stdio.py")
}

#[tokio::test]
async fn stdio_echo_round_trip() {
    let mut t = StdioTransport::connect_local(
        python(),
        vec![fake_node().display().to_string()],
        vec![("REMUDA_FAKE_NODE".into(), "echo".into())],
    )
    .await
    .expect("spawn fake node");
    t.backoff = Backoff {
        initial: Duration::from_millis(5),
        max: Duration::from_millis(20),
    };
    let hello = t.recv_json().await.expect("hello").expect("frame");
    assert_eq!(hello["method"], "node.hello");
    let ping = json!({"id":"ping-1","method":"runtime.heartbeat"});
    t.send_json(&ping).await.expect("send");
    let echoed = t.recv_json().await.expect("echo").expect("frame");
    assert_eq!(echoed, ping);
    t.close().await.expect("close");
}

#[tokio::test]
async fn stdio_reconnects_after_ssh_exit() {
    let mut t = StdioTransport::connect_local(
        python(),
        vec![fake_node().display().to_string()],
        vec![("REMUDA_FAKE_NODE".into(), "hello-exit".into())],
    )
    .await
    .expect("spawn");
    t.backoff = Backoff {
        initial: Duration::from_millis(5),
        max: Duration::from_millis(20),
    };
    let first = t.recv_json().await.expect("first").expect("hello");
    assert_eq!(first["method"], "node.hello");
    let eof = t.recv_json().await.expect("eof");
    assert_eq!(eof, None);
    t.reconnect().await.expect("reconnect");
    let second = t.recv_json().await.expect("second").expect("hello");
    assert_eq!(second["method"], "node.hello");
    t.close().await.expect("close");
}

#[tokio::test]
async fn wss_hello_round_trip() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        let hello = json!({
            "jsonrpc": "2.0",
            "id": "hello-1",
            "method": "node.hello",
            "params": {"nodeVersion": "0.1.0"}
        });
        ws.send(Message::text(hello.to_string())).await.unwrap();
        match ws.next().await {
            Some(Ok(Message::Text(text))) => {
                let value: Value = serde_json::from_str(&text).unwrap();
                ws.send(Message::text(value.to_string())).await.unwrap();
            }
            other => panic!("unexpected {other:?}"),
        }
        let _ = ws.close(None).await;
    });

    let mut t = WssTransport::connect(&format!("ws://{addr}"))
        .await
        .expect("connect");
    let hello = t.recv_json().await.expect("hello").expect("frame");
    assert_eq!(hello["method"], "node.hello");
    let ack = json!({"id":"hello-1","result":{"ok":true}});
    t.send_json(&ack).await.expect("ack");
    let echoed = t.recv_json().await.expect("echo").expect("frame");
    assert_eq!(echoed, ack);
    t.close().await.expect("close");
    server.await.expect("server");
}
