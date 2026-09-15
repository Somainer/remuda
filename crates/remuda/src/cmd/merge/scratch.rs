//! Scratch directories for isolated merge worktrees.
//!
//! Every merge lane creates worktrees outside the caller's checkout. They
//! must never live under (or be removable as) a caller-controlled path:
//! the root is always the OS temp dir, each run gets exactly
//! `remuda-mq-<pid>-<n>-<nanos>/`, and removal is allowed only for paths
//! proven to be inside that exact directory. Symlinks escaping the root are
//! refused. This is the post-2026-09-15 hardening after a mis-constructed
//! relative `rm -rf` deleted `/tmp/remuda-agents` wholesale.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Prefix of every scratch directory this binary creates.
pub(crate) const SCRATCH_PREFIX: &str = "remuda-mq-";

/// Create a fresh, empty scratch root for one merge lane.
pub(crate) fn create_root() -> Result<PathBuf> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before UNIX epoch")?
        .as_nanos();
    let name = format!(
        "{SCRATCH_PREFIX}{}-{}-{timestamp}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let root = std::env::temp_dir().join(name);
    fs::create_dir(&root).with_context(|| format!("create scratch root {}", root.display()))?;
    // Canonicalise so later containment checks compare real paths.
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalise scratch root {}", root.display()))?;
    Ok(root)
}

/// True when `child` equals `root` or is strictly inside it. Both paths
/// should already be canonical (no `.`/`..` components or symlinks).
pub(crate) fn is_within(root: &Path, child: &Path) -> bool {
    use std::path::Component;
    let root_parts: Vec<Component> = root.components().collect();
    let child_parts: Vec<Component> = child.components().collect();
    if child_parts.len() < root_parts.len() {
        return false;
    }
    root_parts
        .iter()
        .zip(child_parts.iter())
        .all(|(a, b)| a == b)
}

/// Resolve `path` without following a symlink at the final component, and
/// assert the result stays inside `root`. The path does not need to exist
/// yet (the worktree is created after the check).
pub(crate) fn ensure_within(root: &Path, path: &Path) -> Result<PathBuf> {
    // Only absolute paths are meaningful for scratch containment; the
    // caller passes an absolute scratch root and absolute children.
    let mut absolute = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => absolute.push(component),
            Component::CurDir => {}
            Component::ParentDir => {
                bail!("scratch path escapes its root via '..': {}", path.display());
            }
            Component::Normal(part) => absolute.push(part),
        }
    }
    if absolute.is_relative() {
        bail!("scratch path must be absolute: {}", path.display());
    }

    // Walk the components, canonicalising any existing intermediate so a
    // symlink escaping the root is detected before deletion.
    let mut resolved = PathBuf::from("/");
    let root_components: Vec<Component> = root.components().collect();
    for (index, component) in absolute.components().enumerate() {
        if let Component::RootDir = component {
            continue;
        }
        let candidate = resolved.join(component);
        if candidate.exists() {
            let real = candidate
                .canonicalize()
                .with_context(|| format!("resolve {}", candidate.display()))?;
            // The canonicalised scratch root is safe; deeper entries must
            // stay beneath it.
            if index >= root_components.len() && !is_within(root, &real) {
                bail!(
                    "scratch path {} resolves outside its scratch root {}",
                    path.display(),
                    root.display()
                );
            }
            resolved = real;
        } else {
            // A not-yet-existing component: compare lexically against the
            // root prefix once we are at or past the root depth.
            if index >= root_components.len() && !is_within(root, &candidate) {
                bail!(
                    "scratch path {} is outside its scratch root {}",
                    path.display(),
                    root.display()
                );
            }
            resolved = candidate;
        }
    }
    if !is_within(root, &resolved) {
        bail!(
            "scratch path {} is outside its scratch root {}",
            path.display(),
            root.display()
        );
    }
    Ok(resolved)
}

/// Remove exactly `path`, which must resolve inside `scratch_root`.
///
/// This is the single allowed scratch deletion primitive: callers never
/// `remove_dir_all` a path they assembled themselves without going through
/// it, so no caller-supplied or ancestor path can ever be deleted.
pub(crate) fn remove_within(scratch_root: &Path, path: &Path) -> Result<()> {
    let root = scratch_root
        .canonicalize()
        .with_context(|| format!("canonicalise scratch root {}", scratch_root.display()))?;
    let target = ensure_within(&root, path)?;
    if target == root {
        fs::remove_dir_all(&root)
            .with_context(|| format!("remove scratch root {}", root.display()))?;
    } else if target.is_dir() {
        fs::remove_dir_all(&target)
            .with_context(|| format!("remove scratch directory {}", target.display()))?;
    } else if target.exists() {
        fs::remove_file(&target)
            .with_context(|| format!("remove scratch file {}", target.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_live_under_temp_dir_with_the_scratch_prefix() {
        let root = create_root().unwrap();
        let parent = std::env::temp_dir().canonicalize().unwrap();
        assert!(is_within(&parent, &root), "{root:?} not under {parent:?}");
        assert!(
            root.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(SCRATCH_PREFIX)
        );
        remove_within(&root, &root).unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn only_paths_inside_the_exact_root_can_be_removed() {
        let root = create_root().unwrap();
        let worktree = root.join("worktree");
        fs::create_dir_all(worktree.join("nested")).unwrap();
        // Removing the worktree target is allowed.
        remove_within(&root, &root.join("worktree")).unwrap();
        assert!(!root.join("worktree").exists());
        assert!(root.exists());
        // Parent traversal and sibling paths are refused even if they exist.
        let outside = std::env::temp_dir().join(format!("{SCRATCH_PREFIX}outside-sibling"));
        fs::create_dir_all(&outside).unwrap();
        assert!(remove_within(&root, &root.join("..").join("..")).is_err());
        assert!(remove_within(&root, &outside).is_err());
        assert!(outside.exists());
        // A symlink pointing at a live path out of the root is refused:
        // deletion/generic callers must never follow it past the root.
        let escape = root.join("escape");
        std::os::unix::fs::symlink(&outside, &escape).unwrap();
        assert!(remove_within(&root, &escape).is_err());
        assert!(outside.exists(), "symlink target must be untouched");
        fs::remove_file(&escape).ok();
        fs::remove_dir_all(&outside).ok();
        remove_within(&root, &root).unwrap();
    }

    #[test]
    fn distinct_lanes_get_distinct_roots() {
        let a = create_root().unwrap();
        let b = create_root().unwrap();
        assert_ne!(a, b);
        remove_within(&a, &a).unwrap();
        remove_within(&b, &b).unwrap();
    }
}
