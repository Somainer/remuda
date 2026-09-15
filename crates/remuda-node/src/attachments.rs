//! D-027: pull staged attachments from the Hub and materialize them on disk.
//!
//! An `instance.send` carries attachment metadata only; the bytes stay on the
//! Hub behind `GET /v1/objects/{id}` until this module fetches them. §8.1 of
//! the design confirmed the Node already holds both halves of the credential
//! needed for that fetch — the Hub base URL and the durable host token — so
//! the MVP needs no second byte channel.
//!
//! Materialization happens *before* dispatch and fails the whole send when it
//! cannot complete. Degrading silently to a text-only prompt would leave the
//! agent answering a question about an image it never received.

use crate::NodeError;
use remuda_protocol::InstanceId;
use remuda_protocol::hubnode::AttachmentRef;
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Per-attachment ceiling, mirroring the Hub's upload cap so a compromised or
/// buggy Hub response cannot fill this disk.
const MAX_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;
/// Attachments accepted on one send, mirroring the Hub's per-message cap.
const MAX_ATTACHMENTS: usize = 4;
/// Pull deadline. A send waits on this, so it stays short.
const PULL_TIMEOUT: Duration = Duration::from_secs(30);

/// One attachment that now exists on this host's disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedAttachment {
    /// Hub object identity.
    pub object_id: String,
    /// Media type as sniffed by the Hub.
    pub media_type: String,
    /// Absolute path written under the instance's attachments directory.
    pub path: PathBuf,
    /// Byte length actually written.
    pub byte_len: u64,
    /// 1-based `[Image #n]` anchor number from the send manifest, when the
    /// client numbered its attachments.
    pub index: Option<u32>,
}

/// Where a Node fetches staged attachment bytes.
///
/// A trait so the runtime can be driven from tests and from a Node with no
/// Hub link at all (`remuda node --stdio`), where attachments are refused
/// rather than silently dropped.
pub trait ObjectSource: Send + Sync + std::fmt::Debug {
    /// Fetch one object's bytes by id.
    fn fetch(
        &self,
        object_id: String,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<u8>, NodeError>> + Send + '_>>;
}

/// `GET {hub}/v1/objects/{id}` with the durable host token.
#[derive(Debug, Clone)]
pub struct HubObjectSource {
    base: String,
    token: String,
    client: reqwest::Client,
}

impl HubObjectSource {
    /// Build a source from the Hub WebSocket URL the Node already dials.
    ///
    /// The Node is configured with `ws(s)://host/v1/node`, so the HTTP origin
    /// is derived from it rather than configured twice and allowed to drift.
    pub fn from_ws_url(ws_url: &str, token: impl Into<String>) -> Result<Self, NodeError> {
        Ok(Self {
            base: http_base(ws_url)?,
            token: token.into(),
            client: reqwest::Client::builder()
                .timeout(PULL_TIMEOUT)
                .build()
                .map_err(|error| NodeError::InvalidConfig(error.to_string()))?,
        })
    }
}

/// Map the Hub dial URL onto its HTTP origin: `wss://h/v1/node` -> `https://h`.
fn http_base(ws_url: &str) -> Result<String, NodeError> {
    let trimmed = ws_url.trim().trim_end_matches('/');
    let (scheme, rest) = if let Some(rest) = trimmed.strip_prefix("wss://") {
        ("https", rest)
    } else if let Some(rest) = trimmed.strip_prefix("ws://") {
        ("http", rest)
    } else if let Some(rest) = trimmed.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        ("http", rest)
    } else {
        return Err(NodeError::InvalidConfig(format!(
            "cannot derive a Hub HTTP origin from {ws_url}"
        )));
    };
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty() {
        return Err(NodeError::InvalidConfig(format!(
            "cannot derive a Hub HTTP origin from {ws_url}"
        )));
    }
    Ok(format!("{scheme}://{authority}"))
}

impl ObjectSource for HubObjectSource {
    fn fetch(
        &self,
        object_id: String,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<u8>, NodeError>> + Send + '_>> {
        Box::pin(async move {
            let url = format!("{}/v1/objects/{object_id}", self.base);
            let response = self
                .client
                .get(&url)
                .bearer_auth(&self.token)
                .send()
                .await
                .map_err(|error| {
                    NodeError::InvalidRequest(format!("attachment {object_id}: {error}"))
                })?;
            let status = response.status();
            if !status.is_success() {
                return Err(NodeError::InvalidRequest(format!(
                    "attachment {object_id}: Hub returned {status}"
                )));
            }
            let bytes = response.bytes().await.map_err(|error| {
                NodeError::InvalidRequest(format!("attachment {object_id}: {error}"))
            })?;
            Ok(bytes.to_vec())
        })
    }
}

/// Directory holding one instance's materialized attachments.
///
/// Deliberately under the Node data dir and not inside the workspace: an
/// attachment is transient input, not repository content.
pub fn attachments_dir(data_dir: &Path, instance_id: &InstanceId) -> PathBuf {
    data_dir
        .join("instances")
        .join(instance_id.as_id().as_str())
        .join("attachments")
}

/// Remove an instance's attachments directory. Missing is success.
pub fn cleanup(data_dir: &Path, instance_id: &InstanceId) -> std::io::Result<()> {
    let dir = attachments_dir(data_dir, instance_id);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Sweep attachment directories for instances that no longer exist locally.
///
/// Covers the case where a Node was killed between a send and the instance's
/// terminal event, which the in-band cleanup cannot reach.
pub fn sweep_orphans(data_dir: &Path, live: &BTreeSet<String>) -> std::io::Result<usize> {
    let instances = data_dir.join("instances");
    let Ok(entries) = std::fs::read_dir(&instances) else {
        return Ok(0);
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if live.contains(&name) {
            continue;
        }
        let dir = entry.path().join("attachments");
        if dir.is_dir() {
            std::fs::remove_dir_all(&dir)?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Pull every referenced attachment and write it under the instance directory.
///
/// Returns in the same order as `refs` so a driver can pair paths with the
/// prompt deterministically. Any failure aborts the whole set: a partial
/// materialization would send the agent an incomplete picture.
pub async fn materialize(
    source: &Arc<dyn ObjectSource>,
    data_dir: &Path,
    instance_id: &InstanceId,
    refs: &[AttachmentRef],
) -> Result<Vec<MaterializedAttachment>, NodeError> {
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    if refs.len() > MAX_ATTACHMENTS {
        return Err(NodeError::InvalidRequest(format!(
            "{} attachments exceeds the {MAX_ATTACHMENTS} per-message limit",
            refs.len()
        )));
    }
    let dir = attachments_dir(data_dir, instance_id);
    create_private_dir(&dir)?;

    let mut out = Vec::with_capacity(refs.len());
    for reference in refs {
        let extension = extension_for(&reference.media_type).ok_or_else(|| {
            NodeError::InvalidRequest(format!(
                "attachment {} has unsupported media type {}",
                reference.object_id, reference.media_type
            ))
        })?;
        // The name is rebuilt from the id and the media type. Nothing the Hub
        // or the caller sent is used as a path component.
        let file_name = format!("{}.{extension}", sanitize_id(&reference.object_id)?);
        let path = dir.join(&file_name);
        let bytes = source.fetch(reference.object_id.clone()).await?;
        if bytes.is_empty() {
            return Err(NodeError::InvalidRequest(format!(
                "attachment {} came back empty",
                reference.object_id
            )));
        }
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err(NodeError::InvalidRequest(format!(
                "attachment {} is {} bytes; the limit is {MAX_ATTACHMENT_BYTES}",
                reference.object_id,
                bytes.len()
            )));
        }
        write_private(&path, &bytes)?;
        out.push(MaterializedAttachment {
            object_id: reference.object_id.clone(),
            media_type: reference.media_type.clone(),
            byte_len: bytes.len() as u64,
            path,
            index: reference.index,
        });
    }
    Ok(out)
}

/// Object ids are opaque to us, so verify the shape before it becomes a path.
fn sanitize_id(object_id: &str) -> Result<String, NodeError> {
    let ok = !object_id.is_empty()
        && object_id.len() <= 128
        && object_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(object_id.to_owned())
    } else {
        Err(NodeError::InvalidRequest(format!(
            "attachment id {object_id} is not a bare identifier"
        )))
    }
}

/// Extension for an allowlisted media type; `None` rejects everything else.
pub fn extension_for(media_type: &str) -> Option<&'static str> {
    match media_type.split(';').next().unwrap_or_default().trim() {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

fn create_private_dir(dir: &Path) -> Result<(), NodeError> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write `0600` through a temp file and rename, matching how the Node already
/// writes launch material.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), NodeError> {
    let parent = path.parent().ok_or_else(|| {
        NodeError::InvalidRequest("attachment path has no parent directory".into())
    })?;
    let temporary = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<(), NodeError> {
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_urls_map_onto_their_http_origin() {
        assert_eq!(
            http_base("wss://hub.example/v1/node").unwrap(),
            "https://hub.example"
        );
        assert_eq!(
            http_base("ws://127.0.0.1:8080/v1/node").unwrap(),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            http_base("https://hub.example/").unwrap(),
            "https://hub.example"
        );
        assert!(http_base("hub.example").is_err());
        assert!(http_base("wss://").is_err());
    }

    #[test]
    fn only_allowlisted_image_types_get_an_extension() {
        assert_eq!(extension_for("image/png"), Some("png"));
        assert_eq!(extension_for("image/jpeg"), Some("jpg"));
        assert_eq!(extension_for("image/webp; charset=binary"), Some("webp"));
        assert_eq!(extension_for("application/pdf"), None);
        assert_eq!(extension_for("image/svg+xml"), None);
    }

    /// A traversal-shaped id must never reach the filesystem, even though the
    /// Hub already mints ids itself.
    #[test]
    fn object_ids_that_are_not_bare_identifiers_are_refused() {
        assert!(sanitize_id("obj_01993ab0-0000-7000").is_ok());
        assert!(sanitize_id("../../etc/passwd").is_err());
        assert!(sanitize_id("obj/nested").is_err());
        assert!(sanitize_id("").is_err());
    }
}
