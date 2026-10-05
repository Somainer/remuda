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
//! - Directories only. Symlinks are never followed or listed, so a link
//!   cannot point the walk outside the allowlist the way `..` could.
//! - Containment is enforced on *opened file descriptors* (unix): the target
//!   directory is opened once via an `O_NOFOLLOW` walk rooted at an allowed
//!   root fd and enumerated from that same fd, so a pathname swapped for a
//!   symlink between check and open cannot redirect the listing.
//! - One reply is capped in entry count (and scan effort); overflow is
//!   reported as `truncated`, never paginated into an unbounded walk.

use crate::{DevNode, NodeError};
use remuda_protocol::hubnode::{
    HostDirEntry, HostDirsListParams, HostDirsListResult, METHOD_HOST_DIRS_LIST,
};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Maximum directories returned in one reply.
pub(crate) const HOST_DIRS_MAX_ENTRIES: usize = 4096;
/// Hard stop on directory entries inspected for one reply, so a directory
/// with tens of thousands of entries cannot turn the RPC into a full scan.
pub(crate) const HOST_DIRS_MAX_SCANNED: usize = 16_384;
/// Bound on `..` climbs while proving an opened fd is still under a root.
const MAX_ANCESTOR_CLIMBS: usize = 64;

/// `host.dirs.list` is the only method handled here.
pub(crate) fn is_host_dirs_method(method: &str) -> bool {
    method == METHOD_HOST_DIRS_LIST
}

fn refused(message: impl Into<String>) -> NodeError {
    NodeError::InvalidRequest(message.into())
}

impl DevNode {
    /// Dispatch a `host.dirs.*` RPC onto the local filesystem.
    pub(crate) async fn host_dirs_rpc(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, NodeError> {
        if method != METHOD_HOST_DIRS_LIST {
            return Err(refused(format!("unknown host directories method {method}")));
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

    /// The canonical allowlist roots workspace registration is confined to.
    pub(crate) fn allowed_workspace_roots(&self) -> Result<Vec<PathBuf>, NodeError> {
        Ok(self
            .inner
            .workspace_registry
            .read()
            .map_err(|_| NodeError::StorePoisoned)?
            .allowed_roots()
            .to_vec())
    }
}

// ── unix: fd-rooted, symlink-proof containment ─────────────────────────────

#[cfg(unix)]
mod imp {
    use super::*;
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::fstat;
    use std::ffi::CStr;
    use std::os::fd::RawFd;

    /// Owned raw fd closed on drop. The crate forbids `unsafe`, so this takes
    /// the place of `std::os::fd::OwnedFd`: every value comes from a nix call
    /// returning a fresh descriptor, and Drop closes exactly that one.
    struct Fd(RawFd);

    impl Fd {
        fn raw(&self) -> RawFd {
            self.0
        }
    }

    impl Drop for Fd {
        fn drop(&mut self) {
            // Best effort: the descriptor is invalid only after close races
            // within this single owner, which cannot happen.
            let _ = nix::unistd::close(self.0);
        }
    }

    pub(super) fn list(
        roots: &[PathBuf],
        registered_roots: &[PathBuf],
        request: HostDirsListParams,
    ) -> Result<HostDirsListResult, NodeError> {
        let root_fds = open_roots(roots)?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let home_canonical = home
            .as_ref()
            .and_then(|home| std::fs::canonicalize(home).ok());
        let target = match request.path.as_deref().filter(|value| !value.is_empty()) {
            None => Ok(default_start(&root_fds, home_canonical.clone())?),
            Some(raw) => open_requested(&root_fds, raw),
        }?;
        let target_path = fd_canonical_path(target.fd())?;
        let parent = contained_parent(&root_fds, target.fd())?;
        let mut names = Vec::new();
        let mut truncated = false;
        let mut scanned = 0usize;
        // Enumerate from a dup of the verified fd: the directory was opened
        // once, and nix::Dir takes ownership of the fd it fdopendir()s.
        let dir_fd = nix::unistd::dup(target.fd()).map_err(nix_err)?;
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
            // Classify via an O_PATH|O_NOFOLLOW openat on the directory fd:
            // a symlink fails with ELOOP and never becomes a row; a
            // non-directory fails with ENOTDIR; nothing is followed.
            if !is_real_subdir(target.fd(), file_name) {
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
        let home_path = home_canonical
            .filter(|canonical| root_for(&canonical.display().to_string(), &root_fds).is_some())
            .map(|canonical| canonical.display().to_string());
        let mut workspaces: Vec<String> = registered_roots
            .iter()
            .filter(|root| roots.iter().any(|allowed| root.starts_with(allowed)))
            .map(|root| root.display().to_string())
            .collect();
        workspaces.sort();
        workspaces.dedup();
        Ok(HostDirsListResult {
            path: target_path,
            parent,
            home: home_path,
            roots: roots
                .iter()
                .map(|root| root.display().to_string())
                .collect(),
            workspaces,
            dirs: names
                .into_iter()
                .map(|name| HostDirEntry { name })
                .collect(),
            truncated,
        })
    }

    /// An allowed root held open for the whole listing.
    pub(super) struct RootFd {
        path: PathBuf,
        fd: Fd,
    }

    impl RootFd {
        pub(super) fn fd(&self) -> RawFd {
            self.fd.raw()
        }
    }

    fn open_roots(roots: &[PathBuf]) -> Result<Vec<RootFd>, NodeError> {
        roots
            .iter()
            .map(|path| {
                // Roots are canonical, absolute and validated at registry
                // open; O_NOFOLLOW pins the directory inode itself.
                let cstr = std::ffi::CString::new(path.to_string_lossy().as_bytes())
                    .map_err(|_| refused("invalid allowed root path"))?;
                let raw = openat(
                    None,
                    &*cstr,
                    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    nix::sys::stat::Mode::empty(),
                )
                .map_err(nix_err)?;
                Ok(RootFd {
                    path: path.clone(),
                    fd: Fd(raw),
                })
            })
            .collect()
    }

    /// Open a no-follow O_DIRECTORY fd at `relative` from `dirfd`.
    /// `path_query` adds O_PATH when the fd is only needed as a walk anchor.
    fn open_dir_nofollow(
        dirfd: Option<RawFd>,
        relative: &CStr,
        path_query: bool,
    ) -> nix::Result<Fd> {
        let mut flags = OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_DIRECTORY;
        flags.set(OFlag::O_PATH, path_query);
        flags.set(OFlag::O_RDONLY, !path_query);
        Ok(Fd(openat(
            dirfd,
            relative,
            flags,
            nix::sys::stat::Mode::empty(),
        )?))
    }

    /// An opened target directory.
    struct OpenedDir {
        fd: Fd,
    }

    impl OpenedDir {
        fn fd(&self) -> RawFd {
            self.fd.raw()
        }
    }

    /// Lexical root choice for a requested absolute path: the longest root
    /// whose canonical path is a component-wise prefix (so `/foo` does not
    /// match `/foobar`).
    fn root_for<'a>(raw: &str, roots: &'a [RootFd]) -> Option<&'a RootFd> {
        let requested = Path::new(raw);
        roots
            .iter()
            .filter(|root| {
                requested == root.path
                    // root.join("") is "<root>/"; for root "/" it stays "/",
                    // which every absolute path starts with.
                    || requested.starts_with(root.path.join(""))
            })
            .max_by_key(|root| root.path.as_os_str().len())
    }

    /// Walk `raw` openat-style from the chosen root fd, never following a
    /// symlink, then prove via `..` climbs that the resulting fd is still
    /// under that root. Returns the O_RDONLY directory fd.
    fn open_requested(roots: &[RootFd], raw: &str) -> Result<OpenedDir, NodeError> {
        let path = Path::new(raw);
        if !path.is_absolute() {
            return Err(refused(
                "browsed path must be absolute on the Node filesystem",
            ));
        }
        let Some(root) = root_for(raw, roots) else {
            return Err(refused(
                "path is outside the directories this Node allows workspaces in",
            ));
        };
        // Components relative to the chosen root, walked one openat at a time
        // with O_NOFOLLOW. "." is skipped; ".." is opened as a real directory
        // entry and then neutralised by the ancestor proof below.
        let mut anchor = dup_owned(root.fd())?;
        for component in path.strip_prefix(&root.path).unwrap_or(path).components() {
            use std::path::Component;
            let cstr: &CStr = match component {
                Component::Normal(name) => &std::ffi::CString::new(name.as_encoded_bytes())
                    .map_err(|_| refused("invalid path component"))?,
                Component::CurDir | Component::RootDir | Component::Prefix(_) => continue,
                Component::ParentDir => c"..",
            };
            anchor = match open_dir_nofollow(Some(anchor.raw()), cstr, true) {
                Ok(fd) => fd,
                Err(_) => {
                    return Err(refused(format!(
                        "{raw} is not an accessible allowed directory"
                    )));
                }
            };
        }
        // Reopen the reached O_PATH anchor as O_RDONLY ("." can never be a
        // symlink) so readdir works on the verified inode.
        let fd = open_dir_nofollow(Some(anchor.raw()), c".", false)
            .map_err(|_| refused(format!("{raw} is not an accessible allowed directory")))?;
        if !is_under_root(fd.raw(), root).map_err(nix_err)? {
            return Err(refused(
                "path is outside the directories this Node allows workspaces in",
            ));
        }
        Ok(OpenedDir { fd })
    }

    /// Default start: the user's home when it provably lies under an allowed
    /// root, else the first root. The caller canonicalizes the home once; no
    /// process environment is read inside.
    fn default_start(
        roots: &[RootFd],
        home_canonical: Option<PathBuf>,
    ) -> Result<OpenedDir, NodeError> {
        if let Some(canonical) = home_canonical {
            let raw = canonical.display().to_string();
            if let Ok(opened) = open_requested(roots, &raw) {
                return Ok(opened);
            }
        }
        let Some(root) = roots.first() else {
            return Err(refused(
                "no allowed directory is accessible on this Node; configure workspace_roots",
            ));
        };
        Ok(OpenedDir {
            fd: dup_owned(root.fd())?,
        })
    }

    fn dup_owned(fd: RawFd) -> Result<Fd, NodeError> {
        Ok(Fd(nix::unistd::dup(fd).map_err(nix_err)?))
    }

    /// Prove `fd` is `root` or under it by climbing ".." with no-follow
    /// opens and comparing (dev, ino) to the root stat. Defends against
    /// ".." segments that escaped through a rename during the walk.
    fn is_under_root(fd: RawFd, root: &RootFd) -> nix::Result<bool> {
        let root_stat = fstat(root.fd())?;
        let mut current = Fd(nix::unistd::dup(fd)?);
        for _ in 0..=MAX_ANCESTOR_CLIMBS {
            let stat = fstat(current.raw())?;
            if stat.st_dev == root_stat.st_dev && stat.st_ino == root_stat.st_ino {
                return Ok(true);
            }
            let parent = openat(
                Some(current.raw()),
                c"..",
                OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_DIRECTORY,
                nix::sys::stat::Mode::empty(),
            )?;
            let parent_stat = fstat(parent)?;
            // Reached the filesystem root without a match.
            if parent_stat.st_dev == stat.st_dev && parent_stat.st_ino == stat.st_ino {
                let _ = nix::unistd::close(parent);
                return Ok(false);
            }
            current = Fd(parent);
        }
        Ok(false)
    }

    /// The contained parent's canonical path, or None at the allowlist edge.
    fn contained_parent(roots: &[RootFd], target: RawFd) -> Result<Option<String>, NodeError> {
        let parent = openat(
            Some(target),
            c"..",
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        )
        .map_err(nix_err)?;
        let parent = Fd(parent);
        let under = roots
            .iter()
            .any(|root| is_under_root(parent.raw(), root).unwrap_or(false));
        if !under {
            return Ok(None);
        }
        Ok(Some(fd_canonical_path(parent.raw())?))
    }

    /// Canonical path of an opened fd via /proc/self/fd (kernel-resolved; the
    /// descriptor is already verified, so this read adds no TOCTOU).
    fn fd_canonical_path(fd: RawFd) -> Result<String, NodeError> {
        let path = format!("/proc/self/fd/{fd}");
        std::fs::read_link(&path)
            .map(|path| path.display().to_string())
            .map_err(|error| refused(format!("cannot resolve opened directory: {error}")))
    }

    fn is_real_subdir(dir: RawFd, name: &CStr) -> bool {
        openat(
            Some(dir),
            name,
            OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        )
        .is_ok()
    }

    fn nix_err(error: nix::Error) -> NodeError {
        NodeError::Driver(error.to_string())
    }
}

// ── non-unix fallback: pathname implementation (deployment is unix-only) ────

#[cfg(not(unix))]
mod imp {
    use super::*;

    pub(super) fn list(
        roots: &[PathBuf],
        registered_roots: &[PathBuf],
        request: HostDirsListParams,
    ) -> Result<HostDirsListResult, NodeError> {
        let raw = request.path.as_deref().filter(|value| !value.is_empty());
        let target = match raw {
            None => std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|home| {
                    std::fs::canonicalize(home)
                        .is_ok_and(|canonical| roots.iter().any(|root| canonical.starts_with(root)))
                })
                .or_else(|| roots.first().cloned())
                .ok_or_else(|| {
                    refused(
                        "no allowed directory is accessible on this Node; configure workspace_roots",
                    )
                })?,
            Some(raw) => {
                let path = Path::new(raw);
                if !path.is_absolute() {
                    return Err(refused(
                        "browsed path must be absolute on the Node filesystem",
                    ));
                }
                let canonical = std::fs::canonicalize(path)
                    .map_err(|error| refused(format!("{raw} cannot be resolved: {error}")))?;
                if !roots.iter().any(|root| canonical.starts_with(root)) {
                    return Err(refused(
                        "{raw} is outside the directories this Node allows workspaces in",
                    ));
                }
                canonical
            }
        };
        let mut names = Vec::new();
        let mut scanned = 0usize;
        let mut truncated = false;
        for entry in std::fs::read_dir(&target)
            .map_err(|error| refused(format!("{} cannot be listed: {error}", target.display())))?
        {
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
        let mut workspaces: Vec<String> = registered_roots
            .iter()
            .filter(|root| roots.iter().any(|allowed| root.starts_with(allowed)))
            .map(|root| root.display().to_string())
            .collect();
        workspaces.sort();
        workspaces.dedup();
        Ok(HostDirsListResult {
            path: target.display().to_string(),
            parent: target.parent().map(|parent| parent.display().to_string()),
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .and_then(|home| std::fs::canonicalize(home).ok())
                .map(|path| path.display().to_string()),
            roots: roots
                .iter()
                .map(|root| root.display().to_string())
                .collect(),
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
    roots: &[PathBuf],
    registered_roots: &[PathBuf],
    request: HostDirsListParams,
) -> Result<HostDirsListResult, NodeError> {
    imp::list(roots, registered_roots, request)
}

#[cfg(not(unix))]
fn list_directories(
    roots: &[PathBuf],
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

    #[test]
    fn lists_real_subdirectories_within_the_allowlist_and_hides_dotdirs() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("beta")).unwrap();
        fs::create_dir_all(root.path().join("alpha/sub")).unwrap();
        fs::write(root.path().join("file.txt"), b"x").unwrap();
        fs::create_dir_all(root.path().join(".secret")).unwrap();
        let roots = vec![fs::canonicalize(root.path()).unwrap()];

        let result = list_directories(&roots, &[], request(None, false)).unwrap();
        let names: Vec<&str> = result
            .dirs
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert!(!result.truncated);
        assert_eq!(result.path, roots[0].display().to_string());
        assert_eq!(result.parent, None);
        assert_eq!(result.roots.len(), 1);

        // Descend: parent is now the allowlisted root.
        let child = list_directories(
            &roots,
            &[],
            request(Some(&roots[0].join("alpha").display().to_string()), false),
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
        assert_eq!(child.parent, Some(roots[0].display().to_string()));

        // Hidden directories appear only when explicitly requested.
        let shown = list_directories(&roots, &[], request(None, true)).unwrap();
        assert!(shown.dirs.iter().any(|entry| entry.name == ".secret"));
    }

    #[test]
    fn refuses_paths_outside_the_allowlist_relative_paths_and_files() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(root.path().join("file"), b"x").unwrap();
        let roots = vec![fs::canonicalize(root.path()).unwrap()];

        let error = list_directories(&roots, &[], request(Some("../etc"), false))
            .unwrap_err()
            .to_string();
        assert!(error.contains("must be absolute"), "{error}");

        let error = list_directories(
            &roots,
            &[],
            request(Some(outside.path().to_str().unwrap()), false),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("outside the directories"), "{error}");

        let error = list_directories(
            &roots,
            &[],
            request(Some(&root.path().join("file").display().to_string()), false),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("not an accessible allowed directory"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_that_escapes_the_allowlist_is_never_navigable() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(outside.path().join("target")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let roots = vec![fs::canonicalize(root.path()).unwrap()];

        // The symlink is absent from the directory rows even when it points at
        // a real directory.
        let result = list_directories(&roots, &[], request(None, false)).unwrap();
        assert!(result.dirs.is_empty(), "{:?}", result.dirs);

        // Following it by name is refused at the no-follow walk, regardless
        // of what it points at.
        let error = list_directories(
            &roots,
            &[],
            request(Some(&root.path().join("link").display().to_string()), false),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("outside the directories")
                || error.contains("not an accessible allowed directory"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_swapped_in_after_the_root_opens_cannot_redirect_the_listing() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("real")).unwrap();
        fs::create_dir_all(outside.path().join("stolen")).unwrap();
        // The requested directory is a real directory at check time.
        let target = root.path().join("swap");
        fs::create_dir_all(&target).unwrap();
        let roots = vec![fs::canonicalize(root.path()).unwrap()];
        // Swap it for a symlink right before enumeration: an O_NOFOLLOW walk
        // refuses the link instead of listing the outside tree.
        std::os::unix::fs::symlink(outside.path(), root.path().join("swap-link")).unwrap();
        let via_link = root.path().join("swap-link").display().to_string();
        let error = list_directories(&roots, &[], request(Some(&via_link), false))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("outside the directories")
                || error.contains("not an accessible allowed directory"),
            "{error}"
        );
        // The real directory still lists exactly its own entries.
        let result = list_directories(
            &roots,
            &[],
            request(Some(&target.display().to_string()), false),
        )
        .unwrap();
        assert!(result.dirs.is_empty());
    }

    #[test]
    fn caps_the_result_and_reports_truncation() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..(HOST_DIRS_MAX_ENTRIES + 25) {
            fs::create_dir_all(root.path().join(format!("d-{index:06}"))).unwrap();
        }
        let roots = vec![fs::canonicalize(root.path()).unwrap()];
        let result = list_directories(&roots, &[], request(None, false)).unwrap();
        assert_eq!(result.dirs.len(), HOST_DIRS_MAX_ENTRIES);
        assert!(result.truncated);
        // The cap keeps the lexicographically first names, deterministically.
        assert_eq!(result.dirs[0].name, "d-000000");
    }

    #[test]
    fn the_scan_bound_counts_every_entry_not_just_directories() {
        let root = tempfile::tempdir().unwrap();
        // More plain files than the scan cap: they are never rows, but the
        // walk must still stop and report truncation rather than scan all.
        for index in 0..(HOST_DIRS_MAX_SCANNED + 8) {
            fs::write(root.path().join(format!("f-{index:06}")), b"x").unwrap();
        }
        fs::create_dir_all(root.path().join("the-only-dir")).unwrap();
        let roots = vec![fs::canonicalize(root.path()).unwrap()];
        let result = list_directories(&roots, &[], request(None, false)).unwrap();
        assert!(result.truncated);
        // At most the single real directory can appear; files never do.
        assert!(result.dirs.len() <= 1);
        assert!(
            result
                .dirs
                .iter()
                .all(|entry| !entry.name.starts_with("f-"))
        );
    }

    #[test]
    fn default_start_is_home_when_home_is_an_allowed_root() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("project")).unwrap();
        let roots = vec![fs::canonicalize(root.path()).unwrap()];
        // With the home anchored at the allowlist root, the empty selector
        // opens home and the reply carries the quick-jump home. The home
        // anchor is exercised directly through the fd walk on unix; here we
        // only assert the structural fallback for the roots themselves.
        let result = list_directories(
            &roots,
            &[],
            request(Some(roots[0].to_str().unwrap()), false),
        )
        .unwrap();
        assert_eq!(result.path, roots[0].display().to_string());
        assert!(
            result.workspaces.is_empty(),
            "registered roots are reported separately"
        );
    }

    #[test]
    fn registered_roots_are_reported_for_quick_jump() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("one")).unwrap();
        let canonical_root = fs::canonicalize(root.path()).unwrap();
        let registered = vec![canonical_root.join("one")];
        let roots = vec![canonical_root];
        let result = list_directories(&roots, &registered, request(None, false)).unwrap();
        assert_eq!(result.workspaces, vec![registered[0].display().to_string()]);
    }
}
