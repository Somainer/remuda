//! Request/response bodies matching `crates/remuda-hub/openapi/openapi.json`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `openapi.json` `#/components/schemas/InstanceCreate`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceCreate {
    /// Target host (`hst_…`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    /// Optional workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Agent kind (`claude`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Driver kind (`claude-print`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// UI title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Initial prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Placement object (`host` / `labels` / `kind: any`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<Value>,
    /// Delegation mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delegation: Option<String>,
    /// Model id (ignored by current Hub if unknown).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Live instance name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Working directory on the host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Worktree name (`remuda worktree create`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
}

/// `openapi.json` `#/components/schemas/InstanceRecord`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceRecord {
    /// `ins_…`.
    pub instance_id: String,
    /// Owning host.
    pub host_id: String,
    /// Agent kind.
    #[serde(default)]
    pub kind: String,
    /// Driver kind.
    #[serde(default)]
    pub driver: String,
    /// Lifecycle.
    #[serde(default)]
    pub lifecycle: String,
    /// Extra fields Hub may add.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// `openapi.json` `#/components/schemas/InstanceCreateResult`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceCreateResult {
    /// Created instance.
    pub instance: InstanceRecord,
    /// Queued `instance.create` command.
    #[serde(default)]
    pub command: Value,
    /// Chosen host, when Hub reports it.
    #[serde(default)]
    pub host_id: Option<String>,
}

/// `openapi.json` `#/components/schemas/CommandRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandRequest {
    /// Optional command id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    /// RPC operation (`instance.send`, `instance.cancel`, `interaction.respond`, …).
    pub operation: String,
    /// Operation payload.
    #[serde(default)]
    pub payload: Value,
    /// Optional idempotency key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// `openapi.json` `#/components/schemas/JournalPage`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalPage {
    /// Instance id.
    pub instance_id: String,
    /// Durable seq as a decimal string.
    pub durable_seq: String,
    /// Mirrored events (`{seq, event, …}`).
    #[serde(default)]
    pub events: Vec<Value>,
}

/// `POST /v1/login` request.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginRequest {
    /// Bootstrap token.
    pub bootstrap_token: String,
    /// Device display name.
    pub device_name: String,
}

/// `POST /v1/login` response.
#[derive(Debug, Clone, Deserialize)]
pub struct LoginResponse {
    /// Device bearer token.
    pub token: String,
}
