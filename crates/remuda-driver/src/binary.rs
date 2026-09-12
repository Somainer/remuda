//! Absolute path, `--version`, and SHA-256 pin for a native agent binary.

use crate::error::{DriverError, DriverResult};
use remuda_protocol::Digest;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pinned native executable recorded in a [`crate::LaunchRecipe`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinaryPin {
    /// Canonical absolute path of the file that will be exec'd.
    pub abs_path: String,
    /// First line of `--version` stdout (stderr fallback), trimmed.
    pub version: String,
    /// SHA-256 of the file contents, `sha256:` prefixed.
    pub sha256: Digest,
}

/// Resolve `command` on `PATH` or accept an absolute path, then pin it.
pub fn pin_binary(command: impl AsRef<Path>) -> DriverResult<BinaryPin> {
    let abs = resolve_binary(command)?;
    let version = read_version(&abs)?;
    let sha256 = hash_file(&abs)?;
    tracing::info!(
        path = %abs.display(),
        version = %version,
        digest = %String::from(sha256.clone()),
        "pinned native binary"
    );
    Ok(BinaryPin {
        abs_path: abs.to_string_lossy().into_owned(),
        version,
        sha256,
    })
}

/// Resolve a command name or path to a canonical absolute file.
pub fn resolve_binary(command: impl AsRef<Path>) -> DriverResult<PathBuf> {
    let command = command.as_ref();
    let candidate = if command.is_absolute() {
        command.to_path_buf()
    } else {
        find_on_path(command).ok_or_else(|| DriverError::BinaryNotFound(command.to_path_buf()))?
    };
    if !candidate.is_file() {
        return Err(DriverError::BinaryNotFound(candidate));
    }
    candidate.canonicalize().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            DriverError::BinaryNotFound(candidate)
        } else {
            DriverError::Io(error)
        }
    })
}

/// SHA-256 a file at `path`.
pub fn hash_file(path: &Path) -> DriverResult<Digest> {
    let mut hasher = Sha256::new();
    let mut file = File::open(path)?;
    let mut buf = [0_u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    digest_from_sha(hasher.finalize())
}

/// SHA-256 in-memory bytes (settings overlay, tests).
pub fn hash_bytes(bytes: &[u8]) -> DriverResult<Digest> {
    digest_from_sha(Sha256::digest(bytes))
}

fn digest_from_sha(hash: impl core::fmt::LowerHex) -> DriverResult<Digest> {
    let encoded = format!("sha256:{hash:x}");
    Digest::try_from(encoded).map_err(DriverError::Protocol)
}

fn read_version(path: &Path) -> DriverResult<String> {
    let output = Command::new(path).arg("--version").output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| {
            DriverError::InvalidLaunchSpec(format!(
                "{} produced no --version output",
                path.display()
            ))
        })?;
    Ok(line.to_string())
}

fn find_on_path(name: &Path) -> Option<PathBuf> {
    let file_name = name.file_name()?;
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(file_name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Default executable name for a driver kind.
pub fn default_command(kind: remuda_protocol::DriverKind) -> Option<&'static str> {
    use remuda_protocol::DriverKind::*;
    match kind {
        ClaudePrint | ClaudePty | ClaudeBg => Some("claude"),
        CodexAppserver => Some("codex"),
        GrokAcp => Some("grok"),
        AgyPrint => Some("agy"),
        GenericPty => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn pin_is_idempotent_for_a_stub_binary() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_stub(dir.path(), "stub-9.9.9 (test)");
        let first = pin_binary(&path).unwrap();
        let second = pin_binary(&path).unwrap();
        assert_eq!(first, second);
        assert!(first.abs_path.starts_with('/'));
        assert_eq!(first.version, "stub-9.9.9 (test)");
        assert!(String::from(first.sha256.clone()).starts_with("sha256:"));
    }

    fn write_stub(dir: &Path, version: &str) -> PathBuf {
        let path = dir.join("agent");
        std::fs::write(&path, format!("#!/bin/sh\necho '{version}'\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
}
