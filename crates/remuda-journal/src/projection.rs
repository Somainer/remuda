//! Pure-function projections over an instance journal.

use remuda_protocol::{
    Activity, Connectivity, ContentBlock, Id, InstanceLifecycle, InteractionId, InteractionState,
    Knowledge, LifecycleEntity, LifecyclePayload, LifecycleTopic, MessagePhase, MessageRole,
    Observation, ObservationPayload, SourceDelivery, ToolCallState, ToolOutcome, U64,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Kind of transcript turn boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TurnKind {
    /// Started by a user input message.
    User,
    /// Compact / compaction boundary.
    Compact,
}

/// Inclusive seq span covering one user turn or a compact fold window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnBound {
    /// First seq in the turn.
    pub start_seq: U64,
    /// Last seq if the turn has closed.
    pub end_seq: Option<U64>,
    /// Why this bound exists.
    pub kind: TurnKind,
}

/// Message / thought / tool item in transcript order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptEntry {
    /// A user, assistant, or system message node.
    Message {
        /// Journal seq that opened or last replaced this node.
        seq: U64,
        /// Stable message node id.
        message_id: Id,
        /// Speaker.
        role: MessageRole,
        /// Concatenated text blocks.
        text: String,
    },
    /// An assistant thought node.
    Thought {
        /// Journal seq.
        seq: U64,
        /// Thought node id.
        thought_id: Id,
        /// Visible text when present.
        text: Option<String>,
    },
    /// A tool invocation.
    ToolCall {
        /// Journal seq.
        seq: U64,
        /// Tool call node id.
        tool_call_id: Id,
        /// Native or mapped name when known.
        name: Option<String>,
        /// Call state.
        state: ToolCallState,
    },
    /// A tool result paired to a call.
    ToolResult {
        /// Journal seq.
        seq: U64,
        /// Tool call node id.
        tool_call_id: Id,
        /// Outcome when structured.
        outcome: ToolOutcome,
        /// Concatenated text blocks.
        text: String,
    },
}

/// Fold of messages, thoughts, and tool_call/tool_result pairs.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptProjection {
    /// Highest applied seq.
    pub as_of_seq: U64,
    /// Ordered transcript items.
    pub entries: Vec<TranscriptEntry>,
    /// User-turn and compact bounds.
    pub turns: Vec<TurnBound>,
    /// Tool pairs keyed by `toolCallId`.
    pub tools: BTreeMap<String, ToolPair>,
}

/// Pairing of a tool call with its latest result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolPair {
    /// Seq of the call.
    pub call_seq: U64,
    /// Seq of the result, if observed.
    pub result_seq: Option<U64>,
    /// Tool call id.
    pub tool_call_id: Id,
    /// Name when known.
    pub name: Option<String>,
    /// Result outcome when known.
    pub outcome: Option<ToolOutcome>,
}

/// First-writer-wins interaction state keyed by request key.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractionProjection {
    /// Records keyed by serialized `InteractionRequestKey`.
    pub by_key: BTreeMap<String, InteractionRecord>,
    /// Request-key lookup keyed by interaction id.
    pub by_id: BTreeMap<String, String>,
}

/// One interaction as folded from requested/answered/expired observations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractionRecord {
    /// Interaction identity from the first request.
    pub interaction_id: InteractionId,
    /// Serialized request key.
    pub request_key: String,
    /// Folded state.
    pub state: InteractionState,
    /// Seq of the first request.
    pub requested_seq: U64,
    /// Seq of the first answer, if any.
    pub answered_seq: Option<U64>,
    /// Seq of expiry, if any.
    pub expired_seq: Option<U64>,
}

/// Lifecycle × activity × connectivity for an instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusProjection {
    /// Instance lifecycle.
    pub lifecycle: InstanceLifecycle,
    /// Current activity.
    pub activity: Activity,
    /// Current connectivity.
    pub connectivity: Connectivity,
    /// Last native/driver error when the instance failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl Default for StatusProjection {
    fn default() -> Self {
        Self {
            lifecycle: InstanceLifecycle::Unknown,
            activity: Activity::Idle,
            connectivity: Connectivity::Disconnected,
            last_error: None,
        }
    }
}

/// Combined projections produced by `snapshot`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Projections {
    /// Transcript fold.
    pub transcript: TranscriptProjection,
    /// Interaction fold.
    pub interaction: InteractionProjection,
    /// Status fold.
    pub status: StatusProjection,
}

impl Projections {
    /// Apply one committed observation.
    pub fn apply(&mut self, obs: &Observation) {
        self.transcript.apply(obs);
        self.interaction.apply(obs);
        self.status.apply(obs);
    }
}

impl TranscriptProjection {
    fn apply(&mut self, obs: &Observation) {
        self.as_of_seq = obs.seq;
        match &obs.body {
            ObservationPayload::Message(payload) => {
                let text = blocks_text(&payload.blocks);
                if payload.role == MessageRole::User && payload.phase == MessagePhase::Input {
                    self.open_turn(obs.seq, TurnKind::User);
                }
                self.entries.push(TranscriptEntry::Message {
                    seq: obs.seq,
                    message_id: payload.message_id.clone(),
                    role: payload.role,
                    text,
                });
            }
            ObservationPayload::Thought(payload) => {
                self.entries.push(TranscriptEntry::Thought {
                    seq: obs.seq,
                    thought_id: payload.thought_id.clone(),
                    text: payload.text.clone(),
                });
            }
            ObservationPayload::ToolCall(payload) => {
                let name = match &payload.tool_name {
                    Knowledge::Known { value } => Some(value.clone()),
                    _ => None,
                };
                let key = payload.tool_call_id.as_str().to_owned();
                self.tools.insert(
                    key,
                    ToolPair {
                        call_seq: obs.seq,
                        result_seq: None,
                        tool_call_id: payload.tool_call_id.clone(),
                        name: name.clone(),
                        outcome: None,
                    },
                );
                self.entries.push(TranscriptEntry::ToolCall {
                    seq: obs.seq,
                    tool_call_id: payload.tool_call_id.clone(),
                    name,
                    state: payload.state,
                });
            }
            ObservationPayload::ToolResult(payload) => {
                let text = blocks_text(&payload.blocks);
                let key = payload.tool_call_id.as_str().to_owned();
                self.tools
                    .entry(key)
                    .and_modify(|pair| {
                        pair.result_seq = Some(obs.seq);
                        pair.outcome = Some(payload.outcome);
                    })
                    .or_insert_with(|| ToolPair {
                        call_seq: obs.seq,
                        result_seq: Some(obs.seq),
                        tool_call_id: payload.tool_call_id.clone(),
                        name: None,
                        outcome: Some(payload.outcome),
                    });
                self.entries.push(TranscriptEntry::ToolResult {
                    seq: obs.seq,
                    tool_call_id: payload.tool_call_id.clone(),
                    outcome: payload.outcome,
                    text,
                });
            }
            ObservationPayload::Lifecycle(payload) => {
                if is_compact(payload) {
                    self.close_turn(obs.seq);
                    self.turns.push(TurnBound {
                        start_seq: obs.seq,
                        end_seq: Some(obs.seq),
                        kind: TurnKind::Compact,
                    });
                }
                if is_turn_result(payload) {
                    self.close_turn(obs.seq);
                }
            }
            _ => {}
        }
    }

    fn open_turn(&mut self, seq: U64, kind: TurnKind) {
        if let Some(last) = self.turns.last_mut()
            && last.end_seq.is_none()
        {
            last.end_seq = Some(seq);
        }
        self.turns.push(TurnBound {
            start_seq: seq,
            end_seq: None,
            kind,
        });
    }

    fn close_turn(&mut self, seq: U64) {
        if let Some(last) = self.turns.last_mut()
            && last.end_seq.is_none()
        {
            last.end_seq = Some(seq);
        }
    }
}

impl InteractionProjection {
    fn apply(&mut self, obs: &Observation) {
        match &obs.body {
            ObservationPayload::InteractionRequested(payload) => {
                let Ok(key) = serde_json::to_string(&payload.interaction.request_key) else {
                    return;
                };
                if self.by_key.contains_key(&key) {
                    return;
                }
                let id = payload.interaction.meta.id.clone();
                let id_text = id.as_id().as_str().to_owned();
                self.by_id.insert(id_text, key.clone());
                self.by_key.insert(
                    key.clone(),
                    InteractionRecord {
                        interaction_id: id,
                        request_key: key,
                        state: InteractionState::Pending,
                        requested_seq: obs.seq,
                        answered_seq: None,
                        expired_seq: None,
                    },
                );
            }
            ObservationPayload::InteractionAnswered(payload) => {
                let id_text = payload.interaction_id.as_id().as_str().to_owned();
                let Some(key) = self.by_id.get(&id_text).cloned() else {
                    return;
                };
                let Some(record) = self.by_key.get_mut(&key) else {
                    return;
                };
                if record.answered_seq.is_some()
                    || matches!(
                        record.state,
                        InteractionState::Expired
                            | InteractionState::AnswerCommitted
                            | InteractionState::Resolved
                    )
                {
                    return;
                }
                record.answered_seq = Some(obs.seq);
                record.state = InteractionState::AnswerCommitted;
            }
            ObservationPayload::InteractionExpired(payload) => {
                let id_text = payload.interaction_id.as_id().as_str().to_owned();
                let Some(key) = self.by_id.get(&id_text).cloned() else {
                    return;
                };
                let Some(record) = self.by_key.get_mut(&key) else {
                    return;
                };
                if record.expired_seq.is_some() || record.answered_seq.is_some() {
                    return;
                }
                record.expired_seq = Some(obs.seq);
                record.state = InteractionState::Expired;
            }
            ObservationPayload::Lifecycle(payload) => {
                if let remuda_protocol::LifecyclePayload::Entity(entity) = payload.as_ref()
                    && let remuda_protocol::LifecycleEntity::Interaction(interaction) =
                        &entity.entity_value
                    && let Some(key) = self.by_id.get(interaction.meta.id.as_id().as_str())
                    && let Some(record) = self.by_key.get_mut(key)
                {
                    record.state = interaction.state;
                }
            }
            _ => {}
        }
    }
}

impl StatusProjection {
    fn apply(&mut self, obs: &Observation) {
        if obs.source.delivery == SourceDelivery::Live
            && self.connectivity == Connectivity::Disconnected
        {
            self.connectivity = Connectivity::Connected;
        }
        match &obs.body {
            ObservationPayload::Lifecycle(payload) => match payload.as_ref() {
                LifecyclePayload::Entity(entity) => {
                    if let LifecycleEntity::Instance(instance) = &entity.entity_value {
                        self.lifecycle = instance.lifecycle;
                        if let Knowledge::Known { value } = &instance.activity {
                            self.activity = *value;
                        }
                        self.connectivity = instance.connectivity;
                        if instance.last_error.is_some() {
                            self.last_error = instance.last_error.clone();
                        }
                    }
                    if entity.state == "failed" {
                        self.lifecycle = InstanceLifecycle::Failed;
                        self.activity = Activity::Idle;
                        self.connectivity = Connectivity::Disconnected;
                        if self.last_error.is_none() && !entity.reason_code.is_empty() {
                            self.last_error = Some(entity.reason_code.clone());
                        }
                    }
                }
                LifecyclePayload::Native(native) => {
                    let name = native.native_name.to_ascii_lowercase();
                    if native_lifecycle_failed(native) {
                        self.lifecycle = InstanceLifecycle::Failed;
                        self.activity = Activity::Idle;
                        self.connectivity = Connectivity::Disconnected;
                        self.last_error = native_lifecycle_error_text(native);
                    }
                    match native.topic {
                        LifecycleTopic::Session => {
                            if self.lifecycle == InstanceLifecycle::Failed {
                                // Keep the failed fold; do not revive from a later session status.
                            } else if name.contains("init") || name.contains("start") {
                                self.lifecycle = InstanceLifecycle::Ready;
                                self.connectivity = Connectivity::Connected;
                                self.activity = Activity::Idle;
                            } else if name.contains("end") || name.contains("exit") {
                                self.lifecycle = InstanceLifecycle::Exited;
                                self.activity = Activity::Idle;
                                self.connectivity = Connectivity::Disconnected;
                            } else if name.contains("idle") {
                                self.activity = Activity::Idle;
                            } else if name.contains("running") || name.contains("working") {
                                self.activity = Activity::Working;
                            }
                        }
                        LifecycleTopic::Turn => {
                            if name.contains("result") || name.contains("stop") {
                                if self.activity != Activity::WaitingInteraction {
                                    self.activity = Activity::Idle;
                                }
                            } else {
                                self.activity = Activity::Working;
                            }
                        }
                        LifecycleTopic::Reconciliation => {
                            self.lifecycle = InstanceLifecycle::Reconciling;
                            self.connectivity = Connectivity::Reconciling;
                        }
                        _ => {}
                    }
                    if let Knowledge::Known { value } = &native.status {
                        let status = value.to_ascii_lowercase();
                        if status.contains("idle") {
                            self.activity = Activity::Idle;
                        } else if status.contains("running") || status.contains("working") {
                            self.activity = Activity::Working;
                        } else if status.contains("requires_action") || status.contains("waiting") {
                            self.activity = Activity::WaitingInteraction;
                        }
                    }
                }
            },
            ObservationPayload::ToolCall(_) | ObservationPayload::WorkflowRun(_) => {
                if self.activity != Activity::WaitingInteraction {
                    self.activity = Activity::Working;
                }
            }
            ObservationPayload::InteractionRequested(_) => {
                self.activity = Activity::WaitingInteraction;
            }
            ObservationPayload::InteractionAnswered(_)
            | ObservationPayload::InteractionExpired(_) => {
                self.activity = Activity::Idle;
            }
            ObservationPayload::Message(payload) if payload.role == MessageRole::Assistant => {
                if self.activity == Activity::Idle {
                    self.activity = Activity::Working;
                }
            }
            _ => {}
        }
    }
}

fn native_lifecycle_failed(native: &remuda_protocol::NativeLifecycle) -> bool {
    if native.severity == remuda_protocol::Severity::Error {
        return true;
    }
    let name = native.native_name.to_ascii_lowercase();
    name.contains("error")
        || name.contains("exit")
        || name.contains("gone")
        || name.contains("agent_not_ready")
        || name.contains("shell")
}

fn native_lifecycle_error_text(native: &remuda_protocol::NativeLifecycle) -> Option<String> {
    if let Some(message) = native.related_ids.get("lastError").cloned()
        && !message.is_empty()
    {
        return Some(message);
    }
    match &native.status {
        Knowledge::Known { value } if !value.is_empty() => Some(value.clone()),
        _ => None,
    }
}

fn blocks_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        if let ContentBlock::Text(text) = block {
            out.push_str(&text.text);
        }
    }
    out
}

fn is_compact(payload: &LifecyclePayload) -> bool {
    match payload {
        LifecyclePayload::Native(native) => {
            native.native_name.to_ascii_lowercase().contains("compact")
        }
        _ => false,
    }
}

fn is_turn_result(payload: &LifecyclePayload) -> bool {
    match payload {
        LifecyclePayload::Native(native) => {
            native.topic == LifecycleTopic::Turn
                && native.native_name.to_ascii_lowercase().contains("result")
        }
        _ => false,
    }
}

/// Ordered fold that buffers out-of-order seq values and applies 1..=n with no holes.
#[derive(Debug, Clone)]
pub struct OrderedFold<T> {
    next: u64,
    buffer: BTreeMap<u64, Observation>,
    /// Folded state.
    pub inner: T,
}

impl<T: Default> Default for OrderedFold<T> {
    fn default() -> Self {
        Self {
            next: 1,
            buffer: BTreeMap::new(),
            inner: T::default(),
        }
    }
}

impl OrderedFold<Projections> {
    /// Empty fold waiting for seq 1.
    pub fn new() -> Self {
        Self::default()
    }
}

impl<T> OrderedFold<T>
where
    T: Fold,
{
    /// Push an observation; apply it once `seq` is the next contiguous value.
    pub fn push(&mut self, obs: Observation) {
        self.buffer.insert(obs.seq.0, obs);
        while let Some(obs) = self.buffer.remove(&self.next) {
            self.inner.apply_obs(&obs);
            self.next += 1;
        }
    }

    /// Next seq this fold will apply.
    pub fn next_seq(&self) -> u64 {
        self.next
    }
}

/// Types that can fold a committed observation.
pub trait Fold {
    /// Apply one observation in seq order.
    fn apply_obs(&mut self, obs: &Observation);
}

impl Fold for Projections {
    fn apply_obs(&mut self, obs: &Observation) {
        self.apply(obs);
    }
}

impl Fold for TranscriptProjection {
    fn apply_obs(&mut self, obs: &Observation) {
        self.apply(obs);
    }
}

/// Replay a contiguous slice in seq order.
pub fn fold_all(observations: &[Observation]) -> Projections {
    let mut fold = OrderedFold::<Projections>::new();
    for obs in observations {
        fold.push(obs.clone());
    }
    fold.inner
}

/// Apply later seqs first, then earlier seqs — used to prove prepend catch-up.
pub fn fold_prepend_backfill(observations: &[Observation]) -> Projections {
    if observations.is_empty() {
        return Projections::default();
    }
    let mut indexed = observations.to_vec();
    indexed.sort_by_key(|obs| obs.seq.0);
    let split = indexed.len() / 2;
    let later = indexed.split_off(split);
    let mut fold = OrderedFold::<Projections>::new();
    for obs in later {
        fold.push(obs);
    }
    for obs in indexed {
        fold.push(obs);
    }
    fold.inner
}
