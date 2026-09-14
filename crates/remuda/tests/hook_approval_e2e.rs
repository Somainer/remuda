//! End-to-end adjudication: real `remuda hook emit` parked on a real
//! `SignalBus`, answered through the interaction handle, reading the decision
//! the harness gets (D-028 §4.4 tier A, P5).
//!
//! Unlike `hook_relay_cli.rs`, the sink here is the production bus, not a
//! recorder. So one test covers the whole path: the relay forwards the
//! recorded `PermissionRequest`, the bus opens the approval card and parks the
//! hook, a device answers the card, and the relay prints the nested decision
//! measured to land on claude 2.1.221. A second test leaves it unanswered and
//! asserts the timeout deny.
#![cfg(unix)]

use remuda_protocol::{
    Completeness, HostId, Id, InstanceId, InteractionAnswer, InteractionCarrier,
    ObservationPayload, RunId, SourceChannel,
};
use remuda_signal::{BusContext, HookServer, SignalBus};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;

/// The recorded real-claude `PermissionRequest` payload (tool, real input, and
/// the acceptEdits suggestion), read from the same fixture the mapper tests
/// use.
fn permission_request_stdin() -> String {
    for line in remuda_testing::hook_session_fixture().lines() {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        if value["event"] == "PermissionRequest" {
            return value["payload"].to_string();
        }
    }
    panic!("the recorded session must contain a PermissionRequest");
}

/// Bind a real bus to a socket and hand back the server, the bus, and the
/// observation channel.
fn bus_on_socket(
    dir: &tempfile::TempDir,
) -> (
    HookServer,
    Arc<SignalBus>,
    mpsc::Receiver<remuda_protocol::Observation>,
) {
    let (tx, rx) = mpsc::channel(64);
    let bus: Arc<SignalBus> = Arc::new(
        SignalBus::new(
            BusContext {
                instance_id: InstanceId::new(),
                host_id: HostId::new(),
                journal_id: Id::new("obj").unwrap(),
                run_id: RunId::new(),
                driver_kind: remuda_protocol::DriverKind::ShellPty,
                adapter_version: "test".into(),
            },
            tx,
            Arc::new(AtomicU64::new(0)),
        )
        // A short wait so the timeout test finishes in milliseconds.
        .with_blocking_wait(std::time::Duration::from_millis(400)),
    );
    let server = HookServer::bind(
        &dir.path().join("hook.sock"),
        "cred-p5".to_owned(),
        Arc::clone(&bus) as Arc<dyn remuda_signal::SignalSink>,
    )
    .unwrap();
    (server, bus, rx)
}

/// Spawn the real relay and feed it the recorded payload on stdin.
fn spawn_relay(socket: &std::path::Path, timeout_ms: u64) -> tokio::process::Child {
    Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args([
            "hook",
            "emit",
            "--socket",
            &socket.to_string_lossy(),
            "--event",
            "PermissionRequest",
            "--credential",
            "cred-p5",
            "--timeout-ms",
            &timeout_ms.to_string(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn relay")
}

/// The card the bus emits for the recorded request.
struct Card {
    id: remuda_protocol::InteractionId,
    digest: remuda_protocol::Digest,
}

/// Pull the approval card off the journal the bus emits.
async fn await_card(rx: &mut mpsc::Receiver<remuda_protocol::Observation>) -> Card {
    loop {
        let observation = rx.recv().await.expect("an observation");
        // The card must be a hook-channel, structured, blocking approval.
        assert_eq!(observation.source.channel, SourceChannel::Hook);
        assert_eq!(observation.completeness, Completeness::Structured);
        if let ObservationPayload::InteractionRequested(payload) = observation.body {
            let interaction = payload.interaction;
            assert_eq!(interaction.kind, remuda_protocol::InteractionKind::Approval);
            assert_eq!(interaction.carrier, InteractionCarrier::HarnessHook);
            assert!(interaction.blocking && interaction.answerable);
            let request = match interaction.request {
                remuda_protocol::InteractionRequest::Approval(a) => *a,
                _ => panic!("approval payload"),
            };
            // The card shows the real tool, not a generic name.
            assert_eq!(request.title, "Write");
            assert!(request.description.contains("probe.txt"));
            // It offers the suggestion as its own button and a deny button.
            let ids: Vec<_> = request.options.iter().map(|o| o.id.as_str()).collect();
            assert_eq!(ids, vec!["allow-once", "allow-always-0", "deny"]);
            return Card {
                id: interaction.meta.id,
                digest: request.input_digest,
            };
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_human_allow_returns_the_decision_that_actually_lands() {
    let dir = tempfile::tempdir().unwrap();
    let (server, bus, mut rx) = bus_on_socket(&dir);

    let mut child = spawn_relay(server.path(), 10_000);
    child
        .stdin
        .take()
        .unwrap()
        .write_all(permission_request_stdin().as_bytes())
        .await
        .unwrap();

    // The card reaches the journal; answer it through the same handle devices
    // use, with the digest the card carried.
    let card = await_card(&mut rx).await;

    bus.resolve_answer(
        &card.id,
        &InteractionAnswer::Approval(Box::new(remuda_protocol::ApprovalAnswer {
            option_id: "allow-once".into(),
            input_digest: card.digest,
        })),
    );

    let output = child.wait_with_output().await.expect("relay exits");
    assert_eq!(output.status.code(), Some(0));
    let stdout: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).unwrap();
    // The one shape measured to apply on claude 2.1.221. A bare top-level
    // `behavior` would mean the silent-drop regression.
    assert_eq!(
        stdout,
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {"behavior": "allow"}
            }
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_permission_request_denies_on_timeout() {
    // §4.4 fail closed, end to end through the real relay and bus.
    let dir = tempfile::tempdir().unwrap();
    let (server, bus, mut rx) = bus_on_socket(&dir);

    let mut child = spawn_relay(server.path(), 10_000);
    child
        .stdin
        .take()
        .unwrap()
        .write_all(permission_request_stdin().as_bytes())
        .await
        .unwrap();
    let _card = await_card(&mut rx).await;
    // Deliberately never answer; the bus's 400 ms wait denies.
    let _ = bus;

    let output = child.wait_with_output().await.expect("relay exits");
    assert_eq!(output.status.code(), Some(0));
    let stdout: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).unwrap();
    assert_eq!(
        stdout["hookSpecificOutput"]["hookEventName"],
        "PermissionRequest"
    );
    assert_eq!(
        stdout["hookSpecificOutput"]["decision"]["behavior"], "deny",
        "an unanswered approval must fail closed, not run or hang"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_after_the_hook_gave_up_is_reported_abandoned() {
    // The honesty gate: an answer that reaches no live hook must not be called
    // applied. This is the confined/ignored trigger the screen fallback is
    // armed by.
    let dir = tempfile::tempdir().unwrap();
    let (server, bus, mut rx) = bus_on_socket(&dir);

    let mut child = spawn_relay(server.path(), 10_000);
    child
        .stdin
        .take()
        .unwrap()
        .write_all(permission_request_stdin().as_bytes())
        .await
        .unwrap();
    let card = await_card(&mut rx).await;
    // Let the 400 ms wait expire and the relay collect its deny.
    let output = child.wait_with_output().await.expect("relay exits");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(String::from_utf8_lossy(&output.stdout).trim())
            .unwrap()["hookSpecificOutput"]["decision"]["behavior"],
        "deny"
    );
    // Now a late human allow arrives: there is no waiter, so it is abandoned,
    // never a false success.
    let outcome = bus.resolve_answer(
        &card.id,
        &InteractionAnswer::Approval(Box::new(remuda_protocol::ApprovalAnswer {
            option_id: "allow-once".into(),
            input_digest: card.digest,
        })),
    );
    assert_eq!(outcome, remuda_signal::Outcome::Abandoned);
}
