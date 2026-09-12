//! Claude Code stream-json types (Agent SDK 0.3.x / CLI 2.1.x).
//!
//! Unknown `type` / `subtype` values become [`Outbound::Unknown`],
//! [`SystemMessage::Unknown`], or [`ControlRequest::Unknown`] instead of a
//! decode error.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;

/// Permission mode string sent on argv and `set_permission_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum PermissionMode {
    /// CLI `default` (help alias `manual`).
    #[serde(rename = "default", alias = "manual")]
    #[default]
    Default,
    /// CLI `acceptEdits`.
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    /// CLI `plan`.
    #[serde(rename = "plan")]
    Plan,
    /// CLI `bypassPermissions`.
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
    /// CLI `dontAsk`.
    #[serde(rename = "dontAsk")]
    DontAsk,
    /// CLI `auto`.
    #[serde(rename = "auto")]
    Auto,
    /// Unrecognised mode; do not send this on argv.
    #[serde(other)]
    Unknown,
}

impl PermissionMode {
    /// Flag value for `--permission-mode`.
    pub fn as_cli_str(self) -> Result<&'static str, crate::Error> {
        match self {
            Self::Default => Ok("default"),
            Self::AcceptEdits => Ok("acceptEdits"),
            Self::Plan => Ok("plan"),
            Self::BypassPermissions => Ok("bypassPermissions"),
            Self::DontAsk => Ok("dontAsk"),
            Self::Auto => Ok("auto"),
            Self::Unknown => Err(crate::Error::InvalidPermissionMode),
        }
    }
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
            Self::DontAsk => "dontAsk",
            Self::Auto => "auto",
            Self::Unknown => "unknown",
        };
        f.write_str(text)
    }
}

/// `type` discriminant of a [`PermissionUpdate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionUpdateType {
    /// `setMode`.
    SetMode,
    /// `addRules`.
    AddRules,
    /// `removeRules`.
    RemoveRules,
    /// `clearRules`.
    ClearRules,
    /// `addDirectories`.
    AddDirectories,
    /// Unrecognised update type.
    #[serde(other)]
    Unknown,
}

/// Where a permission update should be stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionUpdateDestination {
    /// Current session only.
    Session,
    /// User settings.
    UserSettings,
    /// Project settings.
    ProjectSettings,
    /// Local settings.
    LocalSettings,
    /// Unrecognised destination.
    #[serde(other)]
    Unknown,
}

/// One allow/deny rule inside a permission update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRuleValue {
    /// Tool the rule applies to.
    pub tool_name: String,
    /// Optional command/path matcher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_content: Option<String>,
}

/// Suggested or applied permission change (camelCase wire names).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionUpdate {
    /// Update discriminator (`setMode`, `addRules`, …).
    #[serde(rename = "type")]
    pub kind: PermissionUpdateType,
    /// Mode for `setMode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<PermissionMode>,
    /// Settings destination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<PermissionUpdateDestination>,
    /// Rules for add/remove.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<PermissionRuleValue>>,
    /// `allow` / `deny` when the suggestion is a rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behavior: Option<String>,
    /// Directories for `addDirectories`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directories: Option<Vec<String>>,
}

/// Host answer to `can_use_tool`. Allow **must** carry `updatedInput`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "behavior", rename_all = "camelCase")]
pub enum PermissionResult {
    /// Run the tool with this input.
    Allow {
        /// Echoed or edited tool input (required since CLI 2.1.207).
        #[serde(rename = "updatedInput")]
        updated_input: Value,
        /// Optional session permission patches.
        #[serde(
            default,
            rename = "updatedPermissions",
            skip_serializing_if = "Option::is_none"
        )]
        updated_permissions: Option<Vec<PermissionUpdate>>,
    },
    /// Reject the tool; `message` is what the model sees as `tool_result`.
    Deny {
        /// Denial text copied into the tool result.
        message: String,
        /// When `true`, also interrupt the current turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interrupt: Option<bool>,
    },
}

/// `blocked_path` / `blocked_paths` as seen on live frames (string or array).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BlockedPathList {
    /// Single path (vibe-kanban field type).
    One(String),
    /// Multiple paths.
    Many(Vec<String>),
}

impl BlockedPathList {
    /// Paths as a borrowed list.
    pub fn as_slice(&self) -> &[String] {
        match self {
            Self::One(path) => std::slice::from_ref(path),
            Self::Many(paths) => paths,
        }
    }
}

/// User `content`: plain text or Anthropic content blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    /// Single string body.
    Text(String),
    /// Text / image / tool_result blocks.
    Blocks(Vec<Value>),
}

/// Anthropic-style `message` object on a `user` frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessageBody {
    /// Always `"user"` on this frame.
    pub role: String,
    /// Text or content blocks.
    pub content: UserContent,
}

/// `user` frame used on both stdin and stdout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    /// Inner Anthropic message.
    pub message: UserMessageBody,
    /// Host-generated id; echoed on later assistant/result frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Nested agent parent, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_use_id: Option<String>,
    /// Session UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// `false` appends transcript without querying.
    #[serde(
        default,
        rename = "shouldQuery",
        skip_serializing_if = "Option::is_none"
    )]
    pub should_query: Option<bool>,
    /// Origin stamp (`human` is required for ultracode keywords).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Value>,
    /// `now` / `next` / `later`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// Tool result sidecar on stdout user frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_result: Option<Value>,
    /// Remaining fields (timestamp, tool_result_meta, …).
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

impl UserMessage {
    /// Build a minimal text user frame.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            message: UserMessageBody {
                role: "user".into(),
                content: UserContent::Text(content.into()),
            },
            uuid: None,
            parent_tool_use_id: None,
            session_id: None,
            should_query: None,
            origin: None,
            priority: None,
            tool_use_result: None,
            extra: Map::new(),
        }
    }

    /// Build a content-block user frame.
    pub fn blocks(blocks: Vec<Value>) -> Self {
        Self {
            message: UserMessageBody {
                role: "user".into(),
                content: UserContent::Blocks(blocks),
            },
            uuid: None,
            parent_tool_use_id: None,
            session_id: None,
            should_query: None,
            origin: None,
            priority: None,
            tool_use_result: None,
            extra: Map::new(),
        }
    }
}

/// `assistant` stdout frame. `message` is the Anthropic Messages object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// Anthropic `message` object (content blocks, usage, …).
    pub message: Value,
    /// Nested agent parent, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_use_id: Option<String>,
    /// Session UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Frame UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Host user-message UUID echo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_message_uuid: Option<String>,
    /// Remaining fields.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// `stream_event` partial token frame (`--include-partial-messages`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamEventMessage {
    /// Anthropic streaming event (`message_start`, `content_block_delta`, …).
    pub event: Value,
    /// Nested agent parent, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_use_id: Option<String>,
    /// Session UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Frame UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Remaining fields.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// One `result` frame. Streaming input emits **one per turn**, not per process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultMessage {
    /// `success` or `error_*`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    /// Model-facing result text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Whether this turn is an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    /// Stop reason (`end_turn`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Session UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Frame UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Turn count for this result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_turns: Option<u64>,
    /// Index among results in this process (Workflow emits two).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_index: Option<u64>,
    /// Remaining queued user turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_turn_count: Option<u64>,
    /// Cumulative USD cost; read the **latest** result, do not sum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    /// Token usage blob.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
    /// Per-model usage blob.
    #[serde(
        default,
        rename = "modelUsage",
        skip_serializing_if = "Option::is_none"
    )]
    pub model_usage: Option<Value>,
    /// Automatic denials (authoritative vs `system/permission_denied`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_denials: Option<Value>,
    /// Remaining fields.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// `rate_limit_event` stdout frame (seen in live probes; not in SDK 0.3.268).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitEvent {
    /// Rate-limit payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_info: Option<Value>,
    /// Session UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Frame UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Remaining fields.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// `system/init` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SystemInit {
    /// Process working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Session UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Tool names (includes `Workflow` on a full-native profile).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// MCP servers and status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Value>,
    /// Active model id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Permission mode reported by CLI (camelCase on the wire).
    #[serde(
        default,
        rename = "permissionMode",
        skip_serializing_if = "Option::is_none"
    )]
    pub permission_mode: Option<PermissionMode>,
    /// Slash commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slash_commands: Option<Vec<String>>,
    /// Terminal-only slash commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_slash_commands: Option<Vec<String>>,
    /// `none` means claude.ai OAuth, not “logged out”.
    #[serde(
        default,
        rename = "apiKeySource",
        skip_serializing_if = "Option::is_none"
    )]
    pub api_key_source: Option<String>,
    /// CLI version string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_code_version: Option<String>,
    /// Output style name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_style: Option<String>,
    /// Agent names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Value>,
    /// Skill names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Value>,
    /// Loaded plugins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<Value>,
    /// Feature-detect list (`interrupt_receipt_v1`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    /// Frame UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Remaining fields.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// `system/hook_started`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HookStarted {
    /// Hook instance id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_id: Option<String>,
    /// Hook display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_name: Option<String>,
    /// Hook event (`PreToolUse`, `SessionStart`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_event: Option<String>,
    /// Session UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Frame UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Remaining fields.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// `system/hook_progress`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HookProgress {
    /// Hook instance id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_id: Option<String>,
    /// Hook display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_name: Option<String>,
    /// Hook event name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_event: Option<String>,
    /// Captured stdout chunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    /// Captured stderr chunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    /// Session UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Frame UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Remaining fields.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// `system/hook_response`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HookResponse {
    /// Hook instance id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_id: Option<String>,
    /// Hook display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_name: Option<String>,
    /// Hook event name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_event: Option<String>,
    /// Combined output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Stdout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    /// Stderr.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    /// Process exit code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// `success` / `error` / `cancelled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Session UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Frame UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Remaining fields.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// `system/task_started`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TaskStarted {
    /// Task id (`w3upf3n8p`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Parent tool use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// Human description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `local_agent` / `local_workflow` / `local_bash`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_type: Option<String>,
    /// Subagent type when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
    /// Workflow name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_name: Option<String>,
    /// Whether the task is already backgrounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_backgrounded: Option<bool>,
    /// Nesting depth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_depth: Option<u64>,
    /// Remaining fields (includes `prompt` on workflows).
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// `system/task_progress`. `workflow_progress` is kept as an open object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TaskProgress {
    /// Task id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Parent tool use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// Progress description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Last tool name (CLI sometimes reuses this for labels).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool_name: Option<String>,
    /// Usage blob.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
    /// Open array; SDK 0.3.268 does not declare this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_progress: Option<Value>,
    /// Remaining fields.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// `system/task_updated`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TaskUpdated {
    /// Task id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Patch (`status`, `is_backgrounded`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<Value>,
    /// Remaining fields.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// `system/task_notification`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TaskNotification {
    /// Task id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Parent tool use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// `completed` / `failed` / `stopped`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Output file path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_file: Option<String>,
    /// Summary text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Remaining fields.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// `system/background_tasks_changed` — replace the local set, do not diff.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BackgroundTasksChanged {
    /// Full snapshot of background tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tasks: Option<Value>,
    /// Remaining fields.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// `system/api_retry`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ApiRetry {
    /// Attempt number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u64>,
    /// Max retries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u64>,
    /// Backoff milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_delay_ms: Option<u64>,
    /// Error name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    /// Remaining fields.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// `system/thinking_tokens`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ThinkingTokens {
    /// Estimated thinking tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_tokens: Option<u64>,
    /// Delta since the last estimate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_tokens_delta: Option<i64>,
    /// Remaining fields.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// `system/permission_denied` (best-effort; `result.permission_denials` wins).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PermissionDenied {
    /// Tool name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Tool use id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// Reason type (`mode`, `hook`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_reason_type: Option<String>,
    /// Human reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<String>,
    /// Message shown to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Remaining fields.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// `system/session_state_changed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SessionStateChanged {
    /// `idle` / `running` / `requires_action`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Remaining fields.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// Generic system payload used by less common subtypes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SystemGeneric {
    /// Session UUID when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Frame UUID when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Remaining fields.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// `type: system` stdout frames, keyed by `subtype`.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum SystemMessage {
    /// Session/tool table at the start of a turn.
    Init(SystemInit),
    /// Hook started (`--include-hook-events`).
    HookStarted(HookStarted),
    /// Hook stdout/stderr chunk.
    HookProgress(HookProgress),
    /// Hook finished.
    HookResponse(HookResponse),
    /// Background/workflow/subagent task started.
    TaskStarted(TaskStarted),
    /// Task progress (open `workflow_progress`).
    TaskProgress(TaskProgress),
    /// Task patch (`status`, …).
    TaskUpdated(TaskUpdated),
    /// Task finished notification.
    TaskNotification(TaskNotification),
    /// Full replacement snapshot of background tasks.
    BackgroundTasksChanged(BackgroundTasksChanged),
    /// API retry.
    ApiRetry(ApiRetry),
    /// Plugin install progress.
    PluginInstall(SystemGeneric),
    /// Compact / requesting status.
    Status(SystemGeneric),
    /// `idle` / `running` / `requires_action`.
    SessionStateChanged(SessionStateChanged),
    /// Automatic deny edge (not hook deny).
    PermissionDenied(PermissionDenied),
    /// Informational banner.
    Informational(SystemGeneric),
    /// Local slash-command output.
    LocalCommandOutput(SystemGeneric),
    /// Thinking-token estimate.
    ThinkingTokens(ThinkingTokens),
    /// Long control-request progress.
    ControlRequestProgress(SystemGeneric),
    /// MCP elicitation completed.
    ElicitationComplete(SystemGeneric),
    /// Unknown `subtype` (never a decode error).
    Unknown(Value),
}

impl SystemMessage {
    /// Decode a JSON object already known to have `type=system`.
    pub fn from_value(value: Value) -> Self {
        let subtype = value
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match subtype {
            "init" => parse_or_unknown(value, Self::Init, Self::Unknown),
            "hook_started" => parse_or_unknown(value, Self::HookStarted, Self::Unknown),
            "hook_progress" => parse_or_unknown(value, Self::HookProgress, Self::Unknown),
            "hook_response" => parse_or_unknown(value, Self::HookResponse, Self::Unknown),
            "task_started" => parse_or_unknown(value, Self::TaskStarted, Self::Unknown),
            "task_progress" => parse_or_unknown(value, Self::TaskProgress, Self::Unknown),
            "task_updated" => parse_or_unknown(value, Self::TaskUpdated, Self::Unknown),
            "task_notification" => parse_or_unknown(value, Self::TaskNotification, Self::Unknown),
            "background_tasks_changed" => {
                parse_or_unknown(value, Self::BackgroundTasksChanged, Self::Unknown)
            }
            "api_retry" => parse_or_unknown(value, Self::ApiRetry, Self::Unknown),
            "plugin_install" => parse_or_unknown(value, Self::PluginInstall, Self::Unknown),
            "status" => parse_or_unknown(value, Self::Status, Self::Unknown),
            "session_state_changed" => {
                parse_or_unknown(value, Self::SessionStateChanged, Self::Unknown)
            }
            "permission_denied" => parse_or_unknown(value, Self::PermissionDenied, Self::Unknown),
            "informational" => parse_or_unknown(value, Self::Informational, Self::Unknown),
            "local_command_output" => {
                parse_or_unknown(value, Self::LocalCommandOutput, Self::Unknown)
            }
            "thinking_tokens" => parse_or_unknown(value, Self::ThinkingTokens, Self::Unknown),
            "control_request_progress" => {
                parse_or_unknown(value, Self::ControlRequestProgress, Self::Unknown)
            }
            "elicitation_complete" => {
                parse_or_unknown(value, Self::ElicitationComplete, Self::Unknown)
            }
            _ => Self::Unknown(value),
        }
    }

    /// Wire `subtype` string.
    pub fn subtype_name(&self) -> &str {
        match self {
            Self::Init(_) => "init",
            Self::HookStarted(_) => "hook_started",
            Self::HookProgress(_) => "hook_progress",
            Self::HookResponse(_) => "hook_response",
            Self::TaskStarted(_) => "task_started",
            Self::TaskProgress(_) => "task_progress",
            Self::TaskUpdated(_) => "task_updated",
            Self::TaskNotification(_) => "task_notification",
            Self::BackgroundTasksChanged(_) => "background_tasks_changed",
            Self::ApiRetry(_) => "api_retry",
            Self::PluginInstall(_) => "plugin_install",
            Self::Status(_) => "status",
            Self::SessionStateChanged(_) => "session_state_changed",
            Self::PermissionDenied(_) => "permission_denied",
            Self::Informational(_) => "informational",
            Self::LocalCommandOutput(_) => "local_command_output",
            Self::ThinkingTokens(_) => "thinking_tokens",
            Self::ControlRequestProgress(_) => "control_request_progress",
            Self::ElicitationComplete(_) => "elicitation_complete",
            Self::Unknown(value) => value
                .get("subtype")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
        }
    }
}

impl Serialize for SystemMessage {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Init(body) => serialize_tagged(serializer, "subtype", "init", body),
            Self::HookStarted(body) => {
                serialize_tagged(serializer, "subtype", "hook_started", body)
            }
            Self::HookProgress(body) => {
                serialize_tagged(serializer, "subtype", "hook_progress", body)
            }
            Self::HookResponse(body) => {
                serialize_tagged(serializer, "subtype", "hook_response", body)
            }
            Self::TaskStarted(body) => {
                serialize_tagged(serializer, "subtype", "task_started", body)
            }
            Self::TaskProgress(body) => {
                serialize_tagged(serializer, "subtype", "task_progress", body)
            }
            Self::TaskUpdated(body) => {
                serialize_tagged(serializer, "subtype", "task_updated", body)
            }
            Self::TaskNotification(body) => {
                serialize_tagged(serializer, "subtype", "task_notification", body)
            }
            Self::BackgroundTasksChanged(body) => {
                serialize_tagged(serializer, "subtype", "background_tasks_changed", body)
            }
            Self::ApiRetry(body) => serialize_tagged(serializer, "subtype", "api_retry", body),
            Self::PluginInstall(body) => {
                serialize_tagged(serializer, "subtype", "plugin_install", body)
            }
            Self::Status(body) => serialize_tagged(serializer, "subtype", "status", body),
            Self::SessionStateChanged(body) => {
                serialize_tagged(serializer, "subtype", "session_state_changed", body)
            }
            Self::PermissionDenied(body) => {
                serialize_tagged(serializer, "subtype", "permission_denied", body)
            }
            Self::Informational(body) => {
                serialize_tagged(serializer, "subtype", "informational", body)
            }
            Self::LocalCommandOutput(body) => {
                serialize_tagged(serializer, "subtype", "local_command_output", body)
            }
            Self::ThinkingTokens(body) => {
                serialize_tagged(serializer, "subtype", "thinking_tokens", body)
            }
            Self::ControlRequestProgress(body) => {
                serialize_tagged(serializer, "subtype", "control_request_progress", body)
            }
            Self::ElicitationComplete(body) => {
                serialize_tagged(serializer, "subtype", "elicitation_complete", body)
            }
            Self::Unknown(value) => value.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for SystemMessage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_value(Value::deserialize(deserializer)?))
    }
}

/// C→H `can_use_tool` request. Tolerates both `blocked_path` and `blocked_paths`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CanUseToolRequest {
    /// Tool name (`Bash`, `AskUserQuestion`, …).
    #[serde(default)]
    pub tool_name: String,
    /// Tool input object.
    #[serde(default)]
    pub input: Value,
    /// Suggested permission updates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_suggestions: Option<Vec<PermissionUpdate>>,
    /// SDK field (singular).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_path: Option<String>,
    /// vibe-kanban field (plural); string or array.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_paths: Option<BlockedPathList>,
    /// Tool-use id (SDK required; older notes called it undocumented).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// Dialog title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Short description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Why the prompt fired (`mode`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_reason_type: Option<String>,
    /// True for AskUserQuestion / interactive MCP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_user_interaction: Option<bool>,
    /// Remaining fields.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// C→H `hook_callback`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HookCallbackRequest {
    /// Id registered in `initialize.hooks.*.hookCallbackIds`.
    #[serde(default, alias = "callbackId")]
    pub callback_id: String,
    /// Hook event JSON (same schema as command-hook stdin).
    #[serde(default)]
    pub input: Value,
    /// Related tool use, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// Remaining fields.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// Bidirectional `mcp_message`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct McpMessageRequest {
    /// In-process MCP server name.
    #[serde(default)]
    pub server_name: String,
    /// JSON-RPC payload.
    #[serde(default)]
    pub message: Value,
    /// Remaining fields.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// One SDK hook matcher (`hookCallbackIds` is camelCase).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HookCallbackMatcher {
    /// Tool-name regex; omitted = match all for that event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    /// Opaque ids the host maps back to callbacks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_callback_ids: Option<Vec<String>>,
    /// Timeout seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<f64>,
}

/// H→C `initialize` body. Field names follow the SDK (camelCase).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequest {
    /// SDK callback hooks (appended after user settings hooks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<Value>,
    /// Custom agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Value>,
    /// System prompt fragments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<Value>,
    /// Appended system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append_system_prompt: Option<String>,
    /// Freeze the rendered system prompt (SDK default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_snapshot: Option<bool>,
    /// In-process MCP servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdk_mcp_servers: Option<Value>,
    /// In-process MCP server configs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdk_mcp_server_configs: Option<Value>,
    /// In-process MCP manifests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdk_mcp_server_manifests: Option<Value>,
    /// Structured output schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_schema: Option<Value>,
    /// Skill allow-list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Value>,
    /// Plugins (only with `--await-initialize`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<Value>,
    /// Dialog kinds the host can render; omit = fail closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_dialog_kinds: Option<Vec<String>>,
    /// Host can `stop_task` without killing background workflows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_task_stop_affordance: Option<bool>,
    /// Override `--forward-subagent-text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward_subagent_text: Option<bool>,
    /// Agent progress summaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_progress_summaries: Option<Value>,
    /// Prompt suggestions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_suggestions: Option<Value>,
    /// Plan-mode instructions replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_mode_instructions: Option<String>,
    /// Single-hop tool aliases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_aliases: Option<Value>,
    /// Session title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Control-request body. Direction is implied by which side sent the envelope.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum ControlRequest {
    /// C→H: tool permission / AskUserQuestion.
    CanUseTool(CanUseToolRequest),
    /// C→H: SDK hook callback.
    HookCallback(HookCallbackRequest),
    /// Either direction: in-process MCP JSON-RPC.
    McpMessage(McpMessageRequest),
    /// H→C: control handshake.
    Initialize(InitializeRequest),
    /// H→C: interrupt the current turn.
    Interrupt {
        /// Also drop queued user messages (`interrupt_cancel_queued_v1`).
        cancel_queued: Option<bool>,
    },
    /// H→C: change permission mode.
    SetPermissionMode {
        /// Target mode.
        mode: PermissionMode,
    },
    /// H→C: change model; `None` / `"default"` restores the session default.
    SetModel {
        /// Model id, or `None` to restore default.
        model: Option<String>,
    },
    /// Unknown `subtype` (never a decode error).
    Unknown(Value),
}

impl ControlRequest {
    /// Decode the inner `request` object.
    pub fn from_value(value: Value) -> Self {
        let subtype = value
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match subtype {
            "can_use_tool" => parse_or_unknown(value, Self::CanUseTool, Self::Unknown),
            "hook_callback" => parse_or_unknown(value, Self::HookCallback, Self::Unknown),
            "mcp_message" => parse_or_unknown(value, Self::McpMessage, Self::Unknown),
            "initialize" => parse_or_unknown(value, Self::Initialize, Self::Unknown),
            "interrupt" => {
                let cancel_queued = value
                    .get("cancel_queued")
                    .and_then(Value::as_bool)
                    .or_else(|| value.get("cancelQueued").and_then(Value::as_bool));
                Self::Interrupt { cancel_queued }
            }
            "set_permission_mode" => match value.get("mode") {
                Some(mode) => match serde_json::from_value::<PermissionMode>(mode.clone()) {
                    Ok(mode) => Self::SetPermissionMode { mode },
                    Err(_) => Self::Unknown(value),
                },
                None => Self::Unknown(value),
            },
            "set_model" => {
                let model = match value.get("model") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(_) => return Self::Unknown(value),
                };
                Self::SetModel { model }
            }
            _ => Self::Unknown(value),
        }
    }

    /// Wire `subtype` string.
    pub fn subtype_name(&self) -> &str {
        match self {
            Self::CanUseTool(_) => "can_use_tool",
            Self::HookCallback(_) => "hook_callback",
            Self::McpMessage(_) => "mcp_message",
            Self::Initialize(_) => "initialize",
            Self::Interrupt { .. } => "interrupt",
            Self::SetPermissionMode { .. } => "set_permission_mode",
            Self::SetModel { .. } => "set_model",
            Self::Unknown(value) => value
                .get("subtype")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
        }
    }
}

impl Serialize for ControlRequest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::CanUseTool(body) => serialize_tagged(serializer, "subtype", "can_use_tool", body),
            Self::HookCallback(body) => {
                serialize_tagged(serializer, "subtype", "hook_callback", body)
            }
            Self::McpMessage(body) => serialize_tagged(serializer, "subtype", "mcp_message", body),
            Self::Initialize(body) => serialize_tagged(serializer, "subtype", "initialize", body),
            Self::Interrupt { cancel_queued } => {
                let mut map = Map::new();
                map.insert("subtype".into(), Value::String("interrupt".into()));
                if let Some(flag) = cancel_queued {
                    map.insert("cancel_queued".into(), Value::Bool(*flag));
                }
                Value::Object(map).serialize(serializer)
            }
            Self::SetPermissionMode { mode } => serde_json::json!({
                "subtype": "set_permission_mode",
                "mode": mode,
            })
            .serialize(serializer),
            Self::SetModel { model } => serde_json::json!({
                "subtype": "set_model",
                "model": model,
            })
            .serialize(serializer),
            Self::Unknown(value) => value.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ControlRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_value(Value::deserialize(deserializer)?))
    }
}

/// `control_response` inner object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum ControlResponse {
    /// Matched `request_id` succeeded.
    Success {
        /// Echo of the request id.
        request_id: String,
        /// Subtype-specific payload (permission result, initialize catalog, …).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response: Option<Value>,
        /// Inflight permission prompts (v2.1.268+ initialize).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pending_permission_requests: Option<Value>,
        /// Inflight user dialogs (v2.1.268+ initialize).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pending_user_dialog_requests: Option<Value>,
    },
    /// Matched `request_id` failed.
    Error {
        /// Echo of the request id.
        request_id: String,
        /// Human-readable error.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

impl ControlResponse {
    /// Echoed request id.
    pub fn request_id(&self) -> &str {
        match self {
            Self::Success { request_id, .. } | Self::Error { request_id, .. } => request_id,
        }
    }

    /// Parse a `PermissionResult` out of a success payload, if present.
    pub fn permission_result(&self) -> Option<PermissionResult> {
        match self {
            Self::Success {
                response: Some(value),
                ..
            } => serde_json::from_value(value.clone()).ok(),
            _ => None,
        }
    }
}

/// Envelope around a control request (both directions).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlRequestEnvelope {
    /// Sender-generated id; the peer must echo it.
    pub request_id: String,
    /// Request body.
    pub request: ControlRequest,
}

/// Envelope around a control response (both directions).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlResponseEnvelope {
    /// Success or error body.
    pub response: ControlResponse,
}

/// Payload of a successful `control_response` the host sends.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ControlSuccessPayload {
    /// Answer to `can_use_tool`.
    Permission(PermissionResult),
    /// Arbitrary JSON (hook output, MCP, initialize, …).
    Json(Value),
}

/// Stdout (C→H) frame.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Outbound {
    /// `type=system`.
    System(SystemMessage),
    /// `type=assistant`.
    Assistant(AssistantMessage),
    /// `type=user` (tool_result or `--replay-user-messages` echo).
    User(UserMessage),
    /// `type=result` (one per turn; Workflow emits more than one).
    Result(ResultMessage),
    /// `type=stream_event`.
    StreamEvent(StreamEventMessage),
    /// `type=control_request`.
    ControlRequest(ControlRequestEnvelope),
    /// `type=control_response`.
    ControlResponse(ControlResponseEnvelope),
    /// `type=control_cancel_request`.
    ControlCancelRequest {
        /// Request id to cancel.
        request_id: String,
    },
    /// `type=keep_alive` (ignore).
    KeepAlive,
    /// `type=rate_limit_event`.
    RateLimitEvent(RateLimitEvent),
    /// `type=tool_progress`.
    ToolProgress(Value),
    /// `type=prompt_suggestion`.
    PromptSuggestion(Value),
    /// Unknown `type` (never a decode error).
    Unknown(Value),
}

impl Outbound {
    /// Decode one JSON value. Invalid objects become [`Self::Unknown`].
    pub fn from_value(value: Value) -> Self {
        let Some(ty) = value.get("type").and_then(Value::as_str) else {
            return Self::Unknown(value);
        };
        match ty {
            "system" => Self::System(SystemMessage::from_value(value)),
            "assistant" => parse_or_unknown(value, Self::Assistant, Self::Unknown),
            "user" => parse_or_unknown(value, Self::User, Self::Unknown),
            "result" => parse_or_unknown(value, Self::Result, Self::Unknown),
            "stream_event" => parse_or_unknown(value, Self::StreamEvent, Self::Unknown),
            "control_request" => {
                let request_id = value
                    .get("request_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let request = value
                    .get("request")
                    .cloned()
                    .map(ControlRequest::from_value)
                    .unwrap_or_else(|| ControlRequest::Unknown(Value::Null));
                Self::ControlRequest(ControlRequestEnvelope {
                    request_id,
                    request,
                })
            }
            "control_response" => match value.get("response").cloned() {
                Some(inner) => match serde_json::from_value::<ControlResponse>(inner) {
                    Ok(response) => Self::ControlResponse(ControlResponseEnvelope { response }),
                    Err(_) => Self::Unknown(value),
                },
                None => Self::Unknown(value),
            },
            "control_cancel_request" => {
                let request_id = value
                    .get("request_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                Self::ControlCancelRequest { request_id }
            }
            "keep_alive" => Self::KeepAlive,
            "rate_limit_event" => parse_or_unknown(value, Self::RateLimitEvent, Self::Unknown),
            "tool_progress" => Self::ToolProgress(value),
            "prompt_suggestion" => Self::PromptSuggestion(value),
            _ => Self::Unknown(value),
        }
    }

    /// Wire `type` string.
    pub fn type_name(&self) -> &str {
        match self {
            Self::System(_) => "system",
            Self::Assistant(_) => "assistant",
            Self::User(_) => "user",
            Self::Result(_) => "result",
            Self::StreamEvent(_) => "stream_event",
            Self::ControlRequest(_) => "control_request",
            Self::ControlResponse(_) => "control_response",
            Self::ControlCancelRequest { .. } => "control_cancel_request",
            Self::KeepAlive => "keep_alive",
            Self::RateLimitEvent(_) => "rate_limit_event",
            Self::ToolProgress(_) => "tool_progress",
            Self::PromptSuggestion(_) => "prompt_suggestion",
            Self::Unknown(value) => value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
        }
    }

    /// True when the top-level `type` was not recognised.
    pub fn is_unknown_type(&self) -> bool {
        matches!(self, Self::Unknown(_))
    }

    /// True when a known `type` carried an unknown `subtype`.
    pub fn is_unknown_subtype(&self) -> bool {
        match self {
            Self::System(SystemMessage::Unknown(_)) => true,
            Self::ControlRequest(env) => matches!(env.request, ControlRequest::Unknown(_)),
            _ => false,
        }
    }

    /// Extract C→H `can_use_tool`, if this frame is one.
    pub fn as_can_use_tool(&self) -> Option<(&str, &CanUseToolRequest)> {
        match self {
            Self::ControlRequest(ControlRequestEnvelope {
                request_id,
                request: ControlRequest::CanUseTool(req),
            }) => Some((request_id.as_str(), req)),
            _ => None,
        }
    }
}

impl Serialize for Outbound {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::System(msg) => serialize_tagged(serializer, "type", "system", msg),
            Self::Assistant(msg) => serialize_tagged(serializer, "type", "assistant", msg),
            Self::User(msg) => serialize_tagged(serializer, "type", "user", msg),
            Self::Result(msg) => serialize_tagged(serializer, "type", "result", msg),
            Self::StreamEvent(msg) => serialize_tagged(serializer, "type", "stream_event", msg),
            Self::ControlRequest(env) => {
                serialize_tagged(serializer, "type", "control_request", env)
            }
            Self::ControlResponse(env) => {
                serialize_tagged(serializer, "type", "control_response", env)
            }
            Self::ControlCancelRequest { request_id } => serde_json::json!({
                "type": "control_cancel_request",
                "request_id": request_id,
            })
            .serialize(serializer),
            Self::KeepAlive => serde_json::json!({"type": "keep_alive"}).serialize(serializer),
            Self::RateLimitEvent(msg) => {
                serialize_tagged(serializer, "type", "rate_limit_event", msg)
            }
            Self::ToolProgress(value) | Self::PromptSuggestion(value) | Self::Unknown(value) => {
                value.serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for Outbound {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_value(Value::deserialize(deserializer)?))
    }
}

/// Stdin (H→C) frame.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Inbound {
    /// User turn.
    User(UserMessage),
    /// Host → CLI control request.
    ControlRequest(ControlRequestEnvelope),
    /// Host → CLI control response (permission, hook, …).
    ControlResponse(ControlResponseEnvelope),
    /// Cancel an in-flight control request.
    ControlCancelRequest {
        /// Request id to cancel.
        request_id: String,
    },
    /// Keep-alive (optional).
    KeepAlive,
    /// Unknown `type` (never a decode error).
    Unknown(Value),
}

impl Inbound {
    /// Decode one JSON value. Invalid objects become [`Self::Unknown`].
    pub fn from_value(value: Value) -> Self {
        let Some(ty) = value.get("type").and_then(Value::as_str) else {
            return Self::Unknown(value);
        };
        match ty {
            "user" => parse_or_unknown(value, Self::User, Self::Unknown),
            "control_request" => {
                let request_id = value
                    .get("request_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let request = value
                    .get("request")
                    .cloned()
                    .map(ControlRequest::from_value)
                    .unwrap_or_else(|| ControlRequest::Unknown(Value::Null));
                Self::ControlRequest(ControlRequestEnvelope {
                    request_id,
                    request,
                })
            }
            "control_response" => match value.get("response").cloned() {
                Some(inner) => match serde_json::from_value::<ControlResponse>(inner) {
                    Ok(response) => Self::ControlResponse(ControlResponseEnvelope { response }),
                    Err(_) => Self::Unknown(value),
                },
                None => Self::Unknown(value),
            },
            "control_cancel_request" => {
                let request_id = value
                    .get("request_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                Self::ControlCancelRequest { request_id }
            }
            "keep_alive" => Self::KeepAlive,
            _ => Self::Unknown(value),
        }
    }

    /// Minimal text user frame.
    pub fn user_text(content: impl Into<String>) -> Self {
        Self::User(UserMessage::text(content))
    }

    /// Host `initialize` request with a fresh `request_id`.
    pub fn initialize(body: InitializeRequest) -> (String, Self) {
        let request_id = uuid::Uuid::new_v4().to_string();
        (
            request_id.clone(),
            Self::ControlRequest(ControlRequestEnvelope {
                request_id,
                request: ControlRequest::Initialize(body),
            }),
        )
    }

    /// Permission / hook / generic success response echoing `request_id`.
    pub fn control_success(request_id: impl Into<String>, payload: ControlSuccessPayload) -> Self {
        let response = match payload {
            ControlSuccessPayload::Permission(result) => serde_json::to_value(result).ok(),
            ControlSuccessPayload::Json(value) => Some(value),
        };
        Self::ControlResponse(ControlResponseEnvelope {
            response: ControlResponse::Success {
                request_id: request_id.into(),
                response,
                pending_permission_requests: None,
                pending_user_dialog_requests: None,
            },
        })
    }

    /// Control error response echoing `request_id`.
    pub fn control_error(request_id: impl Into<String>, error: impl Into<String>) -> Self {
        Self::ControlResponse(ControlResponseEnvelope {
            response: ControlResponse::Error {
                request_id: request_id.into(),
                error: Some(error.into()),
            },
        })
    }
}

impl Serialize for Inbound {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::User(msg) => serialize_tagged(serializer, "type", "user", msg),
            Self::ControlRequest(env) => {
                serialize_tagged(serializer, "type", "control_request", env)
            }
            Self::ControlResponse(env) => {
                serialize_tagged(serializer, "type", "control_response", env)
            }
            Self::ControlCancelRequest { request_id } => serde_json::json!({
                "type": "control_cancel_request",
                "request_id": request_id,
            })
            .serialize(serializer),
            Self::KeepAlive => serde_json::json!({"type": "keep_alive"}).serialize(serializer),
            Self::Unknown(value) => value.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Inbound {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_value(Value::deserialize(deserializer)?))
    }
}

fn parse_or_unknown<T, U, F, G>(value: Value, ok: F, unknown: G) -> U
where
    T: for<'de> Deserialize<'de>,
    F: FnOnce(T) -> U,
    G: FnOnce(Value) -> U,
{
    match serde_json::from_value::<T>(value.clone()) {
        Ok(parsed) => ok(parsed),
        Err(_) => unknown(value),
    }
}

fn serialize_tagged<S: serde::Serializer, T: Serialize>(
    serializer: S,
    key: &str,
    tag: &str,
    body: &T,
) -> Result<S::Ok, S::Error> {
    merge_tag(key, tag, body)
        .map_err(serde::ser::Error::custom)?
        .serialize(serializer)
}

fn merge_tag<T: Serialize>(key: &str, tag: &str, body: &T) -> serde_json::Result<Value> {
    let mut value = serde_json::to_value(body)?;
    match &mut value {
        Value::Object(map) => {
            map.insert(key.to_owned(), Value::String(tag.to_owned()));
        }
        other => {
            let mut map = Map::new();
            map.insert(key.to_owned(), Value::String(tag.to_owned()));
            map.insert("value".into(), other.take());
            value = Value::Object(map);
        }
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_type_is_not_an_error() {
        let msg: Outbound =
            serde_json::from_str(r#"{"type":"future_frame","x":1}"#).expect("decode");
        assert!(msg.is_unknown_type());
    }

    #[test]
    fn unknown_system_subtype_is_not_an_error() {
        let msg: Outbound =
            serde_json::from_str(r#"{"type":"system","subtype":"brand_new","z":true}"#)
                .expect("decode");
        assert!(msg.is_unknown_subtype());
        assert!(!msg.is_unknown_type());
    }

    #[test]
    fn permission_allow_is_camel_case() {
        let result = PermissionResult::Allow {
            updated_input: serde_json::json!({"command": "git status"}),
            updated_permissions: None,
        };
        let value = serde_json::to_value(result).expect("ser");
        assert_eq!(value["behavior"], "allow");
        assert!(value.get("updatedInput").is_some());
        assert!(value.get("updated_input").is_none());
    }

    #[test]
    fn blocked_path_and_blocked_paths_both_parse() {
        let one: CanUseToolRequest =
            serde_json::from_str(r#"{"tool_name":"Bash","input":{},"blocked_path":"/tmp/a"}"#)
                .expect("one");
        assert_eq!(one.blocked_path.as_deref(), Some("/tmp/a"));
        let many: CanUseToolRequest =
            serde_json::from_str(r#"{"tool_name":"Bash","input":{},"blocked_paths":["/tmp/a"]}"#)
                .expect("many");
        assert_eq!(
            many.blocked_paths.as_ref().map(BlockedPathList::as_slice),
            Some(["/tmp/a".to_owned()].as_slice())
        );
        let str_many: CanUseToolRequest =
            serde_json::from_str(r#"{"tool_name":"Bash","input":{},"blocked_paths":"/tmp/a"}"#)
                .expect("str");
        assert!(matches!(
            str_many.blocked_paths,
            Some(BlockedPathList::One(_))
        ));
    }
}
