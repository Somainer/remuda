//! Push a local musl `remuda` over SSH, verify sha256, run `version`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::client::{SshClient, sh_single_quote};
use crate::error::Error;

/// Default install path on the remote host (`~` expanded via `$HOME`).
pub const DEFAULT_REMOTE_BIN: &str = "~/.local/bin/remuda";

const HASH_TIMEOUT: Duration = Duration::from_secs(20);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const VERSION_TIMEOUT: Duration = Duration::from_secs(20);

/// How a bootstrap run finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapResult {
    /// Remote file already had the same digest.
    Skipped {
        /// `sha256:` hex of the binary.
        digest: String,
        /// Remote path written (absolute).
        remote_path: String,
        /// `remuda version` stdout (may be empty if the binary could not run).
        version: String,
    },
    /// Bytes were copied, digest matched, version ran.
    Uploaded {
        /// `sha256:` hex of the binary.
        digest: String,
        /// Remote path written (absolute).
        remote_path: String,
        /// `remuda version` stdout.
        version: String,
    },
}

/// Copy `local_bin` to `remote_path` if the digest differs.
pub async fn bootstrap(
    client: &SshClient,
    local_bin: &Path,
    remote_path: &str,
) -> Result<BootstrapResult, Error> {
    if !local_bin.is_file() {
        return Err(Error::BinaryNotFound(local_bin.to_path_buf()));
    }
    let digest = sha256_file(local_bin)?;
    let dest = resolve_remote_path(client, remote_path).await?;
    if let Some(existing) = remote_digest(client, &dest).await?
        && (existing.eq_ignore_ascii_case(digest.trim_start_matches("sha256:"))
            || existing == digest)
    {
        let version = remote_version(client, &dest).await.unwrap_or_default();
        return Ok(BootstrapResult::Skipped {
            digest,
            remote_path: dest,
            version,
        });
    }
    upload(client, local_bin, &dest).await?;
    let actual = remote_digest(client, &dest)
        .await?
        .ok_or_else(|| Error::DigestMismatch {
            expected: digest.clone(),
            actual: "missing".into(),
        })?;
    let actual_full = if actual.starts_with("sha256:") {
        actual
    } else {
        format!("sha256:{actual}")
    };
    if !actual_full.eq_ignore_ascii_case(&digest) {
        return Err(Error::DigestMismatch {
            expected: digest,
            actual: actual_full,
        });
    }
    let version = remote_version(client, &dest).await?;
    Ok(BootstrapResult::Uploaded {
        digest,
        remote_path: dest,
        version,
    })
}

/// SHA-256 of a file as `sha256:` + 64 lowercase hex digits.
pub fn sha256_file(path: &Path) -> Result<String, Error> {
    let mut hasher = Sha256::new();
    let mut file = std::fs::File::open(path)?;
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

/// Look for `target/x86_64-unknown-linux-musl/release/remuda` from `cwd`.
#[must_use]
pub fn default_local_musl(cwd: &Path) -> Option<PathBuf> {
    let candidate = cwd
        .join("target")
        .join("x86_64-unknown-linux-musl")
        .join("release")
        .join("remuda");
    candidate.is_file().then_some(candidate)
}

async fn resolve_remote_path(client: &SshClient, path: &str) -> Result<String, Error> {
    validate_path_chars(path)?;
    if path == "~" {
        let home = client
            .exec(&["printenv", "HOME"], None, HASH_TIMEOUT)
            .await?
            .ok()?;
        return Ok(home.stdout.trim().to_string());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        validate_path_chars(rest)?;
        let home = client
            .exec(&["printenv", "HOME"], None, HASH_TIMEOUT)
            .await?
            .ok()?;
        return Ok(format!("{}/{}", home.stdout.trim(), rest));
    }
    Ok(path.to_string())
}

async fn remote_digest(client: &SshClient, dest: &str) -> Result<Option<String>, Error> {
    let quoted = sh_single_quote(dest)?;
    let script = format!(
        r#"if [ ! -f {quoted} ]; then echo MISSING; exit 0; fi
if command -v sha256sum >/dev/null 2>&1; then sha256sum {quoted} | awk '{{print $1}}'
elif command -v shasum >/dev/null 2>&1; then shasum -a 256 {quoted} | awk '{{print $1}}'
else openssl dgst -sha256 {quoted} | awk '{{print $NF}}'
fi"#
    );
    let output = client
        .exec(&["sh", "-c", &script], None, HASH_TIMEOUT)
        .await?
        .ok()?;
    let token = output.stdout.trim();
    if token.is_empty() || token == "MISSING" {
        return Ok(None);
    }
    Ok(Some(token.to_ascii_lowercase()))
}

async fn upload(client: &SshClient, local: &Path, dest: &str) -> Result<(), Error> {
    let parent = Path::new(dest)
        .parent()
        .ok_or_else(|| Error::InvalidPath(dest.into()))?;
    let parent_s = parent.to_string_lossy();
    let q_dir = sh_single_quote(&parent_s)?;
    let q_dest = sh_single_quote(dest)?;
    let q_tmp = sh_single_quote(&format!("{dest}.tmp"))?;
    let script = format!(
        "set -e\nmkdir -p {q_dir}\ncat > {q_tmp}\nchmod 755 {q_tmp}\nmv {q_tmp} {q_dest}\n"
    );
    client
        .exec_stdin_file(&["sh", "-c", &script], local, UPLOAD_TIMEOUT)
        .await?
        .ok()?;
    Ok(())
}

async fn remote_version(client: &SshClient, dest: &str) -> Result<String, Error> {
    let output = client
        .exec(&[dest, "version"], None, VERSION_TIMEOUT)
        .await?
        .ok()?;
    Ok(output.stdout)
}

fn validate_path_chars(path: &str) -> Result<(), Error> {
    if path.is_empty() || path.contains('\0') || path.contains('\n') || path.contains('\r') {
        return Err(Error::InvalidPath(path.into()));
    }
    Ok(())
}
