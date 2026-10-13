//! ma-initiator r14 item 3 (R3) — carrier session ids must NEVER repeat,
//! including across a real Hub restart. The Hub persists a monotonic boot epoch
//! in its SQLite DB (`hub_kv.gate_session_boot_epoch`) and packs it into every
//! minted session identity: `(boot_epoch << 32) | per-boot counter`. A
//! pre-restart job left Running is reconciled back to Queued at the next boot
//! and re-dispatched; that fresh gate.run is stamped with boot 2's session,
//! which can never equal the identity boot 1 handed out.

#![cfg(unix)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TIMEOUT: Duration = Duration::from_secs(20);

/// Minimal raw authenticated Node socket; gate.run arrivals are recorded and
/// left UNANSWERED, so the dispatched job stays Running (holds the lane).
struct RawNode {
    pumps: Vec<tokio::task::JoinHandle<()>>,
    arrivals: Arc<Mutex<Vec<Value>>>,
    node_token: String,
}

impl RawNode {
    fn shutdown(&mut self) {
        for pump in &self.pumps {
            pump.abort();
        }
    }

    async fn wait_run_for(&self, job_id: &str) {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let seen = self
                .arrivals
                .lock()
                .unwrap()
                .iter()
                .any(|p| p["jobId"].as_str() == Some(job_id));
            if seen {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no gate.run for {job_id}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn node_token(&self) -> &str {
        &self.node_token
    }
}

async fn connect_raw(
    addr: std::net::SocketAddr,
    host: &str,
    workspace: &str,
    auth_token: &str,
    revision: u64,
) -> RawNode {
    let mut request = format!("ws://{addr}/v1/node")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {auth_token}").parse().unwrap(),
    );
    let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let (mut write, mut read) = socket.split();

    let (out, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let arrivals = Arc::new(Mutex::new(Vec::new()));

    // Latched before the hello is sent, so its reply cannot be missed.
    let (hello_tx, hello_rx) = oneshot::channel();
    pending.lock().unwrap().insert("hello".to_owned(), hello_tx);

    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if write
                .send(Message::Text(frame.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let reader_pending = pending.clone();
    let reader_arrivals = arrivals.clone();
    let reader = tokio::spawn(async move {
        while let Some(Ok(msg)) = read.next().await {
            let Message::Text(text) = msg else { continue };
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if frame.get("method").and_then(Value::as_str) == Some("gate.run") {
                reader_arrivals
                    .lock()
                    .unwrap()
                    .push(frame["params"].clone());
                continue;
            }
            if let Some(id) = frame.get("id").and_then(Value::as_str)
                && let Some(waiter) = reader_pending.lock().unwrap().remove(id)
            {
                let _ = waiter.send(frame);
            }
        }
    });

    out.send(json!({
        "jsonrpc": "2.0", "id": "hello", "method": "runtime.hello",
        "params": {"hostId": host, "nodeVersion": "0.1.0-test",
            "label": format!("lane-{host}"),
            "host": {"hostname": format!("{host}.local"), "maxInstances": 8,
                "labels": {}, "resources": {"cpuCount": 8},
                "workspaces": [
                    {"workspaceId": workspace, "hostId": host,
                     "root": format!("/tmp/{workspace}")}
                ],
                "workspaceRevision": revision}}
    }))
    .unwrap();

    let hello = tokio::time::timeout(TIMEOUT, hello_rx)
        .await
        .expect("hello reply timed out")
        .expect("hello waiter dropped");
    let result = hello.get("result").cloned().unwrap_or(json!({}));
    let node_token = result["nodeToken"]
        .as_str()
        .unwrap_or(auth_token)
        .to_owned();

    RawNode {
        pumps: vec![writer, reader],
        arrivals,
        node_token,
    }
}

async fn free_port() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr
}

/// Read-only SQLite access straight at the Hub's persisted state.
fn read_only_conn(dir: &std::path::Path) -> rusqlite::Connection {
    use rusqlite::OpenFlags;
    rusqlite::Connection::open_with_flags(
        dir.join("hub.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap()
}

fn kv_i64(conn: &rusqlite::Connection, key: &str) -> i64 {
    conn.query_row("SELECT value FROM hub_kv WHERE key = ?1", [key], |row| {
        row.get(0)
    })
    .unwrap()
}

fn dispatch_session(conn: &rusqlite::Connection, job_id: &str) -> i64 {
    conn.query_row(
        "SELECT dispatch_session FROM gate_jobs WHERE id = ?1 AND dispatch_session IS NOT NULL",
        [job_id],
        |row| row.get(0),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_identities_never_repeat_across_a_real_hub_restart() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("hub");
    let addr = free_port().await;

    // ── Boot 1: enroll, project, enqueue, and drive a REAL dispatch ────────
    let mut config = HubConfig::for_test(data_dir.clone());
    config.listen = addr;
    let hub1 = spawn(config).await.expect("hub1");
    let dev_token = hub1.mint_device_token("boot-epoch-human").await.unwrap();
    let enroll = hub1
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await
        .unwrap();
    let host = remuda_protocol::HostId::new().as_id().to_string();
    let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();

    let mut a = connect_raw(addr, &host, &workspace, &enroll, 1).await;

    let created = reqwest::Client::new()
        .post(format!("http://{addr}/v1/projects"))
        .bearer_auth(&dev_token)
        .json(&json!({
            "name": "session-boot-epoch",
            "members": [{"hostId": host, "workspaceId": workspace, "role": "build"}],
            "hosts": [{"hostId": host, "maxInstances": 8}],
            "gate": {
                "affected": true, "web": "never", "landSerialization": "global-cas",
                "lanes": [{
                    "id": "lane1", "hostId": host,
                    "repoPath": "/tmp/session-boot-epoch/repo",
                    "targetDir": "/tmp/session-boot-epoch/target",
                    "ports": "58710-58719", "env": {},
                    "lockPath": "/tmp/session-boot-epoch.lock",
                    "pwEndpoint": "ws://127.0.0.1:0/"
                }]
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 200, "{:?}", created.text().await);
    let project: Value = created.json().await.unwrap();
    let project_id = project["id"].as_str().unwrap();

    let enqueued = reqwest::Client::new()
        .post(format!("http://{addr}/v1/projects/{project_id}/gate"))
        .bearer_auth(&dev_token)
        .json(&json!({"branch": "wt/session/boot", "mode": "land"}))
        .send()
        .await
        .unwrap();
    assert_eq!(enqueued.status(), 200, "{:?}", enqueued.text().await);
    let job: Value = enqueued.json().await.unwrap();
    let job_id = job["id"].as_str().unwrap().to_owned();

    // The production scheduler dispatched on boot 1's real carrier session.
    a.wait_run_for(&job_id).await;
    let session_boot1 = {
        let conn = read_only_conn(&data_dir);
        assert_eq!(kv_i64(&conn, "gate_session_boot_epoch"), 1);
        let session = dispatch_session(&conn, &job_id);
        assert_eq!(session >> 32, 1, "boot-1 dispatch carries boot epoch 1");
        session
    };

    // ── Real shutdown, then a SECOND Hub on the SAME data dir and port ──────
    a.shutdown();
    hub1.shutdown().await;

    let mut config2 = HubConfig::for_test(data_dir.clone());
    config2.listen = addr;
    let hub2 = spawn(config2).await.expect("hub2");
    // Same enrolled host, persistent node token, fresh runtime revision.
    let mut b = connect_raw(addr, &host, &workspace, a.node_token(), 2).await;

    // Startup reconcile re-queued the Running job; boot 2 re-dispatches it on
    // the new carrier session.
    b.wait_run_for(&job_id).await;
    let session_boot2 = {
        let conn = read_only_conn(&data_dir);
        assert_eq!(
            kv_i64(&conn, "gate_session_boot_epoch"),
            2,
            "the boot epoch is persisted and advanced across the restart"
        );
        let session = dispatch_session(&conn, &job_id);
        assert_eq!(session >> 32, 2, "boot-2 dispatch carries boot epoch 2");
        session
    };

    // R3: an identity minted after the restart can never equal one the prior Hub
    // process handed out, even for the same host on the same socket name.
    assert_ne!(
        session_boot1, session_boot2,
        "carrier session identity reused across a Hub restart"
    );
    assert!(session_boot2 > session_boot1);

    b.shutdown();
    hub2.shutdown().await;
}

/// Open the hub DB read-write directly (only while no Hub is running).
fn rw_conn(dir: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(dir.join("hub.sqlite")).unwrap()
}

/// r15 item 6 (R5): a missing or regressed `gate_session_boot_epoch` row in a
/// database that still RETAINES gate carrier sessions makes the Hub REFUSE to
/// start — it must not silently reset the epoch to 1 (which would mint ids the
/// retained sessions already used).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_or_regressed_epoch_with_retained_sessions_refuses_boot() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("hub");
    let addr = free_port().await;
    let mut config = HubConfig::for_test(data_dir.clone());
    config.listen = addr;
    let hub = spawn(config.clone()).await.expect("first hub boots");
    let dev_token = hub.mint_device_token("h").await.unwrap();
    drop(dev_token);
    hub.shutdown().await;

    // First-boot DB: epoch 1 persisted, no gate jobs yet.
    {
        let conn = read_only_conn(&data_dir);
        assert_eq!(kv_i64(&conn, "gate_session_boot_epoch"), 1);
    }

    // Simulate a partially restored/cloned data dir: a retained Landing
    // carrier session from epoch 5, but the epoch allocation row is MISSING.
    {
        let mut conn = rw_conn(&data_dir);
        let tx = conn.transaction().unwrap();
        tx.execute(
            "INSERT INTO gate_jobs
                (id, project_id, state, doc_json, queued_at, created_at,
                 updated_at, revision, landing_at, dispatch_session)
             VALUES ('gjb_retained', 'prj_ghost', 'landing', '{}',
                     '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z',
                     '2026-01-01T00:00:00Z', 1,
                     '2026-01-01T00:00:00Z', ?1)",
            [5i64 << 32 | 7],
        )
        .unwrap();
        tx.execute(
            "DELETE FROM hub_kv WHERE key = 'gate_session_boot_epoch'",
            [],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    // The Hub must fail to start.
    let missing = spawn(HubConfig::for_test(data_dir.clone())).await;
    let error = match missing {
        Ok(hub) => panic!(
            "a missing epoch with retained sessions must refuse boot: boot={}",
            hub.boot_epoch_value()
        ),
        Err(err) => err.to_string(),
    };
    assert!(
        error.contains("boot epoch"),
        "the failure must surface the boot-epoch problem: {error}"
    );

    // Regressed row (smaller than the retained epoch): same refusal.
    {
        let conn = rw_conn(&data_dir);
        conn.execute(
            "INSERT INTO hub_kv(key, value) VALUES ('gate_session_boot_epoch', 2)
             ON CONFLICT(key) DO UPDATE SET value = 2",
            [],
        )
        .unwrap();
    }
    let regressed = spawn(HubConfig::for_test(data_dir.clone())).await;
    let error = match regressed {
        Ok(hub) => panic!(
            "a regressed epoch must refuse boot: boot={}",
            hub.boot_epoch_value()
        ),
        Err(err) => err.to_string(),
    };
    assert!(
        error.contains("regressed") || error.contains("older than a retained"),
        "the failure must name the regression: {error}"
    );

    // A healthy epoch row (>= the retained epoch) boots normally and allocates
    // the next monotonic value.
    {
        let conn = rw_conn(&data_dir);
        conn.execute(
            "UPDATE hub_kv SET value = 5 WHERE key = 'gate_session_boot_epoch'",
            [],
        )
        .unwrap();
    }
    let healthy = spawn(HubConfig::for_test(data_dir.clone()))
        .await
        .expect("an epoch at least as new as the retained data boots");
    assert_eq!(
        healthy.boot_epoch_value(),
        6,
        "the next monotonic epoch is 6"
    );
    healthy.shutdown().await;
}

/// r16 item 9: a boot epoch at or above 2^32 would shift out of the high 32
/// bits of a packed session id and ALIAS lower-epoch sessions. Both an epoch
/// row already at the limit and one whose NEXT allocation would reach it must
/// refuse to boot rather than wrap. The row is left untouched by the refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_epoch_at_or_above_the_32bit_field_refuses_to_boot() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("hub");
    let hub = spawn(HubConfig::for_test(data_dir.clone()))
        .await
        .expect("first hub boots");
    hub.shutdown().await;

    for bad in [1u64 << 32, (1u64 << 32) - 1] {
        {
            let conn = rw_conn(&data_dir);
            conn.execute(
                "INSERT INTO hub_kv(key, value) VALUES ('gate_session_boot_epoch', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = ?1",
                [i64::try_from(bad).unwrap()],
            )
            .unwrap();
        }
        let refused = spawn(HubConfig::for_test(data_dir.clone())).await;
        match refused {
            Ok(hub) => panic!(
                "epoch row {bad} must refuse boot instead of aliasing session ids: boot={}",
                hub.boot_epoch_value()
            ),
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.contains("2^32") || message.contains("session-id"),
                    "the refusal must name the 2^32 session-id limit: {message}"
                );
            }
        }
        // The refused allocation did not advance the row.
        let conn = read_only_conn(&data_dir);
        assert_eq!(
            kv_i64(&conn, "gate_session_boot_epoch"),
            i64::try_from(bad).unwrap(),
            "a refused boot must not rewrite the epoch row"
        );
    }

    // One step below the last-allowed row boots and allocates exactly 2^32 - 1.
    {
        let conn = rw_conn(&data_dir);
        conn.execute(
            "UPDATE hub_kv SET value = ?1 WHERE key = 'gate_session_boot_epoch'",
            [(1u64 << 32) - 2],
        )
        .unwrap();
    }
    let last_ok = spawn(HubConfig::for_test(data_dir.clone()))
        .await
        .expect("the last under-limit epoch still boots");
    assert_eq!(
        last_ok.boot_epoch_value(),
        (1u64 << 32) - 1,
        "the final valid epoch is allocated"
    );
    last_ok.shutdown().await;
}
