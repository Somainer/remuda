//! Persisted host enrollment (`host_id` + node token) under the Node data dir.

use crate::NodeError;
use crate::identity::load_or_create_host_id;
use remuda_protocol::HostId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};

const ENROLLMENT_FILE: &str = "enrollment.json";

/// Durable Node identity presented on `node.hello` / `node.auth`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Enrollment {
    /// Persisted `hst_…` identity.
    pub host_id: HostId,
    /// Host token from Hub hello; used as Bearer / `node.auth`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_token: Option<String>,
}

/// Default data directory: `$REMUDA_DATA_DIR`, else XDG/home `remuda`.
#[must_use]
pub fn default_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("REMUDA_DATA_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("remuda");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/remuda");
    }
    PathBuf::from("data")
}

/// Load `enrollment.json`, creating a host id when missing.
pub fn load_or_create(data_dir: &Path) -> Result<Enrollment, NodeError> {
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join(ENROLLMENT_FILE);
    if path.is_file() {
        let text = std::fs::read_to_string(&path)?;
        return serde_json::from_str(&text).map_err(NodeError::from);
    }
    let host_id = load_or_create_host_id(&data_dir.join("node"))?;
    let enrollment = Enrollment {
        host_id,
        node_token: std::env::var("REMUDA_BOOTSTRAP_TOKEN")
            .ok()
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty()),
    };
    save(data_dir, &enrollment)?;
    Ok(enrollment)
}

/// Write `enrollment.json` with owner-only permissions.
pub fn save(data_dir: &Path, enrollment: &Enrollment) -> Result<(), NodeError> {
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join(ENROLLMENT_FILE);
    let temporary = data_dir.join(format!(".enrollment-{}.tmp", uuid::Uuid::new_v4()));
    let encoded = serde_json::to_vec_pretty(enrollment)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<(), NodeError> {
        let mut file = options.open(&temporary)?;
        file.write_all(&encoded)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)?;
        std::fs::File::open(data_dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

pub(crate) fn persist_host_token(data_dir: &Path, token: &str) -> Result<(), NodeError> {
    let directory = data_dir.join("node");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join("host-token");
    let temporary = directory.join(format!(".host-token-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<(), NodeError> {
        let mut file = options.open(&temporary)?;
        file.write_all(token.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)?;
        std::fs::File::open(&directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

/// Merge Hub `node.hello` result (`hostId` / `nodeToken`) into the enrollment file.
pub fn apply_hello_result(data_dir: &Path, result: &Value) -> Result<Enrollment, NodeError> {
    let mut enrollment = load_or_create(data_dir)?;
    if let Some(host_id) = result
        .get("hostId")
        .and_then(Value::as_str)
        .or_else(|| result.pointer("/result/hostId").and_then(Value::as_str))
    {
        enrollment.host_id = host_id.parse()?;
    }
    if let Some(token) = result
        .get("nodeToken")
        .and_then(Value::as_str)
        .or_else(|| result.pointer("/result/nodeToken").and_then(Value::as_str))
        .filter(|token| !token.is_empty())
    {
        enrollment.node_token = Some(token.to_owned());
    }
    save(data_dir, &enrollment)?;
    Ok(enrollment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enrollment_survives_restart_and_hello_result() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = load_or_create(dir.path()).expect("first");
        let second = load_or_create(dir.path()).expect("second");
        assert_eq!(first.host_id, second.host_id);
        assert!(first.node_token.is_none());

        let updated = apply_hello_result(
            dir.path(),
            &serde_json::json!({
                "hostId": first.host_id,
                "nodeToken": "host-secret"
            }),
        )
        .expect("hello result");
        assert_eq!(updated.node_token.as_deref(), Some("host-secret"));
        let loaded = load_or_create(dir.path()).expect("reload");
        assert_eq!(loaded.node_token.as_deref(), Some("host-secret"));
        assert_eq!(loaded.host_id, first.host_id);
    }
}
