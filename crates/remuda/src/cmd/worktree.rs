//! `remuda worktree create` — local `git worktree add -b wt/<name>/…`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// `remuda worktree` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum WorktreeCommand {
    /// Create a git worktree (`git worktree add -b wt/<name>/…`).
    Create {
        /// Worktree / agent name (`[a-z][a-z0-9_-]{0,31}`).
        name: String,
        /// Start-point (branch, tag, or commit). Defaults to `main`.
        #[arg(long, default_value = "main")]
        base: String,
        /// Directory for the worktree. Defaults to `../remuda-wt/<name>`
        /// relative to the repository root.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Git repository (default: current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
}

/// Record stored in `<git-common-dir>/remuda-worktrees.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorktreeRecord {
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

/// Run a `remuda worktree` subcommand.
pub(crate) fn run(command: WorktreeCommand) -> Result<()> {
    match command {
        WorktreeCommand::Create {
            name,
            base,
            path,
            repo,
        } => {
            let record = create(&name, &base, path.as_deref(), repo.as_deref())?;
            super::hub_client::print_json(&json!({
                "name": record.name,
                "path": record.path,
                "branch": record.branch,
                "base": record.base,
            }))
        }
    }
}

/// Create (or reuse) a worktree named `name`.
pub(crate) fn create(
    name: &str,
    base: &str,
    path: Option<&Path>,
    repo: Option<&Path>,
) -> Result<WorktreeRecord> {
    validate_name(name)?;
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
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
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

/// Look up a recorded worktree, creating one with defaults when missing.
pub(crate) fn ensure(name: &str, repo: Option<&Path>) -> Result<WorktreeRecord> {
    if let Ok(found) = lookup(name, repo)
        && Path::new(&found.path).exists()
    {
        return Ok(found);
    }
    create(name, "main", None, repo)
}

/// Look up a recorded worktree by name.
pub(crate) fn lookup(name: &str, repo: Option<&Path>) -> Result<WorktreeRecord> {
    validate_name(name)?;
    let repo_root = repo_root(repo)?;
    let git_common = git_common_dir(&repo_root)?;
    let catalog = load_catalog(&git_common)?;
    catalog
        .worktrees
        .into_iter()
        .find(|row| row.name == name)
        .ok_or_else(|| {
            anyhow::anyhow!("unknown worktree {name}; run remuda worktree create {name}")
        })
}

pub(crate) fn validate_name(name: &str) -> Result<()> {
    let valid = (1..=32).contains(&name.len())
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !valid {
        bail!("worktree name must match [a-z][a-z0-9_-]{{0,31}}, got {name:?}");
    }
    Ok(())
}

fn repo_root(repo: Option<&Path>) -> Result<PathBuf> {
    if let Some(repo) = repo {
        return Ok(if repo.is_absolute() {
            repo.to_path_buf()
        } else {
            std::env::current_dir()?.join(repo)
        });
    }
    std::env::current_dir().context("current directory")
}

fn git_common_dir(repo: &Path) -> Result<PathBuf> {
    let out = git(repo, &["rev-parse", "--git-common-dir"])?;
    let path = PathBuf::from(out);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(repo.join(path))
    }
}

fn resolve_path(repo: &Path, name: &str, path: Option<&Path>) -> Result<PathBuf> {
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

fn unique_branch(repo: &Path, name: &str) -> Result<String> {
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
    bail!("could not allocate branch wt/{name}/…");
}

fn ref_exists(repo: &Path, branch: &str) -> Result<bool> {
    let spec = format!("refs/heads/{branch}");
    let status = Command::new("git")
        .current_dir(repo)
        .args(["show-ref", "--verify", "--quiet", &spec])
        .status()
        .context("git show-ref")?;
    Ok(status.success())
}

fn find_listed_worktree(repo: &Path, path: &Path) -> Result<Option<(String, String)>> {
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

fn load_catalog(git_common: &Path) -> Result<Catalog> {
    let path = catalog_path(git_common);
    if !path.exists() {
        return Ok(Catalog::default());
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

fn save_catalog(git_common: &Path, catalog: &Catalog) -> Result<()> {
    fs::create_dir_all(git_common).ok();
    let path = catalog_path(git_common);
    let body = serde_json::to_string_pretty(catalog)?;
    fs::write(&path, body).with_context(|| format!("write {}", path.display()))
}

fn upsert(catalog: &mut Catalog, record: WorktreeRecord) {
    catalog.worktrees.retain(|row| row.name != record.name);
    catalog.worktrees.push(record);
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "git {} failed (status {:?}): {}{}",
            args.join(" "),
            output.status.code(),
            stderr.trim(),
            stdout.trim()
        );
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
        assert!(validate_name("x-acpwire").is_ok());
    }

    #[test]
    fn create_adds_branch_and_catalog() {
        let (_keep, root) = init_repo();
        let path = root.join("wt-agent");
        let record = create("agent1", "main", Some(&path), Some(&root)).expect("create");
        assert_eq!(record.name, "agent1");
        assert!(record.branch.starts_with("wt/agent1/"));
        assert!(Path::new(&record.path).join(".git").exists());
        let again = create("agent1", "main", Some(&path), Some(&root)).expect("reuse");
        assert_eq!(again.path, record.path);
        assert_eq!(again.branch, record.branch);
        let found = lookup("agent1", Some(&root)).expect("lookup");
        assert_eq!(found.branch, record.branch);
    }
}
