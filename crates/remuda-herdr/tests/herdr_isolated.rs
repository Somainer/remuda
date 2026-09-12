//! Isolated Herdr integration. Requires a local `herdr` binary and Claude CLI.
//!
//! Source procedure: `docs/research/herdr-herdrx.md` §3.
//! Run: `cargo test -p remuda-herdr --test herdr_isolated -- --ignored --nocapture`

use std::path::PathBuf;
use std::time::Duration;

use remuda_herdr::{
    AgentReadParams, AgentStartParams, AgentStatus, AgentWaitParams, Client, HerdrServer,
    PaneSplitParams, ReadSource, SplitDirection, WorkspaceCreateParams,
};

const SESSION: &str = "remuda-test";

fn isolation_cwd() -> PathBuf {
    PathBuf::from("/tmp/remuda-herdr")
}

#[tokio::test]
#[ignore = "needs local herdr + claude; allowed once per impl-herdr-client"]
async fn start_prompt_wait_read_ok() -> anyhow::Result<()> {
    let cwd = isolation_cwd();
    std::fs::create_dir_all(&cwd)?;

    let mut server = HerdrServer::ensure(SESSION, None).await?;
    let client = server.client().with_timeout(Duration::from_secs(15));

    let pong = client.ping().await?;
    anyhow::ensure!(!pong.version.is_empty(), "ping version");

    let created = client
        .workspace_create(WorkspaceCreateParams {
            cwd: Some(cwd.to_string_lossy().into_owned()),
            label: Some("remuda-test".into()),
            focus: false,
            ..WorkspaceCreateParams::default()
        })
        .await?;
    let root = created.root_pane.pane_id.clone();

    let split = client
        .pane_split(PaneSplitParams {
            direction: SplitDirection::Right,
            target_pane_id: Some(root),
            cwd: Some(cwd.to_string_lossy().into_owned()),
            focus: false,
            workspace_id: None,
            ratio: None,
            env: Default::default(),
        })
        .await?;
    let pane_id = split.pane.pane_id.clone();

    let start = AgentStartParams {
        name: "probe".into(),
        kind: "claude".into(),
        pane_id: pane_id.clone(),
        args: vec![
            "--model".into(),
            "haiku".into(),
            "--dangerously-skip-permissions".into(),
            "--max-budget-usd".into(),
            "0.3".into(),
        ],
        timeout_ms: Some(90_000),
    };

    match client.agent_start(start).await {
        Ok(_) => {}
        Err(err)
            if err.to_string().contains("blocked") || err.to_string().contains("not ready") =>
        {
            let _ = client
                .agent_send_keys("probe", vec!["down".into(), "enter".into()])
                .await;
        }
        Err(err) => {
            let _ = server.stop().await;
            let _ = server.delete_session().await;
            return Err(err.into());
        }
    }

    wait_idle(&client, "probe").await?;

    client
        .agent_prompt(remuda_herdr::AgentPromptParams {
            target: "probe".into(),
            text: "Reply with exactly OK".into(),
            wait: None,
        })
        .await?;

    wait_idle(&client, "probe").await?;

    let read = client
        .agent_read(AgentReadParams {
            target: "probe".into(),
            source: ReadSource::RecentUnwrapped,
            lines: Some(80),
            format: remuda_herdr::ReadFormat::Text,
            strip_ansi: true,
        })
        .await?;
    let text = read.text();
    anyhow::ensure!(
        text.contains("OK"),
        "agent.read did not contain OK: {text:?}"
    );

    let _ = client.pane_close(pane_id).await;
    server.stop().await?;
    server.delete_session().await?;
    Ok(())
}

async fn wait_idle(client: &Client, target: &str) -> anyhow::Result<()> {
    for _ in 0..8 {
        match client
            .agent_wait(AgentWaitParams {
                target: target.into(),
                until: vec![AgentStatus::Idle, AgentStatus::Done],
                timeout_ms: Some(30_000),
            })
            .await
        {
            Ok(info) => {
                if matches!(
                    info.agent.agent_status,
                    AgentStatus::Idle | AgentStatus::Done
                ) {
                    return Ok(());
                }
                if info.agent.agent_status == AgentStatus::Blocked {
                    let _ = client
                        .agent_send_keys(target, vec!["down".into(), "enter".into()])
                        .await;
                }
            }
            Err(err) if err.to_string().contains("blocked") => {
                let _ = client
                    .agent_send_keys(target, vec!["down".into(), "enter".into()])
                    .await;
            }
            Err(err) => return Err(err.into()),
        }
    }
    anyhow::bail!("agent {target} never reached idle")
}
