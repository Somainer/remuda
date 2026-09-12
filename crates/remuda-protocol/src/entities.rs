//! Entities wire declarations; `protocol.md`.

use crate::*;
use serde::{Deserialize, Serialize};

/// EntityMeta; `protocol.md` §1.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EntityMeta<I = Id> {
    /// `id`; protocol §1.1.
    pub id: I,
    /// `revision`; protocol §1.1.
    pub revision: U64,
    /// `created_at`; protocol §1.1.
    pub created_at: Timestamp,
    /// `updated_at`; protocol §1.1.
    pub updated_at: Timestamp,
}

/// ActorRef; `protocol.md` §1.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActorRef {
    /// `principal_id`; protocol §1.1.
    pub principal_id: Id,
    /// `actor_type`; protocol §1.1.
    #[serde(rename = "type")]
    pub actor_type: ActorType,
    /// `device_id`; protocol §1.1.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub device_id: Option<Id>,
    /// `instance_id`; protocol §1.1.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub instance_id: Option<InstanceId>,
}

/// Platform; `protocol.md` §2.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Platform {
    /// `os`; protocol §2.1.
    pub os: String,
    /// `arch`; protocol §2.1.
    pub arch: String,
    /// `path_style`; protocol §2.1.
    pub path_style: PathStyle,
}

/// HostTransport; `protocol.md` §2.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HostTransport {
    /// `mode`; protocol §2.1.
    pub mode: HostTransportMode,
    /// `endpoint_ref`; protocol §2.1.
    pub endpoint_ref: Id,
}

/// Host; `protocol.md` §2.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Host {
    /// `meta`; protocol §2.1.
    #[serde(flatten)]
    pub meta: EntityMeta<HostId>,
    /// `label`; protocol §2.1.
    pub label: String,
    /// `owner_principal_id`; protocol §2.1.
    pub owner_principal_id: Id,
    /// `state`; protocol §2.1.
    pub state: HostState,
    /// `node_version`; protocol §2.1.
    pub node_version: Knowledge<String>,
    /// `platform`; protocol §2.1.
    pub platform: Knowledge<Platform>,
    /// `identity_key_id`; protocol §2.1.
    pub identity_key_id: Id,
    /// `node_epoch`; protocol §2.1.
    pub node_epoch: Knowledge<Id>,
    /// `transport`; protocol §2.1.
    pub transport: HostTransport,
    /// `last_seen_at`; protocol §2.1.
    pub last_seen_at: Knowledge<Timestamp>,
    /// `lease_expires_at`; protocol §2.1.
    pub lease_expires_at: Knowledge<Timestamp>,
    /// `driver_inventory`; protocol §2.1.
    pub driver_inventory: Vec<DriverDescriptor>,
    /// `journal_id`; protocol §2.1.
    pub journal_id: Id,
    /// `durable_seq`; protocol §2.1.
    pub durable_seq: U64,
}

/// RepositoryRef; `protocol.md` §2.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryRef {
    /// `repository_id`; protocol §2.2.
    pub repository_id: Id,
    /// `git_common_dir`; protocol §2.2.
    pub git_common_dir: String,
    /// `head_oid`; protocol §2.2.
    pub head_oid: String,
}

/// WriterLease; `protocol.md` §2.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WriterLease {
    /// `instance_id`; protocol §2.2.
    pub instance_id: InstanceId,
    /// `fence`; protocol §2.2.
    pub fence: U64,
}

/// WorktreeRecord; `protocol.md` §2.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeRecord {
    /// `id`; protocol §2.2.
    pub id: WorktreeId,
    /// `host_id`; protocol §2.2.
    pub host_id: HostId,
    /// `repository_id`; protocol §2.2.
    pub repository_id: Id,
    /// `parent_workspace_id`; protocol §2.2.
    pub parent_workspace_id: WorkspaceId,
    /// `path`; protocol §2.2.
    pub path: String,
    /// `branch`; protocol §2.2.
    pub branch: Knowledge<String>,
    /// `base_oid`; protocol §2.2.
    pub base_oid: Knowledge<String>,
    /// `head_oid`; protocol §2.2.
    pub head_oid: Knowledge<String>,
    /// `managed_by`; protocol §2.2.
    pub managed_by: WorktreeOwner,
    /// `state`; protocol §2.2.
    pub state: WorktreeState,
    /// `dirty`; protocol §2.2.
    pub dirty: Knowledge<bool>,
    /// `created_by_command_id`; protocol §2.2.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub created_by_command_id: Option<CommandId>,
}

/// Workspace; `protocol.md` §2.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    /// `meta`; protocol §2.2.
    #[serde(flatten)]
    pub meta: EntityMeta<WorkspaceId>,
    /// `host_id`; protocol §2.2.
    pub host_id: HostId,
    /// `label`; protocol §2.2.
    pub label: String,
    /// `root_path`; protocol §2.2.
    pub root_path: String,
    /// `canonical_root`; protocol §2.2.
    pub canonical_root: Knowledge<String>,
    /// `state`; protocol §2.2.
    pub state: WorkspaceState,
    /// `repository`; protocol §2.2.
    pub repository: Knowledge<RepositoryRef>,
    /// `worktree`; protocol §2.2.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub worktree: Option<WorktreeRecord>,
    /// `write_policy`; protocol §2.2.
    pub write_policy: WritePolicy,
    /// `writer_leases`; protocol §2.2.
    pub writer_leases: Vec<WriterLease>,
    /// `access_policy_revision`; protocol §2.2.
    pub access_policy_revision: U64,
    /// `journal_id`; protocol §2.2.
    pub journal_id: Id,
    /// `durable_seq`; protocol §2.2.
    pub durable_seq: U64,
}

/// InstanceParent; `protocol.md` §2.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceParent {
    /// `instance_id`; protocol §2.3.
    pub instance_id: InstanceId,
    /// `run_id`; protocol §2.3.
    pub run_id: RunId,
    /// `command_id`; protocol §2.3.
    pub command_id: CommandId,
}

/// ProcessExit; `protocol.md` §2.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProcessExit {
    /// `code`; protocol §2.3.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub code: Option<i32>,
    /// `signal`; protocol §2.3.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub signal: Option<String>,
    /// `observed_at`; protocol §2.3.
    pub observed_at: Timestamp,
}

/// Instance; `protocol.md` §2.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    /// `meta`; protocol §2.3.
    #[serde(flatten)]
    pub meta: EntityMeta<InstanceId>,
    /// `host_id`; protocol §2.3.
    pub host_id: HostId,
    /// `workspace_id`; protocol §2.3.
    pub workspace_id: WorkspaceId,
    /// `kind`; protocol §2.3.
    pub kind: AgentKind,
    /// `driver`; protocol §2.3.
    pub driver: DriverKind,
    /// `lifecycle`; protocol §2.3.
    pub lifecycle: InstanceLifecycle,
    /// `activity`; protocol §2.3.
    pub activity: Knowledge<Activity>,
    /// `activity_evidence_event_ids`; protocol §2.3.
    pub activity_evidence_event_ids: Vec<EventId>,
    /// `connectivity`; protocol §2.3.
    pub connectivity: Connectivity,
    /// `ownership`; protocol §2.3.
    pub ownership: Ownership,
    /// `native_ref`; protocol §2.3.
    pub native_ref: NativeRef,
    /// `process_ref`; protocol §2.3.
    pub process_ref: ProcessRef,
    /// `spec_revision`; protocol §2.3.
    pub spec_revision: U64,
    /// `launch_id`; protocol §2.3.
    pub launch_id: Knowledge<Id>,
    /// `capabilities`; protocol §2.3.
    pub capabilities: CapabilitySnapshot,
    /// `owner_fence`; protocol §2.3.
    pub owner_fence: U64,
    /// `active_run_ids`; protocol §2.3.
    pub active_run_ids: Vec<RunId>,
    /// `parent`; protocol §2.3.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub parent: Option<InstanceParent>,
    /// `journal_id`; protocol §2.3.
    pub journal_id: Id,
    /// `durable_seq`; protocol §2.3.
    pub durable_seq: U64,
    /// `exit`; protocol §2.3.
    pub exit: Knowledge<ProcessExit>,
    /// Driver or native diagnostic when [`InstanceLifecycle::Failed`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// RunCause; `protocol.md` §2.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunCause {
    /// `actor_type`; protocol §2.4.
    #[serde(rename = "type")]
    pub actor_type: RunCauseType,
    /// `command_id`; protocol §2.4.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub command_id: Option<CommandId>,
    /// `source_event_id`; protocol §2.4.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub source_event_id: Option<EventId>,
}

/// NativeTurn; `protocol.md` §2.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NativeTurn {
    /// `id`; protocol §2.4.
    pub id: String,
    /// `source`; protocol §2.4.
    pub source: String,
    /// `result_index`; protocol §2.4.
    pub result_index: Knowledge<U64>,
}

/// TerminalEvidence; `protocol.md` §2.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerminalEvidence {
    /// `event_ids`; protocol §2.4.
    pub event_ids: Vec<EventId>,
    /// `rule_id`; protocol §2.4.
    pub rule_id: String,
    /// `native_outcome`; protocol §2.4.
    pub native_outcome: String,
}

/// RunResult; `protocol.md` §2.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunResult {
    /// `message_ids`; protocol §2.4.
    pub message_ids: Vec<Id>,
    /// `artifact_ids`; protocol §2.4.
    pub artifact_ids: Vec<Id>,
    /// `output_ref`; protocol §2.4.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub output_ref: Option<Id>,
}

/// OutstandingWork; `protocol.md` §2.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OutstandingWork {
    /// `workflow_ids`; protocol §2.4.
    pub workflow_ids: Vec<Id>,
    /// `child_run_ids`; protocol §2.4.
    pub child_run_ids: Vec<RunId>,
    /// `detached_task_ids`; protocol §2.4.
    pub detached_task_ids: Vec<String>,
}

/// Run; `protocol.md` §2.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    /// `meta`; protocol §2.4.
    #[serde(flatten)]
    pub meta: EntityMeta<RunId>,
    /// `instance_id`; protocol §2.4.
    pub instance_id: InstanceId,
    /// `host_id`; protocol §2.4.
    pub host_id: HostId,
    /// `cause`; protocol §2.4.
    pub cause: RunCause,
    /// `parent_run_id`; protocol §2.4.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub parent_run_id: Option<RunId>,
    /// `root_run_id`; protocol §2.4.
    pub root_run_id: RunId,
    /// `parentage`; protocol §2.4.
    pub parentage: Parentage,
    /// `completion_scope`; protocol §2.4.
    pub completion_scope: CompletionScope,
    /// `state`; protocol §2.4.
    pub state: RunState,
    /// `process_generation`; protocol §2.4.
    pub process_generation: U64,
    /// `run_generation`; protocol §2.4.
    pub run_generation: U64,
    /// `native_turns`; protocol §2.4.
    pub native_turns: Vec<NativeTurn>,
    /// `input_ref`; protocol §2.4.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub input_ref: Option<Id>,
    /// `input_digest`; protocol §2.4.
    pub input_digest: Knowledge<Digest>,
    /// `provider_selection`; protocol §2.4.
    pub provider_selection: ProviderSelection,
    /// `capability_snapshot_id`; protocol §2.4.
    pub capability_snapshot_id: Id,
    /// `started_at`; protocol §2.4.
    pub started_at: Knowledge<Timestamp>,
    /// `ended_at`; protocol §2.4.
    pub ended_at: Knowledge<Timestamp>,
    /// `terminal_evidence`; protocol §2.4.
    pub terminal_evidence: Knowledge<TerminalEvidence>,
    /// `state_confidence`; protocol §2.4.
    pub state_confidence: StateConfidence,
    /// `result`; protocol §2.4.
    pub result: Knowledge<RunResult>,
    /// `outstanding_work`; protocol §2.4.
    pub outstanding_work: Knowledge<OutstandingWork>,
}

/// CommandTarget; `protocol.md` §2.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommandTarget {
    /// `host_id`; protocol §2.5.
    pub host_id: HostId,
    /// `instance_id`; protocol §2.5.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub instance_id: Option<InstanceId>,
    /// `run_id`; protocol §2.5.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub run_id: Option<RunId>,
}

/// ExpectedState; `protocol.md` §2.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExpectedState {
    /// `instance_revision`; protocol §2.5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_revision: Option<U64>,
    /// `process_generation`; protocol §2.5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_generation: Option<U64>,
    /// `run_generation`; protocol §2.5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_generation: Option<U64>,
    /// `owner_fence`; protocol §2.5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_fence: Option<U64>,
    /// `interaction_version`; protocol §2.5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_version: Option<U64>,
}

/// ForwardIntent; `protocol.md` §2.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ForwardIntent {
    /// `host_id`; protocol §2.5.
    pub host_id: HostId,
    /// `node_epoch`; protocol §2.5.
    pub node_epoch: Id,
    /// `hub_revision`; protocol §2.5.
    pub hub_revision: U64,
    /// `created_at`; protocol §2.5.
    pub created_at: Timestamp,
}

/// Acceptance; `protocol.md` §2.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Acceptance {
    /// `scope`; protocol §2.5.
    pub scope: AcceptanceScope,
    /// `event_ids`; protocol §2.5.
    pub event_ids: Vec<EventId>,
}

/// Settlement; `protocol.md` §2.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Settlement {
    /// `outcome`; protocol §2.5.
    pub outcome: SettlementOutcome,
    /// `result_ref`; protocol §2.5.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub result_ref: Option<Id>,
    /// `error`; protocol §2.5.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub error: Option<RuntimeError>,
}

/// NodeReceipt; `protocol.md` §2.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeReceipt {
    /// `node_epoch`; protocol §2.5.
    pub node_epoch: Id,
    /// `ledger_revision`; protocol §2.5.
    pub ledger_revision: U64,
}

/// Command; `protocol.md` §2.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Command {
    /// `meta`; protocol §2.5.
    #[serde(flatten)]
    pub meta: EntityMeta<CommandId>,
    /// `command_id`; protocol §2.5.
    pub command_id: CommandId,
    /// `actor`; protocol §2.5.
    pub actor: ActorRef,
    /// `origin`; protocol §2.5.
    pub origin: CommandOrigin,
    /// `operation`; protocol §2.5.
    pub operation: CommandOperation,
    /// `target`; protocol §2.5.
    pub target: CommandTarget,
    /// `payload_ref`; protocol §2.5.
    pub payload_ref: Id,
    /// `payload_digest`; protocol §2.5.
    pub payload_digest: Digest,
    /// `expected`; protocol §2.5.
    pub expected: ExpectedState,
    /// `state`; protocol §2.5.
    pub state: CommandState,
    /// `authority`; protocol §2.5.
    pub authority: CommandAuthority,
    /// `forward_intent`; protocol §2.5.
    pub forward_intent: Knowledge<ForwardIntent>,
    /// `dispatch`; protocol §2.5.
    pub dispatch: DispatchState,
    /// `resolution`; protocol §2.5.
    pub resolution: ResolutionState,
    /// `acceptance`; protocol §2.5.
    pub acceptance: Knowledge<Acceptance>,
    /// `settlement`; protocol §2.5.
    pub settlement: Knowledge<Settlement>,
    /// `queued_at`; protocol §2.5.
    pub queued_at: Timestamp,
    /// `accepted_at`; protocol §2.5.
    pub accepted_at: Knowledge<Timestamp>,
    /// `settled_at`; protocol §2.5.
    pub settled_at: Knowledge<Timestamp>,
    /// `expires_at`; protocol §2.5.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub expires_at: Option<Timestamp>,
    /// `node_receipt`; protocol §2.5.
    pub node_receipt: Knowledge<NodeReceipt>,
}
