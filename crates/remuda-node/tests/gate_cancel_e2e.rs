//! ma-initiator r7 item 2 / r8 item 2: the gate.cancel-beats-gate.run race
//! through the REAL send path. An in-process Hub dispatches BOTH frames
//! over a live WSS carrier into a real DevNode runtime:
//!
//! 1. POST /gate enqueues; the Hub's OWN scheduler claims the job and the
//!    gate.run frame is in flight — parked in the Node handler by a
//!    test-only entry hold BEFORE the run registry/lane/git steps;
//! 2. the Hub cancel route commits Canceling and the Hub itself sends
//!    gate.cancel over the carrier; the unregistered run tombstones;
//! 3. releasing the hold lets that SAME dispatch proceed — the Node
//!    refuses with status canceled, and the Hub settles through its
//!    Canceling branch (not the Running + canceled branch).
#![allow(missing_docs)]

use std::sync::Arc;
use std::time::Duration;

use remuda_hub::HubConfig;
use remuda_node::{DevNode, DevServerConfig, GateRunHold, WssConfig, WssLink};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(20);

async fn enroll_token(hub: &remuda_hub::RunningHub) -> String {
    hub.mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await
        .expect("mint enroll token")
}

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let body_text = body.map(|value| value.to_string()).unwrap_or_default();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n",
        body_text.len()
    );
    if !body_text.is_empty() {
        request.push_str("Content-Type: application/json\r\n");
    }
    request.push_str("\r\n");
    request.push_str(&body_text);

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read response");
    let text = String::from_utf8_lossy(&response);
    let (head, raw_body) = text.split_once("\r\n\r\n").expect("http header delimiter");
    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .expect("http status");
    let value = if raw_body.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(raw_body.trim()).expect("json body")
    };
    (status, value)
}

async fn wait_host_online(addr: std::net::SocketAddr, token: &str, host_id: &str) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let (status, body) = http(addr, "GET", "/v1/hosts", token, None).await;
        assert_eq!(status, 200, "{body}");
        let online = body["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .any(|item| item["id"] == host_id && item["online"] == json!(true))
            })
            .unwrap_or(false);
        if online {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "host never came online: {body}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn get_job(hub: &remuda_hub::RunningHub, job_id: &str) -> Value {
    hub.test_get_gate_job(job_id)
        .await
        .expect("get job")
        .expect("job exists")
}

/// One full cancel-beats-run race over the real carrier. Returns the final
/// settled job doc.
async fn one_race_iteration(dir: &tempfile::TempDir) -> Value {
    let data_dir = dir.path().join("hub-data");
    let hub = remuda_hub::spawn(HubConfig::for_test(data_dir.clone()))
        .await
        .expect("hub");
    let addr = hub.addr;
    let token = hub
        .mint_device_token("gate-e2e-human")
        .await
        .expect("device token");

    // A REAL DevNode runtime over the REAL outbound WSS carrier; inbound
    // gate frames dispatch into its gate registry through background tasks,
    // so a parked gate.run does not block gate.cancel.
    let node = DevNode::new(
        &DevServerConfig::loopback(0)
            .with_workspace_root(dir.path().to_path_buf())
            .with_workspace_roots(remuda_testing::test_workspace_roots!()),
    )
    .expect("dev node");
    let host_id = node.host().meta.id.as_id().as_str().to_owned();
    let workspace_id = node
        .workspaces()
        .expect("node workspaces")
        .into_iter()
        .next()
        .expect("at least one announced workspace")
        .meta
        .id
        .as_id()
        .as_str()
        .to_owned();
    let ws_config = WssConfig::loopback(addr, enroll_token(&hub).await, host_id.clone());
    let _link = tokio::time::timeout(TIMEOUT, WssLink::connect_runtime(ws_config, node.clone()))
        .await
        .expect("connect timeout")
        .expect("runtime wss connect");
    wait_host_online(addr, &token, &host_id).await;

    // The lane checkout does NOT exist: a run that actually executes dies in
    // git fetch; a tombstoned run never touches either directory.
    let repo_path = dir.path().join("never-checked-out-repo");
    let target_dir = dir.path().join("never-created-target");
    let lock_path = dir.path().join("gate-e2e.lock");

    let (status, project) = http(
        addr,
        "POST",
        "/v1/projects",
        &token,
        Some(json!({
            "name": "gate-cancel-e2e",
            "members": [{"hostId": host_id, "workspaceId": workspace_id, "role": "build"}],
            "hosts": [{"hostId": host_id, "maxInstances": 8}],
            "gate": {
                "affected": true,
                "web": "never",
                "landSerialization": "global-cas",
                "lanes": [{
                    "id": "lane1",
                    "hostId": host_id,
                    "repoPath": repo_path,
                    "targetDir": target_dir,
                    "ports": "58470-58479",
                    "env": {},
                    "lockPath": lock_path,
                    "pwEndpoint": "ws://127.0.0.1:0/"
                }]
            }
        })),
    )
    .await;
    assert_eq!(status, 200, "create project: {project}");
    let project_id = project["id"].as_str().expect("project id");

    // Arm the Node entry hold BEFORE enqueue: the very next gate.run frame
    // parks at the handler entry, before run registration / lane / git.
    let hold = Arc::new(GateRunHold::new());
    node.gate_test_arm_run_hold(hold.clone()).await;

    // Enqueue: the Hub's scheduler (schedule_once from the POST handler,
    // then the 1 s loop) claims the Running job and dispatches gate.run to
    // the Node, which parks in the hold. No SQL touches the job.
    let (status, enqueued) = http(
        addr,
        "POST",
        &format!("/v1/projects/{project_id}/gate"),
        &token,
        Some(json!({"branch": "wt/gate/cancel-overtakes-run", "mode": "verify"})),
    )
    .await;
    assert_eq!(status, 200, "enqueue: {enqueued}");
    let job_id = tokio::time::timeout(TIMEOUT, hold.wait_arrived())
        .await
        .expect("the Hub never dispatched gate.run")
        .to_owned();
    assert_eq!(
        job_id,
        enqueued["id"].as_str().expect("enqueued id"),
        "the parked frame is for a different job"
    );
    // Claim committed: the row the cancel writer reads is Running.
    let claimed = get_job(&hub, &job_id).await;
    assert_eq!(claimed["state"], json!("running"), "{claimed}");

    // Hub-side cancel while the run is genuinely in flight: the Hub writer
    // commits Canceling and the Hub sends gate.cancel; the Node acks after
    // tombstoning (no run registered yet). The 200 body says `canceling`,
    // so the gate.run verdict below can only be applied on a Canceling row
    // — never on the Running + canceled branch.
    let (status, canceled) = http(
        addr,
        "POST",
        &format!("/v1/projects/{project_id}/gate/jobs/{job_id}/cancel"),
        &token,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, 200, "cancel: {canceled}");
    assert_eq!(canceled["state"], json!("canceling"), "{canceled}");

    // Release the SAME in-flight gate.run the Hub dispatched. The tombstone
    // makes it answer canceled before acquiring the lane or running git.
    hold.release();

    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let doc = get_job(&hub, &job_id).await;
        if doc["state"] == json!("canceled") {
            break doc;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "job did not settle canceled: {doc}"
        );
        assert_ne!(
            doc["state"],
            json!("failed"),
            "the run executed (git fetch) instead of being refused canceled: {doc}"
        );
        assert_ne!(
            doc["state"],
            json!("queued"),
            "the verdict requeued the job: {doc}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The cancel-beats-run race over the real carrier must always settle the
/// job Canceled with the tombstone refusal, through the Hub's Canceling
/// branch, without ever reaching git. Looped 20x to rule out carrier/
/// scheduler timing flake.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_overtakes_run_over_the_real_carrier_twenty_times() {
    for iteration in 0..20 {
        let dir = tempfile::tempdir().expect("tmp");
        let doc = one_race_iteration(&dir).await;

        assert_eq!(doc["state"], json!("canceled"), "[{iteration}] {doc}");
        // The Canceling branch keeps the cancel stamp and the tombstone
        // error; exactly one claim happened (no requeue/re-dispatch).
        assert!(
            !doc["cancelRequestedAt"].is_null(),
            "[{iteration}] cancel stamp missing: {doc}"
        );
        assert_eq!(
            doc["attempts"],
            json!(1),
            "[{iteration}] a second claim/dispatch occurred: {doc}"
        );
        let error = doc["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("before the run registered"),
            "[{iteration}] settled without the Node tombstone refusal \
             (would indicate the Running + canceled branch): {doc}"
        );
        // The refused run never executed: neither the checkout nor the
        // target dir was created (a real run starts with git fetch inside
        // repoPath).
        assert!(
            !dir.path().join("never-checked-out-repo").exists(),
            "[{iteration}] the tombstoned run entered the checkout"
        );
        assert!(
            !dir.path().join("never-created-target").exists(),
            "[{iteration}] the tombstoned run created the target dir"
        );
    }
}
