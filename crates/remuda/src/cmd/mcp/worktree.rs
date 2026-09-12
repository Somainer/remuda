//! MCP worktree tools: metadata and handler are registered together.

use super::{
    Tool,
    args::{opt_str, required_str},
};
use crate::cmd::worktree;
use serde_json::json;

pub(super) fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "remuda_worktree_create",
            "Create a git worktree (`git worktree add -b wt/<name>/…`). Default path `../remuda-wt/<name>`.",
            json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string" },
                    "base": { "type": "string" },
                    "path": { "type": "string" },
                    "repo": { "type": "string" }
                }
            }),
            |_client, args| {
                Box::pin(async move {
                    let name = required_str(&args, "name")?;
                    let base = opt_str(&args, "base").unwrap_or("main");
                    let path = opt_str(&args, "path").map(std::path::PathBuf::from);
                    let repo = opt_str(&args, "repo").map(std::path::PathBuf::from);
                    let record = worktree::create(name, base, path.as_deref(), repo.as_deref())?;
                    Ok(json!({
                        "name": record.name,
                        "path": record.path,
                        "branch": record.branch,
                        "base": record.base,
                    }))
                })
            },
        ),
        Tool::new(
            "remuda_worktree_rm",
            "Remove a registered linked Git worktree by Remuda name or explicit path on this MCP server. Keeps the branch. Refuses primary/current/main, locks, or active merge/rebase; dirty files require explicit force=true.",
            json!({"type":"object","additionalProperties":false,"required":["name"],"properties":{
                "name":{"type":"string"},"repo":{"type":"string"},"force":{"type":"boolean"}
            }}),
            |_client, args| {
                Box::pin(async move {
                    #[derive(serde::Deserialize)]
                    #[serde(deny_unknown_fields)]
                    struct RemoveArgs {
                        name: String,
                        repo: Option<std::path::PathBuf>,
                        #[serde(default)]
                        force: bool,
                    }
                    let options: RemoveArgs = serde_json::from_value(args)?;
                    tokio::task::spawn_blocking(move || {
                        worktree::remove(&options.name, options.repo.as_deref(), options.force)
                    })
                    .await?
                })
            },
        ),
    ]
}
