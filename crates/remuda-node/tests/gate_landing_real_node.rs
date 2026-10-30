//! ma-initiator r36 item 2: the Landing owner-release race through the REAL
//! Hub and the PRODUCTION Node (not a scripted fake). An in-process Hub
//! dispatches over a live loopback WSS carrier into a real DevNode runtime:
//!
//! 1. generation 1 gate.run reaches `landing` with the land child parked AFTER
//!    its start barrier/fence write — a genuinely registered owner holding a
//!    live process group. The Hub's forced reconciliation probes through the
//!    production `gate.resolveLanding` handler; the gate registry withholds
//!    Absent while the owner is registered → `unknown`, the row stays Landing.
//! 2. The parked child dies by signal with no verdict; the production owner
//!    observes the group ESRCH and writes the attempt-qualified death
//!    certificate, then unregisters and returns `uncertain`.
//! 3. The next probe now attests `absent` through real certificate validation
//!    and the Hub requeues; the scheduler dispatches generation 2 and the
//!    PRODUCTION lane slot is acquired (a held slot would answer `lane-busy`).
#![allow(missing_docs)]

use std::time::Duration;

use remuda_hub::HubConfig;
use remuda_node::{DevNode, DevServerConfig, WssConfig, WssLink};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(40);

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
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("read");
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
        let online = body["items"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| item["id"] == host_id && item["online"] == json!(true))
        });
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

fn git(repo: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .expect("git spawn");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Scripted merge CLI consumed by the PRODUCTION runner: every authority and
/// ownership decision still runs in the Node/Hub; only the merge CLI's own
/// work is faked.
///
/// verify → a verified report naming the real branch-tip sha.
/// land attempt 1 → blocks on the real start barrier, then holds the
/// registered owner until the test drops a marker, then dies by SIGNAL with no
/// verdict (the owner classifies Unknown; group ESRCH certifies death).
/// land attempt 2 → a structured gate_failed verdict naming the same merge,
/// proving the second generation really executed in the production lane.
const MERGE_SCRIPT: &str = r#"#!/bin/bash
dir=$(dirname "$0")
base=$(git rev-parse origin/main 2>/dev/null || git rev-parse main 2>/dev/null || echo 1111111111111111111111111111111111111111)
head=$(git rev-parse origin/wt/land/r36 2>/dev/null || git rev-parse wt/land/r36 2>/dev/null || echo "$base")
# The land phase pins the exact verified merge via --report; honor it so the
# production merge-matches-grant validation accepts the verdict.
report=""; prev=""
for a in "$@"; do
  if [ "$prev" = "--report" ]; then report="$a"; fi
  prev="$a"
done
if ! printf '%s\n' "$*" | grep -q -- '--land'; then
  echo "{\"exitCode\":0,\"status\":\"verified\",\"base\":\"$base\",\"head\":\"$head\",\"merged\":\"$head\",\"steps\":[]}"
  exit 0
fi
merged="${report:-$head}"
if [ "${REMUDA_START_BARRIER_FD-}" != "0" ]; then
  exit 0
fi
IFS= read -r -n 1 -t 30 byte || exit 4
[ -n "$byte" ] || exit 4
if [ -f "$dir/r36-attempt2.marker" ]; then
  echo "{\"exitCode\":1,\"status\":\"gate_failed\",\"merged\":\"$merged\",\"error\":\"r36 synthetic attempt-2 gate failure\",\"steps\":[]}"
  exit 1
fi
touch "$dir/r36-attempt2.marker"
while [ -f "$dir/r36-hold-land" ]; do sleep 0.1; done
kill -9 $$
"#;

/// The registered-owner → Unknown → death-cert Absent → requeue → generation-2
/// production lane acquisition sequence, over the real carrier.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn landing_absence_routes_through_the_real_node_to_a_generation2_acquisition() {
    let dir = tempfile::tempdir().expect("tmp");
    let root = dir.path();

    // Real local bare origin with main and a task branch (the verified merge
    // is the task tip: present in the lane, genuinely NOT on remote main).
    let origin = root.join("origin.git");
    std::process::Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(&origin)
        .status()
        .expect("git init bare");
    let seed = root.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-b", "main"]);
    git(&seed, &["config", "user.email", "t@example.com"]);
    git(&seed, &["config", "user.name", "T"]);
    std::fs::write(seed.join("file.txt"), "base\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "init"]);
    git(&seed, &["checkout", "-q", "-b", "wt/land/r36"]);
    std::fs::write(seed.join("file.txt"), "task\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "task change"]);
    git(
        &seed,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&seed, &["push", "-q", "origin", "main", "wt/land/r36"]);
    let merge_sha = git(&seed, &["rev-parse", "wt/land/r36"]);

    let lane = root.join("lane");
    git(
        root,
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            lane.to_str().unwrap(),
        ],
    );
    git(&lane, &["config", "user.email", "t@example.com"]);
    git(&lane, &["config", "user.name", "T"]);
    git(&lane, &["fetch", "-q", "origin"]);
    let target = root.join("target");
    std::fs::create_dir_all(&target).unwrap();

    // Scripted merge CLI for the production runner.
    let bin = root.join("merge-fake.sh");
    std::fs::write(&bin, MERGE_SCRIPT).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut config = HubConfig::for_test(root.join("hub-data"));
    config.gate_landing_recovery_ms = 1;
    // Deterministic probes only via test_force_landing_reconcile.
    config.gate_landing_probe_interval_ms = 600_000;
    let hub = remuda_hub::spawn(config).await.expect("hub");
    let addr = hub.addr;
    let token = hub
        .mint_device_token("gate-r36-human")
        .await
        .expect("device token");

    let node = DevNode::new(
        &DevServerConfig::loopback(0)
            .with_workspace_root(root.to_path_buf())
            .with_workspace_roots(remuda_testing::test_workspace_roots!())
            .with_workspace_registry(root.to_path_buf()),
    )
    .expect("dev node");
    node.gate_test_set_merge_bin(bin).await;
    let host_id = node.host().meta.id.as_id().as_str().to_owned();
    let workspace_id = node
        .workspaces()
        .expect("node workspaces")
        .into_iter()
        .next()
        .expect("one announced workspace")
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

    let (status, project) = http(
        addr,
        "POST",
        "/v1/projects",
        &token,
        Some(json!({
            "name": "gate-r36-real-node",
            "members": [{"hostId": host_id, "workspaceId": workspace_id, "role": "build"}],
            "hosts": [{"hostId": host_id, "maxInstances": 8}],
            "gate": {
                "affected": true,
                "web": "never",
                "landSerialization": "global-cas",
                "lanes": [{
                    "id": "lane1",
                    "hostId": host_id,
                    "repoPath": lane,
                    "targetDir": target,
                    "ports": "58480-58489",
                    "env": {},
                    "lockPath": root.join("gate-r36.lock"),
                    "pwEndpoint": "ws://127.0.0.1:0/"
                }]
            }
        })),
    )
    .await;
    assert_eq!(status, 200, "create project: {project}");
    let project_id = project["id"].as_str().expect("project id");

    // Hold attempt-1's land child BEFORE dispatch: it parks in the script's
    // hold loop with the owner registered and the group live.
    std::fs::write(root.join("r36-hold-land"), b"hold").unwrap();
    // r37 item 1: arm the per-registry owner-release barrier (one-shot). The
    // generation-1 finalizer parks here AFTER the group is dead and the
    // certificate is persisted, but while it still holds the lane/registration.
    let release_hold = remuda_node::CleanupHold::new();
    node.gate_test_arm_owner_release_hold(release_hold.clone())
        .await;
    let (status, enqueued) = http(
        addr,
        "POST",
        &format!("/v1/projects/{project_id}/gate"),
        &token,
        Some(json!({"branch": "wt/land/r36", "mode": "land"})),
    )
    .await;
    assert_eq!(status, 200, "enqueue: {enqueued}");
    let job_id = enqueued["id"].as_str().expect("job id").to_owned();

    // Diagnostics window: observe where the job gets stuck.
    let diag_deadline = tokio::time::Instant::now() + TIMEOUT;
    let landing = loop {
        let doc = get_job(&hub, &job_id).await;
        if doc["state"] == json!("landing") {
            break doc;
        }
        if tokio::time::Instant::now() >= diag_deadline {
            let live = node.gate_test_live_run_state(&job_id).await;
            panic!("job never reached landing; final doc {doc}; live run {live:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(landing["attempts"], json!(1), "{landing}");
    assert_eq!(landing["mergeSha"], json!(merge_sha), "{landing}");
    assert_eq!(landing["laneId"], json!("lane1"), "{landing}");

    // (1) Registered owner with a live group: the production liveness path
    // withholds Absent even though the merge is absent from remote main. The
    // forced reconcile AWAITS the whole read-only probe round-trip, so on
    // return the production handler has already answered Unknown.
    hub.test_force_landing_reconcile(&host_id).await;
    let still_landing = get_job(&hub, &job_id).await;
    assert_eq!(
        still_landing["state"],
        json!("landing"),
        "a registered owner must keep the probe Unknown, not requeue: {still_landing}"
    );
    assert_eq!(
        node.gate_test_live_run_state(&job_id).await.map(|(g, _)| g),
        Some(1),
        "the generation-1 owner is still registered during the first probe"
    );
    // A second probe (also awaited) while still held must answer identically.
    hub.test_force_landing_reconcile(&host_id).await;
    let still_landing_2 = get_job(&hub, &job_id).await;
    assert_eq!(
        still_landing_2["state"],
        json!("landing"),
        "a repeated probe against the registered owner must not requeue: {still_landing_2}"
    );

    // (2) Release the child: it dies by signal. The production owner certifies
    // the ESRCH group and parks on the owner-release barrier BEFORE releasing
    // the lane/unregistering — the exact dead-but-still-owned window.
    std::fs::remove_file(root.join("r36-hold-land")).unwrap();
    let hold_deadline = tokio::time::Instant::now() + TIMEOUT;
    while !release_hold.has_entered() {
        assert!(
            tokio::time::Instant::now() < hold_deadline,
            "the generation-1 finalizer never reached the owner-release barrier"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // r37 item 1: INSIDE the release window — the group is dead and the
    // certificate is already durable, yet the owner is still registered and
    // holds the lane. Verify all three facts before probing.
    let pgid = node
        .gate_test_live_land_pgid(&job_id)
        .await
        .expect("the registered owner still records its land group");
    assert!(
        !remuda_node::gate_test_group_exists(pgid),
        "the land group must be ESRCH-dead inside the release window"
    );
    let fence_dir = root.join("gate-land");
    let cert_deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let certs: Vec<_> = std::fs::read_dir(&fence_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("dead-") && name.ends_with(".json"))
            .collect();
        if !certs.is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < cert_deadline,
            "no death certificate before the release window probe: {certs:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        node.gate_test_live_run_state(&job_id).await.map(|(g, _)| g),
        Some(1),
        "the generation-1 owner is still registered inside the release window"
    );

    // The decisive owner-release assertion: even with the group dead and the
    // certificate durable, a registered owner still holding the lane forces the
    // production read-only probe to Unknown — the Hub must NOT requeue. The old
    // defect (VerifiedDead while registered) makes this probe Absent and the
    // job jump to queued/attempt 2.
    hub.test_force_landing_reconcile(&host_id).await;
    let held = get_job(&hub, &job_id).await;
    assert_eq!(
        held["state"],
        json!("landing"),
        "dead group + registered owner must keep the probe Unknown: {held}"
    );
    assert_eq!(
        held["attempts"],
        json!(1),
        "the owner-release window must not requeue: {held}"
    );
    assert_eq!(
        node.gate_test_live_run_state(&job_id).await.map(|(g, _)| g),
        Some(1),
        "the owner is still registered after the in-window probe"
    );

    // Now let the owner actually release the lane and unregister.
    release_hold.release();
    let unregister_deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if node.gate_test_live_run_state(&job_id).await.is_none() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < unregister_deadline,
            "the generation-1 owner never unregistered after release"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // (3) The next production probe now attests Absent → Hub requeues → gen 2
    // is dispatched and the production lane accepts it (no lane-busy).
    hub.test_force_landing_reconcile(&host_id).await;

    // Wait for the PRODUCTION Node to finish generation 2. Its gate.run is the
    // real dispatch over the carrier: a gate_failed verdict here proves the
    // generation-2 run acquired the production lane slot (a held slot answers
    // `lane-busy` instead and no gate_failed run executes). This is the
    // race-under-test, so it is asserted at the exact production verdict rather
    // than on the Hub's later terminal state (the unrelated 1 s schedule tick
    // can otherwise independently requeue on this fast fixture).
    let verdict_deadline = tokio::time::Instant::now() + TIMEOUT;
    let (gen2_lane, gen2_status, gen2_error) = loop {
        if let Some(remuda_node::LastGateVerdict {
            generation,
            lane_id: lane,
            status,
            error,
        }) = node.gate_test_last_verdict(&job_id).await
            && generation == 2
        {
            break (lane, status, error);
        }
        assert!(
            tokio::time::Instant::now() < verdict_deadline,
            "generation 2 never produced a production verdict"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(
        gen2_status, "failed",
        "gen 2 must execute and return the production gate_failed verdict, got {gen2_status:?}"
    );
    assert!(
        !gen2_status.contains("lane-busy"),
        "the released lane must accept generation 2"
    );
    // The generation-2 gate.run acquired the SAME production lane slot: a busy
    // slot returns `lane-busy` before the merge binary ever runs, so an
    // executed gate_failed verdict on lane1 is the lane-acquisition proof.
    assert_eq!(
        gen2_lane, "lane1",
        "gen 2 must acquire lane1, got {gen2_lane:?}"
    );
    let gen2_error = gen2_error.unwrap_or_default();
    assert!(
        gen2_error.contains("r36 synthetic attempt-2 gate failure"),
        "gen 2 ran the production runner with its own failure: {gen2_error}"
    );

    // The Hub recorded a generation-2 dispatch carrying the exact granted
    // merge (the post-absent requeue claims attempts 2). Polled tightly to
    // observe the generation-2 row; the Node verdict above is the
    // race-independent acquisition proof regardless of later tick requeues.
    let row_deadline = tokio::time::Instant::now() + TIMEOUT;
    let mut saw_gen2_merge = false;
    loop {
        let doc = get_job(&hub, &job_id).await;
        if doc["attempts"] == json!(2) && doc["mergeSha"] == json!(merge_sha) {
            assert_eq!(doc["laneId"], json!("lane1"), "{doc}");
            assert_eq!(doc["hostId"].as_str(), Some(host_id.as_str()), "{doc}");
            saw_gen2_merge = true;
            break;
        }
        if tokio::time::Instant::now() >= row_deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        saw_gen2_merge,
        "the Hub never recorded a generation-2 dispatch for the exact merge on lane1"
    );
}
