//! `remuda hook emit` against a real hook socket (D-028 §4.2).
//!
//! The relay is the process a harness hook actually runs, so these drive the
//! built binary rather than calling into the library: stdin in, stdout out,
//! exit code checked. What they protect is the promise that a hook can never
//! break the agent a human is typing into — every failure mode still prints a
//! usable `{}` and exits 0.
#![cfg(unix)]

use remuda_driver::{OverlayOptions, TuiMode, materialize_overlay};
use remuda_signal::{HookEnvelope, HookReply, HookServer, SignalSink};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Default)]
struct Recorder {
    seen: Mutex<Vec<HookEnvelope>>,
    reply: Mutex<HookReply>,
}

impl SignalSink for Recorder {
    fn deliver(
        &self,
        envelope: HookEnvelope,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = HookReply> + Send + '_>> {
        let reply = self.reply.lock().unwrap().clone();
        self.seen.lock().unwrap().push(envelope);
        Box::pin(async move { reply })
    }
}

struct Relay {
    stdout: String,
    code: i32,
}

async fn relay(socket: &std::path::Path, event: &str, credential: &str, stdin: &str) -> Relay {
    let mut child = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args([
            "hook",
            "emit",
            "--socket",
            &socket.to_string_lossy(),
            "--event",
            event,
            "--credential",
            credential,
            // Keep a failing test to seconds rather than the broker TTL.
            "--timeout-ms",
            "4000",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn relay");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .await
        .expect("write payload");
    let output = child.wait_with_output().await.expect("relay exits");
    Relay {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        code: output.status.code().unwrap_or(-1),
    }
}

const SESSION_START: &str = r#"{"session_id":"0199a1f0-0000-7000-8000-000000000000",
        "transcript_path":"/w/s.jsonl","cwd":"/w","hook_event_name":"SessionStart"}"#;

#[test]
fn the_blocking_wait_still_matches_the_interaction_brokers_ttl() {
    // These are two constants in two crates: remuda-signal cannot depend on
    // remuda-driver without a cycle, so the alignment is asserted here, where
    // both are in scope. If the broker's TTL moves and this does not, a hook
    // gives up before the broker retires the ticket and a decision the user
    // *did* make becomes a silent fallback.
    assert_eq!(
        remuda_signal::BLOCKING_WAIT,
        remuda_driver::interaction::DEFAULT_TTL,
        "a blocking hook must not give up before the broker would"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_relay_forwards_stdin_and_prints_the_nodes_reply() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("hook.sock");
    let recorder = Arc::new(Recorder::default());
    let server = HookServer::bind(
        &socket,
        "cred-a".into(),
        Arc::clone(&recorder) as Arc<dyn SignalSink>,
    )
    .unwrap();

    let result = relay(server.path(), "SessionStart", "cred-a", SESSION_START).await;
    assert_eq!(result.code, 0);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(result.stdout.trim()).unwrap(),
        serde_json::json!({}),
        "P1 observes only, so the harness must read no opinion"
    );

    let seen = recorder.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].event, "SessionStart");
    assert_eq!(
        seen[0].payload["session_id"], "0199a1f0-0000-7000-8000-000000000000",
        "the payload must arrive unmodified"
    );
    assert!(
        seen[0].ppid > 0,
        "the relay must report the agent pid the Node binds the session on"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_generated_shell_hook_reports_the_harness_parent_pid() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let server = HookServer::bind(
        &dir.path().join("hook.sock"),
        "cred-shell".into(),
        Arc::clone(&recorder) as Arc<dyn SignalSink>,
    )
    .unwrap();
    let overlay = materialize_overlay(&OverlayOptions {
        launch_dir: dir.path().join("launch"),
        relay_binary: env!("CARGO_BIN_EXE_remuda").into(),
        socket_path: server.path().to_path_buf(),
        tui: TuiMode::Default,
        base: None,
    })
    .unwrap();
    let settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(overlay.path).unwrap()).unwrap();
    let command = settings["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    // A trailing builtin prevents optional final-command exec optimization.
    // The generated command itself must replace the interpreter, as required
    // by shells such as Linux dash, or its PPID is a short-lived shell PID.
    let mut child = Command::new("/bin/sh")
        .args(["-c", &format!("{command}\n:")])
        .env("REMUDA_HOOK_CREDENTIAL", "cred-shell")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(SESSION_START.as_bytes())
        .await
        .unwrap();
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("shell hook must finish")
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let seen = recorder.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].event, "SessionStart");
    assert_eq!(
        seen[0].ppid,
        i32::try_from(std::process::id()).unwrap(),
        "the hook envelope must bind to the harness, not an intermediate shell"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_decision_from_the_node_is_printed_verbatim_for_the_harness() {
    // P5 will answer PermissionRequest this way; the transport has to carry a
    // decision now so that change is a decision change and not a wire change.
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("hook.sock");
    let recorder = Arc::new(Recorder::default());
    *recorder.reply.lock().unwrap() = HookReply {
        decision: Some(serde_json::json!({"behavior": "deny", "message": "no"})),
    };
    let server = HookServer::bind(
        &socket,
        "cred-a".into(),
        Arc::clone(&recorder) as Arc<dyn SignalSink>,
    )
    .unwrap();

    let result = relay(server.path(), "PermissionRequest", "cred-a", "{}").await;
    assert_eq!(result.code, 0);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(result.stdout.trim()).unwrap(),
        serde_json::json!({"behavior": "deny", "message": "no"})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_node_still_leaves_the_agent_with_a_usable_answer() {
    // The socket is gone — a purged or restarted Node. The agent must fall
    // back to its own prompt rather than see a hook error.
    let dir = tempfile::tempdir().unwrap();
    let result = relay(&dir.path().join("absent.sock"), "Stop", "cred-a", "{}").await;
    assert_eq!(result.code, 0, "a hook must not fail the turn");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(result.stdout.trim()).unwrap(),
        serde_json::json!({})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_credential_is_refused_without_reaching_the_journal() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("hook.sock");
    let recorder = Arc::new(Recorder::default());
    let server = HookServer::bind(
        &socket,
        "cred-a".into(),
        Arc::clone(&recorder) as Arc<dyn SignalSink>,
    )
    .unwrap();

    let result = relay(server.path(), "SessionStart", "wrong-cred", SESSION_START).await;
    assert_eq!(result.code, 0);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(result.stdout.trim()).unwrap(),
        serde_json::json!({})
    );
    assert!(
        recorder.seen.lock().unwrap().is_empty(),
        "an unauthenticated event must never be journaled"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_payload_is_forwarded_as_an_empty_object_rather_than_failing() {
    // Whatever the harness sent is the harness's business; the relay's job is
    // to keep the turn alive.
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("hook.sock");
    let recorder = Arc::new(Recorder::default());
    let server = HookServer::bind(
        &socket,
        "cred-a".into(),
        Arc::clone(&recorder) as Arc<dyn SignalSink>,
    )
    .unwrap();

    let result = relay(server.path(), "Notification", "cred-a", "not json at all").await;
    assert_eq!(result.code, 0);
    assert_eq!(
        recorder.seen.lock().unwrap()[0].payload,
        serde_json::json!({})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_credential_can_come_from_the_environment_so_it_stays_out_of_argv() {
    // The overlay puts it in the child's env: a credential on the command line
    // is visible in `ps` to every process on the machine.
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("hook.sock");
    let recorder = Arc::new(Recorder::default());
    let server = HookServer::bind(
        &socket,
        "cred-env".into(),
        Arc::clone(&recorder) as Arc<dyn SignalSink>,
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args([
            "hook",
            "emit",
            "--socket",
            &server.path().to_string_lossy(),
            "--event",
            "Stop",
            "--timeout-ms",
            "4000",
        ])
        .env("REMUDA_HOOK_CREDENTIAL", "cred-env")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn relay");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"{}")
        .await
        .unwrap();
    let output = child.wait_with_output().await.unwrap();
    assert!(output.status.success());
    assert_eq!(
        recorder.seen.lock().unwrap().len(),
        1,
        "the environment credential must authenticate"
    );
}
