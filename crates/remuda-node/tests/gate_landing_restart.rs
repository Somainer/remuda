//! ma-initiator r13 item 1 — the Landing authority invariant across a REAL Hub
//! restart, using the production stack:
//!   * a real in-process Hub is started, ENQUEUED, then SHUT DOWN and a SECOND
//!     Hub is started on the SAME data dir and port (a genuine restart);
//!   * a real DevNode over the real outbound WSS carrier runs the consumed
//!     merge/land CLI (a tiny stand-in that performs REAL `git` against a real
//!     local BARE origin — same shape as the stdio land round-trip);
//!   * the verify→land GRANT is the production Hub Running → Landing writer;
//!   * after the restart a fresh carrier session reconnects and the Hub settles
//!     the Landing ONLY by a read-only `gate.resolveLanding` probe (real
//!     `git fetch` + `merge-base --is-ancestor`) against the EXACT granted
//!     merge. It never re-runs or re-pushes anything.
//!
//! Case A: the push reached origin/main before the Hub exited → after restart
//! the job settles landed exactly once and is never pushed again.
//! Case B: the push is still blocked (not on the remote) at restart → the row
//! STAYS Landing and keeps its lane occupancy; only once the granted push
//! actually completes does the bounded reconciliation settle it landed.

#![allow(missing_docs)]

use std::path::PathBuf;
use std::time::Duration;

use remuda_hub::HubConfig;
use remuda_node::{DevNode, DevServerConfig, WssConfig, WssLink};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(40);

fn git(repo: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

struct Origin {
    lane: PathBuf,
    origin: PathBuf,
    base_main: String,
}

/// Bare origin + seeded main + task branch + a real cloned lane checkout.
fn build_origin(root: &std::path::Path) -> Origin {
    let origin = root.join("origin.git");
    std::process::Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(&origin)
        .status()
        .unwrap();
    let seed = root.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-b", "main"]);
    git(&seed, &["config", "user.email", "t@example.com"]);
    git(&seed, &["config", "user.name", "T"]);
    std::fs::write(seed.join("file.txt"), "base\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "init"]);
    git(&seed, &["checkout", "-q", "-b", "wt/land/restart"]);
    std::fs::write(seed.join("file.txt"), "task\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "task change"]);
    git(
        &seed,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&seed, &["push", "-q", "origin", "main", "wt/land/restart"]);
    let base_main = git(&seed, &["rev-parse", "main"]);

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
    Origin {
        lane,
        origin,
        base_main,
    }
}

/// Write the controllable merge/land CLI. `CASE=push_then_block` pushes in the
/// land phase BEFORE waiting (the push landed before the crash); `block_then_push`
/// waits for `$GATE_DIR/proceed` and only then pushes (a still-outstanding push).
/// Every successful land push appends one line to `$GATE_DIR/pushes.log`.
fn write_merge_cli(dir: &std::path::Path) -> PathBuf {
    let bin = dir.join("fake-merge.sh");
    let script = r#"#!/bin/bash
set -eu
branch=""; onto="main"; mode=""
while [ $# -gt 0 ]; do
  case "$1" in
    --onto) onto="$2"; shift 2;;
    --land) mode=land; shift;;
    --gate) mode=gate; shift;;
    --no-push) shift;;
    --repo|--target-dir) shift 2;;
    --*) shift;;
    merge) shift;;
    *) if [ -z "$branch" ]; then branch="$1"; fi; shift;;
  esac
done
git fetch -q origin
scratch=$(mktemp -d "${TMPDIR:-/tmp}/remuda-mq-$$-0-XXXXXX")
echo '{"name":"cargo-test","status":"ok","durationMs":5,"attempts":1,"retried":false}' > "$scratch/gate.jsonl"
wait_proceed() {
  for _ in $(seq 1 600); do [ -f "$GATE_DIR/proceed" ] && return 0; sleep 0.1; done
}
if [ "$mode" = "gate" ]; then
  git checkout -qB "$onto" "origin/$onto"
  base=$(git rev-parse "$onto")
  head=$(git rev-parse "origin/$branch")
  git merge -q --no-ff "origin/$branch" -m "gate merge $branch"
  if [ "${CASE:-block_then_push}" = "verify_block" ]; then
    echo verifying >> "$GATE_DIR/verify-state.log"
    wait_proceed
  fi
  merged=$(git rev-parse HEAD)
  printf '{"exitCode":0,"status":"verified","branch":"%s","base":"%s","head":"%s","merged":"%s","steps":[{"name":"cargo-test","status":"ok","durationMs":5}]}\n' "$branch" "$base" "$head" "$merged"
else
  if [ "${CASE:-block_then_push}" = "push_then_block" ]; then
    git push -q origin "HEAD:$onto"
    echo pushed >> "$GATE_DIR/pushes.log"
    wait_proceed
  else
    wait_proceed
    git push -q origin "HEAD:$onto"
    echo pushed >> "$GATE_DIR/pushes.log"
  fi
  merged=$(git rev-parse HEAD)
  current=$(git rev-parse "origin/$onto")
  printf '{"exitCode":0,"status":"landed","branch":"%s","merged":"%s","currentMain":"%s","steps":[{"name":"cargo-test","status":"ok","durationMs":5}]}\n' "$branch" "$merged" "$current"
fi
"#;
    std::fs::write(&bin, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let body_text = body.map(|v| v.to_string()).unwrap_or_default();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n",
        body_text.len()
    );
    if !body_text.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
    }
    req.push_str("\r\n");
    req.push_str(&body_text);
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let value = if body.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body.trim()).unwrap_or(Value::Null)
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
                .any(|i| i["id"] == host_id && i["online"] == json!(true))
        });
        if online {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "host never online: {body}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

async fn job_of(hub: &remuda_hub::RunningHub, id: &str) -> Value {
    hub.test_get_gate_job(id).await.unwrap().expect("job")
}

async fn wait_state(hub: &remuda_hub::RunningHub, id: &str, states: &[&str]) -> Value {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let job = job_of(hub, id).await;
        if let Some(s) = job["state"].as_str()
            && states.contains(&s)
        {
            return job;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never reached {states:?}: {job}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Full A scenario: push on remote before crash → restart lands it once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn landing_already_pushed_settles_landed_once_after_real_hub_restart() {
    let dir = tempfile::tempdir().unwrap();
    let gate_dir = dir.path().join("gatedir");
    std::fs::create_dir_all(&gate_dir).unwrap();
    let origin = build_origin(dir.path());
    let bin = write_merge_cli(dir.path());

    let data_dir = dir.path().join("hub-data");
    let port = free_port();

    let node = DevNode::new(
        &DevServerConfig::loopback(0)
            .with_workspace_root(dir.path().to_path_buf())
            .with_workspace_roots(remuda_testing::test_workspace_roots!()),
    )
    .expect("dev node");
    node.gate_test_set_merge_bin(bin).await;
    let host_id = node.host().meta.id.as_id().as_str().to_owned();
    let workspace_id = node
        .workspaces()
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .meta
        .id
        .as_id()
        .as_str()
        .to_owned();

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));

    // Boot 1: enroll, create project, get the persistent node token.
    let mut config = HubConfig::for_test(data_dir.clone());
    config.listen = addr;
    let hub = remuda_hub::spawn(config).await.unwrap();
    let dev_token = hub.mint_device_token("h").await.unwrap();
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await
        .unwrap();
    let link = WssLink::connect_runtime(
        WssConfig::loopback(addr, enroll, host_id.clone()),
        node.clone(),
    )
    .await
    .unwrap();
    wait_host_online(addr, &dev_token, &host_id).await;
    let node_token = link.node_token.clone().expect("persistent node token");

    let body = json!({
        "name": "gate-restart-a",
        "members": [{"hostId": host_id, "workspaceId": workspace_id, "role": "build"}],
        "hosts": [{"hostId": host_id, "maxInstances": 8}],
        "gate": {
            "affected": true, "web": "never", "landSerialization": "global-cas",
            "lanes": [{
                "id": "lane1", "hostId": host_id,
                "repoPath": origin.lane.to_string_lossy(),
                "targetDir": dir.path().join("target").to_string_lossy(),
                "ports": "58610-58619",
                "env": {"GATE_DIR": gate_dir.to_string_lossy(), "CASE": "push_then_block",
                        "PATH": std::env::var("PATH").unwrap_or_default()},
                "lockPath": dir.path().join("gate.lock").to_string_lossy(),
                "pwEndpoint": "ws://127.0.0.1:0/"
            }]
        }
    });
    let (status, created) = http(addr, "POST", "/v1/projects", &dev_token, Some(body)).await;
    assert_eq!(status, 200, "{created}");
    let project_id = created["id"].as_str().unwrap().to_owned();

    let (status, enqueued) = http(
        addr,
        "POST",
        &format!("/v1/projects/{project_id}/gate"),
        &dev_token,
        Some(json!({"branch": "wt/land/restart", "mode": "land"})),
    )
    .await;
    assert_eq!(status, 200, "{enqueued}");
    let job_id = enqueued["id"].as_str().unwrap().to_owned();

    // Wait for the granted Landing AND the real push to have reached origin.
    let landing = wait_state(&hub, &job_id, &["landing"]).await;
    assert_eq!(landing["state"], "landing");
    let merge_sha = landing["mergeSha"].as_str().unwrap().to_owned();
    let pushes = gate_dir.join("pushes.log");
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !pushes.exists() {
        assert!(tokio::time::Instant::now() < deadline, "land never pushed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let remote_after_push = git(&origin.origin, &["rev-parse", "main"]);
    assert_ne!(remote_after_push, origin.base_main, "main advanced");
    assert_eq!(
        git(&origin.origin, &["merge-base", "main", &merge_sha]),
        merge_sha,
        "granted merge is on remote main"
    );

    // Crash the Hub (the granted land child is blocked post-push; its process is
    // kill_on_drop(false) and survives). The row must survive as Landing.
    link.shutdown().await;
    hub.shutdown().await;

    // Restart a real second Hub on the same data dir + port.
    let mut config2 = HubConfig::for_test(data_dir.clone());
    config2.listen = addr;
    let hub2 = remuda_hub::spawn(config2).await.unwrap();
    let persisted = job_of(&hub2, &job_id).await;
    assert_eq!(
        persisted["state"],
        json!("landing"),
        "startup preserves Landing occupancy: {persisted}"
    );
    assert!(
        persisted["hostId"].is_string(),
        "lane/host occupancy kept: {persisted}"
    );

    // A fresh carrier session reconnects; post-hello reconciliation probes the
    // remote for the EXACT granted merge and settles landed — with no re-run.
    let _link2 = WssLink::connect_runtime(
        WssConfig::loopback(addr, node_token, host_id.clone()),
        node.clone(),
    )
    .await
    .unwrap();
    let settled = wait_state(&hub2, &job_id, &["landed"]).await;
    assert_eq!(settled["state"], "landed", "{settled}");

    // Exactly one push ever happened; the reconcile is read-only.
    let push_count = std::fs::read_to_string(&pushes).unwrap().lines().count();
    assert_eq!(push_count, 1, "the granted merge must not be re-pushed");
    assert_eq!(
        git(&origin.origin, &["rev-parse", "main"]),
        remote_after_push
    );

    // Exactly one dispatch and one gate.landed journal entry.
    let actions = hub2.test_gate_audit_actions(&job_id).await.unwrap();
    assert_eq!(
        actions.iter().filter(|a| *a == "gate.running").count(),
        1,
        "{actions:?}"
    );
    assert_eq!(
        actions.iter().filter(|a| *a == "gate.landed").count(),
        1,
        "{actions:?}"
    );

    // Release the orphaned blocked land child so it exits cleanly.
    std::fs::write(gate_dir.join("proceed"), b"go").unwrap();
}

/// Full B scenario: push still outstanding at restart → stays Landing (lane
/// occupied, a second land job is not dispatched); settles landed only once the
/// granted push completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn landing_still_pushing_keeps_occupancy_until_established_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let gate_dir = dir.path().join("gatedir");
    std::fs::create_dir_all(&gate_dir).unwrap();
    let origin = build_origin(dir.path());
    let bin = write_merge_cli(dir.path());

    let data_dir = dir.path().join("hub-data");
    let port = free_port();
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));

    let node = DevNode::new(
        &DevServerConfig::loopback(0)
            .with_workspace_root(dir.path().to_path_buf())
            .with_workspace_roots(remuda_testing::test_workspace_roots!()),
    )
    .expect("dev node");
    node.gate_test_set_merge_bin(bin).await;
    let host_id = node.host().meta.id.as_id().as_str().to_owned();
    let workspace_id = node
        .workspaces()
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .meta
        .id
        .as_id()
        .as_str()
        .to_owned();

    let mut config = HubConfig::for_test(data_dir.clone());
    config.listen = addr;
    let hub = remuda_hub::spawn(config).await.unwrap();
    let dev_token = hub.mint_device_token("h").await.unwrap();
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await
        .unwrap();
    let link = WssLink::connect_runtime(
        WssConfig::loopback(addr, enroll, host_id.clone()),
        node.clone(),
    )
    .await
    .unwrap();
    wait_host_online(addr, &dev_token, &host_id).await;
    let node_token = link.node_token.clone().unwrap();

    let body = json!({
        "name": "gate-restart-b",
        "members": [{"hostId": host_id, "workspaceId": workspace_id, "role": "build"}],
        "hosts": [{"hostId": host_id, "maxInstances": 8}],
        "gate": {
            "affected": true, "web": "never", "landSerialization": "global-cas",
            "lanes": [{
                "id": "lane1", "hostId": host_id,
                "repoPath": origin.lane.to_string_lossy(),
                "targetDir": dir.path().join("target").to_string_lossy(),
                "ports": "58620-58629",
                "env": {"GATE_DIR": gate_dir.to_string_lossy(), "CASE": "block_then_push",
                        "PATH": std::env::var("PATH").unwrap_or_default()},
                "lockPath": dir.path().join("gate.lock").to_string_lossy(),
                "pwEndpoint": "ws://127.0.0.0:0/"
            }]
        }
    });
    let (status, created) = http(addr, "POST", "/v1/projects", &dev_token, Some(body)).await;
    assert_eq!(status, 200, "{created}");
    let project_id = created["id"].as_str().unwrap().to_owned();

    let (status, enqueued) = http(
        addr,
        "POST",
        &format!("/v1/projects/{project_id}/gate"),
        &dev_token,
        Some(json!({"branch": "wt/land/restart", "mode": "land"})),
    )
    .await;
    assert_eq!(status, 200, "{enqueued}");
    let job_id = enqueued["id"].as_str().unwrap().to_owned();
    wait_state(&hub, &job_id, &["landing"]).await;
    // The land child is blocked BEFORE pushing: remote is unchanged.
    assert_eq!(
        git(&origin.origin, &["rev-parse", "main"]),
        origin.base_main
    );

    // A second land job queued while the first is Landing must NOT be dispatched.
    let (status, second) = http(
        addr,
        "POST",
        &format!("/v1/projects/{project_id}/gate"),
        &dev_token,
        Some(json!({"branch": "wt/land/restart", "mode": "land"})),
    )
    .await;
    assert_eq!(status, 200, "{second}");
    let second_id = second["id"].as_str().unwrap().to_owned();
    // Bounded window: while job1 is Landing, job2 must never leave the queue.
    let window = tokio::time::Instant::now() + Duration::from_millis(1500);
    while tokio::time::Instant::now() < window {
        assert_eq!(
            job_of(&hub, &second_id).await["state"],
            json!("queued"),
            "Landing holds occupancy"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Crash + restart; reconnect.
    link.shutdown().await;
    hub.shutdown().await;
    let mut config2 = HubConfig::for_test(data_dir.clone());
    config2.listen = addr;
    let hub2 = remuda_hub::spawn(config2).await.unwrap();
    assert_eq!(job_of(&hub2, &job_id).await["state"], json!("landing"));
    let _link2 = WssLink::connect_runtime(
        WssConfig::loopback(addr, node_token, host_id.clone()),
        node.clone(),
    )
    .await
    .unwrap();

    // Across the forced post-hello probe AND at least one periodic reconciler
    // tick (5 s interval), the push is still outstanding, so the row must remain
    // Landing and main must not move.
    let window = tokio::time::Instant::now() + Duration::from_millis(6500);
    while tokio::time::Instant::now() < window {
        let still = job_of(&hub2, &job_id).await;
        assert_eq!(
            still["state"],
            json!("landing"),
            "uncertain outcome stays Landing: {still}"
        );
        assert_eq!(
            git(&origin.origin, &["rev-parse", "main"]),
            origin.base_main
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(
        job_of(&hub2, &second_id).await["state"],
        json!("queued"),
        "occupancy survives restart"
    );

    // While job1 is still Landing, cancel the queued job2: a Queued cancel
    // settles immediately and job2 can NEVER dispatch or push. This also proves
    // the second job never ran (it never leaves the queue onto the held lane).
    let (cancel_status, cancel_body) = http(
        addr,
        "POST",
        &format!("/v1/projects/{project_id}/gate/jobs/{second_id}/cancel"),
        &dev_token,
        Some(json!({})),
    )
    .await;
    assert!(
        (200..300).contains(&cancel_status),
        "{cancel_status} {cancel_body}"
    );
    wait_state(&hub2, &second_id, &["canceled"]).await;

    // The granted push now actually completes (release the blocked land child).
    std::fs::write(gate_dir.join("proceed"), b"go").unwrap();
    let settled = wait_state(&hub2, &job_id, &["landed"]).await;
    assert_eq!(settled["state"], "landed", "{settled}");

    // Exactly one push; the lane freed only after the granted push settled.
    let pushes = gate_dir.join("pushes.log");
    let push_count = std::fs::read_to_string(&pushes).unwrap().lines().count();
    assert_eq!(
        push_count, 1,
        "settled by the one granted push, never re-pushed"
    );
}

/// r13 item 2: the Node loses the carrier (and thus its gate.run reply) AFTER
/// the land grant, while the granted push has already reached the remote and the
/// land process is still alive. The Hub stays UP. The Landing row must not be
/// failed optimistically and must keep its lane while the carrier is gone; when
/// a fresh carrier session reconnects, the bounded reconciliation probes the
/// exact granted merge and settles landed exactly once (never re-pushed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn carrier_drop_after_grant_keeps_landing_until_reconnect_probe_lands() {
    let dir = tempfile::tempdir().unwrap();
    let gate_dir = dir.path().join("gatedir");
    std::fs::create_dir_all(&gate_dir).unwrap();
    let origin = build_origin(dir.path());
    let bin = write_merge_cli(dir.path());

    let data_dir = dir.path().join("hub-data");
    let port = free_port();
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));

    let node = DevNode::new(
        &DevServerConfig::loopback(0)
            .with_workspace_root(dir.path().to_path_buf())
            .with_workspace_roots(remuda_testing::test_workspace_roots!()),
    )
    .expect("dev node");
    node.gate_test_set_merge_bin(bin).await;
    let host_id = node.host().meta.id.as_id().as_str().to_owned();
    let workspace_id = node
        .workspaces()
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .meta
        .id
        .as_id()
        .as_str()
        .to_owned();

    // The Hub stays RUNNING for the whole test (no restart).
    let mut config = HubConfig::for_test(data_dir);
    config.listen = addr;
    let hub = remuda_hub::spawn(config).await.unwrap();
    let dev_token = hub.mint_device_token("h").await.unwrap();
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await
        .unwrap();
    let link = WssLink::connect_runtime(
        WssConfig::loopback(addr, enroll, host_id.clone()),
        node.clone(),
    )
    .await
    .unwrap();
    wait_host_online(addr, &dev_token, &host_id).await;
    let node_token = link.node_token.clone().unwrap();

    let body = json!({
        "name": "gate-carrier-drop",
        "members": [{"hostId": host_id, "workspaceId": workspace_id, "role": "build"}],
        "hosts": [{"hostId": host_id, "maxInstances": 8}],
        "gate": {
            "affected": true, "web": "never", "landSerialization": "global-cas",
            "lanes": [{
                "id": "lane1", "hostId": host_id,
                "repoPath": origin.lane.to_string_lossy(),
                "targetDir": dir.path().join("target").to_string_lossy(),
                "ports": "58630-58639",
                "env": {"GATE_DIR": gate_dir.to_string_lossy(), "CASE": "push_then_block",
                        "PATH": std::env::var("PATH").unwrap_or_default()},
                "lockPath": dir.path().join("gate.lock").to_string_lossy(),
                "pwEndpoint": "ws://127.0.0.1:0/"
            }]
        }
    });
    let (status, created) = http(addr, "POST", "/v1/projects", &dev_token, Some(body)).await;
    assert_eq!(status, 200, "{created}");
    let project_id = created["id"].as_str().unwrap().to_owned();

    let (status, enqueued) = http(
        addr,
        "POST",
        &format!("/v1/projects/{project_id}/gate"),
        &dev_token,
        Some(json!({"branch": "wt/land/restart", "mode": "land"})),
    )
    .await;
    assert_eq!(status, 200, "{enqueued}");
    let job_id = enqueued["id"].as_str().unwrap().to_owned();

    wait_state(&hub, &job_id, &["landing"]).await;
    let pushes = gate_dir.join("pushes.log");
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !pushes.exists() {
        assert!(tokio::time::Instant::now() < deadline, "land never pushed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let remote_after_push = git(&origin.origin, &["rev-parse", "main"]);
    assert_ne!(remote_after_push, origin.base_main);

    // A second land job is held out by the Landing occupancy.
    let (status, second) = http(
        addr,
        "POST",
        &format!("/v1/projects/{project_id}/gate"),
        &dev_token,
        Some(json!({"branch": "wt/land/restart", "mode": "land"})),
    )
    .await;
    assert_eq!(status, 200, "{second}");
    let second_id = second["id"].as_str().unwrap().to_owned();

    // Drop the carrier AFTER the grant (the granted land child survives in its
    // own process group). The gate.run reply is lost; the host goes offline.
    link.shutdown().await;
    let offline_deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let (_, hosts) = http(addr, "GET", "/v1/hosts", &dev_token, None).await;
        let online = hosts["items"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|i| i["id"] == json!(host_id) && i["online"] == json!(true))
        });
        if !online {
            break;
        }
        assert!(
            tokio::time::Instant::now() < offline_deadline,
            "host never went offline"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // While the carrier is gone the outcome is uncertain: the row STAYS Landing
    // and keeps the lane; the second job stays queued; main does not move.
    let window = tokio::time::Instant::now() + Duration::from_millis(2000);
    while tokio::time::Instant::now() < window {
        assert_eq!(
            job_of(&hub, &job_id).await["state"],
            json!("landing"),
            "must not fail a Landing whose reply was lost"
        );
        assert_eq!(
            job_of(&hub, &second_id).await["state"],
            json!("queued"),
            "lane stays held"
        );
        assert_eq!(
            git(&origin.origin, &["rev-parse", "main"]),
            remote_after_push
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Cancel the queued second job while the first is Landing: it must never run.
    let (cancel_status, cancel_body) = http(
        addr,
        "POST",
        &format!("/v1/projects/{project_id}/gate/jobs/{second_id}/cancel"),
        &dev_token,
        Some(json!({})),
    )
    .await;
    assert!(
        (200..300).contains(&cancel_status),
        "{cancel_status} {cancel_body}"
    );
    wait_state(&hub, &second_id, &["canceled"]).await;

    // A fresh carrier session reconnects (same DevNode, persistent token). The
    // bounded post-reconnect probe establishes the exact granted merge is on
    // remote main and settles landed — with no re-run/re-push.
    let _link2 =
        WssLink::connect_runtime(WssConfig::loopback(addr, node_token, host_id), node.clone())
            .await
            .unwrap();
    let settled = wait_state(&hub, &job_id, &["landed"]).await;
    assert_eq!(settled["state"], "landed", "{settled}");
    assert_eq!(
        std::fs::read_to_string(&pushes).unwrap().lines().count(),
        1,
        "the granted push is never re-sent"
    );
    assert_eq!(
        git(&origin.origin, &["rev-parse", "main"]),
        remote_after_push
    );
    let actions = hub.test_gate_audit_actions(&job_id).await.unwrap();
    assert_eq!(
        actions.iter().filter(|a| *a == "gate.landed").count(),
        1,
        "{actions:?}"
    );

    // Let the orphaned blocked land child exit cleanly (it pushed before block).
    std::fs::write(gate_dir.join("proceed"), b"go").unwrap();
}

/// r13 item 9(a) real path: a cancel that reaches the REAL DevNode during the
/// REAL verify child (a merge CLI blocked mid-gate) kills the run, settles the
/// job canceled, and — because the verify never completed — never takes the
/// land grant and never pushes. The cancel decision and the kill run through
/// production Hub + WSS + Node code, not a stub.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_cancel_during_verify_kills_the_run_and_never_grants_or_pushes() {
    let dir = tempfile::tempdir().unwrap();
    let gate_dir = dir.path().join("gatedir");
    std::fs::create_dir_all(&gate_dir).unwrap();
    let origin = build_origin(dir.path());
    let bin = write_merge_cli(dir.path());

    let data_dir = dir.path().join("hub-data");
    let port = free_port();
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));

    let node = DevNode::new(
        &DevServerConfig::loopback(0)
            .with_workspace_root(dir.path().to_path_buf())
            .with_workspace_roots(remuda_testing::test_workspace_roots!()),
    )
    .expect("dev node");
    node.gate_test_set_merge_bin(bin).await;
    let host_id = node.host().meta.id.as_id().as_str().to_owned();
    let workspace_id = node
        .workspaces()
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .meta
        .id
        .as_id()
        .as_str()
        .to_owned();

    let mut config = HubConfig::for_test(data_dir);
    config.listen = addr;
    let hub = remuda_hub::spawn(config).await.unwrap();
    let dev_token = hub.mint_device_token("h").await.unwrap();
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await
        .unwrap();
    let _link = WssLink::connect_runtime(
        WssConfig::loopback(addr, enroll, host_id.clone()),
        node.clone(),
    )
    .await
    .unwrap();
    wait_host_online(addr, &dev_token, &host_id).await;

    let body = json!({
        "name": "gate-cancel-verify",
        "members": [{"hostId": host_id, "workspaceId": workspace_id, "role": "build"}],
        "hosts": [{"hostId": host_id, "maxInstances": 8}],
        "gate": {
            "affected": true, "web": "never", "landSerialization": "global-cas",
            "lanes": [{
                "id": "lane1", "hostId": host_id,
                "repoPath": origin.lane.to_string_lossy(),
                "targetDir": dir.path().join("target").to_string_lossy(),
                "ports": "58640-58649",
                "env": {"GATE_DIR": gate_dir.to_string_lossy(), "CASE": "verify_block",
                        "PATH": std::env::var("PATH").unwrap_or_default()},
                "lockPath": dir.path().join("gate.lock").to_string_lossy(),
                "pwEndpoint": "ws://127.0.0.1:0/"
            }]
        }
    });
    let (status, created) = http(addr, "POST", "/v1/projects", &dev_token, Some(body)).await;
    assert_eq!(status, 200, "{created}");
    let project_id = created["id"].as_str().unwrap().to_owned();

    let (status, enqueued) = http(
        addr,
        "POST",
        &format!("/v1/projects/{project_id}/gate"),
        &dev_token,
        Some(json!({"branch": "wt/land/restart", "mode": "land"})),
    )
    .await;
    assert_eq!(status, 200, "{enqueued}");
    let job_id = enqueued["id"].as_str().unwrap().to_owned();
    wait_state(&hub, &job_id, &["running"]).await;

    // Wait until the REAL verify child has actually reached its blocking point.
    let verifying = gate_dir.join("verify-state.log");
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !verifying.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "verify child never blocked"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Cancel through the real HTTP route; the grant was never taken.
    let (status, cancel_body) = http(
        addr,
        "POST",
        &format!("/v1/projects/{project_id}/gate/jobs/{job_id}/cancel"),
        &dev_token,
        Some(json!({})),
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "cancel during running: {status} {cancel_body}"
    );
    let done = wait_state(&hub, &job_id, &["canceled"]).await;
    assert_eq!(done["state"], json!("canceled"), "{done}");

    // The land phase never ran (no grant/push), and origin main is unchanged.
    assert!(
        !gate_dir.join("pushes.log").exists(),
        "a canceled verify must never push"
    );
    assert_eq!(
        git(&origin.origin, &["rev-parse", "main"]),
        origin.base_main
    );

    // Releasing the blocked verify now must be harmless: the process group was
    // already killed at cancel (the marker file simply never proceeds).
    std::fs::write(gate_dir.join("proceed"), b"go").unwrap();
}
