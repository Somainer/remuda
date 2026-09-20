//! Warm per-project worktree pool: lease / return / refcount (task-model t-pool).
//!
//! Layered above [`crate::worktree`] and deliberately isolated from it:
//! `worktree.rs` keeps the one-shot `worktree.create` / `provision_record`
//! paths, while this module owns the *pooled* lifecycle.
//!
//! # Rules (plan task 2)
//!
//! * Slots rest on a **detached HEAD** at the pool base. A lease creates a
//!   per-task branch `wt/<slot>/<task>` in place, so git's one-branch-per-
//!   worktree rule and `provision_record`'s "branch already exists" refusal
//!   never bite the pool, and returning the slot detaches again.
//! * Lease is an explicit three-way decision: reuse a clean **parked** slot
//!   (warm, no fetch), otherwise provision a new slot while the pool is below
//!   capacity (fetch-first), otherwise **refuse** with `SUPPLY_DEFERRED`.
//!   There is no silent reroute and no dirty reuse (D-035).
//! * `return` for a pool slot (`mode = pool`) refuses while tracked files are
//!   dirty, otherwise clears untracked files (`git clean -fd`, keeping ignored
//!   build dirs warm) and detaches back to the pool base.
//! * A `reuse` lease (an existing standalone worktree or the workspace root
//!   itself) performs **zero** git operations on return: the directory is the
//!   operator's own, and sharing is modelled purely by refcount.
//! * Refcount counts tasks sharing one directory. While a lease is held the
//!   reclaim path in [`crate::worktree::remove_record`] refuses to delete it.

use crate::NodeError;
use crate::worktree::{
    Catalog, WorktreeLeaseState, git, git_common_dir, git_fetch, load_catalog, repo_root,
    resolve_path, save_catalog, upsert, validate_name,
};
use remuda_protocol::TaskId;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Default pool capacity per (host, repo); configurable with
/// `REMUDA_WORKTREE_POOL_SIZE` (D3).
pub(crate) const DEFAULT_POOL_SIZE: usize = 4;

/// Directory key for a lease on the registered workspace root itself
/// (reuse-to-root, no catalog record).
const ROOT_KEY: &str = ".";

/// One entry from `git status --porcelain` (kept for its raw label in errors).
struct StatusEntry {
    raw: String,
}

/// Handle `worktree.lease`.
pub(crate) fn lease(repo: &Path, params: &Value) -> Result<Value, NodeError> {
    let request = LeaseParams::parse(params)?;
    lease_sized(repo, &request, pool_size_from_env())
}

/// Handle `worktree.return`.
pub(crate) fn return_slot(repo: &Path, params: &Value) -> Result<Value, NodeError> {
    let request = LeaseParams::parse(params)?;
    return_sized(repo, &request)
}

/// Capacity override used by tests (the workspace forbids `unsafe`, so tests
/// cannot mutate `REMUDA_WORKTREE_POOL_SIZE`).
fn lease_sized(repo: &Path, request: &LeaseParams, pool_size: usize) -> Result<Value, NodeError> {
    let repo_root = repo_root(Some(repo))?;

    // (1) reuse-to-root: the registered workspace root. No catalog record, no
    // git operation — the Hub lease row carries the refcount.
    if request.is_root() {
        let branch = current_branch(&repo_root)?;
        return Ok(json!({
            "name": ROOT_KEY,
            "path": repo_root.to_string_lossy(),
            "branch": branch,
            "base": Value::Null,
            "mode": "reuse",
            "state": "leased",
            "refcount": 1,
            "warm": true,
            "queued": false,
            "dirKey": ROOT_KEY,
        }));
    }

    validate_name(&request.name)?;
    let git_common = git_common_dir(&repo_root)?;
    let mut catalog = load_catalog(&git_common)?;

    // (2) Exact catalog match: a reuse lease on a standalone worktree, or a
    // warm lease on a parked pool slot reached by its slot name.
    if let Some(index) = catalog
        .worktrees
        .iter()
        .position(|row| row.name == request.name)
    {
        let record = catalog.worktrees[index].clone();
        if !PathBuf::from(&record.path).is_dir() {
            return Err(NodeError::InvalidRequest(format!(
                "worktree {} is recorded but its directory is gone; reconcile first",
                request.name
            )));
        }
        if record.state == WorktreeLeaseState::Parked {
            // Parked pool slot by exact name: warm branch switch, no fetch.
            let warm = checkout_task_branch(&repo_root, &record, &request.task_id)?;
            let row = &mut catalog.worktrees[index];
            row.state = WorktreeLeaseState::Leased;
            row.branch = warm.branch.clone();
            row.leased_by = vec![request.task_id.clone()];
            save_catalog(&git_common, &catalog)?;
            return Ok(lease_payload("pool", &warm, 1, true, false, &request.name));
        }
        return reuse_record(&mut catalog, &git_common, record, request);
    }

    // (3) Pool lease: `name` names the pool; slots are `<name>-s<n>`.
    lease_pool_slot(&repo_root, &git_common, &mut catalog, request, pool_size)
}

/// Attach a task to an already-leased directory without a branch switch: a
/// second task shares by refcount but is reported queued/blocked — sharing is
/// serial, never concurrent (attach-lock semantics). The mode follows the
/// record itself (a pool slot stays `pool` even while shared).
fn reuse_record(
    catalog: &mut Catalog,
    git_common: &Path,
    mut record: crate::worktree::WorktreeRecord,
    request: &LeaseParams,
) -> Result<Value, NodeError> {
    let already_held = record.leased_by.contains(&request.task_id);
    let queued = !record.leased_by.is_empty() && !already_held;
    if !already_held {
        record.leased_by.push(request.task_id.clone());
    }
    record.state = WorktreeLeaseState::Leased;
    let refcount = record.leased_by.len();
    let path = record.path.clone();
    let branch = record.branch.clone();
    let mode = record_mode(&record);
    upsert(catalog, record);
    save_catalog(git_common, catalog)?;
    let mut payload = json!({
        "name": request.name,
        "path": path,
        "branch": branch,
        "base": Value::Null,
        "mode": mode,
        "state": "leased",
        "refcount": refcount,
        "warm": true,
        "queued": queued,
        "dirKey": request.name,
    });
    if queued {
        payload["blocked"] = json!({
            "reason": "directory is held by another attached task; queued for serial reuse"
        });
    }
    Ok(payload)
}

/// The three-way pool decision for pool name `request.name`.
fn lease_pool_slot(
    repo_root: &Path,
    git_common: &Path,
    catalog: &mut Catalog,
    request: &LeaseParams,
    pool_size: usize,
) -> Result<Value, NodeError> {
    let pool = &request.name;
    let start_point = request.start_point();

    // Self-heal stale catalog rows before deciding.
    let _ = reconcile_with(repo_root, catalog);

    let mut slot_indexes: Vec<usize> = catalog
        .worktrees
        .iter()
        .enumerate()
        .filter_map(|(index, row)| slot_number(pool, &row.name).map(|_| index))
        .collect();

    // Decision 1: a clean parked slot on this base is a warm hit.
    let mut parked_warm: Option<usize> = None;
    let mut parked_other_base: Vec<usize> = Vec::new();
    for &index in &slot_indexes {
        let row = &catalog.worktrees[index];
        if row.state != WorktreeLeaseState::Parked {
            continue;
        }
        if row.base != start_point {
            parked_other_base.push(index);
            continue;
        }
        // Never reuse a dirty slot, even one that claims to be parked.
        if working_tree_status(&PathBuf::from(&row.path), true)?.is_empty() {
            parked_warm = Some(index);
            break;
        }
        tracing::warn!(slot = %row.name, "parked slot is dirty; it is not a warm candidate");
    }

    if let Some(index) = parked_warm {
        // Warm hit: create the per-task branch in place. No fetch (E4).
        let record = catalog.worktrees[index].clone();
        let warm = checkout_task_branch(repo_root, &record, &request.task_id)?;
        let row = &mut catalog.worktrees[index];
        row.state = WorktreeLeaseState::Leased;
        row.branch = warm.branch.clone();
        row.leased_by = vec![request.task_id.clone()];
        save_catalog(git_common, catalog)?;
        return Ok(lease_payload("pool", &warm, 1, true, false, &record.name));
    }

    // Count *live* slots (reconcile only drops catalog rows; the on-disk set is
    // what the capacity bounds).
    slot_indexes.retain(|&index| PathBuf::from(&catalog.worktrees[index].path).is_dir());

    if slot_indexes.len() >= pool_size {
        // D3: before refusing, evict a clean, idle, refcount-0 slot parked on a
        // different base to make room. A leased or dirty slot is never evicted.
        if let Some(victim) = parked_other_base.into_iter().find(|&index| {
            let row = &catalog.worktrees[index];
            row.leased_by.is_empty()
                && working_tree_status(&PathBuf::from(&row.path), true)
                    .map(|entries| entries.is_empty())
                    .unwrap_or(false)
        }) {
            evict_slot(repo_root, catalog, victim)?;
            // `evict_slot` removed a row, shifting every later position: the
            // held indexes are now stale, so recompute from the new catalog.
            slot_indexes = catalog
                .worktrees
                .iter()
                .enumerate()
                .filter_map(|(index, row)| slot_number(pool, &row.name).map(|_| index))
                .collect();
            slot_indexes.retain(|&index| PathBuf::from(&catalog.worktrees[index].path).is_dir());
        }
    }

    if slot_indexes.len() >= pool_size {
        // Decision 3: pool full and nothing clean to reuse. Refuse, never
        // reroute to another directory or to the main checkout (D-035).
        return Ok(json!({
            "deferred": true,
            "code": "SUPPLY_DEFERRED",
            "reason": format!(
                "worktree pool '{pool}' is full ({pool_size} slots) and no clean parked slot is available"
            ),
            "poolSize": pool_size,
        }));
    }

    // Decision 2: provision a new slot below capacity. Fetch-first: a fresh
    // slot branches from the current remote ref, unlike a warm hit.
    let used: BTreeSet<usize> = slot_indexes
        .iter()
        .map(|&index| slot_number(pool, &catalog.worktrees[index].name).unwrap_or(0))
        .collect();
    let number = (1..=pool_size)
        .find(|n| !used.contains(n))
        .ok_or_else(|| NodeError::InvalidRequest("could not allocate a pool slot number".into()))?;
    // `-s<n>` is the reserved pool-slot suffix: it lets the return path tell a
    // pool slot (`alpha-s1`) from a standalone worktree (`agent-one`) without
    // an extra catalog column, and keeps branch names on `wt/<slot>/…`.
    let slot_name = format!("{pool}-s{number}");
    validate_name(&slot_name)?;
    let abs_path = resolve_path(repo_root, &slot_name, None)?;
    if let Some(parent) = abs_path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            NodeError::InvalidRequest(format!("create {}: {err}", parent.display()))
        })?;
    }
    git_fetch(repo_root)?;
    // Provision detached: the slot carries no branch while it rests.
    git(
        repo_root,
        &[
            "worktree",
            "add",
            "--detach",
            &abs_path.to_string_lossy(),
            &start_point,
        ],
    )?;
    let path = abs_path
        .canonicalize()
        .unwrap_or(abs_path)
        .to_string_lossy()
        .into_owned();
    let record = crate::worktree::WorktreeRecord {
        name: slot_name.clone(),
        path: path.clone(),
        branch: "HEAD".into(),
        base: start_point.clone(),
        state: WorktreeLeaseState::Parked,
        leased_by: Vec::new(),
    };
    upsert(catalog, record.clone());
    save_catalog(git_common, catalog)?;

    let warm = checkout_task_branch(repo_root, &record, &request.task_id)?;
    // Reload the row upserted under `slot_name` and mark it leased.
    let index = catalog
        .worktrees
        .iter()
        .position(|row| row.name == slot_name)
        .expect("slot just upserted");
    catalog.worktrees[index].state = WorktreeLeaseState::Leased;
    catalog.worktrees[index].branch = warm.branch.clone();
    catalog.worktrees[index].leased_by = vec![request.task_id.clone()];
    save_catalog(git_common, catalog)?;
    Ok(lease_payload("pool", &warm, 1, false, false, &slot_name))
}

/// Handle `worktree.return` (refcount decrement, and park/reset for pool slots).
fn return_sized(repo: &Path, request: &LeaseParams) -> Result<Value, NodeError> {
    let repo_root = repo_root(Some(repo))?;

    // reuse-to-root: nothing to do on disk, ever.
    if request.is_root() {
        return Ok(json!({
            "name": ROOT_KEY,
            "mode": "reuse",
            "state": "free",
            "refcount": 0,
            "parked": false,
            "dirKey": ROOT_KEY,
        }));
    }

    validate_name(&request.name)?;
    let git_common = git_common_dir(&repo_root)?;
    let mut catalog = load_catalog(&git_common)?;
    let Some(index) = catalog
        .worktrees
        .iter()
        .position(|row| row.name == request.name)
    else {
        return Err(NodeError::NotFound {
            entity: "worktree",
            id: request.name.clone(),
        });
    };

    if !catalog.worktrees[index]
        .leased_by
        .contains(&request.task_id)
    {
        return Err(NodeError::Conflict(format!(
            "task {} does not hold a lease on worktree {}",
            request.task_id.as_id(),
            request.name
        )));
    }
    catalog.worktrees[index]
        .leased_by
        .retain(|task| task != &request.task_id);

    // Still shared: decrement only; the directory keeps working for the
    // remaining holders.
    let refcount = catalog.worktrees[index].leased_by.len();
    if refcount > 0 {
        catalog.worktrees[index].state = WorktreeLeaseState::Leased;
        save_catalog(&git_common, &catalog)?;
        return Ok(json!({
            "name": request.name,
            "mode": record_mode(&catalog.worktrees[index]),
            "state": "leased",
            "refcount": refcount,
            "parked": false,
            "dirKey": request.name,
        }));
    }

    let record = catalog.worktrees[index].clone();
    let mode = record_mode(&record);
    let slot_path = PathBuf::from(&record.path);

    // reuse lease (standalone worktree): zero git operations. The tree is the
    // operator's working directory and stays byte for byte; reset/clean/park
    // are pool-only.
    if mode == "reuse" {
        catalog.worktrees[index].state = WorktreeLeaseState::Free;
        save_catalog(&git_common, &catalog)?;
        return Ok(json!({
            "name": request.name,
            "mode": "reuse",
            "state": "free",
            "refcount": 0,
            "parked": false,
            "dirKey": request.name,
        }));
    }

    // pool slot: refuse on tracked dirt, clean untracked, detach to base.
    reset_park(&slot_path, &record.base)?;
    catalog.worktrees[index].state = WorktreeLeaseState::Parked;
    catalog.worktrees[index].branch = "HEAD".into();
    save_catalog(&git_common, &catalog)?;
    Ok(json!({
        "name": request.name,
        "mode": "pool",
        "state": "parked",
        "refcount": 0,
        "parked": true,
        "dirKey": request.name,
    }))
}

/// Whether a record behaves as a pool slot here: parked records and slots with
/// a pool-shaped name reset; a standalone worktree returns to `free`.
/// Whether a catalog record behaves as a pool slot (`pool`) or a standalone
/// reused worktree (`reuse`). A slot rests detached (`Parked` / branch `HEAD`)
/// or carries the reserved `<pool>-s<n>` name; everything else is standalone.
fn record_mode(record: &crate::worktree::WorktreeRecord) -> &'static str {
    if record.state == WorktreeLeaseState::Parked
        || record.branch == "HEAD"
        || is_slot_name(&record.name)
    {
        "pool"
    } else {
        "reuse"
    }
}

/// Clean untracked files and detach the slot back at the pool base.
///
/// Tracked modifications or staged changes refuse the return outright (the
/// caller would lose real work); untracked entries are exactly what
/// `git clean -fd` reclaims, while ignored dirs (`target/`, `node_modules/`)
/// stay warm.
fn reset_park(slot_path: &Path, base: &str) -> Result<(), NodeError> {
    let tracked = working_tree_status(slot_path, false)?;
    if !tracked.is_empty() {
        let detail = tracked
            .iter()
            .map(|entry| entry.raw.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(NodeError::Conflict(format!(
            "worktree {} has tracked changes; refusing return/reset: {detail}",
            slot_path.display()
        )));
    }
    // Reclaim untracked leftovers but keep ignored files (warm build dirs).
    git(slot_path, &["clean", "-fd"])?;
    // Detach at the recorded base. This is a local remote-tracking ref
    // (`origin/<base>` captured at provision time) — no fetch.
    if let Err(error) = git(slot_path, &["switch", "--detach", base]) {
        // `switch` rejects detaching with local-only commits on the task
        // branch; surface that honestly rather than parking on the wrong oid.
        return Err(NodeError::Conflict(format!(
            "could not detach worktree {} at {base}: {error}",
            slot_path.display()
        )));
    }
    Ok(())
}

/// Create the per-task branch in a (detached) slot and report the checkout.
struct WarmCheckout {
    name: String,
    path: String,
    branch: String,
    base_oid: String,
}

fn checkout_task_branch(
    repo_root: &Path,
    record: &crate::worktree::WorktreeRecord,
    task: &TaskId,
) -> Result<WarmCheckout, NodeError> {
    let slot_path = PathBuf::from(&record.path);
    // Defense in depth: a dirty slot must never be handed out.
    let dirt = working_tree_status(&slot_path, true)?;
    if !dirt.is_empty() {
        return Err(NodeError::Conflict(format!(
            "worktree {} is dirty; refusing to lease it: {}",
            record.name,
            dirt.iter()
                .map(|e| e.raw.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let branch = format!("wt/{}/{}", record.name, task_branch_slug(task));
    crate::worker::validate_branch(&branch)?;
    if git(
        repo_root,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok()
    {
        // A retry for the same task: the branch exists and the detached slot
        // has it checked out nowhere, so a plain switch is safe.
        git(&slot_path, &["switch", &branch])?;
    } else {
        git(&slot_path, &["switch", "-c", &branch])?;
    }
    let base_oid = git(&slot_path, &["rev-parse", "HEAD"])?;
    Ok(WarmCheckout {
        name: record.name.clone(),
        path: record.path.clone(),
        branch,
        base_oid,
    })
}

/// Branch-safe rendering of a task id (`tsk_<uuid>` → `tsk-<uuid>`, ≤48 chars).
fn task_branch_slug(task: &TaskId) -> String {
    remuda_protocol::slugify(task.as_id().as_str())
}

/// `git -C path status --porcelain`. With `untracked`, `??` entries are
/// included; without it only tracked/staged changes are reported.
fn working_tree_status(path: &Path, untracked: bool) -> Result<Vec<StatusEntry>, NodeError> {
    let mut command = Command::new("git");
    command.arg("-C").arg(path).args(["status", "--porcelain"]);
    if !untracked {
        command.arg("--untracked-files=no");
    }
    let output = crate::workspace_access::bounded_workspace_command(
        &mut command,
        path,
        Duration::from_secs(15),
    )?;
    if !output.status.success() {
        return Err(NodeError::InvalidRequest(format!(
            "git status failed in {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| StatusEntry {
            raw: line.to_string(),
        })
        .collect())
}

/// Remove a pool slot physically (only ever called on clean, idle,
/// refcount-0 slots; D3).
fn evict_slot(repo_root: &Path, catalog: &mut Catalog, index: usize) -> Result<(), NodeError> {
    let record = catalog.worktrees[index].clone();
    tracing::info!(slot = %record.name, "evicting clean idle pool slot to free capacity");
    let _ = git(repo_root, &["worktree", "remove", &record.path]);
    let _ = std::fs::remove_dir_all(&record.path);
    let _ = git(repo_root, &["worktree", "prune"]);
    catalog.worktrees.retain(|row| row.name != record.name);
    Ok(())
}

/// Report of a catalog [`reconcile`] pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ReconcileReport {
    pub(crate) dropped_missing: usize,
    pub(crate) healed_leased: usize,
}

/// Reconcile the catalog against the worktrees actually on disk.
///
/// Rows whose directory is gone (or which git no longer lists) are dropped;
/// rows stuck in `leased` with no holders are healed to `parked` (detached
/// pool slot) or `free` (standalone). Exercised by the pool on every lease
/// via [`reconcile_with`], and exposed here for the restart reconciliation
/// the binding task wires to Node startup.
#[allow(dead_code)]
pub(crate) fn reconcile(repo: &Path) -> Result<ReconcileReport, NodeError> {
    let repo_root = repo_root(Some(repo))?;
    let git_common = git_common_dir(&repo_root)?;
    let mut catalog = load_catalog(&git_common)?;
    let report = reconcile_with(&repo_root, &mut catalog);
    save_catalog(&git_common, &catalog)?;
    Ok(report)
}

fn reconcile_with(repo_root: &Path, catalog: &mut Catalog) -> ReconcileReport {
    let mut report = ReconcileReport::default();
    let listed = listed_worktrees(repo_root).unwrap_or_default();
    let mut survivors = Vec::new();
    for record in std::mem::take(&mut catalog.worktrees) {
        let path = PathBuf::from(&record.path);
        let on_disk = path.is_dir() && listed.iter().any(|item| item == &record.path);
        if !on_disk {
            report.dropped_missing += 1;
            continue;
        }
        let mut record = record;
        if record.state == WorktreeLeaseState::Leased && record.leased_by.is_empty() {
            record.state = if record.branch == "HEAD" || is_slot_name(&record.name) {
                WorktreeLeaseState::Parked
            } else {
                WorktreeLeaseState::Free
            };
            report.healed_leased += 1;
        }
        survivors.push(record);
    }
    catalog.worktrees = survivors;
    report
}

/// Absolute paths git currently lists as worktrees.
fn listed_worktrees(repo_root: &Path) -> Result<Vec<String>, NodeError> {
    let out = git(repo_root, &["worktree", "list", "--porcelain"])?;
    let mut paths = Vec::new();
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            paths.push(rest.to_string());
        }
    }
    Ok(paths)
}

fn current_branch(repo_root: &Path) -> Result<String, NodeError> {
    match git(repo_root, &["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        Ok(branch) => Ok(branch),
        Err(_) => Ok("HEAD".into()),
    }
}

/// Parse the pool slot number from `<pool>-s<n>`.
fn slot_number(pool: &str, name: &str) -> Option<usize> {
    let suffix = name.strip_prefix(&format!("{pool}-s"))?;
    if suffix.is_empty() {
        return None;
    }
    suffix.parse::<usize>().ok().filter(|n| *n >= 1)
}

/// Whether a name has the reserved pool-slot shape `<pool>-s<n>`.
fn is_slot_name(name: &str) -> bool {
    match name.rsplit_once("-s") {
        Some((_, number)) => !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

fn lease_payload(
    mode: &str,
    warm: &WarmCheckout,
    refcount: usize,
    warm_hit: bool,
    queued: bool,
    dir_key: &str,
) -> Value {
    json!({
        "name": warm.name,
        "path": warm.path,
        "branch": warm.branch,
        "baseOid": warm.base_oid,
        "mode": mode,
        "state": "leased",
        "refcount": refcount,
        "warm": warm_hit,
        "queued": queued,
        "dirKey": dir_key,
    })
}

fn pool_size_from_env() -> usize {
    std::env::var("REMUDA_WORKTREE_POOL_SIZE")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|size| (1..=16).contains(size))
        .unwrap_or(DEFAULT_POOL_SIZE)
}

/// Validated `worktree.lease` / `worktree.return` params.
///
/// The Hub forwards only `{hostId, workspaceId, name, base, taskId}`
/// (security-review-2 M4): no `path` or `repo` is ever accepted here.
#[derive(Debug)]
struct LeaseParams {
    name: String,
    task_id: TaskId,
    base: Option<String>,
}

impl LeaseParams {
    fn parse(params: &Value) -> Result<Self, NodeError> {
        if params.get("path").is_some() || params.get("repo").is_some() {
            return Err(NodeError::InvalidRequest(
                "worktree lease/return does not accept path or repo; \
                 the Node resolves the directory itself"
                    .into(),
            ));
        }
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if name.is_empty() {
            return Err(NodeError::InvalidRequest(
                "worktree lease/return requires name".into(),
            ));
        }
        let task_id = params
            .get("taskId")
            .cloned()
            .and_then(|value| serde_json::from_value::<TaskId>(value).ok())
            .ok_or_else(|| {
                NodeError::InvalidRequest("worktree lease/return requires taskId".into())
            })?;
        let base = params
            .get("base")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|raw| !raw.is_empty())
            .map(str::to_owned);
        Ok(Self {
            name,
            task_id,
            base,
        })
    }

    fn is_root(&self) -> bool {
        self.name == ROOT_KEY
    }

    /// Git start point for a fresh pool slot. A bare base names the
    /// remote-tracking ref; an already-namespaced ref is used verbatim after
    /// the same traversal checks `worker.provision` applies.
    fn start_point(&self) -> String {
        let raw = self.base.as_deref().unwrap_or("main");
        if raw.contains('/') {
            raw.to_string()
        } else {
            format!("origin/{raw}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    /// Temp repo with a fetchable local origin (same shape as worker tests).
    fn repo_fixture() -> (TempDir, PathBuf) {
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
        // One tracked file plus an ignore rule: dirty/clean tests need a
        // tracked path and a warm ignored build dir.
        fs::write(root.join("seed.txt"), b"seed\n").unwrap();
        fs::write(root.join(".gitignore"), b"target/\nnode_modules/\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);
        run(&["remote", "add", "origin", root.to_str().unwrap()]);
        run(&["fetch", "-q", "origin"]);
        run(&["update-ref", "refs/remotes/origin/main", "refs/heads/main"]);
        (dir, root)
    }

    fn task(_n: u8) -> TaskId {
        // Canonical UUIDv7-shaped ids are validated by the scalar parser; the
        // numeric argument only keeps call sites readable.
        TaskId::new()
    }

    fn lease_at(root: &Path, name: &str, task: &TaskId, size: usize) -> Value {
        lease_sized(
            root,
            &LeaseParams {
                name: name.into(),
                task_id: task.clone(),
                base: None,
            },
            size,
        )
        .expect("lease")
    }

    fn give_back(root: &Path, name: &str, task: &TaskId) -> Value {
        return_slot(
            root,
            &json!({ "name": name, "taskId": task.as_id().as_str() }),
        )
        .expect("return")
    }

    #[test]
    fn lease_and_return_round_trip_warms_a_slot() {
        let (_keep, root) = repo_fixture();
        let t1 = task(1);
        let first = lease_at(&root, "alpha", &t1, 2);
        assert_eq!(first["mode"], "pool");
        assert_eq!(
            first["warm"], false,
            "the first slot is provisioned, not warm"
        );
        assert_eq!(
            first["branch"],
            format!("wt/alpha-s1/{}", task_branch_slug(&t1))
        );
        let path = first["path"].as_str().unwrap().to_string();
        assert!(Path::new(&path).join(".git").exists());

        let returned = give_back(&root, "alpha-s1", &t1);
        assert_eq!(returned["state"], "parked");
        assert_eq!(returned["mode"], "pool");

        // Second lease on the same pool is a warm hit with a new per-task
        // branch, in the same directory.
        let t2 = task(2);
        let second = lease_at(&root, "alpha", &t2, 2);
        assert_eq!(second["warm"], true);
        assert_eq!(second["path"], path);
        assert_eq!(
            second["branch"],
            format!("wt/alpha-s1/{}", task_branch_slug(&t2))
        );
    }

    #[test]
    fn warm_hit_does_not_fetch_while_offline() {
        let (_keep, root) = repo_fixture();
        let t1 = task(1);
        let first = lease_at(&root, "alpha", &t1, 2);
        let path = first["path"].as_str().unwrap().to_string();
        give_back(&root, "alpha-s1", &t1);

        // Point origin at an unreachable location: a warm hit must not fetch,
        // so the lease must still succeed.
        git(
            &root,
            &[
                "remote",
                "set-url",
                "origin",
                "file:///nonexistent/offline/origin",
            ],
        )
        .unwrap();

        let t2 = task(2);
        let second = lease_at(&root, "alpha", &t2, 2);
        assert_eq!(second["warm"], true, "{second}");
        assert_eq!(second["path"], path);
    }

    #[test]
    fn full_pool_without_a_clean_slot_defers_instead_of_rerouting() {
        let (_keep, root) = repo_fixture();
        let holders: Vec<TaskId> = (0..2).map(task).collect();
        let first = lease_at(&root, "beta", &holders[0], 2);
        assert_eq!(first["name"], "beta-s1");
        let second = lease_at(&root, "beta", &holders[1], 2);
        assert_eq!(second["name"], "beta-s2");

        let t3 = task(9);
        let refused = lease_sized(
            &root,
            &LeaseParams {
                name: "beta".into(),
                task_id: t3,
                base: None,
            },
            2,
        )
        .expect("a refusal is still an ok response");
        assert_eq!(refused["deferred"], true);
        assert_eq!(refused["code"], "SUPPLY_DEFERRED");
        // No third directory was created and nothing was silently rerouted.
        assert!(
            !root
                .parent()
                .unwrap()
                .join("remuda-wt")
                .join("beta-s3")
                .exists()
        );
    }

    /// D3: when the pool is at capacity but one slot is clean, idle and
    /// refcount-0 yet parked on a different base, it is evicted to make room;
    /// leased or dirty slots are never evicted.
    #[test]
    fn eviction_reclaims_only_a_clean_idle_slot_on_another_base() {
        let (_keep, root) = repo_fixture();
        // A second fetchable base: origin/stable points at the same commit.
        git(
            &root,
            &[
                "update-ref",
                "refs/remotes/origin/stable",
                "refs/heads/main",
            ],
        )
        .unwrap();

        // Pool of 1: lease once on `stable`, return → parked on origin/stable.
        let t1 = task(1);
        let parked = lease_sized(
            &root,
            &LeaseParams {
                name: "theta".into(),
                task_id: t1.clone(),
                base: Some("stable".into()),
            },
            1,
        )
        .expect("lease stable");
        assert_eq!(parked["name"], "theta-s1");
        let old_path = parked["path"].as_str().unwrap().to_string();
        give_back(&root, "theta-s1", &t1);
        assert!(Path::new(&old_path).exists());
        // An ignored file keeps warm across park (clean -fd preserves it), but
        // a physical eviction deletes the whole directory and re-adds empty.
        let marker = PathBuf::from(&old_path).join("target/warm.cache");
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, b"warm").unwrap();

        // Leasing the same pool at the default `main` base evicts the
        // stable-parked slot and provisions a fresh main one in a new dir.
        let t2 = task(2);
        let main_lease = lease_sized(
            &root,
            &LeaseParams {
                name: "theta".into(),
                task_id: t2.clone(),
                base: None,
            },
            1,
        )
        .expect("lease main after eviction");
        assert_eq!(main_lease["name"], "theta-s1");
        assert_eq!(
            main_lease["warm"], false,
            "eviction provisions fresh, fetch-first"
        );
        assert!(
            !marker.exists(),
            "the different-base slot was physically evicted (warm cache destroyed)"
        );
        let new_path = main_lease["path"].as_str().unwrap().to_string();
        assert!(Path::new(&new_path).join(".git").exists());

        // A dirty parked slot is not evicted: a full pool with one dirty
        // different-base slot refuses instead of losing work.
        give_back(&root, "theta-s1", &t2);
        // The parked slot is on main; now request `stable` again at capacity 1.
        // Make the parked main slot dirty with an untracked file — clean -fd
        // would reclaim it on *return*, but eviction must not delete it.
        std::fs::write(PathBuf::from(&new_path).join("stray.txt"), b"untracked").unwrap();
        let t3 = task(3);
        let refused = lease_sized(
            &root,
            &LeaseParams {
                name: "theta".into(),
                task_id: t3,
                base: Some("stable".into()),
            },
            1,
        )
        .expect("refusal is an ok response");
        assert_eq!(refused["deferred"], true);
        assert!(
            PathBuf::from(&new_path).join("stray.txt").exists(),
            "a dirty slot is never evicted"
        );
    }

    #[test]
    fn refcount_sharing_blocks_force_remove_until_returned() {
        let (_keep, root) = repo_fixture();
        let t1 = task(1);
        let leased = lease_at(&root, "gamma", &t1, 4);
        let slot = leased["name"].as_str().unwrap().to_string();
        let path = leased["path"].as_str().unwrap().to_string();

        // A second task shares the standalone... share through the exact slot
        // name: refcount 2 and the answer is queued/blocked, never concurrent.
        let t2 = task(2);
        let shared = lease_at(&root, &slot, &t2, 4);
        assert_eq!(shared["refcount"], 2);
        assert_eq!(shared["queued"], true);
        assert!(shared.get("blocked").is_some());

        // The reclaim main path must not physically remove a shared slot.
        let outcome = crate::worktree::remove_record(&root, &slot).expect("remove");
        assert!(
            !outcome.removed(),
            "a shared worktree must be retained, not force-removed"
        );
        assert!(Path::new(&path).exists());

        give_back(&root, &slot, &t2);
        give_back(&root, &slot, &t1);
        // Returning a pool slot parks it (still on disk); force-remove of an
        // unleased parked record then reclaims as before.
        let removed = crate::worktree::remove_record(&root, &slot).expect("remove after return");
        assert!(removed.removed());
        assert!(!Path::new(&path).exists());
    }

    #[test]
    fn reuse_return_leaves_the_tree_byte_identical() {
        let (_keep, root) = repo_fixture();
        // A standalone worktree created through the non-pool path.
        let created = crate::worktree::handle_rpc(
            &root,
            "worktree.create",
            &json!({ "name": "shared", "base": "main" }),
        )
        .expect("handled")
        .expect("create");
        let path = PathBuf::from(created["path"].as_str().unwrap());

        // Put real content in the operator's directory.
        fs::write(path.join("notes.txt"), b"operator content").unwrap();
        fs::create_dir_all(path.join("scratch")).unwrap();
        fs::write(path.join("scratch/data.bin"), vec![1u8, 2, 3, 4]).unwrap();
        let snapshot = tree_fingerprint(&path);

        let t1 = task(1);
        let leased = lease_at(&root, "shared", &t1, 4);
        assert_eq!(leased["mode"], "reuse");
        assert_eq!(leased["branch"], created["branch"]);
        let t2 = task(2);
        let queued = lease_at(&root, "shared", &t2, 4);
        assert_eq!(queued["refcount"], 2);
        assert_eq!(queued["queued"], true);

        give_back(&root, "shared", &t2);
        let done = give_back(&root, "shared", &t1);
        assert_eq!(done["state"], "free");
        assert_eq!(done["mode"], "reuse");

        // No clean/reset/remove ran: every byte, including untracked files,
        // is exactly as the operator left it.
        assert_eq!(tree_fingerprint(&path), snapshot);
        assert_eq!(
            fs::read(path.join("notes.txt")).unwrap(),
            b"operator content"
        );
    }

    #[test]
    fn reuse_to_root_lease_keys_the_registered_root() {
        let (_keep, root) = repo_fixture();
        let t1 = task(1);
        let leased = lease_at(&root, ".", &t1, 4);
        assert_eq!(leased["mode"], "reuse");
        assert_eq!(leased["dirKey"], ".");
        assert_eq!(leased["name"], ".");
        assert_eq!(
            leased["path"].as_str().unwrap(),
            root.canonicalize().unwrap().to_string_lossy()
        );
        // Root leasing creates no catalog record.
        let git_common = git_common_dir(&root).unwrap();
        let catalog = load_catalog(&git_common).unwrap();
        assert!(catalog.worktrees.is_empty());

        let returned = give_back(&root, ".", &t1);
        assert_eq!(returned["mode"], "reuse");
        assert_eq!(returned["state"], "free");
    }

    #[test]
    fn dirty_tracked_changes_refuse_pool_return_and_reset() {
        let (_keep, root) = repo_fixture();
        let t1 = task(1);
        let leased = lease_at(&root, "delta", &t1, 4);
        let slot = leased["name"].as_str().unwrap().to_string();
        let path = PathBuf::from(leased["path"].as_str().unwrap());

        // A tracked modification is real work: the return must fail.
        fs::write(path.join("seed.txt"), b"agent edits to a tracked file").unwrap();
        let err = return_slot(
            &root,
            &json!({ "name": &slot, "taskId": t1.as_id().as_str() }),
        )
        .expect_err("dirty return must be refused");
        assert!(err.to_string().contains("tracked changes"), "{err}");
        // The lease is intact and the directory is untouched.
        let git_common = git_common_dir(&root).unwrap();
        let catalog = load_catalog(&git_common).unwrap();
        let record = catalog
            .worktrees
            .iter()
            .find(|row| row.name == slot)
            .unwrap();
        assert_eq!(record.leased_by.len(), 1);
        assert!(path.join("seed.txt").exists());
    }

    #[test]
    fn untracked_leftovers_are_cleaned_but_ignored_dirs_stay_warm() {
        let (_keep, root) = repo_fixture();
        let t1 = task(1);
        let leased = lease_at(&root, "epsilon", &t1, 4);
        let slot = leased["name"].as_str().unwrap().to_string();
        let path = PathBuf::from(leased["path"].as_str().unwrap());

        fs::write(path.join("stray.log"), b"noise").unwrap();
        let target = path.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("artifact.o"), b"binary").unwrap();

        let returned = give_back(&root, &slot, &t1);
        assert_eq!(returned["state"], "parked");
        assert!(
            !path.join("stray.log").exists(),
            "untracked file is cleaned"
        );
        assert!(
            path.join("target/artifact.o").exists(),
            "ignored build dir stays warm"
        );

        // The slot rests detached.
        let head = git(&path, &["symbolic-ref", "--quiet", "--short", "HEAD"]);
        assert!(head.is_err(), "detached HEAD has no symbolic ref");
    }

    #[test]
    fn detached_parking_survives_branch_name_reuse() {
        let (_keep, root) = repo_fixture();
        // Lease/return twice with the SAME task id: the per-task branch
        // already exists on the second round, yet detached parking lets the
        // slot check it out again instead of hitting "branch is checked out
        // elsewhere" / "branch already exists".
        let t1 = task(1);
        let first = lease_at(&root, "zeta", &t1, 4);
        assert_eq!(
            first["branch"],
            format!("wt/zeta-s1/{}", task_branch_slug(&t1))
        );
        give_back(&root, "zeta-s1", &t1);
        let again = lease_at(&root, "zeta", &t1, 4);
        assert_eq!(
            again["branch"],
            format!("wt/zeta-s1/{}", task_branch_slug(&t1))
        );
        assert_eq!(again["warm"], true);
        give_back(&root, "zeta-s1", &t1);

        // Two different branches now exist, neither checked out: git's
        // one-worktree-per-branch rule never bit the pool.
        let branches = git(&root, &["branch", "--list", "wt/zeta-s1/*"]).unwrap();
        assert!(branches.contains(&format!("wt/zeta-s1/{}", task_branch_slug(&t1))));
    }

    #[test]
    fn reconcile_drops_missing_rows_and_heals_stale_leases() {
        let (_keep, root) = repo_fixture();
        let t1 = task(1);
        let leased = lease_at(&root, "eta", &t1, 4);
        let live_path = PathBuf::from(leased["path"].as_str().unwrap());
        let live_slot = leased["name"].as_str().unwrap().to_string();
        // A normal return parks the slot.
        give_back(&root, &live_slot, &t1);

        // Simulate two stale catalog shapes: a leased row with no holders
        // (crash between return and catalog write) and a row whose directory
        // is gone.
        let git_common = git_common_dir(&root).unwrap();
        {
            let mut catalog = load_catalog(&git_common).unwrap();
            let live = catalog
                .worktrees
                .iter_mut()
                .find(|row| row.name == live_slot)
                .unwrap();
            live.state = WorktreeLeaseState::Leased;
            live.leased_by = vec![];
            catalog.worktrees.push(crate::worktree::WorktreeRecord {
                name: "ghost".into(),
                path: root
                    .parent()
                    .unwrap()
                    .join("remuda-wt")
                    .join("ghost")
                    .to_string_lossy()
                    .into(),
                branch: "wt/ghost/work".into(),
                base: "origin/main".into(),
                state: WorktreeLeaseState::Leased,
                leased_by: vec![],
            });
            save_catalog(&git_common, &catalog).unwrap();
        }

        let report = reconcile(&root).expect("reconcile");
        assert_eq!(report.dropped_missing, 1);
        assert_eq!(report.healed_leased, 1, "the live slot is healed to parked");

        let catalog = load_catalog(&git_common).unwrap();
        let names: Vec<String> = catalog.worktrees.iter().map(|r| r.name.clone()).collect();
        assert_eq!(names, vec![live_slot]);
        assert_eq!(
            catalog.worktrees[0].state,
            WorktreeLeaseState::Parked,
            "leased-without-holders detached slot heals to parked"
        );
        assert!(live_path.exists());
    }

    #[test]
    fn lease_refuses_path_and_repo_overrides() {
        let (_keep, root) = repo_fixture();
        let t1 = task(1);
        for bad in [
            json!({"name": "evil", "taskId": t1.as_id().as_str(), "path": "/tmp/escape"}),
            json!({"name": "evil", "taskId": t1.as_id().as_str(), "repo": "/tmp/repo"}),
        ] {
            let err = lease(&root, &bad).expect_err("path/repo must be refused");
            assert!(err.to_string().contains("path or repo"), "{err}");
        }
    }

    #[test]
    fn slot_number_and_slot_name_helpers() {
        assert_eq!(slot_number("alpha", "alpha-s1"), Some(1));
        assert_eq!(slot_number("alpha", "alpha-s12"), Some(12));
        assert_eq!(slot_number("alpha", "alpha-s"), None);
        assert_eq!(slot_number("alpha", "alpha-sx"), None);
        assert_eq!(slot_number("alpha", "alpha-s1-s2"), None);
        assert_eq!(slot_number("alpha", "beta-s1"), None);
        // Hyphenated pool names keep working.
        assert_eq!(slot_number("c-pool", "c-pool-s2"), Some(2));
        assert!(is_slot_name("pool-s3"));
        assert!(is_slot_name("c-pool-s1"));
        // Standalone worker-shaped names are not mistaken for slots.
        assert!(!is_slot_name("agent-one"));
        assert!(!is_slot_name("c-demo"));
    }

    /// Recursive (relative path, bytes) fingerprint so an untouched tree
    /// compares byte-identical regardless of filesystem mtimes.
    fn tree_fingerprint(root: &Path) -> Vec<(String, Vec<u8>)> {
        let mut files = Vec::new();
        fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.file_name().unwrap() == ".git" {
                    continue;
                }
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                    out.push((rel, std::fs::read(&path).unwrap()));
                }
            }
        }
        walk(root, root, &mut files);
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    }

    /// E1 regression: lease/return must be registered in *both* the predicate
    /// and the dispatch match. Before t-pool an unknown worktree method fell to
    /// the catch-all and was answered `{"ok":true}` — a fake success. After
    /// registration the two methods return their real payloads, while a method
    /// nobody implements is an honest error rather than a silent no-op.
    #[tokio::test]
    async fn lease_return_dispatch_real_payloads_and_unknown_method_errors() {
        use crate::DevServerConfig;

        assert!(crate::worktree::is_worktree_method("worktree.lease"));
        assert!(crate::worktree::is_worktree_method("worktree.return"));

        // `handle_rpc` itself returns None for a method it does not serve.
        let (_keep, root) = repo_fixture();
        assert!(crate::worktree::handle_rpc(&root, "worktree.no-such", &json!({})).is_none());

        let data = tempfile::tempdir().unwrap();
        let config = DevServerConfig::loopback(0)
            .with_workspace_root(root.clone())
            .with_workspace_roots(vec![root.parent().unwrap().to_path_buf()])
            .with_workspace_registry(data.path().to_path_buf());
        let node = crate::DevNode::new(&config).unwrap();

        let task = TaskId::new();
        let leased = crate::server::dispatch_hub_rpc(
            &node,
            "worktree.lease",
            json!({ "name": "dispatch", "taskId": task.as_id().as_str() }),
        )
        .await
        .expect("lease dispatch");
        // A real pool payload, never the historical `{"ok":true}`.
        assert_eq!(leased["mode"], "pool");
        assert_eq!(leased["state"], "leased");
        assert!(leased["dirKey"].as_str().is_some());
        let slot = leased["name"].as_str().unwrap().to_string();

        let returned = crate::server::dispatch_hub_rpc(
            &node,
            "worktree.return",
            json!({ "name": slot, "taskId": task.as_id().as_str() }),
        )
        .await
        .expect("return dispatch");
        assert_eq!(returned["state"], "parked");

        // An unregistered worktree method fails honestly: no `{ok:true}`.
        let err = crate::server::dispatch_hub_rpc(
            &node,
            "worktree.shrink-pool",
            json!({ "name": "dispatch" }),
        )
        .await
        .expect_err("an unregistered method must error, not no-op");
        assert!(
            err.to_string().contains("not implemented"),
            "unexpected error: {err}"
        );
    }

    /// Root-cause regression (acceptance #2, blocker#2): the ordinary reclaim
    /// RPC `worker.remove` — the path `retire_worker` drives — must not
    /// physically delete a slot several tasks hold. After both leases are
    /// returned the same RPC reclaims as before.
    #[tokio::test]
    async fn worker_remove_does_not_delete_a_shared_slot() {
        use crate::DevServerConfig;

        let (keep, root) = repo_fixture();
        let data = tempfile::tempdir().unwrap();
        let config = DevServerConfig::loopback(0)
            .with_workspace_root(root.clone())
            .with_workspace_roots(vec![keep.path().to_path_buf()])
            .with_workspace_registry(data.path().to_path_buf());
        let node = crate::DevNode::new(&config).unwrap();

        let t1 = TaskId::new();
        let t2 = TaskId::new();
        let leased = crate::server::dispatch_hub_rpc(
            &node,
            "worktree.lease",
            json!({ "name": "share", "taskId": t1.as_id().as_str() }),
        )
        .await
        .expect("lease 1");
        let slot = leased["name"].as_str().unwrap().to_string();
        let path = leased["path"].as_str().unwrap().to_string();
        // Second task shares the exact slot (serial reuse, queued).
        crate::server::dispatch_hub_rpc(
            &node,
            "worktree.lease",
            json!({ "name": slot, "taskId": t2.as_id().as_str() }),
        )
        .await
        .expect("lease 2");
        assert!(Path::new(&path).join(".git").exists());

        // The normal instance reclaim path, sent by retire with the slot name.
        let remove =
            crate::server::dispatch_hub_rpc(&node, "worker.remove", json!({ "name": slot }))
                .await
                .expect("worker.remove answers, it does not error");
        assert_eq!(
            remove["worktreeRemoved"], false,
            "a shared slot is not force-removed: {remove}"
        );
        assert_eq!(remove["retained"], true);
        assert_eq!(remove["refcount"], 2);
        assert!(
            Path::new(&path).exists(),
            "the directory physically survived a shared reclaim"
        );

        // Both tasks return; now an ordinary reclaim removes the parked slot.
        for task in [&t2, &t1] {
            crate::server::dispatch_hub_rpc(
                &node,
                "worktree.return",
                json!({ "name": slot, "taskId": task.as_id().as_str() }),
            )
            .await
            .expect("return");
        }
        let gone = crate::server::dispatch_hub_rpc(&node, "worker.remove", json!({ "name": slot }))
            .await
            .expect("worker.remove after return");
        assert_eq!(gone["worktreeRemoved"], true);
        assert!(!Path::new(&path).exists());
    }
}
