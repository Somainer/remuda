//! Launch wire declarations; `protocol.md`.

use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// ProfileRef; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileRef {
    /// `id`; protocol §4.1.
    pub id: Id,
    /// `revision`; protocol §4.1.
    pub revision: U64,
}

/// SettingsOverlay; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsOverlay {
    /// `format`; protocol §4.1.
    pub format: SettingsFormat,
    /// `object_ref`; protocol §4.1.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub object_ref: Option<Id>,
    /// `revision`; protocol §4.1.
    pub revision: U64,
}

/// NativeHome; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeHome {
    /// `mode`; protocol §4.1.
    pub mode: NativeHomeMode,
    /// `store_id`; protocol §4.1.
    pub store_id: Id,
}

/// InstanceSpec; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceSpec {
    /// `schema_version`; protocol §4.1.
    pub schema_version: SchemaVersion,
    /// `host`; protocol §4.1.
    pub host: HostId,
    /// `workspace_id`; protocol §4.1.
    pub workspace_id: WorkspaceId,
    /// `kind`; protocol §4.1.
    pub kind: AgentKind,
    /// `driver`; protocol §4.1.
    pub driver: DriverKind,
    /// `binary_ref`; protocol §4.1.
    pub binary_ref: Id,
    /// `cwd`; protocol §4.1.
    pub cwd: String,
    /// `worktree`; protocol §4.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeSpec>,
    /// `provider_profile`; protocol §4.1.
    pub provider_profile: ProfileRef,
    /// `model_id`; protocol §4.1.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub model_id: Option<String>,
    /// `permission_mode`; protocol §4.1.
    pub permission_mode: PermissionMode,
    /// `env`; protocol §4.1.
    pub env: BTreeMap<String, EnvBinding>,
    /// `args`; protocol §4.1.
    pub args: Vec<String>,
    /// `settings_overlay`; protocol §4.1.
    pub settings_overlay: SettingsOverlay,
    /// `native_home`; protocol §4.1.
    pub native_home: NativeHome,
    /// `carrier`; protocol §4.1.
    pub carrier: CarrierSpec,
    /// `required_capabilities`; protocol §4.1.
    pub required_capabilities: Vec<CapabilityName>,
    /// `completion_scope`; protocol §4.1.
    pub completion_scope: CompletionScope,
    /// `parent`; protocol §4.1.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub parent: Option<InstanceParent>,
}

/// ProviderSelection; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSelection {
    /// `profile_id`; protocol §4.1.
    pub profile_id: Id,
    /// `profile_revision`; protocol §4.1.
    pub profile_revision: U64,
    /// `endpoint_id`; protocol §4.1.
    pub endpoint_id: Id,
    /// `ingress`; protocol §4.1.
    pub ingress: ProviderIngress,
    /// `credential_ref`; protocol §4.1.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub credential_ref: Option<Id>,
    /// `credential_version`; protocol §4.1.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub credential_version: Option<U64>,
    /// `model_requested`; protocol §4.1.
    pub model_requested: String,
    /// `model_resolved`; protocol §4.1.
    pub model_resolved: Knowledge<String>,
    /// `selection_reason`; protocol §4.1.
    pub selection_reason: SelectionReason,
}

/// PromptInput; `protocol.md` §3.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptInput {
    /// `mode`; protocol §3.1.
    pub mode: PromptMode,
    /// `blocks`; protocol §3.1.
    pub blocks: Vec<ContentBlock>,
    /// `origin`; protocol §3.1.
    pub origin: InputOrigin,
    /// `native_client_message_id`; protocol §3.1.
    pub native_client_message_id: String,
}

/// SteerInput; `protocol.md` §3.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SteerInput {
    /// `expected_native_turn_id`; protocol §3.1.
    pub expected_native_turn_id: String,
    /// `blocks`; protocol §3.1.
    pub blocks: Vec<ContentBlock>,
    /// `native_client_message_id`; protocol §3.1.
    pub native_client_message_id: String,
}

/// ModelSwitchInput; `protocol.md` §3.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSwitchInput {
    /// `model_id`; protocol §3.1.
    pub model_id: String,
    /// `effective`; protocol §3.1.
    pub effective: ModelEffective,
}

/// DriverInput; `protocol.md` §3.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DriverInput {
    /// `prompt` payload; §3.1.
    #[serde(rename = "prompt")]
    Prompt(Box<PromptInput>),
    /// `steer` payload; §3.1.
    #[serde(rename = "steer")]
    Steer(Box<SteerInput>),
    /// `model-switch` payload; §3.1.
    #[serde(rename = "model-switch")]
    ModelSwitch(Box<ModelSwitchInput>),
}

/// SendInput; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SendInput {
    /// `prompt` payload; §7.2.
    #[serde(rename = "prompt")]
    Prompt(Box<PromptInput>),
    /// `steer` payload; §7.2.
    #[serde(rename = "steer")]
    Steer(Box<SteerInput>),
}

/// LiteralEnv; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiteralEnv {
    /// `value`; protocol §4.1.
    pub value: String,
    /// `visibility`; protocol §4.1.
    pub visibility: EnvVisibility,
}

/// CredentialEnv; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialEnv {
    /// `credential_ref`; protocol §4.1.
    pub credential_ref: Id,
    /// `version`; protocol §4.1.
    pub version: U64,
}

/// HostEnv; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostEnv {
    /// `name`; protocol §4.1.
    pub name: String,
}

/// EnvBinding; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source")]
pub enum EnvBinding {
    /// `literal` payload; §4.1.
    #[serde(rename = "literal")]
    Literal(Box<LiteralEnv>),
    /// `credential` payload; §4.1.
    #[serde(rename = "credential")]
    Credential(Box<CredentialEnv>),
    /// `host-env` payload; §4.1.
    #[serde(rename = "host-env")]
    HostEnv(Box<HostEnv>),
}

/// ClaudePermission; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudePermission {
    /// `mode`; protocol §4.1.
    pub mode: ClaudePermissionMode,
    /// `interaction`; protocol §4.1.
    pub interaction: ClaudeInteractionMode,
}

/// CodexPermission; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexPermission {
    /// `approval_policy`; protocol §4.1.
    pub approval_policy: ApprovalPolicy,
    /// `approvals_reviewer`; protocol §4.1.
    pub approvals_reviewer: ApprovalsReviewer,
    /// `execution`; protocol §4.1.
    pub execution: CodexExecution,
}

/// SandboxExecution; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct SandboxExecution {
    /// `sandbox`; protocol §4.1.
    pub sandbox: SandboxMode,
}

/// NamedPermissions; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct NamedPermissions {
    /// `permissions`; protocol §4.1.
    pub permissions: String,
}

/// Mutually exclusive Codex sandbox or permission profile; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CodexExecution {
    /// Built-in sandbox choice.
    Sandbox(SandboxExecution),
    /// Named native permissions profile.
    Permissions(NamedPermissions),
}

/// GrokPermission; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrokPermission {
    /// `mode`; protocol §4.1.
    pub mode: GrokPermissionMode,
}

/// AgyPermission; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgyPermission {
    /// `mode`; protocol §4.1.
    pub mode: AgyPermissionMode,
}

/// GenericPermission; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenericPermission {
    /// `mode`; protocol §4.1.
    pub mode: GenericPermissionMode,
}

/// PermissionMode; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum PermissionMode {
    /// `claude` payload; §4.1.
    #[serde(rename = "claude")]
    Claude(Box<ClaudePermission>),
    /// `codex` payload; §4.1.
    #[serde(rename = "codex")]
    Codex(Box<CodexPermission>),
    /// `grok` payload; §4.1.
    #[serde(rename = "grok")]
    Grok(Box<GrokPermission>),
    /// `agy` payload; §4.1.
    #[serde(rename = "agy")]
    Agy(Box<AgyPermission>),
    /// `generic` payload; §4.1.
    #[serde(rename = "generic")]
    Generic(Box<GenericPermission>),
}

/// ExistingWorktree; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExistingWorktree {
    /// `worktree_id`; protocol §4.1.
    pub worktree_id: WorktreeId,
}

/// CreateWorktree; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorktree {
    /// `worktree_id`; protocol §4.1.
    pub worktree_id: WorktreeId,
    /// `base_oid`; protocol §4.1.
    pub base_oid: String,
    /// `branch`; protocol §4.1.
    pub branch: String,
}

/// WorktreeSpec; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode")]
pub enum WorktreeSpec {
    /// `existing` payload; §4.1.
    #[serde(rename = "existing")]
    Existing(Box<ExistingWorktree>),
    /// `create` payload; §4.1.
    #[serde(rename = "create")]
    Create(Box<CreateWorktree>),
}

/// PtyCarrier; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtyCarrier {
    /// `backend`; protocol §4.1.
    pub backend: PtyBackend,
    /// `server_ref`; protocol §4.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_ref: Option<Id>,
}

/// CarrierSpec; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CarrierSpec {
    /// `stdio` payload; §4.1.
    #[serde(rename = "stdio")]
    Stdio,
    /// `pty` payload; §4.1.
    #[serde(rename = "pty")]
    Pty(Box<PtyCarrier>),
}
