//! Hand-written Codex app-server types for the D-013 frozen surface.
//!
//! Covered RPCs: `initialize`, `thread/start`, `turn/start`, `turn/interrupt`.
//! `thread/resume`, `model/list`, and unix/ws listens are out of scope.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// `clientInfo` sent on `initialize`. `name` is copied into compliance logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    /// Stable product name. Use `remuda`, not a per-probe name.
    pub name: String,
    /// Optional display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Client version string.
    pub version: String,
}

impl Default for ClientInfo {
    fn default() -> Self {
        Self {
            name: "remuda".into(),
            title: Some("Remuda".into()),
            version: env!("CARGO_PKG_VERSION").into(),
        }
    }
}

/// Client capabilities declared during `initialize`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InitializeCapabilities {
    /// Opt into experimental methods/fields. Remuda leaves this false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub experimental_api: bool,
    /// Opt into `attestation/generate`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub request_attestation: bool,
    /// Exact notification method names to suppress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opt_out_notification_methods: Option<Vec<String>>,
    /// MCP extension map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<HashMap<String, Value>>,
}

/// `initialize` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// Client identity.
    pub client_info: ClientInfo,
    /// Optional capabilities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<InitializeCapabilities>,
}

/// `initialize` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    /// Server user-agent. The prefix may be the first client on a shared process.
    pub user_agent: String,
    /// Absolute `$CODEX_HOME` the server is using.
    pub codex_home: String,
    /// Platform family, for example `unix`.
    pub platform_family: String,
    /// OS id, for example `macos`.
    pub platform_os: String,
}

/// Sandbox mode **on requests** (kebab-case). Responses use [`SandboxPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    /// Read-only sandbox.
    ReadOnly,
    /// Workspace write sandbox.
    WorkspaceWrite,
    /// Unrestricted sandbox.
    DangerFullAccess,
}

/// Named approval policy. Granular objects are left as [`AskForApproval::Granular`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AskForApprovalMode {
    /// Read-only commands auto-approve; others ask.
    Untrusted,
    /// The model decides when to ask.
    OnRequest,
    /// Never ask.
    Never,
}

/// `approvalPolicy` on thread/turn requests and responses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AskForApproval {
    /// One of `untrusted` / `on-request` / `never`.
    Named(AskForApprovalMode),
    /// `{granular: {...}}`.
    Granular(Value),
}

/// Personality preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Personality {
    /// No personality overlay.
    None,
    /// Friendly.
    Friendly,
    /// Pragmatic.
    Pragmatic,
}

/// Reasoning summary preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReasoningSummary {
    /// Provider default.
    Auto,
    /// Short.
    Concise,
    /// Long.
    Detailed,
    /// Disabled.
    None,
}

/// `thread/start` params. All fields are optional on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartParams {
    /// Model id, for example `gpt-5.6-sol`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Working directory. Prefer an absolute path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Approval policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<AskForApproval>,
    /// Request sandbox as kebab-case. Mutually exclusive with `permissions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxMode>,
    /// Named permission profile, for example `:workspace`. Mutually exclusive with `sandbox`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<String>,
    /// Personality.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personality: Option<Personality>,
    /// Originator / service name copied into thread metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_name: Option<String>,
    /// When true the thread is not persisted and cannot be resumed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral: Option<bool>,
    /// Extra config overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<HashMap<String, Value>>,
    /// Model provider id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
}

/// Sandbox policy **on responses** (camelCase tagged object).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SandboxPolicy {
    /// Unrestricted.
    DangerFullAccess,
    /// Read-only.
    ReadOnly {
        /// Whether network is enabled.
        #[serde(default)]
        network_access: bool,
    },
    /// Workspace write. Probe responses used this for `sandbox: "workspace-write"`.
    WorkspaceWrite {
        /// Extra writable roots.
        #[serde(default)]
        writable_roots: Vec<String>,
        /// Network flag.
        #[serde(default)]
        network_access: bool,
        /// Exclude `$TMPDIR`.
        #[serde(default)]
        exclude_tmpdir_env_var: bool,
        /// Exclude `/tmp`.
        #[serde(default)]
        exclude_slash_tmp: bool,
    },
    /// Catch-all for newer policy types.
    #[serde(other)]
    Unknown,
}

/// Active permission profile in a start/resume response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivePermissionProfile {
    /// Profile id such as `:workspace`.
    pub id: String,
    /// Parent profile id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
}

/// Approvals reviewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalsReviewer {
    /// Human reviews approvals.
    User,
    /// Auto-review subagent.
    AutoReview,
    /// Legacy auto-review name.
    GuardianSubagent,
}

/// `thread/start` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartResponse {
    /// Created thread.
    pub thread: Thread,
    /// Resolved model id.
    #[serde(default)]
    pub model: Option<String>,
    /// Resolved provider.
    #[serde(default)]
    pub model_provider: Option<String>,
    /// Service tier.
    #[serde(default)]
    pub service_tier: Option<String>,
    /// Resolved cwd.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Runtime workspace roots.
    #[serde(default)]
    pub runtime_workspace_roots: Vec<String>,
    /// Instruction source paths.
    #[serde(default)]
    pub instruction_sources: Vec<String>,
    /// Effective approval policy.
    #[serde(default)]
    pub approval_policy: Option<AskForApproval>,
    /// Reviewer.
    #[serde(default)]
    pub approvals_reviewer: Option<ApprovalsReviewer>,
    /// Response sandbox object (not the kebab-case request enum).
    #[serde(default)]
    pub sandbox: Option<SandboxPolicy>,
    /// Active profile.
    #[serde(default)]
    pub active_permission_profile: Option<ActivePermissionProfile>,
    /// Resolved effort.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

/// Thread runtime status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ThreadStatus {
    /// Not loaded in this app-server process.
    NotLoaded,
    /// Loaded and idle.
    Idle,
    /// Native system error.
    SystemError,
    /// A turn is running.
    Active {
        /// Extra busy flags.
        #[serde(default)]
        active_flags: Vec<String>,
    },
}

/// One environment on a loaded thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadEnvironment {
    /// Environment id, for example `local`.
    #[serde(default)]
    pub environment_id: Option<String>,
    /// Environment cwd.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Runtime workspace roots.
    #[serde(default)]
    pub runtime_workspace_roots: Option<Vec<String>>,
}

/// Codex thread object. Extra native fields are ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    /// Thread id (UUIDv7 in 0.154.0).
    pub id: String,
    /// Environments; `null` when the thread is not loaded.
    #[serde(default)]
    pub environments: Option<Vec<ThreadEnvironment>>,
    /// Session id; often equal to `id`.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Fork parent.
    #[serde(default)]
    pub forked_from_id: Option<String>,
    /// Sub-agent parent.
    #[serde(default)]
    pub parent_thread_id: Option<String>,
    /// Preview / first user text.
    #[serde(default)]
    pub preview: String,
    /// Ephemeral threads cannot be resumed after unload.
    #[serde(default)]
    pub ephemeral: bool,
    /// History contract: `paginated` or `legacy`.
    #[serde(default)]
    pub history_mode: Option<String>,
    /// Provider id.
    #[serde(default)]
    pub model_provider: Option<String>,
    /// Model id.
    #[serde(default)]
    pub model: Option<String>,
    /// Effort.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Created-at unix seconds.
    #[serde(default)]
    pub created_at: Option<i64>,
    /// Updated-at unix seconds.
    #[serde(default)]
    pub updated_at: Option<i64>,
    /// Recency unix seconds.
    #[serde(default)]
    pub recency_at: Option<i64>,
    /// Runtime status.
    #[serde(default)]
    pub status: Option<ThreadStatus>,
    /// Rollout path. Persist this with `id` for resume.
    #[serde(default)]
    pub path: Option<String>,
    /// Captured cwd.
    #[serde(default)]
    pub cwd: Option<String>,
    /// CLI version that created the thread.
    #[serde(default)]
    pub cli_version: Option<String>,
    /// `clientInfo.name` originator.
    #[serde(default)]
    pub originator: Option<String>,
    /// Session source. App-server defaults to `vscode`.
    #[serde(default)]
    pub source: Option<String>,
    /// Whether this connection can `turn/start` on the loaded thread.
    #[serde(default)]
    pub can_accept_direct_input: Option<bool>,
    /// Display name.
    #[serde(default)]
    pub name: Option<String>,
    /// Turns. Empty unless `includeTurns` / resume hydration.
    #[serde(default)]
    pub turns: Vec<Turn>,
}

/// Turn lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    /// Still running.
    InProgress,
    /// Finished successfully.
    Completed,
    /// Interrupted by `turn/interrupt` or cancel decision.
    Interrupted,
    /// Failed.
    Failed,
}

/// How much of `Turn.items` is populated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum TurnItemsView {
    /// Items were not loaded.
    NotLoaded,
    /// Display summary only.
    Summary,
    /// Full item list.
    #[default]
    Full,
}

/// One turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    /// Turn id.
    pub id: String,
    /// Items according to [`Turn::items_view`].
    #[serde(default)]
    pub items: Vec<ThreadItem>,
    /// Item hydration mode.
    #[serde(default)]
    pub items_view: TurnItemsView,
    /// Lifecycle.
    pub status: TurnStatus,
    /// Failure payload.
    #[serde(default)]
    pub error: Option<Value>,
    /// Start unix seconds.
    #[serde(default)]
    pub started_at: Option<i64>,
    /// Completion unix seconds.
    #[serde(default)]
    pub completed_at: Option<i64>,
    /// Duration in milliseconds.
    #[serde(default)]
    pub duration_ms: Option<i64>,
}

/// User input item. Unknown variants become [`ThreadItem::Unknown`] with the parent item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserInput {
    /// Text prompt.
    Text {
        /// Prompt text.
        text: String,
        /// UI spans; ignored by Remuda.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        text_elements: Vec<Value>,
    },
    /// Remote image.
    Image {
        /// Image URL.
        url: String,
        /// Optional detail hint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// Local image path.
    LocalImage {
        /// Filesystem path.
        path: String,
        /// Optional detail hint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// Remote audio.
    Audio {
        /// Audio URL.
        url: String,
    },
    /// Local audio path.
    LocalAudio {
        /// Filesystem path.
        path: String,
    },
    /// Skill attachment.
    Skill {
        /// Skill name.
        name: String,
        /// Skill path.
        path: String,
    },
    /// File mention.
    Mention {
        /// Display name.
        name: String,
        /// Path.
        path: String,
    },
}

impl UserInput {
    /// Text input without spans.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }
    }
}

/// Agent message phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessagePhase {
    /// Mid-turn commentary.
    Commentary,
    /// Terminal answer. Does **not** complete the turn by itself.
    FinalAnswer,
}

/// A thread item. Unknown `type` values become [`ThreadItem::Unknown`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ThreadItem {
    /// Known tagged item.
    Typed(TypedThreadItem),
    /// Unknown or partially invalid item. Never a decode error.
    Unknown(Value),
}

/// Known `ThreadItem.type` variants Remuda maps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TypedThreadItem {
    /// User echo.
    #[serde(rename = "userMessage", rename_all = "camelCase")]
    UserMessage {
        /// Item id.
        id: String,
        /// Client correlation id.
        #[serde(default)]
        client_id: Option<String>,
        /// Content blocks.
        #[serde(default)]
        content: Vec<UserInput>,
    },
    /// Assistant text.
    #[serde(rename = "agentMessage", rename_all = "camelCase")]
    AgentMessage {
        /// Item id.
        id: String,
        /// Accumulated text.
        #[serde(default)]
        text: String,
        /// Commentary vs final answer.
        #[serde(default)]
        phase: Option<MessagePhase>,
        /// Memory citations.
        #[serde(default)]
        memory_citation: Option<Value>,
        /// Delivery, for example `async`.
        #[serde(default)]
        delivery: Option<String>,
        /// Non-blocking questions.
        #[serde(default)]
        questions: Option<Value>,
    },
    /// Reasoning / thinking.
    #[serde(rename = "reasoning", rename_all = "camelCase")]
    Reasoning {
        /// Item id.
        id: String,
        /// Summary strings.
        #[serde(default)]
        summary: Vec<String>,
        /// Raw reasoning strings.
        #[serde(default)]
        content: Vec<String>,
    },
    /// Shell / command execution.
    #[serde(rename = "commandExecution", rename_all = "camelCase")]
    CommandExecution {
        /// Item id.
        id: String,
        /// Command string.
        #[serde(default)]
        command: Option<String>,
        /// Cwd.
        #[serde(default)]
        cwd: Option<String>,
        /// Native status string.
        #[serde(default)]
        status: Option<String>,
        /// Aggregated output.
        #[serde(default)]
        aggregated_output: Option<String>,
        /// Exit code.
        #[serde(default)]
        exit_code: Option<i32>,
    },
    /// Proposed or applied file change.
    #[serde(rename = "fileChange", rename_all = "camelCase")]
    FileChange {
        /// Item id.
        id: String,
        /// Native status.
        #[serde(default)]
        status: Option<String>,
        /// Remaining fields.
        #[serde(flatten)]
        extra: MapExtra,
    },
    /// MCP tool call.
    #[serde(rename = "mcpToolCall", rename_all = "camelCase")]
    McpToolCall {
        /// Item id.
        id: String,
        /// Remaining fields.
        #[serde(flatten)]
        extra: MapExtra,
    },
    /// Plan item.
    #[serde(rename = "plan", rename_all = "camelCase")]
    Plan {
        /// Item id.
        id: String,
        /// Plan text.
        #[serde(default)]
        text: String,
    },
}

/// Flattened leftover object fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MapExtra {
    /// Remaining JSON members.
    #[serde(flatten)]
    pub fields: HashMap<String, Value>,
}

impl ThreadItem {
    /// Item id when the typed variant exposes one.
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Typed(TypedThreadItem::UserMessage { id, .. })
            | Self::Typed(TypedThreadItem::AgentMessage { id, .. })
            | Self::Typed(TypedThreadItem::Reasoning { id, .. })
            | Self::Typed(TypedThreadItem::CommandExecution { id, .. })
            | Self::Typed(TypedThreadItem::FileChange { id, .. })
            | Self::Typed(TypedThreadItem::McpToolCall { id, .. })
            | Self::Typed(TypedThreadItem::Plan { id, .. }) => Some(id),
            Self::Unknown(value) => value.get("id").and_then(Value::as_str),
        }
    }

    /// Native `type` string.
    pub fn item_type(&self) -> Option<&str> {
        match self {
            Self::Typed(TypedThreadItem::UserMessage { .. }) => Some("userMessage"),
            Self::Typed(TypedThreadItem::AgentMessage { .. }) => Some("agentMessage"),
            Self::Typed(TypedThreadItem::Reasoning { .. }) => Some("reasoning"),
            Self::Typed(TypedThreadItem::CommandExecution { .. }) => Some("commandExecution"),
            Self::Typed(TypedThreadItem::FileChange { .. }) => Some("fileChange"),
            Self::Typed(TypedThreadItem::McpToolCall { .. }) => Some("mcpToolCall"),
            Self::Typed(TypedThreadItem::Plan { .. }) => Some("plan"),
            Self::Unknown(value) => value.get("type").and_then(Value::as_str),
        }
    }
}

/// `turn/start` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    /// Thread id.
    pub thread_id: String,
    /// User input.
    pub input: Vec<UserInput>,
    /// Optional model override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning effort override. Native type is a non-empty string (`low` … `ultra`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Reasoning summary override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<ReasoningSummary>,
    /// Approval policy override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<AskForApproval>,
    /// Client-generated user message id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_user_message_id: Option<String>,
}

impl TurnStartParams {
    /// Text turn on `thread_id`.
    pub fn text(thread_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            input: vec![UserInput::text(text)],
            model: None,
            effort: None,
            summary: None,
            approval_policy: None,
            client_user_message_id: None,
        }
    }
}

/// `turn/start` result. The turn is `inProgress`; wait for `turn/completed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartResponse {
    /// Accepted turn.
    pub turn: Turn,
}

/// `turn/interrupt` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptParams {
    /// Thread id.
    pub thread_id: String,
    /// Turn to interrupt.
    pub turn_id: String,
}

/// `turn/interrupt` result is an empty object. RPC success is not turn completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TurnInterruptResponse {}

/// Token usage breakdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageBreakdown {
    /// Total tokens.
    #[serde(default)]
    pub total_tokens: i64,
    /// Input tokens.
    #[serde(default)]
    pub input_tokens: i64,
    /// Cached input.
    #[serde(default)]
    pub cached_input_tokens: i64,
    /// Cache write.
    #[serde(default)]
    pub cache_write_input_tokens: i64,
    /// Output tokens.
    #[serde(default)]
    pub output_tokens: i64,
    /// Reasoning output tokens.
    #[serde(default)]
    pub reasoning_output_tokens: i64,
}

/// Thread usage snapshot from `thread/tokenUsage/updated`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTokenUsage {
    /// Cumulative totals.
    #[serde(default)]
    pub total: TokenUsageBreakdown,
    /// Last turn.
    #[serde(default)]
    pub last: TokenUsageBreakdown,
    /// Context window.
    #[serde(default)]
    pub model_context_window: Option<i64>,
}
