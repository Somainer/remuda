//! Live list contract against synthetic HTTP/follow events; no native agents.
#![cfg(unix)]

use axum::{
    Json, Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::Uri,
    routing::get,
};
use serde_json::{Value, json};
use std::{
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{broadcast, mpsc},
    time::timeout,
};

#[derive(Clone)]
struct Fixture {
    phase: Arc<AtomicU64>,
    frames: broadcast::Sender<Option<Value>>,
    connected: mpsc::UnboundedSender<()>,
}

async fn follow(
    State(fixture): State<Fixture>,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    assert_eq!(
        uri.query(),
        None,
        "the fleet follow must not send an empty instanceId filter"
    );
    ws.on_upgrade(move |socket| stream(socket, fixture))
}

async fn stream(mut socket: WebSocket, fixture: Fixture) {
    let mut frames = fixture.frames.subscribe();
    fixture.connected.send(()).unwrap();
    loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Ok(Some(value)) => { if socket.send(Message::Text(value.to_string().into())).await.is_err() { break; } }
                _ => { let _ = socket.send(Message::Close(None)).await; break; }
            },
            incoming = socket.recv() => if !matches!(incoming, Some(Ok(_))) { break; }
        }
    }
}

async fn hosts(State(fixture): State<Fixture>) -> Json<Value> {
    Json(
        json!({"items":[{"hostId":"hst_one","label":"first","online":fixture.phase.load(Ordering::SeqCst) == 0},
        {"hostId":"hst_two","label":"second","online":true}]}),
    )
}

async fn instances(State(fixture): State<Fixture>) -> Json<Value> {
    let changed = fixture.phase.load(Ordering::SeqCst) > 0;
    Json(json!({"items":[
        {"instanceId":"ins_one","hostId":"hst_one","name":"one","kind":"codex","lifecycle":"running","activity":if changed { "idle" } else { "busy" },"connectivity":if changed { "disconnected" } else { "connected" },"cwd":"/work/one","durableSeq":if changed { "30" } else { "10" }},
        {"instanceId":"ins_two","hostId":"hst_two","name":"two","kind":"claude","lifecycle":"running","activity":"idle","connectivity":"connected","cwd":"/work/two","durableSeq":"1"}
    ]}))
}

async fn journal(
    State(fixture): State<Fixture>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Json<Value> {
    let changed = fixture.phase.load(Ordering::SeqCst) > 0;
    Json(
        json!({"durableSeq":if changed { "30" } else { "10" },"events":[{"text":if id == "ins_two" { "second host line" } else if changed { "after gap" } else { "initial line" }}]}),
    )
}

async fn until(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    timeout(Duration::from_secs(15), async {
        loop {
            let line = lines.next_line().await.unwrap().expect("watch output");
            let value: Value =
                serde_json::from_str(&line).expect("one compact JSON snapshot per line");
            if predicate(&value) {
                return value;
            }
        }
    })
    .await
    .expect("watch snapshot deadline")
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_follows_all_hosts_resyncs_gaps_reconnects_and_stops_on_sigint() {
    let (frames, _) = broadcast::channel(16);
    let (connected, mut connections) = mpsc::unbounded_channel();
    let fixture = Fixture {
        phase: Arc::new(AtomicU64::new(0)),
        frames,
        connected,
    };
    let router = Router::new()
        .route("/v1/hosts", get(hosts))
        .route("/v1/instances", get(instances))
        .route("/v1/instances/{id}/journal", get(journal))
        .route("/v1/follow", get(follow))
        .with_state(fixture.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args([
            "instance", "ls", "--watch", "--json", "--hub", &url, "--token", "fixture",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    timeout(Duration::from_secs(10), connections.recv())
        .await
        .unwrap()
        .unwrap();
    let first = until(&mut lines, |value| {
        value["stream"] == "connected" && value["items"][0]["lastLine"] == "initial line"
    })
    .await;
    assert_eq!(first["items"].as_array().unwrap().len(), 2);
    assert_eq!(first["items"][1]["lastLine"], "second host line");
    assert_eq!(first["items"][0]["worktree"], "/work/one");
    fixture.frames.send(Some(json!({"type":"event","instanceId":"ins_one","seq":"20","event":{"text":"live update"}}))).unwrap();
    until(&mut lines, |value| {
        value["items"][0]["lastLine"] == "live update"
    })
    .await;
    fixture.phase.store(1, Ordering::SeqCst);
    fixture
        .frames
        .send(Some(json!({"type":"gap","reason":"backpressure"})))
        .unwrap();
    let resynced = until(&mut lines, |value| {
        value["items"][0]["lastLine"] == "after gap"
    })
    .await;
    assert_eq!(resynced["items"][0]["activity"], "idle");
    assert_eq!(resynced["items"][0]["connectivity"], "disconnected");
    assert_eq!(resynced["items"][0]["hostOnline"], false);
    fixture.frames.send(None).unwrap();
    let stale = until(&mut lines, |value| value["stream"] == "disconnected").await;
    assert_eq!(stale["stale"], true);
    timeout(Duration::from_secs(10), connections.recv())
        .await
        .unwrap()
        .unwrap();
    until(&mut lines, |value| {
        value["stream"] == "connected" && value["items"][0]["lastLine"] == "after gap"
    })
    .await;
    let id = child.id().unwrap().to_string();
    assert!(
        Command::new("kill")
            .args(["-INT", &id])
            .status()
            .await
            .unwrap()
            .success()
    );
    assert!(
        timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args([
            "agents", "--json", "--host", "hst_two", "--hub", &url, "--token", "fixture",
        ])
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let snapshot: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(snapshot["items"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["items"][0]["hostId"], "hst_two");
    server.abort();
}
