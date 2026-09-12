//! Live `codex app-server` canary. Ignored by default.
//!
//! Run once with a logged-in Codex install:
//! `cargo test -p remuda-codex-wire --test live -- --ignored --nocapture`

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use remuda_codex_wire::{
    AskForApproval, AskForApprovalMode, CodexAppServer, Inbound, SandboxMode, ServerNotification,
    SpawnSpec, ThreadItem, ThreadStartParams, TurnStartParams, TypedServerNotification,
    TypedThreadItem,
};
use tokio::time::timeout;

fn which_absolute(name: &str) -> Option<PathBuf> {
    if let Ok(explicit) = env::var("CODEX_BIN") {
        let path = PathBuf::from(explicit);
        if path.is_absolute() {
            return path.canonicalize().ok().or(Some(path));
        }
    }
    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return candidate.canonicalize().ok();
        }
    }
    None
}

fn prepare_workspace() -> PathBuf {
    let root = PathBuf::from("/tmp/remuda-codex-wire");
    let _ = fs::create_dir_all(&root);
    if !root.join(".git").exists() {
        let status = Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .status()
            .expect("git init");
        assert!(status.success(), "git init {root:?}");
    }
    root
}

fn live_spec(binary: PathBuf, cwd: &Path) -> SpawnSpec {
    let mut spec = SpawnSpec::new(binary, cwd.to_path_buf()).expect("absolute binary");
    spec.model = Some("gpt-5.6-sol".into());
    spec.reasoning_effort = Some("low".into());
    spec.approval_policy = Some("never".into());
    spec.disable_hooks = true;
    spec
}

#[tokio::test]
#[ignore = "talks to a real Codex install and model; allowed once by impl-codex-wire"]
async fn live_reply_exactly_ok() {
    let binary = which_absolute("codex").unwrap_or_else(|| {
        panic!("set CODEX_BIN to an absolute `codex` path or put `codex` on PATH")
    });
    let cwd = prepare_workspace();
    let (mut client, mut inbound) = CodexAppServer::spawn(live_spec(binary, &cwd))
        .await
        .expect("spawn live codex app-server");
    let init = client.initialize_result().expect("handshake");
    assert!(!init.codex_home.is_empty());

    let started = client
        .thread_start(ThreadStartParams {
            model: Some("gpt-5.6-sol".into()),
            cwd: Some(cwd.to_string_lossy().into_owned()),
            approval_policy: Some(AskForApproval::Named(AskForApprovalMode::Never)),
            sandbox: Some(SandboxMode::WorkspaceWrite),
            ephemeral: Some(true),
            service_name: Some("remuda".into()),
            ..ThreadStartParams::default()
        })
        .await
        .expect("thread/start");

    client
        .turn_start(TurnStartParams {
            thread_id: started.thread.id.clone(),
            input: vec![remuda_codex_wire::UserInput::text("Reply with exactly OK")],
            model: Some("gpt-5.6-sol".into()),
            effort: Some("low".into()),
            summary: Some(remuda_codex_wire::ReasoningSummary::None),
            approval_policy: Some(AskForApproval::Named(AskForApprovalMode::Never)),
            client_user_message_id: None,
        })
        .await
        .expect("turn/start");

    let deadline = Duration::from_secs(120);
    let mut saw_ok = false;
    loop {
        match timeout(deadline, inbound.recv()).await {
            Ok(Some(Inbound::Notification(ServerNotification::Typed(
                TypedServerNotification::ItemCompleted(completed),
            )))) => {
                if let ThreadItem::Typed(TypedThreadItem::AgentMessage { text, .. }) =
                    completed.item
                    && text.trim() == "OK"
                {
                    saw_ok = true;
                }
            }
            Ok(Some(Inbound::Notification(ServerNotification::Typed(
                TypedServerNotification::TurnCompleted(done),
            )))) => {
                assert_eq!(done.turn.status, remuda_codex_wire::TurnStatus::Completed);
                break;
            }
            Ok(Some(Inbound::Notification(ServerNotification::Typed(
                TypedServerNotification::Error(error),
            )))) => {
                panic!("native error notification: {error:?}");
            }
            Ok(Some(_)) => {}
            Ok(None) => panic!("app-server stdout closed before turn/completed"),
            Err(_) => panic!("timed out waiting for turn/completed"),
        }
    }
    assert!(saw_ok, "expected an agentMessage item with text OK");
    client.kill().await.expect("kill");
}
