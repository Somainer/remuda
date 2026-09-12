//! Local Hub `/v1/node` stand-in for `enroll_stdio`.

use futures::{SinkExt, StreamExt};
use remuda_ssh::{HubEnroll, NodeTransport, StdioTransport, enroll_stdio, node_socket_url};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::net::TcpListener;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::http::Request;

fn python() -> PathBuf {
    PathBuf::from("python3")
}

fn fake_node() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join("fake-node-stdio.py")
}

#[test]
fn node_socket_url_rewrites_http() {
    assert_eq!(
        node_socket_url("http://127.0.0.1:18080/"),
        "ws://127.0.0.1:18080/v1/node"
    );
}

#[tokio::test]
#[allow(clippy::result_large_err)]
async fn enroll_stdio_forwards_hello_with_bearer() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(stream, |req: &Request<()>, resp| {
            let auth = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            assert_eq!(auth, "Bearer boot");
            Ok(resp)
        })
        .await
        .unwrap();
        let msg = ws.next().await.unwrap().unwrap();
        let Message::Text(text) = msg else {
            panic!("expected text hello");
        };
        let hello: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(hello["method"], "node.hello");
        assert_eq!(hello["id"], "hello-1");
        assert_eq!(hello["params"]["transport"], "ssh-stdio");
        assert_eq!(hello["params"]["label"], "devbox-sg");
        assert_eq!(hello["version"]["major"], 1);
        ws.send(Message::text(
            json!({
                "jsonrpc": "2.0",
                "id": "hello-1",
                "result": { "hostId": "hst_test", "protocol": { "major": 1 } }
            })
            .to_string(),
        ))
        .await
        .unwrap();
        let _ = ws.close(None).await;
    });

    let mut stdio = StdioTransport::connect_local(
        python(),
        vec![fake_node().display().to_string()],
        vec![("REMUDA_FAKE_NODE".into(), "hello-exit".into())],
    )
    .await
    .expect("spawn fake node");
    let spec = HubEnroll {
        hub_ws_url: format!("ws://{addr}/v1/node"),
        bootstrap_token: "boot".into(),
        display_label: "devbox-sg".into(),
    };
    let (result, mut ws) = enroll_stdio(&mut stdio, &spec).await.expect("enroll");
    assert_eq!(result.host_id, "hst_test");
    assert_eq!(result.hello["params"]["transport"], "ssh-stdio");
    let _ = ws.close().await;
    let _ = stdio.close().await;
    server.await.expect("server");
}

#[tokio::test]
#[allow(clippy::result_large_err)]
async fn enroll_stdio_uses_node_auth_token_and_does_not_forward_it() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(stream, |req: &Request<()>, resp| {
            let auth = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            assert_eq!(auth, "Bearer from-node");
            Ok(resp)
        })
        .await
        .unwrap();
        let msg = ws.next().await.unwrap().unwrap();
        let Message::Text(text) = msg else {
            panic!("expected text hello");
        };
        let hello: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(hello["method"], "node.hello");
        ws.send(Message::text(
            json!({
                "jsonrpc": "2.0",
                "id": "hello-1",
                "result": { "hostId": "hst_auth" }
            })
            .to_string(),
        ))
        .await
        .unwrap();
        let _ = ws.close(None).await;
    });

    let mut stdio = StdioTransport::connect_local(
        python(),
        vec![fake_node().display().to_string()],
        vec![("REMUDA_FAKE_NODE".into(), "auth-hello-exit".into())],
    )
    .await
    .expect("spawn fake node");
    let spec = HubEnroll {
        hub_ws_url: format!("ws://{addr}/v1/node"),
        bootstrap_token: "boot".into(),
        display_label: "x".into(),
    };
    let (result, mut ws) = enroll_stdio(&mut stdio, &spec).await.expect("enroll");
    assert_eq!(result.host_id, "hst_auth");
    let _ = ws.close().await;
    let _ = stdio.close().await;
    server.await.expect("server");
}
