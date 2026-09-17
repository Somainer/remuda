//! Read-only host file listing and fetch.
//!
//! The upward counterpart of D-027's downward attachment channel. A Hub
//! operator lists directories and reads files on a workstation; every path
//! is confined to a registered workspace root or a `<tmp>/remuda-*` scratch
//! directory. Containment is decided on the *canonical* path, so a symlink
//! pointing outside an allowed root is refused exactly like `..` traversal.
//!
//! Hard rules: never execute anything, never write, regular files only (no
//! symlinks/devices/fifos/sockets), and a single file may not exceed the
//! attachment byte ceiling. `host.files.read` stages the bytes through the
//! existing Hub objects channel with the host token and returns an
//! `obj_…` id; bytes themselves keep flowing through `GET /v1/objects/{id}`.

use crate::NodeError;
use crate::attachments::MAX_ATTACHMENT_BYTES;
use remuda_protocol::Workspace;
use remuda_protocol::hubnode::{
    HOST_FILES_SCRATCH_ID, HostFileEntry, HostFileKind, HostFileReadResult, HostFilesListResult,
    HostFilesParams,
};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

/// `host.files.list` / `host.files.read` are handled here.
pub(crate) fn is_host_files_method(method: &str) -> bool {
    matches!(method, "host.files.list" | "host.files.read")
}

/// One staged host file: the object id the Hub minted plus verified metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StagedHostFile {
    pub(crate) object_id: String,
    pub(crate) digest: String,
    pub(crate) size: u64,
}

/// Uploads read bytes to the Hub's objects channel.
///
/// The outbound-WSS runtime installs an HTTP client holding the Hub origin
/// and host token (D-027a). A stdio-enrolled Node with no HTTP route has no
/// stager, and `host.files.read` fails cleanly there.
pub(crate) trait HostFileStager: Send + Sync + std::fmt::Debug {
    fn stage<'a>(
        &'a self,
        name: String,
        bytes: Vec<u8>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<StagedHostFile, NodeError>> + Send + 'a>>;
}

/// `POST {hub}/v1/hosts/{hostId}/files/objects` with the durable host token.
/// Shares D-027a's Hub-origin derivation and credential: no second secret or
/// configured URL is introduced.
#[derive(Debug, Clone)]
pub(crate) struct HubHostFileStager {
    base: String,
    host_id: String,
    token: String,
    client: reqwest::Client,
}

impl HubHostFileStager {
    /// Build the stager from the Hub dial URL, local host id and host token.
    pub(crate) fn from_ws_url(
        ws_url: &str,
        host_id: String,
        token: String,
    ) -> Result<Self, NodeError> {
        Ok(Self {
            base: crate::attachments::http_base(ws_url)?,
            host_id,
            token,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(90))
                .build()
                .map_err(|error| NodeError::InvalidConfig(error.to_string()))?,
        })
    }
}

impl HostFileStager for HubHostFileStager {
    fn stage<'a>(
        &'a self,
        name: String,
        bytes: Vec<u8>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<StagedHostFile, NodeError>> + Send + 'a>>
    {
        Box::pin(async move {
            let url = format!("{}/v1/hosts/{}/files/objects", self.base, self.host_id);
            let response = self
                .client
                .post(url)
                .bearer_auth(&self.token)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .query(&[("name", name.as_str())])
                .body(bytes)
                .send()
                .await
                .map_err(|error| {
                    if error.is_connect() || error.is_timeout() {
                        NodeError::Transport(format!("Hub HTTP origin unreachable: {error}"))
                    } else {
                        NodeError::InvalidRequest(format!("host file upload failed: {error}"))
                    }
                })?;
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if !status.is_success() {
                return Err(NodeError::InvalidRequest(format!(
                    "Hub refused the host file upload with {status}: {body}"
                )));
            }

            let value: Value = serde_json::from_str(&body).map_err(|error| {
                NodeError::Transport(format!("Hub host file upload replied badly: {error}"))
            })?;
            Ok(StagedHostFile {
                object_id: value
                    .get("objectId")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        NodeError::Transport("Hub upload reply missing objectId".into())
                    })?
                    .to_owned(),
                digest: value
                    .get("digest")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned(),
                size: value.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
            })
        })
    }
}

/// What a request is anchored to: a registered workspace, or the scratch area.
enum Anchor {
    /// Canonical registered workspace root.
    Workspace(PathBuf),
    /// Canonical system temp directory; the first path component below it
    /// must start with `remuda-`.
    Scratch(PathBuf),
}

impl Anchor {
    fn base(&self) -> &Path {
        match self {
            Anchor::Workspace(path) | Anchor::Scratch(path) => path,
        }
    }
}

/// Resolve the workspace selector to a canonical anchor directory.
fn resolve_anchor(workspace_id: &str, workspaces: &[Workspace]) -> Result<Anchor, NodeError> {
    if workspace_id == HOST_FILES_SCRATCH_ID {
        let tmp = std::env::temp_dir();
        let canonical = fs_canonicalize(&tmp)?;
        return Ok(Anchor::Scratch(canonical));
    }
    let workspace = workspaces
        .iter()
        .find(|workspace| workspace.meta.id.as_id().as_str() == workspace_id)
        .ok_or_else(|| {
            NodeError::InvalidRequest(format!(
                "workspace {workspace_id} is not registered on this Node"
            ))
        })?;
    let root = fs_canonicalize(Path::new(&workspace.root_path)).map_err(|error| {
        NodeError::InvalidRequest(format!(
            "workspace {} is not accessible: {error}",
            workspace.root_path
        ))
    })?;
    Ok(Anchor::Workspace(root))
}

fn fs_canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

/// Join the relative selector and canonicalize, refusing every escape.
///
/// Checks run in order: relative-only components (a `..` or absolute prefix
/// is rejected before any filesystem call), canonical resolution, and
/// containment under the anchor. The scratch anchor additionally requires
/// the first component below the temp directory to start with `remuda-`.
fn resolve_contained(anchor: &Anchor, rel_path: &str) -> Result<PathBuf, NodeError> {
    let rel_path = rel_path.trim();
    let mut joined = anchor.base().to_path_buf();
    if !rel_path.is_empty() {
        let rel = Path::new(rel_path);
        if rel.is_absolute() {
            return Err(NodeError::InvalidRequest(
                "host file path must be relative to the workspace root".into(),
            ));
        }
        for component in rel.components() {
            match component {
                Component::Normal(segment) => joined.push(segment),
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(NodeError::InvalidRequest(format!(
                        "host file path {rel_path} escapes the workspace: '..' is not allowed"
                    )));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(NodeError::InvalidRequest(
                        "host file path must be relative to the workspace root".into(),
                    ));
                }
            }
        }
    }
    let canonical = std::fs::canonicalize(&joined).map_err(|error| {
        NodeError::InvalidRequest(format!(
            "host file path {rel_path} cannot be resolved: {error}"
        ))
    })?;
    if !canonical.starts_with(anchor.base()) {
        return Err(NodeError::InvalidRequest(format!(
            "host file path {rel_path} escapes the workspace root"
        )));
    }
    if let Anchor::Scratch(_) = anchor {
        let allowed = canonical
            .strip_prefix(anchor.base())
            .ok()
            .and_then(|rest| {
                rest.components().find_map(|component| match component {
                    Component::Normal(name) => Some(name),
                    _ => None,
                })
            })
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("remuda-"));
        if !allowed {
            return Err(NodeError::InvalidRequest(format!(
                "host file path {rel_path} is outside the remuda-* scratch area"
            )));
        }
    }
    Ok(canonical)
}

/// List one contained directory without following entry symlinks.
fn list_entries(workspace_id: &str, target: &Path) -> Result<HostFilesListResult, NodeError> {
    let metadata = std::fs::symlink_metadata(target).map_err(|error| {
        NodeError::InvalidRequest(format!("{} cannot be inspected: {error}", target.display()))
    })?;
    if !metadata.is_dir() {
        return Err(NodeError::InvalidRequest(format!(
            "{} is not a directory",
            target.display()
        )));
    }
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(target).map_err(|error| {
        NodeError::InvalidRequest(format!("{} cannot be listed: {error}", target.display()))
    })? {
        let entry = entry.map_err(|error| {
            NodeError::InvalidRequest(format!("directory entry unreadable: {error}"))
        })?;
        // `symlink_metadata` so a symlink's target is never touched: the link
        // is reported as `symlink`, never as whatever it points at.
        let meta = match std::fs::symlink_metadata(entry.path()) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        let file_type = entry.file_type().unwrap_or_else(|_| meta.file_type());
        let kind = if file_type.is_symlink() {
            HostFileKind::Symlink
        } else if file_type.is_dir() {
            HostFileKind::Dir
        } else if file_type.is_file() {
            HostFileKind::File
        } else {
            HostFileKind::Other
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        entries.push(HostFileEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            kind,
            size: meta.len(),
            mtime,
            mode: unix_mode(&meta),
        });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(HostFilesListResult {
        workspace_id: workspace_id.to_owned(),
        path: target.display().to_string(),
        entries,
    })
}

#[cfg(unix)]
fn unix_mode(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    metadata.mode() & 0o7777
}

#[cfg(not(unix))]
fn unix_mode(_metadata: &std::fs::Metadata) -> u32 {
    0
}

/// Containment and file-type checks for one read, returning the bytes and
/// their digest. Runs entirely off the async runtime thread.
fn read_regular_file(target: &Path) -> Result<(Vec<u8>, String), NodeError> {
    let metadata = std::fs::symlink_metadata(target).map_err(|error| {
        NodeError::InvalidRequest(format!("{} cannot be inspected: {error}", target.display()))
    })?;
    let file_type = metadata.file_type();
    if !file_type.is_file() {
        // `symlink_metadata` classifies a symlink as a symlink, never as its
        // target, so links, fifos, sockets and devices all land here.
        return Err(NodeError::InvalidRequest(format!(
            "{} is not a regular file",
            target.display()
        )));
    }
    if metadata.len() > MAX_ATTACHMENT_BYTES as u64 {
        return Err(NodeError::InvalidRequest(format!(
            "RESOURCE_LIMIT: {} is {} bytes; the limit is {MAX_ATTACHMENT_BYTES}",
            target.display(),
            metadata.len()
        )));
    }
    let bytes = std::fs::read(target).map_err(|error| {
        NodeError::InvalidRequest(format!("{} cannot be read: {error}", target.display()))
    })?;
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        return Err(NodeError::InvalidRequest(format!(
            "RESOURCE_LIMIT: {} is {} bytes; the limit is {MAX_ATTACHMENT_BYTES}",
            target.display(),
            bytes.len()
        )));
    }
    let digest = format!("{:x}", Sha256::digest(&bytes));
    Ok((bytes, digest))
}

impl crate::DevNode {
    /// Dispatch a `host.files.*` RPC onto the local filesystem.
    pub(crate) async fn host_files_rpc(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, NodeError> {
        let parsed: HostFilesParams = serde_json::from_value(params)?;
        let workspace_id = parsed.workspace_id.trim().to_owned();
        if workspace_id.is_empty() {
            return Err(NodeError::InvalidRequest(
                "host file request requires a workspaceId".into(),
            ));
        }
        let rel_path = parsed.rel_path.unwrap_or_default();
        let workspaces = self.workspaces()?;
        let result = match method {
            "host.files.list" => {
                let listing = tokio::task::spawn_blocking(move || {
                    let anchor = resolve_anchor(&workspace_id, &workspaces)?;
                    let target = resolve_contained(&anchor, &rel_path)?;
                    list_entries(&workspace_id, &target)
                })
                .await
                .map_err(|error| NodeError::Driver(format!("host files task failed: {error}")))??;
                serde_json::to_value(listing)?
            }
            "host.files.read" => {
                // Containment and regular-file checks run first and off the
                // async thread; the Hub link is only needed for bytes that
                // passed them.
                let read = tokio::task::spawn_blocking(move || {
                    let anchor = resolve_anchor(&workspace_id, &workspaces)?;
                    let target = resolve_contained(&anchor, &rel_path)?;
                    let (bytes, digest) = read_regular_file(&target)?;
                    let name = target
                        .file_name()
                        .and_then(|value| value.to_str())
                        .map(str::to_owned);
                    Ok::<_, NodeError>((name, bytes, digest))
                })
                .await
                .map_err(|error| NodeError::Driver(format!("host files task failed: {error}")))??;
                let stager = self
                    .inner
                    .host_file_stager
                    .read()
                    .map_err(|_| NodeError::StorePoisoned)?
                    .clone()
                    .ok_or_else(|| {
                        NodeError::InvalidRequest(
                            "host file fetch needs an outbound Hub HTTP link".into(),
                        )
                    })?;
                let (name, bytes, digest) = read;
                let size = bytes.len() as u64;
                let name = name.unwrap_or_else(|| "host-file".to_owned());
                let staged = stager.stage(name, bytes).await?;
                if staged.digest != digest || staged.size != size {
                    return Err(NodeError::Transport(format!(
                        "Hub staged object {} failed integrity verification",
                        staged.object_id
                    )));
                }
                serde_json::to_value(HostFileReadResult {
                    object_id: staged.object_id,
                    digest,
                    size,
                })?
            }
            other => {
                return Err(NodeError::InvalidRequest(format!(
                    "unknown host files method {other}"
                )));
            }
        };
        Ok(result)
    }

    /// Install the uploader `host.files.read` stages bytes through.
    pub(crate) fn set_host_file_stager(&self, stager: Option<Arc<dyn HostFileStager>>) {
        if let Ok(mut slot) = self.inner.host_file_stager.write() {
            *slot = stager;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::sync::Mutex;

    fn workspace_node(root: &Path) -> (crate::DevNode, Workspace) {
        let data = tempfile::tempdir().unwrap();
        let config = crate::DevServerConfig::loopback(0)
            .with_workspace_root(root.to_path_buf())
            .with_workspace_roots(vec![root.to_path_buf()])
            .with_workspace_registry(data.path().to_path_buf());
        let node = crate::DevNode::new(&config).unwrap();
        let workspace = node.workspaces().unwrap()[0].clone();
        (node, workspace)
    }

    fn params(workspace_id: &str, rel_path: Option<&str>) -> Value {
        let mut value = json!({"workspaceId": workspace_id});
        if let Some(rel_path) = rel_path {
            value["relPath"] = json!(rel_path);
        }
        value
    }

    #[derive(Debug, Default)]
    struct FakeStager {
        staged: Mutex<Vec<(String, Vec<u8>)>>,
    }

    impl HostFileStager for FakeStager {
        fn stage<'a>(
            &'a self,
            name: String,
            bytes: Vec<u8>,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<StagedHostFile, NodeError>> + Send + 'a>>
        {
            Box::pin(async move {
                let digest = format!("{:x}", Sha256::digest(&bytes));
                self.staged.lock().unwrap().push((name, bytes.clone()));
                Ok(StagedHostFile {
                    object_id: "obj_fake".into(),
                    digest,
                    size: bytes.len() as u64,
                })
            })
        }
    }

    #[tokio::test]
    async fn lists_and_reads_inside_a_registered_workspace() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("notes.txt"), b"hello host files\n").unwrap();
        fs::create_dir(root.path().join("sub")).unwrap();
        fs::write(root.path().join("sub").join("deep.txt"), b"deep").unwrap();
        let (node, workspace) = workspace_node(root.path());
        node.set_host_file_stager(Some(Arc::new(FakeStager::default())));

        let listing = node
            .host_files_rpc(
                "host.files.list",
                params(&workspace.meta.id.as_id().to_string(), None),
            )
            .await
            .unwrap();
        let names: Vec<String> = listing["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, vec!["notes.txt", "sub"]);
        assert_eq!(
            listing["path"],
            json!(fs::canonicalize(root.path()).unwrap())
        );
        let sub = node
            .host_files_rpc(
                "host.files.list",
                params(&workspace.meta.id.as_id().to_string(), Some("sub")),
            )
            .await
            .unwrap();
        assert_eq!(sub["entries"][0]["name"], json!("deep.txt"));

        let read = node
            .host_files_rpc(
                "host.files.read",
                params(&workspace.meta.id.as_id().to_string(), Some("notes.txt")),
            )
            .await
            .unwrap();
        assert_eq!(read["objectId"], json!("obj_fake"));
        assert_eq!(read["size"], json!(17));
        assert_eq!(
            read["digest"],
            json!(format!("{:x}", Sha256::digest(b"hello host files\n")))
        );
    }

    #[tokio::test]
    async fn parent_traversal_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), b"x").unwrap();
        let (node, workspace) = workspace_node(root.path());
        let error = node
            .host_files_rpc(
                "host.files.read",
                params(
                    &workspace.meta.id.as_id().to_string(),
                    Some(&format!(
                        "../{}/secret",
                        outside.path().file_name().unwrap().to_string_lossy()
                    )),
                ),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("escapes"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_pointing_outside_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), root.path().join("evil-link"))
            .unwrap();
        let (node, workspace) = workspace_node(root.path());
        let error = node
            .host_files_rpc(
                "host.files.read",
                params(&workspace.meta.id.as_id().to_string(), Some("evil-link")),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("escapes the workspace root"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn irregular_files_are_rejected() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let fifo = root.path().join("a-fifo");
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        let outside = tempfile::tempdir().unwrap();
        // Symlinks are irregular even when their target is inside.
        symlink(root.path().join("a-fifo"), root.path().join("link-to-fifo")).unwrap();
        let _ = outside;
        let (node, workspace) = workspace_node(root.path());
        for path in ["a-fifo", "link-to-fifo"] {
            let error = node
                .host_files_rpc(
                    "host.files.read",
                    params(&workspace.meta.id.as_id().to_string(), Some(path)),
                )
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("not a regular file"), "{path}: {error}");
        }
        // Listing shows them with their real kinds rather than following.
        let listing = node
            .host_files_rpc(
                "host.files.list",
                params(&workspace.meta.id.as_id().to_string(), None),
            )
            .await
            .unwrap();
        let kinds: std::collections::HashMap<String, String> = listing["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["name"].as_str().unwrap().to_owned(),
                    entry["kind"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(kinds["a-fifo"], "other");
        assert_eq!(kinds["link-to-fifo"], "symlink");
    }

    #[tokio::test]
    async fn oversized_files_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let big = vec![0u8; MAX_ATTACHMENT_BYTES + 1];
        fs::write(root.path().join("big.bin"), big).unwrap();
        let (node, workspace) = workspace_node(root.path());
        let error = node
            .host_files_rpc(
                "host.files.read",
                params(&workspace.meta.id.as_id().to_string(), Some("big.bin")),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("RESOURCE_LIMIT"), "{error}");
    }

    #[test]
    fn scratch_area_accepts_remuda_prefixed_tmp_and_rejects_other_tmp() {
        let tmp = std::env::temp_dir();
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let scratch = tmp.join(format!("remuda-hostfiles-test-{suffix}"));
        let other = tmp.join(format!("remuda-other-place-{suffix}"));
        let unrelated = tmp.join(format!("not-remuda-{suffix}"));
        fs::create_dir_all(scratch.join("nested")).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("x"), b"x").unwrap();
        fs::write(&unrelated, b"x").unwrap();

        let anchor = Anchor::Scratch(fs_canonicalize(&tmp).unwrap());
        // A file under /tmp/remuda-* is contained.
        let nested = scratch.join("nested");
        assert!(resolve_contained(&anchor, &path_rel(&tmp, &nested)).is_ok());
        // Another remuda-* directory is a scratch area too (prefix match).
        assert!(resolve_contained(&anchor, &path_rel(&tmp, &other.join("x"))).is_ok());
        // Plain /tmp path without a remuda-* first component is refused.
        let error = resolve_contained(&anchor, &path_rel(&tmp, &unrelated)).unwrap_err();
        assert!(error.to_string().contains("scratch area"), "{}", error);
        // The temp directory itself is not readable as a target.
        assert!(resolve_contained(&anchor, "").is_err());
        // Traversal is rejected before it even reaches the prefix rule.
        assert!(resolve_contained(&anchor, "../etc/passwd").is_err());

        fs::remove_dir_all(&scratch).ok();
        fs::remove_dir_all(&other).ok();
        fs::remove_file(&unrelated).ok();
    }

    fn path_rel(base: &Path, path: &Path) -> String {
        path.strip_prefix(base).unwrap().display().to_string()
    }

    #[tokio::test]
    async fn unknown_workspace_and_method_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let (node, _) = workspace_node(root.path());
        let error = node
            .host_files_rpc("host.files.list", json!({"workspaceId": "ws_nope"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("not registered"), "{error}");
        let error = node
            .host_files_rpc("host.files.bogus", json!({"workspaceId": "ws_nope"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown host files method"), "{error}");
    }
}
