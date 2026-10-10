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

/// Set a path's atime+mtime one day in the FUTURE (Unix only), modelling clock
/// skew or a restored tree carrying a timestamp ahead of the hub clock.
#[cfg(unix)]
fn future_mtime(path: &Path) {
    use nix::sys::stat::{UtimensatFlags, utimensat};
    use nix::sys::time::TimeSpec;
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
        + 86_400;
    let ts = TimeSpec::new(secs, 0);
    utimensat(None, path, &ts, &ts, UtimensatFlags::FollowSymlink)
        .expect("utimensat postdates the access-code file");
}

fn explicit_file_config(hub_data: PathBuf, code_file: PathBuf, code: &str) -> HubConfig {
    HubConfig {
        bootstrap_token: code.to_owned(),
        bootstrap_source: BootstrapSource::ExplicitFile(code_file),
        ..HubConfig::for_test(hub_data)
    }
}

/// A config with NO explicit source: the hub owns/mints the code and writes
/// it to `data_dir/bootstrap-token` on first start.
fn minted_config(hub_data: PathBuf) -> HubConfig {
    HubConfig {
        bootstrap_token: String::new(),
        bootstrap_source: BootstrapSource::Generated,
        ..HubConfig::for_test(hub_data)
    }
}

const EXPIRED_STAMP: &str = "2000-01-01T00:00:00.000Z";

/// A current RFC3339 UTC stamp with millisecond precision (what the Hub
/// writes), for simulating the stamp-first crash window.
fn now_stamp() -> String {
    let t = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond()
    )
}

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

    // Expire the stamp content and age the access file. The persisted TOKEN
    // keeps its natural mtime (newer than the year-2000 parsed stamp): round 5
    // removed the token-mtime trigger, so this must not revive the code.
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

/// Round 5 ordering: the previous start wrote the NEW stamp but was killed
/// before the token write, leaving the OLD token on disk. The access file
/// carries the new code with an OLD mtime, so the restart must self-heal via
/// the code-change rule and login with the new code succeeds.
#[cfg(unix)]
#[tokio::test]
async fn restart_after_stamp_write_crash_repairs_and_login_succeeds() -> Result<()> {
    let outer = tempfile::tempdir()?;
    let hub_data = outer.path().join("dev-hub");
    std::fs::create_dir_all(&hub_data)?;
    let code_file = outer.path().join("access-code");
    let old_code = "old-code-before-the-kill-1234567";
    let code = "replaced-after-kill-at-least-sixteen";

    // Pre-crash state: the old code + expired stamp existed.
    std::fs::write(hub_data.join("bootstrap-token"), old_code)?;
    std::fs::write(hub_data.join("bootstrap-issued-at"), EXPIRED_STAMP)?;
    backdate_mtime(&hub_data.join("bootstrap-token"));
    backdate_mtime(&hub_data.join("bootstrap-issued-at"));

    // The killed start landed the NEW stamp (mtime now) but never the token;
    // the operator file already holds the new code, aged older than the stamp.
    std::fs::write(hub_data.join("bootstrap-issued-at"), now_stamp())?;
    write_code_file(&code_file, code)?;
    backdate_mtime(&code_file);

    let hub = spawn(explicit_file_config(
        hub_data.clone(),
        code_file.clone(),
        code,
    ))
    .await?;
    // Old code is gone (config holds the new one), new code logs in.
    assert_eq!(login_status(hub.addr, old_code).await?, 401);
    login(hub.addr, code).await?;
    assert_eq!(
        std::fs::read_to_string(hub_data.join("bootstrap-token"))?.trim(),
        code,
        "the crash window re-persisted the new token"
    );
    hub.shutdown().await;
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

/// Round 6 item 1: an ExplicitFile that is literally the Hub's own
/// `<data>/bootstrap-token` must not re-stamp on restart (the token rename
/// always lands after the stamp). After expiry a second start keeps the old
/// stamp and the code is refused.
#[tokio::test]
async fn self_token_file_keeps_expired_login_rejected_after_restart() -> Result<()> {
    let outer = tempfile::tempdir()?;
    let hub_data = outer.path().join("dev-hub");
    std::fs::create_dir_all(&hub_data)?;
    let own_token = hub_data.join("bootstrap-token");
    let code = "self-referencing-code-at-least-16-x";
    let config = || HubConfig {
        bootstrap_token: code.to_owned(),
        bootstrap_source: BootstrapSource::ExplicitFile(own_token.clone()),
        ..HubConfig::for_test(hub_data.clone())
    };

    // First start on a fresh dir persists the code and works.
    {
        let hub = spawn(config()).await?;
        login(hub.addr, code).await?;
        hub.shutdown().await;
    }

    // Expire the stamp; the token keeps its natural (newer) mtime.
    let stamp_path = hub_data.join("bootstrap-issued-at");
    std::fs::write(&stamp_path, EXPIRED_STAMP)?;
    let stamp_before = std::fs::read(&stamp_path)?;

    // Second start pointing at the same file, same code: no re-stamp, 401.
    {
        let hub = spawn(config()).await?;
        assert_eq!(login_status(hub.addr, code).await?, 401);
        assert_eq!(std::fs::read(&stamp_path)?, stamp_before);
        hub.shutdown().await;
    }
    Ok(())
}

/// Round 6 item 2: an empty presented code is refused on a healthy hub even
/// though an empty string would otherwise compare equal.
#[tokio::test]
async fn empty_presented_code_is_always_refused() -> Result<()> {
    let outer = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(outer.path().join("data"))).await?;
    assert_eq!(login_status(hub.addr, "").await?, 401);
    hub.shutdown().await;
    Ok(())
}

/// Round 6 item 2: a data dir with an EMPTY token file and a fresh stamp is
/// recovered on a no-source start — the Hub boots and mints a replacement,
/// and empty-string login is still refused.
#[tokio::test]
async fn empty_token_dir_boots_and_mints_without_accepting_empty_login() -> Result<()> {
    let outer = tempfile::tempdir()?;
    let hub_data = outer.path().join("dev-hub");
    std::fs::create_dir_all(&hub_data)?;
    std::fs::write(hub_data.join("bootstrap-token"), b"")?;

    let mut config = HubConfig::for_test(hub_data.clone());
    config.bootstrap_token = String::new();
    config.bootstrap_source = BootstrapSource::Generated;
    let hub = spawn(config).await?;
    assert_eq!(
        login_status(hub.addr, "").await?,
        401,
        "empty login stays refused"
    );
    let stored = std::fs::read_to_string(hub_data.join("bootstrap-token"))?;
    assert!(!stored.trim().is_empty());
    hub.shutdown().await;
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

/// Round 7 item 4: an access file with a FUTURE mtime (clock skew / restored
/// tree) must not re-stamp an expired bootstrap on restart — neither the first
/// restart nor the second. The expired login stays refused and the stamp bytes
/// never move; a genuine same-time touch is what re-issues.
#[cfg(unix)]
#[tokio::test]
async fn future_dated_access_file_does_not_restamp_across_restarts() -> Result<()> {
    let outer = tempfile::tempdir()?;
    let hub_data = outer.path().join("dev-hub");
    let code_file = outer.path().join("access-code");
    const CODE: &str = "explicit-file-code-at-least-sixteen";
    write_code_file(&code_file, CODE)?;

    // First start: the explicit file persists the code and a fresh stamp.
    {
        let hub = spawn(explicit_file_config(
            hub_data.clone(),
            code_file.clone(),
            CODE,
        ))
        .await?;
        login(hub.addr, CODE).await?;
        hub.shutdown().await;
    }

    // Expire the stamp and give the file a future mtime WITHOUT changing its
    // content (no code change).
    std::fs::write(hub_data.join("bootstrap-issued-at"), EXPIRED_STAMP)?;
    future_mtime(&code_file);
    let expired_bytes = std::fs::read(hub_data.join("bootstrap-issued-at"))?;

    for restart in 1..=2 {
        let hub = spawn(explicit_file_config(
            hub_data.clone(),
            code_file.clone(),
            CODE,
        ))
        .await?;
        let status = login_status(hub.addr, CODE).await?;
        assert_eq!(
            status, 401,
            "restart {restart}: the future mtime must not revive the code"
        );
        let stamp = std::fs::read(hub_data.join("bootstrap-issued-at"))?;
        assert_eq!(
            stamp, expired_bytes,
            "restart {restart}: a future-dated file must never re-stamp"
        );
        hub.shutdown().await;
    }

    // A genuine redeploy touch (mtime = now) re-issues the stamp once.
    write_code_file(&code_file, CODE)?;
    let hub = spawn(explicit_file_config(
        hub_data.clone(),
        code_file.clone(),
        CODE,
    ))
    .await?;
    login(hub.addr, CODE).await?;
    let stamp = std::fs::read(hub_data.join("bootstrap-issued-at"))?;
    assert_ne!(stamp, expired_bytes, "a real touch re-stamps");
    hub.shutdown().await;
    Ok(())
}

/// Round 7 item 5 (chosen contract: EXPLICIT precedence, documented): pointing
/// `bootstrapToken = "file:…"` at the Hub's OWN regenerated bootstrap-token
/// file (e.g. an operator passing a previously minted token back in) does NOT
/// let a later hub regen override it. The explicit source wins on every
/// restart — the file content is the sole code, it is not silently reminted,
/// rotation is refused, and the explicit marker is present. This pins the
/// existing r6 `self_token_file` behaviour so it cannot silently change to
/// "the hub's own file wins".
#[tokio::test]
async fn explicit_file_at_the_hub_token_path_pins_and_blocks_regen() -> Result<()> {
    let outer = tempfile::tempdir()?;
    let hub_data = outer.path().join("dev-hub");
    std::fs::create_dir_all(&hub_data)?;
    let token_path = hub_data.join("bootstrap-token");
    const OPERATOR_CODE: &str = "operator-pinned-code-at-least-16-chars";

    // Start 1: no explicit source. The hub mints a token into its own file.
    {
        let hub = spawn(minted_config(hub_data.clone())).await?;
        let minted = std::fs::read_to_string(&token_path)?;
        assert!(!minted.trim().is_empty(), "first start mints a token");
        assert_ne!(minted.trim(), OPERATOR_CODE);
        // The hub-minted code logs in; it stays rotateable below until the
        // operator pins an explicit source.
        login(hub.addr, minted.trim()).await?;
        hub.shutdown().await;
    }

    // The operator pins an explicit code by replacing the hub's own file and
    // pointing the source right back AT it.
    write_code_file(&token_path, OPERATOR_CODE)?;
    let marker_path = hub_data.join("bootstrap-token-source-explicit");

    for restart in 1..=2 {
        let hub = spawn(explicit_file_config(
            hub_data.clone(),
            token_path.clone(),
            OPERATOR_CODE,
        ))
        .await?;
        // The file's content logs in; a hub-minted code does not exist.
        login(hub.addr, OPERATOR_CODE).await?;
        assert_eq!(
            std::fs::read_to_string(&token_path)?.trim(),
            OPERATOR_CODE,
            "restart {restart}: the explicit file is never silently reminted"
        );
        assert!(
            marker_path.is_file(),
            "restart {restart}: pointing at the hub's own file still marks it explicit"
        );
        // Rotation is refused even though the path is the hub's mint path.
        let rotated = remuda_hub::rotate_bootstrap(&hub_data);
        assert!(
            rotated.is_err(),
            "restart {restart}: rotate-bootstrap must refuse the explicit precedence"
        );
        hub.shutdown().await;
    }

    // c-bootstrap-dev r8 item 1 (pinning freezes issued-at): age the stamp
    // past the TTL while the source still points at the Hub's own token file.
    // An unchanged code is NOT revived (no mtime rule for the self file,
    // content identical), and re-writing the SAME code doesn't help either.
    let stamp_path = hub_data.join("bootstrap-issued-at");
    std::fs::write(&stamp_path, EXPIRED_STAMP)?;
    {
        let hub = spawn(explicit_file_config(
            hub_data.clone(),
            token_path.clone(),
            OPERATOR_CODE,
        ))
        .await?;
        assert_eq!(login_status(hub.addr, OPERATOR_CODE).await?, 401);
        // The stamp is left exactly as the operator set it.
        assert_eq!(std::fs::read_to_string(&stamp_path)?.trim(), EXPIRED_STAMP);
        hub.shutdown().await;
    }

    // Recovery (1) documented in remuda-cli.md: write a DIFFERENT code to the
    // same self-referential source — a genuine content change re-stamps.
    const NEW_PINNED_CODE: &str = "operator-pinned-code-two-at-least-16-xx";
    write_code_file(&token_path, NEW_PINNED_CODE)?;
    {
        let hub = spawn(explicit_file_config(
            hub_data.clone(),
            token_path.clone(),
            NEW_PINNED_CODE,
        ))
        .await?;
        login(hub.addr, NEW_PINNED_CODE).await?;
        assert_ne!(std::fs::read_to_string(&stamp_path)?.trim(), EXPIRED_STAMP);
        hub.shutdown().await;
    }

    // Recovery (2): deleting the stamp with the code unchanged backfills it.
    std::fs::write(&stamp_path, EXPIRED_STAMP)?;
    std::fs::remove_file(&stamp_path)?;
    {
        let hub = spawn(explicit_file_config(
            hub_data.clone(),
            token_path.clone(),
            NEW_PINNED_CODE,
        ))
        .await?;
        login(hub.addr, NEW_PINNED_CODE).await?;
        assert!(stamp_path.is_file());
        hub.shutdown().await;
    }

    // A no-source restart does not "regain" rotation until the operator starts
    // once without the explicit file (the documented adoption path).
    {
        let hub = spawn(minted_config(hub_data.clone())).await?;
        login(hub.addr, OPERATOR_CODE).await?;
        assert!(
            !marker_path.is_file(),
            "a no-source start adopts and clears the marker"
        );
        let rotated = remuda_hub::rotate_bootstrap(&hub_data)?;
        assert_ne!(
            rotated, OPERATOR_CODE,
            "rotation is hub-owned again after adoption"
        );
        hub.shutdown().await;
    }
    Ok(())
}
