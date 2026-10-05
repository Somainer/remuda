//! Temp-tree sandbox shared by the test doubles (c-resumehome review item 8).
//!
//! A fake must never create or modify files in the operator's real home. All
//! write roots (config homes, transcript dirs, caller-named artifact paths)
//! are checked against the OS temp tree:
//!
//! - an explicit path outside the temp tree is a loud error, because a test
//!   asked for that exact target;
//! - an implicit `$HOME`-derived path outside the tree is treated as
//!   "no sandboxed home" by [`sandboxed_home`], so callers either skip
//!   persistence (fake-claude) or mint a private per-process temp home
//!   (fake-harness).
//!
//! A deliberate manual run of a fake outside a test harness can opt out with
//! the binary-specific `*_ALLOW_HOME_WRITE=1` knob; no test sets it.

use std::path::{Component, Path, PathBuf};

/// Name of the escape-hatch env var that authorizes writes outside the temp
/// tree for a deliberate manual run.
pub fn outside_writes_allowed(allow_env: &str) -> bool {
    std::env::var_os(allow_env).as_deref() == Some(std::ffi::OsStr::new("1"))
}

/// Lexically normalize `path` (`.`/`..` resolved, no symlink following),
/// making it absolute.
pub fn normalize(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether `path` resolves inside a system temp directory.
///
/// `std::env::temp_dir()` (the host's `$TMPDIR`, which on shared CI boxes can
/// point at a per-user scratch) and the canonical system temps (`/tmp`,
/// `/var/tmp`) all count: tests legitimately pin homes with
/// `tempdir_in("/tmp")` even when their own `$TMPDIR` is elsewhere. The
/// operator's real home (`$HOME/.claude`) is never under any of them.
pub fn path_is_in_temp(path: &Path) -> bool {
    let candidate = std::fs::canonicalize(path).unwrap_or_else(|_| normalize(path));
    temp_roots().iter().any(|root| candidate.starts_with(root))
}

/// Canonical system temp roots accepted as sandboxed write locations.
fn temp_roots() -> Vec<PathBuf> {
    let mut roots = vec![normalize(&std::env::temp_dir())];
    #[cfg(unix)]
    {
        roots.push(PathBuf::from("/tmp"));
        roots.push(PathBuf::from("/var/tmp"));
    }
    let mut deduped = Vec::new();
    for root in roots {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        if !deduped.contains(&root) {
            deduped.push(root);
        }
    }
    deduped
}

/// Reject an explicit write target outside the per-test temp tree.
pub fn ensure_path_in_temp(path: &Path, allow_env: &str) -> std::io::Result<()> {
    if outside_writes_allowed(allow_env) || path_is_in_temp(path) {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "test fake refuses to write outside the per-test temp tree: {} \
                 (set {allow_env}=1 only for a deliberate manual run)",
                path.display()
            ),
        ))
    }
}

/// Resolve a sandboxed home for the `(explicit_env, home_env, fallback)`
/// triple used by every fake.
///
/// - an explicit `<explicit_env>` inside the temp tree wins; outside it is an
///   error;
/// - else `$<home_env>/<fallback>` is used only when it is inside the temp
///   tree (a per-test `HOME`);
/// - otherwise `None`.
pub fn sandboxed_home(
    explicit_env: &str,
    home_env: &str,
    fallback: &str,
    allow_env: &str,
) -> std::io::Result<Option<PathBuf>> {
    if outside_writes_allowed(allow_env) {
        if let Some(path) = std::env::var_os(explicit_env)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
        {
            return Ok(Some(path));
        }
        if let Some(home) = std::env::var_os(home_env).filter(|value| !value.is_empty()) {
            return Ok(Some(PathBuf::from(home).join(fallback)));
        }
        return Ok(None);
    }
    if let Some(configured) = std::env::var_os(explicit_env)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        ensure_path_in_temp(&configured, allow_env)?;
        return Ok(Some(configured));
    }
    let implicit = std::env::var_os(home_env)
        .filter(|value| !value.is_empty())
        .map(|home| PathBuf::from(home).join(fallback))
        .filter(|home| path_is_in_temp(home));
    Ok(implicit)
}

/// A private per-process temp directory a fake can use when no sandboxed home
/// was provided. Created lazily by the caller.
pub fn private_temp_home(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{label}-{}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_tree_paths_are_accepted() {
        assert!(path_is_in_temp(&std::env::temp_dir().join("a/b/c")));
        let missing = std::env::temp_dir().join(format!("does-not-exist-{}", std::process::id()));
        assert!(
            path_is_in_temp(&missing),
            "lexical verdict for a not-yet-created dir"
        );
    }

    #[test]
    fn dotdot_traversal_does_not_leave_the_temp_tree() {
        let sneaky = std::env::temp_dir().join("../outside-fake-harness/home/.claude");
        assert!(!path_is_in_temp(&sneaky));
        assert!(!path_is_in_temp(Path::new("/home/someone-else/.claude")));
    }

    #[test]
    fn explicit_outside_temp_is_refused_unless_allowed() {
        // The crate manifest lives in the worktree, not under /tmp on ordinary
        // hosts; skip where the premise does not hold. The verdict is lexical,
        // so the path need not exist. (No env mutation: set_var is unsafe and
        // process-global; the `=1` branch is covered by
        // `outside_writes_allowed` below.)
        let scratch = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("sandbox-scratch");
        let env_key = "FAKE_TEST_ALLOW_DOES_NOT_EXIST";
        if !path_is_in_temp(&scratch) {
            assert!(!outside_writes_allowed(env_key));
            assert!(ensure_path_in_temp(&scratch, env_key).is_err());
        }
    }

    #[test]
    fn only_exactly_one_enables_outside_writes() {
        assert!(!outside_writes_allowed("FAKE_TEST_NEVER_SET_XYZ"));
    }
}
