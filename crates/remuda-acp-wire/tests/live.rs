//! Ignored live grok turns. Isolation dir: `/tmp/remuda-acp-wire`.
//!
//! Allowed by impl-acp-wire.md: two runs (OK, then write-file / tool_call).

use std::path::PathBuf;
use std::time::Duration;

use remuda_acp_wire::{SessionSpec, SpawnSpec, StopReason, connect_stdio};

fn live_root() -> PathBuf {
    PathBuf::from("/tmp/remuda-acp-wire")
}

async fn prepare_cwd() -> PathBuf {
    let root = live_root();
    tokio::fs::create_dir_all(&root).await.unwrap();
    let cwd = root.join(format!("run-{}", std::process::id()));
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let _ = tokio::process::Command::new("git")
        .args(["init"])
        .current_dir(&cwd)
        .status()
        .await;
    cwd
}

#[tokio::test]
#[ignore]
async fn live_reply_ok() {
    let cwd = prepare_cwd().await;
    let result = tokio::time::timeout(
        Duration::from_secs(180),
        connect_stdio(SpawnSpec::stdio(&cwd), async move |conn| {
            let init = conn.initialize().await?;
            assert_eq!(init.protocol_version, remuda_acp_wire::ProtocolVersion::V1);
            let mut session = conn.new_session(SessionSpec::new(&cwd)).await?;
            let turn = session.prompt("Reply with exactly OK").await?;
            Ok::<_, remuda_acp_wire::Error>((turn.stop_reason, turn.assistant_text()))
        }),
    )
    .await
    .expect("live grok timed out")
    .expect("live grok failed");
    assert_eq!(result.0, StopReason::EndTurn);
    assert!(
        result.1.contains("OK"),
        "assistant text should contain OK, got {:?}",
        result.1
    );
}

#[tokio::test]
#[ignore]
async fn live_write_file_tool_call() {
    let cwd = prepare_cwd().await;
    let target = cwd.join("probe-ok.txt");
    let result = tokio::time::timeout(
        Duration::from_secs(240),
        connect_stdio(SpawnSpec::stdio(&cwd), async move |conn| {
            conn.initialize().await?;
            let mut session = conn.new_session(SessionSpec::new(&cwd)).await?;
            let turn = session
                .prompt(
                    "Create a file named probe-ok.txt in the current working directory containing exactly the two letters OK and nothing else. Do not write any other files.",
                )
                .await?;
            Ok::<_, remuda_acp_wire::Error>(turn)
        }),
    )
    .await
    .expect("live grok timed out")
    .expect("live grok failed");
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert!(
        result.has_tool_call(),
        "expected a tool_call update, got {:?}",
        result.updates
    );
    if target.exists() {
        let body = std::fs::read_to_string(&target).unwrap();
        assert!(body.contains("OK"), "file contents {body:?}");
    }
}
