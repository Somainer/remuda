//! Git worktree create/list used by Hub `worktree.create` / `worktree.list`.

use crate::NodeError;
use remuda_protocol::path_guard;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Record stored in `<git-common-dir>/remuda-worktrees.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeRecord {
    /// Agent / worktree name.
    pub name: String,
    /// Absolute worktree path.
    pub path: String,
    /// Branch created for the worktree (`wt/<name>/…`).
    pub branch: String,
    /// Start-point used at creation.
    pub base: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Catalog {
    #[serde(default)]
    worktrees: Vec<WorktreeRecord>,
}

/// Handle Hub JSON-RPC `worktree.create` / `worktree.list`.
pub fn handle_rpc(repo: &Path, method: &str, params: &Value) -> Option<Result<Value, NodeError>> {
    match method {
        "worktree.list" => Some(list(repo)),
        "worktree.create" => Some(create(repo, params)),
        _ => None,
    }
}

/// True when `method` is a worktree Hub RPC.
pub fn is_worktree_method(method: &str) -> bool {
    matches!(method, "worktree.create" | "worktree.list")
}

fn list(repo: &Path) -> Result<Value, NodeError> {
    let repo_root = repo_root(Some(repo))?;
    let git_common = git_common_dir(&repo_root)?;
    let catalog = load_catalog(&git_common)?;
    Ok(json!({
        "workspaceRoot": repo_root.to_string_lossy(),
        "items": catalog.worktrees,
        "nextCursor": null,
    }))
}

fn create(repo: &Path, params: &Value) -> Result<Value, NodeError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| NodeError::InvalidRequest("worktree.create requires name".into()))?;
    validate_name(name)?;
    let base = params
        .get("base")
        .and_then(Value::as_str)
        .filter(|raw| !raw.is_empty())
        .unwrap_or("main");
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    // `repo` is not accepted from the wire: it would let a caller operate on a
    // different repository than the one this Node registered
    // (`security-review-2.md` M4). The Node's own workspace root is the repo.
    if params.get("repo").is_some() {
        return Err(NodeError::InvalidRequest(
            "worktree.create does not accept repo; the Node workspace root is the repository"
                .into(),
        ));
    }
    let record = create_record(name, base, path.as_deref(), Some(repo))?;
    Ok(json!({
        "name": record.name,
        "path": record.path,
        "branch": record.branch,
        "base": record.base,
        "workspaceRoot": repo_root(Some(repo))?.to_string_lossy(),
    }))
}

fn create_record(
    name: &str,
    base: &str,
    path: Option<&Path>,
    repo: Option<&Path>,
) -> Result<WorktreeRecord, NodeError> {
    let repo_root = repo_root(repo)?;
    let git_common = git_common_dir(&repo_root)?;
    let abs_path = resolve_path(&repo_root, name, path)?;
    let mut catalog = load_catalog(&git_common)?;
    if let Some(existing) = catalog
        .worktrees
        .iter()
        .find(|row| row.name == name)
        .cloned()
    {
        if Path::new(&existing.path).exists() {
            return Ok(existing);
        }
        catalog.worktrees.retain(|row| row.name != name);
    }
    if let Some(listed) = find_listed_worktree(&repo_root, &abs_path)? {
        let record = WorktreeRecord {
            name: name.to_string(),
            path: listed.0,
            branch: listed.1,
            base: base.to_string(),
        };
        upsert(&mut catalog, record.clone());
        save_catalog(&git_common, &catalog)?;
        return Ok(record);
    }
    if let Some(parent) = abs_path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            NodeError::InvalidRequest(format!("create {}: {err}", parent.display()))
        })?;
    }
    let branch = unique_branch(&repo_root, name)?;
    git(
        &repo_root,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            &abs_path.to_string_lossy(),
            base,
        ],
    )?;
    let path = abs_path
        .canonicalize()
        .unwrap_or(abs_path)
        .to_string_lossy()
        .into_owned();
    let record = WorktreeRecord {
        name: name.to_string(),
        path,
        branch,
        base: base.to_string(),
    };
    upsert(&mut catalog, record.clone());
    save_catalog(&git_common, &catalog)?;
    Ok(record)
}

/// Resolve a caller-supplied instance `cwd` against the registered workspace.
///
/// An Instance may run in the workspace root or in any worktree beside it
/// (`<repo>/../remuda-wt/…`), and nowhere else. `None` and a path that is not
/// a directory both fall back to the workspace root, preserving the previous
/// behaviour for callers that omit `cwd` (`security-review-2.md` G5).
pub fn resolve_instance_cwd(
    workspace_root: &Path,
    cwd: Option<&str>,
) -> Result<PathBuf, NodeError> {
    let Some(raw) = cwd.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(workspace_root.to_path_buf());
    };
    let candidate = path_guard::absolutize(workspace_root, Path::new(raw));
    // The worktree root is advisory here: a Node whose workspace has no parent
    // simply has no second root, rather than failing every create.
    let worktrees = path_guard::worktree_root(workspace_root).ok();
    let mut roots: Vec<&Path> = vec![workspace_root];
    if let Some(worktrees) = worktrees.as_deref() {
        roots.push(worktrees);
    }
    let resolved = path_guard::contain(&roots, &candidate).map_err(|error| {
        NodeError::InvalidRequest(format!(
            "cwd must resolve inside the registered workspace or a worktree beside it: {error}"
        ))
    })?;
    if !resolved.is_dir() {
        return Err(NodeError::InvalidRequest(format!(
            "cwd {} is not a directory",
            resolved.display()
        )));
    }
    Ok(resolved)
}

fn validate_name(name: &str) -> Result<(), NodeError> {
    path_guard::safe_segment(name)
        .map_err(|error| NodeError::InvalidRequest(format!("worktree {error}")))
}

fn repo_root(repo: Option<&Path>) -> Result<PathBuf, NodeError> {
    if let Some(repo) = repo {
        return Ok(if repo.is_absolute() {
            repo.to_path_buf()
        } else {
            std::env::current_dir()?.join(repo)
        });
    }
    std::env::current_dir().map_err(NodeError::from)
}

fn git_common_dir(repo: &Path) -> Result<PathBuf, NodeError> {
    let out = git(repo, &["rev-parse", "--git-common-dir"])?;
    let path = PathBuf::from(out);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(repo.join(path))
    }
}

/// Resolve the worktree directory and require it under `<repo>/../remuda-wt`.
///
/// Mirrors `remuda::cmd::worktree::resolve_path`; both use the shared guard so
/// the Hub RPC path cannot be looser than the CLI (`security-review-2.md` M4).
fn resolve_path(repo: &Path, name: &str, path: Option<&Path>) -> Result<PathBuf, NodeError> {
    let root = path_guard::worktree_root(repo)
        .map_err(|error| NodeError::InvalidRequest(format!("worktree {error}")))?;
    let raw = match path {
        Some(p) => path_guard::absolutize(repo, p),
        None => root.join(name),
    };
    path_guard::contain_strict(&[root.as_path()], &raw)
        .map_err(|error| NodeError::InvalidRequest(format!("worktree path rejected: {error}")))
}

fn unique_branch(repo: &Path, name: &str) -> Result<String, NodeError> {
    let preferred = format!("wt/{name}/work");
    if !ref_exists(repo, &preferred)? {
        return Ok(preferred);
    }
    for n in 2..1000 {
        let candidate = format!("wt/{name}/work-{n}");
        if !ref_exists(repo, &candidate)? {
            return Ok(candidate);
        }
    }
    Err(NodeError::InvalidRequest(format!(
        "could not allocate branch wt/{name}/…"
    )))
}

fn ref_exists(repo: &Path, branch: &str) -> Result<bool, NodeError> {
    let spec = format!("refs/heads/{branch}");
    let status = Command::new("git")
        .current_dir(repo)
        .args(["show-ref", "--verify", "--quiet", &spec])
        .status()
        .map_err(NodeError::from)?;
    Ok(status.success())
}

fn find_listed_worktree(repo: &Path, path: &Path) -> Result<Option<(String, String)>, NodeError> {
    let want = path.to_string_lossy().into_owned();
    let out = git(repo, &["worktree", "list", "--porcelain"])?;
    let mut current_path = String::new();
    let mut current_branch = String::new();
    for line in out.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if !current_path.is_empty()
                && (current_path == want
                    || Path::new(&current_path).canonicalize().ok().as_deref()
                        == path.canonicalize().ok().as_deref())
            {
                if current_branch.is_empty() {
                    current_branch = "HEAD".into();
                }
                return Ok(Some((current_path, current_branch)));
            }
            current_path.clear();
            current_branch.clear();
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            current_path = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("branch refs/heads/") {
            current_branch = rest.to_string();
        }
    }
    Ok(None)
}

fn catalog_path(git_common: &Path) -> PathBuf {
    git_common.join("remuda-worktrees.json")
}

fn load_catalog(git_common: &Path) -> Result<Catalog, NodeError> {
    let path = catalog_path(git_common);
    if !path.exists() {
        return Ok(Catalog::default());
    }
    let raw = fs::read_to_string(&path)
        .map_err(|err| NodeError::InvalidRequest(format!("read {}: {err}", path.display())))?;
    serde_json::from_str(&raw)
        .map_err(|err| NodeError::InvalidRequest(format!("parse {}: {err}", path.display())))
}

fn save_catalog(git_common: &Path, catalog: &Catalog) -> Result<(), NodeError> {
    let _ = fs::create_dir_all(git_common);
    let path = catalog_path(git_common);
    let body = serde_json::to_string_pretty(catalog)?;
    fs::write(&path, body)
        .map_err(|err| NodeError::InvalidRequest(format!("write {}: {err}", path.display())))
}

fn upsert(catalog: &mut Catalog, record: WorktreeRecord) {
    catalog.worktrees.retain(|row| row.name != record.name);
    catalog.worktrees.push(record);
}

fn git(repo: &Path, args: &[&str]) -> Result<String, NodeError> {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .map_err(NodeError::from)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(NodeError::InvalidRequest(format!(
            "git {} failed (status {:?}): {}{}",
            args.join(" "),
            output.status.code(),
            stderr.trim(),
            stdout.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_repo() -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        // The worktree root is the repo's sibling; give the repo a parent
        // inside the tempdir so that root lands in the tempdir too.
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]).unwrap();
        // `git init -b main` needs git >= 2.28; set HEAD directly instead.
        git(&root, &["symbolic-ref", "HEAD", "refs/heads/main"]).unwrap();
        git(&root, &["config", "user.email", "test@example.com"]).unwrap();
        git(&root, &["config", "user.name", "test"]).unwrap();
        git(&root, &["commit", "--allow-empty", "-m", "init"]).unwrap();
        (dir, root)
    }

    #[test]
    fn rejects_invalid_name() {
        assert!(validate_name("X").is_err());
        assert!(validate_name("1abc").is_err());
        assert!(validate_name("ok").is_ok());
        assert!(validate_name("x-web").is_ok());
        assert!(validate_name("../escape").is_err());
        assert!(validate_name("a/b").is_err());
    }

    #[test]
    fn create_and_list_round_trip() {
        let (keep, root) = init_repo();
        let path = keep.path().join("remuda-wt").join("agent1");
        let created = handle_rpc(
            &root,
            "worktree.create",
            &json!({ "name": "agent1", "base": "main", "path": path }),
        )
        .expect("handled")
        .expect("create");
        assert_eq!(created["name"], "agent1");
        assert!(
            created["branch"]
                .as_str()
                .unwrap()
                .starts_with("wt/agent1/")
        );
        assert!(
            Path::new(created["path"].as_str().unwrap())
                .join(".git")
                .exists()
        );
        let again = handle_rpc(
            &root,
            "worktree.create",
            &json!({ "name": "agent1", "base": "main", "path": path }),
        )
        .expect("handled")
        .expect("reuse");
        assert_eq!(again["path"], created["path"]);
        let listed = handle_rpc(&root, "worktree.list", &json!({}))
            .expect("handled")
            .expect("list");
        assert_eq!(listed["items"].as_array().unwrap().len(), 1);
        assert_eq!(listed["items"][0]["name"], "agent1");
    }

    #[test]
    fn default_path_lands_in_the_worktree_root() {
        let (keep, root) = init_repo();
        let created = handle_rpc(&root, "worktree.create", &json!({ "name": "agent2" }))
            .expect("handled")
            .expect("create");
        let expected = keep.path().canonicalize().unwrap().join("remuda-wt");
        let got = created["path"].as_str().unwrap();
        assert!(
            Path::new(got).starts_with(&expected),
            "{got} is not under {}",
            expected.display()
        );
    }

    #[test]
    fn rejects_path_escaping_the_worktree_root() {
        let (keep, root) = init_repo();
        let outside = keep.path().join("evil");
        let err = handle_rpc(
            &root,
            "worktree.create",
            &json!({ "name": "agent3", "path": outside }),
        )
        .expect("handled")
        .expect_err("absolute path outside the root must be rejected");
        assert!(
            err.to_string().contains("worktree path rejected"),
            "unexpected error: {err}"
        );
        assert!(!outside.exists(), "the rejected directory must not exist");

        // The review's PoC spellings.
        for bad in [
            json!("/home/op/.config/systemd/user"),
            json!("../../../../tmp/evil"),
            json!("/tmp"),
        ] {
            assert!(
                handle_rpc(
                    &root,
                    "worktree.create",
                    &json!({ "name": "agent4", "path": bad }),
                )
                .expect("handled")
                .is_err(),
                "path {bad} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_repo_override() {
        let (keep, root) = init_repo();
        let (_other_keep, other) = init_repo();
        let err = handle_rpc(
            &root,
            "worktree.create",
            &json!({ "name": "agent5", "repo": other }),
        )
        .expect("handled")
        .expect_err("repo override must be rejected");
        assert!(
            err.to_string().contains("does not accept repo"),
            "unexpected error: {err}"
        );
        // Nothing was created in either repository.
        assert!(!keep.path().join("remuda-wt").exists());
    }

    #[test]
    fn instance_cwd_defaults_to_the_workspace_root() {
        let (keep, root) = init_repo();
        let resolved = resolve_instance_cwd(&root, None).expect("default");
        assert_eq!(resolved, root.canonicalize().unwrap());
        assert_eq!(resolve_instance_cwd(&root, Some("  ")).unwrap(), resolved);
        drop(keep);
    }

    #[test]
    fn instance_cwd_accepts_the_workspace_and_its_worktrees() {
        let (keep, root) = init_repo();
        let created = handle_rpc(&root, "worktree.create", &json!({ "name": "agent1" }))
            .expect("handled")
            .expect("create");
        let worktree = created["path"].as_str().unwrap();

        // The workspace root itself, a subdirectory of it, and a worktree.
        assert!(resolve_instance_cwd(&root, Some(&root.to_string_lossy())).is_ok());
        let sub = root.join("crates");
        fs::create_dir_all(&sub).unwrap();
        assert!(resolve_instance_cwd(&root, Some(&sub.to_string_lossy())).is_ok());
        let resolved = resolve_instance_cwd(&root, Some(worktree)).expect("worktree cwd");
        assert_eq!(resolved, Path::new(worktree).canonicalize().unwrap());
        drop(keep);
    }

    #[test]
    fn instance_cwd_rejects_paths_outside_the_workspace() {
        let (keep, root) = init_repo();
        // The review's PoC values, plus traversal and a non-directory.
        for bad in ["/", "/etc", "/home", "../..", "/etc/passwd"] {
            let err =
                resolve_instance_cwd(&root, Some(bad)).expect_err("cwd {bad} must be rejected");
            let message = err.to_string();
            assert!(
                message.contains("must resolve inside") || message.contains("is not a directory"),
                "cwd {bad}: unexpected error {message}"
            );
        }
        // A sibling of the workspace that is not a worktree.
        let sibling = keep.path().join("elsewhere");
        fs::create_dir_all(&sibling).unwrap();
        assert!(resolve_instance_cwd(&root, Some(&sibling.to_string_lossy())).is_err());
    }

    #[test]
    fn instance_cwd_rejects_a_symlink_escaping_the_workspace() {
        let (keep, root) = init_repo();
        let outside = keep.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
            let escape = root.join("escape");
            assert!(
                resolve_instance_cwd(&root, Some(&escape.to_string_lossy())).is_err(),
                "a symlink out of the workspace must not be accepted"
            );
        }
    }
}
