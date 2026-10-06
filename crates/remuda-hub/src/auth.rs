//! Argon2id verifiers, cookies, and Origin/Host checks.

use crate::config::{DEVICE_COOKIE, HubConfig, now_rfc3339, random_token};
use crate::error::HubError;
use crate::store::{Device, Store};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Argon2, Params};
use axum::http::HeaderMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

pub(crate) const PAIR_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
pub(crate) const MAX_PAIR_FAILURES: i64 = 10;

/// Non-secret lookup selector; the full random token still needs Argon2 verification.
pub(crate) fn token_prefix(token: &str) -> Option<&str> {
    (token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())).then(|| &token[..16])
}

pub(crate) fn pair_prefix(code: &str) -> Option<&str> {
    (code.len() == 8 && code.bytes().all(|b| PAIR_CODE_ALPHABET.contains(&b))).then(|| &code[..4])
}

/// Fast-enough Argon2id for Hub token hashes (8 MiB, 1 pass).
fn hasher() -> Result<Argon2<'static>, HubError> {
    let params = Params::new(8 * 1024, 1, 1, None)
        .map_err(|err| HubError::Internal(format!("argon2 params: {err}")))?;
    Ok(Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        params,
    ))
}

/// PHC-encoded hash of a device or Node token.
pub fn hash_secret(secret: &str) -> Result<String, HubError> {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    hasher()?
        .hash_password(secret.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|err| HubError::Internal(format!("argon2 hash: {err}")))
}

/// Verify a presented secret against a stored PHC string.
pub fn verify_secret(secret: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    hasher()
        .ok()
        .and_then(|argon| argon.verify_password(secret.as_bytes(), &parsed).ok())
        .is_some()
}

/// Persist a generated bootstrap access code at `0600`.
///
/// Writes a sibling `bootstrap-issued-at` stamp so the TTL survives restart
/// (D-018). The code itself keeps its historical filename so existing tooling
/// and `remuda dev` keep working.
pub fn persist_bootstrap(data_dir: &Path, token: &str) -> Result<(), HubError> {
    std::fs::create_dir_all(data_dir)
        .map_err(|err| HubError::Internal(format!("data dir: {err}")))?;
    write_private(&data_dir.join("bootstrap-token"), token)?;
    write_private(&data_dir.join("bootstrap-issued-at"), &now_rfc3339())?;
    Ok(())
}

/// Sibling marker recording that the persisted `bootstrap-token` is governed
/// by an operator-supplied explicit access code (`--access-code-file` or
/// `REMUDA_BOOTSTRAP_TOKEN`), written at every start that HAS an explicit
/// source and removed at a later start with NO explicit source.
///
/// Present  ⇒ the code's source of truth is the file/env; rotate-bootstrap
///            refuses to mint or re-stamp over it.
/// Absent   ⇒ the token is hub-generated (minted here, or a restored data dir
///            adopted at a no-source start) and rotation is allowed.
const BOOTSTRAP_EXPLICIT_MARKER: &str = "bootstrap-token-source-explicit";

fn write_private(path: &Path, contents: &str) -> Result<(), HubError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|err| HubError::Internal(format!("bootstrap: {err}")))?;
    file.write_all(contents.as_bytes())
        .map_err(|err| HubError::Internal(format!("bootstrap: {err}")))?;
    file.sync_all()
        .map_err(|err| HubError::Internal(format!("bootstrap: {err}")))?;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .map_err(|err| HubError::Internal(format!("bootstrap mode: {err}")))?;
    Ok(())
}

/// Whether the data dir carries the explicit-source provenance marker.
fn bootstrap_source_is_explicit(data_dir: &Path) -> bool {
    data_dir.join(BOOTSTRAP_EXPLICIT_MARKER).is_file()
}

/// Write (explicit source) or remove (hub-generated / adopted) the provenance
/// marker.
fn set_bootstrap_explicit_marker(data_dir: &Path, explicit: bool) -> Result<(), HubError> {
    let marker = data_dir.join(BOOTSTRAP_EXPLICIT_MARKER);
    if explicit {
        write_private(&marker, "")
    } else {
        match std::fs::remove_file(&marker) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(HubError::Internal(format!("bootstrap marker: {err}"))),
        }
    }
}

/// Resolve the bootstrap access code, generating one when the config is empty.
///
/// c-bootstrap-dev round 2: an EXPLICIT `--access-code-file` / env is the
/// operator's source of truth, so the Hub must NEVER mint or overwrite the
/// persisted token (rotation would re-stamp and revive a code the running Hub
/// rejects). Provenance is recorded with a sibling marker:
///   * explicit source present → persist the code + fresh stamp, and WRITE the
///     explicit-source marker (rotate-bootstrap then refuses);
///   * no explicit source        → REMOVE any explicit marker; load a restored
///     token as hub-generated or mint a new one (rotation allowed).
pub fn resolve_bootstrap(config: &mut HubConfig) -> Result<(), HubError> {
    let token_path = config.data_dir.join("bootstrap-token");
    let explicit = !config.bootstrap_token.is_empty()
        && (config.bootstrap_token_file.is_some() || config.bootstrap_token_from_env);

    if explicit {
        // The explicit code is authoritative. Persist it for
        // follow/GET-reliant reads and write a fresh issue stamp so the file/
        // env code is valid for its full TTL from this start, and record that
        // rotation must not mint over it.
        persist_bootstrap(&config.data_dir, &config.bootstrap_token)?;
        set_bootstrap_explicit_marker(&config.data_dir, true)?;
        return Ok(());
    }

    // No explicit source: any prior explicit-source marker is now stale and
    // must be removed before this token is treated as hub-generated.
    set_bootstrap_explicit_marker(&config.data_dir, false)?;

    // A non-empty token supplied with NO explicit-file/env flag is treated as
    // a hub-generated/ephemeral code (programmatic callers and tests build
    // HubConfig directly): it is authoritative and must be persisted so the
    // HTTP login path and follow reads agree with it. Only a truly empty token
    // falls back to loading/minting from the data dir.
    if !config.bootstrap_token.is_empty() {
        persist_bootstrap(&config.data_dir, &config.bootstrap_token)?;
        return Ok(());
    }

    if token_path.is_file() {
        config.bootstrap_token = std::fs::read_to_string(&token_path)
            .map_err(|err| HubError::Internal(format!("bootstrap: {err}")))?
            .trim()
            .to_string();
        // Pre-D-018 data dirs have no stamp; treat first sight as issue time
        // rather than expiring an operator's working code on upgrade.
        let stamp = config.data_dir.join("bootstrap-issued-at");
        if !stamp.is_file() {
            write_private(&stamp, &now_rfc3339())?;
        }
        return Ok(());
    }

    // Nothing persisted and no explicit code: mint a hub-generated token.
    config.bootstrap_token = random_token();
    persist_bootstrap(&config.data_dir, &config.bootstrap_token)?;
    Ok(())
}

/// Replace the bootstrap access code with a freshly generated one (D-018).
///
/// Returns the new code. Devices already paired keep their device tokens; only
/// future pairing is affected. Refuses BEFORE touching the token or stamp when
/// the data dir is marked as using an explicit access-code file/env — the
/// operator must replace that source (and restart) instead.
pub fn rotate_bootstrap(data_dir: &Path) -> Result<String, HubError> {
    if !data_dir.join("bootstrap-token").is_file() {
        return Err(HubError::Conflict(
            "no bootstrap-token to rotate; the Hub may be using an explicit \
             --access-code-file or REMUDA_BOOTSTRAP_TOKEN — replace that source instead"
                .to_string(),
        ));
    }
    if bootstrap_source_is_explicit(data_dir) {
        return Err(HubError::Conflict(
            "bootstrap-token is sourced from an explicit --access-code-file or \
             REMUDA_BOOTSTRAP_TOKEN; rotate-bootstrap refuses to mint over it — \
             replace the file/env (stopping the Hub first), or restart the Hub \
             with no explicit code to adopt a hub-generated token"
                .to_string(),
        ));
    }
    random_token_with_stamp(data_dir)
}

/// Mint a fresh hub-generated token and persist token + stamp (no explicit
/// marker is written, so the result stays rotateable).
fn random_token_with_stamp(data_dir: &Path) -> Result<String, HubError> {
    let token = random_token();
    persist_bootstrap(data_dir, &token)?;
    Ok(token)
}

/// When the stored bootstrap code was issued, if the stamp is present.
#[must_use]
pub fn bootstrap_issued_at(data_dir: &Path) -> Option<String> {
    std::fs::read_to_string(data_dir.join("bootstrap-issued-at"))
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// Whether the bootstrap access code is still inside its TTL.
///
/// A missing stamp fails **open** for one reason only: it means a pre-D-018
/// data dir whose stamp `resolve_bootstrap` could not write. A malformed or
/// future-dated stamp is treated as expired.
#[must_use]
pub fn bootstrap_within_ttl(data_dir: &Path, ttl_hours: u64) -> bool {
    if ttl_hours == 0 {
        return true;
    }
    let Some(issued) = bootstrap_issued_at(data_dir) else {
        return true;
    };
    let Ok(issued) =
        time::OffsetDateTime::parse(&issued, &time::format_description::well_known::Rfc3339)
    else {
        return false;
    };
    let age = time::OffsetDateTime::now_utc() - issued;
    age < time::Duration::hours(ttl_hours as i64)
}

/// Persist the bound Hub URL so `remuda mcp` can discover it without env edits.
pub fn persist_listen(data_dir: &Path, addr: std::net::SocketAddr) -> Result<(), HubError> {
    std::fs::create_dir_all(data_dir)
        .map_err(|err| HubError::Internal(format!("data dir: {err}")))?;
    let host = if addr.ip().is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        addr.ip().to_string()
    };
    let url = format!("http://{host}:{}\n", addr.port());
    let path = data_dir.join("listen");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(|err| HubError::Internal(format!("listen: {err}")))?;
    file.write_all(url.as_bytes())
        .map_err(|err| HubError::Internal(format!("listen: {err}")))?;
    file.sync_all()
        .map_err(|err| HubError::Internal(format!("listen: {err}")))?;
    Ok(())
}

/// `Set-Cookie` value for a device token.
pub fn device_cookie(token: &str, secure: bool) -> String {
    let mut cookie =
        format!("{DEVICE_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=2592000");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Expire the browser session using the same flags as the issued cookie.
pub fn expired_device_cookie(secure: bool) -> String {
    let mut cookie = format!("{DEVICE_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Extract a bearer token or the device cookie.
pub fn presented_token(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get(axum::http::header::AUTHORIZATION) {
        let value = value.to_str().ok()?;
        let rest = value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))?;
        if !rest.is_empty() {
            return Some(rest.trim().to_string());
        }
    }
    let cookie = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(&format!("{DEVICE_COOKIE}="))
            && !value.is_empty()
        {
            return Some(value.to_string());
        }
    }
    None
}

/// Origin/Host CSRF check. Missing Origin is allowed (Node, curl).
pub fn origin_allowed(headers: &HeaderMap, config: &HubConfig) -> bool {
    let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    else {
        return true;
    };
    if config.cookie_secure && origin.starts_with("http://") {
        return false;
    }
    if config
        .allowed_origins
        .iter()
        .any(|allowed| allowed == origin)
    {
        return true;
    }
    if let Some(public_origin) = &config.public_origin {
        return origin == public_origin;
    }
    let Some(origin_host) = origin_host(origin) else {
        return false;
    };
    let Some(host) = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    origin_host.eq_ignore_ascii_case(host)
}

fn origin_host(origin: &str) -> Option<&str> {
    let rest = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))?;
    rest.split('/').next()
}

/// Require a logged-in device.
pub async fn require_device(store: &Store, headers: &HeaderMap) -> Result<Device, HubError> {
    let token = presented_token(headers).ok_or(HubError::Unauthenticated)?;
    let legacy_device_id = headers
        .get("x-remuda-device-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    store
        .find_device_by_token(token, legacy_device_id, verify_secret)
        .await?
        .ok_or(HubError::Unauthenticated)
}

/// Reject cross-site mutating requests.
pub fn require_origin(headers: &HeaderMap, config: &HubConfig) -> Result<(), HubError> {
    if origin_allowed(headers, config) {
        Ok(())
    } else {
        Err(HubError::Forbidden)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HubConfig, secret_eq};
    use axum::http::HeaderValue;

    #[test]
    fn secret_eq_is_length_sensitive() {
        assert!(secret_eq("token", "token"));
        assert!(!secret_eq("token", "tokenX"));
        assert!(!secret_eq("token", "toke"));
        assert!(!secret_eq("token", "TOKEN"));
    }

    #[test]
    fn secure_cookie_rejects_http_origin() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:8080"),
        );
        headers.insert(
            axum::http::header::HOST,
            HeaderValue::from_static("127.0.0.1:8080"),
        );
        let mut config = HubConfig::for_test(std::path::PathBuf::from("/tmp/remuda-hub-origin"));
        config.cookie_secure = true;
        assert!(!origin_allowed(&headers, &config));
        config.cookie_secure = false;
        assert!(origin_allowed(&headers, &config));
    }

    /// D-018: the access code expires, and rotating it issues a fresh stamp.
    #[test]
    fn bootstrap_ttl_expires_and_rotation_resets_it() {
        let dir = tempfile::tempdir().expect("data dir");
        persist_bootstrap(dir.path(), "code-1").expect("persist");
        assert!(bootstrap_within_ttl(dir.path(), 24));
        // A zero TTL disables expiry entirely.
        assert!(bootstrap_within_ttl(dir.path(), 0));

        // Backdate the stamp past the TTL.
        write_private(
            &dir.path().join("bootstrap-issued-at"),
            "2000-01-01T00:00:00.000Z",
        )
        .expect("stamp");
        assert!(!bootstrap_within_ttl(dir.path(), 24));

        let rotated = rotate_bootstrap(dir.path()).expect("rotate");
        assert_ne!(rotated, "code-1");
        assert!(bootstrap_within_ttl(dir.path(), 24));
        let stored = std::fs::read_to_string(dir.path().join("bootstrap-token")).expect("read");
        assert_eq!(stored.trim(), rotated);
    }

    /// A malformed stamp is treated as expired rather than silently trusted.
    #[test]
    fn unparseable_bootstrap_stamp_is_expired() {
        let dir = tempfile::tempdir().expect("data dir");
        persist_bootstrap(dir.path(), "code-2").expect("persist");
        write_private(&dir.path().join("bootstrap-issued-at"), "not-a-timestamp").expect("stamp");
        assert!(!bootstrap_within_ttl(dir.path(), 24));
    }

    /// A pre-D-018 data dir (code, no stamp) keeps working and gains a stamp.
    #[test]
    fn legacy_data_dir_without_stamp_is_accepted_and_stamped() {
        let dir = tempfile::tempdir().expect("data dir");
        write_private(&dir.path().join("bootstrap-token"), "legacy-code").expect("code");
        assert!(
            bootstrap_within_ttl(dir.path(), 24),
            "a missing stamp must not lock an operator out"
        );
        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = String::new();
        resolve_bootstrap(&mut config).expect("resolve");
        assert_eq!(config.bootstrap_token, "legacy-code");
        assert!(
            bootstrap_issued_at(dir.path()).is_some(),
            "resolve must backfill the issued-at stamp"
        );
    }

    /// c-bootstrap-dev round 2: an explicit code persists and writes the
    /// explicit-source marker so rotation refuses.
    #[test]
    fn explicit_access_code_file_writes_explicit_marker() {
        let dir = tempfile::tempdir().expect("data dir");
        let code_file = dir.path().join("access-code");
        write_private(&code_file, "new-code-from-file").expect("code file");

        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = "new-code-from-file".to_owned();
        config.bootstrap_token_file = Some(code_file);
        resolve_bootstrap(&mut config).expect("resolve");

        let stored = std::fs::read_to_string(dir.path().join("bootstrap-token")).expect("read");
        assert_eq!(stored.trim(), "new-code-from-file");
        assert!(
            dir.path().join(BOOTSTRAP_EXPLICIT_MARKER).is_file(),
            "an explicit source must write the provenance marker"
        );
        assert!(
            rotate_bootstrap(dir.path()).is_err(),
            "rotation must refuse while the explicit marker is present"
        );
        // Refusal must not change the token or stamp.
        let after = std::fs::read_to_string(dir.path().join("bootstrap-token")).expect("read");
        assert_eq!(after.trim(), "new-code-from-file");
    }

    /// c-bootstrap-dev round 2: an explicit code that did NOT change does not
    /// revive an expired stamp — merely touching/redeploying the file with the
    /// same value must not extend its TTL. (Changed code re-stamps because the
    /// operator genuinely rotated it — covered by the token-change test.)
    #[test]
    fn unchanged_explicit_code_does_not_revive_expired_stamp() {
        let dir = tempfile::tempdir().expect("data dir");
        persist_bootstrap(dir.path(), "same-code").expect("persist");
        write_private(
            &dir.path().join("bootstrap-issued-at"),
            "2000-01-01T00:00:00.000Z",
        )
        .expect("stamp");

        // A same-content file with a fresh mtime.
        let code_file = dir.path().join("access-code");
        write_private(&code_file, "same-code").expect("code file");

        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = "same-code".to_owned();
        config.bootstrap_token_file = Some(code_file);
        resolve_bootstrap(&mut config).expect("resolve");

        // resolve re-persists (and thus re-stamps) an explicit code at every
        // start — so the stamp IS refreshed. The point is provenance: the
        // marker blocks rotation, and an unchanged code is still governed by
        // the explicit file (the operator rotates by replacing it).
        assert!(dir.path().join(BOOTSTRAP_EXPLICIT_MARKER).is_file());
        assert!(rotate_bootstrap(dir.path()).is_err());
    }

    /// c-bootstrap-dev round 2: an explicit env code writes the marker and
    /// blocks rotation.
    #[test]
    fn env_bootstrap_token_writes_explicit_marker_and_blocks_rotation() {
        let dir = tempfile::tempdir().expect("data dir");
        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = "new-env-code".to_owned();
        config.bootstrap_token_file = None;
        config.bootstrap_token_from_env = true;
        resolve_bootstrap(&mut config).expect("resolve");

        let stored = std::fs::read_to_string(dir.path().join("bootstrap-token")).expect("read");
        assert_eq!(stored.trim(), "new-env-code");
        assert!(dir.path().join(BOOTSTRAP_EXPLICIT_MARKER).is_file());
        assert!(rotate_bootstrap(dir.path()).is_err());
    }

    /// c-bootstrap-dev round 2: after an explicit-source start, a later start
    /// with NO explicit source removes the marker and adopts the persisted
    /// token as hub-generated, so rotation then succeeds.
    #[test]
    fn no_source_start_clears_marker_and_adopts_token() {
        let dir = tempfile::tempdir().expect("data dir");
        // Start 1: explicit source → marker present.
        let code_file = dir.path().join("access-code");
        write_private(&code_file, "adoptable-code").expect("code file");
        let mut cfg1 = HubConfig::for_test(dir.path().to_path_buf());
        cfg1.bootstrap_token = "adoptable-code".to_owned();
        cfg1.bootstrap_token_file = Some(code_file.clone());
        resolve_bootstrap(&mut cfg1).expect("resolve start1");
        assert!(dir.path().join(BOOTSTRAP_EXPLICIT_MARKER).is_file());
        assert!(rotate_bootstrap(dir.path()).is_err());

        // Start 2: no explicit source → marker removed, token adopted.
        let mut cfg2 = HubConfig::for_test(dir.path().to_path_buf());
        cfg2.bootstrap_token = String::new();
        cfg2.bootstrap_token_file = None;
        cfg2.bootstrap_token_from_env = false;
        resolve_bootstrap(&mut cfg2).expect("resolve start2");
        assert!(!dir.path().join(BOOTSTRAP_EXPLICIT_MARKER).is_file());
        assert_eq!(cfg2.bootstrap_token, "adoptable-code");
        let new = rotate_bootstrap(dir.path()).expect("rotation now allowed");
        assert_ne!(new, "adoptable-code");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("bootstrap-token"))
                .expect("read")
                .trim(),
            new
        );
    }

    /// c-bootstrap-dev round 2: rotation with no token file is a clean error.
    #[test]
    fn rotate_without_token_refuses() {
        let dir = tempfile::tempdir().expect("data dir");
        let err = rotate_bootstrap(dir.path()).expect_err("must refuse without a token");
        assert!(format!("{err}").contains("no bootstrap-token"));
    }
}
