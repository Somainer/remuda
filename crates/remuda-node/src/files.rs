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
    HOST_FILES_SCRATCH_ID, HostFileEntry, HostFileKind, HostFileReadResult, HostFileSearchMode,
    HostFilesListResult, HostFilesParams, HostFilesSearchParams,
};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

/// `host.files.list` / `host.files.read` / `host.files.search` are handled here.
pub(crate) fn is_host_files_method(method: &str) -> bool {
    matches!(
        method,
        "host.files.list" | "host.files.read" | "host.files.search"
    )
}

/// Default cap on returned search matches.
pub(crate) const SEARCH_DEFAULT_MAX_RESULTS: usize = 200;
/// Wall-clock cap for one search; the walk stops at the next entry boundary.
pub(crate) const SEARCH_WALL_CLOCK: std::time::Duration = std::time::Duration::from_secs(10);
/// Total bytes read across all files during one content search.
pub(crate) const SEARCH_TOTAL_BYTES: u64 = 10 * 1024 * 1024;
/// Per-file byte cap: larger files are skipped (and mark the reply truncated).
pub(crate) const SEARCH_PER_FILE_BYTES: u64 = 1024 * 1024;
/// Maximum snippet length in characters.
pub(crate) const SEARCH_SNIPPET_CHARS: usize = 240;

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

/// One directory the search walks.
#[derive(Debug)]
struct SearchRoot {
    /// Canonical directory on disk; always below the anchor.
    dir: PathBuf,
}

/// Resolve the search roots for one workspace selector.
///
/// Workspace anchors resolve to exactly one root (the contained subtree). The
/// scratch anchor with an empty selector fans out to every `<tmp>/remuda-*`
/// directory; `<tmp>` itself is never walked.
fn search_roots(anchor: &Anchor, rel_path: &str) -> Result<Vec<SearchRoot>, NodeError> {
    let rel_path = rel_path.trim();
    if !rel_path.is_empty() {
        let target = resolve_contained(anchor, rel_path)?;
        if !target.is_dir() {
            return Err(NodeError::InvalidRequest(format!(
                "{} is not a directory",
                target.display()
            )));
        }
        return Ok(vec![SearchRoot { dir: target }]);
    }
    match anchor {
        Anchor::Workspace(_) => Ok(vec![SearchRoot {
            dir: anchor.base().to_path_buf(),
        }]),
        Anchor::Scratch(tmp) => {
            let mut roots = Vec::new();
            for entry in std::fs::read_dir(tmp).map_err(|error| {
                NodeError::InvalidRequest(format!("{} cannot be listed: {error}", tmp.display()))
            })? {
                let Ok(entry) = entry else { continue };
                let file_name = entry.file_name();
                let Some(name) = file_name.to_str() else {
                    continue;
                };
                if !name.starts_with("remuda-") {
                    continue;
                }
                // Re-resolve through containment so a symlinked remuda-* entry
                // can never point the walk outside the scratch area.
                if let Ok(target) = resolve_contained(anchor, name)
                    && target.is_dir()
                {
                    roots.push(SearchRoot { dir: target });
                }
            }
            Ok(roots)
        }
    }
}

/// Literal substring or compiled regex needle.
enum Needle {
    Literal(String),
    Regex(regex::Regex),
}

impl Needle {
    fn compile(query: &str, use_regex: bool) -> Result<Self, NodeError> {
        if use_regex {
            regex::Regex::new(query).map(Self::Regex).map_err(|error| {
                NodeError::InvalidRequest(format!("invalid search regex: {error}"))
            })
        } else {
            Ok(Self::Literal(query.to_owned()))
        }
    }

    /// First match in one line as `(byte column, length)`; length is 0 for a
    /// literal hit.
    fn find(&self, line: &str) -> Option<(usize, usize)> {
        match self {
            Needle::Literal(needle) => line
                .find(needle.as_str())
                .map(|start| (start, needle.len())),
            Needle::Regex(pattern) => pattern.find(line).map(|hit| (hit.start(), hit.len())),
        }
    }

    fn is_match(&self, text: &str) -> bool {
        match self {
            Needle::Literal(needle) => text.contains(needle.as_str()),
            Needle::Regex(pattern) => pattern.is_match(text),
        }
    }
}

/// `--glob` narrowing, compiled into a basename matcher and/or path matchers.
struct GlobNarrow {
    basename: Option<glob::Pattern>,
    anywhere: Vec<glob::Pattern>,
}

impl GlobNarrow {
    fn compile(raw: &str) -> Result<Self, NodeError> {
        let pattern = raw.trim().trim_start_matches('/');
        if pattern.is_empty() {
            return Err(NodeError::InvalidRequest("glob pattern is empty".into()));
        }
        let compile = |value: &str| -> Result<glob::Pattern, NodeError> {
            glob::Pattern::new(value)
                .map_err(|error| NodeError::InvalidRequest(format!("invalid glob {raw}: {error}")))
        };
        if pattern.contains('/') {
            // Like ripgrep: `src/x.rs` is rooted at the walk root, but a
            // `**/`-prefixed copy matches the same relative path at any depth.
            let mut anywhere = vec![compile(pattern)?];
            if !pattern.starts_with("**/") {
                anywhere.push(compile(&format!("**/{pattern}"))?);
            }
            Ok(GlobNarrow {
                basename: None,
                anywhere,
            })
        } else {
            Ok(GlobNarrow {
                basename: Some(compile(pattern)?),
                anywhere: Vec::new(),
            })
        }
    }

    fn matches(&self, rel_path: &str, basename: &str) -> bool {
        if let Some(pattern) = &self.basename {
            return pattern.matches(basename);
        }
        self.anywhere
            .iter()
            .any(|pattern| pattern.matches(rel_path))
    }
}

/// Mutable walk state shared across all scratch roots.
struct SearchWalk<'a> {
    anchor: &'a Anchor,
    mode: HostFileSearchMode,
    needle: Needle,
    glob: Option<GlobNarrow>,
    max_results: usize,
    per_file_bytes: u64,
    total_bytes: u64,
    deadline: std::time::Instant,
    matches: Vec<remuda_protocol::hubnode::HostFileSearchMatch>,
    files_scanned: u64,
    bytes_scanned: u64,
    truncated: bool,
    truncated_reason: Option<String>,
}

impl<'a> SearchWalk<'a> {
    fn stop(&mut self, reason: &'static str) {
        self.truncated = true;
        if self.truncated_reason.is_none() {
            self.truncated_reason = Some(reason.to_owned());
        }
    }

    /// Re-verify containment of one walked entry on its canonical path. The
    /// walker never follows symlinks and the root is already canonical, but the
    /// check is repeated per entry so a link or a mount cannot point a read out
    /// of the workspace.
    fn contained(&self, path: &Path) -> Option<PathBuf> {
        let canonical = std::fs::canonicalize(path).ok()?;
        if !canonical.starts_with(self.anchor.base()) {
            return None;
        }
        if let Anchor::Scratch(_) = self.anchor {
            let allowed = canonical
                .strip_prefix(self.anchor.base())
                .ok()?
                .components()
                .find_map(|component| match component {
                    Component::Normal(name) => Some(name),
                    _ => None,
                })
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("remuda-"));
            if !allowed {
                return None;
            }
        }
        Some(canonical)
    }

    fn push_match(
        &mut self,
        path: String,
        line: u64,
        column: Option<u64>,
        snippet: String,
    ) -> bool {
        self.matches
            .push(remuda_protocol::hubnode::HostFileSearchMatch {
                path,
                line,
                column,
                snippet: Some(snippet),
            });
        if self.matches.len() >= self.max_results {
            self.stop("max-results");
            return false;
        }
        true
    }

    fn walk_root(&mut self, root: &SearchRoot) -> Result<(), NodeError> {
        let walker = ignore::WalkBuilder::new(&root.dir)
            .follow_links(false)
            // Honour .gitignore and .ignore files inside the walked tree and in
            // ancestor directories (ripgrep's default: searching a subtree still
            // applies the repository root's rules); skip hidden entries too.
            // Global gitignore and .git/info/exclude stay off: nothing from the
            // operator's home directory may narrow a host search.
            .hidden(true)
            .parents(true)
            .ignore(true)
            .git_ignore(true)
            // ripgrep's own default: .gitignore applies even outside a git
            // repository; only an in-tree .git would change rule precedence.
            .require_git(false)
            .git_global(false)
            .git_exclude(false)
            .sort_by_file_path(|a, b| a.cmp(b))
            .build();
        for item in walker {
            if std::time::Instant::now() >= self.deadline {
                self.stop("time");
                return Ok(());
            }
            let Ok(entry) = item else { continue };
            if entry.depth() == 0 {
                continue;
            }
            // `ignore` classifies from the directory entry; a symlink is a
            // symlink here, never its target, and falls through with dirs.
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() {
                continue;
            }
            let Some(canonical) = self.contained(entry.path()) else {
                continue;
            };
            let rel_abs = canonical.strip_prefix(self.anchor.base()).unwrap();
            let rel_path = rel_to_string(rel_abs);
            let basename = canonical
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            if self
                .glob
                .as_ref()
                .is_some_and(|glob| !glob.matches(&rel_path, basename))
            {
                continue;
            }
            match self.mode {
                HostFileSearchMode::Name => {
                    self.files_scanned += 1;
                    if self.needle.is_match(basename)
                        && !self.push_match(rel_path, 0, None, basename.to_owned())
                    {
                        return Ok(());
                    }
                }
                HostFileSearchMode::Content => {
                    if !self.content_file(&canonical, &rel_path)? {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    /// Scan one content-mode file. Returns false when the whole walk must stop
    /// (result cap reached); truncating byte bounds only skip the file.
    fn content_file(&mut self, target: &Path, rel_path: &str) -> Result<bool, NodeError> {
        let metadata = std::fs::symlink_metadata(target).map_err(|error| {
            NodeError::InvalidRequest(format!("{} cannot be inspected: {error}", target.display()))
        })?;
        if !metadata.file_type().is_file() {
            return Ok(true);
        }
        let size = metadata.len();
        if size > self.per_file_bytes {
            self.stop("per-file-bytes");
            return Ok(true);
        }
        if self.bytes_scanned.saturating_add(size) > self.total_bytes {
            self.stop("total-bytes");
            return Ok(false);
        }
        let bytes = std::fs::read(target).map_err(|error| {
            NodeError::InvalidRequest(format!("{} unreadable: {error}", target.display()))
        })?;
        if bytes.len() as u64 > self.per_file_bytes {
            self.stop("per-file-bytes");
            return Ok(true);
        }
        // A NUL byte marks binary content; binary and irregular files are
        // skipped, exactly like a default ripgrep invocation, and are not
        // counted among the files whose contents were scanned.
        if bytes.contains(&0) {
            return Ok(true);
        }
        self.files_scanned += 1;
        self.bytes_scanned = self.bytes_scanned.saturating_add(bytes.len() as u64);
        let text = String::from_utf8_lossy(&bytes);
        for (index, raw_line) in text.split('\n').enumerate() {
            let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
            if let Some((column, _len)) = self.needle.find(line) {
                let snippet = cap_snippet(line, column);
                if !self.push_match(
                    rel_path.to_owned(),
                    index as u64 + 1,
                    Some(column as u64 + 1),
                    snippet,
                ) {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
}

/// Anchor-relative reported path, always `/`-separated.
fn rel_to_string(rel_abs: &Path) -> String {
    rel_abs
        .components()
        .filter_map(|component| match component {
            Component::Normal(segment) => Some(segment.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Trim a matched line to [`SEARCH_SNIPPET_CHARS`] around the hit, on UTF-8
/// boundaries, marking a shortened window with an ellipsis on either side.
fn cap_snippet(line: &str, byte_column: usize) -> String {
    let total = line.chars().count();
    if total <= SEARCH_SNIPPET_CHARS {
        return line.to_owned();
    }
    // The match column is a byte offset on a char boundary.
    let hit_char = line
        .char_indices()
        .position(|(index, _)| index == byte_column)
        .unwrap_or(0);
    let half = SEARCH_SNIPPET_CHARS / 2;
    let mut start = hit_char.saturating_sub(half);
    if total - start < SEARCH_SNIPPET_CHARS {
        start = total - SEARCH_SNIPPET_CHARS;
    }
    let end = (start + SEARCH_SNIPPET_CHARS).min(total);
    let window: String = line.chars().skip(start).take(end - start).collect();
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        window,
        if end < total { "…" } else { "" }
    )
}

/// Run a bounded read-only search and build the RPC result.
fn search_files(
    workspace_id: &str,
    workspaces: &[Workspace],
    request: HostFilesSearchParams,
) -> Result<remuda_protocol::hubnode::HostFilesSearchResult, NodeError> {
    let query = request.query.trim();
    if query.is_empty() {
        return Err(NodeError::InvalidRequest("search query is empty".into()));
    }
    let max_results = match request.max_results {
        Some(0) => {
            return Err(NodeError::InvalidRequest(
                "maxResults must be at least 1".into(),
            ));
        }
        Some(value) => value as usize,
        None => SEARCH_DEFAULT_MAX_RESULTS,
    };
    let anchor = resolve_anchor(workspace_id, workspaces)?;
    let rel_path = request.rel_path.as_deref().unwrap_or_default();
    let roots = search_roots(&anchor, rel_path)?;
    let root_display = match roots.as_slice() {
        [single] => single.dir.display().to_string(),
        _ => anchor.base().display().to_string(),
    };
    let needle = Needle::compile(query, request.regex)?;
    let glob = match request
        .glob
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(raw) => Some(GlobNarrow::compile(raw)?),
        None => None,
    };
    let mut walk = SearchWalk {
        anchor: &anchor,
        mode: request.mode,
        needle,
        glob,
        max_results,
        per_file_bytes: SEARCH_PER_FILE_BYTES,
        total_bytes: SEARCH_TOTAL_BYTES,
        deadline: std::time::Instant::now() + SEARCH_WALL_CLOCK,
        matches: Vec::new(),
        files_scanned: 0,
        bytes_scanned: 0,
        truncated: false,
        truncated_reason: None,
    };
    for root in &roots {
        walk.walk_root(root)?;
        if walk.truncated
            && walk.truncated_reason.as_deref().is_some_and(|reason| {
                reason == "max-results" || reason == "total-bytes" || reason == "time"
            })
        {
            break;
        }
    }
    Ok(remuda_protocol::hubnode::HostFilesSearchResult {
        workspace_id: workspace_id.to_owned(),
        path: root_display,
        mode: request.mode,
        query: query.to_owned(),
        matches: walk.matches,
        truncated: walk.truncated,
        truncated_reason: walk.truncated_reason,
        files_scanned: walk.files_scanned,
        bytes_scanned: walk.bytes_scanned,
    })
}

impl crate::DevNode {
    /// Dispatch a `host.files.*` RPC onto the local filesystem.
    pub(crate) async fn host_files_rpc(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, NodeError> {
        if method == "host.files.search" {
            let request: HostFilesSearchParams = serde_json::from_value(params)?;
            let workspace_id = request.workspace_id.trim().to_owned();
            if workspace_id.is_empty() {
                return Err(NodeError::InvalidRequest(
                    "host file request requires a workspaceId".into(),
                ));
            }
            let workspaces = self.workspaces()?;
            let result = tokio::task::spawn_blocking(move || {
                search_files(&workspace_id, &workspaces, request)
            })
            .await
            .map_err(|error| NodeError::Driver(format!("host files task failed: {error}")))??;
            return Ok(serde_json::to_value(result)?);
        }
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

    fn search_value(workspace_id: &str, extra: serde_json::Value) -> Value {
        let mut value = json!({"workspaceId": workspace_id});
        if let Some(object) = extra.as_object() {
            for (key, item) in object {
                value[key] = item.clone();
            }
        }
        value
    }

    fn seed_search_tree(root: &Path) {
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(
            root.join("alpha.txt"),
            b"needle in line one\nboring line\nNEEDLE caps line\n",
        )
        .unwrap();
        fs::write(root.join("sub").join("beta.rs"), b"let needle = 1;\n").unwrap();
        fs::write(root.join("ignored.log"), b"needle logged\n").unwrap();
        fs::write(root.join(".gitignore"), b"*.log\n").unwrap();
        // NUL byte => binary, skipped even though it contains the needle.
        fs::write(root.join("binary.dat"), b"needle\x00\x01binary\n").unwrap();
    }

    #[tokio::test]
    async fn search_name_content_regex_and_glob_modes() {
        let root = tempfile::tempdir().unwrap();
        seed_search_tree(root.path());
        let (node, workspace) = workspace_node(root.path());
        let ws = workspace.meta.id.as_id().to_string();

        // Name mode: matches the entry name only.
        let result = node
            .host_files_rpc(
                "host.files.search",
                search_value(&ws, json!({"query": "alpha", "mode": "name"})),
            )
            .await
            .unwrap();
        assert_eq!(result["matches"][0]["path"], json!("alpha.txt"));
        assert_eq!(result["matches"][0]["line"], json!(0));
        assert_eq!(result["truncated"], json!(false));

        // Content literal: hits alpha.txt and sub/beta.rs; the gitignored log
        // and the binary dat are absent.
        let result = node
            .host_files_rpc(
                "host.files.search",
                search_value(&ws, json!({"query": "needle", "mode": "content"})),
            )
            .await
            .unwrap();
        let hits: Vec<(String, u64)> = result["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hit| {
                (
                    hit["path"].as_str().unwrap().to_owned(),
                    hit["line"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            hits,
            vec![("alpha.txt".to_owned(), 1), ("sub/beta.rs".to_owned(), 1)]
        );
        assert_eq!(result["filesScanned"], json!(2));
        assert!(
            result["matches"][0]["snippet"]
                .as_str()
                .unwrap()
                .contains("needle")
        );

        // Regex mode: case-insensitive needle also matches line three.
        let result = node
            .host_files_rpc(
                "host.files.search",
                search_value(
                    &ws,
                    json!({"query": "(?i)needle", "mode": "content", "regex": true}),
                ),
            )
            .await
            .unwrap();
        let lines: Vec<u64> = result["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|hit| hit["path"] == json!("alpha.txt"))
            .map(|hit| hit["line"].as_u64().unwrap())
            .collect();
        assert_eq!(lines, vec![1, 3]);

        // Glob narrowing to *.rs keeps only the Rust file.
        let result = node
            .host_files_rpc(
                "host.files.search",
                search_value(
                    &ws,
                    json!({"query": "needle", "mode": "content", "glob": "*.rs"}),
                ),
            )
            .await
            .unwrap();
        let paths: Vec<String> = result["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hit| hit["path"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(paths, vec!["sub/beta.rs".to_owned()]);

        // A rooted glob with a ** prefix matches at depth.
        let result = node
            .host_files_rpc(
                "host.files.search",
                search_value(
                    &ws,
                    json!({"query": "needle", "mode": "content", "glob": "**/*.rs"}),
                ),
            )
            .await
            .unwrap();
        assert_eq!(result["matches"][0]["path"], json!("sub/beta.rs"));

        // A bad regex is a clean request error.
        let error = node
            .host_files_rpc(
                "host.files.search",
                search_value(&ws, json!({"query": "(", "mode": "content", "regex": true})),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid search regex"), "{error}");
    }

    #[tokio::test]
    async fn search_rejects_traversal_and_symlink_escape() {
        let root = tempfile::tempdir().unwrap();
        seed_search_tree(root.path());
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("escape.txt"), b"needle escape\n").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("escape.txt"),
            root.path().join("evil-link.txt"),
        )
        .unwrap();
        let (node, workspace) = workspace_node(root.path());
        let ws = workspace.meta.id.as_id().to_string();

        let error = node
            .host_files_rpc(
                "host.files.search",
                search_value(
                    &ws,
                    json!({"query": "needle", "mode": "content", "relPath": "../escape-dir"}),
                ),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("escapes"), "{error}");

        // The escaped symlink target is never read: its match is absent.
        let result = node
            .host_files_rpc(
                "host.files.search",
                search_value(&ws, json!({"query": "needle", "mode": "content"})),
            )
            .await
            .unwrap();
        let paths: Vec<String> = result["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hit| hit["path"].as_str().unwrap().to_owned())
            .collect();
        assert!(!paths.iter().any(|path| path.contains("evil-link")));
    }

    #[tokio::test]
    async fn search_caps_truncate_with_reasons() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..5 {
            fs::write(
                root.path().join(format!("match-{index}.txt")),
                b"needle hit\n",
            )
            .unwrap();
        }
        let (node, workspace) = workspace_node(root.path());
        let ws = workspace.meta.id.as_id().to_string();

        // Result cap: two matches returned and truncated.
        let result = node
            .host_files_rpc(
                "host.files.search",
                search_value(
                    &ws,
                    json!({"query": "match", "mode": "name", "maxResults": 2}),
                ),
            )
            .await
            .unwrap();
        assert_eq!(result["matches"].as_array().unwrap().len(), 2);
        assert_eq!(result["truncated"], json!(true));
        assert_eq!(result["truncatedReason"], json!("max-results"));

        // Byte caps and the wall clock are enforced on the inner walker so the
        // test does not wait for the production timeouts.
        let anchor = resolve_anchor(&ws, &node.workspaces().unwrap()).unwrap();
        let roots = search_roots(&anchor, "").unwrap();
        let mut walk = SearchWalk {
            anchor: &anchor,
            mode: HostFileSearchMode::Content,
            needle: Needle::Literal("needle".into()),
            glob: None,
            max_results: 100,
            per_file_bytes: 4,
            total_bytes: SEARCH_TOTAL_BYTES,
            deadline: std::time::Instant::now() + SEARCH_WALL_CLOCK,
            matches: Vec::new(),
            files_scanned: 0,
            bytes_scanned: 0,
            truncated: false,
            truncated_reason: None,
        };
        walk.walk_root(&roots[0]).unwrap();
        // Every file exceeds the 4-byte per-file cap: truncated, no reads.
        assert!(walk.truncated);
        assert_eq!(walk.truncated_reason.as_deref(), Some("per-file-bytes"));
        assert_eq!(walk.bytes_scanned, 0);

        // Total-byte cap after one small file stops the walk.
        fs::write(root.path().join("small-a.txt"), b"needle a\n").unwrap();
        fs::write(root.path().join("small-b.txt"), b"needle b\n").unwrap();
        let mut walk = SearchWalk {
            anchor: &anchor,
            mode: HostFileSearchMode::Content,
            needle: Needle::Literal("needle".into()),
            glob: Some(GlobNarrow::compile("small-*.txt").unwrap()),
            max_results: 100,
            per_file_bytes: SEARCH_PER_FILE_BYTES,
            total_bytes: 9,
            deadline: std::time::Instant::now() + SEARCH_WALL_CLOCK,
            matches: Vec::new(),
            files_scanned: 0,
            bytes_scanned: 0,
            truncated: false,
            truncated_reason: None,
        };
        walk.walk_root(&roots[0]).unwrap();
        assert_eq!(walk.truncated_reason.as_deref(), Some("total-bytes"));
        assert_eq!(walk.files_scanned, 1);

        // An already-expired deadline stops at the first boundary.
        let mut walk = SearchWalk {
            anchor: &anchor,
            mode: HostFileSearchMode::Name,
            needle: Needle::Literal("match".into()),
            glob: None,
            max_results: 100,
            per_file_bytes: SEARCH_PER_FILE_BYTES,
            total_bytes: SEARCH_TOTAL_BYTES,
            deadline: std::time::Instant::now() - std::time::Duration::from_secs(1),
            matches: Vec::new(),
            files_scanned: 0,
            bytes_scanned: 0,
            truncated: false,
            truncated_reason: None,
        };
        walk.walk_root(&roots[0]).unwrap();
        assert!(walk.truncated);
        assert_eq!(walk.truncated_reason.as_deref(), Some("time"));
    }

    #[tokio::test]
    async fn search_scratch_area_only_walks_remuda_dirs() {
        let tmp = std::env::temp_dir();
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let scratch_name = format!("remuda-search-test-{suffix}");
        let scratch = tmp.join(&scratch_name);
        fs::create_dir_all(&scratch).unwrap();
        fs::write(scratch.join("scratchy.txt"), b"needle scratch\n").unwrap();
        let unrelated = tmp.join(format!("not-remuda-search-{suffix}"));
        fs::create_dir_all(&unrelated).unwrap();
        fs::write(unrelated.join("outside.txt"), b"needle outside\n").unwrap();

        let anchor = Anchor::Scratch(fs_canonicalize(&tmp).unwrap());
        // Target the one scratch directory explicitly: the empty-selector
        // fan-out would also scan every other /tmp/remuda-* directory on a
        // shared host (bounded by the wall-clock cap).
        let result = search_files(
            HOST_FILES_SCRATCH_ID,
            &[],
            HostFilesSearchParams {
                workspace_id: HOST_FILES_SCRATCH_ID.into(),
                rel_path: Some(scratch_name.clone()),
                query: "needle".into(),
                mode: HostFileSearchMode::Content,
                regex: false,
                glob: None,
                max_results: None,
            },
        )
        .unwrap();
        let paths: Vec<String> = result.matches.iter().map(|hit| hit.path.clone()).collect();
        assert_eq!(paths, vec![format!("{scratch_name}/scratchy.txt")]);

        // The sibling non-remuda directory is outside the scratch anchor even
        // when named directly.
        let error =
            search_roots(&anchor, &format!("not-remuda-search-{suffix}/outside.txt")).unwrap_err();
        assert!(error.to_string().contains("scratch area"), "{}", error);

        fs::remove_dir_all(&scratch).ok();
        fs::remove_dir_all(&unrelated).ok();
    }
}
