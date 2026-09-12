//! Spawn the fixture stdio stub as if it were `codex app-server`.

use std::path::PathBuf;
use std::time::Duration;

use remuda_codex_wire::{
    AskForApproval, AskForApprovalMode, CodexAppServer, CommandExecutionApprovalDecision,
    CommandExecutionApprovalNamed, CommandExecutionRequestApprovalResponse, Inbound,
    ModelListParams, SandboxMode, ServerNotification, ServerRequest, SpawnSpec, ThreadListParams,
    ThreadReadParams, ThreadResumeParams, ThreadStartParams, TurnStartParams,
    TypedServerNotification, TypedServerRequest, UserInput, WireError,
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

async fn next_typed(
    inbound: &mut tokio::sync::mpsc::UnboundedReceiver<Inbound>,
) -> ServerNotification {
    loop {
        match timeout(Duration::from_secs(5), inbound.recv())
            .await
            .expect("inbound timeout")
            .expect("inbound closed")
        {
            Inbound::Notification(notification) => return notification,
            Inbound::NonJson(_) | Inbound::UnknownFrame(_) => continue,
            Inbound::ServerRequest(request) => {
                panic!("unexpected server request while waiting for notification: {request:?}")
            }
        }
    }
}

#[tokio::test]
async fn not_initialized_before_handshake() {
    let (client, _inbound) = CodexAppServer::spawn_uninitialized(spec())
        .await
        .expect("spawn stub");
    let error = client
        .peer()
        .request::<_, serde_json::Value>("thread/list", serde_json::json!({"limit":1}))
        .await
        .expect_err("must fail");
    match error {
        WireError::Rpc { code, message, .. } => {
            assert_eq!(code, -32600);
            assert!(message.contains("Not initialized"));
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn handshake_thread_turn_and_model_list() {
    let (mut client, mut inbound) = CodexAppServer::spawn(spec())
        .await
        .expect("spawn+handshake");
    let init = client.initialize_result().expect("initialize");
    assert_eq!(init.codex_home, "/tmp/fake-codex-home");

    let models = client
        .model_list(ModelListParams {
            limit: Some(10),
            cursor: None,
            include_hidden: Some(false),
        })
        .await
        .expect("model/list");
    assert_eq!(models.data[0].id, "gpt-5.6-sol");

    let started = client
        .thread_start(ThreadStartParams {
            model: Some("gpt-5.6-sol".into()),
            cwd: Some("/tmp".into()),
            approval_policy: Some(AskForApproval::Named(AskForApprovalMode::Never)),
            sandbox: Some(SandboxMode::WorkspaceWrite),
            personality: None,
            service_name: Some("remuda".into()),
            ..ThreadStartParams::default()
        })
        .await
        .expect("thread/start");
    let thread_id = started.thread.id.clone();

    let turn = client
        .turn_start(TurnStartParams::text(&thread_id, "Reply with exactly OK"))
        .await
        .expect("turn/start");
    assert_eq!(turn.turn.status, remuda_codex_wire::TurnStatus::InProgress);

    let mut completed = false;
    for _ in 0..20 {
        if let ServerNotification::Typed(TypedServerNotification::TurnCompleted(done)) =
            next_typed(&mut inbound).await
        {
            assert_eq!(done.turn.status, remuda_codex_wire::TurnStatus::Completed);
            completed = true;
            break;
        }
    }
    assert!(completed, "expected turn/completed");

    let listed = client
        .thread_list(ThreadListParams {
            limit: Some(10),
            cwd: Some(remuda_codex_wire::ThreadListCwdFilter::One("/tmp".into())),
            source_kinds: Some(vec![
                remuda_codex_wire::ThreadSourceKind::AppServer,
                remuda_codex_wire::ThreadSourceKind::Cli,
                remuda_codex_wire::ThreadSourceKind::Vscode,
                remuda_codex_wire::ThreadSourceKind::Exec,
            ]),
            ..ThreadListParams::default()
        })
        .await
        .expect("thread/list");
    assert_eq!(listed.data[0].id, thread_id);

    let read = client
        .thread_read(ThreadReadParams {
            thread_id: thread_id.clone(),
            include_turns: false,
        })
        .await
        .expect("thread/read");
    assert_eq!(read.thread.id, thread_id);

    let resumed = client
        .thread_resume(ThreadResumeParams {
            thread_id: thread_id.clone(),
            exclude_turns: true,
            model: Some("gpt-5.6-sol".into()),
            cwd: Some("/tmp".into()),
            approval_policy: None,
            sandbox: None,
        })
        .await
        .expect("thread/resume");
    assert_eq!(resumed.thread.id, thread_id);

    client.kill().await.expect("kill");
}

#[tokio::test]
async fn server_request_reply_is_id_result_only() {
    let (client, mut inbound) = CodexAppServer::spawn(spec()).await.expect("spawn");
    let started = client
        .thread_start(ThreadStartParams::default())
        .await
        .expect("thread/start");
    client
        .turn_start(TurnStartParams {
            thread_id: started.thread.id.clone(),
            input: vec![UserInput::text("NEED_APPROVAL")],
            model: None,
            effort: None,
            summary: None,
            approval_policy: None,
            client_user_message_id: None,
        })
        .await
        .expect("turn/start");

    let request = loop {
        match timeout(Duration::from_secs(5), inbound.recv())
            .await
            .expect("timeout")
            .expect("closed")
        {
            Inbound::ServerRequest(request) => break request,
            Inbound::Notification(_) | Inbound::NonJson(_) | Inbound::UnknownFrame(_) => {}
        }
    };
    let ServerRequest::Typed(TypedServerRequest::CommandExecutionApproval { id, params }) = request
    else {
        panic!("expected command approval, got {request:?}");
    };
    assert_eq!(params.command.as_deref(), Some("echo hi"));
    client
        .reply_result(
            id,
            CommandExecutionRequestApprovalResponse {
                decision: CommandExecutionApprovalDecision::Named(
                    CommandExecutionApprovalNamed::Accept,
                ),
            },
        )
        .await
        .expect("reply");

    let mut completed = false;
    for _ in 0..20 {
        if let ServerNotification::Typed(TypedServerNotification::TurnCompleted(_)) =
            next_typed(&mut inbound).await
        {
            completed = true;
            break;
        }
    }
    assert!(completed);
}

#[tokio::test]
async fn steer_without_active_turn_is_rpc_error() {
    let (client, _inbound) = CodexAppServer::spawn(spec()).await.expect("spawn");
    let started = client
        .thread_start(ThreadStartParams::default())
        .await
        .expect("thread/start");
    let error = client
        .turn_steer(remuda_codex_wire::TurnSteerParams {
            thread_id: started.thread.id,
            expected_turn_id: "missing".into(),
            input: vec![UserInput::text("ignore")],
        })
        .await
        .expect_err("steer");
    match error {
        WireError::Rpc { message, .. } => {
            assert!(message.contains("no active turn to steer"));
        }
        other => panic!("{other:?}"),
    }
}
