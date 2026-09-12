//! Containment checks for caller-supplied paths (worktree `path`, instance `cwd`).
//!
//! Two rules, shared by every caller so the CLI, the Node, and the Hub cannot
//! drift apart:
//!
//! - a name is one safe segment (`[a-z][a-z0-9_-]{0,31}`), never a path;
//! - a path resolves — after `..` normalization and canonicalization of the
//!   part that already exists — inside an allowed root.
//!
//! Canonicalizing only the existing prefix is deliberate: a worktree directory
//! is checked *before* `git worktree add` creates it, so the leaf does not
//! exist yet. Everything that does exist is resolved through symlinks, so a
//! symlinked ancestor cannot be used to leave the root.

use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

/// Name / path shape rejections; the caller maps these to its own error type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathGuardError {
    /// A name was not a single `[a-z][a-z0-9_-]{0,31}` segment.
    #[error("name must match [a-z][a-z0-9_-]{{0,31}}, got {0:?}")]
    Segment(String),
    /// A path was relative, or used a prefix/`..` that leaves the filesystem root.
    #[error("{0} is not a plain absolute path")]
    Shape(String),
    /// A path resolved outside every allowed root.
    #[error("{path} resolves outside {root}")]
    Escape {
        /// The rejected path, as resolved.
        path: String,
        /// The root it was required to stay inside.
        root: String,
    },
    /// The path could not be resolved (permission denied, not a directory, …).
    #[error("cannot resolve {path}: {detail}")]
    Unresolvable {
        /// The path being resolved.
        path: String,
        /// The underlying I/O reason.
        detail: String,
    },
    /// A repository root has no parent to hold the worktree directory.
    #[error("{0} has no parent directory for worktrees")]
    NoParent(String),
}

/// Directory name holding worktrees beside a repository root.
pub const WORKTREE_DIR: &str = "remuda-wt";

/// Accept one safe path segment.
///
/// The character set excludes `/`, `\\`, and `.`, so a value that passes is a
/// single segment and can never be `.`, `..`, or a traversal.
pub fn safe_segment(name: &str) -> Result<(), PathGuardError> {
    let valid = (1..=32).contains(&name.len())
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(PathGuardError::Segment(name.to_owned()))
    }
}

/// The only directory worktrees may be created in: `<repo>/../remuda-wt`.
pub fn worktree_root(repo_root: &Path) -> Result<PathBuf, PathGuardError> {
    let real = real_path(repo_root)?;
    let parent = real
        .parent()
        .ok_or_else(|| PathGuardError::NoParent(real.display().to_string()))?;
    Ok(parent.join(WORKTREE_DIR))
}

/// Join `candidate` onto `base` when it is relative; absolute paths pass through.
pub fn absolutize(base: &Path, candidate: &Path) -> PathBuf {
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base.join(candidate)
    }
}

/// Resolve `candidate` and require it to sit inside one of `roots`.
///
/// `candidate` must be absolute — use [`absolutize`] first when the caller has
/// its own base for relative input. Returns the resolved path, which is what
/// the caller should use from then on; the unresolved spelling may still
/// contain `..` or symlinks.
pub fn contain(roots: &[&Path], candidate: &Path) -> Result<PathBuf, PathGuardError> {
    let resolved = real_path(candidate)?;
    for root in roots {
        // An unresolvable root cannot contain anything; try the next one
        // instead of failing the whole check.
        let Ok(root_real) = real_path(root) else {
            continue;
        };
        if resolved.starts_with(&root_real) {
            return Ok(resolved);
        }
    }
    Err(PathGuardError::Escape {
        path: resolved.display().to_string(),
        root: roots
            .first()
            .map(|root| root.display().to_string())
            .unwrap_or_default(),
    })
}

/// Like [`contain`], but also reject the root itself — the path must name
/// something *inside* it. Used where the caller is about to create a directory.
pub fn contain_strict(roots: &[&Path], candidate: &Path) -> Result<PathBuf, PathGuardError> {
    let resolved = contain(roots, candidate)?;
    for root in roots {
        if real_path(root).is_ok_and(|root_real| root_real == resolved) {
            return Err(PathGuardError::Escape {
                path: resolved.display().to_string(),
                root: resolved.display().to_string(),
            });
        }
    }
    Ok(resolved)
}

/// Normalize `..` lexically, then canonicalize the longest existing prefix.
pub fn real_path(path: &Path) -> Result<PathBuf, PathGuardError> {
    let normalized = normalize(path)?;
    let mut tail: Vec<OsString> = Vec::new();
    let mut probe = normalized.clone();
    loop {
        match probe.canonicalize() {
            Ok(real) => {
                let mut out = real;
                out.extend(tail.iter().rev());
                return Ok(out);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let Some(name) = probe.file_name().map(ToOwned::to_owned) else {
                    // Walked up to `/` without finding anything that exists.
                    return Err(PathGuardError::Unresolvable {
                        path: normalized.display().to_string(),
                        detail: "no existing ancestor".to_owned(),
                    });
                };
                tail.push(name);
                probe.pop();
            }
            Err(error) => {
                return Err(PathGuardError::Unresolvable {
                    path: probe.display().to_string(),
                    detail: error.to_string(),
                });
            }
        }
    }
}

/// Resolve `.` and `..` textually. Rejects relative paths, Windows prefixes,
/// and any `..` that would climb past the filesystem root.
fn normalize(path: &Path) -> Result<PathBuf, PathGuardError> {
    if !path.is_absolute() {
        return Err(PathGuardError::Shape(path.display().to_string()));
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Err(PathGuardError::Shape(path.display().to_string()));
                }
            }
            Component::Normal(part) => out.push(part),
            Component::Prefix(_) => {
                return Err(PathGuardError::Shape(path.display().to_string()));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_rejects_traversal_and_separators() {
        assert!(safe_segment("agent1").is_ok());
        assert!(safe_segment("x-acpwire").is_ok());
        assert!(safe_segment("..").is_err());
        assert!(safe_segment(".").is_err());
        assert!(safe_segment("a/b").is_err());
        assert!(safe_segment("a\\b").is_err());
        assert!(safe_segment("../../etc").is_err());
        assert!(safe_segment("Agent").is_err());
        assert!(safe_segment("1abc").is_err());
        assert!(safe_segment("").is_err());
        assert!(safe_segment(&"a".repeat(33)).is_err());
    }

    #[test]
    fn normalize_resolves_dot_segments() {
        assert_eq!(normalize(Path::new("/a/./b")).unwrap(), Path::new("/a/b"));
        assert_eq!(
            normalize(Path::new("/a/b/../c")).unwrap(),
            Path::new("/a/c")
        );
        assert!(normalize(Path::new("a/b")).is_err());
        assert!(normalize(Path::new("/../..")).is_err());
    }

    #[test]
    fn contain_accepts_descendants_and_rejects_escapes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        // Existing and not-yet-existing descendants both pass.
        assert!(contain(&[&root], &root.join("wt")).is_ok());
        assert!(contain(&[&root], &root.join("deep/not/created")).is_ok());
        // The root itself is contained, but not a strict descendant.
        assert!(contain(&[&root], &root).is_ok());
        assert!(contain_strict(&[&root], &root).is_err());
        // Traversal out of the root.
        assert!(contain(&[&root], &root.join("../outside")).is_err());
        assert!(contain(&[&root], Path::new("/etc/passwd")).is_err());
        assert!(contain(&[&root], &outside).is_err());
        // A sibling sharing a name prefix is not contained.
        let sibling = tmp.path().join("rootx");
        std::fs::create_dir_all(&sibling).unwrap();
        assert!(contain(&[&root], &sibling).is_err());
    }

    #[test]
    fn contain_follows_symlinks_before_deciding() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
            // The link sits under the root but resolves outside it.
            assert!(contain(&[&root], &root.join("escape")).is_err());
            assert!(contain(&[&root], &root.join("escape/evil")).is_err());
        }
    }

    #[test]
    fn contain_accepts_any_listed_root() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("repo");
        let worktrees = tmp.path().join(WORKTREE_DIR);
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&worktrees).unwrap();
        let roots: Vec<&Path> = vec![&workspace, &worktrees];
        assert!(contain(&roots, &workspace.join("crates")).is_ok());
        assert!(contain(&roots, &worktrees.join("agent1")).is_ok());
        assert!(contain(&roots, tmp.path()).is_err());
    }

    #[test]
    fn worktree_root_is_the_repo_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let root = worktree_root(&repo).unwrap();
        assert_eq!(root.file_name().unwrap(), WORKTREE_DIR);
        assert_eq!(root.parent().unwrap(), real_path(tmp.path()).unwrap());
    }

    #[test]
    fn absolutize_only_joins_relative_paths() {
        let base = Path::new("/base");
        assert_eq!(
            absolutize(base, Path::new("../wt/x")),
            Path::new("/base/../wt/x")
        );
        assert_eq!(absolutize(base, Path::new("/abs")), Path::new("/abs"));
    }
}
