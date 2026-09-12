//! Spawn the fixture stdio stub as if it were `codex app-server`.

use std::path::PathBuf;
use std::time::Duration;

use remuda_codex_wire::{
    AskForApproval, AskForApprovalMode, CodexAppServer, Inbound, SandboxMode, ServerNotification,
    SpawnSpec, ThreadStartParams, TurnInterruptParams, TurnStartParams, TypedServerNotification,
    UserInput,
};
use tokio::time::timeout;

fn fake_binary() -> PathBuf {
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-app-server.py");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&script, perms).expect("chmod fake-app-server.py");
    }
    script
}

fn spec() -> SpawnSpec {
    let mut spec = SpawnSpec::new(fake_binary(), PathBuf::from("/tmp")).expect("absolute binary");
    spec.model = Some("gpt-5.6-sol".into());
    spec.reasoning_effort = Some("low".into());
    spec.approval_policy = Some("never".into());
    spec.disable_hooks = true;
    spec
}

#[tokio::test]
async fn spawn_handshake_thread_turn_interrupt() {
    let (mut client, mut inbound) = CodexAppServer::spawn(spec())
        .await
        .expect("spawn+handshake");
    let init = client.initialize_result().expect("initialize");
    assert_eq!(init.codex_home, "/tmp/fake-codex-home");

    let started = client
        .thread_start(ThreadStartParams {
            model: Some("gpt-5.6-sol".into()),
            cwd: Some("/tmp".into()),
            approval_policy: Some(AskForApproval::Named(AskForApprovalMode::Never)),
            sandbox: Some(SandboxMode::WorkspaceWrite),
            service_name: Some("remuda".into()),
            ..ThreadStartParams::default()
        })
        .await
        .expect("thread/start");
    let thread_id = started.thread.id.clone();

    let turn = client
        .turn_start(TurnStartParams {
            thread_id: thread_id.clone(),
            input: vec![UserInput::text("SLOW")],
            model: None,
            effort: None,
            summary: None,
            approval_policy: None,
            client_user_message_id: None,
        })
        .await
        .expect("turn/start");
    assert_eq!(turn.turn.status, remuda_codex_wire::TurnStatus::InProgress);

    client
        .turn_interrupt(TurnInterruptParams {
            thread_id,
            turn_id: turn.turn.id,
        })
        .await
        .expect("turn/interrupt");

    let mut interrupted = false;
    for _ in 0..20 {
        match timeout(Duration::from_secs(5), inbound.recv())
            .await
            .expect("inbound timeout")
            .expect("inbound closed")
        {
            Inbound::Notification(ServerNotification::Typed(
                TypedServerNotification::TurnCompleted(done),
            )) => {
                assert_eq!(done.turn.status, remuda_codex_wire::TurnStatus::Interrupted);
                interrupted = true;
                break;
            }
            Inbound::Notification(_)
            | Inbound::NonJson(_)
            | Inbound::UnknownFrame(_)
            | Inbound::ServerRequest(_) => {}
        }
    }
    assert!(interrupted, "expected turn/completed interrupted");
    client.kill().await.expect("kill");
}
