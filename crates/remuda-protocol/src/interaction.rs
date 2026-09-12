//! Interaction wire declarations; `protocol.md`.

use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// InteractionRequestKey; `protocol.md` §2.6.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractionRequestKey {
    /// `native`; protocol §2.6.
    pub native: NativeRequestKey,
    /// `process_generation`; protocol §2.6.
    pub process_generation: U64,
    /// `run_generation`; protocol §2.6.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub run_generation: Option<U64>,
    /// `connection_epoch`; protocol §2.6.
    pub connection_epoch: Id,
}

/// CommittedAnswer; `protocol.md` §2.6.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommittedAnswer {
    /// `command_id`; protocol §2.6.
    pub command_id: CommandId,
    /// `actor`; protocol §2.6.
    pub actor: ActorRef,
    /// `value`; protocol §2.6.
    pub value: InteractionAnswer,
    /// `committed_at`; protocol §2.6.
    pub committed_at: Timestamp,
}

/// InteractionResolution; `protocol.md` §2.6.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractionResolution {
    /// `reason`; protocol §2.6.
    pub reason: InteractionResolutionReason,
    /// `event_ids`; protocol §2.6.
    pub event_ids: Vec<EventId>,
}

/// Interaction; `protocol.md` §2.6.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Interaction {
    /// `meta`; protocol §2.6.
    #[serde(flatten)]
    pub meta: EntityMeta<InteractionId>,
    /// `instance_id`; protocol §2.6.
    pub instance_id: InstanceId,
    /// `run_id`; protocol §2.6.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub run_id: Option<RunId>,
    /// `host_id`; protocol §2.6.
    pub host_id: HostId,
    /// `kind`; protocol §2.6.
    pub kind: InteractionKind,
    /// `request_key`; protocol §2.6.
    pub request_key: InteractionRequestKey,
    /// `request_version`; protocol §2.6.
    pub request_version: U64,
    /// `state`; protocol §2.6.
    pub state: InteractionState,
    /// `blocking`; protocol §2.6.
    pub blocking: bool,
    /// `answerable`; protocol §2.6.
    pub answerable: bool,
    /// `carrier`; protocol §2.6.
    pub carrier: InteractionCarrier,
    /// `request`; protocol §2.6.
    pub request: InteractionRequest,
    /// `deadline`; protocol §2.6.
    pub deadline: Knowledge<Timestamp>,
    /// `deadline_source`; protocol §2.6.
    pub deadline_source: DeadlineSource,
    /// `answer`; protocol §2.6.
    pub answer: Knowledge<CommittedAnswer>,
    /// `delivery`; protocol §2.6.
    pub delivery: DeliveryState,
    /// `resolution`; protocol §2.6.
    pub resolution: Knowledge<InteractionResolution>,
}

/// DecisionOption; `protocol.md` §5.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionOption {
    /// `id`; protocol §5.4.
    pub id: String,
    /// `label`; protocol §5.4.
    pub label: String,
    /// `effect`; protocol §5.4.
    pub effect: DecisionEffect,
    /// `native_value_ref`; protocol §5.4.
    pub native_value_ref: Id,
}

/// QuestionOption; `protocol.md` §5.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionOption {
    /// `id`; protocol §5.4.
    pub id: String,
    /// `label`; protocol §5.4.
    pub label: String,
}

/// QuestionField; `protocol.md` §5.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionField {
    /// `id`; protocol §5.4.
    pub id: String,
    /// `title`; protocol §5.4.
    pub title: String,
    /// `description`; protocol §5.4.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub description: Option<String>,
    /// `input`; protocol §5.4.
    pub input: QuestionInput,
    /// `required`; protocol §5.4.
    pub required: bool,
    /// `options`; protocol §5.4.
    pub options: Vec<QuestionOption>,
    /// `allow_free_text`; protocol §5.4.
    pub allow_free_text: bool,
    /// `sensitive`; protocol §5.4.
    pub sensitive: bool,
}

/// ApprovalRequest; `protocol.md` §5.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequest {
    /// `title`; protocol §5.4.
    pub title: String,
    /// `description`; protocol §5.4.
    pub description: String,
    /// `tool_call_id`; protocol §5.4.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub tool_call_id: Option<Id>,
    /// `action_ref`; protocol §5.4.
    pub action_ref: Id,
    /// `options`; protocol §5.4.
    pub options: Vec<DecisionOption>,
    /// `requested_permissions_ref`; protocol §5.4.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub requested_permissions_ref: Option<Id>,
    /// `input_digest`; protocol §5.4.
    pub input_digest: Digest,
}

/// QuestionRequest; `protocol.md` §5.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionRequest {
    /// `title`; protocol §5.4.
    pub title: String,
    /// `fields`; protocol §5.4.
    pub fields: Vec<QuestionField>,
}

/// PlanReviewRequest; `protocol.md` §5.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanReviewRequest {
    /// `title`; protocol §5.4.
    pub title: String,
    /// `plan_ref`; protocol §5.4.
    pub plan_ref: Id,
    /// `plan_revision`; protocol §5.4.
    pub plan_revision: U64,
    /// `plan_digest`; protocol §5.4.
    pub plan_digest: Digest,
    /// `options`; protocol §5.4.
    pub options: Vec<DecisionOption>,
    /// `allow_feedback`; protocol §5.4.
    pub allow_feedback: bool,
}

/// ElicitationRequest; `protocol.md` §5.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitationRequest {
    /// `title`; protocol §5.4.
    pub title: String,
    /// `mode`; protocol §5.4.
    pub mode: ElicitationMode,
    /// `schema_ref`; protocol §5.4.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub schema_ref: Option<Id>,
    /// `schema_dialect`; protocol §5.4.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub schema_dialect: Option<String>,
    /// `url`; protocol §5.4.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub url: Option<String>,
    /// `native_extension`; protocol §5.4.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub native_extension: Option<String>,
    /// `allowed_actions`; protocol §5.4.
    pub allowed_actions: Vec<ElicitationAction>,
}

/// InteractionRequest; `protocol.md` §5.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum InteractionRequest {
    /// `approval` payload; §5.4.
    #[serde(rename = "approval")]
    Approval(Box<ApprovalRequest>),
    /// `question` payload; §5.4.
    #[serde(rename = "question")]
    Question(Box<QuestionRequest>),
    /// `plan-review` payload; §5.4.
    #[serde(rename = "plan-review")]
    PlanReview(Box<PlanReviewRequest>),
    /// `elicitation` payload; §5.4.
    #[serde(rename = "elicitation")]
    Elicitation(Box<ElicitationRequest>),
}

/// QuestionFieldAnswer; `protocol.md` §5.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionFieldAnswer {
    /// `option_ids`; protocol §5.4.
    pub option_ids: Vec<String>,
    /// `text`; protocol §5.4.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub text: Option<String>,
}

/// ApprovalAnswer; `protocol.md` §5.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalAnswer {
    /// `option_id`; protocol §5.4.
    pub option_id: String,
    /// `input_digest`; protocol §5.4.
    pub input_digest: Digest,
}

/// QuestionAnswer; `protocol.md` §5.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionAnswer {
    /// `answers`; protocol §5.4.
    pub answers: BTreeMap<String, QuestionFieldAnswer>,
}

/// PlanReviewAnswer; `protocol.md` §5.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanReviewAnswer {
    /// `option_id`; protocol §5.4.
    pub option_id: String,
    /// `plan_revision`; protocol §5.4.
    pub plan_revision: U64,
    /// `plan_digest`; protocol §5.4.
    pub plan_digest: Digest,
    /// `feedback`; protocol §5.4.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub feedback: Option<String>,
}

/// ElicitationAnswer; `protocol.md` §5.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitationAnswer {
    /// `action`; protocol §5.4.
    pub action: ElicitationAction,
    /// `content`; protocol §5.4.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub content: Option<serde_json::Value>,
}

/// InteractionAnswer; `protocol.md` §5.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum InteractionAnswer {
    /// `approval` payload; §5.4.
    #[serde(rename = "approval")]
    Approval(Box<ApprovalAnswer>),
    /// `question` payload; §5.4.
    #[serde(rename = "question")]
    Question(Box<QuestionAnswer>),
    /// `plan-review` payload; §5.4.
    #[serde(rename = "plan-review")]
    PlanReview(Box<PlanReviewAnswer>),
    /// `elicitation` payload; §5.4.
    #[serde(rename = "elicitation")]
    Elicitation(Box<ElicitationAnswer>),
}
