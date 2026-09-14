//! Workspace allowlist roots that hold in any checkout location.
//!
//! `DevServerConfig::loopback` defaults `workspace_root` to `"."`, which the
//! registry resolves against the current directory — under `cargo test` that is
//! the crate directory, not the temp dir. Tests that pinned only
//! `std::env::temp_dir()` therefore passed from a `/tmp` checkout and failed on
//! CI, where the tree sits under `/home/runner/work`. Allowing both the crate
//! directory and the temp dir makes the tests independent of either layout.

use std::path::PathBuf;

/// Allowed roots for a test Node: `manifest_dir` plus the system temp dir.
///
/// Both are canonicalized, and a root already contained in another is dropped
/// (a `/tmp` checkout makes the crate directory a descendant of the temp dir).
/// Prefer [`test_workspace_roots!`], which fills in the caller's crate
/// directory; call this directly only when the directory is computed.
pub fn test_workspace_roots(manifest_dir: impl Into<PathBuf>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for root in [manifest_dir.into(), std::env::temp_dir()] {
        let root = root.canonicalize().unwrap_or(root);
        // Keep the shortest covering prefix: a descendant of a root already
        // held, or a duplicate of it, would widen nothing.
        if roots.iter().any(|held| root.starts_with(held)) {
            continue;
        }
        roots.retain(|held| !held.starts_with(&root));
        roots.push(root);
    }
    roots
}

/// [`test_workspace_roots`] for the crate being compiled.
///
/// `CARGO_MANIFEST_DIR` expands at the call site, so this reports the calling
/// crate's directory rather than `remuda-testing`'s.
#[macro_export]
macro_rules! test_workspace_roots {
    () => {
        $crate::test_workspace_roots(env!("CARGO_MANIFEST_DIR"))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_the_crate_directory_and_the_temp_dir() {
        let roots = test_workspace_roots!();
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let manifest = manifest.canonicalize().unwrap_or(manifest);
        let temp = std::env::temp_dir();
        let temp = temp.canonicalize().unwrap_or(temp);
        assert!(
            roots.iter().any(|root| manifest.starts_with(root)),
            "crate dir {} uncovered by {roots:?}",
            manifest.display()
        );
        assert!(
            roots.iter().any(|root| temp.starts_with(root)),
            "temp dir {} uncovered by {roots:?}",
            temp.display()
        );
    }

    #[test]
    fn a_crate_dir_inside_the_temp_dir_collapses_to_one_root() {
        let nested = tempfile::tempdir().unwrap();
        let canonical = nested.path().canonicalize().unwrap();
        let roots = test_workspace_roots(&canonical);
        assert_eq!(roots.len(), 1, "nested crate dir must not add a root");
        assert!(canonical.starts_with(&roots[0]));
    }

    #[test]
    fn a_crate_dir_outside_the_temp_dir_keeps_both_roots() {
        let roots = test_workspace_roots("/");
        assert_eq!(roots.len(), 1, "root path subsumes the temp dir");
        assert_eq!(roots[0], PathBuf::from("/"));
    }
}
