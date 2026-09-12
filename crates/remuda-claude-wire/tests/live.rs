//! Live e2e against a local `claude` binary. Ignored by default.
//!
//! Run: `cargo test -p remuda-claude-wire -- --ignored --nocapture`
//! Isolation: `/tmp/remuda-claude-wire/`, `--model haiku`, `--max-budget-usd 0.3`.

use remuda_claude_wire::{
    ClaudeProcess, ControlSuccessPayload, Outbound, PermissionResult, SettingSources, SpawnSpec,
    UserContent,
};
use std::path::PathBuf;
use std::time::Duration;

#[tokio::test]
#[ignore = "requires local `claude` and spends haiku budget"]
async fn host_allow_echoes_request_id() {
    let cwd = PathBuf::from("/tmp/remuda-claude-wire");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let marker = cwd.join("e2e-allow.txt");
    let _ = std::fs::remove_file(&marker);

    let spec = SpawnSpec {
        binary: PathBuf::from("claude"),
        cwd: cwd.clone(),
        model: Some("haiku".into()),
        permission_mode: remuda_claude_wire::PermissionMode::Default,
        setting_sources: SettingSources::Value(String::new()),
        max_budget_usd: Some(0.3),
        handshake_timeout: Duration::from_secs(45),
        include_partial_messages: false,
        include_hook_events: false,
        forward_subagent_text: false,
        replay_user_messages: false,
        ..SpawnSpec::default()
    };

    let (_tx, mut rx, process) = ClaudeProcess::spawn(spec).await.expect("spawn claude");

    process
        .send_user(UserContent::Text(format!(
            "Call Bash exactly once with command: touch {}. Then stop. Do not use other tools.",
            marker.display()
        )))
        .await
        .expect("user");

    let mut echoed = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let mut saw_result = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(msg)) => msg,
            Ok(None) => break,
            Err(_) => break,
        };
        if let Some((request_id, req)) = msg.as_can_use_tool() {
            assert_eq!(req.tool_name, "Bash");
            let id = request_id.to_owned();
            process
                .respond_control(
                    &id,
                    ControlSuccessPayload::Permission(PermissionResult::Allow {
                        updated_input: req.input.clone(),
                        updated_permissions: None,
                    }),
                )
                .await
                .expect("allow");
            echoed = Some(id);
        }
        if let Outbound::Result(result) = msg {
            assert_eq!(result.subtype.as_deref(), Some("success"));
            saw_result = true;
            break;
        }
    }

    let request_id = echoed.expect("expected can_use_tool from claude");
    assert!(saw_result, "expected a result after allow of {request_id}");
    process.close_stdin().await.expect("close");
}
