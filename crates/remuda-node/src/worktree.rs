//! Git worktree create/list used by Hub `worktree.create` / `worktree.list`.

use crate::NodeError;
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
    let repo_override = params
        .get("repo")
        .and_then(Value::as_str)
        .filter(|raw| !raw.is_empty())
        .map(PathBuf::from);
    let record = create_record(
        name,
        base,
        path.as_deref(),
        repo_override.as_deref().or(Some(repo)),
    )?;
    Ok(json!({
        "name": record.name,
        "path": record.path,
        "branch": record.branch,
        "base": record.base,
        "workspaceRoot": repo_root(repo_override.as_deref().or(Some(repo)))?.to_string_lossy(),
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

fn validate_name(name: &str) -> Result<(), NodeError> {
    let valid = (1..=32).contains(&name.len())
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !valid {
        return Err(NodeError::InvalidRequest(format!(
            "worktree name must match [a-z][a-z0-9_-]{{0,31}}, got {name:?}"
        )));
    }
    Ok(())
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

fn resolve_path(repo: &Path, name: &str, path: Option<&Path>) -> Result<PathBuf, NodeError> {
    let raw = match path {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from("..").join("remuda-wt").join(name),
    };
    Ok(if raw.is_absolute() {
        raw
    } else {
        repo.join(raw)
    })
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
        let root = dir.path().to_path_buf();
        git(&root, &["init", "-b", "main"]).unwrap();
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
    }

    #[test]
    fn create_and_list_round_trip() {
        let (_keep, root) = init_repo();
        let path = root.join("wt-agent");
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
}
