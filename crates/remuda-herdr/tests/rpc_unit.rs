//! Fixture tests for NDJSON framing. Source samples: `docs/research/herdr-herdrx.md` §1.2 / §4.3.

use std::path::PathBuf;
use std::time::Duration;

use remuda_herdr::{Client, EventKind, Incoming, Pong, parse_line, parse_terminal_line};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn ping_fixture_round_trips() {
    let incoming = parse_line(fixture("ping-response.json").trim()).unwrap();
    match incoming {
        Incoming::Response { id, result, error } => {
            assert_eq!(id, "r1");
            assert!(error.is_none());
            let pong: Pong = serde_json::from_value(result.unwrap()).unwrap();
            assert_eq!(pong.kind, "pong");
            assert_eq!(pong.version, "0.9.0");
            assert_eq!(pong.protocol, 22);
            assert_eq!(
                pong.capabilities.unwrap().endpoint_protocol_generation,
                Some(1)
            );
        }
        Incoming::Event(_) => panic!("ping is a response"),
    }
}

#[test]
fn error_fixture_is_api_error() {
    let incoming = parse_line(fixture("error-response.json").trim()).unwrap();
    match incoming {
        Incoming::Response { error, .. } => {
            let error = error.expect("error body");
            assert_eq!(error.code, "invalid_request");
            assert!(error.message.contains("pane_id"));
        }
        Incoming::Event(_) => panic!("expected error response"),
    }
}

#[test]
fn event_jsonl_tolerates_dot_and_underscore_names() {
    let mut kinds = Vec::new();
    for line in fixture("events.jsonl").lines() {
        match parse_line(line).unwrap() {
            Incoming::Response { result, .. } => {
                assert_eq!(result.unwrap()["type"], "subscription_started");
            }
            Incoming::Event(event) => kinds.push(event.kind),
        }
    }
    assert_eq!(
        kinds,
        [
            EventKind::PaneAgentStatusChanged,
            EventKind::PaneAgentStatusChanged,
            EventKind::PaneCreated,
            EventKind::PaneExited,
            EventKind::PaneOutputChanged,
        ]
    );
}

#[test]
fn terminal_frame_decodes_base64_ansi() {
    let body = fixture("terminal-frame.jsonl");
    let mut lines = body.lines();
    let frame = parse_terminal_line(lines.next().unwrap()).unwrap();
    assert_eq!(frame.seq, 1);
    assert!(frame.full);
    assert_eq!(&frame.bytes[..], b"hello");
    let closed = parse_terminal_line(lines.next().unwrap()).unwrap_err();
    assert!(closed.to_string().contains("eof"));
}

#[tokio::test]
async fn one_shot_rpc_against_mock_socket() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        let request: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(request["method"], "ping");
        assert!(request["params"].is_object());
        let id = request["id"].as_str().unwrap();
        let reply = format!(
            "{{\"id\":\"{id}\",\"result\":{{\"type\":\"pong\",\"version\":\"0.9.0\",\"protocol\":22}}}}\n"
        );
        writer.write_all(reply.as_bytes()).await.unwrap();
    });

    let client = Client::connect(&sock).with_timeout(Duration::from_secs(2));
    let pong = client.ping().await.unwrap();
    assert_eq!(pong.version, "0.9.0");
    server.await.unwrap();
}

#[tokio::test]
async fn timeout_on_silent_socket() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let server = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });

    let client = Client::connect(&sock).with_timeout(Duration::from_millis(50));
    let err = client.ping().await.unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
    drop(server);
}

#[tokio::test]
async fn disconnect_when_socket_missing() {
    let client = Client::connect("/tmp/remuda-herdr-does-not-exist.sock")
        .with_timeout(Duration::from_millis(200));
    let err = client.ping().await.unwrap_err();
    assert!(err.is_disconnect(), "{err}");
}
