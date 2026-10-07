//! c-bootstrap-dev round 3 integration tests:
//! - an unchanged explicit code with an older-than-stamp file keeps the
//!   expired stamp across restart and the login is REFUSED;
//! - a changed code, or a touched file, re-stamps and the login succeeds;
//! - a no-source start that fails to bind leaves the explicit marker intact.
//! - rotate-bootstrap targets the dev-hub data layout.

use anyhow::{Context, Result};
use remuda_hub::{BootstrapSource, HubConfig, spawn};
use serde_json::json;
use std::path::{Path, PathBuf};
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

/// POST /v1/login; returns the raw HTTP status so tests can assert refusal.
async fn login_status(addr: std::net::SocketAddr, bootstrap: &str) -> Result<u16> {
    let body =
        json!({"bootstrapToken": bootstrap, "deviceName": "bootstrap-dev-phone"}).to_string();
    let (status, _head, _body) = http(addr, "POST", "/v1/login", &[], Some(&body)).await?;
    Ok(status)
}

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> Result<()> {
    let status = login_status(addr, bootstrap).await?;
    assert_eq!(status, 200, "login must succeed");
    Ok(())
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

/// Backdate a path's atime+mtime (Unix only) so it compares older than the
/// year-2000 test stamp (1999-12-31T00:00:00Z).
#[cfg(unix)]
fn backdate_mtime(path: &Path) {
    use nix::sys::stat::{UtimensatFlags, utimensat};
    use nix::sys::time::TimeSpec;
    let ts = TimeSpec::new(946_598_400, 0);
    utimensat(None, path, &ts, &ts, UtimensatFlags::FollowSymlink)
        .expect("utimensat backdates the access-code file");
}

fn explicit_file_config(hub_data: PathBuf, code_file: PathBuf, code: &str) -> HubConfig {
    HubConfig {
        bootstrap_token: code.to_owned(),
        bootstrap_source: BootstrapSource::ExplicitFile(code_file),
        ..HubConfig::for_test(hub_data)
    }
}

const EXPIRED_STAMP: &str = "2000-01-01T00:00:00.000Z";

/// Same code, but the access-code file was touched (mtime newer than the
/// backdated stamp): restart re-stamps and login succeeds.
#[tokio::test]
async fn restart_with_touched_explicit_file_restamps_and_login_succeeds() -> Result<()> {
    let outer = tempfile::tempdir()?;
    let hub_data = outer.path().join("dev-hub");
    std::fs::create_dir_all(&hub_data)?;
    let code_file = outer.path().join("access-code");
    let code = "explicit-file-code-that-is-at-least-sixteen";
    write_code_file(&code_file, code)?;

    // First Hub: persists the code + stamps now.
    {
        let hub = spawn(explicit_file_config(
            hub_data.clone(),
            code_file.clone(),
            code,
        ))
        .await?;
        login(hub.addr, code).await?;
        hub.shutdown().await;
    }

    // Backdate the persisted stamp; then REWRITE the same file so its mtime is
    // strictly newer than the stamp, as a redeploy that touched the file would.
    std::fs::write(hub_data.join("bootstrap-issued-at"), EXPIRED_STAMP)?;
    write_code_file(&code_file, code)?;

    {
        let hub = spawn(explicit_file_config(
            hub_data.clone(),
            code_file.clone(),
            code,
        ))
        .await?;
        login(hub.addr, code).await?;
        hub.shutdown().await;
    }
    Ok(())
}

/// Item 1: unchanged code AND a file mtime older than the backdated stamp —
/// neither trigger fires, so the expired stamp bytes survive the restart and
/// the login is REFUSED.
#[cfg(unix)]
#[tokio::test]
async fn unchanged_older_explicit_file_keeps_expired_login_rejected() -> Result<()> {
    let outer = tempfile::tempdir()?;
    let hub_data = outer.path().join("dev-hub");
    std::fs::create_dir_all(&hub_data)?;
    let code_file = outer.path().join("access-code");
    let code = "explicit-file-code-that-is-at-least-sixteen";
    write_code_file(&code_file, code)?;

    {
        let hub = spawn(explicit_file_config(
            hub_data.clone(),
            code_file.clone(),
            code,
        ))
        .await?;
        login(hub.addr, code).await?;
        hub.shutdown().await;
    }

    // Expire the stamp AND make the file older than it.
    let stamp_path = hub_data.join("bootstrap-issued-at");
    std::fs::write(&stamp_path, EXPIRED_STAMP)?;
    backdate_mtime(&code_file);
    let stamp_bytes_before = std::fs::read(&stamp_path)?;

    {
        let hub = spawn(explicit_file_config(
            hub_data.clone(),
            code_file.clone(),
            code,
        ))
        .await?;
        let status = login_status(hub.addr, code).await?;
        assert_eq!(
            status, 401,
            "an expired, non-revived explicit code must be refused"
        );
        // Stamp bytes are untouched by the refused login AND the restart.
        let stamp_bytes_after = std::fs::read(&stamp_path)?;
        assert_eq!(
            stamp_bytes_before, stamp_bytes_after,
            "restart plus refusal must leave the stamp byte-identical"
        );
        assert!(hub_data.join("bootstrap-token-source-explicit").is_file());
        hub.shutdown().await;
    }
    Ok(())
}

/// Item 1: a changed code re-stamps regardless of file mtime — login with the
/// new code succeeds, the old code is rejected.
#[cfg(unix)]
#[tokio::test]
async fn changed_explicit_file_code_restarts_and_login_succeeds() -> Result<()> {
    let outer = tempfile::tempdir()?;
    let hub_data = outer.path().join("dev-hub");
    std::fs::create_dir_all(&hub_data)?;
    let code_file = outer.path().join("access-code");
    let old_code = "explicit-file-code-that-is-at-least-sixteen";
    let new_code = "rotated-explicit-code-also-16-plus-x";
    write_code_file(&code_file, old_code)?;

    {
        let hub = spawn(explicit_file_config(
            hub_data.clone(),
            code_file.clone(),
            old_code,
        ))
        .await?;
        login(hub.addr, old_code).await?;
        hub.shutdown().await;
    }

    // Expire the stamp, then replace the code in the file and even backdate
    // the file: a code change must re-stamp on its own.
    std::fs::write(hub_data.join("bootstrap-issued-at"), EXPIRED_STAMP)?;
    write_code_file(&code_file, new_code)?;
    backdate_mtime(&code_file);

    {
        let hub = spawn(explicit_file_config(
            hub_data.clone(),
            code_file.clone(),
            new_code,
        ))
        .await?;
        assert_eq!(login_status(hub.addr, old_code).await?, 401);
        login(hub.addr, new_code).await?;
        hub.shutdown().await;
    }
    Ok(())
}

/// Item 2: a no-source start that fails to BIND must leave the explicit-source
/// marker (and rotation refusal) intact — provenance is adopted only after a
/// successful bind.
#[tokio::test]
async fn failed_no_source_bind_keeps_explicit_marker() -> Result<()> {
    let outer = tempfile::tempdir()?;
    let hub_data = outer.path().join("dev-hub");
    std::fs::create_dir_all(&hub_data)?;
    let code = "explicit-env-code-at-least-sixteen-x";

    // First Hub: explicit env source → marker present, holds the listener.
    let first = {
        let config = HubConfig {
            bootstrap_token: code.to_owned(),
            bootstrap_source: BootstrapSource::ExplicitEnv,
            ..HubConfig::for_test(hub_data.clone())
        };
        spawn(config).await?
    };
    let marker = hub_data.join("bootstrap-token-source-explicit");
    assert!(marker.is_file(), "explicit start writes the marker");

    // Second start: no source, but force the bind to fail by reusing the exact
    // address the first Hub is listening on.
    let mut failed = HubConfig::for_test(hub_data.clone());
    failed.bootstrap_token = String::new();
    failed.bootstrap_source = BootstrapSource::Generated;
    failed.listen = first.addr;
    let err = spawn(failed).await;
    match err {
        Err(err) => eprintln!("bind failed as expected: {err}"),
        Ok(_) => panic!("the occupied address must fail to bind"),
    }

    assert!(
        marker.is_file(),
        "a start that never bound must not remove the explicit marker"
    );
    assert!(
        remuda_hub::rotate_bootstrap(&hub_data).is_err(),
        "rotation must still refuse while the marker survives"
    );

    first.shutdown().await;
    Ok(())
}

/// Round 4 item 7 (HIGH): a marker with NO persisted token (an explicit start
/// that crashed after the marker fsync) plus a failed no-source bind must
/// leave the marker intact and still no token file — the in-memory mint must
/// not be committed before the bind, or rotation would gain authority over a
/// Hub that never started.
#[tokio::test]
async fn failed_no_source_bind_keeps_marker_and_mints_no_token() -> Result<()> {
    let outer = tempfile::tempdir()?;
    let hub_data = outer.path().join("dev-hub");
    std::fs::create_dir_all(&hub_data)?;
    let marker = hub_data.join("bootstrap-token-source-explicit");
    std::fs::write(&marker, "")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o600))?;
    }
    assert!(!hub_data.join("bootstrap-token").is_file());

    // Hold an address with a first Hub so the no-source start fails to bind.
    let holder = spawn(HubConfig::for_test(outer.path().join("holder"))).await?;

    let mut failed = HubConfig::for_test(hub_data.clone());
    failed.bootstrap_token = String::new();
    failed.bootstrap_source = BootstrapSource::Generated;
    failed.listen = holder.addr;
    let err = spawn(failed).await;
    match err {
        Err(err) => eprintln!("bind failed as expected: {err}"),
        Ok(_) => panic!("the occupied address must fail to bind"),
    }

    assert!(marker.is_file(), "the marker survives the failed start");
    assert!(
        !hub_data.join("bootstrap-token").is_file(),
        "no hub-owned token may be minted before a successful bind"
    );
    assert!(
        !hub_data.join("bootstrap-issued-at").is_file(),
        "no stamp may be written for the un-committed mint"
    );
    assert!(
        remuda_hub::rotate_bootstrap(&hub_data).is_err(),
        "rotation must keep refusing the token-less marked dir"
    );

    holder.shutdown().await;
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
