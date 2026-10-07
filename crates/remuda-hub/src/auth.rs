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
///
/// c-bootstrap-dev round 5: the STAMP is written BEFORE the token. A crash in
/// between therefore leaves an OLD token paired with a NEW stamp; on the next
/// start the explicit-code change check (old persisted token ≠ configured
/// code) re-persists both and self-heals. The reverse order plus a token-mtime
/// heuristic revived expired codes on every same-millisecond restart, so no
/// file-mtime comparison against the parsed stamp is allowed for the token.
pub fn persist_bootstrap(data_dir: &Path, token: &str) -> Result<(), HubError> {
    std::fs::create_dir_all(data_dir)
        .map_err(|err| HubError::Internal(format!("data dir: {err}")))?;
    write_private(&data_dir.join("bootstrap-issued-at"), &now_rfc3339())?;
    replace_private(&data_dir.join("bootstrap-token"), token)?;
    Ok(())
}

/// Atomically replace a file: write a fresh 0600 temp file in the SAME
/// directory, fsync it, rename it over `path`, and fsync the directory.
///
/// Round 6 item 2: the bootstrap token must never pass through an observable
/// truncated/empty state. A truncating open on the live token followed by a
/// crash (or `: > bootstrap-token`) used to publish an EMPTY code that login
/// accepted (`secret_eq("", "")`) for the stamp's whole TTL. A rename switches
/// the token in one step, so a crash leaves either the old file or the new one
/// — never an empty one.
fn replace_private(path: &Path, contents: &str) -> Result<(), HubError> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("bootstrap");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    let tmp = dir.join(format!(".{name}.{}.{nanos}.tmp", std::process::id()));

    let do_write = || -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&tmp, perms)?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        Ok(())
    };
    if let Err(err) = do_write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(HubError::Internal(format!("bootstrap: {err}")));
    }
    fsync_dir(dir)
}

/// Read a persisted token, returning None when the file is absent or empty
/// after trimming — both are "no usable code" (round 6 item 2).
fn read_persisted_token(path: &Path) -> Result<Option<String>, HubError> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let code = text.trim();
            Ok((!code.is_empty()).then(|| code.to_owned()))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(HubError::Internal(format!("bootstrap: {err}"))),
    }
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
/// marker. The containing directory is fsync'd after create/unlink so a crash
/// cannot lose the ordering (marker is the durable source of truth).
fn set_bootstrap_explicit_marker(data_dir: &Path, explicit: bool) -> Result<(), HubError> {
    let marker = data_dir.join(BOOTSTRAP_EXPLICIT_MARKER);
    if explicit {
        write_private(&marker, "")
    } else {
        match std::fs::remove_file(&marker) {
            Ok(()) => fsync_dir(data_dir),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(HubError::Internal(format!("bootstrap marker: {err}"))),
        }
    }
}

/// fsync the directory so a just-created/-removed entry is durable.
fn fsync_dir(dir: &Path) -> Result<(), HubError> {
    let handle = std::fs::File::open(dir)
        .map_err(|err| HubError::Internal(format!("fsync dir {}: {err}", dir.display())))?;
    handle
        .sync_all()
        .map_err(|err| HubError::Internal(format!("fsync dir {}: {err}", dir.display())))
}

/// Outcome of [`resolve_bootstrap`] the caller must honour after the Hub has
/// successfully bound its listener.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BootstrapResolution {
    /// Nothing to do post-bind.
    None,
    /// The start used no explicit source yet found an explicit-source marker:
    /// remove the marker (adopt the persisted token) only once the Hub is
    /// listening, so a failed bind leaves the provenance intact.
    AdoptAfterBind,
}

/// Resolve the bootstrap access code at startup.
///
/// c-bootstrap-dev round 3/4 crash-safe and provenance rules:
///   * EXPLICIT file/env → write the provenance marker FIRST (fsync'd), then
///     re-persist token+stamp ONLY when the code changed or (file) the file's
///     mtime is strictly newer than the parsed stamp. An unchanged env, or a
///     file older than the stamp, leaves `bootstrap-issued-at` byte-identical —
///     an expired code is NOT revived just because the process restarted.
///   * EMPTY token with no explicit source → the Hub owns the code: load a
///     persisted token (or mint one). If an explicit marker is present the
///     marker removal (and, for a fresh mint, the token write) is deferred
///     until after a successful bind ([`BootstrapResolution::AdoptAfterBind`]).
///   * a NON-EMPTY token WITHOUT an explicit source is a configuration error
///     (round 4): a programmatic caller must not hand in a code and silently
///     gain rotation authority or wipe an existing explicit marker. Nothing is
///     written.
pub fn resolve_bootstrap(config: &mut HubConfig) -> Result<BootstrapResolution, HubError> {
    let data_dir = config.data_dir.clone();
    let token_path = data_dir.join("bootstrap-token");
    let stamp_path = data_dir.join("bootstrap-issued-at");

    // 1) Explicit operator source.
    if config.bootstrap_source.is_explicit() {
        // Round 4 item 9: normalise the code exactly like the CLI's
        // SecretRef::resolve does (leading/trailing spaces and newlines come
        // from `$'secret\n'` or a file with a trailing newline). Without this
        // a raw env value with padding differed from the trimmed persisted
        // token on every start, so the code re-stamped forever and the TTL
        // never elapsed.
        config.bootstrap_token = config.bootstrap_token.trim().to_owned();
        // Round 4 item 2: an empty (or whitespace-only) explicit code must be
        // refused BEFORE writing anything — accepting it would let an empty
        // string log in (secret_eq("", "") passes) or overwrite a previously
        // persisted real code, and writing the marker first would additionally
        // lock rotation onto the empty value.
        if config.bootstrap_token.is_empty() {
            return Err(HubError::Internal(
                "explicit bootstrap access code is empty; provide a non-empty \
                 --access-code-file / REMUDA_BOOTSTRAP_TOKEN value"
                    .to_owned(),
            ));
        }
        // Marker FIRST, durable, before any token/stamp write — on crash the
        // marker still correctly says "explicit; do not rotate".
        std::fs::create_dir_all(&data_dir)
            .map_err(|err| HubError::Internal(format!("data dir: {err}")))?;
        write_private(&data_dir.join(BOOTSTRAP_EXPLICIT_MARKER), "")?;
        fsync_dir(&data_dir)?;

        let persisted = if token_path.is_file() {
            std::fs::read_to_string(&token_path)
                .map_err(|err| HubError::Internal(format!("bootstrap: {err}")))?
                .trim()
                .to_string()
        } else {
            String::new()
        };
        let code_changed = persisted != config.bootstrap_token;
        let file_newer_than_stamp = match &config.bootstrap_source {
            crate::config::BootstrapSource::ExplicitFile(path) => {
                file_mtime_newer_than_stamp(path, &stamp_path)?
            }
            crate::config::BootstrapSource::ExplicitEnv
            | crate::config::BootstrapSource::Generated
            | crate::config::BootstrapSource::Adopted => false,
        };
        // persist_bootstrap now writes the STAMP first and the TOKEN second:
        // a crash between the two leaves an old token with a new stamp, and
        // the `code_changed` rule above re-persists both on retry. There must
        // be NO token-mtime trigger here — the token file is written after the
        // stamp on every healthy persist, so its nanosecond mtime is normally
        // newer than the millisecond-precision parsed stamp; comparing them
        // re-stamped (and revived expired codes) on every same-millisecond
        // restart, and a `cp -r` without -p did so once after a restore.
        if code_changed || file_newer_than_stamp {
            persist_bootstrap(&data_dir, &config.bootstrap_token)?;
        } else if bootstrap_issued_at(&data_dir).is_none() {
            // Backfill a stamp on an unchanged code when there is no USABLE
            // one: a missing file (crash between writes, pre-D-018 dir,
            // hand-provisioned token) or an EMPTY file (a kill during
            // write_private after the truncating open, or `: >
            // bootstrap-issued-at`). bootstrap_within_ttl fails open for
            // exactly this condition, so without a backfill the code never
            // expires. First sight is the issue time. A non-empty but
            // malformed stamp is left alone — it is treated as expired.
            write_private(&stamp_path, &now_rfc3339())?;
        }
        // Otherwise leave bootstrap-token AND bootstrap-issued-at untouched.
        return Ok(BootstrapResolution::None);
    }

    // 2) A non-empty token with no explicit provenance is refused: the token
    //    field is for the operator-supplied code together with a source; the
    //    Hub mints (empty token) or loads, never adopts a bare caller value.
    if !config.bootstrap_token.is_empty() {
        return Err(HubError::Internal(
            "bootstrap_token is set without an explicit BootstrapSource; pass \
             BootstrapSource::ExplicitFile/ExplicitEnv for an operator code, \
             or leave the token empty for the Hub to mint"
                .to_owned(),
        ));
    }

    // 3) No configured token: load a usable persisted one or mint. A present
    //    but EMPTY token (a crash during a replace, or a hand-truncated file)
    //    is "no usable code" too — adopting "" would accept empty-string
    //    logins — so it falls through to the mint path. Marker adoption is
    //    deferred to post-bind.
    if let Some(code) = read_persisted_token(&token_path)? {
        config.bootstrap_token = code;
        // Backfill a missing OR EMPTY stamp; treat first sight as issue time.
        if bootstrap_issued_at(&data_dir).is_none() {
            write_private(&stamp_path, &now_rfc3339())?;
        }
        config.bootstrap_source = crate::config::BootstrapSource::Adopted;
        // Adopt the provenance only after a successful bind if an explicit
        // marker is currently present.
        return Ok(if bootstrap_source_is_explicit(&data_dir) {
            BootstrapResolution::AdoptAfterBind
        } else {
            BootstrapResolution::None
        });
    }

    // No usable token persisted: mint a hub-generated token IN MEMORY.
    config.bootstrap_token = random_token();
    config.bootstrap_source = crate::config::BootstrapSource::Generated;
    if bootstrap_source_is_explicit(&data_dir) {
        // Round 4 item 7 (HIGH): an explicit start that crashed AFTER the
        // marker fsync but BEFORE persisting a token left marker-present +
        // no-token. Minting and unlinking the marker here (before
        // Store::open/bind) reopened that crash window: a later start failure
        // would leave a hub-owned token with no marker and rotation authority.
        // Persist nothing; adopt_bootstrap_after_bind commits the minted code
        // and removes the marker TOGETHER, only after a successful bind.
        return Ok(BootstrapResolution::AdoptAfterBind);
    }
    // Normal fresh mint on an unmarked dir: no provenance ever existed, so it
    // is safe to persist before bind.
    std::fs::create_dir_all(&data_dir)
        .map_err(|err| HubError::Internal(format!("data dir: {err}")))?;
    set_bootstrap_explicit_marker(&data_dir, false)?;
    persist_bootstrap(&data_dir, &config.bootstrap_token)?;
    Ok(BootstrapResolution::None)
}

/// Whether the operator access-code FILE's mtime is strictly newer than the
/// parsed bootstrap stamp (round 3: a touched/redeployed file re-stamps).
/// Applies ONLY to that external file — never to the persisted token, which is
/// written after the stamp by [`persist_bootstrap`] and so is normally newer.
/// An unreadable/absent file or an unparseable stamp returns false (the
/// conservative choice: do not revive the TTL on uncertainty).
fn file_mtime_newer_than_stamp(file: &Path, stamp: &Path) -> Result<bool, HubError> {
    let Ok(meta) = std::fs::metadata(file) else {
        return Ok(false);
    };
    let Ok(stamp_text) = std::fs::read_to_string(stamp) else {
        return Ok(false);
    };
    let Ok(parsed) = time::OffsetDateTime::parse(
        stamp_text.trim(),
        &time::format_description::well_known::Rfc3339,
    ) else {
        return Ok(false);
    };
    let Ok(modified) = meta.modified() else {
        return Ok(false);
    };
    Ok(time::OffsetDateTime::from(modified) > parsed)
}

/// Commit the post-bind adoption of provenance. Called only when
/// [`resolve_bootstrap`] returned [`BootstrapResolution::AdoptAfterBind`], so
/// a failed bind leaves the marker (and rotation refusal) intact.
///
/// Two pre-bind states reach this point:
///   * a persisted token existed (the usual adopt): the caller passes it but
///     nothing is overwritten — only the marker is removed;
///   * no token existed and `resolve_bootstrap` minted one in memory (round 4
///     item 7 recovery path): the minted code is persisted here and the marker
///     removed together, so the directory never carries a hub-owned token
///     without the marker — a crash between the two writes leaves marker +
///     token, and the next successful start re-enters the first case and
///     finishes the adoption.
pub fn adopt_bootstrap_after_bind(data_dir: &Path, minted_token: &str) -> Result<(), HubError> {
    // An EMPTY token is as good as absent: replace it with the minted code so
    // an empty code is never adopted (round 6 item 2).
    if read_persisted_token(&data_dir.join("bootstrap-token"))?.is_none() {
        persist_bootstrap(data_dir, minted_token)?;
    }
    set_bootstrap_explicit_marker(data_dir, false)
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
    use crate::config::{BootstrapSource, HubConfig, secret_eq};
    use axum::http::HeaderValue;

    /// The expired stamp every round-3 test backdates to.
    const EXPIRED_STAMP: &str = "2000-01-01T00:00:00.000Z";
    /// 1999-12-31T00:00:00Z: strictly older than [`EXPIRED_STAMP`].
    const MTIME_1999: i64 = 946_598_400;

    /// Backdate a path's atime+mtime (Unix only) so the file's mtime compares
    /// older than the year-2000 test stamp.
    #[cfg(unix)]
    fn backdate_mtime(path: &Path, seconds: i64) {
        set_mtime(path, seconds, 0);
    }

    /// Set a path's atime+mtime with nanosecond precision (Unix only).
    #[cfg(unix)]
    fn set_mtime(path: &Path, seconds: i64, nanos: i64) {
        use nix::sys::stat::{UtimensatFlags, utimensat};
        use nix::sys::time::TimeSpec;
        let ts = TimeSpec::new(seconds, nanos);
        utimensat(None, path, &ts, &ts, UtimensatFlags::FollowSymlink)
            .expect("utimensat sets the file mtime");
    }

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
        config.bootstrap_source = BootstrapSource::Generated;
        resolve_bootstrap(&mut config).expect("resolve");
        assert_eq!(config.bootstrap_token, "legacy-code");
        assert!(
            bootstrap_issued_at(dir.path()).is_some(),
            "resolve must backfill the issued-at stamp"
        );
    }

    /// Set up a data dir holding `code` with the year-2000 expired stamp, and
    /// return the raw stamp bytes callers compare against after a restart.
    ///
    /// Round 5: only the STAMP is backdated — the token file keeps its natural
    /// (now) mtime. Since persist_bootstrap writes the stamp before the token,
    /// a healthy token is normally NEWER than its parsed stamp; nothing may
    /// read that as a reason to re-stamp.
    fn expired_token_setup(dir: &Path, code: &str) -> Vec<u8> {
        persist_bootstrap(dir, code).expect("persist");
        write_private(&dir.join("bootstrap-issued-at"), EXPIRED_STAMP).expect("stamp");
        std::fs::read(dir.join("bootstrap-issued-at")).expect("raw stamp bytes")
    }

    /// c-bootstrap-dev round 3: the FIRST explicit-source start persists the
    /// code, writes the provenance marker, and rotation refuses.
    #[test]
    fn explicit_access_code_file_writes_explicit_marker() {
        let dir = tempfile::tempdir().expect("data dir");
        let code_file = dir.path().join("access-code");
        write_private(&code_file, "new-code-from-file").expect("code file");

        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = "new-code-from-file".to_owned();
        config.bootstrap_source = BootstrapSource::ExplicitFile(code_file);
        let resolution = resolve_bootstrap(&mut config).expect("resolve");
        assert_eq!(resolution, BootstrapResolution::None);

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

    /// Round 4 item 9: a padded explicit code (`"  code\n"`, the shape of
    /// `$'secret\n'` or a file with a trailing newline) is trimmed before
    /// persistence AND before the restart comparison. Repeated starts with the
    /// same padded value must therefore be "unchanged" — no re-stamp every
    /// restart — and the stored code carries no padding.
    #[cfg(unix)]
    #[test]
    fn padded_explicit_env_code_is_trimmed_and_does_not_restamp() {
        let dir = tempfile::tempdir().expect("data dir");

        // First start: padded env value persists the trimmed code + stamp.
        let mut cfg1 = HubConfig::for_test(dir.path().to_path_buf());
        cfg1.bootstrap_token = "  padded-secret\n".to_owned();
        cfg1.bootstrap_source = BootstrapSource::ExplicitEnv;
        resolve_bootstrap(&mut cfg1).expect("first resolve");
        assert_eq!(cfg1.bootstrap_token, "padded-secret");
        assert_eq!(
            std::fs::read(dir.path().join("bootstrap-token")).unwrap(),
            b"padded-secret",
            "the persisted token is stored without padding"
        );

        // Expire the stamp content only; the token keeps its natural mtime.
        write_private(&dir.path().join("bootstrap-issued-at"), EXPIRED_STAMP).expect("expired");
        let stamp_bytes = std::fs::read(dir.path().join("bootstrap-issued-at")).unwrap();

        // Second start with the SAME padded env value: trimmed to the same
        // code, so nothing is re-persisted; the expired stamp survives.
        let mut cfg2 = HubConfig::for_test(dir.path().to_path_buf());
        cfg2.bootstrap_token = "  padded-secret\n".to_owned();
        cfg2.bootstrap_source = BootstrapSource::ExplicitEnv;
        resolve_bootstrap(&mut cfg2).expect("second resolve");
        assert_eq!(
            std::fs::read(dir.path().join("bootstrap-issued-at")).unwrap(),
            stamp_bytes,
            "an unchanged padded value must not re-stamp"
        );
        assert_eq!(
            std::fs::read(dir.path().join("bootstrap-token")).unwrap(),
            b"padded-secret"
        );
        assert!(!bootstrap_within_ttl(dir.path(), 24));

        // A third start with a different padded value DOES re-stamp.
        let mut cfg3 = HubConfig::for_test(dir.path().to_path_buf());
        cfg3.bootstrap_token = "\trotated-secret  ".to_owned();
        cfg3.bootstrap_source = BootstrapSource::ExplicitEnv;
        resolve_bootstrap(&mut cfg3).expect("third resolve");
        assert_eq!(cfg3.bootstrap_token, "rotated-secret");
        assert_ne!(
            std::fs::read(dir.path().join("bootstrap-issued-at")).unwrap(),
            stamp_bytes
        );
        assert!(bootstrap_within_ttl(dir.path(), 24));
    }

    /// Item 1: an unchanged FILE code whose mtime is OLDER than the persisted
    /// stamp leaves the stamp byte-identical across a restart — an expired
    /// code must not be revived by a redeploy that did not touch the file.
    #[cfg(unix)]
    #[test]
    fn unchanged_explicit_file_code_older_than_stamp_keeps_expired_stamp_bytes() {
        let dir = tempfile::tempdir().expect("data dir");
        let stamp_bytes = expired_token_setup(dir.path(), "same-code");

        let code_file = dir.path().join("access-code");
        write_private(&code_file, "same-code").expect("code file");
        backdate_mtime(&code_file, MTIME_1999);

        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = "same-code".to_owned();
        config.bootstrap_source = BootstrapSource::ExplicitFile(code_file);
        resolve_bootstrap(&mut config).expect("resolve");

        let after = std::fs::read(dir.path().join("bootstrap-issued-at")).expect("read stamp");
        assert_eq!(
            after, stamp_bytes,
            "an unchanged, older-than-stamp file must not re-stamp"
        );
        let token = std::fs::read(dir.path().join("bootstrap-token")).expect("read token");
        assert_eq!(token, b"same-code");
        assert!(!bootstrap_within_ttl(dir.path(), 24), "stamp stays expired");
        assert!(dir.path().join(BOOTSTRAP_EXPLICIT_MARKER).is_file());
        assert!(rotate_bootstrap(dir.path()).is_err());
    }

    /// Item 1: an unchanged ENV code (no mtime exists) also keeps the expired
    /// stamp bytes across a restart.
    #[test]
    fn unchanged_explicit_env_code_keeps_expired_stamp_bytes() {
        let dir = tempfile::tempdir().expect("data dir");
        let stamp_bytes = expired_token_setup(dir.path(), "same-env-code");

        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = "same-env-code".to_owned();
        config.bootstrap_source = BootstrapSource::ExplicitEnv;
        resolve_bootstrap(&mut config).expect("resolve");

        let after = std::fs::read(dir.path().join("bootstrap-issued-at")).expect("read stamp");
        assert_eq!(
            after, stamp_bytes,
            "an unchanged env code must not re-stamp"
        );
        assert!(!bootstrap_within_ttl(dir.path(), 24), "stamp stays expired");
        assert!(dir.path().join(BOOTSTRAP_EXPLICIT_MARKER).is_file());
        assert!(rotate_bootstrap(dir.path()).is_err());
    }

    /// Item 1: a CHANGED code re-stamps (minted-style fresh stamp) even when
    /// the file's mtime is older than the old stamp — the operator genuinely
    /// rotated the code.
    #[cfg(unix)]
    #[test]
    fn changed_explicit_file_code_restamps_even_with_old_mtime() {
        let dir = tempfile::tempdir().expect("data dir");
        let stamp_bytes = expired_token_setup(dir.path(), "old-code");

        let code_file = dir.path().join("access-code");
        write_private(&code_file, "brand-new-code").expect("code file");
        backdate_mtime(&code_file, MTIME_1999);

        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = "brand-new-code".to_owned();
        config.bootstrap_source = BootstrapSource::ExplicitFile(code_file);
        resolve_bootstrap(&mut config).expect("resolve");

        let after = std::fs::read(dir.path().join("bootstrap-issued-at")).expect("read stamp");
        assert_ne!(after, stamp_bytes, "a changed code must re-stamp");
        let token = std::fs::read_to_string(dir.path().join("bootstrap-token")).expect("read");
        assert_eq!(token.trim(), "brand-new-code");
        assert!(
            bootstrap_within_ttl(dir.path(), 24),
            "the new code is fresh"
        );
    }

    /// Item 1: a changed ENV code re-stamps too.
    #[test]
    fn changed_explicit_env_code_restamps() {
        let dir = tempfile::tempdir().expect("data dir");
        let stamp_bytes = expired_token_setup(dir.path(), "old-env-code");

        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = "brand-new-env-code".to_owned();
        config.bootstrap_source = BootstrapSource::ExplicitEnv;
        resolve_bootstrap(&mut config).expect("resolve");

        let after = std::fs::read(dir.path().join("bootstrap-issued-at")).expect("read stamp");
        assert_ne!(after, stamp_bytes);
        let token = std::fs::read_to_string(dir.path().join("bootstrap-token")).expect("read");
        assert_eq!(token.trim(), "brand-new-env-code");
        assert!(bootstrap_within_ttl(dir.path(), 24));
    }

    /// Item 1: the same code in a file whose mtime is strictly NEWER than the
    /// backdated stamp re-stamps (the operator touched/redeployed the file).
    #[test]
    fn touched_explicit_file_same_code_restamps() {
        let dir = tempfile::tempdir().expect("data dir");
        let stamp_bytes = expired_token_setup(dir.path(), "same-code");

        // A freshly written file has mtime = now, strictly newer than 2000.
        let code_file = dir.path().join("access-code");
        write_private(&code_file, "same-code").expect("code file");

        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = "same-code".to_owned();
        config.bootstrap_source = BootstrapSource::ExplicitFile(code_file);
        resolve_bootstrap(&mut config).expect("resolve");

        let after = std::fs::read(dir.path().join("bootstrap-issued-at")).expect("read stamp");
        assert_ne!(after, stamp_bytes, "a newer-than-stamp file must re-stamp");
        assert!(bootstrap_within_ttl(dir.path(), 24));
    }

    /// Item 2: a no-source start that finds the explicit marker defers marker
    /// removal until after a successful bind. `resolve_bootstrap` itself must
    /// leave the marker (and rotation refusal) intact and report the deferred
    /// adoption; [`adopt_bootstrap_after_bind`] performs the removal.
    #[test]
    fn no_source_start_defers_marker_removal_until_adopted() {
        let dir = tempfile::tempdir().expect("data dir");
        // Start 1: explicit env source → marker present, expired stamp.
        expired_token_setup(dir.path(), "adoptable-code");
        let mut cfg1 = HubConfig::for_test(dir.path().to_path_buf());
        cfg1.bootstrap_token = "adoptable-code".to_owned();
        cfg1.bootstrap_source = BootstrapSource::ExplicitEnv;
        let resolution1 = resolve_bootstrap(&mut cfg1).expect("resolve start1");
        assert_eq!(resolution1, BootstrapResolution::None);
        assert!(dir.path().join(BOOTSTRAP_EXPLICIT_MARKER).is_file());
        assert!(rotate_bootstrap(dir.path()).is_err());

        // Start 2: no source. The token is adopted in memory, but the marker
        // must survive until the caller confirms a successful bind.
        let mut cfg2 = HubConfig::for_test(dir.path().to_path_buf());
        cfg2.bootstrap_token = String::new();
        cfg2.bootstrap_source = BootstrapSource::Generated;
        let resolution2 = resolve_bootstrap(&mut cfg2).expect("resolve start2");
        assert_eq!(
            resolution2,
            BootstrapResolution::AdoptAfterBind,
            "marker removal is deferred to post-bind"
        );
        assert_eq!(cfg2.bootstrap_token, "adoptable-code");
        assert!(
            dir.path().join(BOOTSTRAP_EXPLICIT_MARKER).is_file(),
            "a not-yet-bound start must keep the marker"
        );
        assert!(
            rotate_bootstrap(dir.path()).is_err(),
            "rotation stays refused before the bind is established"
        );

        // Bind succeeded: adopt the provenance, rotation now allowed.
        adopt_bootstrap_after_bind(dir.path(), &cfg2.bootstrap_token).expect("adopt after bind");
        assert!(!dir.path().join(BOOTSTRAP_EXPLICIT_MARKER).is_file());
        let new = rotate_bootstrap(dir.path()).expect("rotation now allowed");
        assert_ne!(new, "adoptable-code");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("bootstrap-token"))
                .expect("read")
                .trim(),
            new
        );
    }

    /// Item 2: an explicit start with an unchanged code writes ONLY the marker
    /// — token and stamp bytes are not touched.
    #[cfg(unix)]
    #[test]
    fn unchanged_explicit_start_writes_only_the_marker() {
        let dir = tempfile::tempdir().expect("data dir");
        let stamp_bytes = expired_token_setup(dir.path(), "same-code");
        let token_bytes = std::fs::read(dir.path().join("bootstrap-token")).expect("token");

        let code_file = dir.path().join("access-code");
        write_private(&code_file, "same-code").expect("code file");
        backdate_mtime(&code_file, MTIME_1999);

        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = "same-code".to_owned();
        config.bootstrap_source = BootstrapSource::ExplicitFile(code_file);
        resolve_bootstrap(&mut config).expect("resolve");

        assert_eq!(
            std::fs::read(dir.path().join("bootstrap-issued-at")).expect("read stamp"),
            stamp_bytes
        );
        assert_eq!(
            std::fs::read(dir.path().join("bootstrap-token")).expect("read token"),
            token_bytes
        );
        assert!(dir.path().join(BOOTSTRAP_EXPLICIT_MARKER).is_file());
    }

    /// c-bootstrap-dev round 2: rotation with no token file is a clean error.
    #[test]
    fn rotate_without_token_refuses() {
        let dir = tempfile::tempdir().expect("data dir");
        let err = rotate_bootstrap(dir.path()).expect_err("must refuse without a token");
        assert!(format!("{err}").contains("no bootstrap-token"));
    }

    /// Round 4 item 1: a non-empty token with the default (Generated) source
    /// is refused WITHOUT writing anything — in particular an existing
    /// explicit marker must survive, so rotation keeps refusing.
    #[test]
    fn sourceless_token_is_refused_and_leaves_the_marker_intact() {
        let dir = tempfile::tempdir().expect("data dir");
        let marker = dir.path().join(BOOTSTRAP_EXPLICIT_MARKER);
        write_private(&marker, "").expect("marker");

        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = "bare-caller-code".to_owned();
        config.bootstrap_source = BootstrapSource::Generated;
        let err = resolve_bootstrap(&mut config).expect_err("sourceless token refused");
        assert!(
            format!("{err}").contains("BootstrapSource"),
            "error explains the provenance requirement: {err}"
        );

        assert!(marker.is_file(), "the refusal must not remove the marker");
        assert!(
            !dir.path().join("bootstrap-token").is_file(),
            "the refusal must not mint a token"
        );
        assert!(rotate_bootstrap(dir.path()).is_err());

        // The same rule applies to the Adopted state (set only by resolution
        // itself; a caller must not forge it with a value either).
        let dir2 = tempfile::tempdir().expect("data dir 2");
        let mut config2 = HubConfig::for_test(dir2.path().to_path_buf());
        config2.bootstrap_token = "bare-caller-code".to_owned();
        config2.bootstrap_source = BootstrapSource::Adopted;
        assert!(resolve_bootstrap(&mut config2).is_err());
    }

    /// Round 4 item 2: empty or whitespace-only explicit codes are refused
    /// before anything is written (no marker, no token, no stamp).
    #[test]
    fn empty_explicit_code_is_rejected_before_writes() {
        for code in ["", "   ", "\n\t "] {
            let dir = tempfile::tempdir().expect("data dir");
            let mut config = HubConfig::for_test(dir.path().to_path_buf());
            config.bootstrap_token = code.to_owned();
            config.bootstrap_source = BootstrapSource::ExplicitEnv;
            let err = resolve_bootstrap(&mut config).expect_err("empty explicit refused");
            assert!(format!("{err}").contains("empty"), "got: {err}");
            assert!(!dir.path().join(BOOTSTRAP_EXPLICIT_MARKER).is_file());
            assert!(!dir.path().join("bootstrap-token").is_file());
            assert!(!dir.path().join("bootstrap-issued-at").is_file());
        }
    }

    /// Round 4 item 2: an empty explicit code on an EXISTING hub-owned dir
    /// must not overwrite the real token or stamp and must not write the
    /// marker (so rotation stays available).
    #[test]
    fn empty_explicit_code_does_not_overwrite_existing_token() {
        let dir = tempfile::tempdir().expect("data dir");
        persist_bootstrap(dir.path(), "real-code").expect("persist");
        let token_bytes = std::fs::read(dir.path().join("bootstrap-token")).expect("token");
        let stamp_bytes = std::fs::read(dir.path().join("bootstrap-issued-at")).expect("stamp");

        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = String::new();
        config.bootstrap_source = BootstrapSource::ExplicitEnv;
        assert!(resolve_bootstrap(&mut config).is_err());

        assert_eq!(
            std::fs::read(dir.path().join("bootstrap-token")).unwrap(),
            token_bytes
        );
        assert_eq!(
            std::fs::read(dir.path().join("bootstrap-issued-at")).unwrap(),
            stamp_bytes
        );
        assert!(!dir.path().join(BOOTSTRAP_EXPLICIT_MARKER).is_file());
        let new = rotate_bootstrap(dir.path()).expect("hub-owned dir still rotates");
        assert_ne!(new, "real-code");
    }

    /// Round 4 item 3: an explicit start backfills a MISSING stamp on an
    /// unchanged code (crash between persist's two writes, pre-D-018 dir, or
    /// hand-provisioned token), and a later unchanged restart with an expired
    /// stamp does not revive it — round-3 semantics still hold.
    #[cfg(unix)]
    #[test]
    fn explicit_start_backfills_missing_stamp_then_does_not_restamp() {
        let dir = tempfile::tempdir().expect("data dir");

        // Simulate the crash window: token present, stamp absent.
        write_private(&dir.path().join("bootstrap-token"), "same-code").expect("token");
        let code_file = dir.path().join("access-code");
        write_private(&code_file, "same-code").expect("code file");
        backdate_mtime(&code_file, MTIME_1999);

        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = "same-code".to_owned();
        config.bootstrap_source = BootstrapSource::ExplicitFile(code_file.clone());
        resolve_bootstrap(&mut config).expect("resolve backfills");

        let stamp = bootstrap_issued_at(dir.path()).expect("a missing stamp is backfilled");
        let parsed =
            time::OffsetDateTime::parse(&stamp, &time::format_description::well_known::Rfc3339)
                .expect("parseable now-stamp");
        assert!(
            (time::OffsetDateTime::now_utc() - parsed) < time::Duration::seconds(60),
            "the backfilled stamp records the current time"
        );
        assert!(bootstrap_within_ttl(dir.path(), 24));

        // Expire the backfilled stamp, restart with the same code and a file
        // older than the stamp: it must not be revived. The token keeps its
        // natural mtime — no heuristic may read that as a repair trigger.
        write_private(&dir.path().join("bootstrap-issued-at"), EXPIRED_STAMP).expect("expired");
        backdate_mtime(&code_file, MTIME_1999);
        let expired_bytes = std::fs::read(dir.path().join("bootstrap-issued-at")).expect("bytes");
        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = "same-code".to_owned();
        config.bootstrap_source = BootstrapSource::ExplicitFile(code_file);
        resolve_bootstrap(&mut config).expect("resolve unchanged");
        assert_eq!(
            std::fs::read(dir.path().join("bootstrap-issued-at")).unwrap(),
            expired_bytes,
            "the expired stamp is not revived"
        );
        assert!(!bootstrap_within_ttl(dir.path(), 24));
    }

    /// Round 5 item 2: an EMPTY stamp file (a kill during write_private after
    /// the truncating open, or `: > bootstrap-issued-at`) is the same fail-open
    /// condition as a missing stamp. An explicit start with an unchanged code
    /// must backfill now, and a later expiry still applies.
    #[test]
    fn explicit_start_backfills_an_empty_stamp_file() {
        for empty in ["", "  \n\t "] {
            let dir = tempfile::tempdir().expect("data dir");
            write_private(&dir.path().join("bootstrap-token"), "same-code").expect("token");
            write_private(&dir.path().join("bootstrap-issued-at"), empty).expect("empty stamp");
            assert!(
                bootstrap_issued_at(dir.path()).is_none(),
                "an empty/whitespace stamp reads as absent"
            );

            let mut config = HubConfig::for_test(dir.path().to_path_buf());
            config.bootstrap_token = "same-code".to_owned();
            config.bootstrap_source = BootstrapSource::ExplicitEnv;
            resolve_bootstrap(&mut config).expect("resolve backfills");

            let stamp = bootstrap_issued_at(dir.path())
                .expect("an empty stamp is backfilled to a usable now-stamp");
            let parsed =
                time::OffsetDateTime::parse(&stamp, &time::format_description::well_known::Rfc3339)
                    .expect("parseable");
            assert!(
                (time::OffsetDateTime::now_utc() - parsed) < time::Duration::seconds(60),
                "the backfill records the current time"
            );
        }
    }

    /// Round 5 item 2: a no-source start that loads a persisted token next to
    /// an empty stamp file backfills the stamp rather than failing open forever.
    #[test]
    fn no_source_start_backfills_an_empty_stamp_file() {
        let dir = tempfile::tempdir().expect("data dir");
        write_private(&dir.path().join("bootstrap-token"), "legacy-code").expect("token");
        write_private(&dir.path().join("bootstrap-issued-at"), "").expect("empty stamp");

        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = String::new();
        config.bootstrap_source = BootstrapSource::Generated;
        resolve_bootstrap(&mut config).expect("resolve");
        assert_eq!(config.bootstrap_token, "legacy-code");
        assert!(
            bootstrap_issued_at(dir.path()).is_some(),
            "the no-source adoption path backfills an empty stamp too"
        );
    }

    /// Round 5 item 2 negative: a NON-empty malformed stamp is not backfilled
    /// — it is treated as expired (the empty-file fail-open is what is closed).
    #[test]
    fn malformed_nonempty_stamp_is_not_backfilled() {
        let dir = tempfile::tempdir().expect("data dir");
        write_private(&dir.path().join("bootstrap-token"), "same-code").expect("token");
        write_private(&dir.path().join("bootstrap-issued-at"), "not-a-timestamp")
            .expect("malformed stamp");

        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = "same-code".to_owned();
        config.bootstrap_source = BootstrapSource::ExplicitEnv;
        resolve_bootstrap(&mut config).expect("resolve");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("bootstrap-issued-at"))
                .unwrap()
                .trim(),
            "not-a-timestamp",
            "a non-empty malformed stamp is left as-is"
        );
        assert!(!bootstrap_within_ttl(dir.path(), 24));
    }

    /// Round 4 item 6 (chosen contract): there is NO path-based magic that
    /// treats an ExplicitFile pointing at the Hub's own minted
    /// `bootstrap-token` as hub-owned. Feeding that file back through
    /// `bootstrapToken = "file:…/bootstrap-token"` (the old m1 example) marks
    /// it explicit like any other operator file, so rotate-bootstrap refuses;
    /// deploy examples must point at a separate operator file instead.
    #[test]
    fn explicit_file_equal_to_the_hubs_own_token_file_is_still_explicit() {
        let dir = tempfile::tempdir().expect("data dir");
        let own_token = dir.path().join("bootstrap-token");
        persist_bootstrap(dir.path(), "hub-minted-code").expect("hub minted first");
        assert!(
            rotate_bootstrap(dir.path()).is_ok(),
            "hub-owned rotates before"
        );

        // Re-feed the Hub's own token file as the configured source.
        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = "hub-minted-code".to_owned();
        config.bootstrap_source = BootstrapSource::ExplicitFile(own_token.clone());
        resolve_bootstrap(&mut config).expect("explicit resolve");
        assert!(
            dir.path().join(BOOTSTRAP_EXPLICIT_MARKER).is_file(),
            "self-referencing file still writes the explicit marker"
        );
        assert!(
            rotate_bootstrap(dir.path()).is_err(),
            "rotation is now refused: the example must not suggest this path"
        );
    }

    /// Round 4 item 7 (HIGH): marker present + NO persisted token (the crash
    /// window between the explicit marker fsync and the token write). A
    /// no-source start must mint ONLY in memory and defer persisting the code
    /// and removing the marker to after a successful bind — a failed bind
    /// leaves the marker intact and no token file, so rotation cannot mint
    /// over the directory.
    #[test]
    fn marker_without_token_defers_the_mint_to_after_bind() {
        let dir = tempfile::tempdir().expect("data dir");
        std::fs::create_dir_all(dir.path()).expect("data dir");
        let marker = dir.path().join(BOOTSTRAP_EXPLICIT_MARKER);
        write_private(&marker, "").expect("crashed explicit start's marker");
        let token_path = dir.path().join("bootstrap-token");
        let stamp_path = dir.path().join("bootstrap-issued-at");

        // No-source start: resolve mints in memory, writes NOTHING.
        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = String::new();
        config.bootstrap_source = BootstrapSource::Generated;
        let resolution = resolve_bootstrap(&mut config).expect("resolve");
        assert_eq!(resolution, BootstrapResolution::AdoptAfterBind);
        assert!(
            config.bootstrap_token.len() >= 16,
            "a fresh code is minted for the running Hub"
        );
        let minted = config.bootstrap_token.clone();
        assert!(!token_path.is_file(), "no token before bind");
        assert!(!stamp_path.is_file(), "no stamp before bind");
        assert!(
            marker.is_file(),
            "the marker survives a not-yet-bound start"
        );
        assert!(
            rotate_bootstrap(dir.path()).is_err(),
            "rotation refuses while the marker guards the token-less dir"
        );

        // Simulate a FAILED bind: no adopt call. A second no-source start
        // reaches the same state and still writes nothing.
        let mut retry = HubConfig::for_test(dir.path().to_path_buf());
        retry.bootstrap_token = String::new();
        retry.bootstrap_source = BootstrapSource::Generated;
        assert_eq!(
            resolve_bootstrap(&mut retry).expect("retry resolve"),
            BootstrapResolution::AdoptAfterBind
        );
        assert!(!token_path.is_file());
        assert!(marker.is_file());
        assert!(rotate_bootstrap(dir.path()).is_err());

        // Bind succeeded: adopt commits the minted token AND clears the marker.
        adopt_bootstrap_after_bind(dir.path(), &retry.bootstrap_token)
            .expect("adopt persists the mint and removes the marker");
        assert!(
            token_path.is_file(),
            "the minted token lands only after bind"
        );
        assert!(stamp_path.is_file());
        assert!(!marker.is_file(), "the marker is removed with the commit");
        assert_eq!(
            std::fs::read_to_string(&token_path).unwrap().trim(),
            retry.bootstrap_token
        );
        let new = rotate_bootstrap(dir.path()).expect("rotation allowed post-bind");
        assert_ne!(new, minted);
    }

    /// Round 5 (replaces round-4 item 8): persist_bootstrap now writes the
    /// STAMP first and the TOKEN second. A kill between the two leaves the OLD
    /// token paired with a NEW stamp; on restart the configured new code
    /// differs from the persisted old one (`code_changed`), so the pair is
    /// re-persisted and the code is usable. This crash shape also covers a
    /// `cp -r`/`scp -r` restore (mtimes not preserved) because no mtime
    /// comparison against the token exists anymore.
    #[cfg(unix)]
    #[test]
    fn crash_between_stamp_and_token_writes_repairs_via_code_change() {
        let dir = tempfile::tempdir().expect("data dir");

        // The previous start had the old code + old stamp.
        expired_token_setup(dir.path(), "previous-code");

        // The new start wrote the NEW stamp but was KILLED before the token
        // write: fresh stamp, still-old token. The operator file holds the new
        // code with an OLD mtime, so only code_changed can repair.
        write_private(&dir.path().join("bootstrap-issued-at"), &now_rfc3339())
            .expect("new stamp landed");
        let code_file = dir.path().join("access-code");
        write_private(&code_file, "replaced-code").expect("code file");
        backdate_mtime(&code_file, MTIME_1999);
        // Token still says the OLD code.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("bootstrap-token"))
                .unwrap()
                .trim(),
            "previous-code"
        );

        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = "replaced-code".to_owned();
        config.bootstrap_source = BootstrapSource::ExplicitFile(code_file);
        resolve_bootstrap(&mut config).expect("restart self-heals");

        assert!(
            bootstrap_within_ttl(dir.path(), 24),
            "the new code is usable"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("bootstrap-token"))
                .unwrap()
                .trim(),
            "replaced-code"
        );
    }

    /// Round 5 negative (the round-3 revival regression): a healthy pair has
    /// the token file mtime NEWER than the parsed millisecond stamp (stamp is
    /// written first). With an EXPIRED stamp and the token exactly 500 µs
    /// newer, an unchanged code and an old access file must leave the stamp
    /// byte-identical — no token-mtime trigger may exist.
    #[cfg(unix)]
    #[test]
    fn token_half_ms_newer_than_expired_stamp_does_not_restamp() {
        let dir = tempfile::tempdir().expect("data dir");
        write_private(&dir.path().join("bootstrap-token"), "same-code").expect("token");
        // Expired stamp with a fractional part: 2000-01-01T00:00:00.123Z.
        const STAMP_TEXT: &str = "2000-01-01T00:00:00.123Z";
        write_private(&dir.path().join("bootstrap-issued-at"), STAMP_TEXT).expect("stamp");
        // Token mtime = parsed stamp + 500 µs (same millisecond, greater).
        set_mtime(
            &dir.path().join("bootstrap-token"),
            946_684_800,
            123_500_000,
        );

        let code_file = dir.path().join("access-code");
        write_private(&code_file, "same-code").expect("code file");
        backdate_mtime(&code_file, MTIME_1999);

        let stamp_bytes = std::fs::read(dir.path().join("bootstrap-issued-at")).unwrap();
        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = "same-code".to_owned();
        config.bootstrap_source = BootstrapSource::ExplicitFile(code_file);
        resolve_bootstrap(&mut config).expect("resolve");

        assert_eq!(
            std::fs::read(dir.path().join("bootstrap-issued-at")).unwrap(),
            stamp_bytes,
            "a token mtime 500 µs newer than the parsed stamp must not re-stamp"
        );
        assert!(
            !bootstrap_within_ttl(dir.path(), 24),
            "the expired code stays expired"
        );
    }

    /// Round 5 negative (fresh variant): a healthy now-pair (stamp written
    /// first, token fractions later) with an unchanged code and an old access
    /// file is left untouched on an ordinary restart.
    #[cfg(unix)]
    #[test]
    fn healthy_token_stamp_pair_with_old_file_does_not_trigger_repair() {
        let dir = tempfile::tempdir().expect("data dir");
        persist_bootstrap(dir.path(), "same-code")
            .expect("persist writes the stamp first, then the token");
        let stamp_bytes = std::fs::read(dir.path().join("bootstrap-issued-at")).expect("stamp");

        let code_file = dir.path().join("access-code");
        write_private(&code_file, "same-code").expect("code file");
        backdate_mtime(&code_file, MTIME_1999);

        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = "same-code".to_owned();
        config.bootstrap_source = BootstrapSource::ExplicitFile(code_file);
        resolve_bootstrap(&mut config).expect("resolve");
        assert_eq!(
            std::fs::read(dir.path().join("bootstrap-issued-at")).unwrap(),
            stamp_bytes,
            "a healthy pair is left untouched"
        );
    }

    /// Round 6 item 2: an EMPTY token file is treated as "no usable code" and
    /// the Hub MINTS a replacement on a no-source start (never adopts "").
    #[test]
    fn empty_token_file_on_no_source_start_is_minted_over() {
        let dir = tempfile::tempdir().expect("data dir");
        write_private(&dir.path().join("bootstrap-token"), "").expect("empty token");
        write_private(&dir.path().join("bootstrap-issued-at"), &now_rfc3339()).expect("stamp");

        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = String::new();
        config.bootstrap_source = BootstrapSource::Generated;
        let resolution = resolve_bootstrap(&mut config).expect("resolve");
        assert_eq!(resolution, BootstrapResolution::None);
        assert!(
            !config.bootstrap_token.is_empty(),
            "a new code is minted, not \"\""
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("bootstrap-token"))
                .unwrap()
                .trim(),
            config.bootstrap_token,
            "the empty file is replaced by the minted code"
        );
        assert!(bootstrap_within_ttl(dir.path(), 24));
    }

    /// Round 6 item 2: marker present + EMPTY token — the replacement mint is
    /// committed only after bind, like the missing-token recovery path.
    #[test]
    fn empty_token_with_marker_defers_mint_to_after_bind() {
        let dir = tempfile::tempdir().expect("data dir");
        write_private(&dir.path().join(BOOTSTRAP_EXPLICIT_MARKER), "").expect("marker");
        write_private(&dir.path().join("bootstrap-token"), "").expect("empty token");
        write_private(&dir.path().join("bootstrap-issued-at"), &now_rfc3339()).expect("stamp");

        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.bootstrap_token = String::new();
        config.bootstrap_source = BootstrapSource::Generated;
        assert_eq!(
            resolve_bootstrap(&mut config).expect("resolve"),
            BootstrapResolution::AdoptAfterBind
        );
        assert!(!config.bootstrap_token.is_empty());
        // Before bind the empty token and marker are untouched.
        assert_eq!(
            std::fs::read(dir.path().join("bootstrap-token")).unwrap(),
            b""
        );
        assert!(dir.path().join(BOOTSTRAP_EXPLICIT_MARKER).is_file());

        adopt_bootstrap_after_bind(dir.path(), &config.bootstrap_token).expect("adopt");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("bootstrap-token"))
                .unwrap()
                .trim(),
            config.bootstrap_token
        );
        assert!(!dir.path().join(BOOTSTRAP_EXPLICIT_MARKER).is_file());
    }
}
