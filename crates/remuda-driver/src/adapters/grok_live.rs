//! File-tier live projection for grok (grok-structural-translation.md §6.2;
//! decisions D1/D2/D4).
//!
//! [`GrokAdapter`] translates the TUI's ACP frames into observations but
//! carries no notion of the nine live phases and no interaction vocabulary.
//! [`GrokLive`] is a pure fold wrapped around it: one poll's ordered
//! observations in, the same facts out, plus:
//!
//! * the live phase tag set (`phase` / `since` / `provision=native` /
//!   `tier=file` / `completeness`) merged onto the existing `turn_started` /
//!   `turn_ended` lifecycles — those names stay load-bearing for the Node
//!   activity fold, so no second turn boundary is minted (D1);
//! * a new `turn.live` native lifecycle for the phases with no host event —
//!   `thinking`, `text-streaming`, `tool-started`, `tool-output`,
//!   `tool-finished` — one observation per phase *episode*. An episode closes
//!   when a different phase opens, so a tool, think, tool turn re-enters
//!   `thinking` after the first tool;
//! * the grok `ask_user_question` tool lifted to `interaction.requested` and
//!   resolved from the completed frame. The file channel has no answer path,
//!   so every request is `answerable=false`, `carrier=native-tty` (D4).
//!
//! `blocked` is deliberately absent from the file tier (D2): the events file
//! is post-hoc and `wait_ms:0` proves nothing about a visible prompt.
//!
//! A `turn.live` lifecycle is invisible to
//! `remuda_node::signal::file_activity` (only `turn_started` / `turn_ended`
//! match), so the extra lifecycles never move Working/Idle themselves.
//!
//! ## Turn slots
//!
//! Update-derived observations carry the native prompt id; the events-file
//! boundaries do not. Both possible interleavings resolve onto one ordered
//! slot per turn:
//!
//! * **live poll** — `turn_started` arrives first, opens a slot, then the
//!   turn's content fills its prompt id; `prompt-accepted` rides the tagged
//!   boundary;
//! * **first poll after attach** — `SessionTail` replays the whole
//!   `updates.jsonl` before `events.jsonl` (the adapter drains updates first),
//!   so every turn's user chunk/thoughts/tools precede every boundary. The
//!   user chunk deterministically anchors `prompt-accepted` on a synthesized
//!   `turn.live`, and the boundary later arrives untagged (it still journals
//!   for the activity fold). Whichever arrives first wins; it is never stamped
//!   twice.
//!
//! The content↔boundary join is **ordinal** today (nth boundary joins the nth
//! ordered slot). The deterministic keys to switch to later are already on the
//! frames: `turnNumber` (events.jsonl `turn_started`) and `promptIndex`
//! (update `_meta`). Finished slots are pruned to the last
//! [`RETAINED_SLOTS`] turns so a long session does not retain every turn's
//! tool maps.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use remuda_protocol::{
    CommandId, DeadlineSource, DeliveryState, EntityLifecycle, HostId, Id, InstanceId, Interaction,
    InteractionCarrier, InteractionId, InteractionKind, InteractionRequest, InteractionRequestKey,
    InteractionResolution, InteractionResolutionReason, InteractionState, Knowledge,
    LifecycleEntity, LifecyclePayload, LifecycleTopic, NativeLifecycle, NativeRequestKey,
    ObservationPayload, QuestionField, QuestionFieldAnswer, QuestionInput, QuestionOption,
    QuestionRequest, RunId, Severity, Timestamp, ToolCallPayload, ToolCallState, ToolResultPayload,
    U64,
};
use remuda_signal::live::{
    COMPLETENESS_KEY, PHASE_KEY, PROVISION_KEY, PROVISION_NATIVE, Phase, SINCE_KEY, TIER_KEY,
};
use remuda_signal::question::resolved_in_terminal;
use serde_json::Value;

use super::{AdapterObservation, FileSignalAdapter, GrokAdapter};
use crate::error::DriverResult;

/// Native lifecycle name carrying file-tier phases with no host event.
pub const TURN_LIVE_NAME: &str = "turn.live";
/// File-tier spelling for the `tier` fidelity tag (`channelHealth.ts`).
const TIER_FILE: &str = "file";
/// The grok tool that is a question form rather than an ordinary tool card.
const ASK_USER_QUESTION: &str = "ask_user_question";
/// Finished turn slots retained after a `turn_ended` sweep; older slots are
/// pruned so a long session keeps bounded fold state.
const RETAINED_SLOTS: usize = 8;
/// Bound on pruned prompt ids remembered so late frames for a pruned turn are
/// journaled without re-opening live evidence.
const CLOSED_TURNS_CAP: usize = 4096;

/// Instance identity the projection stamps interaction entities with.
///
/// The adapter never owns run identity; the supervisor hands over exactly the
/// same [`super::StampCtx`] fields it stamps observation envelopes with.
#[derive(Clone)]
pub struct LiveIdentity {
    /// Owning instance.
    pub instance_id: InstanceId,
    /// Owning host.
    pub host_id: HostId,
    /// Current run.
    pub run_id: RunId,
}

/// Wraps a [`GrokAdapter`] with the file-tier live fold.
pub struct GrokLive {
    inner: GrokAdapter,
    identity: LiveIdentity,
    projection: LiveProjection,
}

impl GrokLive {
    /// Wrap a grok adapter. `identity` stamps the interaction entities the
    /// projection mints.
    #[must_use]
    pub fn new(inner: GrokAdapter, identity: LiveIdentity) -> Self {
        Self {
            inner,
            identity,
            projection: LiveProjection::default(),
        }
    }
}

impl FileSignalAdapter for GrokLive {
    fn kind(&self) -> remuda_protocol::AgentKind {
        self.inner.kind()
    }

    fn session_id(&self) -> Option<&str> {
        self.inner.session_id()
    }

    fn confirm_session(&mut self, session_id: &str) {
        self.inner.confirm_session(session_id);
    }

    fn poll(&mut self) -> DriverResult<Vec<AdapterObservation>> {
        let observed = self.inner.poll()?;
        Ok(self
            .projection
            .fold_batch(observed, &self.identity, now_ts()))
    }
}

/// One tool call's open phase episodes within one turn slot.
#[derive(Default)]
struct ToolEpisode {
    /// The `tool-started` lifecycle was already emitted for this call.
    started: bool,
    /// The `tool-output` lifecycle was already emitted for this call.
    output: bool,
    /// The `tool-finished` lifecycle was already emitted for this call.
    finished: bool,
}

/// One turn's projection state, keyed by its slot index.
#[derive(Default)]
struct TurnSlot {
    /// Native prompt id once an update-derived observation named it.
    turn_id: Option<String>,
    /// The slot already received the human's user message.
    has_user: bool,
    /// `prompt-accepted` was emitted (on the boundary or the user chunk,
    /// whichever arrived first).
    anchored: bool,
    /// The phase currently latched in this turn; re-entry opens a new episode.
    current: Option<Phase>,
    /// Per-call phase episodes already emitted.
    tools: HashMap<String, ToolEpisode>,
    /// Native tool names remembered through the call, for finish-time tags.
    tool_names: HashMap<String, String>,
}

/// A question whose `interaction.requested` has been emitted and whose
/// resolution is owed.
struct PendingQuestion {
    /// The interaction entity as requested; cloned into the resolution.
    interaction: Interaction,
    /// The request's `rawInput.questions[]`, for answer label matching.
    questions: Vec<Value>,
    /// Turn slot the question belongs to; its `turn_ended` sweep owns it.
    slot: usize,
}

/// Fold state carried across polls. Pure apart from the injected clock.
#[derive(Default)]
struct LiveProjection {
    /// Live turn slots, oldest at the front; pruned past [`RETAINED_SLOTS`].
    slots: VecDeque<TurnSlot>,
    /// Absolute turn ordinal of `slots[0]`; shifts when slots are pruned.
    slot_base: usize,
    /// Native prompt id → local `slots` index (only live slots).
    by_turn: HashMap<String, usize>,
    /// Prompt ids whose slots were pruned; their late frames carry no phase.
    closed: HashSet<String>,
    /// FIFO order of `closed`, so the memory bound evicts oldest first.
    closed_order: VecDeque<String>,
    /// Boundary counters: nth `turn_started` / `turn_ended` names its slot.
    started: usize,
    ended: usize,
    /// Open questions by native tool call id.
    pending: HashMap<String, PendingQuestion>,
    /// Insertion order of `pending`, so sweeps stay deterministic.
    pending_order: Vec<String>,
}

impl LiveProjection {
    /// Fold one poll's observations into the same observations plus live phase
    /// evidence and question interactions.
    fn fold_batch(
        &mut self,
        observations: Vec<AdapterObservation>,
        identity: &LiveIdentity,
        now: Timestamp,
    ) -> Vec<AdapterObservation> {
        let mut out = Vec::with_capacity(observations.len() + 8);
        for observed in observations {
            self.project_one(observed, identity, &now, &mut out);
        }
        out
    }

    fn project_one(
        &mut self,
        observed: AdapterObservation,
        identity: &LiveIdentity,
        now: &Timestamp,
        out: &mut Vec<AdapterObservation>,
    ) {
        match &observed.payload {
            ObservationPayload::Lifecycle(box_payload) => match box_payload.as_ref() {
                LifecyclePayload::Native(native)
                    if native.topic == LifecycleTopic::Turn
                        && native.native_name == "turn_started" =>
                {
                    self.begin_turn(&observed, now, out);
                }
                LifecyclePayload::Native(native)
                    if native.topic == LifecycleTopic::Turn
                        && native.native_name == "turn_ended" =>
                {
                    self.end_turn(&observed, now, out);
                }
                _ => out.push(observed),
            },
            ObservationPayload::Thought(thought) => {
                // The turn_completed close snapshot is the same episode's
                // terminal write, not a new thinking phase.
                if thought.status != remuda_protocol::ContentStatus::Streaming
                    || self.is_closed(observed.turn_id.as_deref())
                {
                    out.push(observed);
                    return;
                }
                let slot = self.content_slot(observed.turn_id.as_deref());
                if self.slots[slot].current != Some(Phase::Thinking) {
                    self.slots[slot].current = Some(Phase::Thinking);
                    out.push(self.phase_lifecycle(
                        Phase::Thinking,
                        now,
                        None,
                        self.slots[slot].turn_id.clone(),
                    ));
                }
                out.push(observed);
            }
            ObservationPayload::Message(message) => {
                let role = message.role;
                if role == remuda_protocol::MessageRole::User {
                    // The user echo opens the next turn slot (the human prompt
                    // is the first update frame of each turn).
                    let slot = self.user_slot();
                    if !self.slots[slot].anchored {
                        let mut lifecycle = self.phase_lifecycle(
                            Phase::PromptAccepted,
                            now,
                            None,
                            self.slots[slot].turn_id.clone(),
                        );
                        if let ObservationPayload::Lifecycle(box_payload) = &mut lifecycle.payload
                            && let LifecyclePayload::Native(native) = box_payload.as_mut()
                        {
                            native
                                .related_ids
                                .insert("anchor".into(), "user-message".into());
                        }
                        self.slots[slot].anchored = true;
                        self.slots[slot].current = Some(Phase::PromptAccepted);
                        out.push(lifecycle);
                    }
                    out.push(observed);
                    return;
                }
                let slot = self.content_slot(observed.turn_id.as_deref());
                if role == remuda_protocol::MessageRole::Assistant
                    && message.status == remuda_protocol::ContentStatus::Streaming
                    && !self.is_closed(observed.turn_id.as_deref())
                    && self.slots[slot].current != Some(Phase::TextStreaming)
                {
                    self.slots[slot].current = Some(Phase::TextStreaming);
                    let mut lifecycle = self.phase_lifecycle(
                        Phase::TextStreaming,
                        now,
                        None,
                        self.slots[slot].turn_id.clone(),
                    );
                    if let ObservationPayload::Lifecycle(box_payload) = &mut lifecycle.payload
                        && let LifecyclePayload::Native(native) = box_payload.as_mut()
                    {
                        native
                            .related_ids
                            .insert("messageId".into(), message.message_id.as_str().to_owned());
                    }
                    out.push(lifecycle);
                }
                out.push(observed);
            }
            ObservationPayload::ToolCall(call) => {
                let call = (**call).clone();
                self.project_tool_call(observed, &call, identity, now, out);
            }
            ObservationPayload::ToolResult(result) => {
                let result = (**result).clone();
                self.project_tool_result(observed, &result, now, out);
            }
            _ => out.push(observed),
        }
    }

    /// Resolve (creating if needed) the slot for prompt-bearing content.
    fn content_slot(&mut self, turn_id: Option<&str>) -> usize {
        if let Some(turn_id) = turn_id
            && !turn_id.is_empty()
        {
            if let Some(slot) = self.by_turn.get(turn_id).copied() {
                return slot;
            }
            // First sighting: fill the earliest boundary-created slot that has
            // no prompt id yet (live ordering), else append (replay ordering).
            if let Some(slot) = self.slots.iter().position(|slot| slot.turn_id.is_none()) {
                self.slots[slot].turn_id = Some(turn_id.to_owned());
                self.by_turn.insert(turn_id.to_owned(), slot);
                return slot;
            }
            let slot = self.slots.len();
            self.slots.push_back(TurnSlot {
                turn_id: Some(turn_id.to_owned()),
                ..TurnSlot::default()
            });
            self.by_turn.insert(turn_id.to_owned(), slot);
            return slot;
        }
        // Content without a prompt id joins the earliest unnamed slot, else the
        // most recent one.
        if let Some(slot) = self.slots.iter().position(|slot| slot.turn_id.is_none()) {
            return slot;
        }
        self.latest_or_new_slot()
    }

    /// The newest live slot, opening one when the fold has none yet. A fold
    /// must never index an empty slot vec: prompt-less content (frames with no
    /// `_meta.promptId`) can legally be the first observation.
    fn latest_or_new_slot(&mut self) -> usize {
        if self.slots.is_empty() {
            self.slots.push_back(TurnSlot::default());
        }
        self.slots.len() - 1
    }

    /// True when `turn_id` names a turn whose slot was pruned; its late frames
    /// are journaled as facts but open no new live evidence.
    fn is_closed(&self, turn_id: Option<&str>) -> bool {
        turn_id.is_some_and(|turn_id| self.closed.contains(turn_id))
    }

    /// Resolve the slot a user echo belongs to: the earliest slot that has not
    /// received its user message yet (live ordering: the boundary opened it),
    /// else a fresh appended slot (replay ordering).
    fn user_slot(&mut self) -> usize {
        if let Some(slot) = self.slots.iter().position(|slot| !slot.has_user) {
            self.slots[slot].has_user = true;
            return slot;
        }
        let slot = self.slots.len();
        self.slots.push_back(TurnSlot {
            has_user: true,
            ..TurnSlot::default()
        });
        slot
    }

    /// Resolve the local slot for the nth boundary. `None` when the boundary
    /// names an already-pruned turn (a duplicate/late event): the raw
    /// observation journals, but carries no phase tag.
    fn boundary_slot(&mut self, index: usize) -> Option<usize> {
        if index < self.slot_base {
            return None;
        }
        let local = index - self.slot_base;
        while self.slots.len() <= local {
            self.slots.push_back(TurnSlot::default());
        }
        Some(local)
    }

    /// Drop finished slots past [`RETAINED_SLOTS`], keeping the most recent.
    fn prune(&mut self) {
        while self.slots.len() > RETAINED_SLOTS {
            let Some(removed) = self.slots.pop_front() else {
                break;
            };
            self.slot_base += 1;
            if let Some(turn_id) = removed.turn_id {
                self.closed.insert(turn_id.clone());
                self.closed_order.push_back(turn_id);
                while self.closed.len() > CLOSED_TURNS_CAP
                    && let Some(oldest) = self.closed_order.pop_front()
                {
                    self.closed.remove(&oldest);
                }
            }
            // Surviving locals shift down by one. Every remaining pending
            // question belongs to a surviving slot: a pruned slot's pending was
            // swept at that slot's `turn_ended`.
            for pending in self.pending.values_mut() {
                pending.slot = pending.slot.saturating_sub(1);
            }
        }
        self.by_turn.clear();
        for (local, slot) in self.slots.iter().enumerate() {
            if let Some(turn_id) = &slot.turn_id {
                self.by_turn.insert(turn_id.clone(), local);
            }
        }
    }

    #[cfg(test)]
    fn live_slot_count(&self) -> usize {
        self.slots.len()
    }

    /// Emit (or pass through) the `turn_started` boundary and anchor the slot.
    fn begin_turn(
        &mut self,
        observed: &AdapterObservation,
        now: &Timestamp,
        out: &mut Vec<AdapterObservation>,
    ) {
        let Some(slot) = self.boundary_slot(self.started) else {
            out.push(observed.clone());
            return;
        };
        self.started += 1;
        let turn_id = self.slots[slot].turn_id.clone();
        let mut observed = observed.clone();
        // Replay ordering: the user chunk already anchored prompt-accepted on
        // a synthesized turn.live. The boundary still journals (the Node
        // activity fold matches its name), but the phase is not restamped.
        if !self.slots[slot].anchored {
            merge_phase_tags(
                &mut observed,
                Phase::PromptAccepted,
                now,
                turn_id.as_deref(),
                None,
            );
            self.slots[slot].anchored = true;
            self.slots[slot].current = Some(Phase::PromptAccepted);
        }
        out.push(observed);
    }

    /// Resolve every still-pending question for this slot (a pending
    /// interaction pins the composer), tag the boundary `turn-ended` /
    /// `interrupted`, then prune finished slots.
    fn end_turn(
        &mut self,
        observed: &AdapterObservation,
        now: &Timestamp,
        out: &mut Vec<AdapterObservation>,
    ) {
        let Some(slot) = self.boundary_slot(self.ended) else {
            out.push(observed.clone());
            return;
        };
        self.ended += 1;
        // Sweep this slot's questions in insertion order.
        let owed: Vec<String> = self
            .pending_order
            .iter()
            .filter(|call_id| self.pending.get(*call_id).is_some_and(|p| p.slot == slot))
            .cloned()
            .collect();
        for call_id in owed {
            if let Some(pending) = self.pending.remove(&call_id) {
                self.pending_order.retain(|id| id != &call_id);
                out.push(entity_observation(
                    cleared_native(pending.interaction, now),
                    "native-cleared",
                    self.slots[slot].turn_id.clone(),
                    Some(call_id),
                ));
            }
        }
        let cancelled = observed_cancelled(observed);
        let phase = if cancelled {
            Phase::Interrupted
        } else {
            Phase::TurnEnded
        };
        let turn_id = self.slots[slot].turn_id.clone();
        let mut observed = observed.clone();
        merge_phase_tags(&mut observed, phase, now, turn_id.as_deref(), None);
        self.slots[slot].current = Some(phase);
        out.push(observed);
        self.prune();
    }

    /// Project one Proposed/Running tool call, opening its question and
    /// phase episodes.
    fn project_tool_call(
        &mut self,
        observed: AdapterObservation,
        call: &ToolCallPayload,
        identity: &LiveIdentity,
        now: &Timestamp,
        out: &mut Vec<AdapterObservation>,
    ) {
        let Some(call_id) = observed.item_id.clone() else {
            out.push(observed);
            return;
        };
        // A late frame for a pruned turn is journaled but opens no phase and
        // no interaction on a newer turn.
        if self.is_closed(observed.turn_id.as_deref()) {
            out.push(observed);
            return;
        }
        let slot = self.content_slot(observed.turn_id.as_deref());
        let name = known_string(&call.tool_name);
        if let Some(name) = &name {
            self.slots[slot]
                .tool_names
                .insert(call_id.clone(), name.clone());
        }
        // The Pending frame of the question tool becomes interaction.requested.
        if call.state == ToolCallState::Proposed
            && name.as_deref() == Some(ASK_USER_QUESTION)
            && !self.pending.contains_key(&call_id)
            && let Some(input) = known_value(&call.input)
            && let Some(interaction) = self.open_question(&call_id, slot, identity, input, now)
        {
            let mut request =
                AdapterObservation::structured(ObservationPayload::InteractionRequested(Box::new(
                    remuda_protocol::InteractionRequestedPayload { interaction },
                )));
            request.item_id = Some(call_id.clone());
            request.turn_id = observed.turn_id.clone();
            out.push(request);
        }
        let phase = if call.state == ToolCallState::Running {
            Phase::ToolOutput
        } else {
            Phase::ToolStarted
        };
        let episode = self.slots[slot].tools.entry(call_id.clone()).or_default();
        let opens = match phase {
            Phase::ToolStarted if !episode.started => {
                episode.started = true;
                true
            }
            Phase::ToolOutput if !episode.output => {
                episode.output = true;
                true
            }
            _ => false,
        };
        if opens {
            self.slots[slot].current = Some(phase);
            out.push(self.phase_lifecycle(
                phase,
                now,
                Some((call_id.clone(), name.clone())),
                self.slots[slot].turn_id.clone(),
            ));
        }
        out.push(observed);
    }

    /// Project one terminal tool result: the `tool-finished` phase and the
    /// question resolution when the call was a question.
    fn project_tool_result(
        &mut self,
        observed: AdapterObservation,
        result: &ToolResultPayload,
        now: &Timestamp,
        out: &mut Vec<AdapterObservation>,
    ) {
        let call_id = observed.item_id.clone();
        // Late result for a pruned turn: journal the fact, open no phase.
        if self.is_closed(observed.turn_id.as_deref()) {
            out.push(observed);
            return;
        }
        let slot = self.content_slot(observed.turn_id.as_deref());
        if let Some(id) = &call_id {
            let episode = self.slots[slot].tools.entry(id.clone()).or_default();
            if !episode.finished {
                episode.finished = true;
                let name = self.slots[slot].tool_names.get(id).cloned();
                self.slots[slot].current = Some(Phase::ToolFinished);
                out.push(self.phase_lifecycle(
                    Phase::ToolFinished,
                    now,
                    Some((id.clone(), name)),
                    self.slots[slot].turn_id.clone(),
                ));
            }
        }
        out.push(observed);
        if let Some(call_id) = call_id
            && let Some(pending) = self.pending.remove(&call_id)
        {
            self.pending_order.retain(|id| id != &call_id);
            let answers = harness_answers(&pending.questions, result);
            let (interaction, reason_code) = match answers {
                Some(answers) => (
                    resolved_in_terminal(
                        pending.interaction,
                        answers,
                        now.clone(),
                        CommandId::new(),
                    ),
                    "terminal-answered",
                ),
                None => (cleared_native(pending.interaction, now), "native-cleared"),
            };
            let resolution = entity_observation(
                interaction,
                reason_code,
                self.slots[slot].turn_id.clone(),
                Some(call_id),
            );
            out.push(resolution);
        }
    }

    /// Build and remember the pending question interaction for one Pending
    /// frame. `None` when the frame carries no usable `questions[]`.
    fn open_question(
        &mut self,
        call_id: &str,
        slot: usize,
        identity: &LiveIdentity,
        raw_input: &Value,
        now: &Timestamp,
    ) -> Option<Interaction> {
        let questions = raw_input.get("questions").and_then(Value::as_array)?;
        let fields = questions
            .iter()
            .enumerate()
            .map(|(index, question)| question_field(index, question))
            .collect::<Vec<_>>();
        if fields.is_empty() {
            return None;
        }
        let interaction = question_interaction(identity, fields, now);
        self.pending.insert(
            call_id.to_owned(),
            PendingQuestion {
                interaction: interaction.clone(),
                questions: questions.clone(),
                slot,
            },
        );
        self.pending_order.push(call_id.to_owned());
        Some(interaction)
    }

    /// Build a `turn.live` native lifecycle carrying one phase.
    fn phase_lifecycle(
        &self,
        phase: Phase,
        now: &Timestamp,
        tool: Option<(String, Option<String>)>,
        prompt: Option<String>,
    ) -> AdapterObservation {
        let mut related = phase_tags(phase, now, prompt.as_deref(), None);
        if let Some((call_id, name)) = tool {
            related.insert("toolCallId".into(), call_id);
            if let Some(name) = name {
                related.insert("toolName".into(), name);
            }
        }
        let payload = ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
            NativeLifecycle {
                topic: LifecycleTopic::Turn,
                native_name: TURN_LIVE_NAME.into(),
                native_id: Knowledge::NotApplicable,
                status: Knowledge::Known {
                    value: "working".into(),
                },
                related_ids: related,
                data_ref: None,
                severity: Severity::Info,
                affects_completion: false,
            },
        ))));
        let mut observed = AdapterObservation::structured(payload);
        observed.turn_id = prompt;
        observed
    }
}

/// Merge the file-tier phase tag set onto an existing turn lifecycle
/// observation (D1: the phase rides the host event, no duplicate boundary).
fn merge_phase_tags(
    observed: &mut AdapterObservation,
    phase: Phase,
    now: &Timestamp,
    prompt: Option<&str>,
    tool: Option<(&str, Option<&str>)>,
) {
    if let ObservationPayload::Lifecycle(box_payload) = &mut observed.payload
        && let LifecyclePayload::Native(native) = box_payload.as_mut()
    {
        native
            .related_ids
            .extend(phase_tags(phase, now, prompt, tool));
    }
}

/// Fill the common file-tier tag set for one phase transition.
fn phase_tags(
    phase: Phase,
    now: &Timestamp,
    prompt: Option<&str>,
    tool: Option<(&str, Option<&str>)>,
) -> BTreeMap<String, String> {
    let mut related = BTreeMap::new();
    related.insert(PHASE_KEY.into(), phase.as_str().into());
    related.insert(SINCE_KEY.into(), String::from(now.clone()));
    related.insert(PROVISION_KEY.into(), PROVISION_NATIVE.into());
    related.insert(TIER_KEY.into(), TIER_FILE.into());
    // File evidence is structured except for the streaming phases: thought and
    // text chunks and the Running mutation are partial by construction
    // (live-structured-view.md §2.7 grok row).
    related.insert(
        COMPLETENESS_KEY.into(),
        if matches!(
            phase,
            Phase::Thinking | Phase::ToolOutput | Phase::TextStreaming
        ) {
            "partial"
        } else {
            "structured"
        }
        .into(),
    );
    if let Some(prompt) = prompt {
        related.insert("promptId".into(), prompt.to_owned());
    }
    if let Some((call_id, name)) = tool {
        related.insert("toolCallId".into(), call_id.to_owned());
        if let Some(name) = name {
            related.insert("toolName".into(), name.to_owned());
        }
    }
    related
}

/// Translate one grok `rawInput.questions[]` element into a protocol field.
fn question_field(index: usize, question: &Value) -> QuestionField {
    let title = question
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or("question")
        .to_owned();
    let options = question
        .get("options")
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|option| {
                    let label = option.get("label").and_then(Value::as_str)?;
                    // The label is the id: the completed frame identifies the
                    // answer by its label text.
                    Some(QuestionOption {
                        id: label.to_owned(),
                        label: label.to_owned(),
                        description: option
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let multi = question
        .get("multiSelect")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    QuestionField {
        id: format!("q{index}"),
        title,
        description: None,
        input: if multi {
            QuestionInput::MultiSelect
        } else {
            QuestionInput::SingleSelect
        },
        required: true,
        options,
        // The file channel has no answer path (D4); do not pretend typed
        // answers could be delivered.
        allow_free_text: false,
        sensitive: false,
    }
}

/// Build the non-answerable native-tty question interaction for one Pending
/// frame.
fn question_interaction(
    identity: &LiveIdentity,
    fields: Vec<QuestionField>,
    now: &Timestamp,
) -> Interaction {
    Interaction {
        meta: remuda_protocol::EntityMeta {
            id: InteractionId::new(),
            revision: U64(1),
            created_at: now.clone(),
            updated_at: now.clone(),
        },
        instance_id: identity.instance_id.clone(),
        run_id: Some(identity.run_id.clone()),
        host_id: identity.host_id.clone(),
        kind: InteractionKind::Question,
        request_key: InteractionRequestKey {
            // ACP files are an observation channel, not a request we
            // participate in; there is no native request key to name.
            native: NativeRequestKey::None,
            process_generation: U64(1),
            run_generation: Some(U64(1)),
            connection_epoch: Id::new("epoch").expect("epoch is a registered prefix"),
        },
        request_version: U64(1),
        state: InteractionState::Pending,
        blocking: true,
        // D4: the answer can only be typed into the native TTY; Remuda sees
        // it after the fact, never in time to deliver one.
        answerable: false,
        carrier: InteractionCarrier::NativeTty,
        request: InteractionRequest::Question(Box::new(QuestionRequest {
            title: ASK_USER_QUESTION.into(),
            fields,
        })),
        deadline: unknown("native-tty"),
        deadline_source: DeadlineSource::None,
        answer: unknown("pending"),
        delivery: DeliveryState::NotSent,
        resolution: unknown("pending"),
    }
}

/// Settle a question interaction as resolved by the harness without a
/// committed answer (no offered label matched, or a turn-end sweep).
fn cleared_native(mut interaction: Interaction, now: &Timestamp) -> Interaction {
    interaction.meta.revision.0 += 1;
    interaction.meta.updated_at = now.clone();
    interaction.state = InteractionState::Resolved;
    interaction.blocking = false;
    interaction.answerable = false;
    interaction.delivery = DeliveryState::NotSent;
    interaction.answer = unknown("native-cleared");
    interaction.resolution = Knowledge::Known {
        value: InteractionResolution {
            reason: InteractionResolutionReason::NativeCleared,
            event_ids: Vec::new(),
        },
    };
    interaction
}

/// Wrap a resolved interaction entity in an entity-lifecycle observation.
fn entity_observation(
    interaction: Interaction,
    reason_code: &'static str,
    turn_id: Option<String>,
    item_id: Option<String>,
) -> AdapterObservation {
    let entity_id = interaction.meta.id.as_id().clone();
    let revision = interaction.meta.revision;
    let mut observed = AdapterObservation::structured(ObservationPayload::Lifecycle(Box::new(
        LifecyclePayload::Entity(Box::new(EntityLifecycle {
            entity_id,
            revision,
            previous_state: Some("pending".into()),
            state: "resolved".into(),
            reason_code: reason_code.into(),
            evidence_event_ids: Vec::new(),
            entity_value: LifecycleEntity::Interaction(Box::new(interaction)),
        })),
    )));
    observed.turn_id = turn_id;
    observed.item_id = item_id;
    observed
}

/// Reconstruct the card answer from the completed frame.
///
/// The grok 1.0.30 fixture records only
/// `rawOutput.UserAnswered.message` — a display sentence of the shape
/// `User has answered your questions: "Q"="Alpha". …` — so the label is
/// recovered by matching `="…"` assignments against the offered option
/// labels. A structured `rawOutput.UserAnswered.answers` object (the shape a
/// future client may write) is honoured first. **The message grammar beyond
/// the quoted assignment is synthesized from docs, not captured** ([U],
/// design doc §5/PR7): the 1.0.34 recapture revisits it.
fn harness_answers(
    questions: &[Value],
    result: &ToolResultPayload,
) -> Option<BTreeMap<String, QuestionFieldAnswer>> {
    let frame = known_value(&result.structured_result)?;
    let user_answered = frame.pointer("/rawOutput/UserAnswered")?;
    let questions_value = Value::Array(questions.to_vec());
    if let Some(structured) = user_answered.get("answers") {
        // [U] structured answers object, keyed by question text.
        let mapped = remuda_signal::question::answer_from_harness(&questions_value, structured)?;
        // Every question must be present and every value a recognised option
        // label; free text seen from the file tier is not committable (D4).
        if mapped.len() == questions.len()
            && mapped.values().all(|field| !field.option_ids.is_empty())
        {
            return Some(mapped);
        }
        return None;
    }
    let message = user_answered.get("message").and_then(Value::as_str)?;
    message_answers(&questions_value, message)
}

/// Match the `="…"` assignments in a `UserAnswered.message` against the
/// offered labels. Returns `None` when any question has no label match.
fn message_answers(
    questions: &Value,
    message: &str,
) -> Option<BTreeMap<String, QuestionFieldAnswer>> {
    let questions = questions.as_array()?;
    let values = quoted_assignments(message);
    if values.is_empty() {
        return None;
    }
    let mut answers = BTreeMap::new();
    for (index, question) in questions.iter().enumerate() {
        let candidate = if questions.len() == 1 {
            // One question: any extra quoted assignment folds into its answer
            // (a multi-select answer may render comma-joined).
            values.join(", ")
        } else {
            values.get(index)?.clone()
        };
        let labels = option_labels(question);
        let option_ids = if labels.contains(&candidate.as_str()) {
            vec![candidate]
        } else {
            let multi = question.get("multiSelect").and_then(Value::as_bool) == Some(true);
            let pieces = candidate.split(", ").collect::<Vec<_>>();
            if multi
                && pieces.iter().all(|label| labels.contains(label))
                && pieces.first().is_some_and(|label| !label.is_empty())
            {
                pieces.into_iter().map(str::to_owned).collect()
            } else {
                Vec::new()
            }
        };
        if option_ids.is_empty() {
            return None;
        }
        answers.insert(
            format!("q{index}"),
            QuestionFieldAnswer {
                option_ids,
                text: None,
            },
        );
    }
    Some(answers)
}

/// Labels offered by one raw question, in order.
fn option_labels(question: &Value) -> Vec<&str> {
    question
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|option| option.get("label").and_then(Value::as_str))
        .collect()
}

/// Read the values assigned with `="` in a `UserAnswered.message`, in order.
fn quoted_assignments(message: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut from = 0;
    while from < message.len() {
        let Some(rel) = message[from..].find("=\"") else {
            break;
        };
        let start = from + rel + 2;
        let Some(quote_rel) = message[start..].find('"') else {
            break;
        };
        let end = start + quote_rel;
        values.push(message[start..end].to_owned());
        from = end + 1;
    }
    values
}

/// True when a `turn_ended` observation carries `outcome=cancelled`.
fn observed_cancelled(observed: &AdapterObservation) -> bool {
    if let ObservationPayload::Lifecycle(box_payload) = &observed.payload
        && let LifecyclePayload::Native(native) = box_payload.as_ref()
    {
        return native.related_ids.get("outcome").map(String::as_str) == Some("cancelled");
    }
    false
}

/// Read a `Known` string.
fn known_string(knowledge: &Knowledge<String>) -> Option<String> {
    match knowledge {
        Knowledge::Known { value } => Some(value.clone()),
        _ => None,
    }
}

/// Read a `Known` JSON value.
fn known_value(knowledge: &Knowledge<Value>) -> Option<&Value> {
    match knowledge {
        Knowledge::Known { value } => Some(value),
        _ => None,
    }
}

/// The standard unknown for file-tier interaction fields.
fn unknown<T>(reason: &str) -> Knowledge<T> {
    Knowledge::Unknown {
        reason: reason.into(),
        evidence_event_ids: Vec::new(),
    }
}

/// Millisecond-precision UTC, matching the supervisor stamp clock.
fn now_ts() -> Timestamp {
    let now = time::OffsetDateTime::now_utc();
    let date = now.date();
    let (hour, minute, second) = now.time().as_hms();
    let formatted = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        date.year(),
        u8::from(date.month()),
        date.day(),
        hour,
        minute,
        second,
        now.millisecond(),
    );
    Timestamp::try_from(formatted).expect("clock output is valid RFC3339-ms")
}

#[cfg(test)]
mod tests;
