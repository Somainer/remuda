//! Task ledger, the mandate chain, path ownership and placement ledger rows;
//! design §2.2 (Tier 2 held state), §2.5 (`parentTaskId` mirrors the recursive
//! delegation tree), §4.3 (`TaskSpec.owns` / `scopeCheck`), §5.6 (the placement
//! row is audit trail and bot card at once).
//!
//! Everything in this file is pure data + pure logic. The Hub stores it; the
//! merge gate (later, co-loop) calls [`check_paths_within_owns`] without any
//! Hub dependency.

use crate::*;
use serde::{Deserialize, Serialize};

// ─── State machine ─────────────────────────────────────────────────────────

wire_enum!(TaskState, "2.2/5.3", {
    Pending => "pending",
    Placed => "placed",
    Running => "running",
    Stalled => "stalled",
    Done => "done",
    Failed => "failed",
    Parked => "parked",
    Deferred => "deferred",
});

wire_enum!(TaskClass, "4.3", {
    Research => "research",
    Implement => "implement",
    Review => "review",
    Test => "test",
    MergeGate => "merge-gate",
    Triage => "triage",
    Docs => "docs",
});

// Read-only kanban projection of the ledger (D-050): a column is derived from
// `state + archived_at + placement`, never stored, and it is not a second
// state machine — column drags decompose into legal TaskState transitions.
wire_enum!(BoardColumn, "2.2", {
    Todo => "todo",
    InProgress => "in-progress",
    Done => "done",
    Archived => "archived",
});

impl Default for TaskClass {
    // TaskClass is a wire_enum! (no per-variant Default attribute), so the
    // default is an explicit impl rather than a derive: §4.3 makes
    // `implement` the implicit dispatch class.
    #[allow(clippy::derivable_impls)]
    fn default() -> Self {
        TaskClass::Implement
    }
}

impl TaskState {
    /// Legal ledger transitions; design §5.3 vocabulary plus §7 risk 8.
    ///
    /// Invariant I1 (design §7 #8): `done` is a *claim*, not a gate — nothing
    /// unlocks from it. Dependency edges unlock only from [`Task::landed_sha`],
    /// which is set by the land step, never by this state machine.
    pub fn can_transition_to(self, target: TaskState) -> bool {
        use TaskState::*;
        matches!(
            (self, target),
            (Pending, Placed | Deferred | Failed)
                | (Placed, Running | Pending | Deferred | Failed)
                | (Running, Stalled | Done | Failed | Parked)
                | (Stalled, Running | Failed | Parked)
                | (Parked, Running | Pending | Deferred | Failed)
                | (Deferred, Pending | Placed | Failed)
        )
    }

    /// True once the work (or its failure) is settled; terminal tasks release
    /// their path claims.
    pub fn is_terminal(self) -> bool {
        matches!(self, TaskState::Done | TaskState::Failed)
    }
}

// ─── Mandate chain (§2.5) ──────────────────────────────────────────────────

/// One edge of the inherited owner-intent chain.
///
/// Every delegation edge attaches the upstream's own words, so a depth-3 node
/// still reads the owner's original instruction rather than a retelling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MandateLink {
    /// Depth from the human root; the root task is 0.
    pub depth: u32,
    /// Task whose intent this link carries.
    pub task_id: TaskId,
    /// That task's title, denormalised for cards rendered after renames.
    pub title: String,
    /// The exact words given to that task by its delegator; the root link
    /// carries the owner's original intent verbatim.
    pub intent: String,
}

/// The inherited owner-intent chain carried by every task; design §2.5.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Mandate {
    /// Root-first chain, always ending with the owning task's own link.
    #[serde(default)]
    pub chain: Vec<MandateLink>,
}

impl Mandate {
    /// Build the root chain: the owner's words as given to the first task.
    fn root(task_id: TaskId, title: String, intent: String) -> Mandate {
        Mandate {
            chain: vec![MandateLink {
                depth: 0,
                task_id,
                title,
                intent,
            }],
        }
    }

    /// Extend the chain for a delegated child task; design §2.5.
    ///
    /// The child inherits the whole ancestor chain verbatim and appends the
    /// words its parent delegated *to it* as the new leaf link.
    fn delegate(&self, child_id: TaskId, child_title: String, child_intent: String) -> Mandate {
        let mut chain = self.chain.clone();
        let depth = chain.last().map(|link| link.depth + 1).unwrap_or(0);
        chain.push(MandateLink {
            depth,
            task_id: child_id,
            title: child_title,
            intent: child_intent,
        });
        Mandate { chain }
    }

    /// The owner's original words; present on every task, at any depth.
    pub fn owner_intent(&self) -> Option<&str> {
        self.chain.first().map(|link| link.intent.as_str())
    }

    /// The task's own intent (its own leaf link).
    pub fn own_intent(&self) -> Option<&str> {
        self.chain.last().map(|link| link.intent.as_str())
    }
}

// ─── Directory binding (task-model t-bind, D-050) ──────────────────────────

wire_enum!(TaskBindingMode, "2.2", {
    // Reuse an existing directory: registered root or a `remuda-wt` sibling;
    // no git operations, one directory = one branch.
    Reuse => "reuse",
    // App-managed worktree pooled by the Node; the lease cuts a per-task
    // branch `wt/<slot>/<task-slug>` in place.
    Pool => "pool",
});

/// The operator's per-task working-directory choice (D-050 §1.1/§2).
///
/// Serialised inside the task's existing `doc_json` column, so old task rows
/// need no migration: the field defaults to `None` and is skipped on the wire
/// when absent. The binding routes dispatch two ways without a new dispatch
/// field — `reuse` folds into the existing `cwd` admission, `pool` into the
/// existing `worktree` field backed by a `worktree_leases` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskSpaceBinding {
    /// Reuse an existing directory, or lease an app-managed pool slot.
    pub mode: TaskBindingMode,
    /// Host whose workspace backs the directory.
    pub host_id: HostId,
    /// Registered workspace the directory belongs to.
    pub workspace_id: WorkspaceId,
    /// Worktree/slot name. `None` (reuse only) means the registered workspace
    /// root itself. For `pool` this is the leased slot name; the directory key
    /// relative to the workspace root is then `remuda-wt/<name>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_name: Option<String>,
    /// Branch checked out while leased. Record only — reuse never switches
    /// branches, and the pool derives its branch on the Node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// `worktree_leases` rows backing this binding (one today).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lease_ref_ids: Vec<Id>,
}

impl TaskSpaceBinding {
    /// Directory identity key relative to the workspace root; the registered
    /// root itself is `"."` (the lease table's composite key, D-050 §1.3).
    pub fn dir_key(&self) -> &str {
        self.worktree_name.as_deref().unwrap_or(".")
    }

    /// True for a binding onto the registered workspace root itself.
    pub fn is_root(&self) -> bool {
        self.worktree_name.is_none()
    }

    /// HTTP path token for the lease routes. A literal `.` collapses in URLs,
    /// so the root is addressed as `-` (mirrors `http.rs::node_worktree_name`).
    pub fn lease_path_name(&self) -> &str {
        if self.is_root() {
            "-"
        } else {
            self.worktree_name
                .as_deref()
                .expect("non-root binding has a name")
        }
    }
}

// ─── Task document ─────────────────────────────────────────────────────────

/// One dependency edge. Edges unlock only when the referenced task carries a
/// landed sha (invariant I1, design §7 #8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskDep {
    /// Task that must land first.
    pub task_id: TaskId,
    /// Why the edge exists (audit/card text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Estimated budget envelope; design §4.3. Amounts are estimates (§4.5).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskBudget {
    /// Estimated USD cap (money is an estimate, §4.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_usd: Option<f64>,
    /// Turn cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<i64>,
    /// Wall-clock cap, minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_wall_mins: Option<i64>,
}

/// Where the task's worker is (or was) placed; updated from placement rows.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskPlacementRef {
    /// Latest placement-ledger row describing this slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_id: Option<Id>,
    /// Host the worker runs on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<HostId>,
    /// Instance holding the worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<InstanceId>,
    /// Worktree branch being worked on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Model the placement pinned (pin means no auto-resolve, §4.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// One row of the task ledger; design §2.2/§8.1 row 4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    /// `meta`.
    #[serde(flatten)]
    pub meta: EntityMeta<TaskId>,
    /// Owning project; the task ledger slice key.
    pub project_id: ProjectId,
    /// Parent task; mirrors the recursive delegation tree (§2.5). Absent for a
    /// root task the top coordinator logged from an owner intent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<TaskId>,
    /// Short human title.
    pub title: String,
    /// This node's task plus the inherited chain of upstream intents.
    #[serde(default)]
    pub mandate: Mandate,
    /// Task class; the only scheduling judgement attached at add time (§3.4).
    #[serde(default = "default_task_class")]
    pub class: TaskClass,
    /// Ledger state; see [`TaskState`].
    pub state: TaskState,
    /// Repo-relative path globs this task is allowed to touch; `remuda own`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owns: Vec<String>,
    /// Dependency edges, unlocked by a landed sha only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deps: Vec<TaskDep>,
    /// Estimated budget envelope.
    #[serde(default)]
    pub budget: TaskBudget,
    /// Current placement slot, when placed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<TaskPlacementRef>,
    /// Landed commit sha; set by the land/gate step — never by a worker status
    /// bit. Dependency edges unlock from this value (§7 #8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landed_sha: Option<String>,
    /// Reason carried on `failed` when the worker reported `BLOCKED <reason>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    /// Archive timestamp; an orthogonal flag (D-050), not a ninth state.
    /// Archiving never changes `state` (terminal semantics stay untouched); a
    /// task carrying a timestamp projects to the archive column from any
    /// state. `None` on every row written before the column existed, and
    /// skipped on serialise so such rows stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<Timestamp>,
    /// Working-directory binding chosen at creation (D-050). Lives in
    /// `doc_json` with a serde default, so rows written before t-bind decode
    /// as `None` and serialise byte-identically (no migration).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_binding: Option<TaskSpaceBinding>,
}

fn default_task_class() -> TaskClass {
    TaskClass::Implement
}

impl Task {
    /// Construct a root task (the top coordinator logging an owner intent).
    #[allow(clippy::too_many_arguments)]
    pub fn new_root(
        now: Timestamp,
        project_id: ProjectId,
        title: String,
        intent: String,
        class: TaskClass,
        owns: Vec<String>,
        deps: Vec<TaskDep>,
        budget: TaskBudget,
    ) -> Task {
        let id = TaskId::new();
        Task {
            meta: EntityMeta {
                id: id.clone(),
                revision: U64(1),
                created_at: now.clone(),
                updated_at: now,
            },
            project_id,
            parent_task_id: None,
            mandate: Mandate::root(id, title.clone(), intent),
            title,
            class,
            state: TaskState::Pending,
            owns: normalize_globs(owns),
            deps,
            budget,
            placement: None,
            landed_sha: None,
            blocked_reason: None,
            archived_at: None,
            workspace_binding: None,
        }
    }

    /// Construct a child task (`remuda task split`); the mandate chain is
    /// inherited from `self` and extended with the child's own delegation edge.
    #[allow(clippy::too_many_arguments)]
    pub fn new_child(
        &self,
        now: Timestamp,
        title: String,
        intent: String,
        class: TaskClass,
        owns: Vec<String>,
        deps: Vec<TaskDep>,
        budget: TaskBudget,
    ) -> Task {
        let id = TaskId::new();
        let mandate = self.mandate.delegate(id.clone(), title.clone(), intent);
        Task {
            meta: EntityMeta {
                id,
                revision: U64(1),
                created_at: now.clone(),
                updated_at: now,
            },
            project_id: self.project_id.clone(),
            parent_task_id: Some(self.meta.id.clone()),
            mandate,
            title,
            class,
            state: TaskState::Pending,
            owns: normalize_globs(owns),
            deps,
            budget,
            placement: None,
            landed_sha: None,
            blocked_reason: None,
            archived_at: None,
            workspace_binding: None,
        }
    }

    /// Depth from the owner-root task (root = 0).
    pub fn depth(&self) -> u32 {
        self.mandate
            .chain
            .last()
            .map(|link| link.depth)
            .unwrap_or(0)
    }

    /// Deps that have not unlocked yet: the referenced task is missing or has
    /// no landed sha. A `done` worker status does not unlock anything (§7 #8).
    pub fn locked_deps<'a, I>(&self, tasks: I) -> Vec<TaskId>
    where
        I: IntoIterator<Item = &'a Task>,
    {
        let tasks: Vec<&Task> = tasks.into_iter().collect();
        self.deps
            .iter()
            .filter(|dep| {
                tasks
                    .iter()
                    .find(|task| task.meta.id == dep.task_id)
                    .is_none_or(|task| task.landed_sha.is_none())
            })
            .map(|dep| dep.task_id.clone())
            .collect()
    }

    /// The read-only board column this task projects to; D-050.
    ///
    /// Derived purely from `archived_at`, `state` and `placement` — never
    /// stored, and deliberately absent from [`TaskState::can_transition_to`].
    /// Rules (plan B.4):
    /// - `archived_at` is orthogonal and wins over every state without
    ///   changing it;
    /// - `pending`/`placed`/`deferred`/`parked` read as the to-do column;
    /// - `running`/`stalled` read as in-progress;
    /// - only `done` reads as done;
    /// - `failed` has no history field, so it is placed deterministically from
    ///   existing storage: a held placement means it failed mid-flight
    ///   (in-progress), none means it never left to-do. The failure badge is
    ///   the state itself, read by the caller; this never creates a fifth
    ///   column or pre-fail storage.
    pub fn board_column(&self) -> BoardColumn {
        if self.archived_at.is_some() {
            return BoardColumn::Archived;
        }
        match self.state {
            TaskState::Pending | TaskState::Placed | TaskState::Deferred | TaskState::Parked => {
                BoardColumn::Todo
            }
            TaskState::Running | TaskState::Stalled => BoardColumn::InProgress,
            TaskState::Done => BoardColumn::Done,
            TaskState::Failed => {
                if self.placement.is_some() {
                    BoardColumn::InProgress
                } else {
                    BoardColumn::Todo
                }
            }
        }
    }
}

// ─── Placement ledger ──────────────────────────────────────────────────────

/// One rejected candidate and why; design §4.4/§5.6.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlacementRejection {
    /// The candidate skipped — a host, `(profile, model)`, or other slot id.
    pub candidate: String,
    /// Machine-readable-ish reason (`capacity`, `cooling`, `requires`, …).
    pub reason: String,
}

/// Kind of placement-ledger event.
pub const PLACEMENT_KIND_DISPATCH: &str = "dispatch";
/// Model switch (only legal off a family-level window; §4.6).
pub const PLACEMENT_KIND_SWITCH_MODEL: &str = "switch-model";
/// Parked pending a supply window reset; §4.6.
pub const PLACEMENT_KIND_PARK: &str = "park";
/// Placement withdrawn (unplace/replace).
pub const PLACEMENT_KIND_UNPLACE: &str = "unplace";

/// One placement-ledger row; design §2.2 ⑥/§5.6.
///
/// `reasons` / `rejected` are both the audit trail and the future bot card
/// body — the two never diverge because there is no second representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlacementLedgerRow {
    /// `plc_…` row id.
    pub id: Id,
    /// Task this placement belongs to.
    pub task_id: TaskId,
    /// Project slice copied for cross-project listings.
    pub project_id: ProjectId,
    /// One of the `PLACEMENT_KIND_*` constants.
    pub kind: String,
    /// Chosen host, when one was chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<HostId>,
    /// Worker instance bound by the placement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<InstanceId>,
    /// Harness kind (`claude` / `codex` / …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// Model pinned/selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Worktree branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Positive reasons the chosen slot was chosen (the card body).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    /// Candidates rejected and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejected: Vec<PlacementRejection>,
    /// Device that recorded the row.
    pub created_by: String,
    /// Row timestamp.
    pub created_at: Timestamp,
}

// ─── Path ownership: globs, claims, diff scope ─────────────────────────────

/// Match a repo-relative path against an ownership glob.
///
/// Supported syntax, deliberately small (a subset of gitignore pathspecs):
/// - `?` matches one non-`/` byte;
/// - `*` matches any run of non-`/` bytes;
/// - `**` matches across directory boundaries (`crates/x/**`, `a/**/b.rs`);
/// - everything else is a literal byte match.
///
/// Both arguments must be repo-relative (no leading `./` or `/`).
pub fn glob_match(pattern: &str, path: &str) -> bool {
    fn rec(p: &[u8], s: &[u8]) -> bool {
        match p.split_first() {
            None => s.is_empty(),
            Some((b'?', rest)) => match s.split_first() {
                Some((c, cs)) if *c != b'/' => rec(rest, cs),
                _ => false,
            },
            Some((b'*', rest)) => {
                if rest.first() == Some(&b'*') {
                    // `**` — cross-directory.
                    let after = &rest[1..];
                    match after.first() {
                        None => true,
                        Some(b'/') => {
                            let tail = &after[1..];
                            // Zero directories, or consume one more byte.
                            rec(tail, s) || (!s.is_empty() && rec(p, &s[1..]))
                        }
                        Some(_) => {
                            // `**` not on a slash boundary; behave like `*`.
                            rec(after, s)
                                || matches!(s.split_first(), Some((c, _)) if *c != b'/' && rec(p, &s[1..]))
                        }
                    }
                } else {
                    // `*` — within one segment.
                    rec(rest, s)
                        || matches!(s.split_first(), Some((c, cs)) if *c != b'/' && rec(p, cs))
                }
            }
            Some((pc, rest)) => {
                matches!(s.split_first(), Some((sc, ss)) if sc == pc && rec(rest, ss))
            }
        }
    }
    rec(pattern.as_bytes(), path.as_bytes())
}

/// Normalise one ownership glob: trim, drop `./`, turn a bare directory
/// (`foo/`) into the subtree pattern `foo/**`.
pub fn normalize_glob(raw: &str) -> String {
    let mut pattern = raw.trim().replace('\\', "/");
    while let Some(stripped) = pattern.strip_prefix("./") {
        pattern = stripped.to_string();
    }
    let pattern = pattern.trim_start_matches('/');
    if let Some(dir) = pattern.strip_suffix('/') {
        if dir.is_empty() {
            "**".to_string()
        } else {
            format!("{dir}/**")
        }
    } else if pattern.is_empty() {
        "**".to_string()
    } else {
        pattern.to_string()
    }
}

/// Normalise a set of claim globs, dropping empties.
pub fn normalize_globs(patterns: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = patterns
        .into_iter()
        .map(|raw| normalize_glob(&raw))
        .filter(|pattern| !pattern.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

fn wildcard_prefix(pattern: &str) -> &str {
    pattern
        .find(['*', '?'])
        .map(|idx| &pattern[..idx])
        .unwrap_or(pattern)
}

/// Segment-wise prefix: `crates/x` contains `crates/x/y.rs` but not
/// `crates/xenial`.
fn segment_contains(container: &str, path: &str) -> bool {
    container == path || path.starts_with(&format!("{container}/"))
}

/// Conservative claim conflict: two patterns can both match the same path.
///
/// Glob/glob conflicts are decided from wildcard-free prefixes, so this may
/// over-report (two disjoint prefix branches sharing a literal prefix never
/// collide in practice but are flagged); over-reporting is the safe direction
/// for a claim registry — the coordinator releases or narrows explicitly.
pub fn patterns_conflict(a: &str, b: &str) -> bool {
    let a = normalize_glob(a);
    let b = normalize_glob(b);
    if a == b {
        return true;
    }
    let a_lit = !a.contains(['*', '?']);
    let b_lit = !b.contains(['*', '?']);
    match (a_lit, b_lit) {
        (true, true) => segment_contains(&a, &b) || segment_contains(&b, &a),
        (true, false) => glob_match(&b, &a) || b.starts_with(&format!("{a}/")),
        (false, true) => glob_match(&a, &b) || a.starts_with(&format!("{b}/")),
        (false, false) => {
            let pa = wildcard_prefix(&a);
            let pb = wildcard_prefix(&b);
            pa == pb || pa.starts_with(pb) || pb.starts_with(pa)
        }
    }
}

/// One conflicting claim pair `(new pattern, existing pattern)`.
pub fn claim_conflicts(existing: &[String], candidate: &[String]) -> Vec<(String, String)> {
    let mut conflicts = Vec::new();
    for new in candidate {
        let new = normalize_glob(new);
        for held in existing {
            if patterns_conflict(&new, held) {
                conflicts.push((new.clone(), held.clone()));
            }
        }
    }
    conflicts
}

/// Result of reconciling a diff against a task's `owns[]`; design §4.3
/// (`scopeCheck.diffMustStayWithin: owns`) — the single highest-value new
/// primitive (playbook), and the pure function the later merge gate calls.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DiffScopeCheck {
    /// True when every changed path is covered by an ownership glob.
    pub within: bool,
    /// Number of changed paths examined.
    pub checked: usize,
    /// Changed paths matched by none of the globs; empty means within scope.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub violations: Vec<String>,
}

/// Reconcile changed paths against ownership globs.
///
/// Semantic review of the diff is still the LLM's job; this catches *paths*
/// only (design §7 #3).
pub fn check_paths_within_owns(owns: &[String], changed: &[String]) -> DiffScopeCheck {
    let owns = normalize_globs(owns.to_vec());
    let mut seen = std::collections::BTreeSet::new();
    let mut violations = Vec::new();
    for raw in changed {
        if raw.trim().is_empty() {
            continue;
        }
        let path = normalize_glob(raw);
        if !seen.insert(path.clone()) {
            continue;
        }
        if !owns.iter().any(|pattern| glob_match(pattern, &path)) {
            violations.push(path);
        }
    }
    DiffScopeCheck {
        within: violations.is_empty(),
        checked: seen.len(),
        violations,
    }
}

/// Extract changed paths from a unified diff, a `--stat` listing, a
/// `--name-only` listing, or NUL-separated (`-z`) paths.
///
/// This parses formats, not semantics: deletions and renames count as changed
/// paths, because the gate owns the whole diff.
pub fn parse_diff_paths(diff: &str) -> Vec<String> {
    if diff.contains('\0') {
        return diff
            .split('\0')
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(normalize_glob)
            .collect();
    }
    let mut paths = Vec::new();
    for line in diff.lines() {
        let line = line.trim_end();
        if line.is_empty()
            || line.starts_with("@@")
            || line.starts_with("index ")
            || line.starts_with("new file mode")
            || line.starts_with("deleted file mode")
            || line.starts_with("similarity index")
            || line.starts_with("rename from ")
            || line.starts_with("Binary files")
        {
            continue;
        }
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // `diff --git a/PATH b/PATH`
            if let Some((_, b)) = rest.split_once(' ')
                && let Some(path) = b.strip_prefix("b/")
            {
                paths.push(normalize_glob(path));
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            if rest == "/dev/null" {
                continue;
            }
            if let Some(path) = rest.strip_prefix("b/").or(Some(rest)) {
                paths.push(normalize_glob(path));
            }
            continue;
        }
        if line.starts_with("--- ") || line.starts_with('+') || line.starts_with('-') {
            continue;
        }
        if let Some((path, _stat)) = line.split_once(" | ") {
            // `git diff --stat` body: ` path | 12 ++--`.
            paths.push(normalize_glob(path.trim()));
            continue;
        }
        // `git diff --name-only`: one bare path per line. Paths with spaces
        // are indistinguishable from stray text here; use `-z` input for those.
        if !line.chars().any(char::is_whitespace) {
            paths.push(normalize_glob(line));
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_machine_legal_and_illegal_transitions_are_fixed() {
        use TaskState::*;
        // A representative legal lifecycle.
        assert!(Pending.can_transition_to(Placed));
        assert!(Placed.can_transition_to(Running));
        assert!(Running.can_transition_to(Stalled));
        assert!(Stalled.can_transition_to(Running));
        assert!(Running.can_transition_to(Done));
        // Recovery paths.
        assert!(Running.can_transition_to(Parked));
        assert!(Parked.can_transition_to(Running));
        assert!(Pending.can_transition_to(Deferred));
        assert!(Deferred.can_transition_to(Placed));
        // Illegal jumps: states cannot skip edges.
        for illegal in [Running, Done, Stalled, Parked] {
            assert!(!Pending.can_transition_to(illegal), "pending→{illegal:?}");
        }
        assert!(!Done.can_transition_to(Running), "done is terminal");
        assert!(!Failed.can_transition_to(Pending), "failed is terminal");
        assert!(
            !Stalled.can_transition_to(Done),
            "must recover to running first"
        );
        assert!(!Deferred.can_transition_to(Done));
        assert!(Done.is_terminal());
        assert!(Failed.is_terminal());
        assert!(!Running.is_terminal());
    }

    #[test]
    fn board_column_projects_the_eight_states_onto_three_columns() {
        use TaskState::*;
        let now = Timestamp::try_from("2026-09-20T10:00:00.000Z".to_string()).unwrap();
        let mut task = Task::new_root(
            now,
            ProjectId::new(),
            "board".into(),
            "project the states".into(),
            TaskClass::Implement,
            vec![],
            vec![],
            TaskBudget::default(),
        );
        // No placement yet: the four to-do states and a pre-flight failure.
        for state in [Pending, Placed, Deferred, Parked, Failed] {
            task.state = state;
            task.placement = None;
            assert_eq!(
                task.board_column(),
                BoardColumn::Todo,
                "{state:?} with no placement"
            );
        }
        // The working states; a failure after dispatch keeps the in-progress
        // column but the state stays `failed` for the badge.
        for state in [Running, Stalled] {
            task.state = state;
            assert_eq!(task.board_column(), BoardColumn::InProgress);
        }
        task.state = Failed;
        task.placement = Some(TaskPlacementRef::default());
        assert_eq!(task.board_column(), BoardColumn::InProgress);
        task.placement = None;
        assert_eq!(task.board_column(), BoardColumn::Todo);
        // Only done reads as done.
        task.state = Done;
        assert_eq!(task.board_column(), BoardColumn::Done);
    }

    #[test]
    fn board_column_archive_is_orthogonal_to_state_and_skipped_on_wire() {
        let now = Timestamp::try_from("2026-09-20T10:00:00.000Z".to_string()).unwrap();
        let mut task = Task::new_root(
            now.clone(),
            ProjectId::new(),
            "archive".into(),
            "archive without moving state".into(),
            TaskClass::Implement,
            vec![],
            vec![],
            TaskBudget::default(),
        );
        // A row without archived_at serialises without the key: old rows stay
        // byte-identical (plan acceptance 6).
        assert!(task.archived_at.is_none());
        assert!(!serde_json::to_string(&task).unwrap().contains("archivedAt"));
        assert_eq!(task.board_column(), BoardColumn::Todo);

        // Archiving wins over every state and never changes the state itself.
        task.archived_at = Some(now);
        for state in [
            TaskState::Pending,
            TaskState::Running,
            TaskState::Done,
            TaskState::Failed,
        ] {
            task.state = state;
            assert_eq!(task.board_column(), BoardColumn::Archived);
        }
        assert_eq!(task.state, TaskState::Failed, "state is untouched");
        assert!(
            TaskState::Failed.is_terminal(),
            "the state machine stays authoritative"
        );

        // An old document without the field still decodes as unarchived.
        let legacy = serde_json::json!({
            "id": TaskId::new(),
            "revision": "1",
            "createdAt": "2026-09-20T10:00:00.000Z",
            "updatedAt": "2026-09-20T10:00:00.000Z",
            "projectId": ProjectId::new(),
            "title": "legacy",
            "mandate": { "chain": [] },
            "class": "implement",
            "state": "running"
        });
        let decoded: Task = serde_json::from_value(legacy).unwrap();
        assert_eq!(decoded.state, TaskState::Running);
        assert!(decoded.archived_at.is_none());
        assert_eq!(decoded.board_column(), BoardColumn::InProgress);
    }

    #[test]
    fn workspace_binding_lives_in_doc_json_with_zero_migration() {
        let now = Timestamp::try_from("2026-09-21T10:00:00.000Z".to_string()).unwrap();
        let mut task = Task::new_root(
            now,
            ProjectId::new(),
            "bind".into(),
            "choose a directory".into(),
            TaskClass::Implement,
            vec![],
            vec![],
            TaskBudget::default(),
        );
        // A pre-bind row serialises without the key: old doc_json stays
        // byte-identical (plan task 3 acceptance 3).
        assert!(task.workspace_binding.is_none());
        assert!(
            !serde_json::to_string(&task)
                .unwrap()
                .contains("workspaceBinding")
        );

        // A reuse-to-root binding decodes and folds to the root dir key.
        let binding: TaskSpaceBinding = serde_json::from_value(serde_json::json!({
            "mode": "reuse",
            "hostId": HostId::new(),
            "workspaceId": WorkspaceId::new(),
        }))
        .unwrap();
        assert_eq!(binding.mode, TaskBindingMode::Reuse);
        assert!(binding.is_root());
        assert_eq!(binding.dir_key(), ".");
        assert_eq!(binding.lease_path_name(), "-");
        task.workspace_binding = Some(binding);
        let wire = serde_json::to_string(&task).unwrap();
        assert!(wire.contains("workspaceBinding"));

        // An old document without the field still decodes as unbound.
        let legacy = serde_json::json!({
            "id": TaskId::new(),
            "revision": "1",
            "createdAt": "2026-09-21T10:00:00.000Z",
            "updatedAt": "2026-09-21T10:00:00.000Z",
            "projectId": ProjectId::new(),
            "title": "legacy",
            "mandate": { "chain": [] },
            "class": "implement",
            "state": "pending"
        });
        let decoded: Task = serde_json::from_value(legacy).unwrap();
        assert!(decoded.workspace_binding.is_none());

        // A pool binding names its slot and addresses the lease route by it.
        let pool: TaskSpaceBinding = serde_json::from_value(serde_json::json!({
            "mode": "pool",
            "hostId": HostId::new(),
            "workspaceId": WorkspaceId::new(),
            "worktreeName": "alpha-s1",
            "branch": "wt/alpha-s1/tsk-x"
        }))
        .unwrap();
        assert!(!pool.is_root());
        assert_eq!(pool.dir_key(), "alpha-s1");
        assert_eq!(pool.lease_path_name(), "alpha-s1");
    }

    #[test]
    fn mandate_chain_carries_owner_words_to_depth_three() {
        let now = Timestamp::try_from("2026-09-15T10:00:00.000Z".to_string()).unwrap();
        let root = Task::new_root(
            now.clone(),
            ProjectId::new(),
            "owner goal".into(),
            "把手工 coordinator loop 变成产品".into(),
            TaskClass::Implement,
            vec![],
            vec![],
            TaskBudget::default(),
        );
        let child = root.new_child(
            now.clone(),
            "task ledger".into(),
            "实现 task ledger 与 owns 检查".into(),
            TaskClass::Implement,
            vec![],
            vec![],
            TaskBudget::default(),
        );
        let grandchild = child.new_child(
            now,
            "state machine".into(),
            "实现合法/非法状态迁移".into(),
            TaskClass::Implement,
            vec![],
            vec![],
            TaskBudget::default(),
        );
        // Golden: a depth-2 node still sees the owner's exact words.
        assert_eq!(root.depth(), 0);
        assert_eq!(child.depth(), 1);
        assert_eq!(grandchild.depth(), 2);
        let chain = &grandchild.mandate.chain;
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].depth, 0);
        assert_eq!(chain[0].task_id, root.meta.id);
        assert_eq!(chain[0].intent, "把手工 coordinator loop 变成产品");
        assert_eq!(chain[1].depth, 1);
        assert_eq!(chain[1].task_id, child.meta.id);
        assert_eq!(chain[1].intent, "实现 task ledger 与 owns 检查");
        assert_eq!(chain[2].depth, 2);
        assert_eq!(chain[2].intent, "实现合法/非法状态迁移");
        assert_eq!(
            grandchild.mandate.owner_intent(),
            Some("把手工 coordinator loop 变成产品")
        );
        assert_eq!(
            grandchild.mandate.own_intent(),
            Some("实现合法/非法状态迁移")
        );
        assert_eq!(grandchild.parent_task_id, Some(child.meta.id));
        assert_eq!(grandchild.project_id, root.project_id);
    }

    #[test]
    fn deps_unlock_only_by_landed_sha_never_worker_done() {
        let now = Timestamp::try_from("2026-09-15T10:00:00.000Z".to_string()).unwrap();
        let mut dep = Task::new_root(
            now.clone(),
            ProjectId::new(),
            "dep".into(),
            "first".into(),
            TaskClass::Implement,
            vec![],
            vec![],
            TaskBudget::default(),
        );
        let mut dependent = dep.new_child(
            now,
            "next".into(),
            "second".into(),
            TaskClass::Implement,
            vec![],
            vec![TaskDep {
                task_id: dep.meta.id.clone(),
                note: Some("API first".into()),
            }],
            TaskBudget::default(),
        );
        // Worker claims DONE: no sha → edge stays locked.
        dep.state = TaskState::Done;
        assert_eq!(dependent.locked_deps([&dep]), vec![dep.meta.id.clone()]);
        // Land records the sha → edge unlocks.
        dep.landed_sha = Some("0123456789abcdef".into());
        assert!(dependent.locked_deps([&dep]).is_empty());
        // A missing task id never silently unlocks.
        dependent.deps.push(TaskDep {
            task_id: TaskId::new(),
            note: None,
        });
        assert_eq!(dependent.locked_deps([&dep]).len(), 1);
    }

    #[test]
    fn glob_matching_covers_stars_and_recursive_stars() {
        assert!(glob_match(
            "crates/remuda-hub/src/tasks.rs",
            "crates/remuda-hub/src/tasks.rs"
        ));
        assert!(!glob_match(
            "crates/remuda-hub/src/tasks.rs",
            "crates/other.rs"
        ));
        assert!(glob_match("*.rs", "tasks.rs"));
        assert!(!glob_match("*.rs", "src/tasks.rs"));
        assert!(glob_match(
            "crates/*/src/lib.rs",
            "crates/remuda-hub/src/lib.rs"
        ));
        assert!(!glob_match("crates/*/src/lib.rs", "crates/a/b/src/lib.rs"));
        assert!(glob_match("crates/**", "crates/remuda-hub/src/tasks.rs"));
        assert!(glob_match("crates/**", "crates/x.rs"));
        assert!(glob_match("**/tasks.rs", "crates/remuda-hub/src/tasks.rs"));
        assert!(glob_match("crates/**/tasks.rs", "crates/tasks.rs"));
        assert!(glob_match("crates/**/tasks.rs", "crates/a/b/tasks.rs"));
        assert!(glob_match("a?.rs", "a1.rs"));
        assert!(!glob_match("a?.rs", "a/b.rs"));
        assert_eq!(normalize_glob("foo/"), "foo/**");
        assert_eq!(normalize_glob("./crates/x.rs"), "crates/x.rs");
    }

    #[test]
    fn claim_conflict_detection() {
        let existing = vec![
            "crates/remuda-hub/src/**".to_string(),
            "docs/design/coordinator-hierarchy.md".to_string(),
        ];
        // Overlapping subtree and overlapping single file both conflict.
        let conflicts = claim_conflicts(&existing, &["crates/remuda-hub/src/tasks.rs".into()]);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(
            conflicts[0].1, "crates/remuda-hub/src/**",
            "file inside claimed subtree"
        );
        let conflicts = claim_conflicts(&existing, &["docs/**".into()]);
        assert_eq!(conflicts.len(), 1);
        // An unrelated tree is clean.
        assert!(claim_conflicts(&existing, &["web/src/**".into()]).is_empty());
        // Literal directory containment conflicts; a same-prefix sibling does
        // not via the literal path (segment-wise containment).
        assert!(patterns_conflict("crates/x", "crates/x/y.rs"));
        assert!(!patterns_conflict("crates/x", "crates/xenial/y.rs"));
        // Conservative glob/glob call: shared literal prefix flags it.
        assert!(patterns_conflict("crates/x/**", "crates/x/*"));
        assert!(!patterns_conflict("crates/a/**", "crates/b/**"));
    }

    #[test]
    fn diff_scope_check_flags_paths_crossing_the_boundary() {
        let owns = vec![
            "crates/remuda-protocol/src/task.rs".to_string(),
            "crates/remuda-hub/src/tasks.rs".to_string(),
        ];
        let diff = "\
diff --git a/crates/remuda-hub/src/tasks.rs b/crates/remuda-hub/src/tasks.rs
index 1111..2222 100644
--- a/crates/remuda-hub/src/tasks.rs
+++ b/crates/remuda-hub/src/tasks.rs
@@ -1 +1 @@
-old
+new
diff --git a/crates/remuda/src/cmd/merge.rs b/crates/remuda/src/cmd/merge.rs
--- a/crates/remuda/src/cmd/merge.rs
+++ b/crates/remuda/src/cmd/merge.rs
@@ -1 +1 @@
";
        let paths = parse_diff_paths(diff);
        assert_eq!(
            paths,
            [
                "crates/remuda-hub/src/tasks.rs",
                "crates/remuda/src/cmd/merge.rs"
            ]
        );
        let check = check_paths_within_owns(&owns, &paths);
        assert!(!check.within);
        assert_eq!(check.checked, 2);
        assert_eq!(check.violations, ["crates/remuda/src/cmd/merge.rs"]);

        // A diff entirely within owns is clean.
        let clean = check_paths_within_owns(
            &["crates/remuda-hub/**".to_string()],
            &["crates/remuda-hub/src/tasks.rs".to_string()],
        );
        assert!(clean.within);
        assert!(clean.violations.is_empty());

        // name-only and stat listings parse too.
        let name_only = parse_diff_paths("crates/a.rs\ncrates/b.rs\n");
        assert_eq!(name_only, ["crates/a.rs", "crates/b.rs"]);
        let stat = parse_diff_paths(" crates/a.rs | 2 +-\n 1 file changed\n");
        assert_eq!(stat, ["crates/a.rs"]);
    }
}
