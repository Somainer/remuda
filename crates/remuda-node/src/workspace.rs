//! Durable Node-owned workspace membership and registration policy (D-023).

use crate::{DevNode, DevServerConfig, NodeError};
use remuda_protocol::hubnode::{WorkspaceMutationParams, WorkspaceMutationPhase};
use remuda_protocol::{HostId, Workspace, WorkspaceId, path_guard};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegistryState {
    revision: u64,
    workspaces: Vec<Workspace>,
    commands: BTreeMap<String, Mutation>,
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

pub(crate) struct WorkspaceRegistry {
    state: RegistryState,
    file: Option<PathBuf>,
    roots: Vec<PathBuf>,
    host_id: HostId,
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
        let roots = configured_roots
            .iter()
            .map(|root| canonical_directory(root))
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

    pub(crate) fn snapshot(&self) -> Value {
        json!({"workspaceRevision": self.state.revision, "workspaces": self.state.workspaces.iter().map(|workspace| {
            json!({"workspaceId": workspace.meta.id, "hostId": workspace.host_id, "root": workspace.root_path})
        }).collect::<Vec<_>>()})
    }

    fn validate(&self, path: &Path) -> Result<PathBuf, NodeError> {
        let canonical = canonical_directory(path)?;
        if !self.roots.iter().any(|root| canonical.starts_with(root)) {
            return Err(NodeError::InvalidRequest(format!(
                "workspace {} is outside allowed workspace_roots: {}",
                canonical.display(),
                display_roots(self.roots.iter().map(PathBuf::as_path))
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

    fn mutate(
        &mut self,
        method: &str,
        params: WorkspaceMutationParams,
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
                    let canonical = if method == "workspace.register" {
                        self.validate(Path::new(&params.path))?
                    } else {
                        // A deleted project remains removable using its stored canonical absolute path.
                        let path = Path::new(&params.path);
                        if !path.is_absolute() {
                            return Err(NodeError::InvalidRequest(
                                "workspace path must be absolute".into(),
                            ));
                        }
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
                        if !self
                            .state
                            .workspaces
                            .iter()
                            .any(|workspace| Path::new(&workspace.root_path) == canonical)
                        {
                            return Err(NodeError::InvalidRequest(format!(
                                "workspace {} is not registered",
                                path.display()
                            )));
                        }
                        canonical
                    };
                    let existing = self
                        .state
                        .workspaces
                        .iter()
                        .find(|workspace| Path::new(&workspace.root_path) == canonical)
                        .map(|workspace| workspace.meta.id.clone());
                    let was_registered = existing.is_some();
                    let workspace_id = existing.unwrap_or_default();
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
                        if next.workspaces.iter().any(|workspace| {
                            Path::new(&workspace.root_path) == command.canonical
                                && workspace.meta.id != command.workspace_id
                        }) {
                            return Err(NodeError::Conflict(
                                "workspace was replaced after unregister prepare".into(),
                            ));
                        }
                        let previous_len = next.workspaces.len();
                        next.workspaces
                            .retain(|workspace| workspace.meta.id != command.workspace_id);
                        if previous_len != next.workspaces.len() {
                            next.revision += 1;
                        }
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
        "workspace.list" | "workspace.register" | "workspace.unregister"
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
        self.inner
            .workspace_registry
            .write()
            .map_err(|_| NodeError::StorePoisoned)?
            .mutate(method, serde_json::from_value(params)?)
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
            return Ok((selected.clone(), PathBuf::from(&selected.root_path)));
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
            },
        )
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
        assert!(node.create_worktree(&json!({"name":"agent"})).is_err());
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
