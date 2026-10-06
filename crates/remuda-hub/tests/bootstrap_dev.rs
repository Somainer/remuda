//! c-bootstrap-dev integration tests:
//! - a stale stamp plus an explicit access-code file survives restart;
//! - rotate-bootstrap targets the dev-hub data layout.

use anyhow::{Context, Result};
use remuda_hub::{HubConfig, spawn};
use serde_json::json;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Result<(u16, String, String)> {
    let mut stream = TcpStream::connect(addr).await?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(text) = body {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            text.len()
        ));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut request = head.into_bytes();
    if let Some(text) = body {
        request.extend_from_slice(text.as_bytes());
    }
    stream.write_all(&request).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("headers")?;
    let head_text = String::from_utf8_lossy(&buf[..split]).to_string();
    let status = head_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    Ok((
        status,
        head_text.clone(),
        String::from_utf8_lossy(&buf[split + 4..]).to_string(),
    ))
}

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> Result<String> {
    let body =
        json!({"bootstrapToken": bootstrap, "deviceName": "bootstrap-dev-phone"}).to_string();
    let (status, head, _) = http(addr, "POST", "/v1/login", &[], Some(&body)).await?;
    assert_eq!(status, 200, "login failed");
    for line in head.lines() {
        if line.to_ascii_lowercase().starts_with("set-cookie:") {
            return Ok(line
                .split(':')
                .nth(1)
                .unwrap()
                .trim()
                .split(';')
                .next()
                .unwrap()
                .trim()
                .to_string());
        }
    }
    anyhow::bail!("no set-cookie")
}

/// Write the access-code file at 0600.
fn write_code_file(path: &Path, code: &str) -> Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, code)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Stale stamp + explicit access-code file: first Hub writes an expired
/// stamp for the old code; the second Hub (simulating restart) is given the
/// same file code and must re-persist/re-stamp so login still succeeds.
#[tokio::test]
async fn restart_with_explicit_access_code_file_refreshes_expired_stamp() -> Result<()> {
    let outer = tempfile::tempdir()?;
    // remuda dev writes the Hub data under <data-dir>/dev-hub.
    let hub_data = outer.path().join("dev-hub");
    std::fs::create_dir_all(&hub_data)?;
    let code_file = outer.path().join("access-code");
    let code = "explicit-file-code-that-is-at-least-sixteen";
    write_code_file(&code_file, code)?;

    // First Hub: persists the code + stamps now.
    {
        let config = HubConfig {
            bootstrap_token: code.to_owned(),
            bootstrap_token_file: Some(code_file.clone()),
            bootstrap_token_from_env: false,
            ..HubConfig::for_test(hub_data.clone())
        };
        let hub = spawn(config).await?;
        let _cookie = login(hub.addr, code).await?;
        hub.shutdown().await;
    }

    // Backdate the persisted stamp past the 24h TTL (as if a long time
    // passed between restarts).
    let stamp = hub_data.join("bootstrap-issued-at");
    std::fs::write(&stamp, "2000-01-01T00:00:00.000Z")?;

    // Second Hub with the SAME access-code file: resolve must re-stamp.
    {
        let config = HubConfig {
            bootstrap_token: code.to_owned(),
            bootstrap_token_file: Some(code_file.clone()),
            bootstrap_token_from_env: false,
            ..HubConfig::for_test(hub_data.clone())
        };
        let hub = spawn(config).await?;
        // Login succeeds because the stamp was refreshed despite the
        // backdated persisted stamp.
        let _cookie = login(hub.addr, code).await?;
        hub.shutdown().await;
    }
    Ok(())
}

/// rotate-bootstrap writes into a dev-hub/ subdirectory when present.
#[tokio::test]
async fn rotate_bootstrap_honours_dev_hub_layout() -> Result<()> {
    // We test the pure rotate function against a dev-hub directory (the CLI
    // path detection is a simple is_dir check; this proves the rotation
    // itself lands in the right place).
    let outer = tempfile::tempdir()?;
    let hub_data = outer.path().join("dev-hub");
    std::fs::create_dir_all(&hub_data)?;
    let old = "old-code-1234567890abcdef";
    remuda_hub::persist_bootstrap(&hub_data, old)?;

    let new = remuda_hub::rotate_bootstrap(&hub_data)?;
    assert_ne!(new, old);
    let stored = std::fs::read_to_string(hub_data.join("bootstrap-token"))?;
    assert_eq!(stored.trim(), new);
    // The OUTER dir must NOT have received a token.
    assert!(!outer.path().join("bootstrap-token").exists());
    Ok(())
}
