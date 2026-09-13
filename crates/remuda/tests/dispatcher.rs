//! Built process checks with a local fake consume executable and DryRun outbound.
//! The replayed NDJSON is schema-derived; no live Feishu app or model is used.

#![cfg(unix)]

use anyhow::{Context, Result, ensure};
use remuda_hub_client::HubClient;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::process::Command;

const BOOTSTRAP: &str = "dispatcher-test-bootstrap-credential";

/// `credential` is the `[dispatcher]` credential stanza under test.
fn write_config_with(dir: &Path, credential: &str) -> Result<std::path::PathBuf> {
    let binary =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-dispatcher-lark.py");
    let token = dir.join("bootstrap-token");
    std::fs::write(&token, BOOTSTRAP)?;
    let text = format!(
        "data_dir = {}\nshutdown_timeout_secs = 2\n[hub]\nbootstrap_token = {}\n[dispatcher]\nprofile = 'dispatcher-test'\nowner_open_ids = ['ou_dispatcher_owner']\nlark_cli = {}\nhub_url = 'https://unused.example'\n{credential}",
        serde_json::to_string(&dir.join("data"))?,
        serde_json::to_string(&format!("file:{}", token.display()))?,
        serde_json::to_string(&binary)?,
    );
    let path = dir.join("remuda.toml");
    std::fs::write(&path, text)?;
    Ok(path)
}

fn write_config(dir: &Path, with_token: bool) -> Result<std::path::PathBuf> {
    let credential = if with_token {
        "token = 'file:deliberately-absent-device-token'\n"
    } else {
        ""
    };
    write_config_with(dir, credential)
}

fn command(dir: &Path, config: &Path) -> Result<Command> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("REMUDA_") {
            command.env_remove(key);
        }
    }
    command
        .args(["--config", config.to_str().context("config path")?])
        .current_dir(dir)
        .env("RUST_LOG", "info")
        .env("REMUDA_TEST_LARK_DIR", dir)
        .env("REMUDA_TEST_LARK_STATUS_ONLY", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    Ok(command)
}

#[tokio::test]
async fn hub_with_dispatcher_uses_local_auth_and_sigterm_stops_both_consumers() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let config = write_config(dir.path(), true)?;
    let log_path = dir.path().join("stderr.log");
    let log = std::fs::File::create(&log_path)?;
    let mut child = command(dir.path(), &config)?
        .args(["hub", "--with-dispatcher", "--listen", "127.0.0.1:0"])
        .stderr(log)
        .kill_on_drop(true)
        .spawn()?;
    let exercise: Result<()> = async {
        let address = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let text = std::fs::read_to_string(&log_path)?;
                if text.contains("remuda dispatcher ready") {
                    let address = text
                        .lines()
                        .find(|line| line.contains("remuda hub listening"))
                        .and_then(|line| {
                            line.split_whitespace()
                                .find_map(|part| part.strip_prefix("address="))
                        })
                        .context("Hub listener log")?;
                    return Ok::<_, anyhow::Error>(address.to_owned());
                }
                ensure!(
                    child.try_wait()?.is_none(),
                    "process exited before readiness: {text}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("combined mode readiness")??;
        // Pairing a fresh device with the access code is what the code is for
        // (D-018); it is not what the dispatcher itself uses.
        let client = HubClient::new(format!("http://{address}"), None, Some(BOOTSTRAP.into()))?;
        client
            .list_hosts()
            .await
            .context("local Hub serves authenticated requests")?;
        // The dispatcher must hold its own scoped device token, minted in
        // process at startup, rather than carrying the pairing access code.
        let devices = get_json(&address, "/v1/devices", BOOTSTRAP).await?;
        ensure!(
            devices.contains("remuda-dispatcher"),
            "dispatcher did not mint a scoped device token: {devices}"
        );
        ensure!(
            dir.path().join("data/dispatcher/sessions.sqlite").is_file(),
            "session map missing"
        );
        for key in [
            remuda_feishu::EVENT_IM_RECEIVE,
            remuda_feishu::EVENT_CARD_ACTION,
        ] {
            ensure!(
                !dir.path().join(format!("{key}.stopped")).exists(),
                "consume stdin closed before shutdown"
            );
        }
        Ok(())
    }
    .await;
    // Cleanup also runs when a readiness or HTTP assertion fails.
    if child.try_wait()?.is_none() {
        let pid = child.id().context("child PID")?;
        ensure!(
            std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status()?
                .success(),
            "SIGTERM failed"
        );
    }
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .context("combined mode shutdown")??;
    exercise?;
    ensure!(
        status.success(),
        "combined mode exit: {}",
        std::fs::read_to_string(&log_path)?
    );
    for key in [
        remuda_feishu::EVENT_IM_RECEIVE,
        remuda_feishu::EVENT_CARD_ACTION,
    ] {
        ensure!(
            std::fs::read_to_string(dir.path().join(format!("{key}.stopped")))? == "SIGTERM",
            "consume must stop on SIGTERM"
        );
    }
    ensure!(
        !dir.path().join("unexpected-outbound").exists(),
        "DryRun spawned outbound"
    );
    Ok(())
}

#[tokio::test]
async fn standalone_missing_credentials_fails_before_starting_consume() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let config = write_config(dir.path(), false)?;
    let output = command(dir.path(), &config)?
        .arg("dispatcher")
        .output()
        .await?;
    ensure!(!output.status.success(), "missing credentials must fail");
    ensure!(
        String::from_utf8_lossy(&output.stderr).contains("dispatcher.token"),
        "unexpected error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        !dir.path().join("im.message.receive_v1.starts").exists(),
        "consume started before auth validation"
    );
    Ok(())
}

/// F13: the dispatcher holds its credential for the process lifetime, so it must
/// not be given the Hub bootstrap — an unlimited, unrotatable join secret that
/// also mints device tokens. Configuring one is a startup error, not a fallback.
#[tokio::test]
async fn standalone_refuses_a_bootstrap_token() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let secret = dir.path().join("dispatcher-bootstrap");
    std::fs::write(&secret, BOOTSTRAP)?;
    let config = write_config_with(
        dir.path(),
        &format!(
            "bootstrap_token = {}\n",
            serde_json::to_string(&format!("file:{}", secret.display()))?
        ),
    )?;
    let output = command(dir.path(), &config)?
        .arg("dispatcher")
        .output()
        .await?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    ensure!(
        !output.status.success(),
        "a bootstrap token must be refused for the dispatcher role"
    );
    ensure!(
        stderr.contains("dispatcher.bootstrap_token is not accepted"),
        "unexpected error: {stderr}"
    );
    ensure!(
        !stderr.contains(BOOTSTRAP),
        "the refusal must not echo the credential: {stderr}"
    );
    ensure!(
        !dir.path().join("im.message.receive_v1.starts").exists(),
        "consume started despite a refused credential"
    );
    Ok(())
}

/// Pair a throwaway device over HTTP and GET `path` with its device token.
async fn get_json(address: &str, path: &str, bootstrap: &str) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn send(address: &str, request: &str) -> Result<String> {
        let mut stream = tokio::net::TcpStream::connect(address).await?;
        stream.write_all(request.as_bytes()).await?;
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await?;
        Ok(String::from_utf8_lossy(&buf).to_string())
    }

    let body =
        format!(r#"{{"bootstrapToken":"{bootstrap}","deviceName":"dispatcher-test-probe"}}"#);
    let login = send(
        address,
        &format!(
            "POST /v1/login HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await?;
    let token = login
        .split_once("\"token\":\"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(token, _)| token.to_owned())
        .context("probe device token")?;
    send(
        address,
        &format!(
            "GET {path} HTTP/1.1\r\nHost: {address}\r\n\
             Authorization: Bearer {token}\r\nConnection: close\r\n\r\n"
        ),
    )
    .await
}
