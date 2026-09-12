//! Rpc wire declarations; `protocol.md`.

use crate::*;
use serde::{Deserialize, Serialize};

/// ProtocolVersion; `protocol.md` §7.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolVersion {
    /// `major`; protocol §7.1.
    pub major: u16,
    /// `minor`; protocol §7.1.
    pub minor: u16,
}

/// ProtocolRange; `protocol.md` §7.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolRange {
    /// `major`; protocol §7.1.
    pub major: u16,
    /// `min_minor`; protocol §7.1.
    pub min_minor: u16,
    /// `max_minor`; protocol §7.1.
    pub max_minor: u16,
}

/// JournalWatermark; `protocol.md` §7.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JournalWatermark {
    /// `journal_id`; protocol §7.1.
    pub journal_id: Id,
    /// `durable_seq`; protocol §7.1.
    pub durable_seq: U64,
}

/// ResumeCursor; `protocol.md` §7.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResumeCursor {
    /// `journal_id`; protocol §7.1.
    pub journal_id: Id,
    /// `after_seq`; protocol §7.1.
    pub after_seq: U64,
}

/// HelloParams; `protocol.md` §7.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HelloParams {
    /// `host_id`; protocol §7.1.
    pub host_id: HostId,
    /// `node_epoch`; protocol §7.1.
    pub node_epoch: Id,
    /// `node_version`; protocol §7.1.
    pub node_version: String,
    /// `protocol`; protocol §7.1.
    pub protocol: ProtocolRange,
    /// `observation_schema_majors`; protocol §7.1.
    pub observation_schema_majors: Vec<u16>,
    /// `features`; protocol §7.1.
    pub features: Vec<String>,
    /// `resume_cursors`; protocol §7.1.
    pub resume_cursors: Vec<ResumeCursor>,
}

/// TransportLimits; `protocol.md` §7.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TransportLimits {
    /// `max_json_frame_bytes`; protocol §7.4.
    pub max_json_frame_bytes: u32,
    /// `max_binary_chunk_bytes`; protocol §7.4.
    pub max_binary_chunk_bytes: u32,
    /// `max_tty_input_bytes`; protocol §7.4.
    pub max_tty_input_bytes: u32,
    /// `max_in_flight_rpc`; protocol §7.4.
    pub max_in_flight_rpc: u32,
    /// `max_events_per_batch`; protocol §7.4.
    pub max_events_per_batch: u32,
    /// `max_subscription_buffer_events`; protocol §7.4.
    pub max_subscription_buffer_events: u32,
    /// `heartbeat_interval_ms`; protocol §7.4.
    pub heartbeat_interval_ms: u32,
    /// `lease_ttl_ms`; protocol §7.4.
    pub lease_ttl_ms: u32,
    /// `max_wait_ms`; protocol §7.4.
    pub max_wait_ms: u32,
}

/// ConnectionLease; `protocol.md` §7.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionLease {
    /// `lease_id`; protocol §7.1.
    pub lease_id: Id,
    /// `fence`; protocol §7.1.
    pub fence: U64,
    /// `expires_at`; protocol §7.1.
    pub expires_at: Timestamp,
}

/// HelloResult; `protocol.md` §7.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HelloResult {
    /// `protocol`; protocol §7.1.
    pub protocol: ProtocolVersion,
    /// `connection_id`; protocol §7.1.
    pub connection_id: Id,
    /// `server_epoch`; protocol §7.1.
    pub server_epoch: Id,
    /// `observation_schema_major`; protocol §7.1.
    pub observation_schema_major: SchemaVersion,
    /// `features`; protocol §7.1.
    pub features: Vec<String>,
    /// `limits`; protocol §7.1.
    pub limits: TransportLimits,
    /// `lease`; protocol §7.1.
    pub lease: ConnectionLease,
    /// `reconcile_required`; protocol §7.1.
    pub reconcile_required: bool,
}

/// HeartbeatParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatParams {
    /// `connection_id`; protocol §7.2.
    pub connection_id: Id,
    /// `registry_watermarks`; protocol §7.2.
    pub registry_watermarks: Vec<JournalWatermark>,
    /// `instance_watermarks`; protocol §7.2.
    pub instance_watermarks: Vec<JournalWatermark>,
    /// `lease_id`; protocol §7.2.
    pub lease_id: Id,
}

/// HeartbeatResult; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatResult {
    /// `server_time`; protocol §7.2.
    pub server_time: Timestamp,
    /// `lease_expires_at`; protocol §7.2.
    pub lease_expires_at: Timestamp,
}

/// HostReportParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HostReportParams {
    /// `host_id`; protocol §7.2.
    pub host_id: HostId,
    /// `node_epoch`; protocol §7.2.
    pub node_epoch: Id,
    /// `platform`; protocol §7.2.
    pub platform: Platform,
    /// `driver_inventory`; protocol §7.2.
    pub driver_inventory: Vec<DriverDescriptor>,
    /// `workspace_ids`; protocol §7.2.
    pub workspace_ids: Vec<WorkspaceId>,
    /// `instance_ids`; protocol §7.2.
    pub instance_ids: Vec<InstanceId>,
}

/// HostReportResult; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HostReportResult {
    /// `host_revision`; protocol §7.2.
    pub host_revision: U64,
    /// `registry_seq`; protocol §7.2.
    pub registry_seq: U64,
}

/// HostParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HostParams {
    /// `host_id`; protocol §7.2.
    pub host_id: HostId,
}

/// PageParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PageParams {
    /// `cursor`; protocol §7.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// `limit`; protocol §7.2.
    pub limit: u32,
}

/// DriverCapabilitiesParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DriverCapabilitiesParams {
    /// `driver_kind`; protocol §7.2.
    pub driver_kind: DriverKind,
    /// `binary_ref`; protocol §7.2.
    pub binary_ref: Id,
    /// `profile_ref`; protocol §7.2.
    pub profile_ref: ProfileRef,
}

/// WorkspaceRegisterParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRegisterParams {
    /// `workspace_id`; protocol §7.2.
    pub workspace_id: WorkspaceId,
    /// `root_path`; protocol §7.2.
    pub root_path: String,
    /// `label`; protocol §7.2.
    pub label: String,
    /// `write_policy`; protocol §7.2.
    pub write_policy: WritePolicy,
}

/// WorkspaceParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceParams {
    /// `workspace_id`; protocol §7.2.
    pub workspace_id: WorkspaceId,
}

/// WorkspaceListParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceListParams {
    /// `host_id`; protocol §7.2.
    pub host_id: HostId,
    /// `cursor`; protocol §7.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// `limit`; protocol §7.2.
    pub limit: u32,
}

/// WorktreeCreateParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeCreateParams {
    /// `worktree_id`; protocol §7.2.
    pub worktree_id: WorktreeId,
    /// `parent_workspace_id`; protocol §7.2.
    pub parent_workspace_id: WorkspaceId,
    /// `base_oid`; protocol §7.2.
    pub base_oid: String,
    /// `branch`; protocol §7.2.
    pub branch: String,
    /// `path`; protocol §7.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// WorktreeRemoveParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeRemoveParams {
    /// `worktree_id`; protocol §7.2.
    pub worktree_id: WorktreeId,
    /// `expected_head_oid`; protocol §7.2.
    pub expected_head_oid: String,
    /// `expected_dirty`; protocol §7.2.
    pub expected_dirty: BoolLiteral<false>,
}

/// InstanceCreateParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceCreateParams {
    /// `instance_id`; protocol §7.2.
    pub instance_id: InstanceId,
    /// `spec`; protocol §7.2.
    pub spec: InstanceSpec,
    /// `initial_input`; protocol §7.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_input: Option<CreateInitialInput>,
}

/// CreateInitialInput; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type")]
pub enum CreateInitialInput {
    /// `prompt` payload; §7.2.
    #[serde(rename = "prompt")]
    Prompt(Box<PromptInput>),
}

/// InstanceCreateResult; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceCreateResult {
    /// `command`; protocol §7.2.
    pub command: Command,
    /// `instance_id`; protocol §7.2.
    pub instance_id: InstanceId,
    /// `prepared`; protocol §7.2.
    pub prepared: bool,
    /// `send_command_id`; protocol §7.2.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub send_command_id: Option<CommandId>,
    /// `run_id`; protocol §7.2.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub run_id: Option<RunId>,
}

/// InstanceAttachParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceAttachParams {
    /// `instance_id`; protocol §7.2.
    pub instance_id: InstanceId,
    /// `reference`; protocol §7.2.
    #[serde(rename = "ref")]
    pub reference: AttachRef,
}

/// Explicit human request to create a Claude background attach pane; §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstanceOpenTerminalParams {
    /// Existing claude-bg Instance; no new Run or prompt is created.
    pub instance_id: InstanceId,
    /// Exact daemon job bound to that Instance's native store.
    pub background_job_id: String,
    /// Explicit acknowledgement that `claude attach` may wake the job.
    pub allow_wake: BoolLiteral<true>,
    /// Pinned Herdr server and named session for the new attach pane.
    pub carrier: PtyCarrier,
}

/// InstanceResumeParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceResumeParams {
    /// `instance_id`; protocol §7.2.
    pub instance_id: InstanceId,
    /// `native_ref`; protocol §7.2.
    pub native_ref: NativeRef,
    /// `provider_profile_revision`; protocol §7.2.
    pub provider_profile_revision: U64,
    /// `expected_previous_generation`; protocol §7.2.
    pub expected_previous_generation: U64,
}

/// InstanceSendParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceSendParams {
    /// `instance_id`; protocol §7.2.
    pub instance_id: InstanceId,
    /// `run_id`; protocol §7.2.
    pub run_id: RunId,
    /// `input`; protocol §7.2.
    pub input: SendInput,
    /// `completion_scope`; protocol §7.2.
    pub completion_scope: CompletionScope,
}

/// Native effort tier stored on `instance.configure`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EffortSelection {
    /// Index into the harness-native table.
    pub index: u32,
    /// Native tier name (`think`, `high`, `max`, …).
    pub name: String,
}

/// InstanceConfigureParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceConfigureParams {
    /// `instance_id`; protocol §7.2.
    pub instance_id: InstanceId,
    /// `model_id`; protocol §7.2.
    #[serde(default)]
    pub model_id: String,
    /// `effective`; protocol §7.2.
    #[serde(default = "default_model_effective")]
    pub effective: ModelEffective,
    /// Native effort tier (index + name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<EffortSelection>,
    /// Optional permission mode for print drivers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
}

fn default_model_effective() -> ModelEffective {
    ModelEffective::NextTurn
}

/// ForkBoundary; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ForkBoundary {
    /// `actor_type`; protocol §7.2.
    #[serde(rename = "type")]
    pub actor_type: ForkBoundaryType,
}

/// InstanceForkParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceForkParams {
    /// `source_instance_id`; protocol §7.2.
    pub source_instance_id: InstanceId,
    /// `new_instance_id`; protocol §7.2.
    pub new_instance_id: InstanceId,
    /// `native_boundary`; protocol §7.2.
    pub native_boundary: ForkBoundary,
    /// `new_spec`; protocol §7.2.
    pub new_spec: InstanceSpec,
}

/// InstanceCancelParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceCancelParams {
    /// `instance_id`; protocol §7.2.
    pub instance_id: InstanceId,
    /// `run_id`; protocol §7.2.
    pub run_id: RunId,
}

/// InstanceCloseParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceCloseParams {
    /// `instance_id`; protocol §7.2.
    pub instance_id: InstanceId,
    /// `mode`; protocol §7.2.
    pub mode: CloseMode,
    /// `retain_native_session`; protocol §7.2.
    pub retain_native_session: BoolLiteral<true>,
}

/// InstanceParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceParams {
    /// `instance_id`; protocol §7.2.
    pub instance_id: InstanceId,
}

/// InstanceListParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceListParams {
    /// `workspace_id`; protocol §7.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// `cursor`; protocol §7.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// `limit`; protocol §7.2.
    pub limit: u32,
}

/// CommandParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommandParams {
    /// `command_id`; protocol §7.2.
    pub command_id: CommandId,
}

/// CommandListParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommandListParams {
    /// `instance_id`; protocol §7.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<InstanceId>,
    /// `resolution`; protocol §7.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<ResolutionState>,
    /// `cursor`; protocol §7.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// `limit`; protocol §7.2.
    pub limit: u32,
}

/// RunParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunParams {
    /// `run_id`; protocol §7.2.
    pub run_id: RunId,
}

/// RunListParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunListParams {
    /// `instance_id`; protocol §7.2.
    pub instance_id: InstanceId,
    /// `cursor`; protocol §7.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// `limit`; protocol §7.2.
    pub limit: u32,
}

/// RunWaitParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunWaitParams {
    /// `run_id`; protocol §7.2.
    pub run_id: RunId,
    /// `condition`; protocol §7.2.
    pub condition: RunWaitCondition,
    /// `after_seq`; protocol §7.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_seq: Option<U64>,
    /// `timeout_ms`; protocol §7.2.
    pub timeout_ms: u32,
}

/// RunWaitResult; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunWaitResult {
    /// `reason`; protocol §7.2.
    pub reason: WaitReason,
    /// `run`; protocol §7.2.
    pub run: Run,
    /// `pending_interaction_ids`; protocol §7.2.
    pub pending_interaction_ids: Vec<InteractionId>,
    /// `as_of_seq`; protocol §7.2.
    pub as_of_seq: U64,
}

/// WorkflowWaitParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowWaitParams {
    /// `instance_id`; protocol §7.2.
    pub instance_id: InstanceId,
    /// `workflow_id`; protocol §7.2.
    pub workflow_id: Id,
    /// `after_seq`; protocol §7.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_seq: Option<U64>,
    /// `timeout_ms`; protocol §7.2.
    pub timeout_ms: u32,
}

/// WorkflowWaitResult; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowWaitResult {
    /// `reason`; protocol §7.2.
    pub reason: WaitReason,
    /// `workflow`; protocol §7.2.
    pub workflow: WorkflowRunPayload,
    /// `as_of_seq`; protocol §7.2.
    pub as_of_seq: U64,
}

/// InteractionListParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InteractionListParams {
    /// `instance_id`; protocol §7.2.
    pub instance_id: InstanceId,
    /// `state`; protocol §7.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<InteractionState>,
}

/// InteractionParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InteractionParams {
    /// `interaction_id`; protocol §7.2.
    pub interaction_id: InteractionId,
}

/// InteractionRespondParams; `protocol.md` §6.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InteractionRespondParams {
    /// `interaction_id`; protocol §6.1.
    pub interaction_id: InteractionId,
    /// `request_version`; protocol §6.1.
    pub request_version: U64,
    /// `process_generation`; protocol §6.1.
    pub process_generation: U64,
    /// `run_generation`; protocol §6.1.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub run_generation: Option<U64>,
    /// `connection_epoch`; protocol §6.1.
    pub connection_epoch: Id,
    /// `answer`; protocol §6.1.
    pub answer: InteractionAnswer,
}

/// InteractionRespondResult; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InteractionRespondResult {
    /// `command`; protocol §7.2.
    pub command: Command,
    /// `interaction`; protocol §7.2.
    pub interaction: Interaction,
}

/// EventsSubscribeParams; `protocol.md` §7.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventsSubscribeParams {
    /// `journal_id`; protocol §7.3.
    pub journal_id: Id,
    /// `after_seq`; protocol §7.3.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub after_seq: Option<U64>,
    /// `snapshot`; protocol §7.3.
    pub snapshot: SnapshotMode,
    /// `projection_version`; protocol §7.3.
    pub projection_version: String,
    /// `batch_limit`; protocol §7.3.
    pub batch_limit: u32,
}

/// HistoryCoverage; `protocol.md` §7.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HistoryCoverage {
    /// `earliest_retained_seq`; protocol §7.3.
    pub earliest_retained_seq: U64,
    /// `complete`; protocol §7.3.
    pub complete: bool,
}

/// InstanceSnapshot; `protocol.md` §7.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceSnapshot {
    /// `projection_version`; protocol §7.3.
    pub projection_version: String,
    /// `projection_epoch`; protocol §7.3.
    pub projection_epoch: Id,
    /// `as_of_seq`; protocol §7.3.
    pub as_of_seq: U64,
    /// `instance`; protocol §7.3.
    pub instance: Instance,
    /// `runs`; protocol §7.3.
    pub runs: Vec<Run>,
    /// `commands`; protocol §7.3.
    pub commands: Vec<Command>,
    /// `pending_interactions`; protocol §7.3.
    pub pending_interactions: Vec<Interaction>,
    /// `nodes`; protocol §7.3.
    pub nodes: Vec<ConversationNode>,
    /// `history`; protocol §7.3.
    pub history: HistoryCoverage,
}

/// ConversationNode; `protocol.md` §7.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", content = "payload")]
pub enum ConversationNode {
    /// `message` payload; §7.3.
    #[serde(rename = "message")]
    Message(Box<MessagePayload>),
    /// `thought` payload; §7.3.
    #[serde(rename = "thought")]
    Thought(Box<ThoughtPayload>),
    /// `tool_call` payload; §7.3.
    #[serde(rename = "tool_call")]
    ToolCall(Box<ToolCallPayload>),
    /// `tool_result` payload; §7.3.
    #[serde(rename = "tool_result")]
    ToolResult(Box<ToolResultPayload>),
}

/// RegistrySnapshot; `protocol.md` §7.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RegistrySnapshot {
    /// `projection_version`; protocol §7.3.
    pub projection_version: String,
    /// `projection_epoch`; protocol §7.3.
    pub projection_epoch: Id,
    /// `as_of_seq`; protocol §7.3.
    pub as_of_seq: U64,
    /// `hosts`; protocol §7.3.
    pub hosts: Vec<Host>,
    /// `workspaces`; protocol §7.3.
    pub workspaces: Vec<Workspace>,
    /// `commands`; protocol §7.3.
    pub commands: Vec<Command>,
    /// `history`; protocol §7.3.
    pub history: HistoryCoverage,
}

/// Snapshot; `protocol.md` §7.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "scope")]
pub enum Snapshot {
    /// `instance` payload; §7.3.
    #[serde(rename = "instance")]
    Instance(Box<InstanceSnapshot>),
    /// `registry` payload; §7.3.
    #[serde(rename = "registry")]
    Registry(Box<RegistrySnapshot>),
}

/// EventsSubscribeResult; `protocol.md` §7.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventsSubscribeResult {
    /// `subscription_id`; protocol §7.3.
    pub subscription_id: Id,
    /// `journal_id`; protocol §7.3.
    pub journal_id: Id,
    /// `floor_seq`; protocol §7.3.
    pub floor_seq: U64,
    /// `durable_seq`; protocol §7.3.
    pub durable_seq: U64,
    /// `snapshot`; protocol §7.3.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub snapshot: Option<Snapshot>,
    /// `replay_from_seq`; protocol §7.3.
    pub replay_from_seq: U64,
    /// `next_cursor`; protocol §7.3.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub next_cursor: Option<String>,
    /// `connection_id`; protocol §7.3.
    pub connection_id: Id,
}

/// EventsReadParams; `protocol.md` §7.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventsReadParams {
    /// `journal_id`; protocol §7.3.
    pub journal_id: Id,
    /// `after_seq`; protocol §7.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_seq: Option<U64>,
    /// `before_seq`; protocol §7.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_seq: Option<U64>,
    /// `limit`; protocol §7.3.
    pub limit: u32,
    /// `cursor`; protocol §7.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// EventsReadResult; `protocol.md` §7.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventsReadResult {
    /// `events`; protocol §7.3.
    pub events: Vec<JournalEvent>,
    /// `next_cursor`; protocol §7.3.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub next_cursor: Option<String>,
    /// `floor_seq`; protocol §7.3.
    pub floor_seq: U64,
    /// `durable_seq`; protocol §7.3.
    pub durable_seq: U64,
}

/// EventsAckParams; `protocol.md` §7.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventsAckParams {
    /// `subscription_id`; protocol §7.3.
    pub subscription_id: Id,
    /// `journal_id`; protocol §7.3.
    pub journal_id: Id,
    /// `through_seq`; protocol §7.3.
    pub through_seq: U64,
}

/// EventsAckResult; `protocol.md` §7.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventsAckResult {
    /// `acknowledged_seq`; protocol §7.3.
    pub acknowledged_seq: U64,
}

/// SubscriptionParams; `protocol.md` §7.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionParams {
    /// `subscription_id`; protocol §7.3.
    pub subscription_id: Id,
}

/// ReconcileInstanceParams; `protocol.md` §7.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileInstanceParams {
    /// `instance_id`; protocol §7.5.
    pub instance_id: InstanceId,
    /// `expected_generation`; protocol §7.5.
    pub expected_generation: U64,
    /// `command_ids`; protocol §7.5.
    pub command_ids: Vec<CommandId>,
}

/// ReconcileInstanceResult; `protocol.md` §7.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileInstanceResult {
    /// `state`; protocol §7.5.
    pub state: InstanceLifecycle,
    /// `evidence_event_ids`; protocol §7.5.
    pub evidence_event_ids: Vec<EventId>,
    /// `unresolved_command_ids`; protocol §7.5.
    pub unresolved_command_ids: Vec<CommandId>,
}

/// TtyAttachParams; `protocol.md` §7.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TtyAttachParams {
    /// `instance_id`; protocol §7.4.
    pub instance_id: InstanceId,
    /// `process_generation`; protocol §7.4.
    pub process_generation: U64,
    /// `mode`; protocol §7.4.
    pub mode: TtyMode,
    /// `previous_stream_id`; protocol §7.4.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub previous_stream_id: Option<Id>,
    /// `after_offset`; protocol §7.4.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub after_offset: Option<U64>,
}

/// TtyWriterLease; `protocol.md` §7.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TtyWriterLease {
    /// `lease_id`; protocol §7.4.
    pub lease_id: Id,
    /// `expires_at`; protocol §7.4.
    pub expires_at: Timestamp,
    /// `input_next_seq`; protocol §7.4.
    pub input_next_seq: U64,
}

/// TtyAttachResult; `protocol.md` §7.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TtyAttachResult {
    /// `stream_id`; protocol §7.4.
    pub stream_id: Id,
    /// `stream_epoch`; protocol §7.4.
    pub stream_epoch: Id,
    /// `representation`; protocol §7.4.
    pub representation: TtyRepresentation,
    /// `next_offset`; protocol §7.4.
    pub next_offset: U64,
    /// `available_from`; protocol §7.4.
    pub available_from: U64,
    /// `screen_snapshot_ref`; protocol §7.4.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub screen_snapshot_ref: Option<Id>,
    /// `snapshot_at_offset`; protocol §7.4.
    pub snapshot_at_offset: Knowledge<U64>,
    /// `writer_lease`; protocol §7.4.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub writer_lease: Option<TtyWriterLease>,
    /// Bounded snapshot bytes (standard base64) for attach-time replay; D-016.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_base64: Option<String>,
}

/// TtyDetachParams; `protocol.md` §7.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TtyDetachParams {
    /// `instance_id`; protocol §7.4.
    pub instance_id: InstanceId,
    /// `stream_id`; protocol §7.4.
    pub stream_id: Id,
    /// `writer_lease_id`; protocol §7.4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_lease_id: Option<Id>,
}

/// TtyWriteParams; `protocol.md` §7.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TtyWriteParams {
    /// `instance_id`; protocol §7.4.
    pub instance_id: InstanceId,
    /// `process_generation`; protocol §7.4.
    pub process_generation: U64,
    /// `stream_id`; protocol §7.4.
    pub stream_id: Id,
    /// `stream_epoch`; protocol §7.4.
    pub stream_epoch: Id,
    /// `writer_lease_id`; protocol §7.4.
    pub writer_lease_id: Id,
    /// `input_seq`; protocol §7.4.
    pub input_seq: U64,
    /// `data_base64`; protocol §7.4.
    pub data_base64: String,
}

/// TtyResizeParams; `protocol.md` §7.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TtyResizeParams {
    /// `instance_id`; protocol §7.4.
    pub instance_id: InstanceId,
    /// `stream_id`; protocol §7.4.
    pub stream_id: Id,
    /// `writer_lease_id`; protocol §7.4.
    pub writer_lease_id: Id,
    /// `resize_revision`; protocol §7.4.
    pub resize_revision: U64,
    /// `cols`; protocol §7.4.
    pub cols: u16,
    /// `rows`; protocol §7.4.
    pub rows: u16,
}

/// TtyResizeResult; `protocol.md` §7.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TtyResizeResult {
    /// `resize_revision`; protocol §7.4.
    pub resize_revision: U64,
    /// `cols`; protocol §7.4.
    pub cols: u16,
    /// `rows`; protocol §7.4.
    pub rows: u16,
}

/// ObjectParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObjectParams {
    /// `object_id`; protocol §7.2.
    pub object_id: Id,
}

/// ObjectReadParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObjectReadParams {
    /// `object_id`; protocol §7.2.
    pub object_id: Id,
    /// `offset`; protocol §7.2.
    pub offset: U64,
    /// `length`; protocol §7.2.
    pub length: U64,
    /// `expected_digest`; protocol §7.2.
    pub expected_digest: Digest,
}

/// ObjectMetadata; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObjectMetadata {
    /// `object_id`; protocol §7.2.
    pub object_id: Id,
    /// `size_bytes`; protocol §7.2.
    pub size_bytes: U64,
    /// `digest`; protocol §7.2.
    pub digest: Digest,
    /// `media_type`; protocol §7.2.
    pub media_type: String,
}

/// ObjectReadResult; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObjectReadResult {
    /// `stream_id`; protocol §7.2.
    pub stream_id: Id,
    /// `object_id`; protocol §7.2.
    pub object_id: Id,
    /// `offset`; protocol §7.2.
    pub offset: U64,
    /// `length`; protocol §7.2.
    pub length: U64,
    /// `digest`; protocol §7.2.
    pub digest: Digest,
}

/// ObjectPrepareParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObjectPrepareParams {
    /// `object_id`; protocol §7.2.
    pub object_id: Id,
    /// `host_id`; protocol §7.2.
    pub host_id: HostId,
    /// `workspace_id`; protocol §7.2.
    pub workspace_id: WorkspaceId,
    /// `media_type`; protocol §7.2.
    pub media_type: String,
    /// `size_bytes`; protocol §7.2.
    pub size_bytes: U64,
    /// `digest`; protocol §7.2.
    pub digest: Digest,
    /// `purpose`; protocol §7.2.
    pub purpose: ObjectPurpose,
}

/// ObjectPrepareResult; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObjectPrepareResult {
    /// `upload_id`; protocol §7.2.
    pub upload_id: Id,
    /// `next_offset`; protocol §7.2.
    pub next_offset: U64,
    /// `expires_at`; protocol §7.2.
    pub expires_at: Timestamp,
    /// `max_chunk_bytes`; protocol §7.2.
    pub max_chunk_bytes: u32,
}

/// ObjectWriteParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObjectWriteParams {
    /// `upload_id`; protocol §7.2.
    pub upload_id: Id,
    /// `offset`; protocol §7.2.
    pub offset: U64,
    /// `data_base64`; protocol §7.2.
    pub data_base64: String,
    /// `chunk_digest`; protocol §7.2.
    pub chunk_digest: Digest,
}

/// ObjectWriteResult; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObjectWriteResult {
    /// `next_offset`; protocol §7.2.
    pub next_offset: U64,
}

/// ObjectCommitParams; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObjectCommitParams {
    /// `upload_id`; protocol §7.2.
    pub upload_id: Id,
    /// `digest`; protocol §7.2.
    pub digest: Digest,
}

/// ObjectCommitResult; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObjectCommitResult {
    /// `object_id`; protocol §7.2.
    pub object_id: Id,
    /// `size_bytes`; protocol §7.2.
    pub size_bytes: U64,
    /// `digest`; protocol §7.2.
    pub digest: Digest,
}

/// CommandEnvelope; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommandEnvelope<P> {
    /// `command_id`; protocol §7.2.
    pub command_id: CommandId,
    /// `payload`; protocol §7.2.
    pub payload: P,
    /// `expected`; protocol §7.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<ExpectedState>,
    /// `expires_at`; protocol §7.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
}

/// CommandResult; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommandResult {
    /// `command`; protocol §7.2.
    pub command: Command,
    /// `related_command_ids`; protocol §7.2.
    pub related_command_ids: Vec<CommandId>,
}

/// Page; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    /// `items`; protocol §7.2.
    pub items: Vec<T>,
    /// `next_cursor`; protocol §7.2.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub next_cursor: Option<String>,
}

/// EmptyResult; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EmptyResult {}

/// MethodCall; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "method", content = "params")]
pub enum MethodCall {
    /// `runtime.hello` payload; §7.2.
    #[serde(rename = "runtime.hello")]
    RuntimeHello(Box<HelloParams>),
    /// `runtime.heartbeat` payload; §7.2.
    #[serde(rename = "runtime.heartbeat")]
    RuntimeHeartbeat(Box<HeartbeatParams>),
    /// `host.report` payload; §7.2.
    #[serde(rename = "host.report")]
    HostReport(Box<HostReportParams>),
    /// `host.get` payload; §7.2.
    #[serde(rename = "host.get")]
    HostGet(Box<HostParams>),
    /// `host.list` payload; §7.2.
    #[serde(rename = "host.list")]
    HostList(Box<PageParams>),
    /// `driver.list` payload; §7.2.
    #[serde(rename = "driver.list")]
    DriverList(Box<HostParams>),
    /// `driver.capabilities` payload; §7.2.
    #[serde(rename = "driver.capabilities")]
    DriverCapabilities(Box<DriverCapabilitiesParams>),
    /// `workspace.register` payload; §7.2.
    #[serde(rename = "workspace.register")]
    WorkspaceRegister(Box<CommandEnvelope<WorkspaceRegisterParams>>),
    /// `workspace.get` payload; §7.2.
    #[serde(rename = "workspace.get")]
    WorkspaceGet(Box<WorkspaceParams>),
    /// `workspace.list` payload; §7.2.
    #[serde(rename = "workspace.list")]
    WorkspaceList(Box<WorkspaceListParams>),
    /// `worktree.create` payload; §7.2.
    #[serde(rename = "worktree.create")]
    WorktreeCreate(Box<CommandEnvelope<WorktreeCreateParams>>),
    /// `worktree.remove` payload; §7.2.
    #[serde(rename = "worktree.remove")]
    WorktreeRemove(Box<CommandEnvelope<WorktreeRemoveParams>>),
    /// `instance.create` payload; §7.2.
    #[serde(rename = "instance.create")]
    InstanceCreate(Box<CommandEnvelope<InstanceCreateParams>>),
    /// `instance.attach` payload; §7.2.
    #[serde(rename = "instance.attach")]
    InstanceAttach(Box<CommandEnvelope<InstanceAttachParams>>),
    /// Explicitly create an attach pane; never invoked by tty.attach or recovery; §7.2.
    #[serde(rename = "instance.open_terminal")]
    InstanceOpenTerminal(Box<CommandEnvelope<InstanceOpenTerminalParams>>),
    /// `instance.resume` payload; §7.2.
    #[serde(rename = "instance.resume")]
    InstanceResume(Box<CommandEnvelope<InstanceResumeParams>>),
    /// `instance.send` payload; §7.2.
    #[serde(rename = "instance.send")]
    InstanceSend(Box<CommandEnvelope<InstanceSendParams>>),
    /// `instance.configure` payload; §7.2.
    #[serde(rename = "instance.configure")]
    InstanceConfigure(Box<CommandEnvelope<InstanceConfigureParams>>),
    /// `instance.fork` payload; §7.2.
    #[serde(rename = "instance.fork")]
    InstanceFork(Box<CommandEnvelope<InstanceForkParams>>),
    /// `instance.cancel` payload; §7.2.
    #[serde(rename = "instance.cancel")]
    InstanceCancel(Box<CommandEnvelope<InstanceCancelParams>>),
    /// `instance.close` payload; §7.2.
    #[serde(rename = "instance.close")]
    InstanceClose(Box<CommandEnvelope<InstanceCloseParams>>),
    /// `instance.get` payload; §7.2.
    #[serde(rename = "instance.get")]
    InstanceGet(Box<InstanceParams>),
    /// `instance.list` payload; §7.2.
    #[serde(rename = "instance.list")]
    InstanceList(Box<InstanceListParams>),
    /// `command.get` payload; §7.2.
    #[serde(rename = "command.get")]
    CommandGet(Box<CommandParams>),
    /// `command.list` payload; §7.2.
    #[serde(rename = "command.list")]
    CommandList(Box<CommandListParams>),
    /// `run.get` payload; §7.2.
    #[serde(rename = "run.get")]
    RunGet(Box<RunParams>),
    /// `run.list` payload; §7.2.
    #[serde(rename = "run.list")]
    RunList(Box<RunListParams>),
    /// `run.wait` payload; §7.2.
    #[serde(rename = "run.wait")]
    RunWait(Box<RunWaitParams>),
    /// `workflow.wait` payload; §7.2.
    #[serde(rename = "workflow.wait")]
    WorkflowWait(Box<WorkflowWaitParams>),
    /// `interaction.list` payload; §7.2.
    #[serde(rename = "interaction.list")]
    InteractionList(Box<InteractionListParams>),
    /// `interaction.get` payload; §7.2.
    #[serde(rename = "interaction.get")]
    InteractionGet(Box<InteractionParams>),
    /// `interaction.respond` payload; §7.2.
    #[serde(rename = "interaction.respond")]
    InteractionRespond(Box<CommandEnvelope<InteractionRespondParams>>),
    /// `events.subscribe` payload; §7.2.
    #[serde(rename = "events.subscribe")]
    EventsSubscribe(Box<EventsSubscribeParams>),
    /// `events.read` payload; §7.2.
    #[serde(rename = "events.read")]
    EventsRead(Box<EventsReadParams>),
    /// `events.ack` payload; §7.2.
    #[serde(rename = "events.ack")]
    EventsAck(Box<EventsAckParams>),
    /// `events.unsubscribe` payload; §7.2.
    #[serde(rename = "events.unsubscribe")]
    EventsUnsubscribe(Box<SubscriptionParams>),
    /// `reconcile.instance` payload; §7.2.
    #[serde(rename = "reconcile.instance")]
    ReconcileInstance(Box<ReconcileInstanceParams>),
    /// `tty.attach` payload; §7.2.
    #[serde(rename = "tty.attach")]
    TtyAttach(Box<TtyAttachParams>),
    /// `tty.detach` payload; §7.2.
    #[serde(rename = "tty.detach")]
    TtyDetach(Box<TtyDetachParams>),
    /// `tty.write` payload; §7.2.
    #[serde(rename = "tty.write")]
    TtyWrite(Box<CommandEnvelope<TtyWriteParams>>),
    /// `tty.resize` payload; §7.2.
    #[serde(rename = "tty.resize")]
    TtyResize(Box<TtyResizeParams>),
    /// `object.stat` payload; §7.2.
    #[serde(rename = "object.stat")]
    ObjectStat(Box<ObjectParams>),
    /// `object.read` payload; §7.2.
    #[serde(rename = "object.read")]
    ObjectRead(Box<ObjectReadParams>),
    /// `object.prepare` payload; §7.2.
    #[serde(rename = "object.prepare")]
    ObjectPrepare(Box<ObjectPrepareParams>),
    /// `object.write` payload; §7.2.
    #[serde(rename = "object.write")]
    ObjectWrite(Box<ObjectWriteParams>),
    /// `object.commit` payload; §7.2.
    #[serde(rename = "object.commit")]
    ObjectCommit(Box<ObjectCommitParams>),
}

impl MethodCall {
    /// Method spelling discriminator; protocol §7.2.
    pub fn method(&self) -> MethodName {
        match self {
            Self::RuntimeHello(..) => MethodName::RuntimeHello,
            Self::RuntimeHeartbeat(..) => MethodName::RuntimeHeartbeat,
            Self::HostReport(..) => MethodName::HostReport,
            Self::HostGet(..) => MethodName::HostGet,
            Self::HostList(..) => MethodName::HostList,
            Self::DriverList(..) => MethodName::DriverList,
            Self::DriverCapabilities(..) => MethodName::DriverCapabilities,
            Self::WorkspaceRegister(..) => MethodName::WorkspaceRegister,
            Self::WorkspaceGet(..) => MethodName::WorkspaceGet,
            Self::WorkspaceList(..) => MethodName::WorkspaceList,
            Self::WorktreeCreate(..) => MethodName::WorktreeCreate,
            Self::WorktreeRemove(..) => MethodName::WorktreeRemove,
            Self::InstanceCreate(..) => MethodName::InstanceCreate,
            Self::InstanceAttach(..) => MethodName::InstanceAttach,
            Self::InstanceOpenTerminal(..) => MethodName::InstanceOpenTerminal,
            Self::InstanceResume(..) => MethodName::InstanceResume,
            Self::InstanceSend(..) => MethodName::InstanceSend,
            Self::InstanceConfigure(..) => MethodName::InstanceConfigure,
            Self::InstanceFork(..) => MethodName::InstanceFork,
            Self::InstanceCancel(..) => MethodName::InstanceCancel,
            Self::InstanceClose(..) => MethodName::InstanceClose,
            Self::InstanceGet(..) => MethodName::InstanceGet,
            Self::InstanceList(..) => MethodName::InstanceList,
            Self::CommandGet(..) => MethodName::CommandGet,
            Self::CommandList(..) => MethodName::CommandList,
            Self::RunGet(..) => MethodName::RunGet,
            Self::RunList(..) => MethodName::RunList,
            Self::RunWait(..) => MethodName::RunWait,
            Self::WorkflowWait(..) => MethodName::WorkflowWait,
            Self::InteractionList(..) => MethodName::InteractionList,
            Self::InteractionGet(..) => MethodName::InteractionGet,
            Self::InteractionRespond(..) => MethodName::InteractionRespond,
            Self::EventsSubscribe(..) => MethodName::EventsSubscribe,
            Self::EventsRead(..) => MethodName::EventsRead,
            Self::EventsAck(..) => MethodName::EventsAck,
            Self::EventsUnsubscribe(..) => MethodName::EventsUnsubscribe,
            Self::ReconcileInstance(..) => MethodName::ReconcileInstance,
            Self::TtyAttach(..) => MethodName::TtyAttach,
            Self::TtyDetach(..) => MethodName::TtyDetach,
            Self::TtyWrite(..) => MethodName::TtyWrite,
            Self::TtyResize(..) => MethodName::TtyResize,
            Self::ObjectStat(..) => MethodName::ObjectStat,
            Self::ObjectRead(..) => MethodName::ObjectRead,
            Self::ObjectPrepare(..) => MethodName::ObjectPrepare,
            Self::ObjectWrite(..) => MethodName::ObjectWrite,
            Self::ObjectCommit(..) => MethodName::ObjectCommit,
        }
    }
}

/// RpcRequest; `protocol.md` §7.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RpcRequest {
    /// `jsonrpc`; protocol §7.1.
    pub jsonrpc: JsonRpcVersion,
    /// `id`; protocol §7.1.
    pub id: String,
    /// `call`; protocol §7.1.
    #[serde(flatten)]
    pub call: MethodCall,
}

/// RpcSuccess; `protocol.md` §7.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct RpcSuccess<R> {
    /// `jsonrpc`; protocol §7.1.
    pub jsonrpc: JsonRpcVersion,
    /// `id`; protocol §7.1.
    pub id: String,
    /// `result`; protocol §7.1.
    pub result: R,
}

/// RpcFailure; `protocol.md` §7.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct RpcFailure {
    /// `jsonrpc`; protocol §7.1.
    pub jsonrpc: JsonRpcVersion,
    /// `id`; protocol §7.1.
    pub id: String,
    /// `error`; protocol §7.1.
    pub error: RpcError,
}

/// A response contains exactly one of result/error; `protocol.md` §7.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum RpcResponse<R> {
    /// Successful response with method-specific result type.
    Success(RpcSuccess<R>),
    /// Protocol or runtime error response.
    Failure(RpcFailure),
}

/// EventsBatch; `protocol.md` §7.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventsBatch {
    /// `subscription_id`; protocol §7.3.
    pub subscription_id: Id,
    /// `journal_id`; protocol §7.3.
    pub journal_id: Id,
    /// `from_seq`; protocol §7.3.
    pub from_seq: U64,
    /// `to_seq`; protocol §7.3.
    pub to_seq: U64,
    /// `events`; protocol §7.3.
    pub events: NonEmpty<JournalEvent>,
    /// `durable_seq`; protocol §7.3.
    pub durable_seq: U64,
}

impl EventsBatch {
    /// Check the contiguous journal interval before advancing an ACK; protocol §7.3.
    pub fn validate(&self) -> Result<(), WireValueError> {
        if self.to_seq > self.durable_seq || self.from_seq > self.to_seq || self.from_seq.0 == 0 {
            return Err(WireValueError("invalid batch watermark".into()));
        }
        let mut previous_id = std::collections::BTreeSet::new();
        for (index, event) in self.events.as_slice().iter().enumerate() {
            let (journal, seq, id) = event.position();
            let expected = self
                .from_seq
                .0
                .checked_add(index as u64)
                .ok_or_else(|| WireValueError("batch sequence overflow".into()))?;
            if journal != &self.journal_id || seq.0 != expected || !previous_id.insert(id) {
                return Err(WireValueError(
                    "event batch is not a unique contiguous journal interval".into(),
                ));
            }
        }
        if self
            .events
            .as_slice()
            .last()
            .expect("nonempty")
            .position()
            .1
            != self.to_seq
        {
            return Err(WireValueError("batch end does not match toSeq".into()));
        }
        Ok(())
    }
}

/// NotificationBody; `protocol.md` §7.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "method", content = "params")]
pub enum NotificationBody {
    /// `events.batch` payload; §7.3.
    #[serde(rename = "events.batch")]
    EventsBatch(Box<EventsBatch>),
}

/// RpcNotification; `protocol.md` §7.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RpcNotification {
    /// `jsonrpc`; protocol §7.3.
    pub jsonrpc: JsonRpcVersion,
    /// `event`; protocol §7.3.
    #[serde(flatten)]
    pub event: NotificationBody,
}
