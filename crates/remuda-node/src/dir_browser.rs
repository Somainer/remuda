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
//! - Containment is decided on the *canonical* path of both the browsed
//!   directory and every jump target.
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

/// `host.dirs.list` is the only method handled here.
pub(crate) fn is_host_dirs_method(method: &str) -> bool {
    method == METHOD_HOST_DIRS_LIST
}

/// The canonical user home when it exists and lies inside an allowed root.
fn contained_home(home: Option<&Path>, roots: &[PathBuf]) -> Option<PathBuf> {
    let home = home?;
    let canonical = std::fs::canonicalize(home).ok()?;
    roots
        .iter()
        .any(|root| canonical.starts_with(root))
        .then_some(canonical)
}

/// Choose the first allowlist root that exists on disk.
fn first_existing_root(roots: &[PathBuf]) -> Option<PathBuf> {
    roots
        .iter()
        .find(|root| std::fs::symlink_metadata(root).is_ok_and(|meta| meta.is_dir()))
        .cloned()
}

/// Resolve the requested directory (or the default start) to a canonical
/// directory contained in an allowed root.
fn resolve_target(
    roots: &[PathBuf],
    home: Option<&Path>,
    request_path: Option<&str>,
) -> Result<PathBuf, NodeError> {
    let raw = request_path
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(raw) = raw else {
        let start = contained_home(home, roots).or_else(|| first_existing_root(roots));
        return start.ok_or_else(|| {
            NodeError::InvalidRequest(
                "no allowed directory is accessible on this Node; configure workspace_roots".into(),
            )
        });
    };
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(NodeError::InvalidRequest(
            "browsed path must be absolute on the Node filesystem".into(),
        ));
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| NodeError::InvalidRequest(format!("{raw} cannot be resolved: {error}")))?;
    if !canonical.is_dir() {
        return Err(NodeError::InvalidRequest(format!(
            "{raw} is not a directory"
        )));
    }
    if !roots.iter().any(|root| canonical.starts_with(root)) {
        return Err(NodeError::InvalidRequest(format!(
            "{raw} is outside the directories this Node allows workspaces in"
        )));
    }
    Ok(canonical)
}

/// The parent directory when navigation may still stay inside an allowed
/// root; `None` once another step up would leave the allowlist.
fn contained_parent(roots: &[PathBuf], target: &Path) -> Option<PathBuf> {
    let parent = target.parent()?;
    if parent == target {
        return None;
    }
    roots
        .iter()
        .any(|root| parent.starts_with(root))
        .then(|| parent.to_path_buf())
}

/// List one contained directory's real subdirectories, hidden entries off by
/// default and the result capped at [`HOST_DIRS_MAX_ENTRIES`].
fn list_directories(
    roots: &[PathBuf],
    registered_roots: &[PathBuf],
    home: Option<&Path>,
    request: HostDirsListParams,
) -> Result<HostDirsListResult, NodeError> {
    let target = resolve_target(roots, home, request.path.as_deref())?;
    let metadata = std::fs::symlink_metadata(&target).map_err(|error| {
        NodeError::InvalidRequest(format!("{} cannot be inspected: {error}", target.display()))
    })?;
    if !metadata.is_dir() {
        return Err(NodeError::InvalidRequest(format!(
            "{} is not a directory",
            target.display()
        )));
    }
    let mut names: Vec<String> = Vec::new();
    let mut truncated = false;
    for entry in std::fs::read_dir(&target).map_err(|error| {
        NodeError::InvalidRequest(format!("{} cannot be listed: {error}", target.display()))
    })? {
        let Ok(entry) = entry else { continue };
        if names.len() >= HOST_DIRS_MAX_SCANNED {
            truncated = true;
            break;
        }
        // `symlink_metadata` so a symlink to a directory is classified as a
        // symlink and skipped: following it could jump outside the allowlist.
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
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
        parent: contained_parent(roots, &target).map(|path| path.display().to_string()),
        home: contained_home(home, roots).map(|path| path.display().to_string()),
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
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let registered: Vec<PathBuf> = self
            .workspaces()?
            .iter()
            .map(|workspace| PathBuf::from(&workspace.root_path))
            .collect();
        let result = tokio::task::spawn_blocking(move || {
            list_directories(&roots, &registered, home.as_deref(), request)
        })
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

        let result = list_directories(&roots, &[], None, request(None, false)).unwrap();
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
            None,
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
        let shown = list_directories(&roots, &[], None, request(None, true)).unwrap();
        assert!(shown.dirs.iter().any(|entry| entry.name == ".secret"));
    }

    #[test]
    fn refuses_paths_outside_the_allowlist_relative_paths_and_files() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(root.path().join("file"), b"x").unwrap();
        let roots = vec![fs::canonicalize(root.path()).unwrap()];

        let error = list_directories(&roots, &[], None, request(Some("../etc"), false))
            .unwrap_err()
            .to_string();
        assert!(error.contains("must be absolute"), "{error}");

        let error = list_directories(
            &roots,
            &[],
            None,
            request(Some(outside.path().to_str().unwrap()), false),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("outside the directories"), "{error}");

        let error = list_directories(
            &roots,
            &[],
            None,
            request(Some(&root.path().join("file").display().to_string()), false),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("not a directory"), "{error}");
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
        let result = list_directories(&roots, &[], None, request(None, false)).unwrap();
        assert!(result.dirs.is_empty(), "{:?}", result.dirs);

        // Following it by its canonical target resolves outside and is
        // refused, exactly like naming any other outside directory.
        let error = list_directories(
            &roots,
            &[],
            None,
            request(Some(&root.path().join("link").display().to_string()), false),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("outside the directories"), "{error}");
    }

    #[test]
    fn caps_the_result_and_reports_truncation() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..(HOST_DIRS_MAX_ENTRIES + 25) {
            fs::create_dir_all(root.path().join(format!("d-{index:06}"))).unwrap();
        }
        let roots = vec![fs::canonicalize(root.path()).unwrap()];
        let result = list_directories(&roots, &[], None, request(None, false)).unwrap();
        assert_eq!(result.dirs.len(), HOST_DIRS_MAX_ENTRIES);
        assert!(result.truncated);
        // The cap keeps the lexicographically first names, deterministically.
        assert_eq!(result.dirs[0].name, "d-000000");
    }

    #[test]
    fn default_start_is_home_when_home_is_an_allowed_root() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("project")).unwrap();
        let roots = vec![fs::canonicalize(root.path()).unwrap()];
        // With the home anchored at the allowlist root, the empty selector
        // opens home and the reply carries the quick-jump home.
        let result =
            list_directories(&roots, &[], Some(root.path()), request(None, false)).unwrap();
        assert_eq!(result.path, roots[0].display().to_string());
        assert_eq!(result.home.as_deref(), Some(roots[0].to_str().unwrap()));
        assert!(
            result.workspaces.is_empty(),
            "registered roots are reported separately"
        );

        // A home outside the allowlist is omitted, and the empty selector
        // falls back to the first existing root instead.
        let other_home = tempfile::tempdir().unwrap();
        fs::create_dir_all(other_home.path().join("elsewhere")).unwrap();
        let result =
            list_directories(&roots, &[], Some(other_home.path()), request(None, false)).unwrap();
        assert_eq!(result.path, roots[0].display().to_string());
        assert_eq!(result.home, None);
    }

    #[test]
    fn registered_roots_are_reported_for_quick_jump() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("one")).unwrap();
        let canonical_root = fs::canonicalize(root.path()).unwrap();
        let registered = vec![canonical_root.join("one")];
        let roots = vec![canonical_root];
        let result = list_directories(&roots, &registered, None, request(None, false)).unwrap();
        assert_eq!(result.workspaces, vec![registered[0].display().to_string()]);
    }
}
