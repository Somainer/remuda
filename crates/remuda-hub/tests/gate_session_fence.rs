//! r13 item 3: non-reused, current-session fencing of the land grant over REAL
//! WebSocket carriers into a real in-process Hub (real SQLite, production
//! Running → Landing grant writer). The Node side is a minimal raw socket that
//! parks the Hub's `gate.run` (never replies) so the row stays exactly where the
//! test needs it; every AUTHORSHIP decision — Running → Landing, idempotent
//! re-grant, superseded-session refusal — is made by production Hub code, never
//! supplied by the test.
//!
//! Scenarios:
//!   * takeover while A is still open: A is granted, B connects and supersedes
//!     it; the still-open A is revoked (both the Running-fence authorize and
//!     the already-Landing grant refuse it `session-superseded`), and B cannot
//!     re-grant A's landing either;
//!   * full disconnect before B connects: A drops mid-Running, the job is
//!     requeued (not stranded), B draws a fresh NON-REUSED session, is
//!     redispatched and can take the grant A never took.

#![cfg(unix)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TIMEOUT: Duration = Duration::from_secs(20);
const MERGE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// A raw authenticated Node socket. `gate.run` frames from the Hub are recorded
/// and left UNANSWERED (the dispatch parks), while Node→Hub requests the test
/// sends are correlated and answered normally.
struct RawNode {
    out: mpsc::UnboundedSender<Value>,
    pumps: Vec<tokio::task::JoinHandle<()>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    arrivals: Arc<Mutex<Vec<Value>>>,
    node_token: String,
}

impl RawNode {
    /// Abruptly sever the carrier by aborting BOTH pump tasks (dropping both
    /// WebSocket halves sends FIN/RST), so the Hub observes a real socket end
    /// and tears the session down. A graceful Close frame is unreliable here:
    /// the Hub read loop ignores non-Text frames and keeps the link open.
    fn shutdown(&mut self) {
        for pump in &self.pumps {
            pump.abort();
        }
    }
    async fn request(&self, frame: Value) -> Value {
        let id = frame["id"].as_str().unwrap().to_owned();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        self.out.send(frame).unwrap();
        let reply = tokio::time::timeout(TIMEOUT, rx)
            .await
            .expect("node request reply timed out")
            .unwrap_or(json!({}));
        reply.get("result").cloned().unwrap_or(json!({}))
    }

    fn run_count(&self) -> usize {
        self.arrivals.lock().unwrap().len()
    }

    fn last_run_generation(&self) -> u64 {
        self.arrivals
            .lock()
            .unwrap()
            .last()
            .and_then(|p| p["generation"].as_u64())
            .unwrap_or(0)
    }

    async fn wait_runs(&self, want: usize) {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        while self.run_count() < want {
            assert!(
                tokio::time::Instant::now() < deadline,
                "never got {want} gate.run"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn node_token(&self) -> &str {
        &self.node_token
    }
}

/// Open an authenticated raw node socket for `host` and complete one
/// `runtime.hello`. The first connection uses the single-use enroll token;
/// reconnects reuse the returned node token (passed back in as `auth_token`).
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

    // Register the hello waiter BEFORE sending (latched), so the reply cannot
    // arrive uncorrelated.
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
            if let Some(method) = frame.get("method").and_then(Value::as_str) {
                if method == "gate.run" {
                    reader_arrivals
                        .lock()
                        .unwrap()
                        .push(frame.get("params").cloned().unwrap_or(json!({})));
                }
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
    assert!(
        result["nodeToken"].is_string() || result["connectionId"].is_string(),
        "hello must authenticate: {result}"
    );

    RawNode {
        out,
        pumps: vec![writer, reader],
        pending,
        arrivals,
        node_token,
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    token: String,
    host: String,
    workspace: String,
    project: String,
}

impl Harness {
    /// Boot the Hub, enroll+connect the first raw carrier `a` (so the lane host
    /// is online), then create the single-lane project. Returns both.
    async fn new() -> (Self, RawNode) {
        let dir = tempfile::tempdir().unwrap();
        let hub = spawn(HubConfig::for_test(dir.path().join("hub")))
            .await
            .expect("hub");
        let token = hub
            .mint_device_token("session-fence-human")
            .await
            .expect("device token");
        let host = remuda_protocol::HostId::new().as_id().to_string();
        let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();
        let enroll = hub
            .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
            .await
            .unwrap();

        let a = connect_raw(hub.addr, &host, &workspace, &enroll, 1).await;

        let response = reqwest::Client::new()
            .post(format!("http://{}/v1/projects", hub.addr))
            .bearer_auth(&token)
            .json(&json!({
                "name": "session-fence",
                "members": [{"hostId": host, "workspaceId": workspace, "role": "build"}],
                "hosts": [{"hostId": host, "maxInstances": 8}],
                "gate": {
                    "affected": true, "web": "never",
                    "landSerialization": "global-cas",
                    "lanes": [{
                        "id": "lane1", "hostId": host,
                        "repoPath": "/tmp/session-fence/repo",
                        "targetDir": "/tmp/session-fence/target",
                        "ports": "58510-58519", "env": {},
                        "lockPath": "/tmp/session-fence.lock",
                        "pwEndpoint": "ws://127.0.0.1:0/"
                    }]
                }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{:?}", response.text().await);
        let created: Value = response.json().await.unwrap();
        let project = created["id"].as_str().unwrap().to_owned();

        (
            Self {
                _dir: dir,
                hub,
                token,
                host,
                workspace,
                project,
            },
            a,
        )
    }

    async fn connect(&self, auth_token: &str, revision: u64) -> RawNode {
        connect_raw(
            self.hub.addr,
            &self.host,
            &self.workspace,
            auth_token,
            revision,
        )
        .await
    }

    async fn enqueue_land(&self, branch: &str) -> String {
        let response = reqwest::Client::new()
            .post(format!(
                "http://{}/v1/projects/{}/gate",
                self.hub.addr, self.project
            ))
            .bearer_auth(&self.token)
            .json(&json!({"branch": branch, "mode": "land"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{:?}", response.text().await);
        response.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn job(&self, id: &str) -> Value {
        self.hub
            .test_get_gate_job(id)
            .await
            .unwrap()
            .expect("job exists")
    }

    async fn wait_state(&self, id: &str, states: &[&str]) -> Value {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let job = self.job(id).await;
            if states
                .iter()
                .any(|wanted| job["state"].as_str() == Some(*wanted))
            {
                return job;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "never reached {states:?}: {job}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

fn grant_frame(id: &str, job: &str, generation: u32, host: &str) -> Value {
    json!({
        "jsonrpc": "2.0", "id": id, "method": "gate.landGrant",
        "params": {
            "jobId": job, "generation": generation,
            "hostId": host, "sessionEpoch": format!("sess-{id}"),
            "mergeSha": MERGE,
        }
    })
}

/// Takeover while A is still open: A is granted, then B connects and supersedes
/// it. The still-open A is revoked on BOTH authority checks (pre-run authorize
/// and the already-Landing idempotent grant), and B cannot settle A's landing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn superseded_open_socket_is_revoked_in_both_grant_branches() {
    let (h, a) = Harness::new().await;
    let a_token = a.node_token().to_owned();

    let job = h.enqueue_land("wt/land/takeover-open").await;
    a.wait_runs(1).await;
    let a_gen = a.last_run_generation();
    assert_eq!(h.job(&job).await["state"], json!("running"));

    // A takes the grant legitimately: Running → Landing.
    let granted = a
        .request(grant_frame("a-grant", &job, a_gen as u32, &h.host))
        .await;
    assert_eq!(granted["granted"], json!(true), "{granted}");
    assert_eq!(h.job(&job).await["state"], json!("landing"));

    // B connects for the SAME host while A's socket stays open (takeover).
    let b = h.connect(&a_token, 2).await;

    // The still-open A is no longer current: its pre-run authorize is refused.
    let authz = a
        .request(json!({
            "jsonrpc": "2.0", "id": "a-authz", "method": "gate.authorize",
            "params": {"jobId": job, "generation": a_gen}
        }))
        .await;
    assert_eq!(authz["authorized"], json!(false), "{authz}");
    assert_eq!(authz["state"], json!("session-superseded"), "{authz}");

    // A repeats its grant (the already-Landing idempotent branch): revoked.
    let a_regrant = a
        .request(grant_frame("a-regrant", &job, a_gen as u32, &h.host))
        .await;
    assert_eq!(a_regrant["granted"], json!(false), "{a_regrant}");
    assert_eq!(
        a_regrant["state"],
        json!("session-superseded"),
        "superseded A must be refused in the Landing branch: {a_regrant}"
    );

    // B is current but never held A's grant: the Landing branch refuses it too.
    let b_regrant = b
        .request(grant_frame("b-regrant", &job, a_gen as u32, &h.host))
        .await;
    assert_eq!(b_regrant["granted"], json!(false), "{b_regrant}");
    assert_eq!(b_regrant["state"], json!("landing"), "{b_regrant}");

    // The row stays Landing on A's grant; B never pushed.
    assert_eq!(h.job(&job).await["state"], json!("landing"));
}

/// Full disconnect before B connects: A drops while Running; the job is requeued
/// rather than stranded, and B draws a fresh non-reused session, is
/// redispatched, and takes the grant A never reached.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_disconnect_requeues_and_a_fresh_session_is_dispatched_and_grants() {
    let (h, mut a) = Harness::new().await;
    let a_token = a.node_token().to_owned();

    let job = h.enqueue_land("wt/land/drop-then-b").await;
    a.wait_runs(1).await;
    assert_eq!(h.job(&job).await["state"], json!("running"));

    // A fully disconnects: close handshake so the Hub observes the carrier end.
    a.shutdown();
    drop(a);
    // The Hub must not strand the job: the ended session's Running row requeues.
    let queued = h.wait_state(&job, &["queued"]).await;
    assert!(
        queued["hostId"].is_null(),
        "dispatch host cleared: {queued}"
    );

    // B connects AFTER the disconnect with the same node token: a fresh,
    // non-reused session. The scheduler redispatches the queued job to B.
    let b = h.connect(&a_token, 3).await;
    b.wait_runs(1).await;
    let b_gen = b.last_run_generation();
    assert_eq!(h.job(&job).await["state"], json!("running"));

    // B now owns the dispatch and can legitimately take the grant.
    let granted = b
        .request(grant_frame("b-grant", &job, b_gen as u32, &h.host))
        .await;
    assert_eq!(
        granted["granted"],
        json!(true),
        "fresh session grants: {granted}"
    );
    assert_eq!(h.job(&job).await["state"], json!("landing"));
}
