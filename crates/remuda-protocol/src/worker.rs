//! Worker roster: the durable record of one dispatched T3 worker; design
//! coordinator-hierarchy.md §2.2 (field ④, the productised `relay-workers.tsv`).
//!
//! Every resource a worker occupies — name, branch, worktree, target dir, port
//! block — is assigned by the Hub (§2.4), never self-reported by the worker.
//! The same document carries the worker's lifecycle state so a coordinator
//! (human or agent) can rebuild dispatch→watch→retire from the Hub alone.

use crate::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Initial roster state: resources provisioned, agent not yet observed working.
pub const WORKER_STATE_DISPATCHED: &str = "dispatched";
/// Agent launched and the brief delivered.
pub const WORKER_STATE_WORKING: &str = "working";
/// Worker reported `DONE <sha>`; the sha is carried here (still a claim, not a
/// gate — design §7 I1).
pub const WORKER_STATE_DONE: &str = "done";
/// Worker reported `BLOCKED <reason>`.
pub const WORKER_STATE_BLOCKED: &str = "blocked";
/// Herdr tab closed, worktree and target dir reclaimed.
pub const WORKER_STATE_RETIRED: &str = "retired";

/// Lifecycle of a dispatched worker; design §1.1 goal 6.
///
/// Wire shape is adjacently tagged: `{"state":"done","sha":"…"}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum WorkerState {
    /// Resources provisioned, agent not yet confirmed working.
    #[default]
    Dispatched,
    /// Agent launched and working its brief.
    Working,
    /// Worker replied `DONE <sha>`; the sha is a claim until `land` (I1).
    Done {
        /// Claimed landed sha.
        sha: String,
    },
    /// Worker replied `BLOCKED <reason>`.
    Blocked {
        /// Machine-and-human-readable block reason.
        reason: String,
    },
    /// Tab closed and worktree/target dir reclaimed through the Node.
    Retired,
}

impl WorkerState {
    /// Canonical wire discriminant.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Dispatched => WORKER_STATE_DISPATCHED,
            Self::Working => WORKER_STATE_WORKING,
            Self::Done { .. } => WORKER_STATE_DONE,
            Self::Blocked { .. } => WORKER_STATE_BLOCKED,
            Self::Retired => WORKER_STATE_RETIRED,
        }
    }

    /// True when the worker is still consuming host resources. Retire refuses
    /// this without `--force`.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::Retired)
    }

    /// True for the `working` state.
    #[must_use]
    pub fn is_working(&self) -> bool {
        matches!(self, Self::Working)
    }

    /// Parse a state update from wire form. `done` requires `sha`, `blocked`
    /// requires `reason`; `dispatched` may not be restored by a state patch.
    pub fn from_update(
        kind: &str,
        sha: Option<&str>,
        reason: Option<&str>,
    ) -> Result<Self, String> {
        match kind {
            WORKER_STATE_WORKING => Ok(Self::Working),
            WORKER_STATE_DONE => {
                let sha = sha
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "done requires a sha".to_string())?;
                Ok(Self::Done {
                    sha: sha.to_string(),
                })
            }
            WORKER_STATE_BLOCKED => {
                let reason = reason
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "blocked requires a reason".to_string())?;
                Ok(Self::Blocked {
                    reason: reason.to_string(),
                })
            }
            other => Err(format!("invalid worker state {other}")),
        }
    }
}

/// One row of the per-project worker roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRoster {
    /// `meta`; the id is `wkr_…`.
    #[serde(flatten)]
    pub meta: EntityMeta<WorkerRosterId>,
    /// Owning project.
    pub project_id: ProjectId,
    /// Human-facing worker name; unique among *active* rows of the project.
    /// Branch and worktree directory derive from it.
    pub name: String,
    /// Instance (`ins_…`) running the harness; absent if provisioning stopped
    /// before the agent launched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<InstanceId>,
    /// Host the worker was placed on.
    pub host_id: HostId,
    /// Registered workspace the worktree belongs to.
    pub workspace_id: WorkspaceId,
    /// Harness kind (`claude` / `codex` / `grok`).
    pub harness: String,
    /// Model id the agent launched with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Provider profile used, when admission picked one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_profile_id: Option<String>,
    /// Branch `wt/<name>/<slug>`, product-assigned.
    pub branch: String,
    /// Absolute worktree path reported back by the Node (node-assigned under
    /// its managed worktree root).
    pub worktree_path: String,
    /// Allocated port block (e.g. `58600-58609`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_block: Option<String>,
    /// Per-worker cargo target dir reported by the Node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_dir: Option<String>,
    /// Brief delivered to the worker, as an objects-store attachment (`obj_…`).
    /// The brief always rides the file path, never inline prompt text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief_object_id: Option<String>,
    /// Optional bound task (`tsk_…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// Lifecycle state.
    pub state: WorkerState,
    /// The admission/placement decision JSON (reasons[]/rejected[]), stored for
    /// the audit trail and the bot placement card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supply_decision: Option<serde_json::Value>,
    /// Bytes reclaimed from the target dir at retire (reported by the Node).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reclaimed_bytes: Option<crate::U64>,
}

// ── Hub→Node worker.provision / worker.remove ──────────────────────────────

/// `worker.provision` params: create the product-assigned worktree and the
/// per-worker cargo target directory on the Node.
///
/// The Node owns the filesystem layout: the request names the worker and the
/// branch, but never absolute paths (security-review-2 M4, same rule as
/// `worktree.create`). The Node creates the worktree under its managed
/// `<repo>/../remuda-wt/<name>` root and the target dir under
/// `<repo>/../remuda-target/<name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerProvisionParams {
    /// Worker name; one safe path segment, also the worktree directory.
    pub name: String,
    /// Full branch to create (`wt/<name>/<slug>`); must not exist yet.
    pub branch: String,
    /// Registered workspace whose repository root holds the checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// Start point to branch and check out from (default `origin/main`). The
    /// Node fetches it first, so the worktree always starts at remote main.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_point: Option<String>,
}

/// Result of `worker.provision`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerProvisionResult {
    /// Worker name.
    pub name: String,
    /// Branch created.
    pub branch: String,
    /// Start point actually used.
    pub start_point: String,
    /// Absolute worktree path (node-assigned).
    pub worktree_path: String,
    /// Absolute per-worker cargo target dir (node-assigned).
    pub target_dir: String,
}

/// `worker.remove` params: reclaim one worker's filesystem resources.
///
/// Paths are never taken from the wire: the Node recomputes both locations from
/// the managed roots and the `name`, then containment-checks before deleting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRemoveParams {
    /// Worker name whose worktree/target dir are removed.
    pub name: String,
    /// Registered workspace the worktree was created in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// Instance whose herdr carrier (tab/panes/workspace) is closed first.
    /// Absent for print-driver workers that own no tab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<InstanceId>,
}

/// Result of `worker.remove`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRemoveResult {
    /// Worker name.
    pub name: String,
    /// True when a worktree was found and removed.
    pub worktree_removed: bool,
    /// True when a target dir was found and removed.
    pub target_removed: bool,
    /// Bytes reclaimed from the target dir.
    pub reclaimed_bytes: crate::U64,
}

/// Validate a worker name: one safe path segment, same rule as worktree names.
pub fn validate_worker_name(name: &str) -> Result<(), String> {
    path_guard::safe_segment(name).map_err(|error| error.to_string())
}

/// Validate a worker branch of the form `wt/<name>/<slug>` where both trailing
/// segments are safe (the branch is product-assigned, so it is checked here
/// rather than passed to a shell).
pub fn validate_worker_branch(branch: &str) -> Result<(), String> {
    let mut parts = branch.split('/');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("wt"), Some(name), Some(slug), None) if !name.is_empty() && !slug.is_empty() => {
            path_guard::safe_segment(name).map_err(|error| error.to_string())?;
            validate_slug(slug)
        }
        _ => Err(format!(
            "worker branch must be wt/<name>/<slug> with safe segments: {branch}"
        )),
    }
}

/// Validate a branch slug: like a safe path segment but slightly looser —
/// digits and letters may start it, still single-segment with no traversal.
fn validate_slug(slug: &str) -> Result<(), String> {
    let valid = (1..=48).contains(&slug.len())
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(format!("bad branch slug {slug}"))
    }
}

/// Turn arbitrary text (task title, brief file stem) into a branch-safe slug.
#[must_use]
pub fn slugify(input: &str) -> String {
    let slug: String = input
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let mut slug = slug.trim_matches('-').to_string();
    // Collapse runs of dashes.
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    if slug.is_empty() {
        "work".to_string()
    } else {
        slug.truncate(48);
        slug.trim_end_matches('-').to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn state_wire_shape_is_adjacently_tagged() {
        assert_eq!(
            serde_json::to_value(WorkerState::Done {
                sha: "abc123".into()
            })
            .unwrap(),
            serde_json::json!({ "state": "done", "sha": "abc123" })
        );
        assert_eq!(
            serde_json::to_value(WorkerState::Blocked {
                reason: "need creds".into()
            })
            .unwrap(),
            serde_json::json!({ "state": "blocked", "reason": "need creds" })
        );
        assert_eq!(
            serde_json::to_value(WorkerState::Working).unwrap(),
            serde_json::json!({ "state": "working" })
        );
        let parsed: WorkerState = serde_json::from_value(json!({ "state": "retired" })).unwrap();
        assert_eq!(parsed, WorkerState::Retired);
        assert!(!WorkerState::Done { sha: "x".into() }.is_working());
        assert!(WorkerState::Working.is_working());
        assert!(WorkerState::Working.is_active());
        assert!(!WorkerState::Retired.is_active());
    }

    #[test]
    fn state_update_validation() {
        assert!(WorkerState::from_update("working", None, None).is_ok());
        assert!(WorkerState::from_update("done", Some("abc"), None).is_ok());
        assert!(WorkerState::from_update("done", None, None).is_err());
        assert!(WorkerState::from_update("blocked", None, Some("reason")).is_ok());
        assert!(WorkerState::from_update("blocked", None, None).is_err());
        assert!(WorkerState::from_update("dispatched", None, None).is_err());
    }

    #[test]
    fn names_and_branches_validate() {
        assert!(validate_worker_name("c-task1").is_ok());
        assert!(validate_worker_name("../x").is_err());
        assert!(validate_worker_branch("wt/c-task1/fix-bug").is_ok());
        assert!(validate_worker_branch("main").is_err());
        assert!(validate_worker_branch("wt/c/../x").is_err());
        assert_eq!(slugify("Fix the thing!"), "fix-the-thing");
        assert_eq!(slugify("..."), "work");
        assert_eq!(slugify("TRAIL---dash"), "trail-dash");
    }
}
