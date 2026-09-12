//! Process tests against `fake_claude.py` (no model).

use remuda_claude_wire::{
    ClaudeProcess, ControlSuccessPayload, Outbound, PermissionResult, SpawnSpec, SystemMessage,
    UserContent,
};
use std::path::PathBuf;
use std::time::Duration;

fn spec() -> SpawnSpec {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_claude.py");
    SpawnSpec {
        binary: PathBuf::from("python3"),
        cwd: std::env::temp_dir(),
        raw_argv: Some(vec!["-u".into(), script.to_string_lossy().into_owned()]),
        handshake_timeout: Duration::from_secs(5),
        ..SpawnSpec::default()
    }
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
