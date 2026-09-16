//! D-027 / D-027b: pull staged attachments from the Hub and materialize them
//! on disk.
//!
//! An `instance.send` carries attachment metadata only; the bytes stay on the
//! Hub behind `GET /v1/objects/{id}` until this module fetches them. §8.1 of
//! the design confirmed the Node already holds both halves of the credential
//! needed for that fetch — the Hub base URL and the durable host token — so
//! the MVP needs no second byte channel.
//!
//! Materialization happens *before* dispatch and fails the whole send when it
//! cannot complete. Degrading silently to a text-only prompt would leave the
//! agent answering a question about a file it never received.
//!
//! D-027b (2026-09-15) widened this from four image types to arbitrary files:
//! files land under their sanitised *original* name (the Hub already
//! sanitised it; we defend in depth), collisions get a numeric suffix, the
//! digest is verified, and nothing is ever executed.

use crate::NodeError;
use remuda_protocol::InstanceId;
use remuda_protocol::hubnode::{
    AttachmentKind, AttachmentRef, extension_for_media_type, sanitize_attachment_name,
};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Per-attachment ceiling, mirroring the Hub's default upload cap so a
/// compromised or buggy Hub response cannot fill this disk. A Hub configured
/// with a smaller `attachmentMaxBytes` never sends more; a larger configured
/// cap must be matched here explicitly.
pub const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
/// Attachments accepted on one send, mirroring the Hub's per-message cap.
const MAX_ATTACHMENTS: usize = 8;
/// Pull deadline. A send waits on this, so it stays short.
const PULL_TIMEOUT: Duration = Duration::from_secs(60);

/// One attachment that now exists on this host's disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedAttachment {
    /// Hub object identity.
    pub object_id: String,
    /// Image vs. arbitrary file (D-027b).
    pub kind: AttachmentKind,
    /// Media type as accepted by the Hub.
    pub media_type: String,
    /// Sanitised display name (original filename when one was uploaded).
    pub name: String,
    /// Absolute path written under the instance's attachments directory.
    pub path: PathBuf,
    /// Byte length actually written.
    pub byte_len: u64,
    /// Lowercase hex SHA-256 of the bytes as written (verified against the
    /// manifest digest when the Hub carried one).
    pub digest: String,
    /// 1-based `[Image #n]` / `[File #n]` anchor number from the send
    /// manifest, when the client numbered its attachments.
    pub index: Option<u32>,
}

/// Where a Node fetches staged attachment bytes.
///
/// A trait so the runtime can be driven from tests and from a Node with no
/// Hub link at all (`remuda node --stdio`), where attachments are refused
/// rather than silently dropped.
pub trait ObjectSource: Send + Sync + std::fmt::Debug {
    /// Fetch one object's bytes by id.
    ///
    /// Sources that can only pull per-instance (the carrier path) leave this
    /// default, which fails: the Hub authorizes an `object.pull` for a named
    /// instance, so a fetch without one is a caller bug.
    fn fetch(
        &self,
        object_id: String,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<u8>, NodeError>> + Send + '_>> {
        Box::pin(async move {
            Err(NodeError::InvalidRequest(format!(
                "{object_id}: this object source requires the staging instance"
            )))
        })
    }

    /// Fetch an object staged for a specific instance.
    ///
    /// The carrier pull (`object.pull`) must name the instance: the Hub only
    /// serves objects staged for an instance on the requesting host. Sources
    /// authenticated per-object (the HTTP path) ignore it by default.
    fn fetch_for_instance(
        &self,
        object_id: String,
        _instance_id: &InstanceId,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<u8>, NodeError>> + Send + '_>> {
        Box::pin(async move { self.fetch(object_id).await })
    }
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
                .map_err(|error| map_http_error(&object_id, error))?;
            let status = response.status();
            if !status.is_success() {
                return Err(NodeError::InvalidRequest(format!(
                    "attachment {object_id}: Hub returned {status}"
                )));
            }
            let bytes = response
                .bytes()
                .await
                .map_err(|error| map_http_error(&object_id, error))?;
            Ok(bytes.to_vec())
        })
    }
}

/// A connect/timeout failure means the Hub HTTP origin is unreachable from
/// this host (the ssh-stdio case); the caller may fall back to a carrier pull.
/// Status-level failures stay [`NodeError::InvalidRequest`] — retrying over the
/// carrier cannot fix a rejected or missing object.
fn map_http_error(object_id: &str, error: reqwest::Error) -> NodeError {
    if error.is_connect() || error.is_timeout() {
        NodeError::Transport(format!("attachment {object_id}: Hub HTTP origin unreachable: {error}"))
    } else {
        NodeError::InvalidRequest(format!("attachment {object_id}: {error}"))
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
    // Names already claimed by this batch, so two attachments uploaded under
    // the same filename in one send get distinct landed paths.
    let mut claimed: BTreeSet<String> = BTreeSet::new();
    for reference in refs {
        let bytes = source
            .fetch_for_instance(reference.object_id.clone(), instance_id)
            .await?;
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
        let digest = format!("{:x}", Sha256::digest(&bytes));
        if let Some(expected) = reference
            .digest
            .as_deref()
            .filter(|value| !value.is_empty())
            && expected != digest
        {
            return Err(NodeError::InvalidRequest(format!(
                "attachment {} failed integrity check: digest {digest} != manifest {expected}",
                reference.object_id
            )));
        }
        let name = landing_name(reference)?;
        let file_name = collision_free_name(&dir, &mut claimed, &name, &reference.object_id)?;
        let path = dir.join(&file_name);
        write_private(&path, &bytes)?;
        claimed.insert(file_name);
        out.push(MaterializedAttachment {
            object_id: reference.object_id.clone(),
            kind: reference.kind,
            media_type: reference.media_type.clone(),
            name,
            byte_len: bytes.len() as u64,
            path,
            digest,
            index: reference.index,
        });
    }
    Ok(out)
}

/// Choose the on-disk basename for one attachment (D-027b).
///
/// Prefer the sanitised original filename the manifest carries; when none
/// survived, fall back to the D-027 derived `<obj_id>.<ext>` so the name is
/// still deterministic and path-safe.
fn landing_name(reference: &AttachmentRef) -> Result<String, NodeError> {
    if let Some(name) = reference.name.as_deref().and_then(sanitize_attachment_name) {
        return Ok(name);
    }
    let object_id = sanitize_id(&reference.object_id)?;
    Ok(format!(
        "{object_id}.{}",
        extension_for_media_type(&reference.media_type)
    ))
}

/// Return `name` when it is free both on disk and within this batch, else a
/// `stem-<n>.<ext>` variant. `object_id` is only used for the fallback when a
/// name carries no extension.
fn collision_free_name(
    dir: &Path,
    claimed: &mut BTreeSet<String>,
    name: &str,
    object_id: &str,
) -> Result<String, NodeError> {
    let free =
        |candidate: &str| -> bool { !claimed.contains(candidate) && !dir.join(candidate).exists() };
    if free(name) {
        return Ok(name.to_owned());
    }
    let (stem, ext) = split_name(name);
    let stem = if stem.is_empty() {
        sanitize_id(object_id)?
    } else {
        stem.to_owned()
    };
    for suffix in 1..u32::MAX {
        let candidate = match ext {
            Some(ext) => format!("{stem}-{suffix}.{ext}"),
            None => format!("{stem}-{suffix}"),
        };
        if free(&candidate) {
            return Ok(candidate);
        }
    }
    Err(NodeError::InvalidRequest(format!(
        "could not find a collision-free name for {name}"
    )))
}

/// Split a filename into its last extension (no dot), for the `-<n>` suffix.
fn split_name(name: &str) -> (&str, Option<&str>) {
    // A leading dot is not an extension (".env" stays the stem); use the last
    // dot so "archive.tar.gz" keeps stem "archive.tar" and ext "gz".
    let dot = name
        .char_indices()
        .skip(1)
        .filter(|(_, ch)| *ch == '.')
        .last()
        .map(|(index, _)| index);
    match dot {
        Some(index) => (&name[..index], Some(&name[index + 1..])),
        None => (name, None),
    }
}

/// Build the `image`/`file` + `resource` content blocks describing landed
/// attachments (D-027b).
///
/// Used both for the driver-facing prompt and for the journal user-message
/// record, so the absolute landed path survives in journal metadata even
/// though it never travels in a command frame. Each attachment contributes a
/// media block naming the Hub object followed by a `resource` block carrying
/// the absolute `file://` URI.
#[must_use]
pub fn content_blocks(
    attachments: &[MaterializedAttachment],
) -> Vec<remuda_protocol::ContentBlock> {
    use remuda_protocol::ContentBlock;
    let mut blocks = Vec::with_capacity(attachments.len() * 2);
    for attachment in attachments {
        let Ok(object_id) = remuda_protocol::Id::try_from(attachment.object_id.clone()) else {
            tracing::warn!(
                object_id = %attachment.object_id,
                "attachment id is not a protocol Id; journal/driver block skipped"
            );
            continue;
        };
        let media = Box::new(remuda_protocol::MediaBlock {
            object_id: object_id.clone(),
            media_type: attachment.media_type.clone(),
            name: Some(attachment.name.clone()),
            anchor: attachment.index,
            size: Some(attachment.byte_len),
        });
        blocks.push(match attachment.kind {
            AttachmentKind::Image => ContentBlock::Image(media),
            AttachmentKind::File => ContentBlock::File(media),
        });
        blocks.push(ContentBlock::Resource(Box::new(
            remuda_protocol::ResourceBlock {
                uri: format!("file://{}", attachment.path.display()),
                media_type: remuda_protocol::Knowledge::Known {
                    value: attachment.media_type.clone(),
                },
                object_id: Some(object_id),
            },
        )));
    }
    blocks
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

    /// A traversal-shaped id must never reach the filesystem, even though the
    /// Hub already mints ids itself.
    #[test]
    fn object_ids_that_are_not_bare_identifiers_are_refused() {
        assert!(sanitize_id("obj_01993ab0-0000-7000").is_ok());
        assert!(sanitize_id("../../etc/passwd").is_err());
        assert!(sanitize_id("obj/nested").is_err());
        assert!(sanitize_id("").is_err());
    }

    #[test]
    fn extension_split_keeps_leading_dots_in_the_stem() {
        assert_eq!(split_name("report.pdf"), ("report", Some("pdf")));
        assert_eq!(split_name("archive.tar.gz"), ("archive.tar", Some("gz")));
        assert_eq!(split_name("README"), ("README", None));
        assert_eq!(split_name(".env"), (".env", None));
    }

    /// The landing name is the sanitised original; an unusable one falls back
    /// to `<obj_id>.<ext>`.
    #[test]
    fn landing_names_prefer_the_original_and_fall_back_safely() {
        let reference = AttachmentRef {
            object_id: "obj_1".into(),
            kind: AttachmentKind::File,
            media_type: "application/pdf".into(),
            name: Some("Q3 report.pdf".into()),
            size: Some(10),
            digest: None,
            index: None,
        };
        assert_eq!(landing_name(&reference).unwrap(), "Q3 report.pdf");

        let hostile = AttachmentRef {
            name: Some("../escape.pdf".into()),
            ..reference.clone()
        };
        assert_eq!(landing_name(&hostile).unwrap(), "obj_1.pdf");

        let unnamed = AttachmentRef {
            name: None,
            ..reference.clone()
        };
        assert_eq!(landing_name(&unnamed).unwrap(), "obj_1.pdf");
    }

    /// Existing files — from an earlier send in the same instance directory —
    /// force a numeric suffix, and two same-named files in one batch diverge.
    #[test]
    fn collision_suffixes_keep_every_landing_distinct() {
        let dir = std::env::temp_dir().join(format!(
            "remuda-attach-collision-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        create_private_dir(&dir).unwrap();
        std::fs::write(dir.join("report.txt"), b"old").unwrap();
        let mut claimed = BTreeSet::new();

        let first = collision_free_name(&dir, &mut claimed, "report.txt", "obj_1").unwrap();
        assert_eq!(first, "report-1.txt");
        claimed.insert(first);

        let second = collision_free_name(&dir, &mut claimed, "report.txt", "obj_2").unwrap();
        assert_eq!(second, "report-2.txt");
        claimed.insert(second);

        // An existing base name AND its first suffixed variant are skipped.
        std::fs::write(dir.join("notes.md"), b"old").unwrap();
        std::fs::write(dir.join("notes-1.md"), b"old").unwrap();
        let notes = collision_free_name(&dir, &mut claimed, "notes.md", "obj_3").unwrap();
        assert_eq!(notes, "notes-2.md");

        // An extensionless name gets a bare suffix.
        let bare = collision_free_name(&dir, &mut claimed, "LICENSE", "obj_4").unwrap();
        assert_eq!(bare, "LICENSE");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[derive(Debug)]
    struct FakeSource {
        bytes: Vec<u8>,
    }

    impl ObjectSource for FakeSource {
        fn fetch(
            &self,
            _object_id: String,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<u8>, NodeError>> + Send + '_>>
        {
            let bytes = self.bytes.clone();
            Box::pin(async move { Ok(bytes) })
        }
    }

    #[tokio::test]
    async fn materialize_writes_named_files_and_rejects_digest_mismatch() {
        let dir = std::env::temp_dir().join(format!(
            "remuda-attach-mat-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let data_dir = dir.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let instance = InstanceId::new();
        let source: Arc<dyn ObjectSource> = Arc::new(FakeSource {
            bytes: b"hello file\n".to_vec(),
        });

        let digest = format!("{:x}", Sha256::digest(b"hello file\n"));
        let refs = vec![AttachmentRef {
            object_id: "obj_a".into(),
            kind: AttachmentKind::File,
            media_type: "text/plain".into(),
            name: Some("notes.txt".into()),
            size: Some(11),
            digest: Some(digest.clone()),
            index: Some(1),
        }];
        let landed = materialize(&source, &data_dir, &instance, &refs)
            .await
            .expect("materialize");
        assert_eq!(landed.len(), 1);
        assert_eq!(landed[0].name, "notes.txt");
        assert!(landed[0].path.is_absolute());
        assert!(landed[0].path.ends_with("notes.txt"));
        assert_eq!(landed[0].digest, digest);
        assert_eq!(std::fs::read(&landed[0].path).unwrap(), b"hello file\n");

        // A second materialization of the same name must not overwrite.
        let again = materialize(&source, &data_dir, &instance, &refs)
            .await
            .expect("materialize again");
        assert!(
            again[0].path.ends_with("notes-1.txt"),
            "{:?}",
            again[0].path
        );

        // A wrong digest fails the whole send.
        let bad = vec![AttachmentRef {
            object_id: "obj_b".into(),
            digest: Some("0".repeat(64)),
            ..refs[0].clone()
        }];
        assert!(
            materialize(&source, &data_dir, &instance, &bad)
                .await
                .is_err()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
