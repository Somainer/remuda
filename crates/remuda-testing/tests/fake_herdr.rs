//! Drive `FakeHerdrServer` with `remuda_herdr::Client`.

use remuda_herdr::{
    AgentPromptParams, AgentReadParams, AgentStartParams, AgentStatus, AgentWaitParams, Client,
    EventKind, PaneSplitParams, ReadSource, SplitDirection, Subscription, WorkspaceCreateParams,
};
use remuda_testing::{FakeHerdrOptions, FakeHerdrScript, FakeHerdrServer, fixtures_dir};
use std::time::Duration;

#[tokio::test]
async fn ok_script_create_start_prompt_wait_read() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("herdr.sock");
    let mut options = FakeHerdrOptions::new(&socket);
    options.script = FakeHerdrScript::Ok;
    let server = FakeHerdrServer::spawn(options).unwrap();
    let client = Client::connect(server.socket_path()).with_timeout(Duration::from_secs(5));

    let pong = client.ping().await.unwrap();
    assert_eq!(pong.protocol, 22);

    let created = client
        .workspace_create(WorkspaceCreateParams {
            cwd: Some("/tmp/remuda-herdr".into()),
            label: Some("probe".into()),
            focus: false,
            ..WorkspaceCreateParams::default()
        })
        .await
        .unwrap();
    let root = created.root_pane.pane_id.clone();
    let split = client
        .pane_split(PaneSplitParams {
            direction: SplitDirection::Right,
            target_pane_id: Some(root),
            cwd: Some("/tmp/remuda-herdr".into()),
            focus: false,
            workspace_id: None,
            ratio: None,
            env: Default::default(),
        })
        .await
        .unwrap();
    let pane_id = split.pane.pane_id.clone();

    let mut events = client
        .subscribe(vec![
            Subscription::pane_created(),
            Subscription::pane_agent_status_changed(&pane_id),
        ])
        .await
        .unwrap();

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

    let waited = client
        .agent_wait(AgentWaitParams {
            target: "probe".into(),
            until: vec![AgentStatus::Idle, AgentStatus::Done],
            timeout_ms: Some(5_000),
        })
        .await
        .unwrap();
    assert_eq!(waited.agent.agent_status, AgentStatus::Idle);

    let read = client
        .agent_read(AgentReadParams {
            target: "probe".into(),
            source: ReadSource::RecentUnwrapped,
            lines: Some(80),
            format: remuda_herdr::ReadFormat::Text,
            strip_ansi: true,
        })
        .await
        .unwrap();
    assert!(
        read.text().contains("OK"),
        "agent.read missing OK: {:?}",
        read.text()
    );

    let mut saw_working = false;
    let mut saw_idle = false;
    for _ in 0..8 {
        match tokio::time::timeout(Duration::from_millis(200), events.next_event()).await {
            Ok(Some(Ok(event))) => {
                if event.kind == EventKind::PaneAgentStatusChanged {
                    match event.agent_status() {
                        Some(AgentStatus::Working) => saw_working = true,
                        Some(AgentStatus::Idle) => saw_idle = true,
                        _ => {}
                    }
                }
            }
            _ => break,
        }
    }
    assert!(saw_working, "expected working event");
    assert!(saw_idle, "expected idle event");

    client.pane_close(pane_id).await.unwrap();
    let _ = server.shutdown();
}

#[tokio::test]
async fn trust_script_blocks_until_send_keys() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("herdr.sock");
    let mut options = FakeHerdrOptions::new(&socket);
    options.script = FakeHerdrScript::Trust;
    let server = FakeHerdrServer::spawn(options).unwrap();
    let client = Client::connect(server.socket_path()).with_timeout(Duration::from_secs(5));

    let created = client
        .workspace_create(WorkspaceCreateParams::default())
        .await
        .unwrap();
    let err = client
        .agent_start(AgentStartParams {
            name: "probe".into(),
            kind: "claude".into(),
            pane_id: created.root_pane.pane_id.clone(),
            args: vec![],
            timeout_ms: None,
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("blocked"), "{err}");

    client
        .agent_send_keys("probe", vec!["down".into(), "enter".into()])
        .await
        .unwrap();
    let info = client.agent_get("probe").await.unwrap();
    assert_eq!(info.agent.agent_status, AgentStatus::Idle);
    let _ = server.shutdown();
}

#[test]
fn recorded_session_fixture_is_parseable() {
    let path = fixtures_dir().join("herdr/session-ok.jsonl");
    let body = std::fs::read_to_string(path).unwrap();
    let mut responses = 0;
    for line in body.lines() {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        let raw = serde_json::to_string(&value["rpc"]).unwrap();
        remuda_herdr::parse_line(&raw).unwrap();
        if value["dir"] == "a2c" {
            responses += 1;
        }
    }
    assert!(responses >= 8);
}
