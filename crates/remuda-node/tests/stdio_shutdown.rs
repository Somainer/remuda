//! Regression: SIGTERM must finish even while an SSH client holds stdin open.
#![cfg(unix)]

use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

#[test]
fn sigterm_with_open_stdin_exits_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_remuda-node-stdio"))
        .args(["node", "--stdio", "--data-dir"])
        .arg(dir.path())
        // No model CLI is needed to test the transport/runtime shutdown boundary.
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(result.map(|_| line));
    });
    let hello = rx.recv_timeout(Duration::from_secs(10));
    if hello.is_err() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("stdio Node did not send hello");
    }
    let hello: serde_json::Value = serde_json::from_str(&hello.unwrap().unwrap()).unwrap();
    assert_eq!(hello["method"], "node.hello");
    assert!(
        Command::new("/bin/kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "Node did not exit cleanly: {status}");
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Node shutdown waited for an open stdin pipe");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(stdin);
    reader.join().unwrap();
}
