//! Read-only, real-time git working-tree state for the 「工作区当前变更」 view.
//!
//! Three JSON-RPCs (`workspace.scm.status` / `.diff` / `.file`) compute the
//! *current* state of one registered workspace (files-view-contract §3). They
//! never mutate the workspace: git is invoked with `--no-optional-locks` through
//! a fixed argv whitelist (no shell), every read is bounded by a deadline and a
//! byte ceiling, and caller paths are re-checked against the registered root
//! with [`remuda_protocol::path_guard`] immediately before reading.

use crate::{DevNode, NodeError};
use remuda_protocol::WorkspaceId;
use remuda_protocol::path_guard;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Entry cap for one `workspace.scm.status` answer (contract §3.5).
pub const MAX_ENTRIES: usize = 5000;
/// Total diff bytes returned by one `workspace.scm.diff` request.
pub const MAX_DIFF_BYTES: usize = 256 * 1024;
/// Max inline bytes (and digestible size) for one `workspace.scm.file`.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Hard ceiling on raw `git status` output accepted in one request.
const MAX_STATUS_BYTES: usize = 4 * 1024 * 1024;
/// Deadline for every git / filesystem operation in this module.
const SCM_TIMEOUT: Duration = Duration::from_secs(5);
const ZERO_OID: &str = "0000000000000000000000000000000000000000";

/// True when `method` is one of the read-only SCM RPCs; dispatch tables use
/// this instead of any catch-all (contract §3.5).
pub fn is_scm_method(method: &str) -> bool {
    matches!(
        method,
        "workspace.scm.status" | "workspace.scm.diff" | "workspace.scm.file"
    )
}

/// Dispatch a read-only SCM RPC against the Node's own workspace registry.
pub fn handle_rpc(node: &DevNode, method: &str, params: &Value) -> Result<Value, NodeError> {
    let workspace_id = params
        .get("workspaceId")
        .and_then(Value::as_str)
        .ok_or_else(|| NodeError::InvalidRequest("scm rpc requires workspaceId".into()))
        .and_then(|raw| WorkspaceId::from_str(raw).map_err(NodeError::from))?;
    let root = registered_root(node, &workspace_id)?;
    match method {
        "workspace.scm.status" => Ok(status(&workspace_id, &root)),
        "workspace.scm.diff" => diff(&workspace_id, &root, params),
        "workspace.scm.file" => file(&workspace_id, &root, params),
        other => Err(NodeError::InvalidRequest(format!(
            "unknown scm method {other}"
        ))),
    }
}

/// `handle_rpc` with its blocking git work off the async runtime's threads,
/// under the carrier's in-flight cap.
///
/// These are read-only, but they are not cheap: `status` and `diff` shell out
/// to git over the whole worktree, which on a large or cold repo is seconds of
/// blocking. A carrier that spawns them off the select loop still parks a
/// runtime worker for that time, so on a one- or two-worker host a few
/// concurrent `scm.diff`s park the loop — `gate.cancel` included.
pub(crate) async fn handle_rpc_capped(
    node: &DevNode,
    method: &str,
    params: &Value,
) -> Result<Value, NodeError> {
    let node = node.clone();
    let method = method.to_owned();
    let params = params.clone();
    crate::gate::run_long_method(move || handle_rpc(&node, &method, &params)).await
}

/// Resolve a registered workspace by id and re-validate its canonical identity.
///
/// An unregistered id, a removed root, or a root that moved since registration
/// are all `NotFound` — the view's 「工作区不存在」 state (contract §3.6).
fn registered_root(node: &DevNode, id: &WorkspaceId) -> Result<PathBuf, NodeError> {
    let workspace = node
        .workspaces()?
        .into_iter()
        .find(|workspace| &workspace.meta.id == id)
        .ok_or_else(|| NodeError::NotFound {
            entity: "workspace",
            id: id.as_id().to_string(),
        })?;
    let stored = PathBuf::from(&workspace.root_path);
    let canonical = stored.canonicalize().map_err(|_| NodeError::NotFound {
        entity: "workspace",
        id: id.as_id().to_string(),
    })?;
    if canonical != stored || !canonical.is_dir() {
        return Err(NodeError::NotFound {
            entity: "workspace",
            id: id.as_id().to_string(),
        });
    }
    Ok(canonical)
}

/// Normalize a caller-supplied relative path and require it inside `root`.
fn contained_path(root: &Path, raw: &str) -> Result<PathBuf, NodeError> {
    if raw.is_empty() || raw.contains('\0') {
        return Err(NodeError::InvalidRequest(
            "path must be a non-empty string".into(),
        ));
    }
    let candidate = PathBuf::from(raw);
    if candidate.is_absolute() {
        return Err(NodeError::InvalidRequest(
            "path must be relative to the registered workspace".into(),
        ));
    }
    // The path is handed to git as a literal pathspec; reject pathspec magic,
    // globs, and traversal so a string that resolves inside the root cannot be
    // reinterpreted by git to address something outside it (contract §3.3).
    if raw.starts_with(':')
        || candidate
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        || raw.bytes().any(|byte| matches!(byte, b'*' | b'?' | b'['))
    {
        return Err(NodeError::InvalidRequest(format!(
            "pathspec rejected (no traversal, globs, or pathspec magic): {raw:?}"
        )));
    }
    let absolute = path_guard::absolutize(root, &candidate);
    path_guard::contain(&[root], &absolute).map_err(|error| {
        NodeError::InvalidRequest(format!("path rejected by workspace containment: {error}"))
    })?;
    // `contain` accepts the root itself; the RPCs operate on files inside it.
    if path_guard::real_path(root).ok().as_ref() == path_guard::real_path(&absolute).ok().as_ref() {
        return Err(NodeError::InvalidRequest(
            "path must name a file, not the workspace root".into(),
        ));
    }
    Ok(absolute)
}

// ────────────────────────────────────────────────────────────────────────────
// status
// ────────────────────────────────────────────────────────────────────────────

fn status(workspace_id: &WorkspaceId, root: &Path) -> Value {
    let observed_at = now_rfc3339();
    let mut envelope = json!({
        "workspaceId": workspace_id.as_id().as_str(),
        "root": root.to_string_lossy(),
        "scm": "git",
        "availability": "ok",
        "headOid": Value::Null,
        "branch": {"state": "unknown", "reason": "unknown"},
        "observedAt": observed_at,
        "entries": [],
        "limits": limits(),
        "truncated": {"entries": false, "entriesOmitted": 0, "nonUtf8Omitted": 0, "statusBytes": false},
        "ignoreRules": "git-default",
    });

    match git_toplevel(root) {
        Ok(_) => {}
        Err(GitFailure::Unsupported(reason)) => {
            envelope["availability"] = json!("unsupported");
            envelope["unsupportedReason"] = json!(reason);
            return envelope;
        }
        Err(GitFailure::Denied(reason)) => {
            envelope["availability"] = json!("denied");
            envelope["deniedReason"] = json!(reason);
            return envelope;
        }
    }

    envelope["headOid"] = match git_head(root) {
        Ok(oid) if !oid.is_empty() => json!(oid),
        _ => Value::Null,
    };
    envelope["branch"] = match git_branch(root) {
        Ok(value) => json!({"state": "known", "value": value}),
        Err(GitFailure::Unsupported(_)) => {
            json!({"state": "unknown", "reason": "detached-or-unborn"})
        }
        Err(GitFailure::Denied(reason)) => json!({"state": "unknown", "reason": reason}),
    };

    let raw = match git_status(root) {
        Ok(raw) => raw,
        Err(GitFailure::Unsupported(reason)) => {
            envelope["availability"] = json!("unsupported");
            envelope["unsupportedReason"] = json!(reason);
            return envelope;
        }
        Err(GitFailure::Denied(reason)) => {
            envelope["availability"] = json!("denied");
            envelope["deniedReason"] = json!(reason);
            return envelope;
        }
    };
    if raw.stdout_capped {
        envelope["truncated"]["statusBytes"] = json!(true);
    }

    let mut records: Vec<StatusRecord> = Vec::new();
    let mut tokens = raw.stdout.split(|byte| *byte == 0).peekable();
    while let Some(frame) = tokens.next() {
        if frame.is_empty() {
            continue;
        }
        let tag = frame.first().copied().unwrap_or(0);
        let record = match tag {
            b'?' => parse_untracked(frame),
            b'1' => parse_changed(frame),
            b'2' => {
                // Renames carry a second NUL field with the original path.
                let orig = tokens.next().unwrap_or_default();
                parse_changed(frame).map(|mut record| {
                    record.orig_path = String::from_utf8(orig.to_vec()).ok();
                    record
                })
            }
            b'u' => parse_unmerged(frame),
            _ => None,
        };
        match record {
            Some(record) => records.push(record),
            None => {
                envelope["truncated"]["nonUtf8Omitted"] = json!(
                    envelope["truncated"]["nonUtf8Omitted"]
                        .as_u64()
                        .unwrap_or(0)
                        + 1
                );
            }
        }
    }

    // git status order is already byte-sorted; keep it but cap deterministically.
    let omitted = records.len().saturating_sub(MAX_ENTRIES);
    if omitted > 0 {
        records.truncate(MAX_ENTRIES);
        envelope["truncated"]["entries"] = json!(true);
        envelope["truncated"]["entriesOmitted"] = json!(omitted);
    }

    let entries: Vec<Value> = records
        .into_iter()
        .map(|record| {
            let size = std::fs::symlink_metadata(root.join(&record.path))
                .ok()
                .filter(|meta| meta.is_file())
                .map(|meta| meta.len());
            json!({
                "path": record.path,
                "origPath": record.orig_path,
                "xy": record.xy,
                "kind": record.kind,
                "sizeBytes": size,
                "oldOid": zero_to_null(record.old_oid),
                "newOid": zero_to_null(record.new_oid),
                "digest": {"state": "unknown", "reason": "not-collected"},
            })
        })
        .collect();
    envelope["entries"] = json!(entries);
    envelope
}

#[derive(Debug)]
struct StatusRecord {
    path: String,
    orig_path: Option<String>,
    xy: String,
    kind: String,
    old_oid: Option<String>,
    new_oid: Option<String>,
}

fn parse_untracked(frame: &[u8]) -> Option<StatusRecord> {
    let line = std::str::from_utf8(frame).ok()?;
    let path = line.strip_prefix("? ")?;
    Some(StatusRecord {
        path: path.to_owned(),
        orig_path: None,
        xy: "??".into(),
        kind: "untracked".into(),
        old_oid: None,
        new_oid: None,
    })
}

fn parse_changed(frame: &[u8]) -> Option<StatusRecord> {
    let line = std::str::from_utf8(frame).ok()?;
    let tag = line.split(' ').next()?;
    if tag != "1" && tag != "2" {
        return None;
    }
    let mut fields = line.split(' ');
    let xy = fields.nth(1)?;
    // Ordinary `1` lines carry 8 space-separated fields before the path; rename
    // `2` lines carry a ninth `R<score>` field.
    let field_count = if tag == "2" { 9 } else { 8 };
    let path = &line[fields_offset(line, field_count)..];
    let oids = porcelain_oids(line);
    // Porcelain v2 marks the unchanged side with `.`; emit the classic XY
    // spelling (space) that `git status --porcelain` documents and the web
    // view renders.
    let xy: String = xy
        .chars()
        .map(|char| if char == '.' { ' ' } else { char })
        .collect();
    let xy_chars: Vec<char> = xy.chars().collect();
    Some(StatusRecord {
        path: path.to_owned(),
        orig_path: None,
        kind: kind_of(xy_chars.first().copied(), xy_chars.get(1).copied()),
        xy,
        old_oid: oids.0,
        new_oid: oids.1,
    })
}

/// Byte offset just past `fields` space-separated tokens, i.e. the first byte of
/// the NUL-terminated path (the path itself is taken verbatim, spaces and all).
fn fields_offset(line: &str, fields: usize) -> usize {
    let mut offset = 0;
    for _ in 0..fields {
        match line[offset..].find(' ') {
            Some(relative) => offset += relative + 1,
            None => return line.len(),
        }
    }
    offset
}

/// Extract the head/worktree OIDs (fields 7 and 8 on an ordinary `1`/`2` line).
fn porcelain_oids(line: &str) -> (Option<String>, Option<String>) {
    let mut fields = line.split(' ');
    let old = fields.nth(6).map(str::to_owned);
    let new = fields.next().map(str::to_owned);
    (old, new)
}

fn parse_unmerged(frame: &[u8]) -> Option<StatusRecord> {
    let line = std::str::from_utf8(frame).ok()?;
    let xy = line.split(' ').nth(1)?;
    if line.as_bytes().first() != Some(&b'u') {
        return None;
    }
    // `u XY sub m1 m2 m3 w1 w2 w3 path` — nine fields before the path.
    let path = &line[fields_offset(line, 9)..];
    Some(StatusRecord {
        path: path.to_owned(),
        orig_path: None,
        xy: xy.to_owned(),
        kind: "unmerged".into(),
        old_oid: None,
        new_oid: None,
    })
}

fn kind_of(x: Option<char>, y: Option<char>) -> String {
    match x {
        Some('A') => "added",
        Some('R') => "renamed",
        Some('C') => "copied",
        Some('T') => "type-changed",
        Some('U') => "unmerged",
        Some('D') => "deleted",
        _ => match y {
            Some('D') => "deleted",
            Some('M') => "modified",
            Some('T') => "type-changed",
            _ => "changed",
        },
    }
    .to_owned()
}

fn zero_to_null(oid: Option<String>) -> Value {
    match oid {
        Some(value) if value != ZERO_OID => json!(value),
        _ => Value::Null,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// diff
// ────────────────────────────────────────────────────────────────────────────

fn diff(workspace_id: &WorkspaceId, root: &Path, params: &Value) -> Result<Value, NodeError> {
    let staged = params
        .get("staged")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let paths = params
        .get("paths")
        .and_then(Value::as_array)
        .ok_or_else(|| NodeError::InvalidRequest("workspace.scm.diff requires paths".into()))?;
    if paths.len() > MAX_ENTRIES {
        return Err(NodeError::InvalidRequest("too many paths".into()));
    }
    let mut relative: Vec<String> = Vec::with_capacity(paths.len());
    for value in paths {
        let raw = value
            .as_str()
            .ok_or_else(|| NodeError::InvalidRequest("paths must be strings".into()))?;
        // Validate every path now; the resolved path is intentionally not used
        // as argv — git receives the original relative spelling after `--`.
        contained_path(root, raw)?;
        relative.push(raw.to_owned());
    }

    let observed_at = now_rfc3339();
    let mut envelope = json!({
        "workspaceId": workspace_id.as_id().as_str(),
        "root": root.to_string_lossy(),
        "scm": "git",
        "availability": "ok",
        "headOid": Value::Null,
        "staged": staged,
        "observedAt": observed_at,
        "items": [],
        "limits": limits(),
        "truncated": {"diffBytes": false, "bytesOmitted": 0},
    });

    match git_toplevel(root) {
        Ok(_) => {}
        Err(GitFailure::Unsupported(reason)) => {
            envelope["availability"] = json!("unsupported");
            envelope["unsupportedReason"] = json!(reason);
            return Ok(envelope);
        }
        Err(GitFailure::Denied(reason)) => {
            envelope["availability"] = json!("denied");
            envelope["deniedReason"] = json!(reason);
            return Ok(envelope);
        }
    }
    envelope["headOid"] = match git_head(root) {
        Ok(oid) if !oid.is_empty() => json!(oid),
        _ => Value::Null,
    };

    let mut budget = MAX_DIFF_BYTES;
    let mut items: Vec<Value> = Vec::new();
    for path in relative {
        if budget == 0 {
            items.push(json!({"path": path, "patch": Value::Null, "binary": false,
                "truncated": true, "bytesAvailable": 0}));
            envelope["truncated"]["diffBytes"] = json!(true);
            envelope["truncated"]["bytesOmitted"] =
                json!(envelope["truncated"]["bytesOmitted"].as_u64().unwrap_or(0));
            continue;
        }
        let raw = match git_diff(root, &path, staged, budget + 1) {
            Ok(raw) => raw,
            Err(GitFailure::Unsupported(_)) => {
                items.push(json!({"path": path, "patch": Value::Null, "binary": false,
                    "truncated": false, "bytesAvailable": 0}));
                continue;
            }
            Err(GitFailure::Denied(_)) => {
                envelope["availability"] = json!("denied");
                envelope["deniedReason"] = json!("git-error");
                items.clear();
                envelope["items"] = json!(items);
                return Ok(envelope);
            }
        };
        if raw
            .stdout
            .windows(13)
            .any(|window| window == b"Binary files ")
        {
            items.push(json!({"path": path, "patch": Value::Null, "binary": true,
                "truncated": false, "bytesAvailable": 0}));
            continue;
        }
        let total = raw.stdout.len();
        if total > budget {
            let cut = raw.stdout[..budget]
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(budget, |index| index + 1);
            let patch = String::from_utf8_lossy(&raw.stdout[..cut]).into_owned();
            items.push(json!({"path": path, "patch": patch, "binary": false,
                "truncated": true, "bytesAvailable": cut}));
            envelope["truncated"]["diffBytes"] = json!(true);
            envelope["truncated"]["bytesOmitted"] = json!(
                envelope["truncated"]["bytesOmitted"].as_u64().unwrap_or(0)
                    + u64::try_from(total - cut).unwrap_or(0)
            );
            budget = 0;
            continue;
        }
        let patch = String::from_utf8_lossy(&raw.stdout).into_owned();
        budget -= total;
        items.push(json!({"path": path, "patch": patch, "binary": false,
            "truncated": false, "bytesAvailable": total}));
    }
    envelope["items"] = json!(items);
    Ok(envelope)
}

// ────────────────────────────────────────────────────────────────────────────
// file
// ────────────────────────────────────────────────────────────────────────────

fn file(workspace_id: &WorkspaceId, root: &Path, params: &Value) -> Result<Value, NodeError> {
    let raw_path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| NodeError::InvalidRequest("workspace.scm.file requires path".into()))?;
    // Re-resolve at read time: the file may have been replaced by a symlink
    // between the status and the content request (contract §3.5).
    let absolute = contained_path(root, raw_path)?;
    let relative = absolute
        .strip_prefix(root)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| raw_path.to_owned());

    let mut envelope = json!({
        "workspaceId": workspace_id.as_id().as_str(),
        "root": root.to_string_lossy(),
        "scm": "git",
        "availability": "ok",
        "path": relative,
        "headOid": Value::Null,
        "observedAt": now_rfc3339(),
        "mediaType": "text/plain",
        "binary": false,
        "sizeBytes": Value::Null,
        "digest": {"state": "unknown", "reason": "not-collected"},
        "content": Value::Null,
        "truncated": false,
        "limits": limits(),
    });

    match git_toplevel(root) {
        Ok(_) => {}
        Err(GitFailure::Unsupported(reason)) => {
            envelope["availability"] = json!("unsupported");
            envelope["unsupportedReason"] = json!(reason);
            return Ok(envelope);
        }
        Err(GitFailure::Denied(reason)) => {
            envelope["availability"] = json!("denied");
            envelope["deniedReason"] = json!(reason);
            return Ok(envelope);
        }
    }
    envelope["headOid"] = match git_head(root) {
        Ok(oid) if !oid.is_empty() => json!(oid),
        _ => Value::Null,
    };

    let meta = std::fs::symlink_metadata(&absolute).map_err(|_| NodeError::NotFound {
        entity: "workspace-file",
        id: raw_path.to_owned(),
    })?;
    if !meta.is_file() {
        return Err(NodeError::InvalidRequest(format!(
            "{raw_path} is not a regular file"
        )));
    }
    let size = meta.len();
    envelope["sizeBytes"] = json!(size);

    if size > MAX_FILE_BYTES {
        envelope["truncated"] = json!(true);
        envelope["digest"] = json!({"state": "unknown", "reason": "file-exceeds-inline-limit"});
        envelope["mediaType"] = json!(sniff_media_type(&read_prefix(&absolute, 8192)));
        return Ok(envelope);
    }

    // Re-stat through the opened descriptor-equivalent: the file can grow (or
    // become a symlink) between `symlink_metadata` and the open. O_NOFOLLOW plus
    // a bounded read keeps the answer within `maxFileBytes` regardless.
    let handle = open_regular_file(&absolute)?;
    let mut bytes = Vec::new();
    handle
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| NodeError::InvalidRequest(format!("read {raw_path}: {error}")))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        envelope["truncated"] = json!(true);
        envelope["digest"] = json!({"state": "unknown", "reason": "file-exceeds-inline-limit"});
        envelope["sizeBytes"] = json!(bytes.len() as u64);
        return Ok(envelope);
    }
    let digest = format!("sha256:{}", hex_lower(&Sha256::digest(&bytes)));
    envelope["digest"] = json!({"state": "known", "value": digest});

    match std::str::from_utf8(&bytes) {
        // A NUL is valid UTF-8 but denotes a binary payload for our purposes;
        // match the prefix sniffer used on over-limit files.
        Ok(text) if !text.as_bytes().contains(&0) => {
            envelope["content"] = json!(text);
        }
        _ => {
            envelope["binary"] = json!(true);
            envelope["mediaType"] = json!("application/octet-stream");
        }
    }
    Ok(envelope)
}

/// Open a regular file without following a final symlink. The path has already
/// passed `path_guard::contain`; O_NOFOLLOW closes the TOCTOU window in which
/// the leaf could be swapped for an escaping symlink before the open.
fn open_regular_file(path: &Path) -> Result<std::fs::File, NodeError> {
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .custom_flags(libc_o_no_follow())
            .open(path)
            .map_err(|error| {
                if error.raw_os_error() == Some(nix::errno::Errno::ELOOP as i32) {
                    NodeError::InvalidRequest("refusing to read through a symlink".into())
                } else {
                    NodeError::NotFound {
                        entity: "workspace-file",
                        id: path.display().to_string(),
                    }
                }
            })
    }
    #[cfg(not(unix))]
    {
        std::fs::File::open(path).map_err(|_| NodeError::NotFound {
            entity: "workspace-file",
            id: path.display().to_string(),
        })
    }
}

#[cfg(unix)]
fn libc_o_no_follow() -> i32 {
    // Declared locally to avoid adding a libc dependency; matches Linux/macOS.
    // O_NOFOLLOW is 0o400000 on Linux and 0x100 on macOS.
    if cfg!(target_os = "macos") {
        0o100
    } else {
        0o400000
    }
}

fn read_prefix(path: &Path, max: usize) -> Vec<u8> {
    let mut out = vec![0u8; 0];
    if let Ok(mut handle) = std::fs::File::open(path) {
        let mut buf = [0u8; 4096];
        while out.len() < max {
            let want = (max - out.len()).min(buf.len());
            match handle.read(&mut buf[..want]) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
    }
    out
}

/// Binary sniff: a NUL in the prefix, or non-UTF-8 bytes, means not text.
fn sniff_media_type(prefix: &[u8]) -> String {
    if prefix.contains(&0) || std::str::from_utf8(prefix).is_err() {
        "application/octet-stream".to_owned()
    } else {
        "text/plain".to_owned()
    }
}

fn limits() -> Value {
    json!({
        "maxEntries": MAX_ENTRIES,
        "maxDiffBytes": MAX_DIFF_BYTES,
        "maxFileBytes": MAX_FILE_BYTES,
    })
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

// ────────────────────────────────────────────────────────────────────────────
// bounded, whitelisted git
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum GitFailure {
    Unsupported(&'static str),
    Denied(&'static str),
}

struct GitRaw {
    stdout: Vec<u8>,
    stdout_capped: bool,
    timed_out: bool,
    permission: bool,
    success: bool,
    stderr: String,
}

fn git_toplevel(root: &Path) -> Result<(), GitFailure> {
    // A registered workspace is supported only when *it* is the repository
    // root. On hosts where an ancestor happens to be a repo (e.g. a `/tmp`
    // checkout), git would otherwise report that ancestor and let a non-git
    // directory masquerade as supported.
    let raw = run_git(root, &["rev-parse", "--show-toplevel"], 4096)?;
    let raw = classify(raw)?;
    let toplevel = String::from_utf8_lossy(&raw.stdout).trim().to_owned();
    if Path::new(&toplevel) == root {
        Ok(())
    } else {
        Err(GitFailure::Unsupported("not-a-git-repository"))
    }
}

fn git_head(root: &Path) -> Result<String, GitFailure> {
    let raw = run_git(root, &["rev-parse", "HEAD"], 256)?;
    // Unborn branch: git exits 128 with "unknown revision".
    if !raw.success {
        return Err(GitFailure::Unsupported("unborn-head"));
    }
    Ok(String::from_utf8_lossy(&raw.stdout).trim().to_owned())
}

fn git_branch(root: &Path) -> Result<String, GitFailure> {
    let raw = run_git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"], 256)?;
    if raw.success {
        Ok(String::from_utf8_lossy(&raw.stdout).trim().to_owned())
    } else {
        Err(GitFailure::Unsupported("detached-or-unborn"))
    }
}

fn git_status(root: &Path) -> Result<GitRaw, GitFailure> {
    let raw = run_git(
        root,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
        MAX_STATUS_BYTES,
    )?;
    classify(raw)
}

fn git_diff(
    root: &Path,
    path: &str,
    staged: bool,
    stdout_cap: usize,
) -> Result<GitRaw, GitFailure> {
    let mut args: Vec<&str> = vec!["diff", "--no-color", "--no-ext-diff"];
    if staged {
        args.push("--cached");
    }
    args.push("--");
    // `path` only ever follows the literal `--`, so it is parsed as a pathspec.
    args.push(path);
    let raw = run_git(root, &args, stdout_cap)?;
    classify(raw)
}

/// Map a finished git invocation to success, 「不支持」 (not a git repo / generic
/// failure), or 「权限不足」 (probe denied or timed out).
fn classify(raw: GitRaw) -> Result<GitRaw, GitFailure> {
    if raw.success {
        return Ok(raw);
    }
    if raw.timed_out {
        return Err(GitFailure::Denied("timed-out"));
    }
    if raw.permission
        || raw.stderr.contains("permission denied")
        || raw.stderr.contains("operation not permitted")
    {
        return Err(GitFailure::Denied("permission-denied"));
    }
    if raw.stderr.contains("not a git repository") {
        return Err(GitFailure::Unsupported("not-a-git-repository"));
    }
    Err(GitFailure::Unsupported("git-error"))
}

/// Assert `args` (the argv after `git --no-optional-locks -C <root>`) is either
/// a fixed read-only command with no pathspec, or one of the pathspec templates
/// followed by safe literal paths. Caller strings can never become options.
fn validate_argv(args: &[&str]) -> Result<(), NodeError> {
    const EXACT: &[&[&str]] = &[
        &["rev-parse", "--show-toplevel"],
        &["rev-parse", "HEAD"],
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
    ];
    const PATHSP_TEMPLATES: &[&[&str]] = &[
        &["diff", "--no-color", "--no-ext-diff", "--"],
        &["diff", "--no-color", "--no-ext-diff", "--cached", "--"],
    ];
    if let Some(position) = args.iter().position(|arg| *arg == "--") {
        let head = &args[..=position];
        let paths = &args[position + 1..];
        if !PATHSP_TEMPLATES.contains(&head) {
            return Err(NodeError::InvalidRequest(format!(
                "git argv not on read-only whitelist: {}",
                head.join(" ")
            )));
        }
        if paths.is_empty() {
            return Err(NodeError::InvalidRequest(
                "git argv missing pathspec after --".into(),
            ));
        }
        for path in paths {
            if path.is_empty()
                || path.starts_with('-')
                || Path::new(path).is_absolute()
                || path.contains('\0')
                || path.len() > 4096
            {
                return Err(NodeError::InvalidRequest(format!(
                    "unsafe pathspec rejected: {path:?}"
                )));
            }
        }
        Ok(())
    } else if EXACT.contains(&args) {
        Ok(())
    } else {
        Err(NodeError::InvalidRequest(format!(
            "git argv not on read-only whitelist: {}",
            args.join(" ")
        )))
    }
}

/// Run git with a killable process group, a deadline, and a stdout byte cap.
/// Mirrors `workspace_access::bounded_workspace_command` but bounds output.
fn run_git(root: &Path, suffix: &[&str], stdout_cap: usize) -> Result<GitRaw, GitFailure> {
    validate_argv(suffix).map_err(|_| GitFailure::Denied("git-error"))?;
    let mut command = Command::new("git");
    command
        .current_dir("/")
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(suffix);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            GitFailure::Denied("permission-denied")
        } else {
            GitFailure::Denied("git-error")
        }
    })?;
    let (stdout_send, stdout_recv) = std::sync::mpsc::channel();
    let (stderr_send, stderr_recv) = std::sync::mpsc::channel();
    if let Some(mut pipe) = child.stdout.take() {
        std::thread::spawn(move || {
            // Store at most cap+1 bytes but keep draining so git never blocks
            // on a full pipe; bytes beyond the cap are discarded.
            let mut stored = Vec::new();
            let mut buf = [0u8; 16 * 1024];
            let mut total = 0usize;
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        total += n;
                        let room = stdout_cap.saturating_sub(stored.len());
                        if room > 0 {
                            stored.extend_from_slice(&buf[..n.min(room)]);
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = stdout_send.send((stored, total > stdout_cap));
        });
    }
    if let Some(mut pipe) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut data = Vec::new();
            let _ = pipe.read_to_end(&mut data);
            let _ = stderr_send.send(data);
        });
    }

    let deadline = Instant::now() + SCM_TIMEOUT;
    let mut timed_out = false;
    let status = loop {
        if let Ok(Some(status)) = child.try_wait() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            timed_out = true;
            #[cfg(unix)]
            if let Ok(pid) = i32::try_from(child.id()) {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            let _ = child.kill();
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let (stdout, stdout_capped) = stdout_recv.recv().unwrap_or((Vec::new(), false));
    let stderr_bytes = stderr_recv.recv().unwrap_or_default();
    let stderr = String::from_utf8_lossy(&stderr_bytes).to_lowercase();
    let permission =
        stderr.contains("permission denied") || stderr.contains("operation not permitted");
    Ok(GitRaw {
        success: status.is_some_and(|status| status.success()),
        stdout,
        stdout_capped,
        timed_out,
        permission,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn limits_match(value: &Value) {
        assert_eq!(value["maxEntries"], json!(MAX_ENTRIES));
        assert_eq!(value["maxDiffBytes"], json!(MAX_DIFF_BYTES));
        assert_eq!(value["maxFileBytes"], json!(MAX_FILE_BYTES));
    }

    #[test]
    fn whitelist_accepts_exact_templates_and_rejects_everything_else() {
        assert!(validate_argv(&["rev-parse", "--show-toplevel"]).is_ok());
        assert!(validate_argv(&["rev-parse", "HEAD"]).is_ok());
        assert!(validate_argv(&["symbolic-ref", "--quiet", "--short", "HEAD"]).is_ok());
        assert!(
            validate_argv(&["status", "--porcelain=v2", "-z", "--untracked-files=all"]).is_ok()
        );
        assert!(validate_argv(&["diff", "--no-color", "--no-ext-diff", "--", "src/a.rs"]).is_ok());
        assert!(
            validate_argv(&[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--cached",
                "--",
                "src/a.rs"
            ])
            .is_ok()
        );
        // Option injection after the separator is impossible; a leading dash is
        // still rejected as a pathspec.
        assert!(
            validate_argv(&[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--",
                "--output=/tmp/x"
            ])
            .is_err()
        );
        // Write-side / locking subcommands are not on the whitelist.
        assert!(validate_argv(&["status"]).is_err());
        assert!(validate_argv(&["diff"]).is_err());
        assert!(validate_argv(&["add", "--", "x"]).is_err());
        assert!(validate_argv(&["commit", "-m", "x"]).is_err());
        assert!(validate_argv(&["update-index", "--refresh"]).is_err());
        assert!(validate_argv(&["rev-parse", "--git-path", "index"]).is_err());
        assert!(validate_argv(&["status", "--porcelain=v2", "-z", "--"]).is_err());
    }

    #[test]
    fn status_diff_and_file_round_trip_on_a_temp_repo() {
        let root = temp_repo();
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        run_init_git(&root);
        fs::write(root.join("tracked.txt"), "base\nedited\n").unwrap();
        fs::write(root.join("untracked.txt"), "fresh bytes\n").unwrap();

        let id = WorkspaceId::new();
        let view = status(&id, &root);
        assert_eq!(view["availability"], "ok");
        assert!(view["headOid"].as_str().is_some_and(|oid| oid.len() == 40));
        limits_match(&view["limits"]);
        let entries = view["entries"].as_array().unwrap();
        let modified = entries.iter().find(|e| e["path"] == "tracked.txt").unwrap();
        assert_eq!(modified["xy"], json!(" M"));
        assert_eq!(modified["kind"], json!("modified"));
        assert_eq!(modified["sizeBytes"], json!(12));
        assert!(modified["oldOid"].is_string());
        assert_eq!(modified["digest"]["state"], "unknown");
        let untracked = entries
            .iter()
            .find(|e| e["path"] == "untracked.txt")
            .unwrap();
        assert_eq!(untracked["xy"], json!("??"));
        assert_eq!(untracked["kind"], json!("untracked"));

        let diffs = diff(
            &id,
            &root,
            &json!({"workspaceId": id.as_id().as_str(), "paths": ["tracked.txt"]}),
        )
        .unwrap();
        let item = &diffs["items"][0];
        assert!(item["patch"].as_str().unwrap().contains("+edited"));
        assert_eq!(item["binary"], false);
        assert_eq!(item["truncated"], false);
        assert_eq!(diffs["headOid"], view["headOid"]);

        let content = file(
            &id,
            &root,
            &json!({"workspaceId": id.as_id().as_str(), "path": "untracked.txt"}),
        )
        .unwrap();
        assert_eq!(content["content"], "fresh bytes\n");
        assert!(
            content["digest"]["value"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );
        assert_eq!(content["sizeBytes"], 12);
        assert_eq!(content["truncated"], false);
    }

    #[test]
    fn clean_repo_reports_no_entries_without_attribution_wording() {
        let root = temp_repo();
        fs::write(root.join("only.txt"), "x\n").unwrap();
        run_init_git(&root);
        let id = WorkspaceId::new();
        let view = status(&id, &root);
        assert_eq!(view["availability"], "ok");
        assert!(view["entries"].as_array().unwrap().is_empty());
        assert_eq!(view["truncated"]["entries"], false);
    }

    #[test]
    fn non_git_directory_is_structured_unsupported() {
        let root = tempfile::tempdir().unwrap();
        let id = WorkspaceId::new();
        let view = status(&id, root.path());
        assert_eq!(view["availability"], "unsupported");
        assert_eq!(view["unsupportedReason"], "not-a-git-repository");
        let diffs = diff(&id, root.path(), &json!({"paths": ["x"]})).unwrap();
        assert_eq!(diffs["availability"], "unsupported");
        let content = file(&id, root.path(), &json!({"path": "x"})).unwrap();
        assert_eq!(content["availability"], "unsupported");
    }

    #[test]
    fn binary_changed_file_is_entry_level_unsupported_and_not_inlined() {
        let root = temp_repo();
        fs::write(root.join("blob.dat"), b"AAAA\0BBBB\n").unwrap();
        run_init_git(&root);
        fs::write(root.join("blob.dat"), b"CCCC\0DDDD\nEEEE\n").unwrap();
        let id = WorkspaceId::new();
        let view = status(&id, &root);
        assert!(
            view["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["path"] == "blob.dat")
        );
        let diffs = diff(&id, &root, &json!({"paths": ["blob.dat"]})).unwrap();
        assert_eq!(diffs["items"][0]["binary"], true);
        assert!(diffs["items"][0]["patch"].is_null());

        fs::write(root.join("u.dat"), b"new\0binary\n").unwrap();
        let content = file(&id, &root, &json!({"path": "u.dat"})).unwrap();
        assert_eq!(content["binary"], true);
        assert_eq!(content["mediaType"], "application/octet-stream");
        assert!(content["content"].is_null());
    }

    #[test]
    fn rename_keeps_the_original_path() {
        let root = temp_repo();
        fs::write(root.join("old.txt"), "one\ntwo\n").unwrap();
        run_init_git(&root);
        use std::process::Command as StdCommand;
        let mv = StdCommand::new("git")
            .current_dir(&root)
            .args(["mv", "old.txt", "new.txt"])
            .status()
            .unwrap();
        assert!(mv.success());
        let id = WorkspaceId::new();
        let view = status(&id, &root);
        let entry = view["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["path"] == "new.txt")
            .expect("rename entry");
        assert_eq!(entry["origPath"], "old.txt");
        assert_eq!(entry["kind"], "renamed");
    }

    #[test]
    fn file_and_diff_cap_with_explicit_truncation_markers() {
        let root = temp_repo();
        run_init_git(&root);
        let big = "x".repeat(MAX_FILE_BYTES as usize + 10);
        fs::write(root.join("big.txt"), &big).unwrap();
        let id = WorkspaceId::new();
        let content = file(&id, &root, &json!({"path": "big.txt"})).unwrap();
        assert_eq!(content["truncated"], true);
        assert!(content["content"].is_null());
        assert_eq!(content["digest"]["state"], "unknown");
        assert_eq!(content["digest"]["reason"], "file-exceeds-inline-limit");
        assert_eq!(content["sizeBytes"], json!(big.len() as u64));

        // Tracked file replaced with a much larger body yields a big diff.
        fs::write(root.join("patch.txt"), "base\n").unwrap();
        let added = Command::new("git")
            .current_dir(&root)
            .args(["add", "patch.txt"])
            .status()
            .unwrap();
        assert!(added.success());
        let committed = Command::new("git")
            .current_dir(&root)
            .args(["commit", "-qm", "baseline"])
            .status()
            .unwrap();
        assert!(committed.success());
        let many = "y\n".repeat(MAX_DIFF_BYTES + 4096);
        fs::write(root.join("patch.txt"), many).unwrap();
        let diffs = diff(&id, &root, &json!({"paths": ["patch.txt"]})).unwrap();
        assert_eq!(diffs["truncated"]["diffBytes"], true);
        let item = &diffs["items"][0];
        assert_eq!(item["truncated"], true);
        assert!(item["patch"].as_str().unwrap().len() <= MAX_DIFF_BYTES);

        // Entry-count cap.
        let root2 = temp_repo();
        run_init_git(&root2);
        for index in 0..(MAX_ENTRIES + 3) {
            fs::write(root2.join(format!("f{index:05}.txt")), "z\n").unwrap();
        }
        let view = status(&id, &root2);
        assert_eq!(view["truncated"]["entries"], true);
        assert_eq!(view["entries"].as_array().unwrap().len(), MAX_ENTRIES);
    }

    #[test]
    fn content_change_is_detectable_via_head_and_digest() {
        let root = temp_repo();
        fs::write(root.join("u.txt"), "first\n").unwrap();
        run_init_git(&root);
        let id = WorkspaceId::new();
        let before = file(&id, &root, &json!({"path": "u.txt"})).unwrap();
        let head_before = before["headOid"].as_str().unwrap().to_owned();
        let size_before = before["sizeBytes"].clone();
        fs::write(root.join("u.txt"), "first\nsecond\n").unwrap();
        // A new commit moves HEAD; the file answer carries the new head and size.
        use std::process::Command as StdCommand;
        StdCommand::new("git")
            .current_dir(&root)
            .args(["add", "u.txt"])
            .status()
            .unwrap();
        StdCommand::new("git")
            .current_dir(&root)
            .args(["commit", "-qm", "second"])
            .status()
            .unwrap();
        let after = file(&id, &root, &json!({"path": "u.txt"})).unwrap();
        assert_ne!(after["headOid"], head_before);
        assert_ne!(after["sizeBytes"], size_before);
    }

    #[test]
    fn reads_never_take_the_index_lock_or_mutate_the_workspace() {
        let root = temp_repo();
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        run_init_git(&root);
        fs::write(root.join("tracked.txt"), "base\nedited\n").unwrap();
        fs::write(root.join("untracked.txt"), "fresh\n").unwrap();

        let index = root.join(".git").join("index");
        // Settle racy-git stat refresh with a normal status run before taking
        // the baseline, so a refresh caused by the test harness itself is not
        // mistaken for an SCM-RPC write.
        let before_status = Command::new("git")
            .current_dir(&root)
            .args(["status", "--porcelain"])
            .output()
            .unwrap()
            .stdout;
        let before_meta = fs::metadata(&index).unwrap();
        let before_bytes = fs::read(root.join("tracked.txt")).unwrap();

        let id = WorkspaceId::new();
        for _ in 0..5 {
            let view = status(&id, &root);
            assert_eq!(view["availability"], "ok");
            diff(
                &id,
                &root,
                &json!({"paths": ["tracked.txt"], "staged": false}),
            )
            .unwrap();
            diff(
                &id,
                &root,
                &json!({"paths": ["tracked.txt"], "staged": true}),
            )
            .unwrap();
            file(&id, &root, &json!({"path": "untracked.txt"})).unwrap();
        }

        let after_meta = fs::metadata(&index).unwrap();
        assert_eq!(
            (before_meta.len(), filetime_modified(&before_meta)),
            (after_meta.len(), filetime_modified(&after_meta)),
            "git index must not be rewritten by read-only RPCs"
        );
        assert!(
            !root.join(".git").join("index.lock").exists(),
            "no index.lock may be left behind"
        );
        let after_status = Command::new("git")
            .current_dir(&root)
            .args(["status", "--porcelain"])
            .output()
            .unwrap()
            .stdout;
        assert_eq!(before_status, after_status);
        assert_eq!(before_bytes, fs::read(root.join("tracked.txt")).unwrap());
    }

    #[test]
    fn contained_file_path_rejects_traversal_and_absolute_input() {
        // A parent + repo-root layout so the escaping target is a real sibling
        // inside the temp tree, not the shared temp directory.
        let parent = temp_repo();
        let root = parent.join("repo");
        fs::create_dir_all(&root).unwrap();
        run_init_git(&root);
        for bad in ["../outside", "/etc/passwd", "..", "", "a/../../b"] {
            let result = contained_path(&root, bad);
            assert!(result.is_err(), "path {bad:?} must be rejected");
        }
        #[cfg(unix)]
        {
            fs::write(parent.join("outside.txt"), "secret").unwrap();
            std::os::unix::fs::symlink(parent.join("outside.txt"), root.join("escape.txt"))
                .unwrap();
            assert!(contained_path(&root, "escape.txt").is_err());
            assert!(contained_path(&root, "escape.txt/../x").is_err());
        }
    }

    fn filetime_modified(meta: &std::fs::Metadata) -> std::time::SystemTime {
        meta.modified().unwrap()
    }

    /// Create an empty temp dir; tests that need git call [`run_init_git`].
    fn temp_repo() -> PathBuf {
        let dir = tempfile::tempdir().unwrap();
        // Keep the TempDir alive for the process: tests share a process and the
        // paths are unique; leaking avoids premature teardown across values.
        dir.keep()
    }

    /// Initialise a git repo with one commit on `main`, like the worktree tests.
    fn run_init_git(root: &Path) {
        macro_rules! git {
            ($($arg:expr),* $(,)?) => {{
                let output = Command::new("git")
                    .current_dir(root)
                    .args([$($arg),*])
                    .output()
                    .unwrap();
                assert!(output.status.success(), "git {} failed: {}",
                    [$($arg),*].join(" "),
                    String::from_utf8_lossy(&output.stderr));
            }};
        }
        git!("init", "-q");
        git!("symbolic-ref", "HEAD", "refs/heads/main");
        git!("config", "user.email", "test@example.com");
        git!("config", "user.name", "test");
        git!("add", ".");
        git!("commit", "--allow-empty", "-qm", "init");
    }
}
