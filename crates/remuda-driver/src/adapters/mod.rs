//! Per-harness file-tail signal adapters (D-028 §4.2, §7, §13 P6).
//!
//! `Hook > File > OSC > Screen`. [`crate::launch::HookSession`] owns the hook
//! layer for every harness; this module owns the *file* layer for the two
//! harnesses whose structured signals live on disk rather than on a socket:
//!
//! * **codex** — `$CODEX_HOME/sessions/…/rollout-*.jsonl`, item-level
//!   (completed items only, no deltas); [`codex_adapter::CodexAdapter`].
//! * **grok** — `$GROK_HOME/sessions/<enc cwd>/<id>/{updates,events}.jsonl`
//!   plus `usage.json` and `active_sessions.json`, chunk-level;
//!   [`grok_adapter::GrokAdapter`].
//!
//! Both adapters are deliberately **pure state machines**: they take parsed
//! records in file order and emit [`AdapterObservation`]s. All IO (file tails,
//! discovery polling, observation stamping) lives in [`supervisor`], which is
//! also what makes the adapters testable against the captured real sessions
//! under `crates/remuda-driver/tests/fixtures/{codex,grok}/` without a PTY.
//!
//! Evidence, per adapter:
//! [codex-signals-1](../../../docs/design/evidence/codex-signals-1.md),
//! [grok-signals-1](../../../docs/design/evidence/grok-signals-1.md).

pub mod codex_adapter;
pub mod grok_adapter;
pub mod supervisor;

use remuda_protocol::{
    AgentKind, Completeness, ContentBlock, ContentStatus, EventId, HostId, Id, InstanceId,
    Knowledge, LifecyclePayload, LifecycleTopic, MessagePayload, MessagePhase, MessageRole,
    NativeLifecycle, NativeRequestKey, Observation, ObservationPayload, ObservationSource, RunId,
    RuntimeCursor, SchemaVersion, Severity, SourceChannel, SourceCursor, SourceDelivery, TextBlock,
    ThoughtPayload, ThoughtRepresentation, Timestamp, ToolCallPayload, ToolCallState, ToolCategory,
    ToolOutcome, ToolResultPayload, U64,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub use codex_adapter::CodexAdapter;
pub use grok_adapter::GrokAdapter;
pub use supervisor::{AdapterCtx, spawn_file_adapters};

/// One adapter-produced fact before it gets its envelope stamped.
///
/// Adapters never build an [`Observation`] themselves: they do not own the
/// instance identity or the shared sequence counter, and letting them invent
/// either is how two observation streams end up interleaved with duplicate
/// ids. The supervisor stamps.
#[derive(Debug, Clone)]
pub struct AdapterObservation {
    /// `File` for every record here; the adapters only ever read native files.
    pub channel: SourceChannel,
    /// How complete this fact is. Completed-item records are `Structured`; a
    /// chunk that a later frame replaces is `Partial`.
    pub completeness: Completeness,
    /// The journaled fact.
    pub payload: ObservationPayload,
    /// Native turn id for the source envelope, when the record carried one.
    pub turn_id: Option<String>,
    /// Native item/request id for the envelope, when applicable.
    pub item_id: Option<String>,
}

impl AdapterObservation {
    /// A structured, file-channel observation.
    #[must_use]
    pub fn structured(payload: ObservationPayload) -> Self {
        Self {
            channel: SourceChannel::File,
            completeness: Completeness::Structured,
            payload,
            turn_id: None,
            item_id: None,
        }
    }

    /// A partial (streaming chunk) observation.
    #[must_use]
    pub fn partial(payload: ObservationPayload) -> Self {
        Self {
            channel: SourceChannel::File,
            completeness: Completeness::Partial,
            payload,
            turn_id: None,
            item_id: None,
        }
    }

    /// Attach the native turn id to the envelope.
    #[must_use]
    pub fn with_turn(mut self, turn_id: impl Into<String>) -> Self {
        self.turn_id = Some(turn_id.into());
        self
    }
}

/// Discovery inputs every adapter needs.
#[derive(Debug, Clone)]
pub struct AdapterHome {
    /// Shadow/native harness home (`CODEX_HOME` / `GROK_HOME`).
    pub home: PathBuf,
    /// Session working directory, used to disambiguate registry/file matches.
    pub cwd: PathBuf,
    /// Agent process pid when known (launch child, or promoted foreground
    /// leader). Grok's `active_sessions.json` matches on it.
    pub pid: Option<u32>,
}

/// A session the adapter should follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterBinding {
    /// Native session id.
    pub session_id: String,
    /// Directory holding the session's files, when discovery found it.
    pub directory: Option<PathBuf>,
    /// Explicit main artifact (rollout / updates.jsonl), when known.
    pub main_file: Option<PathBuf>,
}

/// The file-facing half of a harness adapter.
///
/// One poll cycle: discover the session if unbound, then drain every tail. The
/// supervisor calls [`Self::poll`] on a fixed interval and forwards whatever
/// observations come back.
pub trait FileSignalAdapter: Send + Sync {
    /// Which harness this is.
    fn kind(&self) -> AgentKind;
    /// The native session currently followed, once discovery bound one.
    fn session_id(&self) -> Option<&str>;
    /// A hook-confirmed session id; discovery prefers it (Hook > File).
    fn confirm_session(&mut self, session_id: &str);
    /// One discover-and-drain cycle.
    fn poll(&mut self) -> crate::error::DriverResult<Vec<AdapterObservation>>;
}

/// Identity the supervisor stamps every observation with.
#[derive(Debug, Clone)]
pub struct StampCtx {
    /// Owning instance.
    pub instance_id: InstanceId,
    /// Owning host.
    pub host_id: HostId,
    /// Journal id.
    pub journal_id: Id,
    /// Current run.
    pub run_id: RunId,
    /// Bound native session id.
    pub session_id: String,
}

/// Stamp one adapter observation as a journal observation.
///
/// Mirrors `remuda_signal::SignalBus::build` and the promotion poller's
/// `build`: one stamping shape for every channel, so hook, file and runtime
/// observations differ only in their declared channel.
#[must_use]
pub fn stamp(ctx: &StampCtx, seq: u64, observed: &AdapterObservation) -> Option<Observation> {
    let observed_at = now_ts()?;
    Some(Observation {
        schema_version: SchemaVersion,
        event_id: EventId::new(),
        journal_id: ctx.journal_id.clone(),
        instance_id: ctx.instance_id.clone(),
        run_id: Some(ctx.run_id.clone()),
        host_id: ctx.host_id.clone(),
        process_generation: U64(1),
        run_generation: Some(U64(1)),
        seq: U64(seq),
        observed_at,
        native_at: Knowledge::Unknown {
            reason: "file-tail-has-no-envelope-time".into(),
            evidence_event_ids: Vec::new(),
        },
        source: ObservationSource {
            driver_kind: remuda_protocol::DriverKind::ShellPty,
            driver_version: "shell-pty".into(),
            adapter_version: crate::capabilities::ADAPTER_VERSION.to_owned(),
            channel: observed.channel,
            delivery: SourceDelivery::Live,
            native_session_id: Knowledge::Known {
                value: ctx.session_id.clone(),
            },
            native_turn_id: match &observed.turn_id {
                Some(value) => Knowledge::Known {
                    value: value.clone(),
                },
                None => Knowledge::Unknown {
                    reason: "not-emitted".into(),
                    evidence_event_ids: Vec::new(),
                },
            },
            native_agent_id: Knowledge::NotApplicable,
            native_item_id: match &observed.item_id {
                Some(value) => Knowledge::Known {
                    value: value.clone(),
                },
                None => Knowledge::NotApplicable,
            },
            native_event_id: Knowledge::NotApplicable,
            native_request_id: NativeRequestKey::None,
            source_cursor: SourceCursor::Runtime(Box::new(RuntimeCursor {
                ledger_revision: U64(seq),
            })),
        },
        completeness: observed.completeness,
        raw_ref: None,
        evidence_event_ids: Vec::new(),
        body: observed.payload.clone(),
    })
}

// ---------------------------------------------------------------------------
// Payload builders
// ---------------------------------------------------------------------------

/// A `NodeMutation` with revision 1 (first observation for this node).
fn first_mutation(node_id: Id) -> remuda_protocol::NodeMutation {
    remuda_protocol::NodeMutation {
        node_id,
        revision: U64(1),
        operation: remuda_protocol::MutationOperation::Open,
        base_revision: None,
    }
}

/// One complete assistant/user text message (`open`+`close` semantics folded
/// into a single observation by the journal diff normalizer).
#[must_use]
pub fn message_payload(message_id: Id, role: MessageRole, text: String) -> ObservationPayload {
    ObservationPayload::Message(Box::new(MessagePayload {
        mutation: first_mutation(message_id.clone()),
        message_id,
        role,
        phase: MessagePhase::Final,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock { text }))],
        target_block: Some(0),
        parent_tool_call_id: None,
        native_origin: Knowledge::NotApplicable,
        status: ContentStatus::Complete,
    }))
}

/// One streaming assistant text chunk. The node id is per-turn: the journal
/// projection appends chunks onto the same message and a later close replaces.
///
/// `revision` is 1-based and monotonic for `node_id`; the first chunk uses
/// [`MutationOperation::Open`], later ones [`MutationOperation::Append`], and
/// the terminal one [`MutationOperation::Close`] (built by
/// [`message_close`]).
#[must_use]
pub fn message_chunk(node_id: Id, revision: u64, first: bool, text: String) -> ObservationPayload {
    ObservationPayload::Message(Box::new(MessagePayload {
        mutation: streaming_mutation(node_id.clone(), revision, first),
        message_id: node_id,
        role: MessageRole::Assistant,
        phase: MessagePhase::Final,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock { text }))],
        target_block: Some(0),
        parent_tool_call_id: None,
        native_origin: Knowledge::NotApplicable,
        status: ContentStatus::Streaming,
    }))
}

/// Terminal snapshot for a streamed message: full accumulated text and the
/// completion state (`Complete`, or `Interrupted` when the turn was cancelled).
#[must_use]
pub fn message_close(
    node_id: Id,
    revision: u64,
    base_revision: Option<u64>,
    text: String,
    status: ContentStatus,
) -> ObservationPayload {
    ObservationPayload::Message(Box::new(MessagePayload {
        mutation: remuda_protocol::NodeMutation {
            node_id: node_id.clone(),
            revision: U64(revision),
            operation: remuda_protocol::MutationOperation::Close,
            base_revision: base_revision.map(U64),
        },
        message_id: node_id,
        role: MessageRole::Assistant,
        phase: MessagePhase::Final,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock { text }))],
        target_block: Some(0),
        parent_tool_call_id: None,
        native_origin: Knowledge::NotApplicable,
        status,
    }))
}

/// One complete reasoning/thought block.
#[must_use]
pub fn thought_payload(thought_id: Id, text: String) -> ObservationPayload {
    ObservationPayload::Thought(Box::new(ThoughtPayload {
        mutation: first_mutation(thought_id.clone()),
        thought_id,
        representation: ThoughtRepresentation::Text,
        text: Some(text),
        part_index: 0,
        status: ContentStatus::Complete,
    }))
}

/// A streaming thought chunk with explicit revision.
#[must_use]
pub fn thought_chunk(node_id: Id, revision: u64, first: bool, text: String) -> ObservationPayload {
    ObservationPayload::Thought(Box::new(ThoughtPayload {
        mutation: streaming_mutation(node_id.clone(), revision, first),
        thought_id: node_id,
        representation: ThoughtRepresentation::Text,
        text: Some(text),
        part_index: 0,
        status: ContentStatus::Streaming,
    }))
}

/// `Open` on the first chunk, `Append` afterwards.
fn streaming_mutation(node_id: Id, revision: u64, first: bool) -> remuda_protocol::NodeMutation {
    remuda_protocol::NodeMutation {
        node_id,
        revision: U64(revision),
        operation: if first {
            remuda_protocol::MutationOperation::Open
        } else {
            remuda_protocol::MutationOperation::Append
        },
        base_revision: None,
    }
}

/// Known value, or `Unknown` with the standard "not emitted" reason.
fn known_or_unknown(value: Option<String>) -> Knowledge<String> {
    match value {
        Some(value) => Knowledge::Known { value },
        None => not_emitted(),
    }
}

/// The standard "the source did not emit this" unknown.
fn not_emitted<T>() -> Knowledge<T> {
    Knowledge::Unknown {
        reason: "not-emitted".into(),
        evidence_event_ids: Vec::new(),
    }
}

/// A tool call proposal with its known input.
#[must_use]
pub fn tool_call_payload(
    tool_call_id: Id,
    name: Option<String>,
    input: Option<serde_json::Value>,
) -> ObservationPayload {
    let tool_name = known_or_unknown(name.clone());
    let display_title = known_or_unknown(name);
    ObservationPayload::ToolCall(Box::new(ToolCallPayload {
        mutation: first_mutation(tool_call_id.clone()),
        tool_call_id,
        parent_tool_call_id: None,
        tool_name,
        display_title,
        category: ToolCategory::Shell,
        input: input.map_or_else(not_emitted, |value| Knowledge::Known { value }),
        input_text_delta: None,
        state: ToolCallState::Proposed,
        executor: Knowledge::NotApplicable,
    }))
}

/// A final tool result. `outcome` carries denial vs failure vs success.
#[must_use]
pub fn tool_result_payload(
    tool_call_id: Id,
    text: Option<String>,
    structured: Option<serde_json::Value>,
    exit_code: Option<i32>,
    outcome: ToolOutcome,
) -> ObservationPayload {
    ObservationPayload::ToolResult(Box::new(ToolResultPayload {
        mutation: remuda_protocol::NodeMutation {
            node_id: tool_call_id.clone(),
            revision: U64(2),
            operation: remuda_protocol::MutationOperation::Close,
            base_revision: Some(U64(1)),
        },
        tool_call_id,
        stage: remuda_protocol::ResultStage::Final,
        outcome,
        blocks: text
            .filter(|value| !value.is_empty())
            .map(|text| vec![ContentBlock::Text(Box::new(TextBlock { text }))])
            .unwrap_or_default(),
        structured_result: structured.map_or_else(not_emitted, |value| Knowledge::Known { value }),
        exit_code: exit_code.map_or_else(
            || Knowledge::Unknown {
                reason: "not-emitted".into(),
                evidence_event_ids: Vec::new(),
            },
            |code| Knowledge::Known { value: code },
        ),
        changes: Vec::new(),
    }))
}

/// A native turn lifecycle event. `status` is the text the Node fold matches
/// on (`working` / `idle`), so those spellings are load-bearing.
#[must_use]
pub fn turn_lifecycle(name: &'static str, status: &'static str) -> ObservationPayload {
    ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
        NativeLifecycle {
            topic: LifecycleTopic::Turn,
            native_name: name.into(),
            native_id: Knowledge::NotApplicable,
            status: Knowledge::Known {
                value: status.into(),
            },
            related_ids: BTreeMap::new(),
            data_ref: None,
            severity: Severity::Info,
            affects_completion: false,
        },
    ))))
}

/// Allocate a journal node id for a native id, reusing the same journal id for
/// repeat sightings of the same native node.
#[must_use]
pub fn node_id(map: &mut std::collections::HashMap<String, Id>, native: &str) -> Id {
    if let Some(id) = map.get(native) {
        return id.clone();
    }
    let id = Id::new("obj").expect("object id prefix is registered");
    map.insert(native.to_owned(), id.clone());
    id
}

/// Millisecond-precision UTC, matching the hook bus.
fn now_ts() -> Option<Timestamp> {
    let now = time::OffsetDateTime::now_utc();
    let date = now.date();
    let (hour, minute, second) = now.time().as_hms();
    Timestamp::try_from(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        date.year(),
        u8::from(date.month()),
        date.day(),
        hour,
        minute,
        second,
        now.millisecond(),
    ))
    .ok()
}

/// Shared sequence counter so adapter observations order against hook and
/// screen observations from the same instance.
pub(crate) fn next_seq(seq: &Arc<AtomicU64>) -> u64 {
    seq.fetch_add(1, Ordering::SeqCst) + 1
}
