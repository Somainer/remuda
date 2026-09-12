//! Argon2id verifiers, cookies, and Origin/Host checks.

use crate::config::{DEVICE_COOKIE, HubConfig, random_token};
use crate::error::HubError;
use crate::store::{Device, Store};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Argon2, Params};
use axum::http::HeaderMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

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

/// Persist a generated bootstrap token at `0600`.
pub fn persist_bootstrap(data_dir: &Path, token: &str) -> Result<(), HubError> {
    std::fs::create_dir_all(data_dir)
        .map_err(|err| HubError::Internal(format!("data dir: {err}")))?;
    let path = data_dir.join("bootstrap-token");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(|err| HubError::Internal(format!("bootstrap: {err}")))?;
    file.write_all(token.as_bytes())
        .map_err(|err| HubError::Internal(format!("bootstrap: {err}")))?;
    file.sync_all()
        .map_err(|err| HubError::Internal(format!("bootstrap: {err}")))?;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(&path, perms)
        .map_err(|err| HubError::Internal(format!("bootstrap mode: {err}")))?;
    Ok(())
}

/// Resolve the bootstrap token, generating one when the config is empty.
pub fn resolve_bootstrap(config: &mut HubConfig) -> Result<(), HubError> {
    let path = config.data_dir.join("bootstrap-token");
    if config.bootstrap_token.is_empty() {
        if path.is_file() {
            config.bootstrap_token = std::fs::read_to_string(&path)
                .map_err(|err| HubError::Internal(format!("bootstrap: {err}")))?
                .trim()
                .to_string();
            return Ok(());
        }
        config.bootstrap_token = random_token();
    }
    if !path.is_file() {
        persist_bootstrap(&config.data_dir, &config.bootstrap_token)?;
    }
    Ok(())
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
    store
        .find_device_by_token(token, verify_secret)
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
}
