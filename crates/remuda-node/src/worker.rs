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

/// Deadline for the whole `worker.remove` reclaim (git worktree removal + the
/// recursive cargo-target delete).
///
/// Must stay comfortably under the Hub's own `WORKTREE_RPC_TIMEOUT` of 60s
/// (remuda-hub/src/http.rs), because a deadline at or above it is worse than no
/// deadline: the Hub fails the retire first, so the honest error can never reach
/// it, and a reclaim that then lands between the two windows removes the
/// worktree *after* the Hub already gave up — leaving the worker row un-retired
/// against reclaimed disk, which is the one outcome nobody can reconcile. 30s
/// leaves half the Hub's window for the reply to travel and be recorded.
const RECLAIM_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Deadline for the best-effort carrier teardown that precedes the reclaim.
///
/// Closing a pty-backed worker's Herdr carrier is several round-trips to the
/// Herdr server (`owned_workspace_present`, `interrupt_agent`, `owned_panes`,
/// then a re-check per close), and none of them is bounded. This step is
/// already best-effort — a failure only warns and the reclaim continues — so
/// waiting on a wedged Herdr forever would be the same park by another door.
///
/// Sized with `RECLAIM_DEADLINE` so the worst case still fits inside the Hub's
/// 60s window: 10 + 30 (`RECLAIM_DEADLINE`) + slack to answer = 40s, leaving
/// the Hub a fifth of its budget to record the reply.
const CARRIER_CLOSE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

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

/// Whether this Node has a cargo target dir for `name`. Containment-checked the
/// same way the removal is, so a probe cannot look outside the scratch root.
fn target_dir_present(repo_root: &Path, name: &str) -> Result<bool, NodeError> {
    Ok(target_dir_for(repo_root, name)?.exists())
}

/// A provision request that has passed validation but has not touched git or
/// the filesystem yet — the part safe to hand to a blocking thread.
struct ValidatedProvision {
    name: String,
    branch: String,
    start_point: String,
    workspace_root: PathBuf,
}

/// The blocking half of `worker.provision`: create (or reuse) the worktree and
/// the per-worker target dir.
///
/// Free of `&self` so it can run on a blocking thread; it only needs the
/// already-resolved workspace root.
fn provision_validated(request: ValidatedProvision) -> Result<WorkerProvisionResult, NodeError> {
    let provisioned = crate::worktree::provision_record(
        &request.workspace_root,
        &request.name,
        &request.branch,
        &request.start_point,
    )?;
    let target = ensure_target_dir(&request.workspace_root, &request.name)?;
    Ok(WorkerProvisionResult {
        name: request.name,
        branch: provisioned.branch,
        start_point: provisioned.start_point,
        worktree_path: provisioned.path,
        target_dir: target.to_string_lossy().into_owned(),
    })
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
    /// Validate the request, then run the blocking provisioning under the
    /// carrier's in-flight cap.
    ///
    /// Validation stays here so a malformed request is refused without taking a
    /// permit or a blocking thread. The body is synchronous and its `git fetch`
    /// is explicitly unbounded (see `worktree::git_fetch`), so awaiting it
    /// directly — even from a spawned task — would occupy a runtime worker for
    /// as long as the fetch takes.
    pub(crate) async fn provision_worker_capped(&self, params: &Value) -> Result<Value, NodeError> {
        let request: WorkerProvisionParams =
            serde_json::from_value(params.clone()).map_err(|error| {
                NodeError::InvalidRequest(format!("invalid worker.provision params: {error}"))
            })?;
        let validated = self.validate_provision(request)?;
        let result = crate::gate::run_long_method(move || provision_validated(validated)).await?;
        serde_json::to_value(result).map_err(NodeError::from)
    }

    /// Everything in a provision that does not touch git or the filesystem.
    fn validate_provision(
        &self,
        request: WorkerProvisionParams,
    ) -> Result<ValidatedProvision, NodeError> {
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
        Ok(ValidatedProvision {
            name: request.name,
            branch: request.branch,
            start_point,
            workspace_root,
        })
    }

    /// Handle `worker.remove`: close the herdr carrier (pty drivers), then
    /// remove worktree + target dir.
    pub(crate) async fn remove_worker(&self, params: &Value) -> Result<Value, NodeError> {
        let request: WorkerRemoveParams =
            serde_json::from_value(params.clone()).map_err(|error| {
                NodeError::InvalidRequest(format!("invalid worker.remove params: {error}"))
            })?;
        self.remove_worker_typed(request).await
    }

    async fn remove_worker_typed(&self, request: WorkerRemoveParams) -> Result<Value, NodeError> {
        validate_worker_name(&request.name).map_err(NodeError::InvalidRequest)?;
        // Retiring a worker whose instance row this Node has lost must not wait
        // on that instance — that was the resume-over-a-dead-instance park. It
        // must still reclaim, though: the worktree and the cargo target are this
        // Node's disk and nothing else frees them. So an unknown instance only
        // skips the machinery that needs the row, never the reclaim.
        let named_instance = request.instance_id.as_ref();
        let known_instance = match named_instance {
            Some(instance_id) => match self.get_instance(instance_id) {
                Ok(instance) => Some(instance),
                // Only a genuinely absent row is "unknown"; a poisoned store or
                // any other fault is still a fault.
                Err(NodeError::NotFound { .. }) => None,
                Err(error) => return Err(error),
            },
            None => None,
        };
        // The carrier teardown needs a live row to find the pty resource;
        // without one there is nothing to close.
        if let (Some(instance_id), Some(_)) = (named_instance, known_instance.as_ref())
            && let Err(error) = self.close_worker_carrier(instance_id).await
        {
            tracing::warn!(%error, instance_id = %instance_id.as_id(), "worker carrier close failed; continuing with filesystem reclaim");
        }
        let (_, workspace_root) =
            self.resolve_workspace_cwd(request.workspace_id.as_ref(), None)?;
        // A retire that *named* an instance this Node has lost, and that has
        // nothing left on disk, is a prompt not-found — there is no row to touch
        // and nothing to free. Gated on the instance being named because that is
        // the honest claim being answered ("I do not have this instance"); a
        // retire with no instance at all is the idempotent form and still
        // reports what it reclaimed.
        if let Some(instance_id) = named_instance
            && known_instance.is_none()
        {
            let name = request.name.clone();
            let probe_root = workspace_root.clone();
            let present = tokio::task::spawn_blocking(move || {
                Ok::<_, NodeError>((
                    crate::worktree::has_record(&probe_root, &name)?,
                    target_dir_present(&probe_root, &name)?,
                ))
            })
            .await
            .map_err(|error| NodeError::Driver(format!("worker.remove probe join: {error}")))??;
            if !present.0 && !present.1 {
                tracing::debug!(instance_id = %instance_id.as_id(), worker = %request.name, "retire of an instance this Node does not know, with nothing to reclaim");
                return Err(NodeError::NotFound {
                    entity: "worker",
                    id: request.name,
                });
            }
        }
        // The reclaim shells out to `git worktree remove --force` and then does
        // a recursive std fs remove of the whole cargo target — both blocking,
        // both unbounded. Run them on a blocking thread under an explicit
        // deadline so a slow or stuck git can never own an async task or park
        // the carrier loop (a stuck git here is exactly what froze the ssh-stdio
        // Node for good).
        //
        // t-pool: when the worktree still carries leases, `remove_record`
        // retains it and we skip the target-dir delete too (it is the shared
        // warm scratch for that name). The Hub side additionally guards
        // retire/delete before this RPC is ever sent; this is the on-disk
        // root-cause guard for any caller that reaches the Node directly.
        let name = request.name.clone();
        let reclaim = tokio::time::timeout(
            RECLAIM_DEADLINE,
            tokio::task::spawn_blocking(move || {
                let worktree = crate::worktree::remove_record(&workspace_root, &name)?;
                let (target_removed, reclaimed_bytes) = match &worktree {
                    crate::worktree::ReclaimOutcome::Retained { .. } => (false, 0u64),
                    crate::worktree::ReclaimOutcome::Removed(_) => {
                        remove_target_dir(&workspace_root, &name)?
                    }
                };
                Ok::<_, NodeError>((worktree, target_removed, reclaimed_bytes))
            }),
        )
        .await
        .map_err(|_| {
            NodeError::InvalidRequest(format!(
                "worker.remove reclaim exceeded {}s; git or filesystem removal is stuck",
                RECLAIM_DEADLINE.as_secs()
            ))
        })?
        .map_err(|error| NodeError::Driver(format!("worker.remove reclaim join: {error}")))??;
        let (worktree_reclaim, target_removed, reclaimed_bytes) = reclaim;
        let worker_name = request.name.clone();
        let mut result = serde_json::to_value(WorkerRemoveResult {
            name: worker_name.clone(),
            worktree_removed: worktree_reclaim.removed(),
            target_removed,
            reclaimed_bytes: remuda_protocol::U64(reclaimed_bytes),
        })?;
        if let crate::worktree::ReclaimOutcome::Retained { path, refcount } = &worktree_reclaim {
            tracing::warn!(worker = %worker_name, refcount, "worker.remove retained a leased worktree instead of deleting it");
            result["retained"] = serde_json::json!(true);
            result["refcount"] = serde_json::json!(refcount);
            result["worktreePath"] = serde_json::json!(path);
        }
        Ok(result)
    }

    /// Best-effort teardown of a worker's pty carrier, under its own deadline.
    ///
    /// Split out so the caller decides whether a failed or late close is fatal
    /// (it never is) and so the bound is visible next to the reclaim's.
    async fn close_worker_carrier(
        &self,
        instance_id: &remuda_protocol::InstanceId,
    ) -> Result<(), NodeError> {
        match tokio::time::timeout(
            CARRIER_CLOSE_DEADLINE,
            self.close_instance_carrier(instance_id.as_id().as_str()),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(NodeError::Driver(format!(
                "worker carrier close exceeded its {}s deadline",
                CARRIER_CLOSE_DEADLINE.as_secs()
            ))),
        }
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
        assert!(
            crate::worktree::remove_record(&root, "c-demo")
                .expect("rm worktree")
                .removed()
        );
        assert!(!Path::new(&provisioned.path).exists());
        // Idempotent remove.
        assert!(
            !crate::worktree::remove_record(&root, "c-demo")
                .unwrap()
                .removed()
        );
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
