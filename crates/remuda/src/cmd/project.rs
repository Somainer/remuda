//! `remuda project` — CLI shell over the authoritative Hub Project entity.
//!
//! Design §3.1: the Hub `Project` is the authority; this command group only
//! creates and edits it. The repo `.remuda/project.toml` stays advisory and
//! is never read here for authoritative fields.

use clap::{Args, Subcommand};
use serde_json::{Value, json};

use super::hub_client::{HubClient, HubOpts, block_on, print_json};

/// `remuda project` subcommands.
#[derive(Args)]
#[command(about = "Manage Hub projects (authority lives in the Hub, not the repo).")]
pub(crate) struct ProjectArgs {
    #[command(flatten)]
    hub: HubOpts,
    #[command(subcommand)]
    command: ProjectCommand,
}

impl super::registry::Entrypoint for ProjectArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        run(self.hub, self.command).map(|()| 0)
    }
}

#[derive(Subcommand)]
enum ProjectCommand {
    /// List projects visible to this caller.
    List,
    /// Show one project as JSON.
    Show {
        /// `prj_…` id.
        id: String,
    },
    /// Create a project; prints the stored document.
    Create {
        /// Display name.
        #[arg(long)]
        name: String,
        /// T2 seat host (`hst_…`).
        #[arg(long)]
        home_host: Option<String>,
        /// Git remote, validation only.
        #[arg(long)]
        repo_remote: Option<String>,
        /// Default base branch.
        #[arg(long)]
        default_base_branch: Option<String>,
        /// Worktree branch pattern.
        #[arg(long)]
        branch_pattern: Option<String>,
        /// Default provider delegation (`gateway` / `direct` / `none`).
        #[arg(long)]
        delegation: Option<String>,
        /// Default provider profile id (`pvp_…`).
        #[arg(long)]
        provider_profile_id: Option<String>,
        /// Default effort tier.
        #[arg(long)]
        default_effort: Option<String>,
        /// Permission posture (`ask` / `accept-edits` / `bypass`).
        #[arg(long)]
        permission_posture: Option<String>,
        /// Seed member as `hostId:workspaceId`; repeat for more.
        #[arg(long = "member", value_name = "HOST:WORKSPACE")]
        members: Vec<String>,
    },
    /// Update project settings (`null` clears nullable fields).
    Set {
        /// `prj_…` id.
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        home_host: Option<String>,
        #[arg(long)]
        repo_remote: Option<String>,
        #[arg(long)]
        default_base_branch: Option<String>,
        #[arg(long)]
        branch_pattern: Option<String>,
        #[arg(long)]
        delegation: Option<String>,
        #[arg(long)]
        provider_profile_id: Option<String>,
        #[arg(long)]
        default_effort: Option<String>,
        #[arg(long)]
        permission_posture: Option<String>,
        /// Set `policy.configurable.maxConcurrentWorkers`.
        #[arg(long)]
        max_concurrent_workers: Option<i64>,
        /// Set `policy.configurable.maxDelegationDepth`.
        #[arg(long)]
        max_delegation_depth: Option<i64>,
        /// Set `policy.configurable.coordinatorFanOut`.
        #[arg(long)]
        coordinator_fan_out: Option<i64>,
        /// Set `policy.configurable.allowMultipleDispatchers`.
        #[arg(long)]
        allow_multiple_dispatchers: Option<bool>,
        /// Replace the whole gate configuration from a JSON document
        /// (`{lanes:[{id,hostId,repoPath,targetDir,ports,env,lockPath,
        /// pwEndpoint,toolchainPath,timeouts}], web, affected, timeouts}`) or
        /// `@file.json`. `timeouts` maps a step name to seconds (0 = no cap);
        /// a per-lane entry overrides the project-level one.
        #[arg(long)]
        gate: Option<String>,
    },
    /// Add a `(hostId, workspaceId)` member.
    AddMember {
        /// `prj_…` id.
        id: String,
        /// Member host (`hst_…`).
        #[arg(long)]
        host: String,
        /// Registered workspace on that host (`wsp_…`).
        #[arg(long)]
        workspace: String,
        /// Display-only member role.
        #[arg(long)]
        role: Option<String>,
    },
    /// Remove a `(hostId, workspaceId)` member.
    RemoveMember {
        /// `prj_…` id.
        id: String,
        /// Member host (`hst_…`).
        #[arg(long)]
        host: String,
        /// Workspace to remove (`wsp_…`).
        #[arg(long)]
        workspace: String,
    },
}

fn run(hub: HubOpts, command: ProjectCommand) -> anyhow::Result<()> {
    block_on(async move {
        let client = hub.connect()?;
        let value = match command {
            ProjectCommand::List => list(&client).await?,
            ProjectCommand::Show { id } => client.get(&format!("/v1/projects/{id}")).await?,
            ProjectCommand::Create {
                name,
                home_host,
                repo_remote,
                default_base_branch,
                branch_pattern,
                delegation,
                provider_profile_id,
                default_effort,
                permission_posture,
                members,
            } => {
                let mut body = json!({ "name": name });
                put(&mut body, "homeHost", home_host);
                put(&mut body, "repoRemote", repo_remote);
                put(&mut body, "defaultBaseBranch", default_base_branch);
                put(&mut body, "branchPattern", branch_pattern);
                put(&mut body, "defaultEffort", default_effort);
                put(&mut body, "permissionPosture", permission_posture);
                if delegation.is_some() || provider_profile_id.is_some() {
                    body["provider"] = json!({
                        "delegation": delegation,
                        "profileId": provider_profile_id,
                    });
                }
                if !members.is_empty() {
                    let parsed: anyhow::Result<Vec<Value>> =
                        members.iter().map(|pair| member_value(pair)).collect();
                    body["members"] = json!(parsed?);
                }
                client.post("/v1/projects", &body).await?
            }
            ProjectCommand::Set {
                id,
                name,
                home_host,
                repo_remote,
                default_base_branch,
                branch_pattern,
                delegation,
                provider_profile_id,
                default_effort,
                permission_posture,
                max_concurrent_workers,
                max_delegation_depth,
                coordinator_fan_out,
                allow_multiple_dispatchers,
                gate,
            } => {
                let mut body = json!({});
                put(&mut body, "name", name);
                put(&mut body, "homeHost", home_host.map(Value::from));
                put(&mut body, "repoRemote", repo_remote.map(Value::from));
                put(&mut body, "defaultBaseBranch", default_base_branch);
                put(&mut body, "branchPattern", branch_pattern);
                put(&mut body, "defaultEffort", default_effort.map(Value::from));
                put(
                    &mut body,
                    "permissionPosture",
                    permission_posture.map(Value::from),
                );
                if delegation.is_some() || provider_profile_id.is_some() {
                    body["provider"] = json!({
                        "delegation": delegation,
                        "profileId": provider_profile_id,
                    });
                }
                let mut configurable = json!({});
                if let Some(value) = max_concurrent_workers {
                    configurable["maxConcurrentWorkers"] = json!(value);
                }
                if let Some(value) = max_delegation_depth {
                    configurable["maxDelegationDepth"] = json!(value);
                }
                if let Some(value) = coordinator_fan_out {
                    configurable["coordinatorFanOut"] = json!(value);
                }
                if let Some(value) = allow_multiple_dispatchers {
                    configurable["allowMultipleDispatchers"] = json!(value);
                }
                if configurable.as_object().is_some_and(|map| !map.is_empty()) {
                    body["policy"] = json!({ "configurable": configurable });
                }
                if let Some(raw) = gate {
                    let text = raw
                        .strip_prefix('@')
                        .map_or(Ok(raw.clone()), std::fs::read_to_string)?;
                    body["gate"] = serde_json::from_str::<Value>(text.trim())?;
                }
                client.patch(&format!("/v1/projects/{id}"), &body).await?
            }
            ProjectCommand::AddMember {
                id,
                host,
                workspace,
                role,
            } => {
                let mut body = json!({ "hostId": host, "workspaceId": workspace });
                put(&mut body, "role", role);
                client
                    .post(&format!("/v1/projects/{id}/members"), &body)
                    .await?
            }
            ProjectCommand::RemoveMember {
                id,
                host,
                workspace,
            } => {
                client
                    .delete(
                        &format!("/v1/projects/{id}/members"),
                        &json!({ "hostId": host, "workspaceId": workspace }),
                    )
                    .await?
            }
        };
        print_json(&value)
    })
}

async fn list(client: &HubClient) -> anyhow::Result<Value> {
    Ok(client.get("/v1/projects").await?)
}

/// Parse a `hst_…:wsp_…` member argument.
fn member_value(pair: &str) -> anyhow::Result<Value> {
    let (host, workspace) = pair
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("--member expects HOST:WORKSPACE, got {pair:?}"))?;
    if host.trim().is_empty() || workspace.trim().is_empty() {
        anyhow::bail!("--member expects non-empty HOST:WORKSPACE, got {pair:?}");
    }
    Ok(json!({ "hostId": host.trim(), "workspaceId": workspace.trim() }))
}

fn put<T: Into<Value>>(body: &mut Value, key: &str, value: Option<T>) {
    if let Some(value) = value
        && let Some(object) = body.as_object_mut()
    {
        object.insert(key.into(), value.into());
    }
}
