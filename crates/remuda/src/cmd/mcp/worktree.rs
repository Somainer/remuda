//! MCP worktree tools: metadata and handler are registered together.

use super::{
    Tool,
    args::{opt_str, reject_removed_args, required_str},
};
use crate::cmd::worktree;
use serde_json::json;

pub(super) fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "remuda_worktree_create",
            "Create a git worktree (`git worktree add -b wt/<name>/…`) at `../remuda-wt/<name>` beside the repository. The location is fixed; it is not caller-selectable.",
            json!({
                "type": "object",
                "required": ["name"],
                "additionalProperties": false,
                "properties": {
                    "name": { "type": "string" },
                    "base": { "type": "string" }
                }
            }),
            |_client, args| {
                Box::pin(async move {
                    let name = required_str(&args, "name")?;
                    let base = opt_str(&args, "base").unwrap_or("main");
                    reject_removed_args(&args, &["path", "repo"])?;
                    // No caller-supplied path or repo: `create` places the
                    // worktree under `<repo>/../remuda-wt/<name>`
                    // (security-review-2 M4).
                    let record = worktree::create(name, base, None, None)?;
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
            "Remove a registered linked Git worktree by Remuda name on this MCP server. Keeps the branch. Refuses primary/current/main, locks, or active merge/rebase; dirty files require explicit force=true.",
            json!({"type":"object","additionalProperties":false,"required":["name"],"properties":{
                "name":{"type":"string"},"force":{"type":"boolean"}
            }}),
            |_client, args| {
                Box::pin(async move {
                    // `repo` is not accepted: it would let an agent operate on
                    // a repository other than this server's (M4). The name is
                    // resolved against the local catalog.
                    reject_removed_args(&args, &["repo"])?;
                    #[derive(serde::Deserialize)]
                    #[serde(deny_unknown_fields)]
                    struct RemoveArgs {
                        name: String,
                        #[serde(default)]
                        force: bool,
                    }
                    let options: RemoveArgs = serde_json::from_value(args)?;
                    tokio::task::spawn_blocking(move || {
                        worktree::remove(&options.name, None, options.force)
                    })
                    .await?
                })
            },
        ),
    ]
}
