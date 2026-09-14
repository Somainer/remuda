//! Observation wire declarations; `protocol.md`.

use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// StreamCursor; `protocol.md` §5.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StreamCursor {
    /// `connection_epoch`; protocol §5.1.
    pub connection_epoch: Id,
    /// `frame`; protocol §5.1.
    pub frame: U64,
}

/// FileCursor; `protocol.md` §5.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileCursor {
    /// `file_identity`; protocol §5.1.
    pub file_identity: Id,
    /// `file_generation`; protocol §5.1.
    pub file_generation: U64,
    /// `offset`; protocol §5.1.
    pub offset: U64,
    /// `length`; protocol §5.1.
    pub length: U64,
    /// `digest`; protocol §5.1.
    pub digest: Digest,
}

/// HookCursor; `protocol.md` §5.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HookCursor {
    /// `invocation_id`; protocol §5.1.
    pub invocation_id: Id,
    /// `frame`; protocol §5.1.
    pub frame: U64,
}

/// TtyCursor; `protocol.md` §5.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TtyCursor {
    /// `stream_id`; protocol §5.1.
    pub stream_id: Id,
    /// `offset`; protocol §5.1.
    pub offset: U64,
    /// `length`; protocol §5.1.
    pub length: U64,
}

/// RuntimeCursor; `protocol.md` §5.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeCursor {
    /// `ledger_revision`; protocol §5.1.
    pub ledger_revision: U64,
}

/// SourceCursor; `protocol.md` §5.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type")]
pub enum SourceCursor {
    /// `stream` payload; §5.1.
    #[serde(rename = "stream")]
    Stream(Box<StreamCursor>),
    /// `file` payload; §5.1.
    #[serde(rename = "file")]
    File(Box<FileCursor>),
    /// `hook` payload; §5.1.
    #[serde(rename = "hook")]
    Hook(Box<HookCursor>),
    /// `tty` payload; §5.1.
    #[serde(rename = "tty")]
    Tty(Box<TtyCursor>),
    /// `runtime` payload; §5.1.
    #[serde(rename = "runtime")]
    Runtime(Box<RuntimeCursor>),
}

/// ObservationSource; `protocol.md` §5.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObservationSource {
    /// `driver_kind`; protocol §5.1.
    pub driver_kind: DriverKind,
    /// `driver_version`; protocol §5.1.
    pub driver_version: String,
    /// `adapter_version`; protocol §5.1.
    pub adapter_version: String,
    /// `channel`; protocol §5.1.
    pub channel: SourceChannel,
    /// `delivery`; protocol §5.1.
    pub delivery: SourceDelivery,
    /// `native_session_id`; protocol §5.1.
    pub native_session_id: Knowledge<String>,
    /// `native_turn_id`; protocol §5.1.
    pub native_turn_id: Knowledge<String>,
    /// `native_agent_id`; protocol §5.1.
    pub native_agent_id: Knowledge<String>,
    /// `native_item_id`; protocol §5.1.
    pub native_item_id: Knowledge<String>,
    /// `native_event_id`; protocol §5.1.
    pub native_event_id: Knowledge<String>,
    /// `native_request_id`; protocol §5.1.
    pub native_request_id: NativeRequestKey,
    /// `source_cursor`; protocol §5.1.
    pub source_cursor: SourceCursor,
}

/// RawRef; `protocol.md` §5.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RawRef {
    /// `object_id`; protocol §5.1.
    pub object_id: Id,
    /// `offset`; protocol §5.1.
    pub offset: U64,
    /// `length`; protocol §5.1.
    pub length: U64,
    /// `digest`; protocol §5.1.
    pub digest: Digest,
    /// `media_type`; protocol §5.1.
    pub media_type: String,
    /// `redaction`; protocol §5.1.
    pub redaction: Redaction,
}

/// TextBlock; `protocol.md` §5.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TextBlock {
    /// `text`; protocol §5.2.
    pub text: String,
}

/// MediaBlock; `protocol.md` §5.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaBlock {
    /// `object_id`; protocol §5.2.
    pub object_id: Id,
    /// `media_type`; protocol §5.2.
    pub media_type: String,
    /// `name`; protocol §5.2.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub name: Option<String>,
}

/// ResourceBlock; `protocol.md` §5.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResourceBlock {
    /// `uri`; protocol §5.2.
    pub uri: String,
    /// `media_type`; protocol §5.2.
    pub media_type: Knowledge<String>,
    /// `object_id`; protocol §5.2.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub object_id: Option<Id>,
}

/// OpaqueBlock; `protocol.md` §5.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OpaqueBlock {
    /// `raw_ref`; protocol §5.2.
    pub raw_ref: RawRef,
    /// `native_type`; protocol §5.2.
    pub native_type: String,
}

/// ContentBlock; `protocol.md` §5.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type")]
pub enum ContentBlock {
    /// `text` payload; §5.2.
    #[serde(rename = "text")]
    Text(Box<TextBlock>),
    /// `image` payload; §5.2.
    #[serde(rename = "image")]
    Image(Box<MediaBlock>),
    /// `audio` payload; §5.2.
    #[serde(rename = "audio")]
    Audio(Box<MediaBlock>),
    /// `file` payload; §5.2.
    #[serde(rename = "file")]
    File(Box<MediaBlock>),
    /// `resource` payload; §5.2.
    #[serde(rename = "resource")]
    Resource(Box<ResourceBlock>),
    /// `opaque` payload; §5.2.
    #[serde(rename = "opaque")]
    Opaque(Box<OpaqueBlock>),
}

/// NodeMutation; `protocol.md` §5.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeMutation {
    /// `node_id`; protocol §5.2.
    pub node_id: Id,
    /// `revision`; protocol §5.2.
    pub revision: U64,
    /// `operation`; protocol §5.2.
    pub operation: MutationOperation,
    /// `base_revision`; protocol §5.2.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub base_revision: Option<U64>,
}

/// MessagePayload; `protocol.md` §5.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessagePayload {
    /// `mutation`; protocol §5.2.
    #[serde(flatten)]
    pub mutation: NodeMutation,
    /// `message_id`; protocol §5.2.
    pub message_id: Id,
    /// `role`; protocol §5.2.
    pub role: MessageRole,
    /// `phase`; protocol §5.2.
    pub phase: MessagePhase,
    /// `blocks`; protocol §5.2.
    pub blocks: Vec<ContentBlock>,
    /// `target_block`; protocol §5.2.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub target_block: Option<u32>,
    /// `parent_tool_call_id`; protocol §5.2.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub parent_tool_call_id: Option<Id>,
    /// `native_origin`; protocol §5.2.
    pub native_origin: Knowledge<String>,
    /// `origin`; protocol §5.2 (D-028 P3). Additive: absent from pre-D-028
    /// producers, which is why it is an `Option` rather than a defaulted enum
    /// — the schema stays honest about what is actually on the wire.
    ///
    /// Who authored this record. A Claude transcript files skill bodies,
    /// slash-command expansions, hook context and task notifications as `user`
    /// records, so `role` alone cannot tell the human's words from text the
    /// harness injected on their behalf. Rendering all of them as the user's
    /// own is both duplicated and misleading, so the classification travels
    /// with the message instead of being re-guessed by every client.
    ///
    /// `None` means "this producer does not classify", and must render like
    /// [`MessageOrigin::Human`]: showing one row too many is recoverable,
    /// silently hiding what someone said is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
    /// `command_id`; protocol §5.2 (C2). Set only when this prompt text was
    /// delivered through a Remuda command: the Node attaches the delivering
    /// command's id to the next matching prompt observation (the hook
    /// `UserPromptSubmit` turn event carries it in `related_ids`, and the
    /// transcript user record carries it here and joins onto the queued
    /// message's node). Additive exactly like `origin`: a prompt typed
    /// natively into the PTY has no command and serialises without this
    /// field, which clients must read as "human typed, no command".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<CommandId>,
    /// `status`; protocol §5.2.
    pub status: ContentStatus,
}

impl MessagePayload {
    /// The classified author, treating an unclassified message as the human's.
    #[must_use]
    pub fn origin_or_human(&self) -> MessageOrigin {
        self.origin.unwrap_or(MessageOrigin::Human)
    }
}

/// ThoughtPayload; `protocol.md` §5.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThoughtPayload {
    /// `mutation`; protocol §5.2.
    #[serde(flatten)]
    pub mutation: NodeMutation,
    /// `thought_id`; protocol §5.2.
    pub thought_id: Id,
    /// `representation`; protocol §5.2.
    pub representation: ThoughtRepresentation,
    /// `text`; protocol §5.2.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub text: Option<String>,
    /// `part_index`; protocol §5.2.
    pub part_index: u32,
    /// `status`; protocol §5.2.
    pub status: ContentStatus,
}

/// ExecutorRef; `protocol.md` §5.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorRef {
    /// `host_id`; protocol §5.2.
    pub host_id: HostId,
    /// `workspace_id`; protocol §5.2.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub workspace_id: Option<WorkspaceId>,
    /// `native_agent_id`; protocol §5.2.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub native_agent_id: Option<String>,
}

/// ToolCallPayload; `protocol.md` §5.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallPayload {
    /// `mutation`; protocol §5.2.
    #[serde(flatten)]
    pub mutation: NodeMutation,
    /// `tool_call_id`; protocol §5.2.
    pub tool_call_id: Id,
    /// `parent_tool_call_id`; protocol §5.2.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub parent_tool_call_id: Option<Id>,
    /// `tool_name`; protocol §5.2.
    pub tool_name: Knowledge<String>,
    /// `display_title`; protocol §5.2.
    pub display_title: Knowledge<String>,
    /// `category`; protocol §5.2.
    pub category: ToolCategory,
    /// `input`; protocol §5.2.
    pub input: Knowledge<serde_json::Value>,
    /// `input_text_delta`; protocol §5.2.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub input_text_delta: Option<String>,
    /// `state`; protocol §5.2.
    pub state: ToolCallState,
    /// `executor`; protocol §5.2.
    pub executor: Knowledge<ExecutorRef>,
}

/// FileChange; `protocol.md` §5.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    /// `path`; protocol §5.2.
    pub path: String,
    /// `diff`; protocol §5.2.
    pub diff: String,
    /// `application`; protocol §5.2.
    pub application: ChangeApplication,
}

/// ToolResultPayload; `protocol.md` §5.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultPayload {
    /// `mutation`; protocol §5.2.
    #[serde(flatten)]
    pub mutation: NodeMutation,
    /// `tool_call_id`; protocol §5.2.
    pub tool_call_id: Id,
    /// `stage`; protocol §5.2.
    pub stage: ResultStage,
    /// `outcome`; protocol §5.2.
    pub outcome: ToolOutcome,
    /// `blocks`; protocol §5.2.
    pub blocks: Vec<ContentBlock>,
    /// `structured_result`; protocol §5.2.
    pub structured_result: Knowledge<serde_json::Value>,
    /// `exit_code`; protocol §5.2.
    pub exit_code: Knowledge<i32>,
    /// `changes`; protocol §5.2.
    pub changes: Vec<FileChange>,
}

/// WorkflowRunPayload; `protocol.md` §5.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunPayload {
    /// `workflow_id`; protocol §5.3.
    pub workflow_id: Id,
    /// `engine`; protocol §5.3.
    pub engine: WorkflowEngine,
    /// `native_run_id`; protocol §5.3.
    pub native_run_id: Knowledge<String>,
    /// `native_task_id`; protocol §5.3.
    pub native_task_id: Knowledge<String>,
    /// `tool_call_id`; protocol §5.3.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub tool_call_id: Option<Id>,
    /// `state`; protocol §5.3.
    pub state: WorkflowState,
    /// `revision`; protocol §5.3.
    pub revision: U64,
    /// `title`; protocol §5.3.
    pub title: Knowledge<String>,
    /// `result_ref`; protocol §5.3.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub result_ref: Option<Id>,
}

/// WorkflowPhasePayload; `protocol.md` §5.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowPhasePayload {
    /// `workflow_id`; protocol §5.3.
    pub workflow_id: Id,
    /// `phase_id`; protocol §5.3.
    pub phase_id: Id,
    /// `native_phase_id`; protocol §5.3.
    pub native_phase_id: Knowledge<String>,
    /// `label`; protocol §5.3.
    pub label: Knowledge<String>,
    /// `state`; protocol §5.3.
    pub state: WorkflowState,
    /// `revision`; protocol §5.3.
    pub revision: U64,
    /// `parent_phase_id`; protocol §5.3.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub parent_phase_id: Option<Id>,
}

/// WorkflowMemberPayload; `protocol.md` §5.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowMemberPayload {
    /// `workflow_id`; protocol §5.3.
    pub workflow_id: Id,
    /// `member_id`; protocol §5.3.
    pub member_id: Id,
    /// `native_agent_id`; protocol §5.3.
    pub native_agent_id: Knowledge<String>,
    /// `native_key`; protocol §5.3.
    pub native_key: Knowledge<String>,
    /// `attempt`; protocol §5.3.
    pub attempt: Knowledge<U64>,
    /// `phase_id`; protocol §5.3.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub phase_id: Option<Id>,
    /// `label`; protocol §5.3.
    pub label: Knowledge<String>,
    /// `state`; protocol §5.3.
    pub state: WorkflowState,
    /// `model_requested`; protocol §5.3.
    pub model_requested: Knowledge<String>,
    /// `model_resolved`; protocol §5.3.
    pub model_resolved: Knowledge<String>,
    /// `result_ref`; protocol §5.3.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub result_ref: Option<Id>,
    /// `revision`; protocol §5.3.
    pub revision: U64,
}

/// InteractionRequestedPayload; `protocol.md` §5.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InteractionRequestedPayload {
    /// `interaction`; protocol §5.4.
    pub interaction: Interaction,
}

/// InteractionAnsweredPayload; `protocol.md` §5.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InteractionAnsweredPayload {
    /// `interaction_id`; protocol §5.4.
    pub interaction_id: InteractionId,
    /// `request_version`; protocol §5.4.
    pub request_version: U64,
    /// `answer_command_id`; protocol §5.4.
    pub answer_command_id: CommandId,
    /// `actor`; protocol §5.4.
    pub actor: ActorRef,
    /// `answer_ref`; protocol §5.4.
    pub answer_ref: Id,
    /// `delivery`; protocol §5.4.
    pub delivery: DeliveryState,
}

/// InteractionExpiredPayload; `protocol.md` §5.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InteractionExpiredPayload {
    /// `interaction_id`; protocol §5.4.
    pub interaction_id: InteractionId,
    /// `request_version`; protocol §5.4.
    pub request_version: U64,
    /// `reason`; protocol §5.4.
    pub reason: InteractionExpiredReason,
    /// `evidence_event_ids`; protocol §5.4.
    pub evidence_event_ids: Vec<EventId>,
}

/// LifecycleEntity; `protocol.md` §5.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "entityType", content = "entity")]
pub enum LifecycleEntity {
    /// `host` payload; §5.5.
    #[serde(rename = "host")]
    Host(Box<Host>),
    /// `workspace` payload; §5.5.
    #[serde(rename = "workspace")]
    Workspace(Box<Workspace>),
    /// `instance` payload; §5.5.
    #[serde(rename = "instance")]
    Instance(Box<Instance>),
    /// `run` payload; §5.5.
    #[serde(rename = "run")]
    Run(Box<Run>),
    /// `command` payload; §5.5.
    #[serde(rename = "command")]
    Command(Box<Command>),
    /// `interaction` payload; §5.5.
    #[serde(rename = "interaction")]
    Interaction(Box<Interaction>),
}

/// EntityLifecycle; `protocol.md` §5.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EntityLifecycle {
    /// `entity_id`; protocol §5.5.
    pub entity_id: Id,
    /// `revision`; protocol §5.5.
    pub revision: U64,
    /// `previous_state`; protocol §5.5.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub previous_state: Option<String>,
    /// `state`; protocol §5.5.
    pub state: String,
    /// `reason_code`; protocol §5.5.
    pub reason_code: String,
    /// `evidence_event_ids`; protocol §5.5.
    pub evidence_event_ids: Vec<EventId>,
    /// `entity_value`; protocol §5.5.
    #[serde(flatten)]
    pub entity_value: LifecycleEntity,
}

// TODO(protocol §5.5): lifecycle state mirrors the tagged entity; dispatcher/journal must validate identity, revision, and state together before committing.

/// NativeLifecycle; `protocol.md` §5.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NativeLifecycle {
    /// `topic`; protocol §5.5.
    pub topic: LifecycleTopic,
    /// `native_name`; protocol §5.5.
    pub native_name: String,
    /// `native_id`; protocol §5.5.
    pub native_id: Knowledge<String>,
    /// `status`; protocol §5.5.
    pub status: Knowledge<String>,
    /// `related_ids`; protocol §5.5.
    pub related_ids: BTreeMap<String, String>,
    /// `data_ref`; protocol §5.5.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub data_ref: Option<Id>,
    /// `severity`; protocol §5.5.
    pub severity: Severity,
    /// `affects_completion`; protocol §5.5.
    pub affects_completion: bool,
}

/// LifecyclePayload; `protocol.md` §5.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type")]
pub enum LifecyclePayload {
    /// `entity` payload; §5.5.
    #[serde(rename = "entity")]
    Entity(Box<EntityLifecycle>),
    /// `native` payload; §5.5.
    #[serde(rename = "native")]
    Native(Box<NativeLifecycle>),
}

/// Cost; `protocol.md` §5.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    /// `amount`; protocol §5.5.
    pub amount: String,
    /// `currency`; protocol §5.5.
    pub currency: String,
}

/// UsagePayload; `protocol.md` §5.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UsagePayload {
    /// `usage_id`; protocol §5.5.
    pub usage_id: Id,
    /// `scope`; protocol §5.5.
    pub scope: UsageScope,
    /// `scope_id`; protocol §5.5.
    pub scope_id: String,
    /// `mode`; protocol §5.5.
    pub mode: UsageMode,
    /// `metric_revision`; protocol §5.5.
    pub metric_revision: U64,
    /// `input_tokens`; protocol §5.5.
    pub input_tokens: Knowledge<U64>,
    /// `input_accounting`; protocol §5.5.
    pub input_accounting: InputAccounting,
    /// `output_tokens`; protocol §5.5.
    pub output_tokens: Knowledge<U64>,
    /// `reasoning_tokens`; protocol §5.5.
    pub reasoning_tokens: Knowledge<U64>,
    /// `cache_read_tokens`; protocol §5.5.
    pub cache_read_tokens: Knowledge<U64>,
    /// `cache_write_tokens`; protocol §5.5.
    pub cache_write_tokens: Knowledge<U64>,
    /// `total_tokens`; protocol §5.5.
    pub total_tokens: Knowledge<U64>,
    /// `cost`; protocol §5.5.
    pub cost: Knowledge<Cost>,
    /// `accounting`; protocol §5.5.
    pub accounting: Accounting,
    /// `native_fields_ref`; protocol §5.5.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub native_fields_ref: Option<Id>,
}

/// BlobLocator; `protocol.md` §5.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BlobLocator {
    /// `object_id`; protocol §5.5.
    pub object_id: Id,
    /// `digest`; protocol §5.5.
    pub digest: Digest,
}

/// WorkspaceFileLocator; `protocol.md` §5.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFileLocator {
    /// `workspace_id`; protocol §5.5.
    pub workspace_id: WorkspaceId,
    /// `relative_path`; protocol §5.5.
    pub relative_path: String,
    /// `revision`; protocol §5.5.
    pub revision: U64,
    /// `digest`; protocol §5.5.
    pub digest: Knowledge<Digest>,
}

/// NativeLocator; `protocol.md` §5.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NativeLocator {
    /// `native_uri`; protocol §5.5.
    pub native_uri: String,
}

/// UrlLocator; `protocol.md` §5.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UrlLocator {
    /// `url`; protocol §5.5.
    pub url: String,
}

/// ArtifactLocator; `protocol.md` §5.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type")]
pub enum ArtifactLocator {
    /// `blob` payload; §5.5.
    #[serde(rename = "blob")]
    Blob(Box<BlobLocator>),
    /// `workspace-file` payload; §5.5.
    #[serde(rename = "workspace-file")]
    WorkspaceFile(Box<WorkspaceFileLocator>),
    /// `native` payload; §5.5.
    #[serde(rename = "native")]
    Native(Box<NativeLocator>),
    /// `url` payload; §5.5.
    #[serde(rename = "url")]
    Url(Box<UrlLocator>),
}

/// ArtifactPayload; `protocol.md` §5.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactPayload {
    /// `artifact_id`; protocol §5.5.
    pub artifact_id: Id,
    /// `revision`; protocol §5.5.
    pub revision: U64,
    /// `action`; protocol §5.5.
    pub action: ArtifactAction,
    /// `actor_type`; protocol §5.5.
    #[serde(rename = "type")]
    pub actor_type: ArtifactType,
    /// `title`; protocol §5.5.
    pub title: Knowledge<String>,
    /// `media_type`; protocol §5.5.
    pub media_type: Knowledge<String>,
    /// `size_bytes`; protocol §5.5.
    pub size_bytes: Knowledge<U64>,
    /// `locator`; protocol §5.5.
    pub locator: ArtifactLocator,
    /// `producer_tool_call_id`; protocol §5.5.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub producer_tool_call_id: Option<Id>,
    /// `verification`; protocol §5.5.
    pub verification: ArtifactVerification,
}

/// NativeTerminalFrame; `protocol.md` §5.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NativeTerminalFrame {
    /// `seq`; protocol §5.5.
    pub seq: U64,
    /// `width`; protocol §5.5.
    pub width: u16,
    /// `height`; protocol §5.5.
    pub height: u16,
    /// `full`; protocol §5.5.
    pub full: bool,
}

/// TtyOutput; `protocol.md` §5.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TtyOutput {
    /// `stream_id`; protocol §5.5.
    pub stream_id: Id,
    /// `stream_epoch`; protocol §5.5.
    pub stream_epoch: Id,
    /// `representation`; protocol §5.5.
    pub representation: TtyRepresentation,
    /// `offset`; protocol §5.5.
    pub offset: U64,
    /// `byte_length`; protocol §5.5.
    pub byte_length: U64,
    /// `data_ref`; protocol §5.5.
    pub data_ref: RawRef,
    /// `native_frame`; protocol §5.5.
    pub native_frame: Knowledge<NativeTerminalFrame>,
}

/// TtyInput; `protocol.md` §5.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TtyInput {
    /// `stream_id`; protocol §5.5.
    pub stream_id: Id,
    /// `stream_epoch`; protocol §5.5.
    pub stream_epoch: Id,
    /// `input_id`; protocol §5.5.
    pub input_id: Id,
    /// `byte_length`; protocol §5.5.
    pub byte_length: U64,
    /// `data_ref`; protocol §5.5.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub data_ref: Option<RawRef>,
    /// `actor`; protocol §5.5.
    pub actor: ActorRef,
    /// `delivery`; protocol §5.5.
    pub delivery: TtyInputDelivery,
}

/// TtyResize; `protocol.md` §5.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TtyResize {
    /// `stream_id`; protocol §5.5.
    pub stream_id: Id,
    /// `stream_epoch`; protocol §5.5.
    pub stream_epoch: Id,
    /// `cols`; protocol §5.5.
    pub cols: u16,
    /// `rows`; protocol §5.5.
    pub rows: u16,
    /// `resize_revision`; protocol §5.5.
    pub resize_revision: U64,
}

/// RawTtyPayload; `protocol.md` §5.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "direction")]
pub enum RawTtyPayload {
    /// `output` payload; §5.5.
    #[serde(rename = "output")]
    Output(Box<TtyOutput>),
    /// `input` payload; §5.5.
    #[serde(rename = "input")]
    Input(Box<TtyInput>),
    /// `resize` payload; §5.5.
    #[serde(rename = "resize")]
    Resize(Box<TtyResize>),
}

/// OpaquePayload; `protocol.md` §5.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OpaquePayload {
    /// `native_type`; protocol §5.5.
    pub native_type: String,
    /// `reason`; protocol §5.5.
    pub reason: OpaqueReason,
    /// `raw_ref`; protocol §5.5.
    pub raw_ref: RawRef,
    /// `affects`; protocol §5.5.
    pub affects: Vec<OpaqueImpact>,
    /// `summary`; protocol §5.5.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub summary: Option<String>,
}

/// ObservationPayload; `protocol.md` §5.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", content = "payload")]
pub enum ObservationPayload {
    /// `message` payload; §5.1.
    #[serde(rename = "message")]
    Message(Box<MessagePayload>),
    /// `thought` payload; §5.1.
    #[serde(rename = "thought")]
    Thought(Box<ThoughtPayload>),
    /// `tool_call` payload; §5.1.
    #[serde(rename = "tool_call")]
    ToolCall(Box<ToolCallPayload>),
    /// `tool_result` payload; §5.1.
    #[serde(rename = "tool_result")]
    ToolResult(Box<ToolResultPayload>),
    /// `interaction.requested` payload; §5.1.
    #[serde(rename = "interaction.requested")]
    InteractionRequested(Box<InteractionRequestedPayload>),
    /// `interaction.answered` payload; §5.1.
    #[serde(rename = "interaction.answered")]
    InteractionAnswered(Box<InteractionAnsweredPayload>),
    /// `interaction.expired` payload; §5.1.
    #[serde(rename = "interaction.expired")]
    InteractionExpired(Box<InteractionExpiredPayload>),
    /// `workflow.run` payload; §5.1.
    #[serde(rename = "workflow.run")]
    WorkflowRun(Box<WorkflowRunPayload>),
    /// `workflow.phase` payload; §5.1.
    #[serde(rename = "workflow.phase")]
    WorkflowPhase(Box<WorkflowPhasePayload>),
    /// `workflow.member` payload; §5.1.
    #[serde(rename = "workflow.member")]
    WorkflowMember(Box<WorkflowMemberPayload>),
    /// `lifecycle` payload; §5.1.
    #[serde(rename = "lifecycle")]
    Lifecycle(Box<LifecyclePayload>),
    /// `usage` payload; §5.1.
    #[serde(rename = "usage")]
    Usage(Box<UsagePayload>),
    /// `artifact` payload; §5.1.
    #[serde(rename = "artifact")]
    Artifact(Box<ArtifactPayload>),
    /// `raw_tty` payload; §5.1.
    #[serde(rename = "raw_tty")]
    RawTty(Box<RawTtyPayload>),
    /// `opaque` payload; §5.1.
    #[serde(rename = "opaque")]
    Opaque(Box<OpaquePayload>),
}

impl ObservationPayload {
    /// Discriminant serialized alongside `payload`; protocol §5.1.
    pub fn kind(&self) -> ObservationKind {
        match self {
            Self::Message(..) => ObservationKind::Message,
            Self::Thought(..) => ObservationKind::Thought,
            Self::ToolCall(..) => ObservationKind::ToolCall,
            Self::ToolResult(..) => ObservationKind::ToolResult,
            Self::InteractionRequested(..) => ObservationKind::InteractionRequested,
            Self::InteractionAnswered(..) => ObservationKind::InteractionAnswered,
            Self::InteractionExpired(..) => ObservationKind::InteractionExpired,
            Self::WorkflowRun(..) => ObservationKind::WorkflowRun,
            Self::WorkflowPhase(..) => ObservationKind::WorkflowPhase,
            Self::WorkflowMember(..) => ObservationKind::WorkflowMember,
            Self::Lifecycle(..) => ObservationKind::Lifecycle,
            Self::Usage(..) => ObservationKind::Usage,
            Self::Artifact(..) => ObservationKind::Artifact,
            Self::RawTty(..) => ObservationKind::RawTty,
            Self::Opaque(..) => ObservationKind::Opaque,
        }
    }
}

/// Observation; `protocol.md` §5.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Observation {
    /// `schema_version`; protocol §5.1.
    pub schema_version: SchemaVersion,
    /// `event_id`; protocol §5.1.
    pub event_id: EventId,
    /// `journal_id`; protocol §5.1.
    pub journal_id: Id,
    /// `instance_id`; protocol §5.1.
    pub instance_id: InstanceId,
    /// `run_id`; protocol §5.1.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub run_id: Option<RunId>,
    /// `host_id`; protocol §5.1.
    pub host_id: HostId,
    /// `process_generation`; protocol §5.1.
    pub process_generation: U64,
    /// `run_generation`; protocol §5.1.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub run_generation: Option<U64>,
    /// `seq`; protocol §5.1.
    pub seq: U64,
    /// `observed_at`; protocol §5.1.
    pub observed_at: Timestamp,
    /// `native_at`; protocol §5.1.
    pub native_at: Knowledge<Timestamp>,
    /// `source`; protocol §5.1.
    pub source: ObservationSource,
    /// `completeness`; protocol §5.1.
    pub completeness: Completeness,
    /// `raw_ref`; protocol §5.1.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub raw_ref: Option<RawRef>,
    /// `evidence_event_ids`; protocol §5.1.
    pub evidence_event_ids: Vec<EventId>,
    /// `body`; protocol §5.1.
    #[serde(flatten)]
    pub body: ObservationPayload,
}

/// RegistryEvent; `protocol.md` §5.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RegistryEvent {
    /// `schema_version`; protocol §5.5.
    pub schema_version: SchemaVersion,
    /// `event_id`; protocol §5.5.
    pub event_id: EventId,
    /// `host_id`; protocol §5.5.
    pub host_id: HostId,
    /// `journal_id`; protocol §5.5.
    pub journal_id: Id,
    /// `seq`; protocol §5.5.
    pub seq: U64,
    /// `observed_at`; protocol §5.5.
    pub observed_at: Timestamp,
    /// `kind`; protocol §5.5.
    pub kind: RegistryKind,
    /// `payload`; protocol §5.5.
    pub payload: LifecyclePayload,
}

/// Instance or registry event; no fallback from a malformed instance event; §7.3.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum JournalEvent {
    /// An instance journal entry.
    Instance(Box<Observation>),
    /// A host or workspace registry entry.
    Registry(Box<RegistryEvent>),
}
impl<'de> Deserialize<'de> for JournalEvent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        if value.get("instanceId").is_some() {
            serde_json::from_value(value)
                .map(Box::new)
                .map(Self::Instance)
                .map_err(serde::de::Error::custom)
        } else {
            serde_json::from_value(value)
                .map(Box::new)
                .map(Self::Registry)
                .map_err(serde::de::Error::custom)
        }
    }
}
impl JournalEvent {
    /// Journal identity, sequence, and event ID used for continuity checks; §7.3.
    pub fn position(&self) -> (&Id, U64, &EventId) {
        match self {
            Self::Instance(event) => (&event.journal_id, event.seq, &event.event_id),
            Self::Registry(event) => (&event.journal_id, event.seq, &event.event_id),
        }
    }
}

impl schemars::JsonSchema for JournalEvent {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "JournalEvent".into()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"oneOf":[
            generator.subschema_for::<Observation>(),
            {"allOf":[generator.subschema_for::<RegistryEvent>(), {"not":{"required":["instanceId"]}}]}
        ]})
    }
}
