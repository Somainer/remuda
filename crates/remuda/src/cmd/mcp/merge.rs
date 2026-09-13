//! MCP merge tools: metadata and handler are registered together.

use super::Tool;
use crate::cmd::merge;
use serde_json::json;

pub(super) fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "remuda_merge",
            "Merge a local branch into main in a disposable worktree, run the shared gate, compare-and-swap main and push origin. Requires gate=true or dryRun=true. dryRun only inspects local refs. Reports exitCode 0 ok / 1 gate failed / 2 conflict / 3 CAS lost with step timings. Runs on the MCP server's machine.",
            json!({
                "type": "object",
                "required": ["branch"],
                "additionalProperties": false,
                "properties": {
                    "branch": { "type": "string" },
                    "gate": { "type": "boolean" },
                    "dryRun": { "type": "boolean" },
                    "affected": { "type": "boolean", "default": true },
                    "full": { "type": "boolean", "description": "Test the full workspace" },
                    "web": { "type": "boolean" },
                    "webE2e": { "type": "boolean", "description": "Run the live Hub Playwright suite" },
                    "noPush": { "type": "boolean" },
                    "repo": { "type": "string" },
                    "targetDir": { "type": "string" },
                    "message": { "type": "string" }
                }
            }),
            |_client, args| {
                Box::pin(async move {
                    let options: merge::MergeArgs = serde_json::from_value(args)?;
                    let report = tokio::task::spawn_blocking(move || merge::execute(options)).await?;
                    Ok(serde_json::to_value(report)?)
                })
            },
        ).report(),
    ]
}
