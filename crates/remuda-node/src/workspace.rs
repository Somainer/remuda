//! Durable Node-owned workspace membership and registration policy (D-023).

use crate::{DevNode, DevServerConfig, NodeError};
use remuda_protocol::hubnode::{
    WorkspaceMutationParams, WorkspaceMutationPhase, WorkspaceResolveParams,
};
use remuda_protocol::{HostId, Workspace, WorkspaceId, path_guard};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegistryState {
    revision: u64,
    workspaces: Vec<Workspace>,
    commands: BTreeMap<String, Mutation>,
    /// Workspaces with a prepared-but-uncommitted unregister. New sessions
    /// are refused here between prepare and commit, closing the
    /// check-then-unbind race (c-dirpicker round 2 item 6). The flag is set
    /// atomically with the occupancy check under the registry write lock and
    /// disappears with the membership row at commit.
    #[serde(default)]
    unbinding: std::collections::HashSet<WorkspaceId>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Mutation {
    method: String,
    path: String,
    canonical: PathBuf,
    workspace_id: WorkspaceId,
    #[serde(default)]
    was_registered: bool,
    settled: bool,
}

/// Test-only async barrier between create resolution and occupancy
/// reservation (c-dirpicker round 4 item 2). Defined here so both the
/// runtime field and the workspace mutation tests share the type.
#[cfg(test)]
pub(crate) type CreateBarrierFn = std::sync::Arc<
    dyn Fn(&WorkspaceId) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

pub(crate) struct WorkspaceRegistry {
    state: RegistryState,
    file: Option<PathBuf>,
    roots: Vec<crate::dir_browser::AllowedRoot>,
    host_id: HostId,
    /// In-memory (never persisted) occupancy reservations held by instance
    /// creates from admission until their row is durably inserted (or the
    /// create fails and the guard drops). Unregister prepare counts them
    /// together with live instances, so a create that passed admission but
    /// has not inserted yet cannot be overtaken by an unbind (round 3 item 6).
    reservations: Arc<std::sync::Mutex<std::collections::HashMap<WorkspaceId, u32>>>,
}

impl WorkspaceRegistry {
    pub(crate) fn open(config: &DevServerConfig, host_id: HostId) -> Result<Self, NodeError> {
        let configured_roots = config.workspace_roots.clone().unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .into_iter()
                .collect()
        });
        if configured_roots.is_empty() {
            return Err(NodeError::InvalidConfig("workspace_roots is empty and HOME is unavailable; configure an absolute allowed directory".into()));
        }
        // Canonicalize once at policy load and pin each root's identity; a
        // later symlinked/replaced ancestor is refused by the directory
        // browser (c-dirpicker round 3). Identity is mandatory on unix
        // (round 4 item 1): an un-stat-able root fails policy load instead of
        // silently browsing unpinned.
        let roots = configured_roots
            .iter()
            .map(|root| canonical_directory(root).and_then(crate::dir_browser::AllowedRoot::new))
            .collect::<Result<Vec<_>, _>>()?;
        let file = config
            .workspace_registry
            .as_ref()
            .map(|dir| dir.join("workspaces.json"));
        let state = match file.as_ref().map(fs::read) {
            Some(Ok(bytes)) => serde_json::from_slice(&bytes)?,
            Some(Err(error)) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error.into());
            }
            _ => RegistryState::default(),
        };
        let mut registry = Self {
            state,
            file,
            roots,
            host_id,
            reservations: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        };
        // Persisted entries may be deleted, unmounted, or disallowed by a tightened
        // policy. Keep them listable/removable; session and worktree admission
        // revalidate current existence, canonical identity, and allowlist.
        // Explicit startup roots must still pass the current registration policy.
        for path in std::iter::once(&config.workspace_root).chain(config.workspaces.iter()) {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                std::env::current_dir()?.join(path)
            };
            let canonical = registry.validate(&absolute)?;
            registry.insert(&canonical)?;
        }
        registry.persist(&registry.state)?;
        Ok(registry)
    }

    pub(crate) fn workspaces(&self) -> Vec<Workspace> {
        self.state.workspaces.clone()
    }

    /// Canonical allowlist roots (with pinned identities) workspace
    /// registration and the directory browser (c-dirpicker) are confined to.
    pub(crate) fn allowed_roots(&self) -> &[crate::dir_browser::AllowedRoot] {
        &self.roots
    }

    pub(crate) fn snapshot(&self) -> Value {
        json!({"workspaceRevision": self.state.revision, "workspaces": self.state.workspaces.iter().map(|workspace| {
            json!({"workspaceId": workspace.meta.id, "hostId": workspace.host_id, "root": workspace.root_path})
        }).collect::<Vec<_>>()})
    }

    /// Reserve one occupancy slot. The caller MUST hold the registry's write
    /// guard (see [`DevNode::reserve_workspace`]); this never takes the
    /// registry RwLock itself, so it is safe to call from inside [`mutate`]'s
    /// closure too. The counter has its own mutex because the returned guard
    /// releases the slot after the registry guard was dropped (the create
    /// holds it across an await).
    ///
    /// Round 4 item 2: this also re-validates *membership* under the same
    /// write lock. A create resolves its workspace earlier (a slow
    /// canonicalize/worktree probe) while only holding a read lock; an
    /// unregister can commit and remove the membership in that window.
    /// Checking both membership and unbinding here, atomically with the
    /// reservation, closes the gap.
    pub(crate) fn reserve_locked(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<WorkspaceReservation, NodeError> {
        let root_path = self
            .state
            .workspaces
            .iter()
            .find(|workspace| &workspace.meta.id == workspace_id)
            .ok_or_else(|| {
                NodeError::Conflict(format!(
                    "workspace {} is no longer registered on this Node",
                    workspace_id.as_id()
                ))
            })?
            .root_path
            .clone();
        if self.state.unbinding.contains(workspace_id) {
            return Err(NodeError::Conflict(format!(
                "workspace {} is being unregistered; wait for it to settle before starting a session",
                root_path
            )));
        }
        *self
            .reservations
            .lock()
            .map_err(|_| NodeError::StorePoisoned)?
            .entry(workspace_id.clone())
            .or_insert(0) += 1;
        Ok(WorkspaceReservation {
            workspace_id: workspace_id.clone(),
            table: self.reservations.clone(),
            released: false,
        })
    }

    /// Current in-flight create reservations for a workspace. Caller holds
    /// the registry write lock; the reservation table is locked only for the
    /// read.
    fn reservation_count_locked(&self, workspace_id: &WorkspaceId) -> u32 {
        self.reservations
            .lock()
            .map(|map| map.get(workspace_id).copied().unwrap_or(0))
            .unwrap_or(0)
    }

    /// Resolve an unregister candidate with the ONLY identity semantics an
    /// unregister accepts:
    /// * an absolute path;
    /// * the exact stored canonical root, matched byte-for-byte (a real
    ///   trailing space is a different name; nothing is trimmed);
    /// * anything else goes through [`canonical_directory`] — a real `realpath`
    ///   with the access probe, never a lexical `..` collapse
    ///   (`/allowed/link/../p` resolves where the symlink actually points);
    /// * the result must match a currently registered workspace.
    ///
    /// Returns `(workspaceId, canonicalRoot)`. This is the single resolution
    /// function shared by unregister prepare AND the read-only
    /// `workspace.resolve` RPC (c-dirpicker round 6 item 1): the two can never
    /// disagree, so do not duplicate it.
    pub(crate) fn resolve_unregister(
        &self,
        candidate: &str,
    ) -> Result<(WorkspaceId, PathBuf), NodeError> {
        let path = Path::new(candidate);
        if !path.is_absolute() {
            return Err(NodeError::InvalidRequest(
                "workspace path must be absolute".into(),
            ));
        }
        // A deleted project remains removable using its stored canonical
        // absolute path; for that exact string the root is trusted verbatim.
        let canonical = if self
            .state
            .workspaces
            .iter()
            .any(|workspace| Path::new(&workspace.root_path) == path)
        {
            path.to_path_buf()
        } else {
            canonical_directory(path)?
        };
        let workspace = self
            .state
            .workspaces
            .iter()
            .find(|workspace| Path::new(&workspace.root_path) == canonical)
            .ok_or_else(|| {
                NodeError::InvalidRequest(format!("workspace {} is not registered", path.display()))
            })?;
        Ok((workspace.meta.id.clone(), canonical))
    }

    fn validate(&self, path: &Path) -> Result<PathBuf, NodeError> {
        let canonical = canonical_directory(path)?;
        if !self
            .roots
            .iter()
            .any(|root| canonical.starts_with(&root.path))
        {
            return Err(NodeError::InvalidRequest(format!(
                "workspace {} is outside allowed workspace_roots: {}",
                canonical.display(),
                display_roots(self.roots.iter().map(|root| root.path.as_path()))
            )));
        }
        for workspace in &self.state.workspaces {
            let root = Path::new(&workspace.root_path);
            if canonical == root {
                continue;
            }
            if let Some(worktrees) = worktree_boundary(root)?
                && canonical.starts_with(&worktrees)
            {
                return Err(NodeError::InvalidRequest(format!(
                    "workspace {} is inside the worktree directory of registered workspace {}",
                    canonical.display(),
                    root.display()
                )));
            }
            // Preserve the invariant when a repository is registered after its sibling worktree.
            if let Some(worktrees) = worktree_boundary(&canonical)?
                && root.starts_with(&worktrees)
            {
                return Err(NodeError::InvalidRequest(format!(
                    "registered workspace {} is inside this directory's worktree directory",
                    root.display()
                )));
            }
        }
        Ok(canonical)
    }

    fn validate_existing(&self, workspace: &Workspace) -> Result<(), NodeError> {
        let root = Path::new(&workspace.root_path);
        let canonical = self.validate(root)?;
        if canonical != root {
            return Err(NodeError::InvalidRequest(format!(
                "registered workspace {} changed its canonical location; unregister and register it again",
                root.display()
            )));
        }
        if self.state.unbinding.contains(&workspace.meta.id) {
            return Err(NodeError::Conflict(format!(
                "workspace {} is being unregistered; wait for it to settle before starting a session",
                root.display()
            )));
        }
        Ok(())
    }

    fn insert(&mut self, path: &Path) -> Result<(), NodeError> {
        if !self
            .state
            .workspaces
            .iter()
            .any(|workspace| Path::new(&workspace.root_path) == path)
        {
            self.state
                .workspaces
                .push(crate::runtime::fixture_workspace(
                    WorkspaceId::new(),
                    self.host_id.clone(),
                    path,
                )?);
            self.state.revision += 1;
        }
        Ok(())
    }

    fn persist(&self, state: &RegistryState) -> Result<(), NodeError> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        let parent = file
            .parent()
            .ok_or_else(|| NodeError::InvalidConfig("workspace registry has no parent".into()))?;
        fs::create_dir_all(parent)?;
        let temporary = file.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(&temporary)?;
        let result = (|| -> Result<(), NodeError> {
            output.write_all(&serde_json::to_vec_pretty(state)?)?;
            output.sync_all()?;
            drop(output);
            fs::rename(&temporary, file)?;
            fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    /// Apply one prepared/committed mutation. `live_sessions` reports the
    /// current live-session count for a workspace and is invoked for a fresh
    /// unregister prepare while the write lock is held, so the occupancy
    /// check and the unbinding mark are one atomic step.
    fn mutate(
        &mut self,
        method: &str,
        params: WorkspaceMutationParams,
        mut live_sessions: impl FnMut(&WorkspaceId) -> Result<usize, NodeError>,
    ) -> Result<Value, NodeError> {
        if params.command_id.trim().is_empty() || params.command_id.len() > 256 {
            return Err(NodeError::InvalidRequest(
                "workspace mutation requires a bounded commandId".into(),
            ));
        }
        let mut next = self.state.clone();
        let command = next.commands.get(&params.command_id).cloned();
        if let Some(command) = &command
            && (command.method != method || command.path != params.path)
        {
            return Err(NodeError::Conflict(
                "workspace commandId was reused with different input".into(),
            ));
        }
        match params.phase {
            WorkspaceMutationPhase::Prepare => {
                if command.is_none() {
                    let (canonical, workspace_id, was_registered) = if method
                        == "workspace.register"
                    {
                        let canonical = self.validate(Path::new(&params.path))?;
                        let existing = self
                            .state
                            .workspaces
                            .iter()
                            .find(|workspace| Path::new(&workspace.root_path) == canonical)
                            .map(|workspace| workspace.meta.id.clone());
                        let was_registered = existing.is_some();
                        (canonical, existing.unwrap_or_default(), was_registered)
                    } else {
                        // Same function the read-only workspace.resolve RPC
                        // runs — never a second, drift-able resolution path
                        // (round 6 item 1).
                        let (workspace_id, canonical) = self.resolve_unregister(&params.path)?;
                        // When the Hub prepared from a resolve result, it
                        // must send the exact stored root bytes in `path`
                        // plus the resolved id. Verify both before anything
                        // is marked: an alias that realpaths to the root is
                        // rejected here, and an id/root mismatch means the
                        // identity moved.
                        if let Some(expected) = params.workspace_id.as_deref()
                            && (expected != workspace_id.as_id().as_str()
                                || Path::new(&params.path) != canonical)
                        {
                            return Err(NodeError::Conflict(format!(
                                "workspace unregister identity does not match the resolved \
                                 workspace (expected {}@{}, got {})",
                                workspace_id.as_id(),
                                canonical.display(),
                                params.path
                            )));
                        }
                        (canonical, workspace_id, true)
                    };
                    // Round 3 item 7: an unregister already prepared against
                    // this canonical directory cannot be overtaken by a
                    // register; the caller must let the unregister settle.
                    if method == "workspace.register"
                        && self.state.unbinding.iter().any(|id| {
                            self.state.workspaces.iter().any(|workspace| {
                                &workspace.meta.id == id
                                    && Path::new(&workspace.root_path) == canonical
                            })
                        })
                    {
                        return Err(NodeError::Conflict(
                            "an unregister for this workspace is already prepared; \
                             let it settle before registering again"
                                .into(),
                        ));
                    }
                    if method == "workspace.unregister" {
                        // Atomic with the write lock: a session entering in
                        // another thread must take this same lock to reserve
                        // occupancy, so it cannot slip in between the check
                        // and the unbinding mark. Both live instances and
                        // in-flight create reservations count.
                        let live = live_sessions(&workspace_id)?;
                        let reservations = self.reservation_count_locked(&workspace_id) as usize;
                        let occupancy = live + reservations;
                        if occupancy > 0 {
                            return Err(NodeError::Conflict(format!(
                                "workspace {} is still used by {occupancy} live session(s); \
                                 end them before removing the directory (session history is kept)",
                                params.path
                            )));
                        }
                        next.unbinding.insert(workspace_id.clone());
                    }
                    next.commands.insert(
                        params.command_id.clone(),
                        Mutation {
                            method: method.into(),
                            path: params.path.clone(),
                            canonical,
                            workspace_id,
                            was_registered,
                            settled: false,
                        },
                    );
                }
            }
            WorkspaceMutationPhase::Commit => {
                let Some(mut command) = command else {
                    return Err(NodeError::InvalidRequest(
                        "workspace mutation must be prepared before commit".into(),
                    ));
                };
                if !command.settled {
                    if method == "workspace.register" {
                        let canonical = self.validate(Path::new(&params.path))?;
                        if canonical != command.canonical {
                            return Err(NodeError::Conflict(
                                "workspace path changed after prepare".into(),
                            ));
                        }
                        if let Some(existing) = next
                            .workspaces
                            .iter()
                            .find(|workspace| Path::new(&workspace.root_path) == canonical)
                        {
                            command.workspace_id = existing.meta.id.clone();
                            // Round 3 item 7: never clear another command's
                            // unbinding mark. If a different unregister is
                            // prepared against this identity, this register
                            // cannot settle — the unregister must commit (or
                            // fail) first, otherwise its later commit would
                            // re-add/remove against a mark the register hid.
                            if next.unbinding.contains(&command.workspace_id) {
                                return Err(NodeError::Conflict(
                                    "an unregister for this workspace is already prepared; \
                                     let it settle before registering again"
                                        .into(),
                                ));
                            }
                        } else {
                            // A prepared idempotent register must not resurrect a removed
                            // membership identity that an older unregister may still target.
                            if command.was_registered {
                                command.workspace_id = WorkspaceId::new();
                            }
                            next.workspaces.push(crate::runtime::fixture_workspace(
                                command.workspace_id.clone(),
                                self.host_id.clone(),
                                &canonical,
                            )?);
                            next.revision += 1;
                        }
                    } else {
                        // Round 6 item 1: the Hub re-sends the exact stored
                        // root and the resolved id at commit; verify both
                        // before removing. Anything that changed between the
                        // phases refuses rather than unbinding.
                        if let Some(expected) = params.workspace_id.as_deref()
                            && (expected != command.workspace_id.as_id().as_str()
                                || Path::new(&params.path) != command.canonical)
                        {
                            return Err(NodeError::Conflict(
                                "workspace unregister identity changed between prepare and commit"
                                    .into(),
                            ));
                        }
                        if next.workspaces.iter().any(|workspace| {
                            Path::new(&workspace.root_path) == command.canonical
                                && workspace.meta.id != command.workspace_id
                        }) {
                            return Err(NodeError::Conflict(
                                "workspace was replaced after unregister prepare".into(),
                            ));
                        }
                        // Re-count at commit (defence in depth): prepare
                        // blocked any create with a reservation and the
                        // unbinding mark refused new ones, so this should be
                        // zero; refuse the unbind rather than remove a
                        // membership that gained occupancy since prepare.
                        let live_at_commit = live_sessions(&command.workspace_id)?;
                        let reserved_at_commit =
                            self.reservation_count_locked(&command.workspace_id) as usize;
                        if live_at_commit + reserved_at_commit > 0 {
                            return Err(NodeError::Conflict(format!(
                                "workspace {} gained occupancy after unregister prepare; \
                                 the removal did not settle",
                                command.canonical.display()
                            )));
                        }
                        let previous_len = next.workspaces.len();
                        next.workspaces
                            .retain(|workspace| workspace.meta.id != command.workspace_id);
                        if previous_len != next.workspaces.len() {
                            next.revision += 1;
                        }
                        // The identity is gone with the membership; drop its
                        // unbinding mark too.
                        next.unbinding.remove(&command.workspace_id);
                    }
                    command.settled = true;
                    next.commands.insert(params.command_id.clone(), command);
                }
            }
        }
        self.persist(&next)?;
        self.state = next;
        let mut result = self.snapshot();
        result["workspaceId"] = json!(
            self.state
                .commands
                .get(&params.command_id)
                .map(|command| &command.workspace_id)
        );
        result["commandId"] = json!(params.command_id);
        result["phase"] = json!(match params.phase {
            WorkspaceMutationPhase::Prepare => "prepared",
            WorkspaceMutationPhase::Commit => "settled",
        });
        Ok(result)
    }
}

/// One held occupancy reservation for an in-flight instance create
/// (c-dirpicker round 3 item 6). Releases the slot on drop, so every failed
/// create path frees its reservation; a successful create drops it after the
/// instance row is durable (the live row then counts in its place).
pub(crate) struct WorkspaceReservation {
    workspace_id: WorkspaceId,
    table: Arc<std::sync::Mutex<std::collections::HashMap<WorkspaceId, u32>>>,
    released: bool,
}

impl WorkspaceReservation {
    /// Release the slot explicitly after a durable insert; idempotent with
    /// Drop.
    fn release(&mut self) {
        if self.released {
            return;
        }
        if let Ok(mut table) = self.table.lock()
            && let Some(count) = table.get_mut(&self.workspace_id)
        {
            if *count > 0 {
                *count -= 1;
            }
            if *count == 0 {
                table.remove(&self.workspace_id);
            }
        }
        self.released = true;
    }
}

impl Drop for WorkspaceReservation {
    fn drop(&mut self) {
        self.release();
    }
}

fn canonical_directory(path: &Path) -> Result<PathBuf, NodeError> {
    canonical_directory_with_probe(path, crate::workspace_access_check)
}

fn canonical_directory_with_probe(
    path: &Path,
    probe: impl FnOnce(&Path) -> Result<(), NodeError>,
) -> Result<PathBuf, NodeError> {
    if !path.is_absolute() {
        return Err(NodeError::InvalidRequest(
            "workspace path must be absolute".into(),
        ));
    }
    probe(path)?;
    let canonical = fs::canonicalize(path).map_err(|error| {
        NodeError::InvalidRequest(format!(
            "workspace {} cannot be resolved: {error}",
            path.display()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(NodeError::InvalidRequest(format!(
            "workspace {} is not a directory",
            path.display()
        )));
    }
    Ok(canonical)
}

/// Resolve a stored canonical root's sibling boundary without accessing that root.
/// Even a stale or protected boundary is inspected only by the killable probe.
fn worktree_boundary(root: &Path) -> Result<Option<PathBuf>, NodeError> {
    let Some(parent) = root.parent() else {
        return Ok(None);
    };
    let boundary = parent.join(path_guard::WORKTREE_DIR);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args([
                "-c",
                r#"probe_error=$(/bin/ls -ld "$1" 2>&1 >/dev/null)
if [ $? -ne 0 ]; then
    case "$probe_error" in *": No such file or directory") exit 44 ;; esac
    printf '%s\n' "$probe_error" >&2; exit 1
fi
CDPATH=; cd -P "$1" || exit; pwd -P"#,
                "remuda-worktree-boundary",
            ])
            .arg(&boundary);
        let output = crate::workspace_access::bounded_workspace_command(
            &mut command,
            &boundary,
            std::time::Duration::from_secs(3),
        )?;
        if output.status.code() == Some(44) {
            return Ok(Some(boundary));
        }
        crate::workspace_access::check_workspace_output(&boundary, &output)?;
        if !output.status.success() {
            return Err(NodeError::InvalidRequest(format!(
                "worktree boundary {} is inaccessible: {}",
                boundary.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let mut path = output.stdout;
        if path.last() == Some(&b'\n') {
            path.pop();
        }
        let canonical = PathBuf::from(std::ffi::OsString::from_vec(path));
        if !canonical.is_absolute() {
            return Err(NodeError::InvalidRequest(
                "worktree boundary probe returned a non-absolute path".into(),
            ));
        }
        Ok(Some(canonical))
    }
    #[cfg(not(unix))]
    {
        path_guard::real_path(&boundary)
            .map(Some)
            .map_err(|error| NodeError::InvalidRequest(error.to_string()))
    }
}

fn display_roots<'a>(roots: impl Iterator<Item = &'a Path>) -> String {
    let roots = roots
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>();
    if roots.is_empty() {
        "(none)".into()
    } else {
        roots.join(", ")
    }
}

pub(crate) fn is_workspace_method(method: &str) -> bool {
    matches!(
        method,
        "workspace.list" | "workspace.resolve" | "workspace.register" | "workspace.unregister"
    )
}

impl DevNode {
    /// Registered workspace entities, including persistent identities.
    pub fn workspaces(&self) -> Result<Vec<Workspace>, NodeError> {
        Ok(self
            .inner
            .workspace_registry
            .read()
            .map_err(|_| NodeError::StorePoisoned)?
            .workspaces())
    }

    /// Authoritative workspace membership and revision for Hub inventory.
    pub fn workspace_snapshot(&self) -> Result<Value, NodeError> {
        Ok(self
            .inner
            .workspace_registry
            .read()
            .map_err(|_| NodeError::StorePoisoned)?
            .snapshot())
    }

    pub(crate) fn workspace_rpc(&self, method: &str, params: Value) -> Result<Value, NodeError> {
        if method == "workspace.list" {
            return self.workspace_snapshot();
        }
        // c-dirpicker round 6 item 1: read-only, Node-authoritative identity
        // for a pending unregister. It runs the SAME resolve_unregister
        // function prepare uses, takes only the read lock, and mutates
        // nothing; the Hub calls it before taking its occupancy guard.
        if method == "workspace.resolve" {
            let request: WorkspaceResolveParams = serde_json::from_value(params)?;
            let registry = self
                .inner
                .workspace_registry
                .read()
                .map_err(|_| NodeError::StorePoisoned)?;
            let (workspace_id, canonical_root) = registry.resolve_unregister(&request.path)?;
            return Ok(json!({
                "workspaceId": workspace_id,
                "canonicalRoot": canonical_root.display().to_string(),
            }));
        }
        // The unregister occupancy check runs inside the registry's mutate
        // under its write lock (the callback below), so it and the unbinding
        // mark that blocks new creates are one atomic step. The closure only
        // takes a shared reborrow of self for the instance store, a different
        // lock than the registry write guard being held.
        self.inner
            .workspace_registry
            .write()
            .map_err(|_| NodeError::StorePoisoned)?
            .mutate(method, serde_json::from_value(params)?, |workspace_id| {
                Ok(self
                    .list_instances()?
                    .items
                    .iter()
                    // Process-end evidence (`exited` = clean/close,
                    // `failed` = ended with an error; D-057 OA6: failed is
                    // terminal process state, not a turn-level error) frees
                    // the directory. The Hub has the same ended definition.
                    // A live process keeps blocking regardless of any stale
                    // lifecycle recorded before the process actually ended.
                    .filter(|instance| {
                        instance.workspace_id == *workspace_id
                            && !matches!(
                                instance.lifecycle,
                                remuda_protocol::InstanceLifecycle::Exited
                                    | remuda_protocol::InstanceLifecycle::Failed
                            )
                    })
                    .count())
            })
    }

    /// Reserve occupancy for an instance create on a workspace, atomically
    /// with the unbinding check (both take the registry write lock). The
    /// guard must be held until the instance row is durable; it releases on
    /// any failure. See [`WorkspaceRegistry::reserve_locked`].
    pub(crate) fn reserve_workspace(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<WorkspaceReservation, NodeError> {
        self.inner
            .workspace_registry
            .write()
            .map_err(|_| NodeError::StorePoisoned)?
            .reserve_locked(workspace_id)
    }

    pub(crate) fn resolve_workspace_cwd(
        &self,
        workspace_id: Option<&WorkspaceId>,
        cwd: Option<&str>,
    ) -> Result<(Workspace, PathBuf), NodeError> {
        let workspaces = self.workspaces()?;
        let selected = match workspace_id {
            Some(id) => workspaces
                .iter()
                .find(|workspace| &workspace.meta.id == id)
                .ok_or_else(|| {
                    NodeError::InvalidRequest(format!(
                        "workspace {} is not registered on this Node",
                        id.as_id()
                    ))
                })?,
            None => workspaces
                .first()
                .ok_or_else(|| self.unregistered_cwd(cwd.unwrap_or(""), &workspaces))?,
        };
        self.inner
            .workspace_registry
            .read()
            .map_err(|_| NodeError::StorePoisoned)?
            .validate_existing(selected)?;
        let expanded = cwd
            .map(|raw| {
                crate::worktree::expand_home(
                    raw.trim(),
                    std::env::var_os("HOME").as_deref().map(Path::new),
                )
            })
            .transpose()?;
        let candidate = expanded
            .as_ref()
            .map(|path| path_guard::absolutize(Path::new(&selected.root_path), path));
        if cwd.is_none_or(|raw| raw.trim().is_empty()) {
            return Ok((
                selected.clone(),
                crate::worktree::resolve_instance_cwd(Path::new(&selected.root_path), None)?,
            ));
        }
        let candidate = candidate.ok_or_else(|| self.unregistered_cwd("", &workspaces))?;
        // Preserve the bounded probe's access/FDA error before any containment
        // canonicalization, and never retry filesystem resolution after a timeout.
        let resolved = canonical_directory(&candidate).map_err(|error| {
            NodeError::InvalidRequest(format!(
                "cwd is not a directory or is inaccessible: {error}"
            ))
        })?;
        for workspace in std::iter::once(selected).chain(
            workspaces
                .iter()
                .filter(|workspace| workspace.meta.id != selected.meta.id),
        ) {
            if workspace.meta.id != selected.meta.id
                && self
                    .inner
                    .workspace_registry
                    .read()
                    .map_err(|_| NodeError::StorePoisoned)?
                    .validate_existing(workspace)
                    .is_err()
            {
                continue;
            }
            let root = Path::new(&workspace.root_path);
            if resolved.starts_with(root)
                || worktree_boundary(root)?.is_some_and(|boundary| resolved.starts_with(boundary))
            {
                return Ok((workspace.clone(), resolved));
            }
        }
        Err(self.unregistered_cwd(&candidate.display().to_string(), &workspaces))
    }

    fn unregistered_cwd(&self, cwd: &str, workspaces: &[Workspace]) -> NodeError {
        NodeError::InvalidRequest(format!(
            "cwd {cwd} is outside registered workspaces: {}. Register an absolute project path in Hosts > Add directory or POST /v1/hosts/{}/workspaces.",
            display_roots(
                workspaces
                    .iter()
                    .map(|workspace| Path::new(&workspace.root_path))
            ),
            self.host().meta.id.as_id()
        ))
    }

    pub(crate) fn advertise_workspaces(&self, host: &mut Value) -> Result<(), NodeError> {
        let snapshot = self.workspace_snapshot()?;
        host["workspaces"] = snapshot["workspaces"].clone();
        host["workspaceRevision"] = snapshot["workspaceRevision"].clone();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(root: &Path, data: &Path) -> DevServerConfig {
        fs::create_dir_all(root.join("project")).unwrap();
        DevServerConfig::loopback(0)
            .with_workspace_root(root.join("project"))
            .with_workspace_roots(vec![root.to_path_buf()])
            .with_workspace_registry(data.to_path_buf())
    }

    fn mutation(
        registry: &mut WorkspaceRegistry,
        method: &str,
        command: &str,
        path: &Path,
        phase: WorkspaceMutationPhase,
    ) -> Result<Value, NodeError> {
        registry.mutate(
            method,
            WorkspaceMutationParams {
                command_id: command.into(),
                path: path.display().to_string(),
                phase,
                workspace_id: None,
            },
            |_workspace_id| Ok(0),
        )
    }

    #[test]
    fn a_held_create_reservation_blocks_unregister_prepare_then_frees_it() {
        // Round 3 item 6: a create that reserved occupancy (admission passed,
        // durable insert not finished yet) must make unregister prepare
        // refuse; releasing the reservation unblocks it.
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let config = crate::DevServerConfig::loopback(0)
            .with_workspace_root(root.path().to_path_buf())
            .with_workspace_roots(vec![root.path().to_path_buf()])
            .with_workspace_registry(data.path().to_path_buf());
        let mut registry = WorkspaceRegistry::open(&config, HostId::new()).unwrap();
        let workspace_id = registry.state.workspaces[0].meta.id.clone();

        let reservation = registry.reserve_locked(&workspace_id).unwrap();
        // Held reservation counts as occupancy even with zero live instances.
        let error = registry
            .mutate(
                "workspace.unregister",
                WorkspaceMutationParams {
                    command_id: "blocked-unregister".into(),
                    path: registry.state.workspaces[0].root_path.clone(),
                    phase: WorkspaceMutationPhase::Prepare,
                    workspace_id: None,
                },
                |_id| Ok(0),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("1 live session(s)"), "{error}");

        // Create failed/finished: the guard drops and the unregister prepares.
        drop(reservation);
        registry
            .mutate(
                "workspace.unregister",
                WorkspaceMutationParams {
                    command_id: "allowed-unregister".into(),
                    path: registry.state.workspaces[0].root_path.clone(),
                    phase: WorkspaceMutationPhase::Prepare,
                    workspace_id: None,
                },
                |_id| Ok(0),
            )
            .unwrap();
        // The unbinding mark is now set: a fresh create reservation is
        // refused too.
        assert!(registry.reserve_locked(&workspace_id).is_err());
    }

    #[test]
    fn register_cannot_clear_another_commands_unbinding_mark() {
        // Round 3 item 7: once an unregister is prepared against a workspace,
        // neither a register prepare nor (if one existed) its commit can
        // settle/clear the mark — the unregister must finish first.
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let config = crate::DevServerConfig::loopback(0)
            .with_workspace_root(root.path().to_path_buf())
            .with_workspace_roots(vec![root.path().to_path_buf()])
            .with_workspace_registry(data.path().to_path_buf());
        let mut registry = WorkspaceRegistry::open(&config, HostId::new()).unwrap();
        let root_path = registry.state.workspaces[0].root_path.clone();

        // Prepare the unregister with no live sessions.
        registry
            .mutate(
                "workspace.unregister",
                WorkspaceMutationParams {
                    command_id: "u1".into(),
                    path: root_path.clone(),
                    phase: WorkspaceMutationPhase::Prepare,
                    workspace_id: None,
                },
                |_id| Ok(0),
            )
            .unwrap();
        assert!(
            registry
                .state
                .unbinding
                .contains(&registry.state.workspaces[0].meta.id)
        );

        // A register for the same directory is refused at prepare.
        let register_error = registry
            .mutate(
                "workspace.register",
                WorkspaceMutationParams {
                    command_id: "r1".into(),
                    path: root_path.clone(),
                    phase: WorkspaceMutationPhase::Prepare,
                    workspace_id: None,
                },
                |_id| Ok(0),
            )
            .unwrap_err()
            .to_string();
        assert!(
            register_error.contains("already prepared"),
            "{register_error}"
        );
        // The mark survived the refused register.
        assert!(
            registry
                .state
                .unbinding
                .contains(&registry.state.workspaces[0].meta.id)
        );

        // The unregister commits and clears its own mark.
        registry
            .mutate(
                "workspace.unregister",
                WorkspaceMutationParams {
                    command_id: "u1".into(),
                    path: root_path.clone(),
                    phase: WorkspaceMutationPhase::Commit,
                    workspace_id: None,
                },
                |_id| Ok(0),
            )
            .unwrap();
        assert!(registry.state.unbinding.is_empty());
    }

    #[test]
    fn registry_persists_membership_identity_and_two_phase_receipts() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let config = config(root.path(), data.path());
        let host = HostId::new();
        let mut registry = WorkspaceRegistry::open(&config, host.clone()).unwrap();
        let extra = root.path().join("extra");
        fs::create_dir(&extra).unwrap();
        let prepared = mutation(
            &mut registry,
            "workspace.register",
            "cmd-register",
            &extra,
            WorkspaceMutationPhase::Prepare,
        )
        .unwrap();
        assert_eq!(prepared["phase"], "prepared");
        assert_eq!(
            registry.workspaces().len(),
            1,
            "prepare must not change membership"
        );
        drop(registry);
        let mut registry = WorkspaceRegistry::open(&config, host.clone()).unwrap();
        let settled = mutation(
            &mut registry,
            "workspace.register",
            "cmd-register",
            &extra,
            WorkspaceMutationPhase::Commit,
        )
        .unwrap();
        assert_eq!(settled["phase"], "settled");
        assert_eq!(prepared["workspaceId"], settled["workspaceId"]);
        assert_eq!(settled["workspaceRevision"], 2);
        let replay = mutation(
            &mut registry,
            "workspace.register",
            "cmd-register",
            &extra,
            WorkspaceMutationPhase::Commit,
        )
        .unwrap();
        assert_eq!(settled, replay);
        drop(registry);
        let mut registry = WorkspaceRegistry::open(&config, host.clone()).unwrap();
        assert_eq!(
            registry.workspaces()[1].meta.id.as_id().to_string(),
            settled["workspaceId"].as_str().unwrap()
        );
        mutation(
            &mut registry,
            "workspace.unregister",
            "cmd-remove",
            &extra,
            WorkspaceMutationPhase::Prepare,
        )
        .unwrap();
        mutation(
            &mut registry,
            "workspace.unregister",
            "cmd-remove",
            &extra,
            WorkspaceMutationPhase::Commit,
        )
        .unwrap();
        let merged = config.with_workspaces(vec![root.path().join("project")]);
        let registry = WorkspaceRegistry::open(&merged, host).unwrap();
        assert_eq!(registry.workspaces().len(), 1);
        assert_eq!(registry.snapshot()["workspaceRevision"], 3);
    }

    #[test]
    fn registry_rejects_invalid_paths_and_allows_canonical_aliases() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let registry =
            WorkspaceRegistry::open(&config(root.path(), data.path()), HostId::new()).unwrap();
        assert!(
            registry
                .validate(Path::new("relative"))
                .unwrap_err()
                .to_string()
                .contains("absolute")
        );
        assert!(
            registry
                .validate(&root.path().join("missing"))
                .unwrap_err()
                .to_string()
                .contains("inaccessible")
        );
        fs::write(root.path().join("file"), b"file").unwrap();
        assert!(
            registry
                .validate(&root.path().join("file"))
                .unwrap_err()
                .to_string()
                .contains("inaccessible")
        );
        assert!(
            registry
                .validate(outside.path())
                .unwrap_err()
                .to_string()
                .contains("outside allowed workspace_roots")
        );
        let worktree = root.path().join("remuda-wt/agent");
        fs::create_dir_all(&worktree).unwrap();
        assert!(
            registry
                .validate(&worktree)
                .unwrap_err()
                .to_string()
                .contains("worktree directory")
        );
        assert_eq!(
            registry
                .validate(&root.path().join("project/../project"))
                .unwrap(),
            fs::canonicalize(root.path().join("project")).unwrap()
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
            assert!(
                registry
                    .validate(&root.path().join("escape"))
                    .unwrap_err()
                    .to_string()
                    .contains("outside allowed workspace_roots")
            );
        }
    }

    #[test]
    fn registry_rejects_unprepared_or_changed_mutations() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut registry =
            WorkspaceRegistry::open(&config(root.path(), data.path()), HostId::new()).unwrap();
        let extra = root.path().join("extra");
        fs::create_dir(&extra).unwrap();
        assert!(
            mutation(
                &mut registry,
                "workspace.register",
                "cmd",
                &extra,
                WorkspaceMutationPhase::Commit
            )
            .is_err()
        );
        mutation(
            &mut registry,
            "workspace.register",
            "cmd",
            &extra,
            WorkspaceMutationPhase::Prepare,
        )
        .unwrap();
        assert!(
            mutation(
                &mut registry,
                "workspace.unregister",
                "cmd",
                &extra,
                WorkspaceMutationPhase::Commit
            )
            .is_err()
        );
        fs::remove_dir(&extra).unwrap();
        assert!(
            mutation(
                &mut registry,
                "workspace.register",
                "cmd",
                &extra,
                WorkspaceMutationPhase::Commit
            )
            .is_err()
        );
        assert_eq!(registry.workspaces().len(), 1);
    }

    #[test]
    fn startup_flags_obey_allowlist_and_merge_without_duplicate_ids() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let base = config(root.path(), data.path());
        let mut registry = WorkspaceRegistry::open(&base, HostId::new()).unwrap();
        let initial = registry.workspaces()[0].meta.id.clone();
        let canonical = registry.validate(&root.path().join("project")).unwrap();
        registry.insert(&canonical).unwrap();
        assert_eq!(registry.workspaces().len(), 1);
        assert_eq!(registry.workspaces()[0].meta.id, initial);
        assert!(
            WorkspaceRegistry::open(
                &base.with_workspaces(vec![outside.path().to_path_buf()]),
                HostId::new()
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn runtime_resolves_registered_roots_and_explains_expanded_home_escape() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let node = DevNode::new(&config(root.path(), data.path())).unwrap();
        let extra = root.path().join("extra");
        fs::create_dir_all(extra.join("src")).unwrap();
        let params = json!({"path": extra, "commandId":"cmd", "phase":"prepare"});
        node.workspace_rpc("workspace.register", params.clone())
            .unwrap();
        let mut commit = params;
        commit["phase"] = json!("commit");
        let result = node.workspace_rpc("workspace.register", commit).unwrap();
        let id: WorkspaceId = result["workspaceId"].as_str().unwrap().parse().unwrap();
        let (workspace, cwd) = node.resolve_workspace_cwd(Some(&id), Some("src")).unwrap();
        assert_eq!(workspace.meta.id, id);
        assert_eq!(cwd, fs::canonicalize(extra.join("src")).unwrap());
        let home = std::env::var_os("HOME").unwrap();
        let error = node
            .resolve_workspace_cwd(None, Some("~"))
            .unwrap_err()
            .to_string();
        assert!(error.contains(&PathBuf::from(home).display().to_string()));
        assert!(error.contains("outside registered workspaces"));
        assert!(error.contains(&node.workspaces().unwrap()[0].root_path));
        assert!(error.contains("POST /v1/hosts/"));
        for workspace in node.workspaces().unwrap() {
            let path = Path::new(&workspace.root_path);
            let command_id = workspace.meta.id.as_id().to_string();
            let mut registry = node.inner.workspace_registry.write().unwrap();
            mutation(
                &mut registry,
                "workspace.unregister",
                &command_id,
                path,
                WorkspaceMutationPhase::Prepare,
            )
            .unwrap();
            mutation(
                &mut registry,
                "workspace.unregister",
                &command_id,
                path,
                WorkspaceMutationPhase::Commit,
            )
            .unwrap();
        }
        assert!(
            node.resolve_workspace_cwd(None, None)
                .unwrap_err()
                .to_string()
                .contains("(none)")
        );
        assert!(
            node.worktree_rpc_capped("worktree.create", &json!({"name":"agent"}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn wire_create_preserves_registry_id_with_omitted_or_relative_cwd() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let extra = root.path().join("extra");
        fs::create_dir_all(extra.join("src")).unwrap();
        let config = config(root.path(), data.path()).with_workspaces(vec![extra]);
        let node = DevNode::new(&config).unwrap();
        let workspace = node.workspaces().unwrap()[1].clone();
        for cwd in [None, Some("src")] {
            let mut spec = json!({"workspaceId":workspace.meta.id, "driver":"claude-print"});
            if let Some(cwd) = cwd {
                spec["cwd"] = json!(cwd);
            }
            let created = crate::transport::hubnode::dispatch_method(
                &node,
                "instance.create",
                json!({"spec":spec}),
            )
            .await
            .unwrap();
            assert_eq!(created["instance"]["workspaceId"], json!(workspace.meta.id));
        }
        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn worktree_catalog_uses_selected_registered_project() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let extra = root.path().join("extra");
        fs::create_dir_all(&extra).unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .arg(&extra)
            .status()
            .unwrap();
        assert!(status.success());
        let node =
            DevNode::new(&config(root.path(), data.path()).with_workspaces(vec![extra.clone()]))
                .unwrap();
        let workspace = node.workspaces().unwrap()[1].clone();
        let result = node
            .worktree_rpc("worktree.list", &json!({"workspaceId":workspace.meta.id}))
            .unwrap();
        assert_eq!(
            result["workspaceRoot"],
            json!(fs::canonicalize(extra).unwrap())
        );
        assert!(
            node.worktree_rpc("worktree.list", &json!({"workspaceId":WorkspaceId::new()}))
                .is_err()
        );
    }

    #[test]
    fn prepared_unregister_cannot_delete_a_replacement_registration() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut registry =
            WorkspaceRegistry::open(&config(root.path(), data.path()), HostId::new()).unwrap();
        let path = root.path().join("project");
        mutation(
            &mut registry,
            "workspace.unregister",
            "old-delete",
            &path,
            WorkspaceMutationPhase::Prepare,
        )
        .unwrap();
        mutation(
            &mut registry,
            "workspace.unregister",
            "new-delete",
            &path,
            WorkspaceMutationPhase::Prepare,
        )
        .unwrap();
        mutation(
            &mut registry,
            "workspace.unregister",
            "new-delete",
            &path,
            WorkspaceMutationPhase::Commit,
        )
        .unwrap();
        mutation(
            &mut registry,
            "workspace.register",
            "new-register",
            &path,
            WorkspaceMutationPhase::Prepare,
        )
        .unwrap();
        mutation(
            &mut registry,
            "workspace.register",
            "new-register",
            &path,
            WorkspaceMutationPhase::Commit,
        )
        .unwrap();
        assert!(
            mutation(
                &mut registry,
                "workspace.unregister",
                "old-delete",
                &path,
                WorkspaceMutationPhase::Commit
            )
            .unwrap_err()
            .to_string()
            .contains("replaced")
        );
        assert_eq!(registry.workspaces().len(), 1);
    }

    #[test]
    fn prepared_register_does_not_resurrect_a_removed_membership_identity() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut registry =
            WorkspaceRegistry::open(&config(root.path(), data.path()), HostId::new()).unwrap();
        let path = root.path().join("project");
        let original_id = registry.workspaces()[0].meta.id.clone();
        mutation(
            &mut registry,
            "workspace.register",
            "register-existing",
            &path,
            WorkspaceMutationPhase::Prepare,
        )
        .unwrap();
        for command in ["old-delete", "new-delete"] {
            mutation(
                &mut registry,
                "workspace.unregister",
                command,
                &path,
                WorkspaceMutationPhase::Prepare,
            )
            .unwrap();
        }
        mutation(
            &mut registry,
            "workspace.unregister",
            "new-delete",
            &path,
            WorkspaceMutationPhase::Commit,
        )
        .unwrap();
        mutation(
            &mut registry,
            "workspace.register",
            "register-existing",
            &path,
            WorkspaceMutationPhase::Commit,
        )
        .unwrap();
        assert_ne!(registry.workspaces()[0].meta.id, original_id);
        assert!(
            mutation(
                &mut registry,
                "workspace.unregister",
                "old-delete",
                &path,
                WorkspaceMutationPhase::Commit
            )
            .unwrap_err()
            .to_string()
            .contains("replaced")
        );
        assert_eq!(registry.workspaces().len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn registered_root_cannot_be_replaced_with_an_escaping_symlink() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let node = DevNode::new(&config(root.path(), data.path())).unwrap();
        let project = root.path().join("project");
        fs::remove_dir(&project).unwrap();
        std::os::unix::fs::symlink(outside.path(), project).unwrap();
        assert!(
            node.resolve_workspace_cwd(None, None)
                .unwrap_err()
                .to_string()
                .contains("outside allowed workspace_roots")
        );
    }

    #[tokio::test]
    async fn restart_keeps_deleted_projects_listable_and_removable() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let config = config(root.path(), data.path());
        let host = HostId::new();
        let node = DevNode::with_host_id(&config, host.clone()).unwrap();
        let extra = root.path().join("extra");
        fs::create_dir(&extra).unwrap();
        let mut params = json!({"commandId":"register-extra", "path":extra, "phase":"prepare"});
        node.workspace_rpc("workspace.register", params.clone())
            .unwrap();
        params["phase"] = json!("commit");
        let result = node.workspace_rpc("workspace.register", params).unwrap();
        let id: WorkspaceId = result["workspaceId"].as_str().unwrap().parse().unwrap();
        let stored_root = node.workspaces().unwrap()[1].root_path.clone();
        drop(node);
        fs::remove_dir(extra).unwrap();
        let node = DevNode::with_host_id(&config, host.clone()).unwrap();
        assert_eq!(node.workspaces().unwrap().len(), 2);
        assert!(node.resolve_workspace_cwd(None, None).is_ok());
        assert!(node.resolve_workspace_cwd(Some(&id), None).is_err());
        let mut params = json!({"commandId":"remove-extra", "path":stored_root, "phase":"prepare"});
        node.workspace_rpc("workspace.unregister", params.clone())
            .unwrap();
        params["phase"] = json!("commit");
        node.workspace_rpc("workspace.unregister", params).unwrap();
        drop(node);
        assert_eq!(
            DevNode::with_host_id(&config, host)
                .unwrap()
                .workspaces()
                .unwrap()
                .len(),
            1
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restarted_registry_can_remove_symlink_drift_without_allowing_launch() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let config = config(root.path(), data.path());
        let host = HostId::new();
        let extra = root.path().join("extra");
        fs::create_dir(&extra).unwrap();
        let node = DevNode::with_host_id(
            &config.clone().with_workspaces(vec![extra.clone()]),
            host.clone(),
        )
        .unwrap();
        let workspace = node.workspaces().unwrap()[1].clone();
        drop(node);
        fs::remove_dir(&extra).unwrap();
        std::os::unix::fs::symlink(outside.path(), extra).unwrap();
        let node = DevNode::with_host_id(&config, host).unwrap();
        assert!(
            node.resolve_workspace_cwd(Some(&workspace.meta.id), None)
                .unwrap_err()
                .to_string()
                .contains("outside allowed workspace_roots")
        );
        let mut params =
            json!({"commandId":"remove-drift", "path":workspace.root_path, "phase":"prepare"});
        node.workspace_rpc("workspace.unregister", params.clone())
            .unwrap();
        params["phase"] = json!("commit");
        node.workspace_rpc("workspace.unregister", params).unwrap();
        assert_eq!(node.workspaces().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn restarted_registry_applies_tightened_allowlist_at_admission() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let extra = root.path().join("extra");
        fs::create_dir(&extra).unwrap();
        let config = config(root.path(), data.path());
        let host = HostId::new();
        let node =
            DevNode::with_host_id(&config.clone().with_workspaces(vec![extra]), host.clone())
                .unwrap();
        let workspace = node.workspaces().unwrap()[1].clone();
        drop(node);
        let config = config.with_workspace_roots(vec![root.path().join("project")]);
        let node = DevNode::with_host_id(&config, host).unwrap();
        assert!(node.resolve_workspace_cwd(None, None).is_ok());
        assert!(
            node.resolve_workspace_cwd(Some(&workspace.meta.id), None)
                .unwrap_err()
                .to_string()
                .contains("outside allowed workspace_roots")
        );
        assert_eq!(node.workspaces().unwrap().len(), 2);
    }

    #[test]
    fn denied_probe_precedes_canonicalization_and_preserves_remediation() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing");
        let message = "workspace access probe timed out; grant Full Disk Access";
        let error = canonical_directory_with_probe(&missing, |_| {
            Err(NodeError::InvalidRequest(message.into()))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), format!("invalid request: {message}"));
    }

    #[cfg(unix)]
    #[test]
    fn registration_checks_worktree_aliases_without_touching_stale_project_roots() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let worktrees = tempfile::tempdir().unwrap();
        let config = config(root.path(), data.path()).with_workspace_roots(vec![
            root.path().to_path_buf(),
            worktrees.path().to_path_buf(),
        ]);
        let registry = WorkspaceRegistry::open(&config, HostId::new()).unwrap();
        std::os::unix::fs::symlink(worktrees.path(), root.path().join("remuda-wt")).unwrap();
        let candidate = worktrees.path().join("agent");
        fs::create_dir(&candidate).unwrap();
        fs::remove_dir(root.path().join("project")).unwrap();
        assert!(
            registry
                .validate(&candidate)
                .unwrap_err()
                .to_string()
                .contains("worktree directory")
        );
    }

    #[tokio::test]
    async fn unregister_is_refused_while_a_live_session_uses_the_workspace() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let extra = root.path().join("extra");
        fs::create_dir_all(extra.join("src")).unwrap();
        let config = config(root.path(), data.path()).with_workspaces(vec![extra.clone()]);
        let node = DevNode::new(&config).unwrap();
        let workspace = node.workspaces().unwrap()[1].clone();

        // A long-lived shell in the workspace blocks the unregister prepare.
        let request: crate::CreateInstanceRequest = serde_json::from_value(json!({
            "workspaceId": workspace.meta.id,
            "kind": "terminal",
            "driver": "shell-pty",
            "args": ["sleep", "120"],
            "prompt": "",
        }))
        .unwrap();
        let created = node.create_instance(request).await.unwrap();
        let instance_id = created.instance.meta.id.clone();
        let error = node
            .workspace_rpc(
                "workspace.unregister",
                json!({"commandId": "remove-busy", "path": workspace.root_path, "phase": "prepare"}),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("1 live session"), "{error}");

        // A canonical alias for the same busy directory must not slip past
        // the guard.
        let aliased = format!("{}/./extra", root.path().canonicalize().unwrap().display());
        let error = node
            .workspace_rpc(
                "workspace.unregister",
                json!({"commandId": "remove-busy-alias", "path": aliased, "phase": "prepare"}),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("1 live session"), "{error}");

        // Other workspaces are unaffected.
        let first = node.workspaces().unwrap()[0].clone();
        node.workspace_rpc(
            "workspace.unregister",
            json!({"commandId": "remove-other", "path": first.root_path, "phase": "prepare"}),
        )
        .unwrap();

        // Once the live session is purged (purge closes the driver first),
        // the same command prepares.
        node.purge_instance(&instance_id).await.unwrap();
        node.workspace_rpc(
            "workspace.unregister",
            json!({"commandId": "remove-busy", "path": workspace.root_path, "phase": "prepare"}),
        )
        .unwrap();

        // Between prepare and commit the directory is unbinding: a new
        // session cannot enter in the check→unbind window.
        let blocked: crate::CreateInstanceRequest = serde_json::from_value(json!({
            "workspaceId": workspace.meta.id,
            "kind": "terminal",
            "driver": "shell-pty",
            "args": ["sleep", "1"],
            "prompt": "",
        }))
        .unwrap();
        let error = node.create_instance(blocked).await.unwrap_err().to_string();
        assert!(error.contains("being unregistered"), "{error}");

        node.workspace_rpc(
            "workspace.unregister",
            json!({"commandId": "remove-busy", "path": workspace.root_path, "phase": "commit"}),
        )
        .unwrap();
        assert!(
            node.workspaces()
                .unwrap()
                .iter()
                .all(|row| row.meta.id != workspace.meta.id)
        );
        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_session_whose_native_process_dies_with_an_error_unblocks_removal() {
        // Round 3 item 5: failed IS terminal (a non-zero native exit), so a
        // real error exit — driven through the native-exit path, not an SQL
        // update — ends occupancy just like a clean exit.
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let config = crate::DevServerConfig::loopback(0)
            .with_workspace_root(root.path().to_path_buf())
            .with_workspace_roots(vec![root.path().to_path_buf()])
            .with_workspace_registry(data.path().to_path_buf());
        let node = crate::DevNode::new(&config).unwrap();
        let workspace = node.workspaces().unwrap()[0].clone();

        // A foreground command that exits non-zero: the process is genuinely
        // gone, and the exit is terminal evidence (native-exit-code-1).
        let request: crate::CreateInstanceRequest = serde_json::from_value(json!({
            "workspaceId": workspace.meta.id,
            "kind": "terminal",
            "driver": "shell-pty",
            "args": ["/bin/sh", "-c", "exit 1"],
            "prompt": "",
        }))
        .unwrap();
        let created = node.create_instance(request).await.unwrap();
        let instance_id = created.instance.meta.id.clone();

        let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let instance = node
                    .list_instances()
                    .unwrap()
                    .items
                    .into_iter()
                    .find(|instance| instance.meta.id == instance_id)
                    .unwrap();
                if matches!(
                    instance.lifecycle,
                    remuda_protocol::InstanceLifecycle::Exited
                        | remuda_protocol::InstanceLifecycle::Failed
                ) {
                    break instance.lifecycle;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("native error exit detection");
        assert!(
            matches!(
                settled,
                remuda_protocol::InstanceLifecycle::Exited
                    | remuda_protocol::InstanceLifecycle::Failed
            ),
            "non-zero native exit is terminal: {settled:?}"
        );

        // Removal prepares without a purge: the dead session does not count.
        node.workspace_rpc(
            "workspace.unregister",
            json!({"commandId": "remove-failed", "path": workspace.root_path, "phase": "prepare"}),
        )
        .unwrap();
        node.workspace_rpc(
            "workspace.unregister",
            json!({"commandId": "remove-failed", "path": workspace.root_path, "phase": "commit"}),
        )
        .unwrap();
        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn create_after_resolution_is_refused_when_unregister_committed_in_the_gap() {
        // Round 4 item 2: resolve_workspace_cwd runs (slow fs resolution)
        // before the occupancy reservation. A barrier parks the create right
        // after resolution; an unregister commits in that window; when the
        // create resumes, the SAME write-lock step that reserves also
        // re-checks membership and refuses it.
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let config = crate::DevServerConfig::loopback(0)
            .with_workspace_root(root.path().to_path_buf())
            .with_workspace_roots(vec![root.path().to_path_buf()])
            .with_workspace_registry(data.path().to_path_buf());
        let node = crate::DevNode::new(&config).unwrap();
        let workspace = node.workspaces().unwrap()[0].clone();
        let workspace_id = workspace.meta.id.clone();
        let root_path = workspace.root_path.clone();

        let reached = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let (reached2, release2) = (reached.clone(), release.clone());
        let barrier: CreateBarrierFn = std::sync::Arc::new(move |_id| {
            let (reached, release) = (reached2.clone(), release2.clone());
            Box::pin(async move {
                reached.notify_one();
                release.notified().await;
            })
        });
        node.set_create_reservation_barrier(barrier).await;

        let node_for_create = node.clone();
        let create = tokio::spawn(async move {
            let request: crate::CreateInstanceRequest = serde_json::from_value(json!({
                "workspaceId": workspace_id,
                "kind": "terminal",
                "driver": "shell-pty",
                "args": ["/bin/true"],
                "prompt": "",
            }))
            .unwrap();
            node_for_create.create_instance(request).await
        });

        // Wait until the create is parked between resolution and reservation.
        reached.notified().await;
        // Commit an unregister for the resolved workspace.
        node.workspace_rpc(
            "workspace.unregister",
            json!({"commandId": "gap-unregister", "path": root_path, "phase": "prepare"}),
        )
        .unwrap();
        node.workspace_rpc(
            "workspace.unregister",
            json!({"commandId": "gap-unregister", "path": root_path, "phase": "commit"}),
        )
        .unwrap();
        // Release the parked create.
        release.notify_one();

        let result = create.await.expect("create task joins");
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("no longer registered"),
            "create after a committed unregister must be refused at the \
             membership+reservation step, got: {error}"
        );
        node.shutdown().await.unwrap();
    }

    #[test]
    fn expansion_uses_only_node_home_prefixes() {
        let home = Path::new("/home/node");
        for raw in ["~", "$HOME"] {
            assert_eq!(crate::worktree::expand_home(raw, Some(home)).unwrap(), home);
        }
        for raw in ["~/project", "$HOME/project"] {
            assert_eq!(
                crate::worktree::expand_home(raw, Some(home)).unwrap(),
                home.join("project")
            );
        }
        for raw in ["~someone", "$HOME_OTHER", "project/~", "$(whoami)"] {
            assert_eq!(
                crate::worktree::expand_home(raw, Some(home)).unwrap(),
                PathBuf::from(raw)
            );
        }
        assert!(crate::worktree::expand_home("~", None).is_err());
    }
}
