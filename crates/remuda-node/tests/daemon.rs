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
use std::path::{Path, PathBuf};
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

    /// One frame, or `None` when the controller closed the pipe.
    ///
    /// `next` panics on a close, which is right when a frame is owed; this is
    /// for the polls that run until a deadline and must treat a closed stream
    /// as a (failing) end rather than a panic.
    async fn next_opt(&mut self) -> Option<Value> {
        let mut line = String::new();
        let count = self.read.read_line(&mut line).await.ok()?;
        if count == 0 {
            return None;
        }
        serde_json::from_str(&line).ok()
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
            codex_home: None,
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
    // Fake model runs must never preflight the developer's personal config.
    std::fs::create_dir_all(path.join("claude-config")).unwrap();
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
            .with_workspace_roots(remuda_testing::test_workspace_roots!()),
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
    first.send(json!({"jsonrpc":"2.0","id":"create","method":"instance.create","params":{"spec":{"kind":"claude","driver":"claude-print","claudeConfigDir":dir.path().join("claude-config"),"model":"fake","prompt":"Reply with the fixture result"}}})).await;
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

/// A data directory longer than `sun_path`: the daemon binds the short
/// runtime socket, the under-data-dir `node.sock` is a symlink, clients reach
/// the daemon through the resolved real path, and shutdown removes both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_socket_redirects_under_a_long_data_dir() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("d".repeat(80)).join("node-data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let preferred = data_dir.join("node.sock");
    assert!(
        preferred.as_os_str().len() > 107,
        "fixture must exceed the Linux sun_path limit: {}",
        preferred.as_os_str().len()
    );

    let node = compose(&ServeConfig::fake(
        DevServerConfig::loopback(0).with_workspace_roots(remuda_testing::test_workspace_roots!()),
        data_dir.clone(),
    ))
    .unwrap();
    let task = start(node.clone(), &data_dir, test_control(&data_dir)).await;

    // The discovery link under the data directory points at a short socket.
    let link_meta = std::fs::symlink_metadata(&preferred).unwrap();
    assert!(
        link_meta.file_type().is_symlink(),
        "node.sock must be a symlink when redirected"
    );
    let real = daemon_socket_path(&data_dir);
    assert_ne!(real, preferred, "clients must resolve to the real socket");
    assert!(
        real.as_os_str().len() <= 107,
        "real socket must fit sun_path: {}",
        real.display()
    );
    assert_eq!(
        std::fs::read_link(&preferred).unwrap(),
        real,
        "node.sock must point at the resolved daemon path"
    );
    // The real socket is private; the status probe already proved it answers.
    assert_eq!(
        std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // A bridge connects through the resolved path as well.
    let peer = Peer::connect(&data_dir, true, json!([])).await;
    drop(peer);

    task.abort();
    let _ = task.await;
    node.shutdown().await.unwrap();
    assert!(
        std::fs::symlink_metadata(&preferred).is_err(),
        "shutdown must remove the discovery symlink"
    );
    assert!(
        std::fs::symlink_metadata(&real).is_err(),
        "shutdown must remove the real socket"
    );
}

/// Before a daemon has ever started under a too-long data directory there is
/// no `node.sock` symlink to resolve; the client path must nevertheless be
/// derived deterministically and fit `sun_path`, and the status probe must
/// report "not running" rather than fail with the kernel's `InvalidInput`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_status_under_a_long_data_dir_before_first_start_is_not_running() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("d".repeat(80)).join("node-data");
    std::fs::create_dir_all(&data_dir).unwrap();
    assert!(data_dir.join("node.sock").as_os_str().len() > 107);
    assert!(
        std::fs::symlink_metadata(data_dir.join("node.sock")).is_err(),
        "no daemon has run; no link may exist yet"
    );

    let resolved = daemon_socket_path(&data_dir);
    assert!(
        resolved.as_os_str().len() <= 107,
        "derived socket must fit sun_path: {}",
        resolved.display()
    );
    assert!(
        !resolved.starts_with(&data_dir),
        "derived socket must live in the per-user runtime dir: {}",
        resolved.display()
    );
    let running = daemon_is_running(&data_dir)
        .await
        .expect("a missing derived socket must read as not-running, not an error");
    assert!(!running);
    // A second bind attempt for the same data dir derives the same name.
    assert_eq!(resolved, daemon_socket_path(&data_dir));
}

/// The installed symlink wins over local derivation: a client whose
/// XDG_RUNTIME_DIR/TMPDIR differ from the daemon's must connect where the
/// running daemon actually is, not where the client would have derived.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_socket_path_honors_the_installed_link_over_local_derivation() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("d".repeat(80)).join("node-data");
    std::fs::create_dir_all(&data_dir).unwrap();
    assert!(data_dir.join("node.sock").as_os_str().len() > 107);

    // Simulate the daemon's environment: serve the status RPC at a short path
    // the current process would not derive, and publish it through node.sock.
    let external = PathBuf::from(format!(
        "/tmp/remuda-linktest-{}/node-foreign.sock",
        std::process::id()
    ));
    std::fs::create_dir_all(external.parent().unwrap()).unwrap();
    let listener = tokio::net::UnixListener::bind(&external).unwrap();
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"status\",\"result\":{\"running\":true}}\n")
            .await
            .unwrap();
    });
    std::os::unix::fs::symlink(&external, data_dir.join("node.sock")).unwrap();

    assert_eq!(daemon_socket_path(&data_dir), external);
    assert!(
        daemon_is_running(&data_dir)
            .await
            .expect("link target is live")
    );
    server.await.unwrap();
    let _ = std::fs::remove_file(data_dir.join("node.sock"));
    let _ = std::fs::remove_file(&external);
    let _ = std::fs::remove_dir_all(external.parent().unwrap());
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
        DevServerConfig::loopback(0).with_workspace_roots(remuda_testing::test_workspace_roots!()),
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
        DevServerConfig::loopback(0).with_workspace_roots(remuda_testing::test_workspace_roots!()),
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
    let claude_config = dir.path().join("claude-config");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut first = tokio_tungstenite::accept_async(stream).await.unwrap();
        let hello: Value =
            serde_json::from_str(first.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(hello["params"]["daemon"], true);
        assert_eq!(hello["params"]["durable"], true);
        first.send(Message::Text(json!({"jsonrpc":"2.0","id":hello["id"],"result":{"instanceWatermarks":[],"nodeToken":"fixture-host-token"}}).to_string().into())).await.unwrap();
        first.send(Message::Text(json!({"jsonrpc":"2.0","id":"create","method":"instance.create","params":{"spec":{"kind":"claude","driver":"claude-print","claudeConfigDir":claude_config,"model":"fake","prompt":"Return the fixture result"}}}).to_string().into())).await.unwrap();
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

/// A long carrier method must not blind the daemon controller.
///
/// The live freeze rode `remuda node bridge`: the controller awaited the whole
/// body on its read arm, so journals stopped forwarding and — the point of the
/// design — `gate.cancel` could not even be *read* while a run was in flight.
/// The escape hatch was unusable exactly when it was needed.
///
/// `gate.then` is a long carrier method that runs an arbitrary `bash -lc`, so a
/// command that sleeps is a long method with a controllable duration and no
/// fixture binary to build. While it runs the controller must still (1) answer
/// an unrelated request and (2) accept the next frame, which is what makes
/// `gate.cancel` readable. Both are checked inside the sleep, before the long
/// method has returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_method_does_not_blind_the_daemon_controller() {
    let dir = tempfile::tempdir().unwrap();
    let node = compose(&ServeConfig::fake(
        DevServerConfig::loopback(0).with_workspace_roots(remuda_testing::test_workspace_roots!()),
        dir.path().to_path_buf(),
    ))
    .unwrap();
    let control = test_control(dir.path());
    let task = start(node.clone(), dir.path(), control).await;
    let mut peer = Peer::connect(dir.path(), false, json!([])).await;

    // A long carrier method, parked in a sleep the test controls by duration.
    let sleep_secs = 6;
    peer.send(json!({
        "jsonrpc": "2.0",
        "id": "then-1",
        "method": "gate.then",
        "params": {
            "jobId": "gjb_bridge",
            "command": format!("sleep {sleep_secs}"),
            "cwd": dir.path().to_string_lossy(),
        },
    }))
    .await;

    // Let it get into the sleep, then prove the loop is still serving. These
    // are read off the same controller, so they can only arrive if the read arm
    // is turning while the long method is in flight.
    tokio::time::sleep(Duration::from_millis(500)).await;
    peer.send(json!({
        "jsonrpc": "2.0",
        "id": "screen-1",
        "method": "host.resources",
        "params": {},
    }))
    .await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw_unrelated = false;
    let mut saw_cancel_read = false;
    while tokio::time::Instant::now() < deadline && !(saw_unrelated && saw_cancel_read) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(Some(frame)) = tokio::time::timeout(remaining, peer.next_opt()).await else {
            break;
        };
        if frame["id"] == json!("screen-1") && frame.get("result").is_some() {
            saw_unrelated = true;
            // The frame that stood in for `gate.cancel` in the live incident:
            // it must be accepted, not merely queued behind the long method.
            peer.send(json!({
                "jsonrpc": "2.0",
                "id": "cancel-1",
                "method": "gate.cancel",
                "params": { "jobId": "gjb_bridge" },
            }))
            .await;
        }
        if frame["id"] == json!("cancel-1") {
            saw_cancel_read = true;
        }
    }
    assert!(
        saw_unrelated,
        "the controller must answer an unrelated request while a long method runs"
    );
    assert!(
        saw_cancel_read,
        "gate.cancel must be readable while a long method runs — that is the escape hatch"
    );

    // And the long method's own reply still lands, with its real result.
    let mut saw_then = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline && !saw_then {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(Some(frame)) = tokio::time::timeout(remaining, peer.next_opt()).await else {
            break;
        };
        if frame["id"] == json!("then-1") {
            assert_eq!(
                frame["result"]["exitCode"],
                json!(0),
                "gate.then result: {frame}"
            );
            saw_then = true;
        }
    }
    assert!(saw_then, "the long method's own reply must still arrive");

    drop(peer);
    task.abort();
    let _ = task.await;
    node.shutdown().await.unwrap();
}
