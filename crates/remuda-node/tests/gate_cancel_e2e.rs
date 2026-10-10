//! ma-initiator r7 item 2: the gate.cancel-beats-gate.run race through the
//! REAL send path — an in-process Hub dispatching BOTH frames over a live
//! WSS carrier into a real DevNode runtime (not `dispatch_gate_rpc` in
//! isolation). The cancel is Hub-sent after the claim commits; the gate.run
//! it raced with is then dispatched by the Hub itself, and the Node must
//! refuse it as canceled without ever reaching the git-fetch step.
#![allow(missing_docs)]

use remuda_hub::HubConfig;
use remuda_node::{DevNode, DevServerConfig, WssConfig, WssLink};
use serde_json::{Value, json};
use std::time::Duration;
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
    let status = head
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

/// Poll a Hub GET until it yields `wanted` or time out.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hub_dispatched_cancel_then_hub_dispatched_run_refuses_on_the_real_carrier() {
    let dir = tempfile::tempdir().expect("tmp");
    let data_dir = dir.path().join("hub-data");
    let hub = remuda_hub::spawn(HubConfig::for_test(data_dir.clone()))
        .await
        .expect("hub");
    let addr = hub.addr;
    let token = hub
        .mint_device_token("gate-e2e-human")
        .await
        .expect("device token");

    // A REAL DevNode runtime, connected over the REAL outbound WSS carrier;
    // incoming gate.run/gate.cancel frames dispatch into its gate registry.
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

    // The lane checkout deliberately DOES NOT EXIST: a run that actually
    // executes tries `git fetch` in it; a tombstoned run returns before the
    // registry and must never create or enter the directory.
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

    // Enqueue a LAND job and deterministically commit the tick's claim
    // (queued -> running, pinned to lane1) the way the scheduler's tick does.
    let (status, enqueued) = http(
        addr,
        "POST",
        &format!("/v1/projects/{project_id}/gate"),
        &token,
        Some(json!({"branch": "wt/gate/cancel-overtakes-run", "mode": "land"})),
    )
    .await;
    assert_eq!(status, 200, "enqueue: {enqueued}");
    let job_id = enqueued["id"].as_str().expect("job id").to_owned();
    let claimed = hub
        .test_claim_gate_job(&job_id, "lane1", &host_id)
        .await
        .expect("claim")
        .expect("job");
    assert_eq!(claimed["state"], json!("running"), "{claimed}");

    // Hub-side cancel on the Running job: the Hub's OWN writer stamps
    // canceling and the Hub itself sends gate.cancel over the carrier. The
    // real Node has no registered run yet (its gate.run is still in flight),
    // so it records the tombstone and acks.
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

    // The gate.run frame for the claim that already committed is dispatched
    // by the Hub's own scheduler (the job is re-queued exactly as the window
    // leaves it: claim committed before the cancel writer ran).
    let db = rusqlite::Connection::open(data_dir.join("hub.sqlite")).expect("open hub db");
    db.execute(
        "UPDATE gate_jobs
            SET state = 'queued', lane_id = NULL,
                doc_json = json_set(
                    json_set(
                        json_set(
                            json_set(doc_json, '$.state', 'queued'),
                            '$.startedAt', NULL),
                        '$.hostId', NULL),
                    '$.laneId', NULL),
                revision = revision + 1,
                updated_at = '2030-01-01T00:00:00.000Z'
          WHERE id = ?1",
        rusqlite::params![job_id],
    )
    .expect("requeue the claimed job for the in-flight dispatch");
    drop(db);
    hub.test_gate_tick().await;

    // The Node tombstone makes the Hub-dispatched gate.run answer canceled;
    // the Hub applies that verdict and the job settles Canceled — never
    // requeued, never landed.
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let final_doc = loop {
        let doc = hub
            .test_get_gate_job(&job_id)
            .await
            .expect("get job")
            .expect("job exists");
        if doc["state"] == json!("canceled") {
            break doc;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "job did not settle canceled: {doc}"
        );
        assert_ne!(
            doc["state"],
            json!("queued"),
            "the tombstoned run requeued the job: {doc}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(
        final_doc["error"]
            .as_str()
            .is_some_and(|text| text.contains("before the run registered")),
        "the refusal reason must reach the Hub: {final_doc}"
    );

    // The refused run never executed: neither the checkout nor the target
    // dir exists (a real run starts with `git fetch` inside repoPath).
    assert!(
        !repo_path.exists(),
        "the tombstoned gate.run entered/created the checkout: {repo_path:?}"
    );
    assert!(
        !target_dir.exists(),
        "the tombstoned gate.run created the target dir: {target_dir:?}"
    );

    // A later tick keeps the job terminal: no second dispatch.
    hub.test_gate_tick().await;
    let doc = hub
        .test_get_gate_job(&job_id)
        .await
        .expect("get job")
        .expect("job exists");
    assert_eq!(
        doc["state"],
        json!("canceled"),
        "tick revived the job: {doc}"
    );
}
