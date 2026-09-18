//! Regression: no long carrier method — not just a gate — may park the
//! ssh-stdio request/response loop.
//!
//! Follow-up to `stdio_gate_nonblocking.rs`. Live evidence showed the same
//! freeze from methods with no gate running at all: a `remuda retire`
//! (`worker.remove`) whose git/filesystem reclaim ran on the loop's own task
//! parked the Node for good, and retiring or resuming a worker whose instance
//! is not on this Node blocked the loop instead of answering promptly. The fix
//! spawns every long carrier method (the gate family, worker.provision/remove,
//! instance.close, worktree/SCM reads) off the read arm and rides its reply
//! back through the outgoing channel, so cheap requests keep flowing and a
//! stuck reclaim can never own the loop.
//!
//! These drive the real `remuda-node-stdio` binary the way
//! `stdio_gate_nonblocking.rs` does and prove: (1) an unknown-worker
//! `worker.remove` answers within 2s and a `tty.screen` sent during it also
//! answers within 2s; (2) a retire whose `git worktree remove` is made slow by
//! a fixture does not delay an unrelated request beyond that bound, and its own
//! reply still arrives with the real result; (3) an `instance.close` and a
//! `worker.remove` for an instance that is not on this Node both answer
//! not-found promptly instead of hanging.
#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

/// One git invocation in `repo`, asserting success.
fn git(repo: &Path, args: &[&str]) -> String {
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

/// Absolute path of the real git on the ambient PATH, resolved before any test
/// shim shadows it.
fn real_git() -> PathBuf {
    let out = Command::new("bash")
        .args(["-lc", "command -v git"])
        .output()
        .unwrap();
    assert!(out.status.success(), "could not locate git");
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
}

/// A registered workspace: a git repo with a fetchable origin (so
/// `worker.provision` can `git fetch origin` and branch from `origin/main`),
/// plus a `git` shim directory that sleeps before a destructive
/// `worktree remove` and delegates everything else to the real git.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    shim_dir: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "T"]);
    std::fs::write(repo.join("file.txt"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    // provision fetches origin; point origin at the repo itself so the fetch
    // succeeds in a temp fixture.
    git(&repo, &["remote", "add", "origin", repo.to_str().unwrap()]);
    git(&repo, &["fetch", "-q", "origin"]);
    git(
        &repo,
        &["update-ref", "refs/remotes/origin/main", "refs/heads/main"],
    );

    // A `git` shim that sleeps ~5s before `git -C <repo> worktree remove …` and
    // otherwise execs the real git. That is the one destructive step of a
    // retire's reclaim; slowing it proves the reclaim runs off the carrier loop.
    let shim_dir = root.join("shim-bin");
    std::fs::create_dir_all(&shim_dir).unwrap();
    let shim = shim_dir.join("git");
    let real = real_git();
    let script = format!(
        "#!/bin/bash\n\
         real={real}\n\
         if [ \"$1\" = \"-C\" ] && [ \"$3\" = \"worktree\" ] && [ \"$4\" = \"remove\" ]; then\n\
         \x20 sleep 5\n\
         fi\n\
         if [ \"$1\" = \"worktree\" ] && [ \"$2\" = \"remove\" ]; then\n\
         \x20 sleep 5\n\
         fi\n\
         exec \"$real\" \"$@\"\n",
        real = real.display()
    );
    std::fs::write(&shim, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();

    Fixture {
        _dir: dir,
        root,
        repo,
        shim_dir,
    }
}

/// A spawned stdio Node plus a background reader turning each stdout line into a
/// JSON frame on a channel.
struct Node {
    child: Child,
    stdin: std::process::ChildStdin,
    rx: mpsc::Receiver<Value>,
    /// Frames already read off the wire that no assertion has claimed yet.
    ///
    /// Replies to two requests sent back to back arrive in whichever order the
    /// Node finishes them, and the Node interleaves pumps with replies. Dropping
    /// a non-matching frame here would lose a reply the next assertion is
    /// waiting for — and the failure would read as "the carrier stopped
    /// answering" when it had answered. Hold them instead.
    held: Vec<Value>,
    _reader: std::thread::JoinHandle<()>,
}

impl Node {
    /// Spawn the stdio Node with `fixture.repo` registered as its workspace. If
    /// `shim` is set, the git shim directory leads PATH so the retire reclaim's
    /// `worktree remove` is slow.
    fn spawn(home: &Path, fixture: &Fixture, shim: bool) -> Self {
        let base = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
        let ambient = std::env::var("PATH").unwrap_or_default();
        let mut path = if ambient.is_empty() {
            base.to_owned()
        } else {
            format!("{ambient}:{base}")
        };
        if shim {
            path = format!("{}:{path}", fixture.shim_dir.display());
        }
        let mut child = Command::new(env!("CARGO_BIN_EXE_remuda-node-stdio"))
            .args(["node", "--stdio", "--data-dir"])
            .arg(home.join("data"))
            .arg("--workspace")
            .arg(&fixture.repo)
            .arg("--workspace-root")
            .arg(&fixture.root)
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
            held: Vec::new(),
        }
    }

    fn send(&mut self, frame: &Value) {
        let mut line = serde_json::to_vec(frame).unwrap();
        line.push(b'\n');
        self.stdin.write_all(&line).unwrap();
        self.stdin.flush().unwrap();
    }

    fn wait_for(
        &mut self,
        deadline: Instant,
        mut pred: impl FnMut(&Value) -> bool,
    ) -> Option<Value> {
        // Anything already read and not yet claimed is fair game first, so a
        // reply that arrived while the previous assertion was looking for a
        // different frame is not silently lost.
        if let Some(index) = self.held.iter().position(&mut pred) {
            return Some(self.held.remove(index));
        }
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
                    self.held.push(frame);
                }
                Err(_) => return None,
            }
        }
    }

    /// Wait for the Node's `node.hello`, proving it is serving frames.
    fn await_hello(&mut self) {
        let hello = self
            .wait_for(Instant::now() + Duration::from_secs(10), |frame| {
                frame["method"] == json!("node.hello")
            })
            .expect("stdio Node sent its node.hello");
        assert_eq!(hello["method"], "node.hello");
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const UNKNOWN_INSTANCE: &str = "ins_00000000-0000-7000-8000-000000000000";

#[test]
fn worker_remove_and_a_concurrent_screen_both_answer_within_two_seconds() {
    let home = tempfile::tempdir().unwrap();
    let fixture = fixture();
    let mut node = Node::spawn(home.path(), &fixture, false);
    node.await_hello();

    // A retire of a worker this Node never provisioned. It must not hang on any
    // wait; both this and an unrelated screen answer within 2s.
    node.send(&json!({
        "jsonrpc": "2.0",
        "id": "remove-unknown",
        "method": "worker.remove",
        "params": { "name": "c-nope" },
    }));
    node.send(&json!({
        "jsonrpc": "2.0",
        "id": "screen-1",
        "method": "tty.screen",
        "params": { "instanceId": UNKNOWN_INSTANCE },
    }));

    let screen = node
        .wait_for(Instant::now() + Duration::from_secs(2), |frame| {
            frame["id"] == json!("screen-1")
        })
        .expect("tty.screen must answer within 2s");
    assert_eq!(screen["id"], "screen-1");
    // Its own bound, not what is left of the first request's: each must be
    // prompt on its own, which is the property the fix is about.
    let removed = node
        .wait_for(Instant::now() + Duration::from_secs(2), |frame| {
            frame["id"] == json!("remove-unknown")
        })
        .expect("worker.remove for an unknown worker must answer within 2s");
    // Unknown worker: nothing removed, but a real (non-hanging) reply.
    assert_eq!(removed["id"], "remove-unknown");
    assert_eq!(removed["result"]["worktreeRemoved"], json!(false));
}

#[test]
fn a_slow_retire_reclaim_does_not_delay_an_unrelated_request() {
    let home = tempfile::tempdir().unwrap();
    let fixture = fixture();
    // Shim on PATH: the reclaim's `git worktree remove` sleeps ~5s.
    let mut node = Node::spawn(home.path(), &fixture, true);
    node.await_hello();

    // Provision a real worktree so the retire actually shells out to the slow
    // `git worktree remove`.
    node.send(&json!({
        "jsonrpc": "2.0",
        "id": "provision-1",
        "method": "worker.provision",
        "params": { "name": "c-slow", "branch": "wt/c-slow/task" },
    }));
    let provisioned = node
        .wait_for(Instant::now() + Duration::from_secs(30), |frame| {
            frame["id"] == json!("provision-1")
        })
        .expect("worker.provision replies");
    assert!(
        provisioned["result"]["worktreePath"].is_string(),
        "provision produced a worktree: {provisioned}"
    );

    // Start the slow retire, then immediately ask something unrelated.
    node.send(&json!({
        "jsonrpc": "2.0",
        "id": "remove-slow",
        "method": "worker.remove",
        "params": { "name": "c-slow" },
    }));
    std::thread::sleep(Duration::from_millis(300));
    node.send(&json!({
        "jsonrpc": "2.0",
        "id": "screen-2",
        "method": "tty.screen",
        "params": { "instanceId": UNKNOWN_INSTANCE },
    }));

    // The unrelated request must come back well inside the reclaim's sleep.
    let screen = node
        .wait_for(Instant::now() + Duration::from_secs(2), |frame| {
            frame["id"] == json!("screen-2")
        })
        .expect("an unrelated request must answer within 2s while the reclaim runs");
    assert_eq!(screen["id"], "screen-2");

    // The retire's own reply still arrives, carrying the real result.
    let removed = node
        .wait_for(Instant::now() + Duration::from_secs(30), |frame| {
            frame["id"] == json!("remove-slow") && frame.get("result").is_some()
        })
        .expect("the slow worker.remove reply must still arrive with its real result");
    assert_eq!(
        removed["result"]["worktreeRemoved"],
        json!(true),
        "the reclaim really removed the provisioned worktree: {removed}"
    );
}

#[test]
fn close_and_retire_of_an_instance_not_on_this_node_answer_not_found_promptly() {
    let home = tempfile::tempdir().unwrap();
    let fixture = fixture();
    let mut node = Node::spawn(home.path(), &fixture, false);
    node.await_hello();

    node.send(&json!({
        "jsonrpc": "2.0",
        "id": "close-1",
        "method": "instance.close",
        "params": { "instanceId": UNKNOWN_INSTANCE },
    }));
    node.send(&json!({
        "jsonrpc": "2.0",
        "id": "remove-2",
        "method": "worker.remove",
        "params": { "name": "c-ghost", "instanceId": UNKNOWN_INSTANCE },
    }));

    // Each request gets its own bound: each must be prompt on its own, which is
    // the property under test, rather than sharing one window between them.
    let close = node
        .wait_for(Instant::now() + Duration::from_secs(2), |frame| {
            frame["id"] == json!("close-1")
        })
        .expect("instance.close for an unknown instance must answer promptly");
    assert!(
        close.get("error").is_some(),
        "closing an instance not on this Node is a prompt error, not a wait: {close}"
    );
    let removed = node
        .wait_for(Instant::now() + Duration::from_secs(2), |frame| {
            frame["id"] == json!("remove-2")
        })
        .expect("worker.remove for an unknown instance must answer promptly");
    assert!(
        removed.get("error").is_some(),
        "retiring an instance not on this Node is a prompt not-found, not a wait: {removed}"
    );

    // And the carrier is still there afterwards. Serving a request for an
    // instance this Node does not know used to be fatal: the journal pump's
    // `?` unwound the stdio loop and the process exited, so every later request
    // hung and journals stopped reaching the Hub — the live freeze, with no
    // gate running and no busy thread.
    node.send(&json!({
        "jsonrpc": "2.0",
        "id": "after-1",
        "method": "host.resources",
        "params": {},
    }));
    let after = node
        .wait_for(Instant::now() + Duration::from_secs(2), |frame| {
            frame["id"] == json!("after-1")
        })
        .expect("the carrier must still answer after an unknown-instance request");
    assert!(
        after.get("result").is_some(),
        "the carrier is still serving: {after}"
    );
}
