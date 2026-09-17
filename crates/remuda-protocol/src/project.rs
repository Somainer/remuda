//! Hub `Project` entity and the recursive delegation-tree primitives; design §§2.5, 3.2.
//!
//! Three tiers are *not* schema. An [`Instance`] carries a [`InstanceScope`]
//! (resources it may touch) and a set of [`GrantVerb`]s (verbs it may use).
//! `role` is only a **preset name** applied at create time and stored for
//! display — enforcement always reads scope and grants.

use crate::*;
use serde::{Deserialize, Serialize};

/// Preset name for a leaf worker: no grants, empty/narrowed scope.
pub const ROLE_WORKER: &str = "worker";
/// Preset name for a per-project coordinator seat.
pub const ROLE_PROJECT_COORDINATOR: &str = "project-coordinator";
/// Preset name for the single Hub-wide top coordinator seat.
pub const ROLE_TOP_COORDINATOR: &str = "top-coordinator";

/// Every registered preset name.
pub const PRESET_NAMES: &[&str] = &[ROLE_WORKER, ROLE_PROJECT_COORDINATOR, ROLE_TOP_COORDINATOR];

/// True for a registered preset name (`worker` / `project-coordinator` /
/// `top-coordinator`).
pub fn is_preset_name(role: &str) -> bool {
    PRESET_NAMES.contains(&role)
}

/// Grant bundle a preset expands to at create time.
///
/// The expansion is applied once, then stored on the instance as explicit
/// `grants`. `worker` is the empty set; a project coordinator may delegate,
/// land, and spend; the top coordinator may delegate, spend, and address the
/// owner but never lands (design §2.1).
pub fn preset_grants(role: &str) -> Option<Vec<GrantVerb>> {
    match role {
        ROLE_WORKER => Some(Vec::new()),
        ROLE_PROJECT_COORDINATOR => {
            Some(vec![GrantVerb::Dispatch, GrantVerb::Land, GrantVerb::Spend])
        }
        ROLE_TOP_COORDINATOR => Some(vec![
            GrantVerb::Dispatch,
            GrantVerb::Spend,
            GrantVerb::AddressOwner,
        ]),
        _ => None,
    }
}

/// Resource reach of one delegation node; design §2.5.
///
/// Scope narrows monotonically down the tree: a child scope must be a subset
/// of its parent's. An *empty* dimension means "no narrowing on this
/// dimension" for the dimension's parent view — a node with every dimension
/// empty is the universe root (the human-seated top coordinator).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceScope {
    /// Projects this node may touch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_ids: Vec<ProjectId>,
    /// Hosts this node may place work on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_ids: Vec<HostId>,
    /// Workspaces this node may touch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_ids: Vec<WorkspaceId>,
    /// Provider profiles / supply buckets this node may spend. Entries are
    /// `pvp_…` profile ids; batch 3 (co-supply) adds structured grants.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supply_grants: Vec<String>,
}

impl InstanceScope {
    /// A scope that narrows nothing (universe root).
    pub fn universe() -> Self {
        Self::default()
    }

    /// True when every dimension is empty — the universe root view.
    pub fn is_universe(&self) -> bool {
        self.project_ids.is_empty()
            && self.host_ids.is_empty()
            && self.workspace_ids.is_empty()
            && self.supply_grants.is_empty()
    }

    /// Subset predicate used by the Hub at child create; design §2.5 ①.
    ///
    /// Per dimension: an empty parent dimension means "not narrowed", so the
    /// child is unconstrained there; a non-empty parent dimension requires
    /// every child entry to be present. An empty child dimension is the empty
    /// set (a leaf that reaches no resources of that kind), which is always a
    /// subset.
    pub fn is_subset_of(&self, parent: &InstanceScope) -> bool {
        subset_branded(&self.project_ids, &parent.project_ids)
            && subset_branded(&self.host_ids, &parent.host_ids)
            && subset_branded(&self.workspace_ids, &parent.workspace_ids)
            && subset_str(&self.supply_grants, &parent.supply_grants)
    }

    /// Convenience `projectId` view: the sole project when scope names exactly
    /// one; design §2.5 (`projectId` replaces the single column).
    pub fn single_project_id(&self) -> Option<&ProjectId> {
        match self.project_ids.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }

    /// True when `project` is in scope (or projects are not narrowed).
    pub fn allows_project(&self, project: &str) -> bool {
        self.project_ids.is_empty()
            || self
                .project_ids
                .iter()
                .any(|id| id.as_id().as_str() == project)
    }

    /// True when `host` is in scope (or hosts are not narrowed).
    pub fn allows_host(&self, host: &str) -> bool {
        self.host_ids.is_empty() || self.host_ids.iter().any(|id| id.as_id().as_str() == host)
    }

    /// True when `workspace` is in scope (or workspaces are not narrowed).
    pub fn allows_workspace(&self, workspace: &str) -> bool {
        self.workspace_ids.is_empty()
            || self
                .workspace_ids
                .iter()
                .any(|id| id.as_id().as_str() == workspace)
    }
}

fn subset_branded<T: PartialEq>(child: &[T], parent: &[T]) -> bool {
    parent.is_empty() || child.iter().all(|item| parent.contains(item))
}

fn subset_str(child: &[String], parent: &[String]) -> bool {
    parent.is_empty() || child.iter().all(|item| parent.contains(item))
}

/// One `(hostId, workspaceId)` pair — the same key Space uses (D-024).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectMember {
    /// Member host.
    pub host_id: HostId,
    /// Registered workspace on that host (D-023).
    pub workspace_id: WorkspaceId,
    /// Member role label (`primary` / `build` / …); display only.
    #[serde(default = "default_member_role")]
    pub role: String,
}

fn default_member_role() -> String {
    "member".into()
}

/// Project-side capacity view of one host with capacity; design §3.2.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectHostQuota {
    /// Host this quota describes; it must also appear (with a workspace) in
    /// `members` for placement to select it.
    pub host_id: HostId,
    /// Project cap, never above the host's own `maxInstances`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_instances: Option<i64>,
    /// How many instances may be building at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_building: Option<i64>,
    /// Scratch disk budget (GiB); retire must reclaim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_budget_gb: Option<i64>,
    /// Port blocks allocated to this project on this host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub port_blocks: Vec<String>,
    /// Hard host-label requirements (`toolchain=rust`, …); design §3.4 step 1.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// `local` / `remote` latency class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_class: Option<String>,
}

/// Project placement defaults.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectPlacement {
    /// `auto` / `local` / `remote`.
    #[serde(default = "default_placement")]
    pub default: String,
    /// Labels every selected host must carry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Explicit candidate hosts; empty means every member host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_ids: Vec<HostId>,
}

fn default_placement() -> String {
    "auto".into()
}

/// Provider reference (no secret — projects hold only a profileId; design §3.2).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectProviderRef {
    /// `gateway` / `direct` / `none`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<String>,
    /// `pvp_…` profile id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

/// Project speaks model roles, not concrete model ids; design §3.2.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelRoles {
    /// Default workhorse model id/alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workhorse: Option<String>,
    /// Frontier model id/alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontier: Option<String>,
    /// Model used for review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
    /// Cheap model id/alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cheap: Option<String>,
}

/// One gate lane; batch 6 enforces these via the Hub gate queue and the Node
/// lane runner (`gate.run`). Lane environment is set once on the Project and
/// sent verbatim on every run — never re-typed per gate.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectGateLane {
    /// Stable lane id.
    pub id: String,
    /// Host that runs the lane.
    pub host_id: HostId,
    /// Checkout path on that host.
    pub repo_path: String,
    /// Dedicated cargo target dir.
    pub target_dir: String,
    /// Allocated port block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<String>,
    /// SSH remote alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// Which host pushes `main` when a land job runs on this lane. Defaults to
    /// `lane`; set `home` for a lane whose host holds no push credential for
    /// the project `repoRemote` (the policy for the remote lane), and the lane
    /// then verifies only while the project home host pushes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_from: Option<crate::GatePushFrom>,
    /// Operator's existing remote/alias **on the home host** that reaches this
    /// lane's repo, used to fetch the verified merge commit for a
    /// `pushFrom: home` land. Required when `pushFrom` resolves to `home`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_remote: Option<String>,
    /// Extra environment applied to every gate run on this lane
    /// (`CARGO_HOME`, `PW_CHANNEL`, `RUSTFLAGS`, …).
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Advisory lock path for the shared browser web-hub-e2e step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_path: Option<String>,
    /// Playwright WebSocket endpoint (`PW_TEST_CONNECT_WS_ENDPOINT`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pw_endpoint: Option<String>,
    /// PATH prefix (toolchain bin dirs) for the gate environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain_path: Option<String>,
}

/// Gate configuration; design §3.2 (placeholder, consumed by r-mergequeue).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectGate {
    /// Gate command; authority is the merged worktree's copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Run affected-only gates.
    #[serde(default = "default_true")]
    pub affected: bool,
    /// Web gate mode (`auto` / `always` / `never`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web: Option<String>,
    /// Verification lanes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lanes: Vec<ProjectGateLane>,
    /// `global-cas` — verify may parallelize, land is always serialized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub land_serialization: Option<String>,
    /// Mandatory gate steps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mandatory_steps: Vec<String>,
}

impl Default for ProjectGate {
    fn default() -> Self {
        Self {
            command: Some("scripts/ci/gate.sh".into()),
            affected: true,
            web: Some("auto".into()),
            lanes: Vec::new(),
            land_serialization: Some("global-cas".into()),
            mandatory_steps: vec![
                "secret-scan".into(),
                "no-tunnel-scan".into(),
                "gen-api-current".into(),
                "verify-tree".into(),
            ],
        }
    }
}

fn default_true() -> bool {
    true
}

/// Enforced policy switches. All default on; agents may never change them
/// (D-031 — read-only for coordinators, change requires escalation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectEnforcedPolicy {
    /// Never run anything under `deploy/`.
    #[serde(default = "default_true")]
    pub no_deploy_scripts: bool,
    /// Never probe/install tunnel tooling (D-031).
    #[serde(default = "default_true")]
    pub no_tunnel_tools: bool,
    /// Workers never push; landing goes through the gate.
    #[serde(default = "default_true")]
    pub workers_never_push: bool,
    /// No landing without a green gate.
    #[serde(default = "default_true")]
    pub gate_before_land: bool,
    /// Land requires an ancestor check + CAS.
    #[serde(default = "default_true")]
    pub cas_ancestor_check: bool,
    /// One worktree per worker.
    #[serde(default = "default_true")]
    pub one_worktree_per_worker: bool,
    /// Port blocks are product-allocated.
    #[serde(default = "default_true")]
    pub allocated_port_blocks: bool,
    /// Retire must reclaim scratch disk.
    #[serde(default = "default_true")]
    pub reclaim_disk_on_retire: bool,
    /// No OS settings changes from agents.
    #[serde(default = "default_true")]
    pub no_os_settings_changes: bool,
    /// Briefs never carry secrets.
    #[serde(default = "default_true")]
    pub secrets_never_in_briefs: bool,
}

impl Default for ProjectEnforcedPolicy {
    fn default() -> Self {
        Self {
            no_deploy_scripts: true,
            no_tunnel_tools: true,
            workers_never_push: true,
            gate_before_land: true,
            cas_ancestor_check: true,
            one_worktree_per_worker: true,
            allocated_port_blocks: true,
            reclaim_disk_on_retire: true,
            no_os_settings_changes: true,
            secrets_never_in_briefs: true,
        }
    }
}

/// Configurable policy. Owner-tunable; the first four delegate-tree limits
/// implement design §2.5 ⑤ (depth/fan-out are policy, not schema).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectConfigurablePolicy {
    /// Max concurrent workers in this project.
    #[serde(default = "default_max_concurrent_workers")]
    pub max_concurrent_workers: i64,
    /// Nudge throttle per worker, minutes.
    #[serde(default = "default_nudge_throttle_mins")]
    pub nudge_throttle_mins: i64,
    /// Stall threshold, minutes.
    #[serde(default = "default_stall_threshold_mins")]
    pub stall_threshold_mins: i64,
    /// Max children per delegated task.
    #[serde(default = "default_max_fan_out_per_task")]
    pub max_fan_out_per_task: i64,
    /// Literal worker completion line.
    #[serde(default = "default_completion_line")]
    pub completion_line: String,
    /// Placement tendency policy name.
    #[serde(default = "default_default_placement")]
    pub default_placement: String,
    /// Delegation depth limit overriding the Hub default (3).
    #[serde(default = "default_max_delegation_depth")]
    pub max_delegation_depth: i64,
    /// Active children one node may delegate, overriding the Hub default (8).
    #[serde(default = "default_coordinator_fan_out")]
    pub coordinator_fan_out: i64,
    /// Relax "one active dispatch holder per project".
    #[serde(default)]
    pub allow_multiple_dispatchers: bool,
}

fn default_max_concurrent_workers() -> i64 {
    8
}
fn default_nudge_throttle_mins() -> i64 {
    15
}
fn default_stall_threshold_mins() -> i64 {
    20
}
fn default_max_fan_out_per_task() -> i64 {
    6
}
fn default_completion_line() -> String {
    "DONE <sha>".into()
}
fn default_default_placement() -> String {
    "remote-when-build-heavy".into()
}
fn default_max_delegation_depth() -> i64 {
    crate::DEFAULT_MAX_DELEGATION_DEPTH as i64
}
fn default_coordinator_fan_out() -> i64 {
    crate::DEFAULT_COORDINATOR_FAN_OUT as i64
}

impl Default for ProjectConfigurablePolicy {
    fn default() -> Self {
        Self {
            max_concurrent_workers: default_max_concurrent_workers(),
            nudge_throttle_mins: default_nudge_throttle_mins(),
            stall_threshold_mins: default_stall_threshold_mins(),
            max_fan_out_per_task: default_max_fan_out_per_task(),
            completion_line: default_completion_line(),
            default_placement: default_default_placement(),
            max_delegation_depth: default_max_delegation_depth(),
            coordinator_fan_out: default_coordinator_fan_out(),
            allow_multiple_dispatchers: false,
        }
    }
}

/// Full project policy envelope; design §3.2.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectPolicy {
    /// Enforced switches; read-only for coordinator agents (D-031).
    #[serde(default)]
    pub enforced: ProjectEnforcedPolicy,
    /// Owner-configurable limits.
    #[serde(default)]
    pub configurable: ProjectConfigurablePolicy,
}

/// Thin authoritative Hub Project entity; design §3.1–§3.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    /// `meta`; design §3.2.
    #[serde(flatten)]
    pub meta: EntityMeta<ProjectId>,
    /// Project display name.
    pub name: String,
    /// Host the project coordinator seat is pinned to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_host: Option<HostId>,
    /// Optional git remote, validation only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote: Option<String>,
    /// Default base branch for worktrees/gates.
    #[serde(default = "default_base_branch")]
    pub default_base_branch: String,
    /// Worktree branch pattern.
    #[serde(default = "default_branch_pattern")]
    pub branch_pattern: String,
    /// Workspaces participating in the project (Space key reused).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<ProjectMember>,
    /// Per-host capacity views.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<ProjectHostQuota>,
    /// Placement defaults.
    #[serde(default)]
    pub placement: ProjectPlacement,
    /// Provider reference (never a secret).
    #[serde(default)]
    pub provider: ProjectProviderRef,
    /// Model role → model alias map.
    #[serde(default)]
    pub model_roles: ModelRoles,
    /// Default effort tier name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    /// `ask` / `accept-edits` / `bypass`. Agent launches are forced non-yolo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_posture: Option<String>,
    /// Gate configuration placeholder.
    #[serde(default)]
    pub gate: ProjectGate,
    /// Policy envelope.
    #[serde(default)]
    pub policy: ProjectPolicy,
    /// Repo-relative brief template (advisory layer lives in the repo).
    #[serde(default = "default_brief_ref")]
    pub brief_ref: String,
}

fn default_base_branch() -> String {
    "main".into()
}
fn default_branch_pattern() -> String {
    "wt/{worker}/{topic}".into()
}
fn default_brief_ref() -> String {
    ".remuda/brief.md".into()
}

/// Launch-relevant defaults folded into an instance create; design §3.2 / §6.
///
/// Fold priority is explicit (request body) > project > host > global, so
/// every field here only fills a key the request did not name.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectLaunchDefaults {
    /// Default agent kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Default driver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// Default model id/alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Provider delegation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<String>,
    /// Provider profile id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_profile_id: Option<String>,
    /// Default extra launch args.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub launch_args: Vec<String>,
    /// Default effort tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Default permission posture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_posture: Option<String>,
}

impl Project {
    /// Extract the launch fold from the stored project document.
    pub fn launch_defaults(&self) -> ProjectLaunchDefaults {
        ProjectLaunchDefaults {
            agent: None,
            driver: None,
            model: self.model_roles.workhorse.clone(),
            delegation: self.provider.delegation.clone(),
            provider_profile_id: self.provider.profile_id.clone(),
            launch_args: Vec::new(),
            effort: self.default_effort.clone(),
            permission_posture: self.permission_posture.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_names_and_grant_bundles_are_fixed() {
        // Golden: presets are named bundles, §2.5. Any change here is a
        // deliberate change to the default three-tier topology.
        assert_eq!(
            PRESET_NAMES,
            ["worker", "project-coordinator", "top-coordinator"]
        );
        assert_eq!(preset_grants(ROLE_WORKER), Some(Vec::new()));
        assert_eq!(
            preset_grants(ROLE_PROJECT_COORDINATOR),
            Some(vec![GrantVerb::Dispatch, GrantVerb::Land, GrantVerb::Spend])
        );
        assert_eq!(
            preset_grants(ROLE_TOP_COORDINATOR),
            Some(vec![
                GrantVerb::Dispatch,
                GrantVerb::Spend,
                GrantVerb::AddressOwner
            ])
        );
        assert!(preset_grants("made-up").is_none());
        assert!(!is_preset_name("root"));
    }

    #[test]
    fn scope_narrows_monotonically_per_dimension() {
        let project_a = ProjectId::new();
        let project_b = ProjectId::new();
        let host = HostId::new();
        let workspace = WorkspaceId::new();
        let universe = InstanceScope::universe();
        assert!(universe.is_universe());
        let child = InstanceScope {
            project_ids: vec![project_a.clone()],
            host_ids: vec![host.clone()],
            workspace_ids: vec![workspace.clone()],
            supply_grants: vec!["pvp_relay".into()],
        };
        assert!(child.is_subset_of(&universe));
        // Enforcement only ever queries child⊆parent; an empty child
        // dimension is the vacuous subset, so the wildcard convention is
        // intentionally not antisymmetric.
        let narrowed = InstanceScope {
            project_ids: vec![project_a.clone()],
            ..Default::default()
        };
        assert!(narrowed.is_subset_of(&child));
        let other_project = InstanceScope {
            project_ids: vec![project_b.clone()],
            ..Default::default()
        };
        assert!(!other_project.is_subset_of(&child));
        let both = InstanceScope {
            project_ids: vec![project_a.clone(), project_b.clone()],
            ..Default::default()
        };
        assert!(!both.is_subset_of(&narrowed));
        assert_eq!(
            narrowed
                .single_project_id()
                .map(|id| id.as_id().to_string()),
            narrowed
                .project_ids
                .first()
                .map(|id| id.as_id().to_string())
        );
        assert!(InstanceScope::default().single_project_id().is_none());
        assert!(narrowed.allows_project(narrowed.project_ids.first().unwrap().as_id().as_str()));
        assert!(!narrowed.allows_project(project_b.as_id().as_str()));
        assert!(InstanceScope::default().allows_project(project_b.as_id().as_str()));
    }
}
