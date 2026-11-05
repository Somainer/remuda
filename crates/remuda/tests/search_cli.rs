//! `remuda search` (cb-search) against the in-process Hub, run the way a seat
//! runs it: its own Agent credential. The seat finds what its worker said and
//! never what a foreign lineage said; the owner finds both.

use anyhow::{Context, Result};
use remuda_hub::{HubConfig, spawn};
use serde_json::{Value, json};
use std::process::{Command, Output};

const HOST: &str = "hst_search_cli";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_remuda")
}

struct Hub {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    base: String,
    owner: String,
}

impl Hub {
    async fn spawn() -> Result<Hub> {
        let dir = tempfile::tempdir()?;
        let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
        hub.test_insert_host(HOST).await?;
        let owner = hub.mint_device_token("search-cli-owner").await?;
        let base = format!("http://{}", hub.addr);
        Ok(Hub {
            _dir: dir,
            hub,
            base,
            owner,
        })
    }

    async fn instance(&self, parent: Option<&str>) -> Result<String> {
        let id = self
            .hub
            .store()
            .context("store")?
            .insert_instance(
                HOST.into(),
                None,
                "claude".into(),
                "claude-sdk".into(),
                None,
                json!({"kind": "claude"}),
            )
            .await?
            .instance_id;
        if let Some(parent) = parent {
            rusqlite::Connection::open(self._dir.path().join("hub").join("hub.sqlite"))?.execute(
                "UPDATE instances SET spec_json = ?1 WHERE id = ?2",
                rusqlite::params![
                    json!({"kind": "claude", "parentInstanceId": parent}).to_string(),
                    id
                ],
            )?;
        }
        Ok(id)
    }

    async fn say(&self, instance: &str, text: &str) -> Result<i64> {
        let event = json!({
            "kind": "message",
            "payload": {
                "nodeId": "m", "operation": "close", "messageId": "m",
                "role": "assistant", "phase": "final",
                "blocks": [{"type": "text", "text": text}],
            },
        });
        Ok(self
            .hub
            .store()
            .context("store")?
            .append_journal(HOST.into(), instance.into(), None, event)
            .await?
            .record
            .seq)
    }

    fn run(&self, token: &str, args: &[&str]) -> Result<Output> {
        let mut command = Command::new(bin());
        command
            .arg("search")
            .args(args)
            .env("REMUDA_HUB", &self.base)
            .env("REMUDA_TOKEN", token);
        for name in [
            "REMUDA_INSTANCE_ID",
            "REMUDA_BOOTSTRAP_TOKEN",
            "REMUDA_DATA_DIR",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command.env_remove(name);
        }
        Ok(command.output()?)
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_seat_finds_its_workers_words_and_never_a_foreign_lineages() -> Result<()> {
    let hub = Hub::spawn().await?;
    let seat = hub.instance(None).await?;
    let worker = hub.instance(Some(&seat)).await?;
    let foreign = hub.instance(None).await?;
    let seq = hub
        .say(&worker, "the quartermaster ledger is reconciled")
        .await?;
    hub.say(&foreign, "a foreign quartermaster ledger").await?;
    let agent = hub.hub.test_mint_agent_token("seat", &seat).await?;
    // The boot backfill finds an empty journal and finishes at once; wait
    // for it so `indexComplete` below is deterministic.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let output = tokio::task::block_in_place(|| hub.run(&hub.owner, &["--status", "--json"]))?;
        let status: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        if status["complete"] == json!(true) {
            break;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "backfill: {status}");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let output = tokio::task::block_in_place(|| hub.run(&agent, &["quartermaster", "ledger"]))?;
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.starts_with(&format!("{worker} seq {seq}  ")), "{text}");
    assert!(text.contains("message/assistant  the quartermaster ledger is reconciled"));

    // JSON, narrowed by time: the hit carries its seq as the journal does.
    let output = tokio::task::block_in_place(|| {
        hub.run(&agent, &["quartermaster", "--since", "1h", "--json"])
    })?;
    assert!(output.status.success(), "{output:?}");
    let body: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(body["hits"].as_array().map(Vec::len), Some(1), "{body}");
    assert_eq!(body["hits"][0]["seq"], json!(seq.to_string()));
    assert_eq!(body["indexComplete"], json!(true));

    // The owner reads both; narrowing to the foreign instance is the owner's
    // to make, and the seat gets nothing there.
    let output = tokio::task::block_in_place(|| hub.run(&hub.owner, &["quartermaster"]))?;
    assert_eq!(stdout(&output).lines().count(), 2, "{output:?}");
    let output = tokio::task::block_in_place(|| {
        hub.run(&agent, &["quartermaster", "--instance", &foreign])
    })?;
    assert!(output.status.success());
    assert_eq!(stdout(&output), "");
    assert!(String::from_utf8_lossy(&output.stderr).contains("no matches"));

    // The status: counts for the owner, the state alone for the seat.
    let output = tokio::task::block_in_place(|| hub.run(&hub.owner, &["--status"]))?;
    assert!(output.status.success(), "{output:?}");
    assert!(
        stdout(&output).starts_with("index complete (state done)"),
        "{output:?}"
    );
    assert!(stdout(&output).contains(", docs 2"), "{output:?}");
    let output = tokio::task::block_in_place(|| hub.run(&agent, &["--status", "--json"]))?;
    let status: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(status["complete"], json!(true));
    assert!(status.get("docs").is_none(), "{status}");

    // A bad duration is refused before any request.
    let output = tokio::task::block_in_place(|| hub.run(&agent, &["x", "--since", "soon"]))?;
    assert!(!output.status.success());
    Ok(())
}
