//! Persistent Node transport lifetime and durable resume regression tests.
#![cfg(unix)]

use remuda_node::{
    DaemonControl, DevNode, DevServerConfig, LocalDrivers, NativeDriverConfig, ServeConfig,
    StdioOptions, bind_daemon, compose, connect_daemon_bridge, daemon_is_running,
    daemon_socket_path, run_daemon_runtime_listener,
};
use remuda_protocol::{InstanceId, U64};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

struct Peer {
    read: BufReader<OwnedReadHalf>,
    write: OwnedWriteHalf,
}

impl Peer {
    async fn connect(path: &Path, takeover: bool, watermarks: Value) -> Self {
        let stream = connect_daemon_bridge(path, takeover).await.unwrap();
        let (read, write) = stream.into_split();
        let mut peer = Self {
            read: BufReader::new(read),
            write,
        };
        let hello = peer.next().await;
        assert_eq!(hello["method"], "node.hello");
        assert_eq!(hello["params"]["bridge"], true);
        assert_eq!(hello["params"]["daemon"], true);
        peer.send(
            json!({"jsonrpc":"2.0","id":"hello-1","result":{"instanceWatermarks":watermarks}}),
        )
        .await;
        peer
    }

    async fn send(&mut self, frame: Value) {
        self.write
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn next(&mut self) -> Value {
        let mut line = String::new();
        let count = tokio::time::timeout(Duration::from_secs(10), self.read.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert!(count > 0, "peer disconnected");
        serde_json::from_str(&line).unwrap()
    }

    async fn ack(&mut self, frame: &Value) -> u64 {
        let seq: u64 = frame["params"]["seq"].as_str().unwrap().parse().unwrap();
        self.send(
            json!({"jsonrpc":"2.0","id":frame["id"],"result":{"durableSeq":seq.to_string()}}),
        )
        .await;
        seq
    }
}

fn test_control(path: &Path) -> DaemonControl {
    let snapshot = remuda_node::Collector::new(
        remuda_node::ProbeEnv {
            path: "/usr/bin:/bin".into(),
            home: path.to_path_buf(),
            hostname: Some("fixture-node".into()),
            herdr_socket_env: None,
            xdg_config_home: None,
        },
        Duration::from_secs(60),
    )
    .snapshot(&remuda_node::CollectRequest::default());
    DaemonControl::with_inventory(Some(snapshot)).unwrap()
}

async fn start(
    node: DevNode,
    data_dir: &Path,
    control: DaemonControl,
) -> tokio::task::JoinHandle<()> {
    let listener = bind_daemon(data_dir).await.unwrap();
    let opts = StdioOptions {
        data_dir: data_dir.to_path_buf(),
        ..StdioOptions::default()
    };
    let task = tokio::spawn(async move {
        run_daemon_runtime_listener(node, opts, control, &listener)
            .await
            .unwrap();
    });
    assert!(daemon_is_running(data_dir).await.unwrap());
    task
}

fn native_fixture(path: &Path) -> (ServeConfig, std::path::PathBuf, std::path::PathBuf) {
    let wrapper = path.join("fake-claude-gated");
    std::fs::write(&wrapper, include_bytes!("fixtures/daemon-fake-claude.sh")).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let gate = path.join("in-flight");
    let release = path.join("release");
    let mut native = NativeDriverConfig::new(path.to_path_buf()).with_claude_binary(wrapper);
    native.extra_env.extend([
        (
            "FAKE_CLAUDE_TEST_BINARY".into(),
            remuda_testing::ensure_workspace_bin("fake-claude")
                .display()
                .to_string(),
        ),
        ("FAKE_CLAUDE_TEST_GATE".into(), gate.display().to_string()),
        (
            "FAKE_CLAUDE_TEST_RELEASE".into(),
            release.display().to_string(),
        ),
        ("FAKE_CLAUDE_SCRIPT".into(), "ok".into()),
    ]);
    let config = ServeConfig {
        http: DevServerConfig::loopback(0)
            .with_workspace_root(path.to_path_buf())
            .with_workspace_roots(vec![std::env::temp_dir()]),
        data_dir: path.to_path_buf(),
        drivers: LocalDrivers::Native(native),
    };
    (config, gate, release)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killed_bridge_keeps_fake_claude_alive_and_replays_completion_after_watermark() {
    let dir = tempfile::tempdir().unwrap();
    let (config, gate, release) = native_fixture(dir.path());
    let node = compose(&config).unwrap();
    let task = start(node.clone(), dir.path(), test_control(dir.path())).await;
    assert_eq!(
        std::fs::metadata(daemon_socket_path(dir.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("node.pid"))
            .unwrap()
            .trim(),
        std::process::id().to_string()
    );
    assert!(
        bind_daemon(dir.path()).await.is_err(),
        "second daemon must be rejected before native reconciliation"
    );
    let mut first = Peer::connect(dir.path(), false, json!([])).await;
    first.send(json!({"jsonrpc":"2.0","id":"create","method":"instance.create","params":{"spec":{"kind":"claude","driver":"claude-print","model":"fake","prompt":"Reply with the fixture result"}}})).await;
    let created = loop {
        let frame = first.next().await;
        if frame["id"] == "create" {
            break frame;
        }
        if frame["method"] == "journal.append" {
            first.ack(&frame).await;
        }
    };
    assert!(created.get("error").is_none(), "{created}");
    let instance = node.list_instances().unwrap().items.remove(0);
    let mut watermark = 0_u64;
    while watermark == 0 {
        let frame = first.next().await;
        if frame["method"] == "journal.append" {
            watermark = first.ack(&frame).await;
        }
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while !gate.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fake-claude turn is in flight");
    assert!(!journal_complete(&node, &instance.meta.id));
    drop(first);
    assert!(daemon_is_running(dir.path()).await.unwrap());
    std::fs::write(&release, "continue").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !journal_complete(&node, &instance.meta.id) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("instance must finish without any controller");
    let mut second = Peer::connect(
        dir.path(),
        true,
        json!([{"instanceId":instance.meta.id,"durableSeq":watermark.to_string()}]),
    )
    .await;
    let mut replay = Vec::new();
    loop {
        let frame = second.next().await;
        if frame["method"] != "journal.append" {
            continue;
        }
        let seq = second.ack(&frame).await;
        assert!(seq > watermark, "resume must exclude Hub-acked prefix");
        replay.push(frame);
        if replay
            .iter()
            .any(|frame| frame.to_string().contains("\"text\":\"OK\""))
        {
            break;
        }
    }
    assert!(!replay.is_empty());
    drop(second);
    task.abort();
    let _ = task.await;
    node.shutdown().await.unwrap();
    assert!(!daemon_socket_path(dir.path()).exists());
    assert!(!dir.path().join("node.pid").exists());
    // Open SQLite again to prove completion was durable, not only broadcast.
    let restored = compose(&config).unwrap();
    assert!(journal_complete(&restored, &instance.meta.id));
}

fn journal_complete(node: &DevNode, instance: &InstanceId) -> bool {
    let journal = node.get_instance(instance).unwrap().journal_id;
    serde_json::to_string(&node.read_journal(&journal, Some(U64(0)), 256).unwrap())
        .unwrap()
        .contains("\"text\":\"OK\"")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn controller_takeover_fences_old_bridge_and_outbound_waits_until_detach() {
    let dir = tempfile::tempdir().unwrap();
    let node = compose(&ServeConfig::fake(
        DevServerConfig::loopback(0),
        dir.path().to_path_buf(),
    ))
    .unwrap();
    let control = test_control(dir.path());
    let task = start(node.clone(), dir.path(), control.clone()).await;
    let mut first = Peer::connect(dir.path(), false, json!([])).await;
    assert!(connect_daemon_bridge(dir.path(), false).await.is_err());
    let second = Peer::connect(dir.path(), true, json!([])).await;
    let mut line = String::new();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), first.read.read_line(&mut line))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), control.acquire_outbound())
            .await
            .is_err()
    );
    drop(second);
    let outbound = tokio::time::timeout(Duration::from_secs(2), control.acquire_outbound())
        .await
        .unwrap()
        .unwrap();
    assert!(connect_daemon_bridge(dir.path(), false).await.is_err());
    let bridge = Peer::connect(dir.path(), true, json!([])).await;
    tokio::time::timeout(Duration::from_secs(2), outbound.revoked())
        .await
        .unwrap();
    drop(outbound);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), control.acquire_outbound())
            .await
            .is_err(),
        "dropping revoked WSS must not release bridge"
    );
    drop(bridge);
    task.abort();
    let _ = task.await;
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_reply_pipe_does_not_block_replacement_controller() {
    let dir = tempfile::tempdir().unwrap();
    let node = compose(&ServeConfig::fake(
        DevServerConfig::loopback(0),
        dir.path().to_path_buf(),
    ))
    .unwrap();
    let task = start(node.clone(), dir.path(), test_control(dir.path())).await;
    let mut old = Peer::connect(dir.path(), false, json!([])).await;
    // A large echoed JSON-RPC id fills the old controller's unread socket.
    old.send(
        json!({"jsonrpc":"2.0","id":"x".repeat(900_000),"method":"node.heartbeat","params":{}}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut new = Peer::connect(dir.path(), true, json!([])).await;
    new.send(json!({"jsonrpc":"2.0","id":"new","method":"node.heartbeat","params":{}}))
        .await;
    let reply = tokio::time::timeout(Duration::from_secs(2), new.next())
        .await
        .expect("stalled old reply must not hold dispatch fence");
    assert_eq!(reply["id"], "new");
    assert_eq!(reply["result"]["ok"], true);
    drop(old);
    drop(new);
    task.abort();
    let _ = task.await;
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outbound_reconnect_replays_offline_completion_without_a_new_command() {
    use futures::{SinkExt, StreamExt};
    use remuda_node::{Backoff, WssConfig, WssLink};
    use tokio_tungstenite::tungstenite::Message;

    let dir = tempfile::tempdir().unwrap();
    let (config, gate, release) = native_fixture(dir.path());
    let node = compose(&config).unwrap();
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let (disconnected, offline) = tokio::sync::oneshot::channel();
    let (resume, resumed) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut first = tokio_tungstenite::accept_async(stream).await.unwrap();
        let hello: Value =
            serde_json::from_str(first.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(hello["params"]["daemon"], true);
        assert_eq!(hello["params"]["durable"], true);
        first.send(Message::Text(json!({"jsonrpc":"2.0","id":hello["id"],"result":{"instanceWatermarks":[],"nodeToken":"fixture-host-token"}}).to_string().into())).await.unwrap();
        first.send(Message::Text(json!({"jsonrpc":"2.0","id":"create","method":"instance.create","params":{"spec":{"kind":"claude","driver":"claude-print","model":"fake","prompt":"Return the fixture result"}}}).to_string().into())).await.unwrap();
        let mut watermark = 0_u64;
        let mut instance_id = String::new();
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        loop {
            tokio::select! {
                _ = tick.tick(), if watermark > 0 => {
                    if gate.exists() {break;}
                }
                message = first.next() => {
                    let message = message.unwrap().unwrap();
                    let Ok(text) = message.to_text() else {continue;};
                    let frame: Value = serde_json::from_str(text).unwrap();
                    if frame["method"] == "journal.append" {
                        watermark = watermark.max(frame["params"]["seq"].as_str().unwrap().parse::<u64>().unwrap());
                        instance_id = frame["params"]["instanceId"].as_str().unwrap().to_owned();
                        first.send(Message::Text(json!({"jsonrpc":"2.0","id":frame["id"],"result":{"durableSeq":watermark.to_string()}}).to_string().into())).await.unwrap();
                    }
                }
            }
        }
        drop(first);
        disconnected.send(()).unwrap();
        resumed.await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let mut second = tokio_tungstenite::accept_async(stream).await.unwrap();
        let hello: Value =
            serde_json::from_str(second.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(hello["params"]["instances"].as_array().unwrap().len(), 1);
        second.send(Message::Text(json!({"jsonrpc":"2.0","id":hello["id"],"result":{"instanceWatermarks":[{"instanceId":instance_id,"durableSeq":watermark.to_string()}]}}).to_string().into())).await.unwrap();
        let mut seen = std::collections::BTreeSet::new();
        loop {
            let message = second.next().await.unwrap().unwrap();
            let Ok(text) = message.to_text() else {
                continue;
            };
            let frame: Value = serde_json::from_str(text).unwrap();
            if frame["method"] != "journal.append" {
                continue;
            }
            let seq = frame["params"]["seq"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap();
            assert!(seq > watermark, "WSS must honor the Hub's acked prefix");
            seen.insert(seq);
            second.send(Message::Text(json!({"jsonrpc":"2.0","id":frame["id"],"result":{"durableSeq":seq.to_string()}}).to_string().into())).await.unwrap();
            if text.contains("\"text\":\"OK\"") {
                break;
            }
        }
        assert!(!seen.is_empty());
    });
    let mut config = WssConfig::loopback(
        address,
        "fixture-token",
        node.host().meta.id.as_id().to_string(),
    );
    config.backoff = Backoff {
        initial: Duration::from_millis(20),
        max: Duration::from_millis(50),
        jitter_ppt: 0,
    };
    let control = test_control(dir.path());
    let lease = control.acquire_outbound().await.unwrap();
    let link = WssLink::connect_runtime_controlled_persisting(
        config,
        node.clone(),
        lease.clone(),
        dir.path().to_path_buf(),
    )
    .await
    .unwrap();
    let token_path = dir.path().join("node/host-token");
    assert_eq!(
        std::fs::read_to_string(&token_path).unwrap().trim(),
        "fixture-host-token"
    );
    assert_eq!(
        std::fs::metadata(token_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        remuda_node::load_or_create_enrollment(dir.path())
            .unwrap()
            .node_token
            .as_deref(),
        Some("fixture-host-token")
    );
    tokio::time::timeout(Duration::from_secs(10), offline)
        .await
        .unwrap()
        .unwrap();
    let instance = node.list_instances().unwrap().items.remove(0);
    assert!(!journal_complete(&node, &instance.meta.id));
    std::fs::write(release, "continue").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !journal_complete(&node, &instance.meta.id) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("native instance completes while WSS is offline");
    resume.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    link.shutdown().await;
    drop(lease);
    node.shutdown().await.unwrap();
}
