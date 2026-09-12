//! Development HTTP request and response declarations.

use remuda_protocol::{
    AgentKind, Command, CommandId, DriverKind, HostId, Instance, InstanceId, InteractionId, RunId,
    WorkspaceId,
};
use serde::{Deserialize, Serialize};

fn default_agent_kind() -> AgentKind {
    AgentKind::Claude
}

fn default_driver_kind() -> DriverKind {
    DriverKind::ClaudePrint
}

fn deserialize_driver_kind<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<DriverKind, D::Error> {
    let raw = String::deserialize(deserializer)?;
    match raw.as_str() {
        "pty" => Ok(DriverKind::GenericPty),
        other => serde_json::from_value(serde_json::Value::String(other.to_owned()))
            .map_err(serde::de::Error::custom),
    }
}

fn default_model() -> String {
    "fake".to_owned()
}

fn default_profile() -> String {
    "dev-fake".to_owned()
}

fn default_permission() -> String {
    "dontAsk".to_owned()
}

/// Body accepted by `POST /v1/instances` and the local JSON-RPC `instance.create` bridge.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateInstanceRequest {
    /// Optional client-selected Instance identity for retry-safe creation.
    #[serde(default)]
    pub instance_id: Option<InstanceId>,
    /// Target host; omitted requests use the development registry host.
    #[serde(default)]
    pub host_id: Option<HostId>,
    /// Target workspace; omitted requests use the development registry workspace.
    #[serde(default)]
    pub workspace_id: Option<WorkspaceId>,
    /// Native product kind.
    #[serde(default = "default_agent_kind")]
    pub kind: AgentKind,
    /// Driver selected from the local registry (`pty` is an alias of `generic-pty`).
    #[serde(
        default = "default_driver_kind",
        deserialize_with = "deserialize_driver_kind"
    )]
    pub driver: DriverKind,
    /// Requested native model label.
    #[serde(default = "default_model")]
    pub model: String,
    /// Extra allowlisted native CLI arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Provider profile label used to construct the native launch profile.
    #[serde(default = "default_profile")]
    pub provider_profile_id: String,
    /// Development permission label; the fake driver performs no tools.
    #[serde(default = "default_permission")]
    pub permission_mode: String,
    /// Optional initial prompt delivered through the bounded instance task.
    #[serde(default)]
    pub prompt: String,
}

/// Result returned after an Instance and its create command are represented in the local store.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateInstanceResponse {
    /// Latest create command state.
    pub command: Command,
    /// Created Instance projection.
    pub instance: Instance,
}

/// Operations accepted by `POST /v1/instances/:id/commands`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandAction {
    /// Submit one fake prompt.
    Send,
    /// Cancel the current fake operation.
    Cancel,
    /// Record a fake Interaction response.
    RespondInteraction,
    /// Close the instance task and mark the Instance exited.
    Close,
}

/// Command body for the local REST surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceCommandRequest {
    /// Optional client-supplied idempotency identity.
    #[serde(default)]
    pub command_id: Option<CommandId>,
    /// Requested operation.
    pub operation: CommandAction,
    /// Prompt text for `send`.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Optional Run identity carried by cancel requests.
    #[serde(default)]
    pub run_id: Option<RunId>,
    /// Interaction identity carried by `respond_interaction`.
    #[serde(default)]
    pub interaction_id: Option<InteractionId>,
    /// Opaque fake answer retained only for deterministic command hashing.
    #[serde(default)]
    pub answer: Option<serde_json::Value>,
}
