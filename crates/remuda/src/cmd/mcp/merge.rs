//! MCP merge tools: metadata and handler are registered together.

use super::Tool;
use crate::cmd::merge;
use serde_json::json;

pub(super) fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "remuda_merge",
            "Merge a local branch into main in a disposable worktree, run the shared gate, compare-and-swap main and push origin. Requires gate=true or dryRun=true. dryRun only inspects local refs. onto=<sha|main> verifies on an explicit base without advancing main; land fast-forwards main to that persisted verification (exit 2 base_moved when main changed); queue verifies branches optimistically across lanes. Reports exitCode 0 ok / 1 gate failed / 2 conflict or base_moved / 3 CAS lost with step timings. Runs on the MCP server's machine.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "branch": { "type": "string", "description": "Branch to merge; omitted when queue is given" },
                    "gate": { "type": "boolean" },
                    "dryRun": { "type": "boolean" },
                    "onto": { "type": "string", "description": "Verify merged onto this sha or main without advancing main" },
                    "land": { "type": "boolean", "description": "Advance main to the report verified for branch+onto without re-running the gate" },
                    "queue": { "type": "array", "items": { "type": "string" }, "description": "Branches to verify as an optimistic queue (sets branch aside)" },
                    "lanes": { "type": "integer", "minimum": 1, "default": 2 },
                    "e2ePortBase": { "type": "integer", "default": 58980 },
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
