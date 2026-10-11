//! r12 item 3 regression: a real `remuda node --stdio` LAND round trip.
//!
//! The stdio carrier's `gate.landGrant` frame used to send `hostId: null`,
//! while `GateLandGrantParams.host_id` was a required `String`, so the Hub
//! rejected the frame on typed deserialization and every real stdio land died
//! before the push. This test spawns the real stdio binary in LAND mode over
//! a real local bare origin, drives the handshake the Hub bridge drives, and:
//!   1. parses the node's `gate.landGrant` frame with the production
//!      `GateLandGrantParams` type (null hostId must deserialize; the Hub
//!      derives the host from the authenticated carrier),
//!   2. grants it,
//!   3. asserts the run reports `landed` AND main actually advanced on origin.
#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

fn git(repo: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
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

struct Fixture {
    _dir: tempfile::TempDir,
    lane: std::path::PathBuf,
    target: std::path::PathBuf,
    bin: std::path::PathBuf,
    tmp: std::path::PathBuf,
    origin: std::path::PathBuf,
    base_main: String,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let origin = dir.path().join("origin.git");
    Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(&origin)
        .status()
        .unwrap();

    let seed = dir.path().join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-b", "main"]);
    git(&seed, &["config", "user.email", "t@example.com"]);
    git(&seed, &["config", "user.name", "T"]);
    std::fs::write(seed.join("file.txt"), "base\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "init"]);
    git(&seed, &["checkout", "-q", "-b", "wt/land/task"]);
    std::fs::write(seed.join("file.txt"), "task\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "task change"]);
    git(
        &seed,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&seed, &["push", "-q", "origin", "main", "wt/land/task"]);
    let base_main = git(&seed, &["rev-parse", "main"]);

    let lane = dir.path().join("lane");
    git(
        dir.path(),
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

    // Land-capable fake merge CLI. --gate creates a REAL merge commit and
    // reports its shas (the Node pins it); --land REALLY pushes that merge to
    // origin main, so the test can assert main advanced.
    let bin = dir.path().join("fake-merge-land.sh");
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
scratch=$(mktemp -d "${TMPDIR:-/tmp}/remuda-mq-$$-0-XXXXXX")
trap 'rm -rf "$scratch"' EXIT
echo '{"name":"cargo-test","status":"ok","durationMs":5,"attempts":1,"retried":false}' > "$scratch/gate.jsonl"
git fetch -q origin
if [ "$mode" = "gate" ]; then
  git checkout -qB "$onto" "origin/$onto"
  base=$(git rev-parse "$onto")
  head=$(git rev-parse "origin/$branch")
  git merge -q --no-ff "origin/$branch" -m "gate merge $branch"
  merged=$(git rev-parse HEAD)
  printf '{"exitCode":0,"status":"verified","branch":"%s","base":"%s","head":"%s","merged":"%s","steps":[{"name":"cargo-test","status":"ok","durationMs":5}]}\n' "$branch" "$base" "$head" "$merged"
else
  merged=$(git rev-parse HEAD)
  git push -q origin "HEAD:$onto"
  printf '{"exitCode":0,"status":"landed","branch":"%s","merged":"%s","currentMain":"%s","steps":[{"name":"cargo-test","status":"ok","durationMs":5}]}\n' "$branch" "$merged" "$merged"
fi
"#;
    std::fs::write(&bin, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let target = dir.path().join("target");
    std::fs::create_dir_all(&target).unwrap();
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();

    Fixture {
        _dir: dir,
        lane,
        target,
        bin,
        tmp,
        origin,
        base_main,
    }
}

struct Node {
    child: Child,
    stdin: std::process::ChildStdin,
    rx: mpsc::Receiver<Value>,
}

impl Node {
    fn spawn(home: &std::path::Path, tmp: &std::path::Path) -> Self {
        let path = match std::env::var("PATH") {
            Ok(path) if !path.is_empty() => {
                format!("{path}:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
            }
            _ => "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into(),
        };
        let mut child = Command::new(env!("CARGO_BIN_EXE_remuda-node-stdio"))
            .args(["node", "--stdio", "--data-dir"])
            .arg(home.join("data"))
            .env("PATH", path)
            .env("HOME", home)
            .env("TMPDIR", tmp)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut lines = BufReader::new(stdout).lines();
            while let Some(Ok(line)) = lines.next() {
                if !line.trim().is_empty()
                    && let Ok(value) = serde_json::from_str::<Value>(&line)
                    && tx.send(value).is_err()
                {
                    break;
                }
            }
        });
        Self { child, stdin, rx }
    }

    fn send(&mut self, frame: &Value) {
        let mut line = serde_json::to_vec(frame).unwrap();
        line.push(b'\n');
        self.stdin.write_all(&line).unwrap();
        self.stdin.flush().unwrap();
    }

    fn wait_for(&self, deadline: Instant, pred: impl Fn(&Value) -> bool) -> Option<Value> {
        while Instant::now() < deadline {
            match self.rx.recv_timeout(deadline - Instant::now()) {
                Ok(frame) if pred(&frame) => return Some(frame),
                Ok(_) => {}
                Err(_) => return None,
            }
        }
        None
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn stdio_land_roundtrip_grant_has_null_host_and_pushes_main() {
    let home = tempfile::tempdir().unwrap();
    let fixture = fixture();
    let mut node = Node::spawn(home.path(), &fixture.tmp);
    let _hello = node
        .wait_for(Instant::now() + Duration::from_secs(10), |f| {
            f["method"] == "node.hello"
        })
        .expect("node.hello");

    node.send(&json!({
        "jsonrpc": "2.0", "id": "run-1", "method": "gate.run",
        "params": {
            "jobId": "gjb_stdio_land", "laneId": "lane1",
            "repoPath": fixture.lane.to_string_lossy(),
            "targetDir": fixture.target.to_string_lossy(),
            "branch": "wt/land/task", "baseBranch": "main",
            "mode": "land", "web": "never",
            "push": true, "generation": 1,
            "binary": fixture.bin.to_string_lossy(),
        }
    }));

    let authorize = node
        .wait_for(Instant::now() + Duration::from_secs(10), |f| {
            f["method"] == json!("gate.authorize")
        })
        .expect("gate.authorize before execution");
    node.send(&json!({
        "jsonrpc": "2.0", "id": authorize["id"].clone(),
        "result": {"authorized": true, "generation": authorize["params"]["generation"].clone()},
    }));

    // The decisive frame: the stdio carrier's landGrant. Parse it with the
    // production Hub-side type exactly as ws.rs does.
    let grant = node
        .wait_for(Instant::now() + Duration::from_secs(15), |f| {
            f["method"] == json!("gate.landGrant")
        })
        .expect("a real land run must ask gate.landGrant");
    let typed: remuda_protocol::GateLandGrantParams =
        serde_json::from_value(grant["params"].clone())
            .expect("gate.landGrant params must deserialize (hostId may be null)");
    assert_eq!(typed.job_id, "gjb_stdio_land");
    assert_eq!(
        typed.host_id, None,
        "the stdio carrier stamps no host id; the Hub uses the carrier host"
    );
    assert!(
        !typed.merge_sha.is_empty(),
        "the grant carries the verified merge"
    );
    node.send(&json!({
        "jsonrpc": "2.0", "id": grant["id"].clone(),
        "result": {"granted": true, "generation": typed.generation,
                   "landingInProgress": false},
    }));

    let reply = node
        .wait_for(Instant::now() + Duration::from_secs(20), |f| {
            f["id"] == json!("run-1")
        })
        .expect("gate.run reply");
    assert_eq!(
        reply["result"]["status"],
        json!("landed"),
        "the land child completed: {reply}"
    );

    // The push is real: origin main advanced past the seeded base and now
    // contains the task change.
    let new_main = git(&fixture.origin, &["rev-parse", "main"]);
    assert_ne!(new_main, fixture.base_main, "origin main did not advance");
    let task_file = git(&fixture.origin, &["show", "main:file.txt"]);
    assert_eq!(task_file, "task");
}
