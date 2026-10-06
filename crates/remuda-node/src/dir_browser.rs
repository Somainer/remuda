//! Human-only directory browser for picking a directory to register (c-dirpicker).
//!
//! Unlike [`crate::files`], whose `host.files.*` RPCs are anchored inside one
//! registered workspace, `host.dirs.list` browses the host filesystem so a
//! not-yet-registered directory can be found. The wider reach is bounded by
//! the same allowlist registration itself enforces: every listed path must
//! stay inside the Node's configured `workspace_roots` (default `$HOME`).
//!
//! Hard rules:
//! - Human-origin callers only; the Hub refuses Bot/Agent before proxying.
//! - Directories only. Every component is opened `O_NOFOLLOW` from the
//!   allowlist root fd, so a symlinked ancestor or component can never
//!   redirect the walk; entries are classified with no-follow `fstatat`
//!   (no descriptor is opened for an entry).
//! - Containment is lexical first (pure normalization, no filesystem
//!   access) and enforced on *opened file descriptors*: the root is pinned
//!   by (dev, ino) at policy init and re-walked component-by-component from
//!   `/` per request, so a root parent swapped for a symlink or another
//!   real directory after startup is refused.
//! - Outside, missing, not-a-directory and symlink-escape all return one
//!   path-free error, giving no existence/type oracle.
//! - One reply is capped in shortcut count, entry count and scan effort;
//!   overflow is reported as `truncated`, never paginated into an
//!   unbounded walk.

use crate::{DevNode, NodeError};
use remuda_protocol::hubnode::{
    HostDirEntry, HostDirsListParams, HostDirsListResult, METHOD_HOST_DIRS_LIST,
};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

/// Maximum directories returned in one reply.
pub(crate) const HOST_DIRS_MAX_ENTRIES: usize = 4096;
/// Hard stop on directory entries inspected for one reply, so a directory
/// with tens of thousands of entries cannot turn the RPC into a full scan.
pub(crate) const HOST_DIRS_MAX_SCANNED: usize = 16_384;
/// Cap on the shortcut arrays (allowed roots, registered roots).
pub(crate) const HOST_DIRS_MAX_SHORTCUTS: usize = 64;

/// One path-free refusal for every unreadable/out-of-policy browse target,
/// so the endpoint cannot act as an existence or type oracle.
pub(crate) const DIR_NOT_ALLOWED: &str = "the browsed path is outside the directories this Node allows workspaces in, \
     or is not an accessible directory";

/// `host.dirs.list` is the only method handled here.
pub(crate) fn is_host_dirs_method(method: &str) -> bool {
    method == METHOD_HOST_DIRS_LIST
}

fn refused() -> NodeError {
    NodeError::InvalidRequest(DIR_NOT_ALLOWED.to_owned())
}

/// A configured allowlist root: canonical path plus the (dev, ino) identity
/// pinned when the workspace policy was loaded. The identity lets a
/// request-time walk detect an ancestor that became a symlink or a different
/// real directory after Node startup (c-dirpicker round 3). Round 4 item 1:
/// on unix the identity is mandatory — policy load fails for a root whose
/// metadata cannot be read, instead of silently disabling pinning.
#[derive(Clone)]
pub(crate) struct AllowedRoot {
    /// Canonical path of the root.
    pub(crate) path: PathBuf,
    /// Pinned inode identity on unix.
    #[cfg(unix)]
    pub(crate) identity: RootIdentity,
}

/// Pinned root identity for an [`AllowedRoot`]. Both fields are normalized
/// to `u64` so comparison is portable across the unix targets this crate
/// builds on: Linux `dev_t`/`ino_t` are 64-bit, but macOS `dev_t` is `i32`
/// (its `ino_t` is `u64`), so a direct `st_dev == u64` fails to compile
/// there.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RootIdentity {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
}

impl AllowedRoot {
    #[cfg(not(unix))]
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Construct from a canonical path that has just been canonicalized by
    /// the caller. On unix the identity is required: a root whose metadata
    /// cannot be stat'd is rejected rather than admitted unpinned.
    #[cfg(unix)]
    pub(crate) fn new(path: PathBuf) -> Result<Self, NodeError> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(&path).map_err(|error| {
            NodeError::InvalidConfig(format!(
                "allowed workspace root {} is not accessible: {error}",
                path.display()
            ))
        })?;
        // Cast through u64 (no-op on Linux's 64-bit dev_t, required for
        // Darwin's i32 dev_t).
        #[allow(clippy::unnecessary_cast)]
        let identity = RootIdentity {
            dev: MetadataExt::dev(&metadata) as u64,
            ino: MetadataExt::ino(&metadata) as u64,
        };
        Ok(Self { path, identity })
    }
}

/// Lexically normalize an absolute path: collapse empty and `.` segments and
/// apply `..` lexically (clamped at `/`), with no filesystem access. Returns
/// None for a relative path. Because every `..` is consumed before any open
/// syscall, normalization cannot touch a directory outside the allowlist.
fn lexical_normalize(raw: &str) -> Option<PathBuf> {
    if !raw.starts_with('/') {
        return None;
    }
    let mut stack: Vec<&str> = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            name => stack.push(name),
        }
    }
    if stack.is_empty() {
        Some(PathBuf::from("/"))
    } else {
        let mut path = String::from('/');
        path.push_str(&stack.join("/"));
        Some(PathBuf::from(path))
    }
}

/// Whether `normalized` is the root or a component-wise descendant of it.
/// `/` as a root contains every absolute path; `/foo` never matches
/// `/foobar`.
fn is_within(normalized: &Path, root: &Path) -> bool {
    normalized == root || root == Path::new("/") || normalized.starts_with(root.join(""))
}

/// Sort, dedupe and cap a shortcut array, flagging truncation.
fn cap_shortcuts(items: Vec<String>, truncated: &mut bool) -> Vec<String> {
    let mut items = items;
    items.sort();
    items.dedup();
    if items.len() > HOST_DIRS_MAX_SHORTCUTS {
        items.truncate(HOST_DIRS_MAX_SHORTCUTS);
        *truncated = true;
    }
    items
}

impl DevNode {
    /// Dispatch a `host.dirs.*` RPC onto the local filesystem.
    pub(crate) async fn host_dirs_rpc(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, NodeError> {
        if method != METHOD_HOST_DIRS_LIST {
            return Err(NodeError::InvalidRequest(format!(
                "unknown host directories method {method}"
            )));
        }
        let request: HostDirsListParams = serde_json::from_value(params)?;
        let roots = self.allowed_workspace_roots()?;
        let registered: Vec<PathBuf> = self
            .workspaces()?
            .iter()
            .map(|workspace| PathBuf::from(&workspace.root_path))
            .collect();
        let result =
            tokio::task::spawn_blocking(move || list_directories(&roots, &registered, request))
                .await
                .map_err(|error| NodeError::Driver(format!("host dirs task failed: {error}")))??;
        Ok(serde_json::to_value(result)?)
    }

    /// The pinned allowlist roots workspace registration is confined to.
    pub(crate) fn allowed_workspace_roots(&self) -> Result<Vec<AllowedRoot>, NodeError> {
        Ok(self
            .inner
            .workspace_registry
            .read()
            .map_err(|_| NodeError::StorePoisoned)?
            .allowed_roots()
            .to_vec())
    }
}

// ── unix: fd-rooted, no-follow component walk with pinned root identity ────

#[cfg(unix)]
mod imp {
    use super::*;
    use nix::fcntl::{AtFlags, OFlag, openat};
    use nix::sys::stat::{SFlag, fstat, fstatat};
    use std::ffi::CString;
    use std::os::fd::RawFd;

    /// Owned raw fd closed on drop. The crate forbids `unsafe`, so this takes
    /// the place of `std::os::fd::OwnedFd`: every value comes from a nix call
    /// returning a fresh descriptor, and Drop closes exactly that one.
    struct Fd(RawFd);

    impl Drop for Fd {
        fn drop(&mut self) {
            let _ = nix::unistd::close(self.0);
        }
    }

    /// The directory opened for one request: fd plus its kernel-resolved path.
    struct OpenedDir {
        fd: Fd,
        path: String,
    }

    pub(super) fn list(
        roots: &[AllowedRoot],
        registered_roots: &[PathBuf],
        request: HostDirsListParams,
    ) -> Result<HostDirsListResult, NodeError> {
        let mut truncated = false;
        // Pinned root fds, opened per request by a no-follow walk from "/".
        let root_fds = open_roots(roots)?;
        let home_canonical = std::env::var_os("HOME")
            .map(PathBuf::from)
            .and_then(|home| std::fs::canonicalize(&home).ok());
        let target = match request.path.as_deref().filter(|value| !value.is_empty()) {
            None => default_start(&root_fds, home_canonical.clone())?,
            Some(raw) => open_requested(&root_fds, raw)?,
        };
        let parent = contained_parent(&root_fds, &target.path)?;

        let mut names = Vec::new();
        let mut scanned = 0usize;
        // Enumerate from a dup of the verified fd: nix::Dir takes ownership of
        // the fd it fdopendir()s; the verified `target` fd stays open for
        // fstatat classification and parent resolution.
        let dir_fd = nix::unistd::dup(target.fd.0).map_err(nix_err)?;
        let mut directory = nix::dir::Dir::from_fd(dir_fd).map_err(nix_err)?;
        for entry in directory.iter() {
            let Ok(entry) = entry else { continue };
            scanned += 1;
            if scanned > HOST_DIRS_MAX_SCANNED {
                truncated = true;
                break;
            }
            let file_name = entry.file_name();
            let bytes = file_name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            // fstatat with AT_SYMLINK_NOFOLLOW classifies the entry without
            // opening it: no descriptor is created (so no leak), a symlink
            // never becomes a row, and a non-directory is skipped.
            if !entry_is_dir(target.fd.0, file_name) {
                continue;
            }
            let name = String::from_utf8_lossy(bytes).into_owned();
            if !request.show_hidden && name.starts_with('.') {
                continue;
            }
            names.push(name);
        }
        drop(directory);
        names.sort();
        if names.len() > HOST_DIRS_MAX_ENTRIES {
            names.truncate(HOST_DIRS_MAX_ENTRIES);
            truncated = true;
        }

        let roots_view = cap_shortcuts(
            roots
                .iter()
                .map(|root| root.path.display().to_string())
                .collect(),
            &mut truncated,
        );
        let mut workspaces = cap_shortcuts(
            registered_roots
                .iter()
                .filter(|root| roots.iter().any(|allowed| is_within(root, &allowed.path)))
                .map(|root| root.display().to_string())
                .collect(),
            &mut truncated,
        );
        workspaces.retain(|root| !root.is_empty());
        let home = home_canonical
            .map(|home| home.display().to_string())
            .filter(|home| {
                root_fds
                    .iter()
                    .any(|root| is_within(Path::new(home), &root.pin.path))
            });

        Ok(HostDirsListResult {
            path: target.path,
            parent,
            home,
            roots: roots_view,
            workspaces,
            dirs: names
                .into_iter()
                .map(|name| HostDirEntry { name })
                .collect(),
            truncated,
        })
    }

    /// An [`AllowedRoot`] plus its per-request verified fd.
    struct RootFd {
        pin: AllowedRoot,
        fd: Fd,
    }

    /// Open every pinned root by walking from `/` without following symlinks
    /// and verifying the pinned (dev, ino). Any root whose ancestry was
    /// replaced after policy init makes the whole listing fail closed.
    fn open_roots(pins: &[AllowedRoot]) -> Result<Vec<RootFd>, NodeError> {
        pins.iter().map(open_root).collect()
    }

    fn open_root(pin: &AllowedRoot) -> Result<RootFd, NodeError> {
        let mut anchor = open_component(None, c"/")?;
        for component in pin.path.components() {
            if let Component::Normal(name) = component {
                let cstr = CString::new(name.as_encoded_bytes()).map_err(|_| refused())?;
                test_seam_fire(&pin.path, 0);
                anchor = open_component(Some(anchor.0), &cstr)?;
            }
        }
        // The pin was built from metadata at policy load; verify the opened
        // root fd still has exactly that identity (portable u64 compare).
        let stat = fstat(anchor.0).map_err(nix_err)?;
        #[allow(clippy::unnecessary_cast)]
        let stat_identity = RootIdentity {
            dev: stat.st_dev as u64,
            ino: stat.st_ino as u64,
        };
        if pin.identity != stat_identity {
            return Err(refused());
        }
        Ok(RootFd {
            pin: pin.clone(),
            fd: anchor,
        })
    }

    fn open_component(dirfd: Option<RawFd>, name: &std::ffi::CStr) -> Result<Fd, NodeError> {
        // O_RDONLY (no O_PATH: that flag is Linux-specific) is enough for a
        // directory anchor and for readdir on every unix target; O_NOFOLLOW
        // refuses a symlink at the named component.
        match openat(
            dirfd,
            name,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        ) {
            Ok(raw) => Ok(Fd(raw)),
            Err(_) => Err(refused()),
        }
    }

    /// Lexically normalize and verify containment before touching the
    /// filesystem, then walk each remaining component from the pinned root
    /// fd with O_NOFOLLOW. Every failure is the same path-free refusal.
    fn open_requested(root_fds: &[RootFd], raw: &str) -> Result<OpenedDir, NodeError> {
        let Some(normalized) = lexical_normalize(raw) else {
            return Err(NodeError::InvalidRequest(
                "browsed path must be absolute on the Node filesystem".to_owned(),
            ));
        };
        let root = root_fds
            .iter()
            .filter(|root| is_within(&normalized, &root.pin.path))
            .max_by_key(|root| root.pin.path.as_os_str().len())
            .ok_or_else(refused)?;

        let mut anchor = nix::unistd::dup(root.fd.0).map(Fd).map_err(nix_err)?;
        let relative = normalized
            .strip_prefix(&root.pin.path)
            .unwrap_or(&normalized);
        for (index, component) in relative.components().enumerate() {
            if let Component::Normal(name) = component {
                let cstr = CString::new(name.as_encoded_bytes()).map_err(|_| refused())?;
                test_seam_fire(&normalized, index + 1);
                anchor = open_component(Some(anchor.0), &cstr)?;
            }
        }
        // The target path is the normalized request path. The walk started at
        // a canonical pinned root and opened every component O_NOFOLLOW, so
        // this string is the fd's real canonical path — no fd→path syscall
        // (which would need /proc on Linux or F_GETPATH on macOS) is required,
        // keeping the crate free of unsafe fcntl.
        Ok(OpenedDir {
            fd: anchor,
            path: normalized.display().to_string(),
        })
    }

    fn default_start(
        root_fds: &[RootFd],
        home_canonical: Option<PathBuf>,
    ) -> Result<OpenedDir, NodeError> {
        if let Some(home) = home_canonical
            .map(|home| home.display().to_string())
            .filter(|home| {
                root_fds
                    .iter()
                    .any(|root| is_within(Path::new(home), &root.pin.path))
            })
            && let Ok(opened) = open_requested(root_fds, &home)
        {
            return Ok(opened);
        }
        let root = root_fds.first().ok_or_else(|| {
            NodeError::InvalidRequest(
                "no allowed directory is accessible on this Node; configure workspace_roots"
                    .to_owned(),
            )
        })?;
        Ok(OpenedDir {
            fd: nix::unistd::dup(root.fd.0).map(Fd).map_err(nix_err)?,
            path: root.pin.path.display().to_string(),
        })
    }

    /// The contained parent path, or None at the pinned root. Resolved
    /// through the same no-follow walk, so an unreadable parent yields no
    /// shortcut.
    fn contained_parent(
        root_fds: &[RootFd],
        target_path: &str,
    ) -> Result<Option<String>, NodeError> {
        let normalized = lexical_normalize(target_path).ok_or_else(refused)?;
        let root = root_fds
            .iter()
            .filter(|root| is_within(&normalized, &root.pin.path))
            .max_by_key(|root| root.pin.path.as_os_str().len())
            .ok_or_else(refused)?;
        if normalized == root.pin.path {
            return Ok(None);
        }
        let Some(parent) = normalized.parent() else {
            return Ok(None);
        };
        if !is_within(parent, &root.pin.path) {
            return Ok(None);
        }
        Ok(Some(parent.display().to_string()))
    }

    fn entry_is_dir(dir: RawFd, name: &std::ffi::CStr) -> bool {
        match fstatat(Some(dir), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => (stat.st_mode & SFlag::S_IFMT.bits()) == SFlag::S_IFDIR.bits(),
            Err(_) => false,
        }
    }

    fn nix_err(error: nix::Error) -> NodeError {
        NodeError::Driver(error.to_string())
    }

    /// Test-only seam: fired immediately before a no-follow component open,
    /// with the normalized absolute directory being entered and its
    /// component index (0 = first root component). Lets a regression test
    /// swap a verified directory for a symlink *after* the lexical check but
    /// *before* its open, exercising the TOCTOU defence deterministically.
    #[cfg(test)]
    fn test_seam_fire(_normalized: &Path, _component: usize) {
        super::test_seam::fire(_normalized, _component);
    }

    #[cfg(not(test))]
    fn test_seam_fire(_normalized: &Path, _component: usize) {}
}

// ── Test seam for mid-walk directory swaps ──────────────────────────────────

#[cfg(test)]
pub(crate) mod test_seam {
    use std::path::Path;
    use std::sync::{Mutex, OnceLock};

    type SeamHook = Box<dyn Fn(&Path, usize) + Send + Sync>;

    static HOOK: OnceLock<Mutex<Option<SeamHook>>> = OnceLock::new();

    fn slot() -> &'static Mutex<Option<SeamHook>> {
        HOOK.get_or_init(|| Mutex::new(None))
    }

    /// Install a hook fired before each component open during one test.
    pub(crate) fn set<F>(hook: F)
    where
        F: Fn(&Path, usize) + Send + Sync + 'static,
    {
        *slot().lock().unwrap() = Some(Box::new(hook));
    }

    pub(crate) fn clear() {
        *slot().lock().unwrap() = None;
    }

    pub(crate) fn fire(normalized: &Path, component: usize) {
        if let Some(hook) = slot().lock().unwrap().as_ref() {
            hook(normalized, component);
        }
    }
}

// ── non-unix fallback: pathname implementation (deployment is unix-only) ────

#[cfg(not(unix))]
mod imp {
    use super::*;

    pub(super) fn list(
        roots: &[AllowedRoot],
        registered_roots: &[PathBuf],
        request: HostDirsListParams,
    ) -> Result<HostDirsListResult, NodeError> {
        let raw = request.path.as_deref().filter(|value| !value.is_empty());
        let target = match raw {
            None => std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|home| {
                    std::fs::canonicalize(home).is_ok_and(|canonical| {
                        roots.iter().any(|root| canonical.starts_with(&root.path))
                    })
                })
                .or_else(|| roots.first().map(|root| root.path.clone()))
                .ok_or_else(|| {
                    NodeError::InvalidRequest(
                        "no allowed directory is accessible on this Node; configure workspace_roots"
                            .to_owned(),
                    )
                })?,
            Some(raw) => {
                let normalized = lexical_normalize(raw).ok_or_else(|| {
                    NodeError::InvalidRequest(
                        "browsed path must be absolute on the Node filesystem".to_owned(),
                    )
                })?;
                let canonical = std::fs::canonicalize(&normalized).map_err(|_| refused())?;
                if !roots.iter().any(|root| is_within(&canonical, &root.path)) {
                    return Err(refused());
                }
                canonical
            }
        };
        let mut names = Vec::new();
        let mut scanned = 0usize;
        let mut truncated = false;
        for entry in std::fs::read_dir(&target).map_err(|_| refused())? {
            let Ok(entry) = entry else { continue };
            scanned += 1;
            if scanned > HOST_DIRS_MAX_SCANNED {
                truncated = true;
                break;
            }
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str() else {
                continue;
            };
            if !request.show_hidden && name.starts_with('.') {
                continue;
            }
            names.push(name.to_owned());
        }
        names.sort();
        if names.len() > HOST_DIRS_MAX_ENTRIES {
            names.truncate(HOST_DIRS_MAX_ENTRIES);
            truncated = true;
        }
        let roots_view = cap_shortcuts(
            roots
                .iter()
                .map(|root| root.path.display().to_string())
                .collect(),
            &mut truncated,
        );
        let workspaces = cap_shortcuts(
            registered_roots
                .iter()
                .filter(|root| roots.iter().any(|allowed| is_within(root, &allowed.path)))
                .map(|root| root.display().to_string())
                .collect(),
            &mut truncated,
        );
        Ok(HostDirsListResult {
            path: target.display().to_string(),
            parent: target.parent().map(|parent| parent.display().to_string()),
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .and_then(|home| std::fs::canonicalize(home).ok())
                .map(|path| path.display().to_string()),
            roots: roots_view,
            workspaces,
            dirs: names
                .into_iter()
                .map(|name| HostDirEntry { name })
                .collect(),
            truncated,
        })
    }
}

#[cfg(unix)]
fn list_directories(
    roots: &[AllowedRoot],
    registered_roots: &[PathBuf],
    request: HostDirsListParams,
) -> Result<HostDirsListResult, NodeError> {
    imp::list(roots, registered_roots, request)
}

#[cfg(not(unix))]
fn list_directories(
    roots: &[AllowedRoot],
    registered_roots: &[PathBuf],
    request: HostDirsListParams,
) -> Result<HostDirsListResult, NodeError> {
    imp::list(roots, registered_roots, request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn request(path: Option<&str>, show_hidden: bool) -> HostDirsListParams {
        HostDirsListParams {
            path: path.map(str::to_owned),
            show_hidden,
        }
    }

    fn roots_for(dir: &tempfile::TempDir) -> Vec<AllowedRoot> {
        vec![AllowedRoot::new(fs::canonicalize(dir.path()).unwrap()).unwrap()]
    }

    #[test]
    fn lists_real_subdirectories_within_the_allowlist_and_hides_dotdirs() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("beta")).unwrap();
        fs::create_dir_all(root.path().join("alpha/sub")).unwrap();
        fs::write(root.path().join("file.txt"), b"x").unwrap();
        fs::create_dir_all(root.path().join(".secret")).unwrap();
        let roots = roots_for(&root);

        let result = list_directories(&roots, &[], request(None, false)).unwrap();
        let names: Vec<&str> = result
            .dirs
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert!(!result.truncated);
        assert_eq!(result.path, roots[0].path.display().to_string());
        assert_eq!(result.parent, None);
        assert_eq!(result.roots.len(), 1);

        // Descend: parent is now the allowlisted root.
        let child = list_directories(
            &roots,
            &[],
            request(
                Some(&roots[0].path.join("alpha").display().to_string()),
                false,
            ),
        )
        .unwrap();
        assert_eq!(
            child
                .dirs
                .iter()
                .map(|e| e.name.clone())
                .collect::<Vec<_>>(),
            vec!["sub".to_string()]
        );
        assert_eq!(child.parent, Some(roots[0].path.display().to_string()));

        // Hidden directories appear only when explicitly requested.
        let shown = list_directories(&roots, &[], request(None, true)).unwrap();
        assert!(shown.dirs.iter().any(|entry| entry.name == ".secret"));
    }

    #[test]
    fn lexical_normalization_collapses_dots_without_filesystem_access() {
        assert_eq!(
            lexical_normalize("/a/./b//c").as_deref(),
            Some(Path::new("/a/b/c"))
        );
        assert_eq!(lexical_normalize("/..").as_deref(), Some(Path::new("/")));
        assert_eq!(
            lexical_normalize("/a/../../b").as_deref(),
            Some(Path::new("/b"))
        );
        assert_eq!(lexical_normalize("a/b"), None);
        assert!(is_within(Path::new("/foo"), Path::new("/foo")));
        assert!(is_within(Path::new("/foo/bar"), Path::new("/foo")));
        assert!(is_within(Path::new("/foobar"), Path::new("/")));
        assert!(!is_within(Path::new("/foobar"), Path::new("/foo")));
    }

    #[cfg(unix)]
    #[test]
    fn four_unreadable_cases_return_one_byte_identical_path_free_error() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(root.path().join("file"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let roots = roots_for(&root);

        // Relative: a distinct malformed-request error.
        let rel = list_directories(&roots, &[], request(Some("../etc"), false))
            .unwrap_err()
            .to_string();
        assert!(rel.contains("must be absolute"), "{rel}");

        // Outside (lexical containment, no openat attempted).
        let outside_err = list_directories(
            &roots,
            &[],
            request(Some(outside.path().to_str().unwrap()), false),
        )
        .unwrap_err()
        .to_string();
        // Missing component.
        let missing_err = list_directories(
            &roots,
            &[],
            request(Some(&root.path().join("nope").display().to_string()), false),
        )
        .unwrap_err()
        .to_string();
        // Existing non-directory.
        let file_err = list_directories(
            &roots,
            &[],
            request(Some(&root.path().join("file").display().to_string()), false),
        )
        .unwrap_err()
        .to_string();
        // Symlink escape.
        let link_err = list_directories(
            &roots,
            &[],
            request(Some(&root.path().join("link").display().to_string()), false),
        )
        .unwrap_err()
        .to_string();

        assert_eq!(outside_err, missing_err);
        assert_eq!(outside_err, file_err);
        assert_eq!(outside_err, link_err);
        // All four are the same path-free refusal (the NodeError display
        // prefix is constant), byte-for-byte.
        for error in [&outside_err, &missing_err, &file_err, &link_err] {
            assert!(error.contains(DIR_NOT_ALLOWED), "{error}");
            assert!(!error.contains("nope"));
            assert!(!error.contains("link"));
            assert!(!error.contains("file"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_swapped_after_the_lexical_check_is_refused_at_open() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("a/inside")).unwrap();
        fs::create_dir_all(outside.path().join("stolen")).unwrap();
        let canonical = fs::canonicalize(root.path()).unwrap();
        let target = canonical.join("a");
        let roots = vec![AllowedRoot::new(canonical.clone()).unwrap()];

        // Swap the verified directory for a symlink AFTER the lexical
        // containment check but BEFORE its component open (test seam).
        let target_for_hook = target.clone();
        let live_for_hook = root.path().join("a");
        let backup_for_hook = root.path().join("a-real");
        let outside_for_hook = outside.path().to_path_buf();
        test_seam::set(move |normalized, _component| {
            if normalized == target_for_hook && live_for_hook.is_dir() {
                fs::rename(&live_for_hook, &backup_for_hook).unwrap();
                std::os::unix::fs::symlink(&outside_for_hook, &live_for_hook).unwrap();
            }
        });
        let error = list_directories(
            &roots,
            &[],
            request(Some(&target.display().to_string()), false),
        )
        .unwrap_err()
        .to_string();
        test_seam::clear();
        assert!(error.contains(DIR_NOT_ALLOWED), "{error}");
        // Restore the directory so the root opens again; then prove the
        // swapped-in outside tree was never listed.
        let live = root.path().join("a");
        fs::remove_file(&live).unwrap();
        fs::rename(root.path().join("a-real"), &live).unwrap();
        let result = list_directories(&roots, &[], request(None, false)).unwrap();
        assert!(
            !result.dirs.iter().any(|entry| entry.name == "stolen"),
            "outside tree must never be listed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_root_parent_replaced_by_a_symlink_after_startup_is_refused() {
        let base = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(base.path().join("parent/root/child")).unwrap();
        fs::create_dir_all(outside.path().join("elsewhere")).unwrap();
        let canonical = fs::canonicalize(base.path().join("parent/root")).unwrap();
        // Pin the identity at policy load.
        let roots = vec![AllowedRoot::new(canonical.clone()).unwrap()];
        // Baseline listing works.
        assert!(list_directories(&roots, &[], request(None, false)).is_ok());
        // Replace the root's PARENT with a symlink to an outside tree.
        let live_parent = base.path().join("parent");
        let backup = base.path().join("parent-real");
        fs::rename(&live_parent, &backup).unwrap();
        std::os::unix::fs::symlink(outside.path(), &live_parent).unwrap();
        // Rebuild the same outside tree shape so the bare path would resolve
        // if the walk naively followed the ancestor; the pinned identity must
        // refuse it.
        fs::create_dir_all(outside.path().join("root")).unwrap();
        let error = list_directories(&roots, &[], request(None, false))
            .unwrap_err()
            .to_string();
        assert!(error.contains(DIR_NOT_ALLOWED), "{error}");
    }

    // Linux-only: it enumerates /proc/self/fd symlinks to count fds opened
    // under the test tree. Darwin's /dev/fd entries are character devices,
    // not symlinks; the no-fd-opened classification itself is platform-neutral
    // and is exercised on macOS by all the other dir_browser tests.
    #[cfg(target_os = "linux")]
    #[test]
    fn enumeration_does_not_leak_file_descriptors_under_repeated_load() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        // Count only descriptors whose opened target lives inside THIS temp
        // tree, so concurrent tests in the same binary (sockets, other
        // tempdirs) cannot perturb the measurement. Every listing opens the
        // root and its walk components; the root is the only target under
        // this tree, and fstatat classification opens nothing.
        let root = tempfile::tempdir().unwrap();
        for index in 0..40 {
            fs::create_dir_all(root.path().join(format!("sub-{index:02}"))).unwrap();
        }
        let roots = roots_for(&root);
        let canonical_root = fs::canonicalize(root.path()).unwrap();
        // Linux exposes the open target of each descriptor as a
        // /proc/self/fd/N symlink.
        let fd_dir = "/proc/self/fd";
        let fds_under_root = || -> usize {
            fs::read_dir(fd_dir)
                .unwrap()
                .filter_map(Result::ok)
                .filter_map(|entry| fs::read_link(entry.path()).ok())
                .filter(|target| target.starts_with(&canonical_root))
                .count()
        };

        // Quiescent baseline after a warm listing: no descriptor survives.
        list_directories(&roots, &[], request(None, false)).unwrap();
        assert_eq!(
            fds_under_root(),
            0,
            "a completed listing left a descriptor open"
        );

        let burst = |millis: u64| {
            let stop = Arc::new(AtomicBool::new(false));
            let mut handles = Vec::new();
            for _ in 0..4 {
                let roots = roots.clone();
                let stop = stop.clone();
                handles.push(std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let result = list_directories(&roots, &[], request(None, false)).unwrap();
                        assert_eq!(result.dirs.len(), 40);
                    }
                }));
            }
            std::thread::sleep(Duration::from_millis(millis));
            stop.store(true, Ordering::Relaxed);
            for handle in handles {
                handle.join().unwrap();
            }
        };
        // Two bursts: after every worker returns an fstatat-based walk must
        // have leaked zero descriptors, with no accumulation across runs.
        burst(800);
        assert_eq!(fds_under_root(), 0, "fds leaked after the first burst");
        burst(800);
        assert_eq!(fds_under_root(), 0, "fds accumulated across bursts");
    }

    #[test]
    fn caps_the_result_and_reports_truncation() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..(HOST_DIRS_MAX_ENTRIES + 25) {
            fs::create_dir_all(root.path().join(format!("d-{index:06}"))).unwrap();
        }
        let roots = roots_for(&root);
        let result = list_directories(&roots, &[], request(None, false)).unwrap();
        assert_eq!(result.dirs.len(), HOST_DIRS_MAX_ENTRIES);
        assert!(result.truncated);
        assert_eq!(result.dirs[0].name, "d-000000");
    }

    #[test]
    fn the_scan_bound_counts_every_entry_not_just_directories() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..(HOST_DIRS_MAX_SCANNED + 8) {
            fs::write(root.path().join(format!("f-{index:06}")), b"x").unwrap();
        }
        fs::create_dir_all(root.path().join("the-only-dir")).unwrap();
        let roots = roots_for(&root);
        let result = list_directories(&roots, &[], request(None, false)).unwrap();
        assert!(result.truncated);
        assert!(result.dirs.len() <= 1);
        assert!(
            result
                .dirs
                .iter()
                .all(|entry| !entry.name.starts_with("f-"))
        );
    }

    #[test]
    fn shortcut_arrays_are_capped_and_flag_truncated() {
        let base = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for index in 0..(HOST_DIRS_MAX_SHORTCUTS + 5) {
            let dir = base.path().join(format!("w-{index:03}"));
            fs::create_dir_all(&dir).unwrap();
            paths.push(fs::canonicalize(&dir).unwrap());
        }
        let root = vec![AllowedRoot::new(fs::canonicalize(base.path()).unwrap()).unwrap()];
        let result = list_directories(&root, &paths, request(None, false)).unwrap();
        assert_eq!(result.workspaces.len(), HOST_DIRS_MAX_SHORTCUTS);
        assert!(result.truncated);
    }

    #[test]
    fn registered_roots_are_reported_for_quick_jump() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("one")).unwrap();
        let canonical_root = fs::canonicalize(root.path()).unwrap();
        let registered = vec![canonical_root.join("one")];
        let roots = vec![AllowedRoot::new(canonical_root.clone()).unwrap()];
        let result = list_directories(&roots, &registered, request(None, false)).unwrap();
        assert_eq!(result.workspaces, vec![registered[0].display().to_string()]);
    }
}
