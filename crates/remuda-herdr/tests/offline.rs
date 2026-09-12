//! Offline create→start→prompt→wait→read→close against `fake-herdr`.
//!
//! Fixtures: `crates/remuda-testing/fixtures/herdr/` (live herdr 0.9.0 capture).
//! The `fake-herdr` binary comes from [`remuda_testing::fake_herdr_bin`] (env
//! `REMUDA_FAKE_HERDR_BIN`, then the already-built target artifact). Nested
//! `cargo build` is only a last resort and never targets a parent cargo test's
//! `--target-dir`.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use remuda_herdr::{
    AgentPromptParams, AgentReadParams, AgentStartParams, AgentStatus, AgentWaitParams, Client,
    PaneSplitParams, ReadSource, SplitDirection, TerminalObserver, WorkspaceCreateParams,
};
use remuda_testing::{fake_herdr_bin, herdr_session_ok_path};

struct ChildGuard(Option<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_fake(socket: &std::path::Path, script: &str) -> ChildGuard {
    let child = Command::new(fake_herdr_bin())
        .arg("--socket")
        .arg(socket)
        .arg("--script")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn fake-herdr");
    let mut guard = ChildGuard(Some(child));
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if socket.exists() {
            return guard;
        }
        let status = guard
            .0
            .as_mut()
            .unwrap()
            .try_wait()
            .expect("poll fake-herdr");
        assert!(
            status.is_none(),
            "fake-herdr exited before binding {}: {status:?}",
            socket.display()
        );
        assert!(
            Instant::now() < deadline,
            "fake-herdr socket {} did not appear within 15 seconds",
            socket.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test]
async fn fake_herdr_ok_flow() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("herdr.sock");
    let _child = spawn_fake(&socket, "ok");
    let client = Client::connect(&socket)
        .with_timeout(Duration::from_secs(5))
        .with_binary(fake_herdr_bin());

    client.ping().await.unwrap();
    let created = client
        .workspace_create(WorkspaceCreateParams {
            cwd: Some("/tmp/remuda-herdr".into()),
            label: Some("offline".into()),
            focus: false,
            ..WorkspaceCreateParams::default()
        })
        .await
        .unwrap();
    let split = client
        .pane_split(PaneSplitParams {
            direction: SplitDirection::Right,
            target_pane_id: Some(created.root_pane.pane_id.clone()),
            cwd: Some("/tmp/remuda-herdr".into()),
            focus: false,
            workspace_id: None,
            ratio: None,
            env: Default::default(),
        })
        .await
        .unwrap();
    let pane_id = split.pane.pane_id.clone();

    client
        .agent_start(AgentStartParams {
            name: "probe".into(),
            kind: "claude".into(),
            pane_id: pane_id.clone(),
            args: vec!["--model".into(), "haiku".into()],
            timeout_ms: Some(5_000),
        })
        .await
        .unwrap();
    client
        .agent_wait(AgentWaitParams {
            target: "probe".into(),
            until: vec![AgentStatus::Idle],
            timeout_ms: Some(5_000),
        })
        .await
        .unwrap();
    client
        .agent_prompt(AgentPromptParams {
            target: "probe".into(),
            text: "Reply with exactly OK".into(),
            wait: None,
        })
        .await
        .unwrap();
    client
        .agent_wait(AgentWaitParams {
            target: "probe".into(),
            until: vec![AgentStatus::Idle, AgentStatus::Done],
            timeout_ms: Some(5_000),
        })
        .await
        .unwrap();
    let read = client
        .agent_read(AgentReadParams {
            target: "probe".into(),
            source: ReadSource::RecentUnwrapped,
            lines: Some(40),
            format: remuda_herdr::ReadFormat::Text,
            strip_ansi: true,
        })
        .await
        .unwrap();
    assert!(
        read.text().contains("OK"),
        "expected OK in agent.read, got {:?}",
        read.text()
    );

    let mut observer = TerminalObserver::open(&client, &pane_id, 80, 24)
        .await
        .unwrap();
    let frame = observer.next_frame().await.expect("terminal frame");
    let frame = frame.expect("decode");
    assert_eq!(&frame.bytes[..], b"OK\n");

    client.pane_close(pane_id).await.unwrap();
    let listed = client.agent_list().await.unwrap();
    assert!(listed.agents.is_empty());
}

#[tokio::test]
async fn recorded_ok_fixture_round_trips_client_types() {
    let path = herdr_session_ok_path();
    let body = std::fs::read_to_string(path).unwrap();
    for line in body.lines() {
        let wrapper: serde_json::Value = serde_json::from_str(line).unwrap();
        let raw = serde_json::to_string(&wrapper["rpc"]).unwrap();
        remuda_herdr::parse_line(&raw).unwrap();
        if wrapper["dir"] == "a2c" && wrapper["rpc"]["result"]["type"] == "pong" {
            let result = &wrapper["rpc"]["result"];
            let pong: remuda_herdr::Pong = serde_json::from_value(result.clone()).unwrap();
            assert_eq!(pong.protocol, 22);
        }
    }
}
