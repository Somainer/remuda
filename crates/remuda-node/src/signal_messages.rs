//! Streaming message assembly from `MessageDisplay` hook deltas (D-028 §7).
//!
//! The user's complaint was that the 结构 view arrived a whole message at a
//! time: «现在 Structural 的界面不是按文本流式出现的…我觉得它的实时性不够».
//! For an agent running inside a PTY there is no stream-json channel to read,
//! so the only live text signal is the `MessageDisplay` hook. This module
//! folds those deltas into protocol §5.2 message mutations —
//! `open` → `append` → `close` per `messageId` — which is the same shape the
//! web assembler already merges incrementally.
//!
//! ## What the hook actually gives us (measured, claude 2.1.221)
//!
//! The design recorded `MessageDisplay` as **[U]**. Measured behaviour:
//!
//! - `index` is a **chunk counter, not an API block index**: one text block
//!   arrived as `index:0 delta:"1\n2\n3\n4\n"` then `index:1 delta:"5\n6\n7\n8"`.
//!   The deltas are append-only pieces of one message, so `index` orders them
//!   and nothing more. Treating it as a block index would scatter one
//!   paragraph across several nodes.
//! - `final:true` marks the last chunk; earlier chunks omit it.
//! - `message_id` and `turn_id` live in **their own id space**. The hook's
//!   `message_id` appears nowhere in the transcript, whose assistant records
//!   use `msg_vrtx_*`. The two channels therefore cannot be joined by id, only
//!   positionally — which is why the streamed node is closed and left for the
//!   transcript's own (authoritative) message to supersede visually, rather
//!   than being reconciled onto the same node id.
//! - Headless `claude -p` fires the hook **once with the whole message**. Only
//!   an interactive PTY streams — which is exactly D-028's target.
//!
//! ## Ordering
//!
//! Measured: the chunks arrive ~50 ms *after* the transcript record is
//! written. The hook is a display echo, not a preview, so this fold never
//! assumes it runs first. The transcript message is the authoritative value
//! (§7: «transcript / rollout 的完整块到达 → replace + close»); the streamed
//! node exists to put text on screen during the seconds before it lands.

use remuda_protocol::{
    Completeness, ContentBlock, ContentStatus, Id, MessageOrigin, MessagePayload, MessagePhase,
    MessageRole, MutationOperation, NodeMutation, Observation, ObservationPayload, SourceChannel,
    TextBlock, U64,
};
use std::collections::HashMap;

/// One `MessageDisplay` payload, read off a hook lifecycle observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageDelta {
    /// Native message id from the hook's own id space.
    pub message_id: String,
    /// Native turn id, when reported.
    pub turn_id: Option<String>,
    /// Chunk counter within the message. Not an API content-block index.
    pub index: u64,
    /// Whether this is the last chunk of the message.
    pub final_chunk: bool,
    /// The text this chunk adds.
    pub delta: String,
}

/// Read a hook observation as a message delta, if it is one.
///
/// The channel is checked by [`crate::signal::hook_lifecycle`], so a
/// screen-derived guess carrying the same name cannot fabricate text.
#[must_use]
pub fn message_delta(observation: &Observation) -> Option<MessageDelta> {
    let native = crate::signal::hook_lifecycle(observation)?;
    if native.native_name != "MessageDisplay" {
        return None;
    }
    let related = &native.related_ids;
    let delta = related.get("delta")?.clone();
    Some(MessageDelta {
        message_id: related.get("messageId")?.clone(),
        turn_id: related.get("turnId").cloned(),
        // A missing index means "the only chunk"; refusing to fold it would
        // drop real text for a field that is only there to order chunks.
        index: related
            .get("index")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0),
        final_chunk: related.get("final").map(String::as_str) == Some("true"),
        delta,
    })
}

/// Per-message streaming state.
#[derive(Debug)]
struct Streamed {
    /// Journal node this message streams into.
    node_id: Id,
    /// Last mutation revision emitted for the node.
    revision: u64,
    /// Highest chunk index folded, so a repeated hook cannot double-append.
    last_index: Option<u64>,
    /// Whether `close` was already emitted.
    closed: bool,
}

/// Folds `MessageDisplay` deltas into per-message mutation chains.
///
/// One per instance. Deliberately not keyed by turn: a `message_id` is already
/// unique, and keying on the turn too would lose the chain if `turn_id` were
/// ever absent.
#[derive(Debug, Default)]
pub struct MessageAssembler {
    messages: HashMap<String, Streamed>,
}

impl MessageAssembler {
    /// A fresh assembler with no streaming messages.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one delta, returning the message payload to journal.
    ///
    /// Returns `None` when the delta adds nothing: a duplicate chunk index, or
    /// a chunk arriving after the message was closed. Re-appending either
    /// would duplicate text on screen.
    pub fn fold(&mut self, delta: &MessageDelta) -> Option<MessagePayload> {
        let entry = self.messages.entry(delta.message_id.clone());
        let streamed = match entry {
            std::collections::hash_map::Entry::Occupied(occupied) => occupied.into_mut(),
            std::collections::hash_map::Entry::Vacant(vacant) => vacant.insert(Streamed {
                node_id: Id::new("obj").ok()?,
                revision: 0,
                last_index: None,
                closed: false,
            }),
        };
        if streamed.closed {
            return None;
        }
        // Hooks can re-fire; only a strictly newer chunk adds text.
        if streamed
            .last_index
            .is_some_and(|last| delta.index <= last && streamed.revision > 0)
        {
            return None;
        }
        let operation = if streamed.revision == 0 {
            MutationOperation::Open
        } else {
            MutationOperation::Append
        };
        let base_revision = (streamed.revision > 0).then_some(U64(streamed.revision));
        streamed.revision += 1;
        streamed.last_index = Some(delta.index);
        streamed.closed = delta.final_chunk;
        Some(MessagePayload {
            mutation: NodeMutation {
                node_id: streamed.node_id.clone(),
                revision: U64(streamed.revision),
                operation,
                base_revision,
            },
            message_id: streamed.node_id.clone(),
            role: MessageRole::Assistant,
            phase: MessagePhase::Final,
            blocks: vec![ContentBlock::Text(Box::new(TextBlock {
                text: delta.delta.clone(),
            }))],
            // Appends target the single text block this node carries.
            target_block: Some(0),
            parent_tool_call_id: None,
            native_origin: remuda_protocol::Knowledge::Known {
                value: delta.message_id.clone(),
            },
            origin: Some(MessageOrigin::Human),
            command_id: None,
            prompt_mode: None,
            status: if delta.final_chunk {
                ContentStatus::Complete
            } else {
                ContentStatus::Streaming
            },
        })
    }

    /// Build the Observation carrying `payload`, derived from the hook event.
    ///
    /// The envelope is inherited from the hook observation (same run, same
    /// generation) and restamped by the store on append; only the channel and
    /// body differ. Keeping [`SourceChannel::Hook`] is deliberate — §4.3 ranks
    /// sources by channel, and this text *is* hook evidence, so the transcript
    /// value can later supersede it rather than being outranked by it.
    #[must_use]
    pub fn observation(source: &Observation, payload: MessagePayload) -> Observation {
        let mut observation = source.clone();
        observation.event_id = remuda_protocol::EventId::new();
        observation.completeness = Completeness::Structured;
        observation.source.channel = SourceChannel::Hook;
        observation.body = ObservationPayload::Message(Box::new(payload));
        observation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_protocol::{
        DriverKind, EventId, HostId, InstanceId, Knowledge, LifecyclePayload, LifecycleTopic,
        NativeLifecycle, NativeRequestKey, ObservationSource, RunId, RuntimeCursor, SchemaVersion,
        Severity, SourceCursor, SourceDelivery, Timestamp,
    };
    use std::collections::BTreeMap;

    /// A hook observation shaped exactly like the recorded live payloads in
    /// `docs/design/evidence/native-pty-3.md`.
    fn hook(name: &str, related: &[(&str, &str)]) -> Observation {
        fn unknown<T>() -> Knowledge<T> {
            Knowledge::Unknown {
                reason: "not-emitted".into(),
                evidence_event_ids: Vec::new(),
            }
        }
        Observation {
            schema_version: SchemaVersion,
            event_id: EventId::new(),
            journal_id: Id::new("obj").unwrap(),
            instance_id: InstanceId::new(),
            run_id: Some(RunId::new()),
            host_id: HostId::new(),
            process_generation: U64(1),
            run_generation: Some(U64(1)),
            seq: U64(1),
            observed_at: Timestamp::try_from("2026-09-14T00:00:00.000Z".to_owned()).unwrap(),
            native_at: unknown(),
            source: ObservationSource {
                driver_kind: DriverKind::ShellPty,
                driver_version: "shell-pty".into(),
                adapter_version: "test".into(),
                channel: SourceChannel::Hook,
                delivery: SourceDelivery::Live,
                native_session_id: unknown(),
                native_turn_id: Knowledge::NotApplicable,
                native_agent_id: Knowledge::NotApplicable,
                native_item_id: Knowledge::NotApplicable,
                native_event_id: Knowledge::NotApplicable,
                native_request_id: NativeRequestKey::None,
                source_cursor: SourceCursor::Runtime(Box::new(RuntimeCursor {
                    ledger_revision: U64(1),
                })),
            },
            completeness: Completeness::Structured,
            raw_ref: None,
            evidence_event_ids: Vec::new(),
            body: ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
                NativeLifecycle {
                    topic: LifecycleTopic::Hook,
                    native_name: name.into(),
                    native_id: Knowledge::Known {
                        value: "s-1".into(),
                    },
                    status: Knowledge::Known {
                        value: "streaming".into(),
                    },
                    related_ids: related
                        .iter()
                        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                        .collect::<BTreeMap<_, _>>(),
                    data_ref: None,
                    severity: Severity::Info,
                    affects_completion: false,
                },
            )))),
        }
    }

    fn text_of(payload: &MessagePayload) -> String {
        payload
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect()
    }

    /// The exact two-chunk payload recorded from a live interactive session.
    fn recorded_chunks() -> Vec<Observation> {
        vec![
            hook(
                "MessageDisplay",
                &[
                    ("messageId", "1da06679-00ca-474c-8a1f-7a20a272f53d"),
                    ("turnId", "cab187b9-9209-4d91-9b04-f5130b6f9f90"),
                    ("index", "0"),
                    ("delta", "1\n2\n3\n4\n"),
                ],
            ),
            hook(
                "MessageDisplay",
                &[
                    ("messageId", "1da06679-00ca-474c-8a1f-7a20a272f53d"),
                    ("turnId", "cab187b9-9209-4d91-9b04-f5130b6f9f90"),
                    ("index", "1"),
                    ("final", "true"),
                    ("delta", "5\n6\n7\n8"),
                ],
            ),
        ]
    }

    /// The whole point of P3: text must appear as an `open` + `append` chain
    /// while the turn is still running, not one payload at the end.
    #[test]
    fn recorded_deltas_stream_as_open_then_append_then_complete() {
        let mut assembler = MessageAssembler::new();
        let folded: Vec<MessagePayload> = recorded_chunks()
            .iter()
            .filter_map(|observation| assembler.fold(&message_delta(observation).expect("a delta")))
            .collect();
        assert_eq!(folded.len(), 2, "each chunk is visible on arrival");
        assert_eq!(folded[0].mutation.operation, MutationOperation::Open);
        assert_eq!(folded[0].status, ContentStatus::Streaming);
        assert_eq!(text_of(&folded[0]), "1\n2\n3\n4\n");

        assert_eq!(folded[1].mutation.operation, MutationOperation::Append);
        assert_eq!(
            folded[1].status,
            ContentStatus::Complete,
            "`final: true` closes the message"
        );
        // An append carries only the new text (§5.2), never the whole message.
        assert_eq!(text_of(&folded[1]), "5\n6\n7\n8");
    }

    /// Both chunks are one message. Minting a node per chunk is what would put
    /// half a sentence in its own bubble.
    #[test]
    fn every_chunk_of_one_message_shares_one_node() {
        let mut assembler = MessageAssembler::new();
        let nodes: std::collections::BTreeSet<Id> = recorded_chunks()
            .iter()
            .filter_map(|observation| {
                assembler
                    .fold(&message_delta(observation).expect("a delta"))
                    .map(|payload| payload.mutation.node_id)
            })
            .collect();
        assert_eq!(nodes.len(), 1);
    }

    /// §5.2: an `append` must name the revision it builds on, or the web
    /// assembler refuses it and shows a gap.
    #[test]
    fn the_mutation_chain_is_contiguous() {
        let mut assembler = MessageAssembler::new();
        let folded: Vec<MessagePayload> = recorded_chunks()
            .iter()
            .filter_map(|o| assembler.fold(&message_delta(o).expect("delta")))
            .collect();
        assert_eq!(folded[0].mutation.revision, U64(1));
        assert_eq!(folded[0].mutation.base_revision, None, "open has no base");
        assert_eq!(folded[1].mutation.revision, U64(2));
        assert_eq!(folded[1].mutation.base_revision, Some(U64(1)));
    }

    /// A hook that fires twice for one chunk must not print the text twice.
    #[test]
    fn a_repeated_chunk_is_folded_once() {
        let mut assembler = MessageAssembler::new();
        let chunks = recorded_chunks();
        let delta = message_delta(&chunks[0]).expect("delta");
        assert!(assembler.fold(&delta).is_some());
        assert!(
            assembler.fold(&delta).is_none(),
            "the same chunk index must not append a second time"
        );
    }

    /// Text arriving after `final` would append past the end of a completed
    /// message; the transcript is authoritative by then.
    #[test]
    fn a_chunk_after_the_final_one_is_ignored() {
        let mut assembler = MessageAssembler::new();
        for observation in recorded_chunks() {
            assembler.fold(&message_delta(&observation).expect("delta"));
        }
        let late = hook(
            "MessageDisplay",
            &[
                ("messageId", "1da06679-00ca-474c-8a1f-7a20a272f53d"),
                ("index", "2"),
                ("delta", "9"),
            ],
        );
        assert!(
            assembler
                .fold(&message_delta(&late).expect("delta"))
                .is_none()
        );
    }

    /// Two concurrent messages (a subagent streaming beside the main turn)
    /// must not interleave into one bubble.
    #[test]
    fn separate_messages_keep_separate_nodes() {
        let mut assembler = MessageAssembler::new();
        let a = hook(
            "MessageDisplay",
            &[("messageId", "m-a"), ("index", "0"), ("delta", "alpha")],
        );
        let b = hook(
            "MessageDisplay",
            &[("messageId", "m-b"), ("index", "0"), ("delta", "beta")],
        );
        let first = assembler
            .fold(&message_delta(&a).expect("delta"))
            .expect("fold");
        let second = assembler
            .fold(&message_delta(&b).expect("delta"))
            .expect("fold");
        assert_ne!(first.mutation.node_id, second.mutation.node_id);
        assert_eq!(second.mutation.operation, MutationOperation::Open);
    }

    /// Only `MessageDisplay` on the hook channel is text. A screen guess with
    /// the same name must not be able to inject a message into the journal.
    #[test]
    fn only_a_hook_channel_message_display_is_a_delta() {
        assert!(message_delta(&hook("Stop", &[("delta", "x")])).is_none());
        let mut screen = hook(
            "MessageDisplay",
            &[("messageId", "m"), ("index", "0"), ("delta", "x")],
        );
        screen.source.channel = SourceChannel::Pty;
        assert!(
            message_delta(&screen).is_none(),
            "a screen guess cannot fabricate assistant text"
        );
    }

    /// Headless `-p` fires one chunk carrying the whole message. It must still
    /// produce a complete, readable message rather than being treated as a
    /// fragment awaiting more.
    #[test]
    fn a_single_final_chunk_is_a_complete_message() {
        let mut assembler = MessageAssembler::new();
        let whole = hook(
            "MessageDisplay",
            &[
                ("messageId", "m-1"),
                ("index", "0"),
                ("final", "true"),
                ("delta", "1\n2\n3\n4\n5\n6"),
            ],
        );
        let folded = assembler
            .fold(&message_delta(&whole).expect("delta"))
            .expect("fold");
        assert_eq!(folded.mutation.operation, MutationOperation::Open);
        assert_eq!(folded.status, ContentStatus::Complete);
        assert_eq!(text_of(&folded), "1\n2\n3\n4\n5\n6");
    }

    /// The derived Observation keeps the hook's identity but carries a message.
    #[test]
    fn the_derived_observation_is_a_message_on_the_hook_channel() {
        let mut assembler = MessageAssembler::new();
        let source = recorded_chunks().remove(0);
        let payload = assembler
            .fold(&message_delta(&source).expect("delta"))
            .expect("fold");
        let derived = MessageAssembler::observation(&source, payload);
        assert!(matches!(derived.body, ObservationPayload::Message(_)));
        assert_eq!(derived.source.channel, SourceChannel::Hook);
        assert_eq!(derived.instance_id, source.instance_id);
        assert_ne!(
            derived.event_id, source.event_id,
            "the derived message is its own event, not a rewrite of the hook"
        );
    }
}
