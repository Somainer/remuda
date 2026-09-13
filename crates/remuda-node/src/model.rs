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
        "shell" | "terminal" | "shell-pty" => Ok(DriverKind::ShellPty),
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
    "manual".to_owned()
}

/// Body accepted by `POST /v1/instances` and the local JSON-RPC `instance.create` bridge.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateInstanceRequest {
    /// Authenticated Hub caller kind; absent or unknown means Agent.
    #[serde(
        default = "crate::origin::agent_origin",
        deserialize_with = "crate::origin::deserialize_origin"
    )]
    pub origin: remuda_protocol::InputOrigin,
    /// Transport-only scoped credential for this instance's MCP process.
    #[serde(default, skip_serializing)]
    pub agent_credential: Option<crate::origin::AgentCredential>,
    /// Optional client-selected Command identity for retry-safe Hub forwarding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<CommandId>,
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
    /// Permission label; omission defaults to manual.
    #[serde(default = "default_permission")]
    pub permission_mode: String,
    /// Optional initial prompt delivered through the bounded instance task.
    #[serde(default)]
    pub prompt: String,
    /// Working directory for the native driver (git worktree path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Provider delegation (`none` / `gateway`). Omitted means native none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<String>,
    /// Host-local `--settings` overlay path. `~` is expanded on the Node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_overlay_path: Option<String>,
    /// Explicit `CLAUDE_CONFIG_DIR`. Wins over inherited default login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_config_dir: Option<String>,
    /// `--max-budget-usd` cap forwarded onto Claude argv.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_opt_stringish"
    )]
    pub max_budget_usd: Option<String>,
    /// Public overlay snapshot from Hub (no token).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_overlay: Option<serde_json::Value>,
    /// Auth token injected by Hub SecretBroker for this launch only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_auth_token: Option<String>,
    /// Native session this launch continues with `--resume <uuid>` (D-026).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_session_id: Option<String>,
    /// Exited Instance this launch continues; recorded as `instance.parent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<InstanceId>,
}

impl CreateInstanceRequest {
    /// Copy launch fields that Hub stores on `spec` onto the Node request.
    pub fn apply_spec_launch_fields(&mut self, spec: &serde_json::Value) {
        if let Some(value) = spec
            .get("delegation")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
        {
            self.delegation = Some(value.to_owned());
        }
        if let Some(value) = spec
            .get("providerProfileId")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
        {
            self.provider_profile_id = value.to_owned();
        }
        if let Some(value) = spec
            .get("settingsOverlayPath")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
        {
            self.settings_overlay_path = Some(value.to_owned());
        }
        if let Some(value) = spec
            .get("claudeConfigDir")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
        {
            self.claude_config_dir = Some(value.to_owned());
        }
        if self.max_budget_usd.is_none() {
            self.max_budget_usd = stringish(spec.get("maxBudgetUsd"));
        }
        if self.provider_overlay.is_none() {
            self.provider_overlay = spec.get("providerOverlay").cloned();
        }
        if self.provider_auth_token.is_none() {
            self.provider_auth_token = spec
                .get("providerAuthToken")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
        }
        if self.resume_session_id.is_none() {
            self.resume_session_id = spec
                .get("resumeSessionId")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
        }
        if self.resumed_from.is_none() {
            self.resumed_from = spec
                .get("resumedFrom")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .and_then(|value| InstanceId::try_from(value.to_owned()).ok());
        }
    }
}

fn stringish(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(text) if !text.is_empty() => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn deserialize_opt_stringish<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Ok(stringish(
        Option::<serde_json::Value>::deserialize(deserializer)?.as_ref(),
    ))
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
    /// Write logical keys (`tty.write` / `instance.keys`).
    #[serde(rename = "tty.write", alias = "instance.keys")]
    WriteTty,
    /// Switch model / effort (`instance.configure`).
    #[serde(rename = "instance.configure", alias = "configure")]
    Configure,
}

/// Command body for the local REST surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceCommandRequest {
    /// Authenticated Hub caller kind; absent or unknown means Agent.
    #[serde(
        default = "crate::origin::agent_origin",
        deserialize_with = "crate::origin::deserialize_origin"
    )]
    pub origin: remuda_protocol::InputOrigin,
    /// Optional client-supplied idempotency identity.
    #[serde(default)]
    pub command_id: Option<CommandId>,
    /// Requested operation.
    pub operation: CommandAction,
    /// Prompt text for `send`.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Attachment metadata for `send` (D-027). The bytes are pulled from the
    /// Hub and written to disk before the command reaches a driver.
    #[serde(default)]
    pub attachments: Vec<remuda_protocol::hubnode::AttachmentRef>,
    /// Optional Run identity carried by cancel requests.
    #[serde(default)]
    pub run_id: Option<RunId>,
    /// Interaction identity carried by `respond_interaction`.
    #[serde(default)]
    pub interaction_id: Option<InteractionId>,
    /// Opaque fake answer retained only for deterministic command hashing.
    #[serde(default)]
    pub answer: Option<serde_json::Value>,
    /// Logical keys for `tty.write`.
    #[serde(default)]
    pub keys: Option<Vec<String>>,
    /// Model id for `instance.configure`.
    #[serde(default)]
    pub model: Option<String>,
    /// Native effort name for `instance.configure`.
    #[serde(default)]
    pub effort_name: Option<String>,
    /// Native effort index for `instance.configure`.
    #[serde(default)]
    pub effort_index: Option<u32>,
}

impl InstanceCommandRequest {
    /// Fill optional configure fields after setting the operation.
    pub fn with_configure(mut self, params: &serde_json::Value) -> Self {
        self.model = params
            .get("model")
            .or_else(|| params.get("modelId"))
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        if let Some(effort) = params.get("effort") {
            self.effort_name = effort
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            self.effort_index = effort
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .map(|n| n as u32);
        }
        self
    }
}
