//! Process tests against `fake_claude.py` (no model).

use remuda_claude_wire::{
    ClaudeProcess, ControlSuccessPayload, Outbound, PermissionResult, SpawnSpec, SystemMessage,
    UserContent,
};
use std::path::PathBuf;
use std::time::Duration;

fn spec_with(script: &str, env: &[(&str, &str)]) -> SpawnSpec {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(script);
    let mut spec = SpawnSpec {
        binary: PathBuf::from("python3"),
        cwd: std::env::temp_dir(),
        raw_argv: Some(vec!["-u".into(), script.to_string_lossy().into_owned()]),
        handshake_timeout: Duration::from_secs(5),
        ..SpawnSpec::default()
    };
    spec.env = env
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();
    spec
}

fn spec() -> SpawnSpec {
    spec_with("fake_claude.py", &[])
}

/// Drain the handshake frames (init/keepalive/success) from a fresh spawn.
async fn drain_handshake(rx: &mut tokio::sync::mpsc::Receiver<Outbound>) {
    for _ in 0..8 {
        let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("msg");
        if let Outbound::ControlResponse(_) = msg {
            return;
        }
    }
    panic!("handshake never completed");
}

#[tokio::test]
async fn handshake_forwards_other_frames_then_two_results() {
    let (tx, mut rx, mut process) = ClaudeProcess::spawn(spec()).await.expect("spawn");

    let mut saw_init = false;
    let mut saw_keepalive = false;
    let mut saw_handshake = false;
    for _ in 0..8 {
        let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("msg");
        match msg {
            Outbound::System(SystemMessage::Init(_)) => saw_init = true,
            Outbound::KeepAlive => saw_keepalive = true,
            Outbound::ControlResponse(env) => {
                assert!(matches!(
                    env.response,
                    remuda_claude_wire::ControlResponse::Success { .. }
                ));
                saw_handshake = true;
                break;
            }
            other => panic!("unexpected during handshake replay: {other:?}"),
        }
    }
    assert!(saw_init && saw_keepalive && saw_handshake);

    process
        .send_user(UserContent::Text("touch x".into()))
        .await
        .expect("user");

    let request_id = loop {
        let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("msg");
        if let Some((id, req)) = msg.as_can_use_tool() {
            assert_eq!(req.tool_name, "Bash");
            let input = req.input.clone();
            let id = id.to_owned();
            process
                .respond_control(
                    &id,
                    ControlSuccessPayload::Permission(PermissionResult::Allow {
                        updated_input: input,
                        updated_permissions: None,
                    }),
                )
                .await
                .expect("allow");
            break id;
        }
    };
    assert_eq!(request_id, "perm-1");

    let mut results = Vec::new();
    let mut unknown_type = false;
    let mut unknown_subtype = false;
    while results.len() < 2 || !unknown_type || !unknown_subtype {
        let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("msg");
        match msg {
            Outbound::Result(result) => results.push(result),
            Outbound::Unknown(value) => {
                assert_eq!(value["type"], "brand_new_future_event");
                unknown_type = true;
            }
            Outbound::System(SystemMessage::Unknown(value)) => {
                assert_eq!(value["subtype"], "brand_new_subtype");
                unknown_subtype = true;
            }
            Outbound::User(_) | Outbound::System(SystemMessage::TaskStarted(_)) => {}
            other => panic!("unexpected post-allow frame: {other:?}"),
        }
    }
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].result_index, Some(0));
    assert_eq!(results[1].result_index, Some(1));
    drop(tx);
    process.close_stdin().await.expect("close");
    let status = tokio::time::timeout(Duration::from_secs(5), process.wait())
        .await
        .expect("wait timeout")
        .expect("wait");
    assert!(status.success());
}

/// Read the FIFO marker file the order-peer appends one marker per inbound
/// frame in read order.
fn read_markers(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Poll the marker file until it contains `wanted` lines (or a deadline): the
/// peer writes markers as it READS, which can lag the parent's write ack.
async fn wait_for_markers(path: &std::path::Path, wanted: usize) -> Vec<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let markers = read_markers(path);
        if markers.len() >= wanted || std::time::Instant::now() >= deadline {
            return markers;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Wait until `path` exists (a tiny Python peer signaling readiness).
async fn wait_for_file(path: &std::path::Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if path.exists() {
            // The peer closes fd 0 immediately before writing the marker; a
            // beat lets the kernel propagate the pipe state.
            tokio::time::sleep(Duration::from_millis(50)).await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("peer never signaled readiness at {}", path.display());
}

/// D-057 OA6 r3 item 6: a user prompt rides the SAME FIFO queue as every
/// control command. A control response enqueued before a prompt must reach
/// the child before that prompt (the old select! over two channels could
/// write the later prompt first).
#[tokio::test]
async fn writer_fifo_control_response_then_prompt() {
    let dir = tempdir_unique();
    let order = dir.join("order.log");
    let (_tx, mut rx, process) = ClaudeProcess::spawn(spec_with(
        "fake_claude_order.py",
        &[("FAKE_ORDER_FILE", order.to_str().unwrap())],
    ))
    .await
    .expect("spawn");
    drain_handshake(&mut rx).await;

    process
        .respond_control(
            "perm-1",
            ControlSuccessPayload::Permission(PermissionResult::Allow {
                updated_input: serde_json::json!({"command": "ls"}),
                updated_permissions: None,
            }),
        )
        .await
        .expect("control response");
    process
        .send_user(UserContent::Text("after the control".into()))
        .await
        .expect("user write");

    let markers = wait_for_markers(&order, 3).await;
    let response_at = markers.iter().position(|m| m == "control_response");
    let user_at = markers.iter().position(|m| m == "user");
    assert!(response_at.is_some(), "markers: {markers:?}");
    assert!(user_at.is_some(), "markers: {markers:?}");
    assert!(
        response_at < user_at,
        "control reordered behind prompt: {markers:?}"
    );

    drop(_tx);
    process.close_stdin().await.expect("close");
}

/// Item 6: an interrupt control request enqueued before a prompt is written
/// before that prompt — FIFO across all commands.
#[tokio::test]
async fn writer_fifo_interrupt_then_prompt() {
    let dir = tempdir_unique();
    let order = dir.join("order.log");
    let (_tx, mut rx, process) = ClaudeProcess::spawn(spec_with(
        "fake_claude_order.py",
        &[("FAKE_ORDER_FILE", order.to_str().unwrap())],
    ))
    .await
    .expect("spawn");
    drain_handshake(&mut rx).await;

    process.interrupt(true).await.expect("interrupt");
    process
        .send_user(UserContent::Text("queued prompt".into()))
        .await
        .expect("user write");

    let markers = wait_for_markers(&order, 3).await;
    let interrupt_at = markers
        .iter()
        .position(|m| m == "control_request:interrupt");
    let user_at = markers.iter().position(|m| m == "user");
    assert!(interrupt_at.is_some(), "markers: {markers:?}");
    assert!(user_at.is_some(), "markers: {markers:?}");
    assert!(
        interrupt_at < user_at,
        "interrupt reordered behind prompt: {markers:?}"
    );

    drop(_tx);
    process.close_stdin().await.expect("close");
}

/// Item 7: a write to a peer that closed its OWN stdin and is still alive
/// fails as an I/O error — while the process remains running and no shutdown
/// was requested. This exercises the write itself, unlike the close path
/// (which reports ControlUnavailable before any write).
#[tokio::test]
async fn send_errors_broken_pipe_while_the_process_stays_alive() {
    let dir = tempdir_unique();
    let ready = dir.join("ready");
    let (_tx, _rx, mut process) = ClaudeProcess::spawn(spec_with(
        "fake_claude_broken_stdin.py",
        &[("FAKE_READY_FILE", ready.to_str().unwrap())],
    ))
    .await
    .expect("spawn");
    wait_for_file(&ready).await;

    // A prompt larger than a pipe fragment still fails on the first write:
    // the read end is gone.
    let big = "x".repeat(256 * 1024);
    let result = process.send_user(UserContent::Text(big)).await;
    let error = result.expect_err("write to a closed stdin must error");
    let kind = match &error {
        remuda_claude_wire::Error::Io(io) => io.kind(),
        other => panic!("expected an I/O error, got {other:?}"),
    };
    assert_eq!(
        kind,
        std::io::ErrorKind::BrokenPipe,
        "a closed read end must surface BrokenPipe, got {error:?}"
    );

    // The peer itself never exited: wait() must time out (stdout stayed open).
    let waited = tokio::time::timeout(Duration::from_millis(500), process.wait()).await;
    assert!(
        waited.is_err(),
        "peer must still be alive after the failed write"
    );
    assert!(
        process.id().is_some(),
        "the child handle is still a live pid"
    );

    let _ = process.kill();
}

fn tempdir_unique() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "remuda-wire-test-{}-{}",
        std::process::id(),
        TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

static TEST_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
