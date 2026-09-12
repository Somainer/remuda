//! Typed server notifications plus [`ServerNotification::Unknown`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::{Thread, ThreadItem, ThreadStatus, ThreadTokenUsage, Turn};

/// Server→client notification. Unknown methods become [`ServerNotification::Unknown`]
/// instead of failing the reader.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerNotification {
    /// Known `method` + `params`.
    Typed(TypedServerNotification),
    /// Catch-all. Includes the original object so callers can inspect `emittedAtMs`.
    Unknown(Value),
}

/// Internally tagged notification (`method` / `params`). Extra top-level fields
/// such as `emittedAtMs` are ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum TypedServerNotification {
    /// Native error. `willRetry` true still waits for turn completion.
    #[serde(rename = "error")]
    Error(ErrorNotification),
    /// Thread object after `thread/start`.
    #[serde(rename = "thread/started")]
    ThreadStarted(ThreadStartedNotification),
    /// idle / active / notLoaded / systemError.
    #[serde(rename = "thread/status/changed")]
    ThreadStatusChanged(ThreadStatusChangedNotification),
    /// Cumulative usage. Resume may replay the same totals.
    #[serde(rename = "thread/tokenUsage/updated")]
    ThreadTokenUsageUpdated(ThreadTokenUsageUpdatedNotification),
    /// Display name.
    #[serde(rename = "thread/name/updated")]
    ThreadNameUpdated(Value),
    /// Archived.
    #[serde(rename = "thread/archived")]
    ThreadArchived(Value),
    /// Unarchived.
    #[serde(rename = "thread/unarchived")]
    ThreadUnarchived(Value),
    /// Deleted.
    #[serde(rename = "thread/deleted")]
    ThreadDeleted(Value),
    /// Closed.
    #[serde(rename = "thread/closed")]
    ThreadClosed(Value),
    /// Reverted.
    #[serde(rename = "thread/reverted")]
    ThreadReverted(Value),
    /// Turn accepted and running.
    #[serde(rename = "turn/started")]
    TurnStarted(TurnStartedNotification),
    /// Terminal turn. Status is the native outcome.
    #[serde(rename = "turn/completed")]
    TurnCompleted(TurnCompletedNotification),
    /// Aggregated diff snapshot for the turn.
    #[serde(rename = "turn/diff/updated")]
    TurnDiffUpdated(Value),
    /// Plan snapshot.
    #[serde(rename = "turn/plan/updated")]
    TurnPlanUpdated(Value),
    /// Item started.
    #[serde(rename = "item/started")]
    ItemStarted(ItemStartedNotification),
    /// Item completed.
    #[serde(rename = "item/completed")]
    ItemCompleted(ItemCompletedNotification),
    /// Assistant text delta. `params.delta` is a string.
    #[serde(rename = "item/agentMessage/delta")]
    AgentMessageDelta(AgentMessageDeltaNotification),
    /// Reasoning text delta.
    #[serde(rename = "item/reasoning/textDelta")]
    ReasoningTextDelta(Value),
    /// Reasoning summary delta.
    #[serde(rename = "item/reasoning/summaryTextDelta")]
    ReasoningSummaryTextDelta(Value),
    /// Command stdout/stderr delta.
    #[serde(rename = "item/commandExecution/outputDelta")]
    CommandExecutionOutputDelta(Value),
    /// File-change patch update.
    #[serde(rename = "item/fileChange/patchUpdated")]
    FileChangePatchUpdated(Value),
    /// Plan text delta.
    #[serde(rename = "item/plan/delta")]
    PlanDelta(Value),
    /// MCP server startup.
    #[serde(rename = "mcpServer/startupStatus/updated")]
    McpServerStartupStatusUpdated(McpServerStartupStatusUpdatedNotification),
    /// Warning; not fatal.
    #[serde(rename = "warning")]
    Warning(WarningNotification),
    /// Deprecation notice; not fatal.
    #[serde(rename = "deprecationNotice")]
    DeprecationNotice(DeprecationNoticeNotification),
    /// Config warning.
    #[serde(rename = "configWarning")]
    ConfigWarning(Value),
    /// Guardian warning.
    #[serde(rename = "guardianWarning")]
    GuardianWarning(Value),
    /// Rate-limit snapshot. Payload is left untyped.
    #[serde(rename = "account/rateLimits/updated")]
    AccountRateLimitsUpdated(Value),
    /// Remote-control status. Probe emits this after initialize.
    #[serde(rename = "remoteControl/status/changed")]
    RemoteControlStatusChanged(Value),
    /// Server-request lifecycle.
    #[serde(rename = "serverRequest/resolved")]
    ServerRequestResolved(Value),
    /// Hook started.
    #[serde(rename = "hook/started")]
    HookStarted(Value),
    /// Hook completed.
    #[serde(rename = "hook/completed")]
    HookCompleted(Value),
}

/// Native `error` notification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorNotification {
    /// Error object. May include `message` and provider details.
    #[serde(default)]
    pub error: Value,
    /// When true the server will retry; do not treat as turn failure yet.
    #[serde(default)]
    pub will_retry: bool,
    /// Thread id.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Turn id.
    #[serde(default)]
    pub turn_id: Option<String>,
}

/// `thread/started`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartedNotification {
    /// Thread object.
    pub thread: Thread,
}

/// `thread/status/changed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStatusChangedNotification {
    /// Thread id.
    pub thread_id: String,
    /// New status.
    pub status: ThreadStatus,
}

/// `thread/tokenUsage/updated`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTokenUsageUpdatedNotification {
    /// Thread id.
    pub thread_id: String,
    /// Turn id.
    #[serde(default)]
    pub turn_id: Option<String>,
    /// Usage snapshot.
    pub token_usage: ThreadTokenUsage,
}

/// `turn/started`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartedNotification {
    /// Thread id.
    pub thread_id: String,
    /// Turn object.
    pub turn: Turn,
}

/// `turn/completed`. Inspect `turn.status` for succeeded/cancelled/failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnCompletedNotification {
    /// Thread id.
    pub thread_id: String,
    /// Completed turn. `itemsView` may be `summary`.
    pub turn: Turn,
}

/// `item/started`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemStartedNotification {
    /// Item.
    pub item: ThreadItem,
    /// Thread id.
    pub thread_id: String,
    /// Turn id.
    pub turn_id: String,
    /// Start timestamp in milliseconds.
    #[serde(default)]
    pub started_at_ms: Option<i64>,
}

/// `item/completed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemCompletedNotification {
    /// Item.
    pub item: ThreadItem,
    /// Thread id.
    pub thread_id: String,
    /// Turn id.
    pub turn_id: String,
    /// Completion timestamp in milliseconds.
    #[serde(default)]
    pub completed_at_ms: Option<i64>,
}

/// `item/agentMessage/delta`. `delta` is a string, not an object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessageDeltaNotification {
    /// Thread id.
    pub thread_id: String,
    /// Turn id.
    pub turn_id: String,
    /// Item id.
    pub item_id: String,
    /// Text fragment.
    pub delta: String,
}

/// `mcpServer/startupStatus/updated`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStartupStatusUpdatedNotification {
    /// Thread id.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// MCP server name.
    pub name: String,
    /// `starting` / `ready` / `failed` / `cancelled`.
    pub status: String,
    /// Error text.
    #[serde(default)]
    pub error: Option<String>,
    /// Failure reason.
    #[serde(default)]
    pub failure_reason: Option<Value>,
}

/// `warning`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WarningNotification {
    /// Optional thread.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Warning text.
    pub message: String,
}

/// `deprecationNotice`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeprecationNoticeNotification {
    /// Summary.
    pub summary: String,
    /// Extra detail.
    #[serde(default)]
    pub details: Option<String>,
}

impl ServerNotification {
    /// Classify a notification frame. Never returns a decode error.
    pub fn from_method_params(method: &str, params: Option<Value>, raw: Value) -> Self {
        let tagged = serde_json::json!({
            "method": method,
            "params": params.unwrap_or(Value::Null),
        });
        match serde_json::from_value::<TypedServerNotification>(tagged) {
            Ok(typed) => Self::Typed(typed),
            Err(_) => Self::Unknown(raw),
        }
    }

    /// Wire method name.
    pub fn method(&self) -> Option<&str> {
        match self {
            Self::Typed(typed) => Some(typed.method_name()),
            Self::Unknown(value) => value.get("method").and_then(Value::as_str),
        }
    }

    /// True when this is the catch-all variant.
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown(_))
    }
}

impl TypedServerNotification {
    fn method_name(&self) -> &'static str {
        match self {
            Self::Error(_) => "error",
            Self::ThreadStarted(_) => "thread/started",
            Self::ThreadStatusChanged(_) => "thread/status/changed",
            Self::ThreadTokenUsageUpdated(_) => "thread/tokenUsage/updated",
            Self::ThreadNameUpdated(_) => "thread/name/updated",
            Self::ThreadArchived(_) => "thread/archived",
            Self::ThreadUnarchived(_) => "thread/unarchived",
            Self::ThreadDeleted(_) => "thread/deleted",
            Self::ThreadClosed(_) => "thread/closed",
            Self::ThreadReverted(_) => "thread/reverted",
            Self::TurnStarted(_) => "turn/started",
            Self::TurnCompleted(_) => "turn/completed",
            Self::TurnDiffUpdated(_) => "turn/diff/updated",
            Self::TurnPlanUpdated(_) => "turn/plan/updated",
            Self::ItemStarted(_) => "item/started",
            Self::ItemCompleted(_) => "item/completed",
            Self::AgentMessageDelta(_) => "item/agentMessage/delta",
            Self::ReasoningTextDelta(_) => "item/reasoning/textDelta",
            Self::ReasoningSummaryTextDelta(_) => "item/reasoning/summaryTextDelta",
            Self::CommandExecutionOutputDelta(_) => "item/commandExecution/outputDelta",
            Self::FileChangePatchUpdated(_) => "item/fileChange/patchUpdated",
            Self::PlanDelta(_) => "item/plan/delta",
            Self::McpServerStartupStatusUpdated(_) => "mcpServer/startupStatus/updated",
            Self::Warning(_) => "warning",
            Self::DeprecationNotice(_) => "deprecationNotice",
            Self::ConfigWarning(_) => "configWarning",
            Self::GuardianWarning(_) => "guardianWarning",
            Self::AccountRateLimitsUpdated(_) => "account/rateLimits/updated",
            Self::RemoteControlStatusChanged(_) => "remoteControl/status/changed",
            Self::ServerRequestResolved(_) => "serverRequest/resolved",
            Self::HookStarted(_) => "hook/started",
            Self::HookCompleted(_) => "hook/completed",
        }
    }
}
