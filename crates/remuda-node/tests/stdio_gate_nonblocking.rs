//! Regression: a long `gate.run` on the ssh-stdio carrier must not park the
//! whole request/response loop.
//!
//! On the live demo a Node enrolled over `remuda node --stdio` took a lane
//! gate — a 20–40 minute `cargo test` run — and from that frame until the run
//! ended every other read (screen, host files, journal) timed out and the Hub
//! saw the job `running` with no steps. The carrier served one frame at a time
//! and awaited the entire gate on the input arm, so the outgoing pumps beside
//! it never got a turn.
//!
//! These tests drive the real `remuda-node-stdio` binary the way
//! `stdio_shutdown.rs` does — spawn it, read NDJSON off its stdout — and prove
//! that while a ~10s gate is in flight (1) an unrelated `tty.screen` request is
//! answered within 2s, (2) `gate.event` step notifications reach the wire
//! during the run rather than all at the end, and (3) the `gate.run` reply for
//! the saved id still arrives, carrying a status, a non-empty steps array and a
//! mergeSha, with its id echoed back.
#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

/// One git invocation in `repo`, asserting success.
fn git(repo: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A bare origin plus a lane clone carrying `main` and a `wt/fake/task` branch,
/// and a fake merge binary that flushes one step, then sleeps ~10s before it
/// reports a verified merge. The sleep is what keeps the gate in flight long
/// enough to prove the carrier stays responsive.
struct Fixture {
    _dir: tempfile::TempDir,
    lane: std::path::PathBuf,
    target: std::path::PathBuf,
    bin: std::path::PathBuf,
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
    git(&seed, &["branch", "wt/fake/task"]);
    git(
        &seed,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&seed, &["push", "-q", "origin", "main", "wt/fake/task"]);

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

    // The fake merge CLI: flush one step to a `remuda-mq-*/gate.jsonl` (what the
    // real gate does, and what `stream_gate_report` tails to emit gate.event
    // steps), then sleep ~10s, then print the verdict JSON. Fake shas are fine
    // for a verify — pinning the merge only warns when the object is absent.
    let bin = dir.path().join("fake-merge.sh");
    let script = r#"#!/bin/bash
set -eu
scratch=$(mktemp -d "${TMPDIR:-/tmp}/remuda-mq-fake.XXXXXX")
echo '{"name":"secret-scan","status":"ok","durationMs":11,"attempts":1,"retried":false}' > "$scratch/gate.jsonl"
echo 'gate: secret-scan' >&2
sleep 10
cat <<'JSON'
{"exitCode":0,"status":"verified","branch":"wt/fake/task","base":"1111111111111111111111111111111111111111","head":"2222222222222222222222222222222222222222","merged":"3333333333333333333333333333333333333333","steps":[{"name":"secret-scan","status":"ok","durationMs":11},{"name":"cargo-test","status":"ok","durationMs":22}]}
JSON
"#;
    std::fs::write(&bin, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let target = dir.path().join("target");
    std::fs::create_dir_all(&target).unwrap();

    Fixture {
        _dir: dir,
        lane,
        target,
        bin,
    }
}

/// A spawned stdio Node plus a background reader turning each stdout line into a
/// JSON frame on a channel.
struct Node {
    child: Child,
    stdin: std::process::ChildStdin,
    rx: mpsc::Receiver<Value>,
    _reader: std::thread::JoinHandle<()>,
}

impl Node {
    fn spawn(home: &std::path::Path) -> Self {
        // Inherit the ambient PATH so the Node's own git pre-steps (fetch / ff)
        // use whatever git the harness provides, then guarantee the base system
        // dirs the gate child needs (bash, mktemp, sleep) are present too.
        let path = match std::env::var("PATH") {
            Ok(path) if !path.is_empty() => {
                format!("{path}:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
            }
            _ => "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_owned(),
        };
        let mut child = Command::new(env!("CARGO_BIN_EXE_remuda-node-stdio"))
            .args(["node", "--stdio", "--data-dir"])
            .arg(home.join("data"))
            .env("PATH", path)
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut lines = BufReader::new(stdout).lines();
            while let Some(Ok(line)) = lines.next() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(value) = serde_json::from_str::<Value>(&line)
                    && tx.send(value).is_err()
                {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            rx,
            _reader: reader,
        }
    }

    fn send(&mut self, frame: &Value) {
        let mut line = serde_json::to_vec(frame).unwrap();
        line.push(b'\n');
        self.stdin.write_all(&line).unwrap();
        self.stdin.flush().unwrap();
    }

    /// Read frames until `pred` returns true or the deadline passes; returns the
    /// matching frame, or None on timeout.
    fn wait_for(&self, deadline: Instant, mut pred: impl FnMut(&Value) -> bool) -> Option<Value> {
        loop {
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            match self.rx.recv_timeout(deadline - now) {
                Ok(frame) => {
                    if pred(&frame) {
                        return Some(frame);
                    }
                }
                Err(_) => return None,
            }
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn gate_run_params(fixture: &Fixture) -> Value {
    json!({
        "jobId": "gjb_test",
        "laneId": "lane1",
        "repoPath": fixture.lane.to_string_lossy(),
        "targetDir": fixture.target.to_string_lossy(),
        "branch": "wt/fake/task",
        "baseBranch": "main",
        "mode": "verify",
        "web": "never",
        "binary": fixture.bin.to_string_lossy(),
    })
}

/// Drive the hello handshake, start a gate, and return the started Node so the
/// caller can prove the carrier stays live while the gate runs.
fn start_gate(home: &std::path::Path, fixture: &Fixture) -> Node {
    let mut node = Node::spawn(home);
    let hello = node
        .wait_for(Instant::now() + Duration::from_secs(10), |frame| {
            frame["method"] == json!("node.hello")
        })
        .expect("stdio Node sent its node.hello");
    assert_eq!(hello["method"], "node.hello");

    node.send(&json!({
        "jsonrpc": "2.0",
        "id": "gate-run-1",
        "method": "gate.run",
        "params": gate_run_params(fixture),
    }));
    node
}

#[test]
fn tty_screen_is_answered_while_a_gate_runs() {
    let home = tempfile::tempdir().unwrap();
    let fixture = fixture();
    let mut node = start_gate(home.path(), &fixture);

    // Let the gate get parked in its ~10s sleep, then ask something unrelated.
    // Before the fix this request never came back until the whole gate ended.
    std::thread::sleep(Duration::from_millis(500));
    node.send(&json!({
        "jsonrpc": "2.0",
        "id": "screen-1",
        "method": "tty.screen",
        "params": { "instanceId": "ins_00000000-0000-7000-8000-000000000000" },
    }));

    let reply = node
        .wait_for(Instant::now() + Duration::from_secs(2), |frame| {
            frame["id"] == json!("screen-1")
        })
        .expect("tty.screen must be answered within 2s while the gate runs");
    // Any reply (result or a bounded error for the unknown instance) proves the
    // carrier is live; it must not be the gate's own reply.
    assert_eq!(reply["id"], "screen-1");
    assert_ne!(reply["id"], "gate-run-1");
}

#[test]
fn gate_event_steps_reach_the_wire_during_the_run() {
    let home = tempfile::tempdir().unwrap();
    let fixture = fixture();
    let node = start_gate(home.path(), &fixture);

    // The fake CLI flushes its first step immediately, then sleeps ~10s. A
    // gate.event step frame arriving well before that sleep ends is the proof
    // that the outgoing pump is polled while the gate future is in flight.
    let event = node
        .wait_for(Instant::now() + Duration::from_secs(5), |frame| {
            frame["method"] == json!("gate.event") && frame["params"]["kind"] == json!("step")
        })
        .expect("a gate.event step must appear during the run, not only at the end");
    assert_eq!(event["params"]["jobId"], "gjb_test");
    assert_eq!(event["params"]["step"]["name"], "secret-scan");
}

#[test]
fn gate_run_reply_carries_the_full_result() {
    let home = tempfile::tempdir().unwrap();
    let fixture = fixture();
    let node = start_gate(home.path(), &fixture);

    // The saved-id reply still arrives when the run ends (the fixture sleeps
    // ~10s), and it deserializes into a GateRunResult with the verdict, a
    // non-empty steps array and the merge sha.
    let reply = node
        .wait_for(Instant::now() + Duration::from_secs(30), |frame| {
            frame["id"] == json!("gate-run-1") && frame.get("result").is_some()
        })
        .expect("the gate.run reply for the saved id must arrive when the run ends");
    assert_eq!(reply["id"], "gate-run-1");

    let result: remuda_protocol::GateRunResult =
        serde_json::from_value(reply["result"].clone()).expect("reply is a GateRunResult");
    assert_eq!(result.status, "passed", "{result:?}");
    assert!(
        !result.steps.is_empty(),
        "steps must be non-empty: {result:?}"
    );
    assert_eq!(
        result.merge_sha.as_deref(),
        Some("3333333333333333333333333333333333333333"),
        "the reply carries the verified merge sha",
    );
}
