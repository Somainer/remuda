//! Parsing the `<task-notification>` records Claude injects when a background
//! task (a background `Agent`/`Task` subagent or a background shell/monitor)
//! finishes.
//!
//! The record arrives in the transcript as a `user` message — `promptSource:
//! "system"`, `origin.kind: "task-notification"` — whose text is XML-ish tags:
//!
//! ```text
//! <task-notification>
//! <task-id>acf1e742…</task-id>
//! <tool-use-id>toolu_vrtx_01DS…</tool-use-id>
//! <output-file>…/tasks/acf1e742….output</output-file>
//! <status>completed</status>
//! <summary>Agent "…" finished</summary>
//! <result>…the subagent's final report…</result>
//! </task-notification>
//! ```
//!
//! It is also mirrored by `queue-operation` enqueue/remove records, but only
//! the injected user record carries the `<tool-use-id>` that joins the
//! completion back to the original Agent tool call. Without that join the
//! structured view's task row never leaves its running state: the launch
//! tool_result is written *immediately* (`toolUseResult.isAsync = true`,
//! `status = "async_launched"`) and the real completion arrives only here.
//!
//! Pure data + logic, no IO; shared by the two transcript mappers
//! (`remuda-journal` and `remuda-driver`) so both fold identically.

use crate::ToolOutcome;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One parsed `<task-notification>` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskNotification {
    /// `<task-id>` — the background task / subagent agentId (16 hex for a
    /// subagent, `bsl…` for a background shell command).
    pub task_id: String,
    /// `<tool-use-id>` — the native `tool_use_id` of the launch call. Present
    /// on 2.1.221+; older records can omit it, in which case the completion
    /// cannot be folded onto a tool row.
    pub tool_use_id: Option<String>,
    /// Raw `<status>` spelling (`completed` / `killed` / `failed` / …).
    pub status: String,
    /// `<summary>` when present (one human-readable line).
    pub summary: Option<String>,
    /// `<result>` body when present (the subagent's final report).
    pub result: Option<String>,
}

impl TaskNotification {
    /// Parse one injected user-record text. Returns `None` when the text is
    /// not a task notification or lacks both a task id and a tool-use id.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (body, _) = tagged(text, "task-notification")?;
        let task_id = tagged(body, "task-id").map(|(v, _)| v.trim().to_owned())?;
        let tool_use_id = tagged(body, "tool-use-id")
            .map(|(v, _)| v.trim().to_owned())
            .filter(|v| !v.is_empty());
        let status = tagged(body, "status")
            .map(|(v, _)| v.trim().to_owned())
            .unwrap_or_else(|| "completed".to_owned());
        let summary = tagged(body, "summary").map(|(v, _)| v.trim().to_owned());
        let result = tagged(body, "result").map(|(v, _)| v.trim().to_owned());
        Some(Self {
            task_id,
            tool_use_id,
            status,
            summary,
            result,
        })
    }

    /// The tool-row outcome a completion with this status represents.
    ///
    /// `killed` reads as failed: the task track's failure vocabulary is
    /// `failed`/`denied` and a deliberately stopped subagent must not keep a
    /// running row. `completed` succeeds; anything unrecognised stays unknown
    /// rather than being optimistically marked done.
    #[must_use]
    pub fn outcome(&self) -> ToolOutcome {
        match self.status.as_str() {
            "completed" | "done" | "success" | "succeeded" => ToolOutcome::Succeeded,
            "killed" | "failed" | "error" | "errored" | "terminated" | "crashed" => {
                ToolOutcome::Failed
            }
            "cancelled" | "canceled" | "stopped" | "interrupted" => ToolOutcome::Cancelled,
            _ => ToolOutcome::Unknown,
        }
    }
}

/// Extract the inner text of the first `<tag>…</tag>` occurrence, returning
/// `(inner, remainder_after_close)`. Tolerates newlines and extra content.
fn tagged<'a>(text: &'a str, tag: &str) -> Option<(&'a str, &'a str)> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let rest = &text[start..];
    let end = rest.find(&close)?;
    Some((&rest[..end], &rest[end + close.len()..]))
}

/// Whether a launch `toolUseResult` sidecar is an asynchronous launch rather
/// than the subagent's real result.
///
/// Both background and (on some builds) foreground subagents launch this way:
/// the tool returns immediately with `{isAsync: true, status:
/// "async_launched", agentId}` and completion arrives later as a
/// [`TaskNotification`]. Such a result is [`ResultStage::Partial`], never a
/// final.
///
/// [`ResultStage::Partial`]: crate::ResultStage::Partial
#[must_use]
pub fn tool_result_is_async_launch(sidecar: Option<&serde_json::Value>) -> bool {
    let Some(value) = sidecar else {
        return false;
    };
    if value.get("isAsync").and_then(serde_json::Value::as_bool) == Some(true) {
        return true;
    }
    matches!(
        value.get("status").and_then(serde_json::Value::as_str),
        Some("async_launched" | "launched" | "backgrounded")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMPLETED: &str = "<task-notification>\n<task-id>acf1e742ec621d5f2</task-id>\n<tool-use-id>toolu_vrtx_01DSao3YudJggcUggGojkrZj</tool-use-id>\n<output-file>/tmp/x/tasks/acf.output</output-file>\n<status>completed</status>\n<summary>Agent \"Map\" finished</summary>\n<note>may notify more than once</note>\n<result>All done.\nLine two.</result>\n</task-notification>";

    #[test]
    fn parses_a_recorded_completion() {
        let note = TaskNotification::parse(COMPLETED).expect("parse");
        assert_eq!(note.task_id, "acf1e742ec621d5f2");
        assert_eq!(
            note.tool_use_id.as_deref(),
            Some("toolu_vrtx_01DSao3YudJggcUggGojkrZj")
        );
        assert_eq!(note.status, "completed");
        assert_eq!(note.summary.as_deref(), Some("Agent \"Map\" finished"));
        assert_eq!(note.result.as_deref(), Some("All done.\nLine two."));
        assert_eq!(note.outcome(), ToolOutcome::Succeeded);
    }

    #[test]
    fn a_killed_task_is_a_failed_outcome() {
        let text = "<task-notification><task-id>bpbkdwqbh</task-id>\
<tool-use-id>toolu_01NV</tool-use-id><status>killed</status>\
<summary>Monitor stopped</summary></task-notification>";
        let note = TaskNotification::parse(text).expect("parse");
        assert_eq!(note.outcome(), ToolOutcome::Failed);
    }

    #[test]
    fn non_notifications_do_not_parse() {
        assert!(TaskNotification::parse("ordinary text").is_none());
        assert!(TaskNotification::parse("<task-notification></task-notification>").is_none());
    }

    #[test]
    fn async_launch_sidecars_are_recognised() {
        assert!(tool_result_is_async_launch(Some(&serde_json::json!({
            "isAsync": true,
            "status": "async_launched",
            "agentId": "acf1e742"
        }))));
        assert!(!tool_result_is_async_launch(Some(&serde_json::json!({
            "status": "completed"
        }))));
        assert!(!tool_result_is_async_launch(None));
    }
}
