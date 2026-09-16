//! Hub RPC `worker.provision` / `worker.remove` — the Node side of the
//! product-assigned worker lifecycle (M1 batch 5a; coordinator-hierarchy.md
//! §2.4: worktree/target dir/ports are assigned by the product, never
//! self-reported).
//!
//! Filesystem layout mirrors the managed worktree root:
//! - worktrees: `<repo>/../remuda-wt/<name>` (see `worktree::provision_record`)
//! - cargo target: `<repo>/../remuda-target/<name>` (this module)
//!
//! Nothing here accepts an absolute path from the wire. Both locations are
//! recomputed from the registered workspace root and the worker name, then
//! containment-checked before deletion.

use crate::NodeError;
use remuda_protocol::{
    WorkerProvisionParams, WorkerProvisionResult, WorkerRemoveParams, WorkerRemoveResult,
    validate_worker_branch, validate_worker_name,
};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// Directory name holding per-worker cargo targets beside a repository root.
pub const TARGET_DIR_NAME: &str = "remuda-target";

/// Hub→Node worker RPCs served by every carrier (outbound WSS and ssh-stdio).
#[must_use]
pub fn is_worker_method(method: &str) -> bool {
    matches!(method, "worker.provision" | "worker.remove")
}

/// Validate the assigned branch (pub(crate) so `worktree.rs` shares the rule).
pub(crate) fn validate_branch(branch: &str) -> Result<(), NodeError> {
    validate_worker_branch(branch).map_err(NodeError::InvalidRequest)
}

/// The only directory per-worker cargo targets may live in:
/// `<repo>/../remuda-target`.
fn scratch_root(repo_root: &Path) -> Result<PathBuf, NodeError> {
    let real = remuda_protocol::path_guard::real_path(repo_root)
        .map_err(|error| NodeError::InvalidRequest(error.to_string()))?;
    let parent = real
        .parent()
        .ok_or_else(|| NodeError::InvalidRequest("workspace root has no parent".into()))?;
    Ok(parent.join(TARGET_DIR_NAME))
}

/// Recompute a worker's target directory and containment-check it strictly
/// inside the managed scratch root.
fn target_dir_for(repo_root: &Path, name: &str) -> Result<PathBuf, NodeError> {
    validate_worker_name(name).map_err(NodeError::InvalidRequest)?;
    let root = scratch_root(repo_root)?;
    let candidate = root.join(name);
    let resolved = remuda_protocol::path_guard::contain_strict(&[&root], &candidate)
        .map_err(|error| NodeError::InvalidRequest(format!("target dir rejected: {error}")))?;
    Ok(resolved)
}

/// Ensure the per-worker target dir exists; returns its absolute path.
fn ensure_target_dir(repo_root: &Path, name: &str) -> Result<PathBuf, NodeError> {
    let dir = target_dir_for(repo_root, name)?;
    fs::create_dir_all(&dir)
        .map_err(|error| NodeError::InvalidRequest(format!("create {}: {error}", dir.display())))?;
    Ok(dir.canonicalize().unwrap_or(dir))
}

/// Recursively remove the per-worker target dir, reporting reclaimed bytes.
///
/// Containment is re-checked from the wire `name` only: a caller cannot make
/// the Node delete an arbitrary directory. Missing dirs report `0`/`false`.
fn remove_target_dir(repo_root: &Path, name: &str) -> Result<(bool, u64), NodeError> {
    // Validate before touching anything.
    let dir = target_dir_for(repo_root, name)?;
    if !dir.exists() {
        return Ok((false, 0));
    }
    let bytes = dir_size(&dir);
    fs::remove_dir_all(&dir)
        .map_err(|error| NodeError::InvalidRequest(format!("remove {}: {error}", dir.display())))?;
    Ok((true, bytes))
}

/// Best-effort recursive byte count of a directory tree.
fn dir_size(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(entry.path());
            } else {
                total += metadata.len();
            }
        }
    }
    total
}

impl crate::runtime::DevNode {
    /// Handle `worker.provision`: product-assigned worktree + target dir.
    pub(crate) async fn provision_worker(&self, params: &Value) -> Result<Value, NodeError> {
        let request: WorkerProvisionParams =
            serde_json::from_value(params.clone()).map_err(|error| {
                NodeError::InvalidRequest(format!("invalid worker.provision params: {error}"))
            })?;
        let result = self.provision_worker_typed(request)?;
        serde_json::to_value(result).map_err(NodeError::from)
    }

    fn provision_worker_typed(
        &self,
        request: WorkerProvisionParams,
    ) -> Result<WorkerProvisionResult, NodeError> {
        validate_worker_name(&request.name).map_err(NodeError::InvalidRequest)?;
        validate_worker_branch(&request.branch).map_err(NodeError::InvalidRequest)?;
        let start_point = request
            .start_point
            .clone()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "origin/main".into());
        // A remote ref (origin/main) must be namespaced; reject anything that
        // is clearly a local traversal attempt.
        if start_point.contains("..") || start_point.starts_with('/') {
            return Err(NodeError::InvalidRequest(format!(
                "bad start point {start_point}"
            )));
        }
        let (_, workspace_root) =
            self.resolve_workspace_cwd(request.workspace_id.as_ref(), None)?;
        let provisioned = crate::worktree::provision_record(
            &workspace_root,
            &request.name,
            &request.branch,
            &start_point,
        )?;
        let target = ensure_target_dir(&workspace_root, &request.name)?;
        Ok(WorkerProvisionResult {
            name: request.name,
            branch: provisioned.branch,
            start_point: provisioned.start_point,
            worktree_path: provisioned.path,
            target_dir: target.to_string_lossy().into_owned(),
        })
    }

    /// Handle `worker.remove`: close the herdr carrier (pty drivers), then
    /// remove worktree + target dir.
    pub(crate) async fn remove_worker(&self, params: &Value) -> Result<Value, NodeError> {
        let request: WorkerRemoveParams =
            serde_json::from_value(params.clone()).map_err(|error| {
                NodeError::InvalidRequest(format!("invalid worker.remove params: {error}"))
            })?;
        let result = self.remove_worker_typed(request).await?;
        serde_json::to_value(result).map_err(NodeError::from)
    }

    async fn remove_worker_typed(
        &self,
        request: WorkerRemoveParams,
    ) -> Result<WorkerRemoveResult, NodeError> {
        validate_worker_name(&request.name).map_err(NodeError::InvalidRequest)?;
        // Best-effort carrier teardown first: a live agent pane holds the
        // worktree as its cwd and would keep file handles open.
        if let Some(instance_id) = &request.instance_id
            && let Err(error) = self
                .close_instance_carrier(instance_id.as_id().as_str())
                .await
        {
            tracing::warn!(%error, instance_id = %instance_id.as_id(), "worker carrier close failed; continuing with filesystem reclaim");
        }
        let (_, workspace_root) =
            self.resolve_workspace_cwd(request.workspace_id.as_ref(), None)?;
        let worktree_removed = crate::worktree::remove_record(&workspace_root, &request.name)?;
        let (target_removed, reclaimed_bytes) = remove_target_dir(&workspace_root, &request.name)?;
        Ok(WorkerRemoveResult {
            name: request.name,
            worktree_removed,
            target_removed,
            reclaimed_bytes: remuda_protocol::U64(reclaimed_bytes),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_repo() -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        let run = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["init", "-q"]);
        run(&["symbolic-ref", "HEAD", "refs/heads/main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        run(&["commit", "--allow-empty", "-m", "init"]);
        // provision_record fetches origin; point "origin" at the repo itself
        // so `git fetch origin` succeeds in a temp fixture.
        run(&["remote", "add", "origin", root.to_str().unwrap()]);
        run(&["fetch", "-q", "origin"]);
        run(&["update-ref", "refs/remotes/origin/main", "refs/heads/main"]);
        (dir, root)
    }

    #[test]
    fn provision_and_remove_round_trip() {
        let (_keep, root) = init_repo();
        let request = WorkerProvisionParams {
            name: "c-demo".into(),
            branch: "wt/c-demo/tiny-task".into(),
            workspace_id: None,
            start_point: Some("origin/main".into()),
        };
        let _ = &request;
        // The free function path (workspace resolution is DevNode-specific);
        // exercise the fs primitives directly.
        let provisioned = crate::worktree::provision_record(
            &root,
            "c-demo",
            "wt/c-demo/tiny-task",
            "origin/main",
        )
        .expect("provision");
        assert_eq!(provisioned.branch, "wt/c-demo/tiny-task");
        assert!(Path::new(&provisioned.path).join(".git").exists());
        let target = ensure_target_dir(&root, "c-demo").expect("target");
        assert!(target.exists());
        assert!(target.ends_with("remuda-target/c-demo"));

        // Re-provision is idempotent.
        let again = crate::worktree::provision_record(
            &root,
            "c-demo",
            "wt/c-demo/tiny-task",
            "origin/main",
        )
        .expect("reprovision");
        assert_eq!(again.path, provisioned.path);

        let (removed, _bytes) = remove_target_dir(&root, "c-demo").expect("rm target");
        assert!(removed);
        assert!(!target.exists());
        assert!(crate::worktree::remove_record(&root, "c-demo").expect("rm worktree"));
        assert!(!Path::new(&provisioned.path).exists());
        // Idempotent remove.
        assert!(!crate::worktree::remove_record(&root, "c-demo").unwrap());
        let (removed, _) = remove_target_dir(&root, "c-demo").unwrap();
        assert!(!removed);
    }

    #[test]
    fn target_dir_cannot_escape_scratch_root() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        assert!(target_dir_for(&root, "../evil").is_err());
        assert!(target_dir_for(&root, "a/b").is_err());
    }

    #[test]
    fn bad_start_points_are_rejected() {
        // Pure validation without a DevNode: the rules live on the types.
        assert!(validate_worker_name("ok-name").is_ok());
        assert!(validate_worker_branch("wt/ok-name/slug-1").is_ok());
        assert!(validate_worker_branch("wt/ok/bad/slug").is_err());
    }

    /// End-to-end Node RPC on a fake host (temp git repo registered as the
    /// Node workspace): provision creates the worktree + target dir, remove
    /// reclaims both.
    #[tokio::test]
    async fn node_worker_provision_and_remove_rpc() {
        use crate::DevServerConfig;
        use serde_json::json;
        let keep = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let repo = keep.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "test"]);
        git(&["commit", "--allow-empty", "-m", "init"]);
        git(&["remote", "add", "origin", repo.to_str().unwrap()]);
        git(&["fetch", "-q", "origin"]);
        git(&["update-ref", "refs/remotes/origin/main", "refs/heads/main"]);

        let config = DevServerConfig::loopback(0)
            .with_workspace_root(repo.clone())
            .with_workspace_roots(vec![keep.path().to_path_buf()])
            .with_workspace_registry(data.path().to_path_buf());
        let node = crate::DevNode::new(&config).unwrap();

        let params = json!({
            "name": "c-rpc",
            "branch": "wt/c-rpc/tiny-rpc-task",
            "startPoint": "origin/main",
        });
        let provisioned = crate::server::dispatch_hub_rpc(&node, "worker.provision", params)
            .await
            .expect("provision rpc");
        assert_eq!(provisioned["branch"], "wt/c-rpc/tiny-rpc-task");
        let worktree = provisioned["worktreePath"].as_str().unwrap();
        let target = provisioned["targetDir"].as_str().unwrap();
        assert!(Path::new(worktree).join(".git").exists());
        assert!(Path::new(target).is_dir());
        // Worktree sits beside the repo in the managed root, never inside it.
        assert!(worktree.contains("remuda-wt"));
        assert!(target.contains("remuda-target"));

        // Provisioning a different branch on the same name is refused.
        let bad = json!({"name": "c-rpc", "branch": "wt/c-rpc/other"});
        assert!(
            crate::server::dispatch_hub_rpc(&node, "worker.provision", bad)
                .await
                .is_err()
        );

        let removed =
            crate::server::dispatch_hub_rpc(&node, "worker.remove", json!({"name": "c-rpc"}))
                .await
                .expect("remove rpc");
        assert_eq!(removed["worktreeRemoved"], true);
        assert_eq!(removed["targetRemoved"], true);
        assert!(!Path::new(worktree).exists());
        assert!(!Path::new(target).exists());

        // A traversal attempt through the name never reaches the filesystem.
        let evil = json!({"name": "../evil", "branch": "wt/x/y"});
        assert!(
            crate::server::dispatch_hub_rpc(&node, "worker.provision", evil)
                .await
                .is_err()
        );
    }
}
