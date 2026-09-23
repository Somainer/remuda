//! [`SignalBus`]: hook events in, [`Observation`]s out (D-028 §4.3, P1).
//!
//! The bus owns the stamping (`seq`, ids, source envelope) and the small amount
//! of state a hook stream needs: which native session this agent reported, and
//! which pid it reported it from. Classification itself lives in [`crate::map`].
//!
//! Non-blocking events are journaled and answered with no opinion; blocking
//! events (`PermissionRequest`, `Elicitation`) additionally open an
//! interaction and park the hook on [`PendingDecisions`] until a device
//! answers or the bounded wait denies (D-028 §4.4 tier A, P5).

use crate::event::{HookEnvelope, HookEvent, HookReply};
use crate::live::LiveState;
use crate::map::{Mapped, MappedKind, map_event};
use crate::socket::SignalSink;
use remuda_protocol::{
    CommandId, Completeness, EntityLifecycle, EventId, HostId, Id, InstanceId, Knowledge,
    LifecycleEntity, LifecyclePayload, NativeRequestKey, Observation, ObservationPayload,
    ObservationSource, RunId, RuntimeCursor, SchemaVersion, SourceChannel, SourceCursor,
    SourceDelivery, Timestamp, U64,
};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use tokio::sync::mpsc;

/// Identity every Observation this bus emits is stamped with.
#[derive(Clone, Debug)]
pub struct BusContext {
    /// Owning instance.
    pub instance_id: InstanceId,
    /// Owning host.
    pub host_id: HostId,
    /// Journal this instance writes to.
    pub journal_id: Id,
    /// Current run.
    pub run_id: RunId,
    /// Driver this bus is attached to (`shell-pty` in P1).
    pub driver_kind: remuda_protocol::DriverKind,
    /// Adapter version recorded in the source envelope.
    pub adapter_version: String,
}

/// What the bus learned from a `SessionStart`.
///
/// `pid` is the agent process, taken from the relay's parent. The Node binds
/// a promoted shell-pty instance to this session by matching `pid` against the
/// PTY's foreground process group leader, which is what makes the binding
/// deterministic when two terminals each run their own `claude`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionBinding {
    /// Agent pid that reported this session.
    pub pid: i32,
    /// Native session id, for `--resume` (D-026).
    pub session_id: String,
    /// Transcript the agent is writing.
    pub transcript_path: Option<String>,
}

/// Receives hook events, journals them, answers the agent.
pub struct SignalBus {
    context: BusContext,
    events: mpsc::Sender<Observation>,
    seq: Arc<AtomicU64>,
    binding: std::sync::Mutex<Option<SessionBinding>>,
    pending: crate::pending::PendingDecisions,
    /// What each parked hook needs to turn a device's protocol answer back
    /// into the decision JSON the harness reads. Keyed by the interaction id,
    /// which is also the decision key.
    parked: std::sync::Mutex<std::collections::HashMap<crate::pending::DecisionKey, ParkedHook>>,
    /// Open AskUserQuestion cards, keyed the same way. A `PostToolUse` whose
    /// answers were produced by the TUI closes one of these even when no
    /// device answered through Remuda.
    open_questions:
        std::sync::Mutex<std::collections::HashMap<crate::pending::DecisionKey, OpenQuestion>>,
    /// In-flight AskUserQuestion calls, keyed by a content fingerprint of
    /// their `tool_input`. Auto mode raises the question as a `PreToolUse` and
    /// — when that hook returns no opinion — the same call again as a
    /// `PermissionRequest`; both hooks park, but only the first opens a card
    /// and one answer must resolve both (evidence `askq-pretooluse-1.md`).
    question_calls: std::sync::Mutex<std::collections::HashMap<String, QuestionCall>>,
    blocking_wait: std::time::Duration,
    /// P5 (c-hookgap): turns observed on the hook channel, most recent last,
    /// plus the pid a `Stop`/`Esc` interrupt is aimed at.
    turn_active: std::sync::Mutex<VecDeque<TurnState>>,
    /// Live phase fold + deterministic tool node ids (design §3.1).
    live: std::sync::Mutex<LiveState>,
    interrupt_pid: Arc<AtomicI32>,
}

/// The context kept for one parked blocking hook, so an answer arriving later
/// can be turned back into the decision the harness reads.
struct ParkedHook {
    /// Suggestions the harness offered; an allow-always answer echoes one.
    suggestions: Vec<crate::PermissionSuggestion>,
    /// The original permission request, kept while the parked event is an
    /// AskUserQuestion so a card answer can rebuild `updatedInput.answers`.
    question: Option<crate::PermissionRequestEvent>,
    /// Receiver for the decision, registered before the card was emitted on
    /// the delivery path; `None` for a direct fold that never blocks.
    decision_rx: Option<tokio::sync::oneshot::Receiver<crate::decision::HookDecision>>,
}

/// An open AskUserQuestion card that the TUI may answer on its own.
///
/// The hook carrier's interaction is normally closed by a device answer. But
/// claude renders its own form while the hook is pending (measured,
/// native-pty-5 §5.1), and when the human completes it there the closing
/// evidence is the `PostToolUse` carrying `answers`. That observation closes
/// the card here so no banner stays stuck on 「等待操作」.
#[derive(Clone)]
struct OpenQuestion {
    /// The card as emitted; restamped terminal-resolved on close.
    interaction: remuda_protocol::Interaction,
    /// The original request, used to validate the observed answers.
    request: crate::PermissionRequestEvent,
}

/// One in-flight AskUserQuestion call and the two hooks it may park.
///
/// The harness can raise the same call twice — a `PreToolUse` and, when that
/// hook does not decide it, a `PermissionRequest` ~tens of ms later (measured
/// on claude 2.1.277; the pair carries byte-identical `tool_input`). Only the
/// first hook opens a card; the second parks beside it and one device answer
/// resolves both.
#[derive(Clone)]
struct QuestionCall {
    /// Decision key of the hook that opened the card (the interaction id).
    primary: crate::pending::DecisionKey,
    /// Decision key of the paired hook, once it has arrived.
    secondary: Option<crate::pending::DecisionKey>,
}

/// Fingerprint one AskUserQuestion call across its two hook events.
///
/// The paired `PreToolUse` / `PermissionRequest` carry identical
/// `tool_input` (measured), but only the `PreToolUse` has a `tool_use_id`.
/// Content is therefore the correlation the two hooks can actually share.
/// The `agent_id` scopes it: a parked question blocks its agent's turn, so one
/// agent never has two of these in flight, but the main session and a
/// sub-agent (or two sub-agents) may ask the same question text concurrently
/// and must not be folded onto one card.
fn question_fingerprint(agent_id: Option<&str>, tool_input: &serde_json::Value) -> String {
    let body = tool_input
        .get("questions")
        .and_then(|q| serde_json::to_string(q).ok())
        .unwrap_or_else(|| serde_json::to_string(tool_input).unwrap_or_default());
    match agent_id {
        Some(agent) => format!("agent:{agent}\0{body}"),
        None => format!("main\0{body}"),
    }
}
/// A blocking hook whose card is open and whose process may await a decision.
///
/// Produced by the non-blocking fold and handed to the socket delivery task,
/// which alone awaits it — keeping the observation pipeline unblocked.
struct PendingDecision {
    /// Rendezvous key, equal to the interaction id.
    key: crate::pending::DecisionKey,
    /// Event name the eventual reply must echo.
    event: String,
}

/// One turn as seen on the hook channel (P5, c-hookgap).
struct TurnState {
    /// Agent pid for the turn, from the relay's parent.
    pid: i32,
    /// Native session id, once a `SessionStart` named it.
    session_id: Option<String>,
    /// Whether the turn is currently active.
    active: Option<bool>,
    /// Wall-clock time of the newest hook record from this pid, whatever its
    /// kind. Hooks are event-driven, so a long tool genuinely goes quiet; the
    /// driver bounds its "hooks still vouch for a live turn" guard by this
    /// age rather than holding busy forever once the channel stalls (the
    /// missing-Stop hole). `None` until the first fold touches the slot.
    last_seen: Option<std::time::Instant>,
}

impl SignalBus {
    /// Build a bus that emits on `events`.
    #[must_use]
    pub fn new(
        context: BusContext,
        events: mpsc::Sender<Observation>,
        seq: Arc<AtomicU64>,
    ) -> Self {
        let live_scope = context.instance_id.as_id().as_str().to_owned();
        Self {
            context,
            events,
            seq,
            binding: std::sync::Mutex::new(None),
            pending: crate::pending::PendingDecisions::new(),
            parked: std::sync::Mutex::new(std::collections::HashMap::new()),
            open_questions: std::sync::Mutex::new(std::collections::HashMap::new()),
            question_calls: std::sync::Mutex::new(std::collections::HashMap::new()),
            blocking_wait: crate::BLOCKING_WAIT,
            turn_active: std::sync::Mutex::new(VecDeque::new()),
            live: std::sync::Mutex::new(LiveState::new(live_scope)),
            interrupt_pid: Arc::new(AtomicI32::new(0)),
        }
    }

    /// Shorten the bounded wait. Tests only; production uses
    /// [`BLOCKING_WAIT`](crate::BLOCKING_WAIT), which is aligned with the
    /// broker TTL.
    #[must_use]
    pub fn with_blocking_wait(mut self, wait: std::time::Duration) -> Self {
        self.blocking_wait = wait;
        self
    }

    /// The rendezvous table this bus parks blocking hooks in.
    ///
    /// The Node resolves through this handle when a device answers.
    #[must_use]
    pub fn pending(&self) -> crate::pending::PendingDecisions {
        self.pending.clone()
    }

    /// True while the interaction `id` belongs to a hook still waiting.
    #[must_use]
    pub fn is_parked(&self, id: &remuda_protocol::InteractionId) -> bool {
        self.pending
            .is_waiting(&crate::pending::DecisionKey::new(id.as_id().as_str()))
    }

    /// Resolve the hook behind interaction `id` with a device's answer.
    ///
    /// The returned [`Outcome`](crate::Outcome) is the honesty gate: only
    /// [`Answered`](crate::Outcome::Answered) means the decision actually
    /// reached a waiting process. [`Abandoned`](crate::Outcome::Abandoned) is
    /// the confined/ignored case the screen-key fallback exists for — the
    /// answer was real but the hook had already stopped listening.
    pub fn resolve_answer(
        &self,
        id: &remuda_protocol::InteractionId,
        answer: &remuda_protocol::InteractionAnswer,
    ) -> crate::Outcome {
        let key = crate::pending::DecisionKey::new(id.as_id().as_str());
        // A question call may have two parked hooks (a paired PreToolUse and
        // PermissionRequest). Peek the association up front so the answer fans
        // out to both and the primary ending first does not strand it; the
        // association itself is reaped by note_hook_ended as the hooks finish.
        let call = matches!(answer, remuda_protocol::InteractionAnswer::Question(_))
            .then(|| self.peek_question_call(&key))
            .flatten();
        // The parked record carries what the answer needs: suggestions for an
        // approval echo, the original request for a question rebuild. If the
        // card-owning hook already ended, a still-parked paired hook for the
        // same call answers in its place.
        let mut resolve_key = key.clone();
        let parked_record = self.parked_record(&key);
        let parked_record = match parked_record {
            Some(record) => Some(record),
            None => {
                if let Some(open) = &call
                    && let Some(secondary) = &open.secondary
                    && secondary != &key
                    && let Some(record) = self.parked_record(secondary)
                {
                    resolve_key = secondary.clone();
                    Some(record)
                } else {
                    None
                }
            }
        };
        let Some((suggestions, question_event)) = parked_record else {
            // No parked context: the hook already ended. Still report
            // honestly rather than claiming a delivery we cannot prove.
            return self.pending.resolve(
                &key,
                crate::HookDecision::Deny {
                    message: "Remuda: the answer arrived after the request closed".to_owned(),
                },
            );
        };
        let Some(decision) = (match answer {
            remuda_protocol::InteractionAnswer::Question(question) => {
                question_event.and_then(|request| crate::question_decision(&request, question))
            }
            other => ParkedHook::to_decision(&suggestions, other),
        }) else {
            tracing::warn!(?answer, "a hook answer did not match its interaction kind");
            return self.pending.resolve(
                &key,
                crate::HookDecision::Deny {
                    message: "Remuda: the answer did not match the request".to_owned(),
                },
            );
        };
        let outcome = self.pending.resolve(&resolve_key, decision.clone());
        // One call has two parked hooks: the same decision reaches the paired
        // one, serialized in whichever event vocabulary that hook carries.
        if let Some(open) = &call {
            for peer in [Some(&open.primary), open.secondary.as_ref()]
                .into_iter()
                .flatten()
                .filter(|peer| **peer != resolve_key)
            {
                if self
                    .parked
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains_key(peer)
                {
                    // The reported outcome is the card-owning hook's; a
                    // secondary that ended on its own deadline is simply not
                    // there to receive the fan-out.
                    let _ = self.pending.resolve(peer, decision.clone());
                }
            }
        }
        // A device answer wins the question outright: the terminal cannot
        // answer the same card a moment later and emit a second resolution.
        if matches!(answer, remuda_protocol::InteractionAnswer::Question(_)) {
            self.open_questions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
        }
        outcome
    }

    /// Clone the parked context a decision needs, if a hook is parked there.
    fn parked_record(
        &self,
        key: &crate::pending::DecisionKey,
    ) -> Option<(
        Vec<crate::PermissionSuggestion>,
        Option<crate::PermissionRequestEvent>,
    )> {
        self.parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .map(|parked| (parked.suggestions.clone(), parked.question.clone()))
    }

    /// Return the question-call association for whichever of its two hooks
    /// matches `hook`, if any. The association is left in place; reaping is
    /// done by [`note_hook_ended`](Self::note_hook_ended) as hooks finish.
    fn peek_question_call(&self, hook: &crate::pending::DecisionKey) -> Option<QuestionCall> {
        self.question_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .find(|call| &call.primary == hook || call.secondary.as_ref() == Some(hook))
            .cloned()
    }

    /// Drop a finished hook from its question-call association.
    ///
    /// The association lives while at least one of the two hooks is parked:
    /// the first of the pair to finish leaves it in place for the other; once
    /// neither has a parked record the entry is gone so a later, genuinely new
    /// question with identical text opens its own card.
    fn note_hook_ended(&self, hook: &crate::pending::DecisionKey) {
        // Find under the calls lock only; touch the parked table separately
        // so the lock order never inverts.
        let found = {
            let mut calls = self
                .question_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let Some((fingerprint, call)) = calls
                .iter_mut()
                .find(|(_, call)| &call.primary == hook || call.secondary.as_ref() == Some(hook))
                .map(|(fingerprint, call)| (fingerprint.clone(), call))
            else {
                return;
            };
            if &call.primary == hook {
                if call.secondary.is_none() {
                    calls.remove(&fingerprint);
                    return;
                }
                // The paired hook may still be parked; leave the association
                // anchored on the card id, and let the pair's own finish (or a
                // terminal close / device answer) remove it.
                return;
            }
            call.secondary = None;
            let primary = call.primary.clone();
            (fingerprint, primary)
        };
        let (fingerprint, primary) = found;
        let primary_live = self
            .parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&primary);
        if !primary_live {
            self.question_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&fingerprint);
        }
    }

    /// Retire every parked hook, denying them; called when the instance stops.
    pub fn retire_all(&self) {
        let keys: Vec<crate::pending::DecisionKey> = self
            .parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        for key in keys {
            self.pending.retire(&key, crate::RetireReason::Shutdown);
        }
        self.parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.open_questions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.question_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Share a pending interrupt with the PTY's screen observer. Zero means no
    /// pending request; a native turn end or new interruption marker claims it
    /// once. Writing an interrupt key alone never proves that the turn ended.
    #[must_use]
    pub fn with_interrupt_tracker(mut self, pending: Arc<AtomicI32>) -> Self {
        self.interrupt_pid = pending;
        self
    }

    /// Last hook turn boundary for this exact foreground process, if observed.
    #[must_use]
    pub fn turn_active(&self, pid: i32) -> Option<bool> {
        self.turn_active.lock().ok().and_then(|slots| {
            slots
                .iter()
                .find(|slot| slot.pid == pid)
                .and_then(|slot| slot.active)
        })
    }

    /// Wall-clock instant the newest hook record of *any* kind arrived from
    /// `pid`, when one has. The driver bounds its hook guard by this age so a
    /// channel that stops delivering (a degraded relay, a stalled journal pump)
    /// cannot hold the screen's idle edge forever. Returns `None` for a pid the
    /// bus never bound, so background CLIs sharing the socket cannot be timed.
    #[must_use]
    pub fn hook_last_seen(&self, pid: i32) -> Option<std::time::Instant> {
        self.turn_active
            .lock()
            .ok()
            .and_then(|slots| slots.iter().find(|slot| slot.pid == pid)?.last_seen)
    }

    /// Stamp the newest-hook-record time on an existing slot for `pid`.
    ///
    /// Only touches a slot `SessionStart`/a turn boundary already created: it
    /// never mints one, so an event from an unbound background process cannot
    /// fabricate freshness for the foreground agent.
    fn touch(&self, pid: i32) {
        if let Ok(mut slots) = self.turn_active.lock()
            && let Some(slot) = slots.iter_mut().find(|slot| slot.pid == pid)
        {
            slot.last_seen = Some(std::time::Instant::now());
        }
    }

    /// A pending cancel was confirmed by fresh native screen evidence. Keep
    /// the driver's idle guard in sync even when Claude emits no Stop hook.
    pub fn confirm_screen_interrupt(&self, pid: i32) {
        if let Ok(mut slots) = self.turn_active.lock()
            && let Some(slot) = slots.iter_mut().find(|slot| slot.pid == pid)
        {
            slot.active = Some(false);
        }
    }

    // A background CLI shares the terminal's hook socket, but its turn must
    // not replace the foreground CLI's cancellation guard. Bound retained
    // processes to 32 and reject another session's boundary for a known PID.
    fn record_turn(&self, pid: i32, session_id: Option<String>, active: Option<bool>) -> bool {
        let Ok(mut slots) = self.turn_active.lock() else {
            return false;
        };
        if let Some(index) = slots.iter().position(|slot| slot.pid == pid) {
            if active.is_some()
                && slots[index].session_id.is_some()
                && slots[index].session_id != session_id
            {
                return false;
            }
            let Some(previous) = slots.remove(index) else {
                return false;
            };
            let active = if active.is_none() && previous.session_id == session_id {
                previous.active
            } else {
                active
            };
            slots.push_back(TurnState {
                pid,
                session_id: session_id.or(previous.session_id),
                active,
                last_seen: previous.last_seen,
            });
        } else {
            slots.push_back(TurnState {
                pid,
                session_id,
                active,
                last_seen: None,
            });
        }
        while slots.len() > 32 {
            slots.pop_front();
        }
        true
    }

    /// The session this bus has seen a `SessionStart` for, if any.
    #[must_use]
    pub fn binding(&self) -> Option<SessionBinding> {
        self.binding.lock().ok().and_then(|slot| slot.clone())
    }

    /// Content gate: only the agent this bus is bound to may inject content
    /// (tool calls, results, streamed text). A second `claude` in a promoted
    /// terminal shares the hook socket; its phase evidence is still journaled,
    /// but it must not draw tool cards on the bound instance (design §3.0).
    #[must_use]
    fn owns(&self, ppid: i32) -> bool {
        self.binding().is_some_and(|binding| binding.pid == ppid)
    }

    /// Handle one authenticated event: fold and journal it.
    ///
    /// This never waits on a human. A blocking event's interaction card is
    /// emitted, but no hook is parked in the decision table — direct fold
    /// callers never answer through it — so the observation fold and the live
    /// pipeline return immediately regardless of event kind.
    pub async fn handle(&self, envelope: HookEnvelope) -> HookReply {
        let event = HookEvent::from_envelope(envelope);
        self.fold_event(event, false).await;
        HookReply::empty()
    }

    /// The socket delivery path: fold the event and, for a blocking event,
    /// await the human's decision up to the bounded wait.
    ///
    /// This is the *only* entry point that blocks, and it runs on the
    /// per-connection socket task, so a parked approval never stalls the
    /// observation fold or any other event.
    pub async fn deliver(&self, envelope: HookEnvelope) -> HookReply {
        let event = HookEvent::from_envelope(envelope);
        match self.fold_event(event, true).await {
            Some(pending) => self.await_decision(pending).await,
            None => HookReply::empty(),
        }
    }

    /// The fold itself: bind session, track turns/live/OSC, journal the raw +
    /// derived observations, and — for a blocking event — open the card and
    /// park the hook. Returns the parked handle (if any) for the caller to
    /// optionally await.
    ///
    /// `register_waiter` must be true exactly on the socket delivery path:
    /// when set, the decision waiter is registered **before** the card is
    /// emitted, so an answer that arrives the instant the card is visible can
    /// only find a live hook and can never be lost as a late abandon.
    async fn fold_event(&self, event: HookEvent, register_waiter: bool) -> Option<PendingDecision> {
        let mapped = map_event(&event);
        if mapped.kind == MappedKind::SessionStarted
            && let Some(session_id) = mapped.session_id.clone()
            && let Ok(mut slot) = self.binding.lock()
        {
            *slot = Some(SessionBinding {
                pid: event.ppid,
                session_id,
                transcript_path: mapped.transcript_path.clone(),
            });
        }
        if mapped.kind == MappedKind::SessionStarted && mapped.session_id.is_some() {
            self.record_turn(event.ppid, mapped.session_id.clone(), None);
        }
        let turn_matches = matches!(mapped.kind, MappedKind::TurnStarted | MappedKind::TurnEnded)
            && self.record_turn(
                event.ppid,
                mapped.session_id.clone(),
                Some(mapped.kind == MappedKind::TurnStarted),
            );
        // Stamp recency on every folded record so the driver's hook guard can
        // bound itself (see `hook_last_seen`); a no-op for an unbound pid.
        self.touch(event.ppid);
        if turn_matches && mapped.kind == MappedKind::TurnStarted {
            let _ = self.interrupt_pid.compare_exchange(
                event.ppid,
                0,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        }
        let interrupted = turn_matches
            && mapped.kind == MappedKind::TurnEnded
            && event.ppid > 0
            && self
                .interrupt_pid
                .compare_exchange(event.ppid, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok();
        // Live fold BEFORE the raw emit: the phase vocabulary rides on the
        // raw observation as related-id tags (one wire event per transition);
        // tool content comes back as gated extra observations.
        let at = now_ts();
        let fold = match &at {
            Some(at) => {
                let mut live = self
                    .live
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                live.observe(
                    &event,
                    &mapped,
                    &String::from(at.clone()),
                    self.owns(event.ppid),
                )
            }
            None => crate::live::LiveFold::default(),
        };

        // On a confirmed cancel the raw Stop must not claim `turn-ended`;
        // it stays untagged and the synthesized observation carries
        // `interrupted` (the precedent at this same seam).
        let mut raw = mapped.clone();
        if !interrupted
            && !fold.related.is_empty()
            && let ObservationPayload::Lifecycle(payload) = &mut raw.payload
            && let remuda_protocol::LifecyclePayload::Native(native) = payload.as_mut()
        {
            native.related_ids.extend(fold.related.clone());
        }
        if let Err(error) = self.emit(&raw).await {
            tracing::debug!(%error, event = %event.name, "hook observation not journaled");
        }
        if interrupted {
            let mut confirmed = mapped.clone();
            if let ObservationPayload::Lifecycle(payload) = &mut confirmed.payload
                && let remuda_protocol::LifecyclePayload::Native(native) = payload.as_mut()
            {
                native.native_name = crate::live::INTERRUPTED_NAME.into();
                native
                    .related_ids
                    .insert("afterEvent".into(), event.name.clone());
                native
                    .related_ids
                    .insert("requestedBy".into(), "instance.cancel".into());
                if let Some(at) = &at {
                    // The interrupted live phase rides this one synthesized
                    // event: projections read `phase = interrupted`.
                    let tagged =
                        crate::live::interrupted_lifecycle(&event, &String::from(at.clone()));
                    if let ObservationPayload::Lifecycle(tagged_payload) = tagged
                        && let remuda_protocol::LifecyclePayload::Native(tagged_native) =
                            tagged_payload.as_ref()
                    {
                        native.related_ids.extend(tagged_native.related_ids.clone());
                    }
                }
            }
            if let Err(error) = self.emit(&confirmed).await {
                tracing::debug!(%error, "confirmed interrupt not journaled");
            }
        }
        // Live-derived content (MessageDisplay chunks, OSC budget phases) rides
        // the same fold and is journaled before any blocking reply, so the card
        // and its surrounding stream are ordered together.
        if let Some(at) = at {
            // Sub-agent tool hooks (workflow members and foreground/background
            // Agent tasks) carry `agent_id`; stamp it onto the derived tool
            // observations so the web can group them UNDER their parent row
            // instead of flattening them into the main transcript. Main-session
            // hooks omit the field and stay unattributed.
            let agent_id = event.text("agent_id");
            for payload in fold.extras {
                if let Err(error) = self
                    .emit_derived(payload, mapped.session_id.as_ref(), at.clone(), agent_id)
                    .await
                {
                    tracing::debug!(%error, event = %event.name, "live content not journaled");
                }
            }
        }
        // An AskUserQuestion the human answered in the agent's own TUI closes
        // here: the hook was not answered through Remuda, but the closing
        // PostToolUse proves the dialog is done. Journaled after the tool
        // result above so the row closes and the banner clears together.
        self.close_terminal_answered(&event).await;
        // A blocking event opens its interaction card and parks the hook in
        // the same pass. The fold does not await the decision — it hands the
        // handle back so the socket delivery task can. In auto permission
        // mode an AskUserQuestion arrives as a PreToolUse (no
        // PermissionRequest follows while that hook is pending), so
        // "blocks" is payload-shaped there.
        if crate::event::hooks_block(&event.name, &event.payload) {
            return self.open_and_park(&event, register_waiter).await;
        }
        None
    }

    /// Open the interaction card for a blocking event and park its hook.
    ///
    /// Returns the handle `await_decision` blocks on. Done synchronously with
    /// the fold so the card and the phase observations are one ordered batch,
    /// but *without* blocking on the human — that split is what lets the live
    /// layer drain observations whether or not a decision ever arrives.
    async fn open_and_park(
        &self,
        event: &HookEvent,
        register_waiter: bool,
    ) -> Option<PendingDecision> {
        let Some(question) = crate::question::question_request_from_event(event) else {
            return self.open_plain_blocker(event, register_waiter).await;
        };
        let mut fingerprint = question_fingerprint(event.text("agent_id"), &question.tool_input);
        // The same call may already have parked its first hook (a PreToolUse
        // followed ~tens of ms later by a PermissionRequest, or the reverse).
        // It must not mint a second card; park behind the existing one.
        let pair_with = {
            let calls = self
                .question_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            calls
                .get(&fingerprint)
                .filter(|call| call.secondary.is_none())
                .map(|call| call.primary.clone())
        };
        if let Some(primary) = pair_with {
            return self.park_paired_hook(event, question, primary, fingerprint, register_waiter);
        }
        // A fully occupied entry (two hooks already parked) is a third
        // concurrent question with identical text in the same agent — not a
        // shape the harness can produce (a parked question blocks the turn),
        // but never clobber the live association: stand this card alone.
        let occupied = self
            .question_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&fingerprint);
        if occupied {
            fingerprint.push_str("\0standalone");
        }
        self.open_question_card(event, question, fingerprint, register_waiter)
            .await
    }

    /// Park the second hook of an AskUserQuestion call behind the open card.
    ///
    /// No interaction is emitted (one call, one card) and no
    /// `open_questions` row is added (the primary's already closes it). The
    /// waiter and parked record are still registered before this function
    /// returns, so the one device answer resolves both hooks.
    fn park_paired_hook(
        &self,
        event: &HookEvent,
        question: crate::PermissionRequestEvent,
        primary: crate::pending::DecisionKey,
        fingerprint: String,
        register_waiter: bool,
    ) -> Option<PendingDecision> {
        let key = Self::paired_key(&primary, &event.name);
        let decision_rx = register_waiter.then(|| self.pending.park(key.clone()));
        self.parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                key.clone(),
                ParkedHook {
                    suggestions: Vec::new(),
                    question: Some(question),
                    decision_rx,
                },
            );
        if let Some(open) = self
            .question_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&fingerprint)
        {
            open.secondary = Some(key.clone());
        }
        Some(PendingDecision {
            key,
            event: event.name.clone(),
        })
    }

    /// The decision key the paired hook for an interaction parks under.
    fn paired_key(
        primary: &crate::pending::DecisionKey,
        event: &str,
    ) -> crate::pending::DecisionKey {
        crate::pending::DecisionKey::new(format!("{}::paired::{}", primary.as_str(), event))
    }

    /// Open the card for the first hook of an AskUserQuestion call.
    async fn open_question_card(
        &self,
        event: &HookEvent,
        question: crate::PermissionRequestEvent,
        fingerprint: String,
        register_waiter: bool,
    ) -> Option<PendingDecision> {
        let Some((interaction, key)) = self.open_interaction(event) else {
            tracing::debug!(
                event = %event.name,
                "no interaction could be opened; the agent keeps its own prompt"
            );
            return None;
        };
        // Register waiter, parked record, and the call association BEFORE the
        // card becomes visible (same window as the approval path): a paired
        // hook or a device answer arriving the instant the card appears must
        // find them.
        let decision_rx = register_waiter.then(|| self.pending.park(key.clone()));
        self.parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                key.clone(),
                ParkedHook {
                    suggestions: Vec::new(),
                    question: Some(question.clone()),
                    decision_rx,
                },
            );
        self.question_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                fingerprint,
                QuestionCall {
                    primary: key.clone(),
                    secondary: None,
                },
            );
        // The TUI can answer this card itself; remember it so the closing
        // PostToolUse resolves the banner even with no device answer.
        self.open_questions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                key.clone(),
                OpenQuestion {
                    interaction: interaction.clone(),
                    request: question,
                },
            );
        if !self.emit_interaction(interaction).await {
            // Nobody can see a card that never reached the journal. Retire
            // everything just registered; dropping the sender reads as
            // fail-closed TimedOut, never an answered deny.
            tracing::debug!(event = %event.name, "interaction not journaled; not parking a hook");
            self.parked
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            self.open_questions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            self.question_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|_, call| call.primary != key);
            if register_waiter {
                self.pending.retire(&key, crate::RetireReason::Deadline);
            }
            return None;
        }
        Some(PendingDecision {
            key,
            event: event.name.clone(),
        })
    }

    /// Open a card and park the hook for a non-question blocking event
    /// (`PermissionRequest` for an ordinary tool, `Elicitation`).
    async fn open_plain_blocker(
        &self,
        event: &HookEvent,
        register_waiter: bool,
    ) -> Option<PendingDecision> {
        let Some((interaction, key)) = self.open_interaction(event) else {
            tracing::debug!(
                event = %event.name,
                "no interaction could be opened; the agent keeps its own prompt"
            );
            return None;
        };
        let permission_request = crate::decision::PermissionRequestEvent::from_event(event);
        let suggestions = permission_request
            .as_ref()
            .map(|request| request.suggestions.clone())
            .unwrap_or_default();
        // Register BOTH the decision waiter and the parked record BEFORE the
        // card becomes visible. A device that answers the instant it receives
        // the card must find a parked hook in both tables: the oneshot so its
        // decision reaches the wait, and the parked record so resolve_answer
        // can map its answer into the harness decision. Registering either
        // only after the emit reopened a window where an allow was dropped as
        // abandoned / "after the request closed" and the hook then denied on
        // its deadline (the hook_approval_e2e flakes).
        let decision_rx = register_waiter.then(|| self.pending.park(key.clone()));
        self.parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                key.clone(),
                ParkedHook {
                    suggestions,
                    question: None,
                    decision_rx,
                },
            );
        if !self.emit_interaction(interaction).await {
            // Nobody can see a card that never reached the journal. Answering
            // it is impossible, so the relay must not park for the full TTL.
            // Retire the just-registered waiter and parked record: dropping
            // the sender reads as fail-closed TimedOut, never an answered
            // deny, and avoids leaving a dead entry for the instance lifetime.
            tracing::debug!(event = %event.name, "interaction not journaled; not parking a hook");
            self.parked
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            if register_waiter {
                self.pending.retire(&key, crate::RetireReason::Deadline);
            }
            return None;
        }
        Some(PendingDecision {
            key,
            event: event.name.clone(),
        })
    }

    /// Block until the parked hook for `pending` is answered or its bounded
    /// wait expires, then return the decision JSON the harness reads.
    ///
    /// This is the only place the bus awaits a human, and it runs on the socket
    /// delivery task, never on the observation fold.
    async fn await_decision(&self, pending: PendingDecision) -> HookReply {
        // The receiver was registered before the card was emitted in
        // open_and_park; take it from the parked record so the wait starts
        // without any window in which an answer could miss the hook.
        let rx = self
            .parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&pending.key)
            .and_then(|parked| parked.decision_rx.take());
        let (decision, outcome) = match rx {
            Some(rx) => {
                self.pending
                    .await_parked(pending.key.clone(), rx, self.blocking_wait)
                    .await
            }
            // Direct fold callers (handle) never register a waiter; nothing
            // can answer through this path, so fail closed without parking.
            None => (
                crate::decision::HookDecision::timed_out(),
                crate::pending::Outcome::TimedOut,
            ),
        };
        self.parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&pending.key);
        // Release this hook's slot in its question-call association; the
        // reply below is serialized in THIS hook's event vocabulary
        // (PermissionRequest vs PreToolUse differ).
        self.note_hook_ended(&pending.key);
        tracing::debug!(event = %pending.event, ?outcome, "blocking hook resolved");
        HookReply {
            decision: Some(decision.to_hook_json(&pending.event)),
        }
    }

    /// Build the entity a device answers, plus the key its hook is filed under.
    ///
    /// The interaction id *is* the decision key, so a `respond_interaction` for
    /// that id resolves the parked hook directly.
    fn open_interaction(
        &self,
        event: &HookEvent,
    ) -> Option<(remuda_protocol::Interaction, crate::pending::DecisionKey)> {
        let interaction_id = remuda_protocol::InteractionId::new();
        let context = crate::approval::ApprovalContext {
            instance_id: self.context.instance_id.clone(),
            host_id: self.context.host_id.clone(),
            run_id: self.context.run_id.clone(),
            interaction_id: interaction_id.clone(),
            now: now_ts()?,
            deadline: deadline_ts(self.blocking_wait)?,
        };
        let interaction = match event.name.as_str() {
            "PermissionRequest" => {
                let request = crate::decision::PermissionRequestEvent::from_event(event)?;
                if crate::question::is_ask_user_question(&request) {
                    crate::question::question_interaction(&request, &context).ok()?
                } else {
                    crate::approval::approval_interaction(&request, &context).ok()?
                }
            }
            // Auto permission mode raises AskUserQuestion as a PreToolUse
            // without a PermissionRequest (evidence askq-pretooluse-1). Only
            // the question form reaches this arm; every other PreToolUse is a
            // fire-and-forget event and never calls open_interaction.
            "PreToolUse" => {
                let request = crate::question::question_request_from_event(event)?;
                crate::question::question_interaction(&request, &context).ok()?
            }
            "Elicitation" => crate::approval::elicitation_interaction(event, &context).ok()?,
            _ => return None,
        };
        Some((
            interaction,
            crate::pending::DecisionKey::new(interaction_id.as_id().as_str()),
        ))
    }

    /// Close an AskUserQuestion card that was answered in the agent's own TUI.
    ///
    /// Evidence: a `PostToolUse(AskUserQuestion)` carrying an `answers` object.
    /// The card Remuda showed was never answered through the broker, but the
    /// harness has moved on — leaving it pending would strand the
    /// 「等待操作」 banner forever and keep the structured tool row running.
    /// So journal a terminal-sourced resolved entity (with the chosen labels)
    /// and drop the parked hook: a device answer arriving afterwards must find
    /// nothing to double-answer.
    async fn close_terminal_answered(&self, event: &HookEvent) {
        if event.name != "PostToolUse" || event.text("tool_name") != Some(crate::ASK_USER_QUESTION)
        {
            return;
        }
        let Some(tool_input) = event.payload.get("tool_input") else {
            return;
        };
        let Some(questions) = tool_input.get("questions") else {
            return;
        };
        let Some(raw_answers) = tool_input.get("answers").or_else(|| {
            event
                .payload
                .get("tool_response")
                .and_then(|r| r.get("answers"))
        }) else {
            return;
        };
        let found = {
            let mut table = self
                .open_questions
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            // Score by how many answer keys are questions on the card; an
            // exact, non-empty match wins. With one open card this is trivial,
            // but two questions in flight must not close each other's cards.
            let mut best: Option<(crate::pending::DecisionKey, OpenQuestion, usize)> = None;
            for (key, open) in table.iter() {
                let score = answer_overlap(open, raw_answers);
                let total = raw_answers.as_object().map_or(0, |o| o.len());
                if score > 0
                    && score == total
                    && best.as_ref().is_none_or(|b: &(_, _, usize)| score > b.2)
                {
                    best = Some((key.clone(), open.clone(), score));
                }
            }
            best.and_then(|(key, _, _)| table.remove(&key).map(|open| (key, open)))
        };
        let Some((key, open)) = found else {
            return;
        };
        let Some(answers) = crate::answer_from_harness(questions, raw_answers) else {
            // Not one of our cards' questions; put it back so another close
            // attempt can match.
            self.open_questions
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(key, open);
            return;
        };
        self.parked
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&key);
        self.pending.retire(&key, crate::RetireReason::Deadline);
        // The call may have a second parked hook (the paired
        // PreToolUse/PermissionRequest): it can never be answered now either.
        let secondary = {
            let mut calls = self
                .question_calls
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let secondary = calls
                .values()
                .find(|call| call.primary == key)
                .and_then(|call| call.secondary.clone());
            calls.retain(|_, call| call.primary != key);
            secondary
        };
        if let Some(secondary) = secondary {
            self.parked
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(&secondary);
            self.pending
                .retire(&secondary, crate::RetireReason::Deadline);
        }
        let Some(now) = now_ts() else {
            return;
        };
        let resolved =
            crate::resolved_in_terminal(open.interaction, answers, now.clone(), CommandId::new());
        let seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let observation = self.build_payload(
            seq,
            now,
            ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Entity(Box::new(
                EntityLifecycle {
                    entity_id: resolved.meta.id.as_id().clone(),
                    revision: resolved.meta.revision,
                    previous_state: Some("pending".into()),
                    state: "resolved".into(),
                    reason_code: "terminal-answered".into(),
                    evidence_event_ids: Vec::new(),
                    entity_value: LifecycleEntity::Interaction(Box::new(resolved)),
                },
            )))),
            self.binding().map(|binding| binding.session_id).as_ref(),
            Completeness::Structured,
            None,
        );
        if self.events.send(observation).await.is_err() {
            tracing::debug!("terminal answer resolution not journaled");
        }
    }

    /// Journal `interaction.requested`. False when the channel is gone.
    async fn emit_interaction(&self, interaction: remuda_protocol::Interaction) -> bool {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let mapped = Mapped {
            kind: MappedKind::InteractionObserved,
            completeness: Completeness::Structured,
            payload: ObservationPayload::InteractionRequested(Box::new(
                remuda_protocol::InteractionRequestedPayload { interaction },
            )),
            session_id: self.binding().map(|binding| binding.session_id),
            transcript_path: None,
        };
        let Some(observation) = self.build(seq, &mapped) else {
            return false;
        };
        self.events.send(observation).await.is_ok()
    }

    async fn emit(&self, mapped: &Mapped) -> Result<(), mpsc::error::SendError<Observation>> {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let Some(observation) = self.build(seq, mapped) else {
            return Ok(());
        };
        self.events.send(observation).await
    }

    /// Emit a live-layer payload (`turn.live`, `ToolCall`, `ToolResult`) on the
    /// same envelope the raw hook observation uses. `agent_id` is stamped onto
    /// `source.native_agent_id` when the fold ran on a sub-agent hook.
    async fn emit_derived(
        &self,
        body: ObservationPayload,
        session_id: Option<&String>,
        observed_at: Timestamp,
        agent_id: Option<&str>,
    ) -> Result<(), mpsc::error::SendError<Observation>> {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let observation = self.build_payload(
            seq,
            observed_at,
            body,
            session_id,
            HOOK_COMPLETENESS,
            agent_id,
        );
        self.events.send(observation).await
    }

    fn build(&self, seq: u64, mapped: &Mapped) -> Option<Observation> {
        let observed_at = now_ts()?;
        Some(self.build_payload(
            seq,
            observed_at,
            mapped.payload.clone(),
            mapped.session_id.as_ref(),
            mapped.completeness,
            None,
        ))
    }

    fn build_payload(
        &self,
        seq: u64,
        observed_at: Timestamp,
        body: ObservationPayload,
        session_id: Option<&String>,
        completeness: Completeness,
        agent_id: Option<&str>,
    ) -> Observation {
        Observation {
            schema_version: SchemaVersion,
            event_id: EventId::new(),
            journal_id: self.context.journal_id.clone(),
            instance_id: self.context.instance_id.clone(),
            run_id: Some(self.context.run_id.clone()),
            host_id: self.context.host_id.clone(),
            process_generation: U64(1),
            run_generation: Some(U64(1)),
            seq: U64(seq),
            observed_at,
            // The harness stamps no time of its own on a hook payload; saying
            // so beats inventing one from our own clock.
            native_at: Knowledge::Unknown {
                reason: "not-emitted".into(),
                evidence_event_ids: Vec::new(),
            },
            source: ObservationSource {
                driver_kind: self.context.driver_kind,
                driver_version: "shell-pty".into(),
                adapter_version: self.context.adapter_version.clone(),
                channel: SourceChannel::Hook,
                delivery: SourceDelivery::Live,
                native_session_id: match session_id {
                    Some(value) => Knowledge::Known {
                        value: value.clone(),
                    },
                    None => Knowledge::Unknown {
                        reason: "not-emitted".into(),
                        evidence_event_ids: Vec::new(),
                    },
                },
                native_turn_id: Knowledge::NotApplicable,
                native_agent_id: match agent_id {
                    Some(value) => Knowledge::Known {
                        value: value.to_owned(),
                    },
                    None => Knowledge::NotApplicable,
                },
                native_item_id: Knowledge::NotApplicable,
                native_event_id: Knowledge::NotApplicable,
                native_request_id: NativeRequestKey::None,
                source_cursor: SourceCursor::Runtime(Box::new(RuntimeCursor {
                    ledger_revision: U64(seq),
                })),
            },
            completeness,
            raw_ref: None,
            evidence_event_ids: Vec::new(),
            body,
        }
    }
}

impl SignalSink for SignalBus {
    fn deliver(
        &self,
        envelope: HookEnvelope,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = HookReply> + Send + '_>> {
        Box::pin(SignalBus::deliver(self, envelope))
    }
}

/// Millisecond-precision UTC, matching the driver's own stamping.
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

/// `now + wait`, for the deadline a card advertises.
fn deadline_ts(wait: std::time::Duration) -> Option<Timestamp> {
    let now = time::OffsetDateTime::now_utc() + time::Duration::seconds(wait.as_secs() as i64);
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

/// Completeness every hook observation carries. Exposed so tests elsewhere can
/// assert the channel/completeness pair without rebuilding a bus.
pub const HOOK_COMPLETENESS: Completeness = Completeness::Structured;

impl ParkedHook {
    /// Turn a device's protocol answer into the harness decision for this hook.
    ///
    /// Returns `None` when the answer kind does not match the interaction the
    /// hook actually opened; the caller then denies rather than sending the
    /// harness something it cannot interpret.
    fn to_decision(
        suggestions: &[crate::PermissionSuggestion],
        answer: &remuda_protocol::InteractionAnswer,
    ) -> Option<crate::HookDecision> {
        use remuda_protocol::InteractionAnswer;
        match answer {
            InteractionAnswer::Approval(answer) => {
                // The option ids are exactly the ones the card offered (see
                // approval.rs); anything else is an answer for a different
                // request and must not be guessed.
                if answer.option_id == crate::approval::ALLOW_ONCE {
                    Some(crate::HookDecision::Allow {
                        updated_input: None,
                        updated_permissions: Vec::new(),
                    })
                } else if answer.option_id == crate::approval::DENY {
                    Some(crate::HookDecision::Deny {
                        message: "Denied through Remuda".to_owned(),
                    })
                } else if let Some(index) = crate::approval::allow_always_index(&answer.option_id) {
                    // Echo the exact suggestion the button was built from. An
                    // out-of-range index means the answer named a grant that
                    // was never offered, which is the deny case, not an allow.
                    let suggestion = suggestions.get(index)?.value.clone();
                    Some(crate::HookDecision::Allow {
                        updated_input: None,
                        updated_permissions: vec![suggestion],
                    })
                } else {
                    None
                }
            }
            InteractionAnswer::Elicitation(answer) => {
                let action = match answer.action {
                    remuda_protocol::ElicitationAction::Accept => crate::ElicitationAction::Accept,
                    remuda_protocol::ElicitationAction::Decline => {
                        crate::ElicitationAction::Decline
                    }
                    remuda_protocol::ElicitationAction::Cancel => crate::ElicitationAction::Cancel,
                };
                Some(crate::HookDecision::Elicitation {
                    action,
                    content: answer.content.clone(),
                })
            }
            // A Question / PlanReview answer cannot answer a hook that opened
            // an Approval / Elicitation.
            InteractionAnswer::Question(_) | InteractionAnswer::PlanReview(_) => None,
        }
    }
}

/// Count of `raw_answers` keys that are question texts on an open card.
///
/// PermissionRequests carry no correlation id of their own (the payload has
/// no `tool_use_id`), so a terminal close matches by content: the answers
/// object is keyed by question text, and every key must be one of the card's
/// questions. Zero means this PostToolUse does not close that card.
fn answer_overlap(open: &OpenQuestion, raw_answers: &serde_json::Value) -> usize {
    let Some(card_questions) = open
        .request
        .tool_input
        .get("questions")
        .and_then(serde_json::Value::as_array)
    else {
        return 0;
    };
    let Some(answer_keys) = raw_answers.as_object() else {
        return 0;
    };
    answer_keys
        .keys()
        .filter(|key| {
            card_questions.iter().any(|question| {
                question.get("question").and_then(serde_json::Value::as_str) == Some(key.as_str())
            })
        })
        .count()
}

/// `ObservationPayload` accessor used by the Node's fold; keeps the match on
/// lifecycle payloads in one place.
#[must_use]
pub fn native_lifecycle(payload: &ObservationPayload) -> Option<&remuda_protocol::NativeLifecycle> {
    let ObservationPayload::Lifecycle(lifecycle) = payload else {
        return None;
    };
    match lifecycle.as_ref() {
        remuda_protocol::LifecyclePayload::Native(native) => Some(native),
        remuda_protocol::LifecyclePayload::Entity(_) => None,
    }
}

/// Whether a lifecycle observation carries live phase `phase`.
#[must_use]
pub fn has_phase(payload: &ObservationPayload, expected_phase: &str) -> bool {
    native_lifecycle(payload).is_some_and(|native| {
        native
            .related_ids
            .get(crate::live::PHASE_KEY)
            .is_some_and(|phase| phase == expected_phase)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> BusContext {
        BusContext {
            instance_id: InstanceId::new(),
            host_id: HostId::new(),
            journal_id: Id::new("obj").unwrap(),
            run_id: RunId::new(),
            driver_kind: remuda_protocol::DriverKind::ShellPty,
            adapter_version: "test".into(),
        }
    }

    fn envelope(event: &str, payload: serde_json::Value) -> HookEnvelope {
        HookEnvelope {
            credential: "cred".into(),
            event: event.into(),
            ppid: 4242,
            payload,
        }
    }

    fn bus() -> (SignalBus, mpsc::Receiver<Observation>) {
        let (tx, rx) = mpsc::channel(128);
        (
            SignalBus::new(context(), tx, Arc::new(AtomicU64::new(0))),
            rx,
        )
    }

    /// A bus with a short blocking wait, so timeout tests do not sleep.
    fn fast_bus() -> (SignalBus, mpsc::Receiver<Observation>) {
        let (tx, rx) = mpsc::channel(128);
        (
            SignalBus::new(context(), tx, Arc::new(AtomicU64::new(0)))
                .with_blocking_wait(std::time::Duration::from_millis(50)),
            rx,
        )
    }

    fn permission_request() -> serde_json::Value {
        serde_json::json!({
            "tool_name": "Write",
            "tool_input": {"file_path": "/tmp/p.txt", "content": "hi"},
            "permission_suggestions": [
                {"type": "setMode", "mode": "acceptEdits", "destination": "session"}
            ],
        })
    }

    /// Every observation a `handle` produced (the raw evidence plus any
    /// second-emit live observations).
    async fn drain(rx: &mut mpsc::Receiver<Observation>) -> Vec<Observation> {
        let mut out = Vec::new();
        while let Ok(observation) = rx.try_recv() {
            out.push(observation);
        }
        out
    }

    fn native_name(observation: &Observation) -> &str {
        native_lifecycle(&observation.body)
            .expect("a lifecycle observation")
            .native_name
            .as_str()
    }

    fn live_phase<'a>(observations: &'a [Observation], phase: &str) -> Option<&'a Observation> {
        observations
            .iter()
            .find(|observation| has_phase(&observation.body, phase))
    }

    #[tokio::test]
    async fn cancel_is_confirmed_only_by_a_matching_native_turn_end() {
        let pending = Arc::new(AtomicI32::new(4242));
        let (bus, mut rx) = bus();
        let bus = bus.with_interrupt_tracker(Arc::clone(&pending));
        let mut other = envelope("Stop", serde_json::json!({}));
        other.ppid = 99;
        bus.handle(other).await;
        // Lifecycle is ungated: a foreign ppid still reports the turn-ended
        // phase, but it cannot settle the foreground's cancel.
        let batch = drain(&mut rx).await;
        assert_eq!(native_name(&batch[0]), "Stop");
        assert!(live_phase(&batch, "turn-ended").is_some());
        assert!(
            batch.iter().all(|o| native_name(o) != "interrupted"),
            "a foreign turn end cannot confirm the cancel"
        );
        assert_eq!(pending.load(Ordering::SeqCst), 4242);

        bus.handle(envelope("StopFailure", serde_json::json!({})))
            .await;
        let batch = drain(&mut rx).await;
        assert_eq!(native_name(&batch[0]), "StopFailure");
        let confirmed = batch
            .iter()
            .find(|o| native_name(o) == "interrupted")
            .expect("matching StopFailure confirms the cancel");
        assert_eq!(confirmed.source.channel, SourceChannel::Hook);
        let native = native_lifecycle(&confirmed.body).unwrap();
        assert_eq!(native.related_ids["afterEvent"], "StopFailure");
        // The confirmation *is* the interrupted live phase; no competing
        // turn-ended phase is emitted in the same batch.
        assert_eq!(native.related_ids["phase"], "interrupted");
        assert_eq!(native.related_ids["outcome"], "cancelled");
        assert!(live_phase(&batch, "turn-ended").is_none());
        assert_eq!(pending.load(Ordering::SeqCst), 0);
        assert_eq!(bus.turn_active(4242), Some(false));

        bus.handle(envelope("Stop", serde_json::json!({}))).await;
        let batch = drain(&mut rx).await;
        assert!(
            batch.iter().all(|o| native_name(o) != "interrupted"),
            "duplicate Stop cannot confirm twice"
        );
        assert!(live_phase(&batch, "turn-ended").is_some());
    }

    #[tokio::test]
    async fn a_new_turn_cannot_settle_a_previous_turns_pending_cancel() {
        // Pending cancel from before the prompt; the new prompt claims it.
        let pending = Arc::new(AtomicI32::new(4242));
        let (bus, mut rx) = bus();
        let bus = bus.with_interrupt_tracker(Arc::clone(&pending));
        bus.handle(envelope("UserPromptSubmit", serde_json::json!({})))
            .await;
        let batch = drain(&mut rx).await;
        assert_eq!(native_name(&batch[0]), "UserPromptSubmit");
        assert!(live_phase(&batch, "prompt-accepted").is_some());
        assert_eq!(bus.turn_active(4242), Some(true));
        assert_eq!(pending.load(Ordering::SeqCst), 0, "the prompt claims it");
        bus.handle(envelope("Stop", serde_json::json!({}))).await;
        let batch = drain(&mut rx).await;
        assert!(
            batch.iter().all(|o| native_name(o) != "interrupted"),
            "a later turn end cannot settle the previous turn's cancel"
        );
        assert!(live_phase(&batch, "turn-ended").is_some());
    }

    #[tokio::test]
    async fn background_turns_and_foreign_sessions_do_not_clear_foreground_activity() {
        let pending = Arc::new(AtomicI32::new(0));
        let (bus, mut rx) = bus();
        let bus = bus.with_interrupt_tracker(Arc::clone(&pending));
        for (name, pid, session) in [
            ("SessionStart", 99, "background"),
            ("UserPromptSubmit", 99, "background"),
            ("SessionStart", 4242, "foreground"),
            ("UserPromptSubmit", 4242, "foreground"),
        ] {
            let mut event = envelope(name, serde_json::json!({"session_id":session}));
            event.ppid = pid;
            bus.handle(event).await;
            let batch = drain(&mut rx).await;
            if name == "UserPromptSubmit" {
                assert!(live_phase(&batch, "prompt-accepted").is_some());
            }
        }
        pending.store(4242, Ordering::SeqCst);
        for (name, pid, session) in [
            ("SessionStart", 4242, "foreground"),
            ("Stop", 99, "background"),
            ("Stop", 4242, "foreign"),
            ("SubagentStop", 4242, "foreground"),
        ] {
            let mut event = envelope(name, serde_json::json!({"session_id":session}));
            event.ppid = pid;
            bus.handle(event).await;
            let batch = drain(&mut rx).await;
            assert!(
                batch.iter().all(|o| native_name(o) != "interrupted"),
                "unrelated evidence cannot settle cancel"
            );
            assert_eq!(bus.turn_active(4242), Some(true));
            assert_eq!(pending.load(Ordering::SeqCst), 4242);
        }
        bus.handle(envelope(
            "Stop",
            serde_json::json!({"session_id":"foreground"}),
        ))
        .await;
        let batch = drain(&mut rx).await;
        assert!(
            batch.iter().any(|o| native_name(o) == "interrupted"),
            "the foreground Stop confirms the cancel"
        );
        assert_eq!(
            native_lifecycle(
                &batch
                    .iter()
                    .find(|o| native_name(o) == "interrupted")
                    .unwrap()
                    .body
            )
            .unwrap()
            .related_ids["phase"],
            "interrupted"
        );
        assert!(live_phase(&batch, "turn-ended").is_none());
        assert_eq!(bus.turn_active(4242), Some(false));
        bus.handle(envelope(
            "SessionStart",
            serde_json::json!({"session_id":"replacement"}),
        ))
        .await;
        let batch = drain(&mut rx).await;
        assert_eq!(native_name(&batch[0]), "SessionStart");
        assert_eq!(
            bus.turn_active(4242),
            None,
            "a different session resets prior activity"
        );
        for pid in 1..=64 {
            bus.record_turn(pid, None, Some(true));
        }
        assert_eq!(bus.turn_active.lock().unwrap().len(), 32);
    }

    #[tokio::test]
    async fn every_hook_observation_is_stamped_as_the_hook_channel() {
        let (bus, mut rx) = bus();
        bus.handle(envelope("Stop", serde_json::json!({}))).await;
        let observation = rx.recv().await.unwrap();
        assert_eq!(observation.source.channel, SourceChannel::Hook);
        assert_eq!(observation.completeness, Completeness::Structured);
        assert_eq!(observation.source.delivery, SourceDelivery::Live);
    }

    #[tokio::test]
    async fn session_start_binds_the_agent_pid_to_its_session() {
        let (bus, mut rx) = bus();
        assert_eq!(bus.binding(), None);
        bus.handle(envelope(
            "SessionStart",
            serde_json::json!({
                "session_id": "0199a1f0-0000-7000-8000-000000000000",
                "transcript_path": "/w/s.jsonl",
            }),
        ))
        .await;
        assert_eq!(
            bus.binding(),
            Some(SessionBinding {
                pid: 4242,
                session_id: "0199a1f0-0000-7000-8000-000000000000".into(),
                transcript_path: Some("/w/s.jsonl".into()),
            })
        );
        let observation = rx.recv().await.unwrap();
        assert_eq!(
            observation.source.native_session_id,
            Knowledge::Known {
                value: "0199a1f0-0000-7000-8000-000000000000".into()
            }
        );
    }

    #[tokio::test]
    async fn a_later_event_without_a_session_does_not_clear_the_binding() {
        let (bus, _rx) = bus();
        bus.handle(envelope(
            "SessionStart",
            serde_json::json!({"session_id": "s-1", "transcript_path": "/w/s.jsonl"}),
        ))
        .await;
        bus.handle(envelope("Notification", serde_json::json!({})))
            .await;
        assert_eq!(bus.binding().map(|b| b.session_id), Some("s-1".into()));
    }

    #[tokio::test]
    async fn sequence_numbers_are_monotonic_across_events_and_their_live_emissions() {
        let (bus, mut rx) = bus();
        for event in [
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "Stop",
        ] {
            let payload = if event == "PreToolUse" || event == "PostToolUse" {
                serde_json::json!({"tool_use_id":"call_1","tool_name":"Bash","tool_input":{"command":"sleep 20"},"tool_response":{"stdout":""}})
            } else {
                serde_json::json!({})
            };
            bus.handle(envelope(event, payload)).await;
        }
        // Raw evidence plus the live second-emissions must share one strictly
        // increasing sequence space; the journal pump orders on it.
        let mut seqs = Vec::new();
        while let Ok(observation) = rx.try_recv() {
            seqs.push(observation.seq.0);
        }
        let sorted = seqs.windows(2).all(|w| w[0] + 1 == w[1]);
        assert!(sorted, "seqs must be contiguous, got {seqs:?}");
        assert_eq!(seqs.first(), Some(&1));
    }

    /// Pull the next `interaction.requested` off the channel and return the
    /// decision key its hook is parked under.
    async fn next_decision_key(
        rx: &mut mpsc::Receiver<Observation>,
    ) -> crate::pending::DecisionKey {
        use remuda_protocol::NativeRequestKey;
        loop {
            let observation = rx.recv().await.expect("an observation");
            if let ObservationPayload::InteractionRequested(payload) = &observation.body
                && let NativeRequestKey::Hook { invocation_id } =
                    &payload.interaction.request_key.native
            {
                return crate::pending::DecisionKey::new(invocation_id.as_str());
            }
        }
    }

    #[tokio::test]
    async fn a_human_allow_comes_back_in_the_shape_the_harness_applies() {
        // End-to-end through the bus: the hook arrives, an interaction is
        // journaled, an external answer resolves it, and the reply is the
        // nested shape measured to land on claude 2.1.221.
        let (bus, mut rx) = bus();
        let pending = bus.pending();
        let handle = tokio::spawn(async move {
            bus.deliver(envelope("PermissionRequest", permission_request()))
                .await
        });
        let key = next_decision_key(&mut rx).await;
        assert!(pending.is_waiting(&key));
        assert!(matches!(
            pending.resolve(
                &key,
                crate::HookDecision::Allow {
                    updated_input: None,
                    updated_permissions: Vec::new(),
                },
            ),
            crate::Outcome::Answered
        ));
        let reply = handle.await.expect("handler");
        assert_eq!(
            reply.to_hook_json(),
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": {"behavior": "allow"},
                }
            })
        );
    }

    /// The exact hook_approval_e2e race: a device answers the moment the card
    /// reaches its journal — with no await between observing the card and
    /// resolving it. The waiter must already be registered when the card is
    /// emitted, so the allow lands even under a 1 ms blocking budget: it can
    /// never be dropped as abandoned and then denied on the deadline.
    #[tokio::test]
    async fn an_allow_landing_with_the_card_never_loses_to_the_deadline() {
        let (bus, mut rx) = fast_blocking_one_ms();
        let pending = bus.pending();
        let handle = tokio::spawn(async move {
            bus.deliver(envelope("PermissionRequest", permission_request()))
                .await
        });
        // The card is the first blocking observation. `recv` returning means
        // `events.send` just completed inside the fold; resolve before
        // yielding again, i.e. before the delivery task can register a waiter
        // that was previously created only *after* the emit.
        let key = next_decision_key(&mut rx).await;
        let outcome = pending.resolve(
            &key,
            crate::HookDecision::Allow {
                updated_input: None,
                updated_permissions: Vec::new(),
            },
        );
        assert_eq!(
            outcome,
            crate::Outcome::Answered,
            "the hook must be parked before its card becomes visible"
        );
        let reply = handle.await.expect("handler");
        assert_eq!(
            reply.to_hook_json()["hookSpecificOutput"]["decision"]["behavior"],
            "allow",
            "an allow that arrives with the card must not time out as deny"
        );
    }

    /// A bus whose blocking window is deliberately 1 ms: only a waiter that is
    /// registered before the card is emitted can still receive an immediate
    /// answer inside it.
    fn fast_blocking_one_ms() -> (SignalBus, mpsc::Receiver<Observation>) {
        let (tx, rx) = mpsc::channel(128);
        (
            SignalBus::new(context(), tx, Arc::new(AtomicU64::new(0)))
                .with_blocking_wait(std::time::Duration::from_millis(1)),
            rx,
        )
    }

    #[tokio::test]
    async fn an_unanswered_request_denies_rather_than_allowing() {
        let (bus, mut rx) = fast_bus();
        let handle = tokio::spawn(async move {
            bus.deliver(envelope("PermissionRequest", permission_request()))
                .await
        });
        let _key = next_decision_key(&mut rx).await;
        let reply = handle.await.expect("handler");
        let json = reply.to_hook_json();
        assert_eq!(json["hookSpecificOutput"]["decision"]["behavior"], "deny");
    }

    #[tokio::test]
    async fn an_elicitation_is_answered_with_an_action() {
        let (bus, mut rx) = bus();
        let pending = bus.pending();
        let handle = tokio::spawn(async move {
            bus.deliver(envelope(
                "Elicitation",
                serde_json::json!({"message": "Which account?", "schema": {"type": "object"}}),
            ))
            .await
        });
        let key = next_decision_key(&mut rx).await;
        assert!(matches!(
            pending.resolve(
                &key,
                crate::HookDecision::Elicitation {
                    action: crate::ElicitationAction::Accept,
                    content: Some(serde_json::json!({"account": "ada"})),
                },
            ),
            crate::Outcome::Answered
        ));
        let reply = handle.await.expect("handler");
        assert_eq!(
            reply.to_hook_json(),
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "Elicitation",
                    "action": "accept",
                    "content": {"account": "ada"},
                }
            })
        );
    }

    #[tokio::test]
    async fn a_non_blocking_event_still_gets_no_opinion() {
        let (bus, _rx) = bus();
        assert_eq!(
            bus.handle(envelope("Stop", serde_json::json!({})))
                .await
                .to_hook_json(),
            serde_json::json!({})
        );
    }

    #[tokio::test]
    async fn an_allow_always_carries_the_grant_back_to_the_harness() {
        // Measured: with the suggestion echoed in updatedPermissions the next
        // request for the same tool never fires. Dropping it here would make
        // "always" silently mean "once".
        let (bus, mut rx) = bus();
        let pending = bus.pending();
        let handle = tokio::spawn(async move {
            bus.deliver(envelope("PermissionRequest", permission_request()))
                .await
        });
        let key = next_decision_key(&mut rx).await;
        let suggestion =
            serde_json::json!({"type": "setMode", "mode": "acceptEdits", "destination": "session"});
        assert!(matches!(
            pending.resolve(
                &key,
                crate::HookDecision::Allow {
                    updated_input: None,
                    updated_permissions: vec![suggestion.clone()],
                },
            ),
            crate::Outcome::Answered
        ));
        let json = handle.await.expect("handler").to_hook_json();
        assert_eq!(
            json["hookSpecificOutput"]["decision"]["updatedPermissions"],
            serde_json::json!([suggestion])
        );
    }

    #[tokio::test]
    async fn a_human_deny_reaches_the_agent_with_its_reason() {
        let (bus, mut rx) = bus();
        let pending = bus.pending();
        let handle = tokio::spawn(async move {
            bus.deliver(envelope("PermissionRequest", permission_request()))
                .await
        });
        let key = next_decision_key(&mut rx).await;
        assert!(matches!(
            pending.resolve(
                &key,
                crate::HookDecision::Deny {
                    message: "not that file".into(),
                },
            ),
            crate::Outcome::Answered
        ));
        let json = handle.await.expect("handler").to_hook_json();
        assert_eq!(
            json["hookSpecificOutput"]["decision"],
            serde_json::json!({"behavior": "deny", "message": "not that file"})
        );
    }

    #[tokio::test]
    async fn the_card_a_device_sees_carries_the_real_tool_input() {
        // The whole point of tier A over screen scraping (§2.2).
        let (bus, mut rx) = bus();
        let handle = tokio::spawn(async move {
            bus.handle(envelope("PermissionRequest", permission_request()))
                .await
        });
        let mut card = None;
        while card.is_none() {
            let observation = rx.recv().await.expect("an observation");
            if let ObservationPayload::InteractionRequested(payload) = &observation.body {
                card = Some(payload.interaction.clone());
            }
        }
        let card = card.expect("a card");
        assert_eq!(
            card.carrier,
            remuda_protocol::InteractionCarrier::HarnessHook
        );
        assert!(card.blocking, "the agent really is parked on this");
        let remuda_protocol::InteractionRequest::Approval(approval) = &card.request else {
            panic!("expected an approval");
        };
        assert_eq!(approval.title, "Write");
        assert_eq!(approval.description, "/tmp/p.txt");
        // The suggestion claude offered became an always-allow button.
        assert!(
            approval
                .options
                .iter()
                .any(|option| option.id == "allow-always-0"),
            "{:?}",
            approval.options
        );
        handle.abort();
    }

    #[tokio::test]
    async fn a_closed_journal_channel_does_not_stall_the_agent() {
        let tx = mpsc::channel(1).0;
        let bus = SignalBus::new(context(), tx, Arc::new(AtomicU64::new(0)));
        // A blocking event whose card cannot be journaled must fall back to
        // the agent's own prompt rather than parking for the whole TTL.
        let reply = bus
            .handle(envelope("PermissionRequest", permission_request()))
            .await;
        assert_eq!(reply.to_hook_json(), serde_json::json!({}));
    }

    fn ask_user_question_request() -> serde_json::Value {
        serde_json::json!({
            "tool_name": "AskUserQuestion",
            "tool_input": {"questions": [
                {"question": "下一步做什么？", "header": "下一步", "multiSelect": false,
                 "options": [
                    {"label": "继续排查", "description": "继续"},
                    {"label": "回到 GravityDB", "description": "切回"}]},
                {"question": "保存什么？", "header": "记忆", "multiSelect": true,
                 "options": [
                    {"label": "保存端口", "description": "端口"},
                    {"label": "保存环境变量", "description": "变量"}]}
            ]}
        })
    }

    fn post_tool_use_answers() -> serde_json::Value {
        serde_json::json!({
            "tool_name": "AskUserQuestion",
            "tool_input": {"questions": [
                {"question": "下一步做什么？", "header": "下一步", "multiSelect": false,
                 "options": [{"label": "继续排查"}, {"label": "回到 GravityDB"}]},
                {"question": "保存什么？", "header": "记忆", "multiSelect": true,
                 "options": [{"label": "保存端口"}, {"label": "保存环境变量"}]}
            ], "answers": {
                "下一步做什么？": "继续排查",
                "保存什么？": ["保存端口", "保存环境变量"]
            }},
            "tool_response": {"answers": {
                "下一步做什么？": "继续排查",
                "保存什么？": ["保存端口", "保存环境变量"]
            }}
        })
    }

    async fn next_interaction(
        rx: &mut mpsc::Receiver<Observation>,
    ) -> remuda_protocol::Interaction {
        loop {
            let observation = rx.recv().await.expect("an observation");
            if let ObservationPayload::InteractionRequested(payload) = &observation.body {
                return payload.interaction.clone();
            }
        }
    }

    #[tokio::test]
    async fn an_askuserquestion_hook_opens_a_question_card_not_an_approval() {
        let (bus, mut rx) = bus();
        let handle = tokio::spawn(async move {
            bus.handle(envelope("PermissionRequest", ask_user_question_request()))
                .await
        });
        let card = next_interaction(&mut rx).await;
        assert_eq!(card.kind, remuda_protocol::InteractionKind::Question);
        assert_eq!(
            card.carrier,
            remuda_protocol::InteractionCarrier::HarnessHook
        );
        let remuda_protocol::InteractionRequest::Question(request) = &card.request else {
            panic!("expected a question");
        };
        assert_eq!(request.fields.len(), 2);
        assert_eq!(request.fields[0].description.as_deref(), Some("下一步"));
        assert_eq!(
            request.fields[0].input,
            remuda_protocol::QuestionInput::SingleSelect
        );
        assert_eq!(
            request.fields[1].input,
            remuda_protocol::QuestionInput::MultiSelect
        );
        // The TUI always offers free text; so must the card.
        assert!(request.fields.iter().all(|field| field.allow_free_text));
        handle.abort();
    }

    #[tokio::test]
    async fn a_device_question_answer_reaches_the_hook_as_updated_input_answers() {
        let (bus, mut rx) = bus();
        let bus = Arc::new(bus);
        let delivery = Arc::clone(&bus);
        let handle = tokio::spawn(async move {
            delivery
                .deliver(envelope("PermissionRequest", ask_user_question_request()))
                .await
        });
        let card = next_interaction(&mut rx).await;
        let answer = remuda_protocol::InteractionAnswer::Question(Box::new(
            remuda_protocol::QuestionAnswer {
                answers: std::collections::BTreeMap::from([
                    (
                        "q0".into(),
                        remuda_protocol::QuestionFieldAnswer {
                            option_ids: vec!["回到 GravityDB".into()],
                            text: None,
                        },
                    ),
                    (
                        "q1".into(),
                        remuda_protocol::QuestionFieldAnswer {
                            option_ids: vec!["保存端口".into(), "保存环境变量".into()],
                            text: None,
                        },
                    ),
                ]),
            },
        ));
        assert_eq!(
            bus.resolve_answer(&card.meta.id, &answer),
            crate::Outcome::Answered
        );
        let reply = handle.await.expect("handler");
        let json = reply.to_hook_json();
        assert_eq!(
            json["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
        assert_eq!(json["hookSpecificOutput"]["decision"]["behavior"], "allow");
        assert_eq!(
            json["hookSpecificOutput"]["decision"]["updatedInput"]["answers"]["下一步做什么？"],
            serde_json::json!("回到 GravityDB")
        );
        assert_eq!(
            json["hookSpecificOutput"]["decision"]["updatedInput"]["answers"]["保存什么？"],
            serde_json::json!(["保存端口", "保存环境变量"])
        );
    }

    #[tokio::test]
    async fn an_empty_question_answer_denies_the_hook() {
        let (bus, mut rx) = bus();
        let bus = Arc::new(bus);
        let delivery = Arc::clone(&bus);
        let handle = tokio::spawn(async move {
            delivery
                .deliver(envelope("PermissionRequest", ask_user_question_request()))
                .await
        });
        let card = next_interaction(&mut rx).await;
        let answer = remuda_protocol::InteractionAnswer::Question(Box::new(
            remuda_protocol::QuestionAnswer {
                answers: std::collections::BTreeMap::new(),
            },
        ));
        assert_eq!(
            bus.resolve_answer(&card.meta.id, &answer),
            crate::Outcome::Answered
        );
        let json = handle.await.expect("handler").to_hook_json();
        assert_eq!(json["hookSpecificOutput"]["decision"]["behavior"], "deny");
    }

    #[tokio::test]
    async fn a_terminal_answer_resolves_the_card_and_clears_the_wait() {
        // The human answers in the TUI: no device answer ever arrives, but the
        // closing PostToolUse must resolve the interaction with source
        // terminal and leave nothing a late device answer could hit.
        let (bus, mut rx) = bus();
        let bus = Arc::new(bus);
        let delivery = Arc::clone(&bus);
        let handle = tokio::spawn(async move {
            delivery
                .deliver(envelope("PermissionRequest", ask_user_question_request()))
                .await
        });
        let card = next_interaction(&mut rx).await;
        assert!(bus.is_parked(&card.meta.id));
        bus.handle(envelope("PostToolUse", post_tool_use_answers()))
            .await;
        // Retiring the parked hook must let the delivery finish rather than
        // sitting out the full 15-minute wait.
        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("delivery unblocked")
            .expect("task");
        assert_eq!(
            reply.to_hook_json()["hookSpecificOutput"]["decision"]["behavior"],
            "deny",
            "nobody answered through the hook: fail closed"
        );

        // The terminal resolution is journaled as an entity lifecycle.
        let mut resolved = None;
        while let Ok(observation) = rx.try_recv() {
            if let ObservationPayload::Lifecycle(payload) = &observation.body
                && let remuda_protocol::LifecyclePayload::Entity(entity) = payload.as_ref()
                && let remuda_protocol::LifecycleEntity::Interaction(interaction) =
                    &entity.entity_value
                && interaction.meta.id == card.meta.id
            {
                resolved = Some(interaction.clone());
            }
        }
        let resolved = resolved.expect("a resolved interaction entity");
        assert_eq!(resolved.state, remuda_protocol::InteractionState::Resolved);
        let remuda_protocol::Knowledge::Known { value: committed } = &resolved.answer else {
            panic!("the terminal answer must be recorded");
        };
        assert_eq!(
            committed.actor.actor_type,
            remuda_protocol::ActorType::Human
        );
        assert_eq!(committed.actor.device_id, None, "no device answered");
        let remuda_protocol::InteractionAnswer::Question(question) = &committed.value else {
            panic!("expected a question answer");
        };
        assert_eq!(
            question.answers["q0"].option_ids,
            vec!["继续排查".to_owned()]
        );
        assert_eq!(
            question.answers["q1"].option_ids,
            vec!["保存端口".to_owned(), "保存环境变量".to_owned()]
        );
        assert_eq!(
            resolved.resolution,
            remuda_protocol::Knowledge::Known {
                value: remuda_protocol::InteractionResolution {
                    reason: remuda_protocol::InteractionResolutionReason::Answered,
                    event_ids: Vec::new(),
                }
            }
        );
        // The parked hook is gone: a late device answer cannot land.
        assert!(!bus.is_parked(&card.meta.id));
        let late = remuda_protocol::InteractionAnswer::Question(Box::new(
            remuda_protocol::QuestionAnswer {
                answers: std::collections::BTreeMap::new(),
            },
        ));
        assert_eq!(
            bus.resolve_answer(&card.meta.id, &late),
            crate::Outcome::Abandoned
        );
    }

    #[tokio::test]
    async fn an_unrelated_posttooluse_does_not_close_the_question() {
        let (bus, mut rx) = bus();
        let bus = Arc::new(bus);
        let delivery = Arc::clone(&bus);
        let handle = tokio::spawn(async move {
            delivery
                .deliver(envelope("PermissionRequest", ask_user_question_request()))
                .await
        });
        let card = next_interaction(&mut rx).await;
        // A Write finishing has nothing to do with the open question.
        bus.handle(envelope(
            "PostToolUse",
            serde_json::json!({
                "tool_name": "Write",
                "tool_response": {"stdout": "ok"}
            }),
        ))
        .await;
        assert!(bus.is_parked(&card.meta.id), "the question stays open");
        handle.abort();
    }

    // ----- auto mode: AskUserQuestion arrives as a PreToolUse (c-askq) -----

    /// The recorded auto-mode PreToolUse payload (claude 2.1.277,
    /// evidence askq-pretooluse-1), verbatim.
    fn auto_pretooluse() -> serde_json::Value {
        serde_json::from_str(include_str!("../fixtures/askuser/pretooluse-auto.json")).unwrap()
    }

    fn auto_posttooluse() -> serde_json::Value {
        serde_json::from_str(include_str!("../fixtures/askuser/posttooluse-auto.json")).unwrap()
    }

    /// Same call as [`ask_user_question_request`] but on the PreToolUse event,
    /// carrying the `tool_use_id` only that event has.
    fn ask_user_question_pretooluse() -> serde_json::Value {
        let mut payload = ask_user_question_request();
        if let serde_json::Value::Object(map) = &mut payload {
            map.insert(
                "tool_use_id".into(),
                serde_json::json!("call_00000000000000000000000001"),
            );
            map.insert("permission_mode".into(), serde_json::json!("auto"));
        }
        payload
    }

    fn tea_answer() -> remuda_protocol::InteractionAnswer {
        remuda_protocol::InteractionAnswer::Question(Box::new(remuda_protocol::QuestionAnswer {
            answers: std::collections::BTreeMap::from([(
                "q0".into(),
                remuda_protocol::QuestionFieldAnswer {
                    option_ids: vec!["Tea".into()],
                    text: None,
                },
            )]),
        }))
    }

    #[tokio::test]
    async fn an_auto_mode_pretooluse_opens_a_question_card() {
        // The owner's main path: auto permission mode, only a PreToolUse
        // arrives for AskUserQuestion — before this fix it was journaled as a
        // plain tool call and no card ever reached the phone.
        let (bus, mut rx) = bus();
        let handle =
            tokio::spawn(
                async move { bus.handle(envelope("PreToolUse", auto_pretooluse())).await },
            );
        let card = next_interaction(&mut rx).await;
        assert_eq!(card.kind, remuda_protocol::InteractionKind::Question);
        let remuda_protocol::InteractionRequest::Question(request) = &card.request else {
            panic!("expected a question");
        };
        assert_eq!(request.fields.len(), 1);
        assert_eq!(request.fields[0].title, "Tea or coffee?");
        assert_eq!(request.fields[0].description.as_deref(), Some("Beverage"));
        assert_eq!(request.fields[0].options.len(), 2);
        handle.abort();
    }

    #[tokio::test]
    async fn an_ordinary_pretooluse_opens_nothing() {
        // Only AskUserQuestion turns a PreToolUse into a blocker.
        let (bus, mut rx) = bus();
        bus.handle(envelope(
            "PreToolUse",
            serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}),
        ))
        .await;
        for observation in std::iter::from_fn(|| rx.try_recv().ok()) {
            assert!(
                !matches!(
                    observation.body,
                    ObservationPayload::InteractionRequested(_)
                ),
                "an ordinary PreToolUse must not open a card"
            );
        }
    }

    #[tokio::test]
    async fn an_auto_mode_question_answer_reaches_the_hook_in_pretooluse_shape() {
        let (bus, mut rx) = bus();
        let bus = Arc::new(bus);
        let delivery = Arc::clone(&bus);
        let handle = tokio::spawn(async move {
            delivery
                .deliver(envelope("PreToolUse", auto_pretooluse()))
                .await
        });
        let card = next_interaction(&mut rx).await;
        assert_eq!(
            bus.resolve_answer(&card.meta.id, &tea_answer()),
            crate::Outcome::Answered
        );
        let reply = handle.await.expect("handler");
        let json = reply.to_hook_json();
        // PreToolUse vocabulary: permissionDecision + updatedInput at the
        // hookSpecificOutput level — the PermissionRequest decision.behavior
        // shape is deprecated and dropped on this event.
        assert_eq!(json["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(
            json["hookSpecificOutput"]["permissionDecision"],
            serde_json::json!("allow")
        );
        assert_eq!(
            json["hookSpecificOutput"]["updatedInput"]["answers"]["Tea or coffee?"],
            serde_json::json!("Tea")
        );
        assert!(
            json["hookSpecificOutput"].get("decision").is_none(),
            "{json}"
        );
    }

    #[tokio::test]
    async fn paired_pretooluse_and_permission_request_open_one_card() {
        // Measured on 2.1.277: a call whose first hook gives no opinion is
        // raised again on the other event ~tens of ms later. Same call: one
        // card, both hooks parked.
        let (bus, mut rx) = bus();
        let bus = Arc::new(bus);
        let pre = Arc::clone(&bus);
        let perm = Arc::clone(&bus);
        let pre_handle = tokio::spawn(async move {
            pre.deliver(envelope("PreToolUse", ask_user_question_pretooluse()))
                .await
        });
        let card = next_interaction(&mut rx).await;
        let perm_handle = tokio::spawn(async move {
            perm.deliver(envelope("PermissionRequest", ask_user_question_request()))
                .await
        });
        // Let the paired hook park.
        while bus.pending().len() < 2 {
            tokio::task::yield_now().await;
        }
        // Exactly one interaction card for the two hook events.
        let mut cards = 0;
        while let Ok(observation) = rx.try_recv() {
            if matches!(
                observation.body,
                ObservationPayload::InteractionRequested(_)
            ) {
                cards += 1;
            }
        }
        assert_eq!(cards, 0, "the second event must not emit another card");
        assert_eq!(
            bus.resolve_answer(
                &card.meta.id,
                &remuda_protocol::InteractionAnswer::Question(Box::new(
                    remuda_protocol::QuestionAnswer {
                        answers: std::collections::BTreeMap::from([
                            (
                                "q0".into(),
                                remuda_protocol::QuestionFieldAnswer {
                                    option_ids: vec!["回到 GravityDB".into()],
                                    text: None,
                                },
                            ),
                            (
                                "q1".into(),
                                remuda_protocol::QuestionFieldAnswer {
                                    option_ids: vec!["保存端口".into(), "保存环境变量".into()],
                                    text: None,
                                },
                            ),
                        ]),
                    },
                )),
            ),
            crate::Outcome::Answered
        );
        let pre_reply = pre_handle.await.expect("handler").to_hook_json();
        let perm_reply = perm_handle.await.expect("handler").to_hook_json();
        // One decision, each serialized in its own event's vocabulary.
        assert_eq!(
            pre_reply["hookSpecificOutput"]["hookEventName"],
            "PreToolUse"
        );
        assert_eq!(
            pre_reply["hookSpecificOutput"]["permissionDecision"],
            serde_json::json!("allow")
        );
        assert_eq!(
            pre_reply["hookSpecificOutput"]["updatedInput"]["answers"]["下一步做什么？"],
            serde_json::json!("回到 GravityDB")
        );
        assert_eq!(
            perm_reply["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
        assert_eq!(
            perm_reply["hookSpecificOutput"]["decision"]["behavior"],
            serde_json::json!("allow")
        );
        assert_eq!(
            perm_reply["hookSpecificOutput"]["decision"]["updatedInput"]["answers"]["下一步做什么？"],
            serde_json::json!("回到 GravityDB")
        );
        // Both hooks done: the association is reaped.
        assert_eq!(bus.pending().len(), 0);
    }

    #[tokio::test]
    async fn paired_events_in_the_other_order_also_open_one_card() {
        // Do not rely on the measured ordering: a PermissionRequest could be
        // the first hook observed.
        let (bus, mut rx) = bus();
        let bus = Arc::new(bus);
        let perm = Arc::clone(&bus);
        let pre = Arc::clone(&bus);
        let perm_handle = tokio::spawn(async move {
            perm.deliver(envelope("PermissionRequest", ask_user_question_request()))
                .await
        });
        let card = next_interaction(&mut rx).await;
        let pre_handle = tokio::spawn(async move {
            pre.deliver(envelope("PreToolUse", ask_user_question_pretooluse()))
                .await
        });
        while bus.pending().len() < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            bus.resolve_answer(&card.meta.id, &tea_answer_alt()),
            crate::Outcome::Answered
        );
        assert_eq!(
            perm_handle.await.expect("handler").to_hook_json()["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
        assert_eq!(
            pre_handle.await.expect("handler").to_hook_json()["hookSpecificOutput"]["hookEventName"],
            "PreToolUse"
        );
    }

    fn tea_answer_alt() -> remuda_protocol::InteractionAnswer {
        remuda_protocol::InteractionAnswer::Question(Box::new(remuda_protocol::QuestionAnswer {
            answers: std::collections::BTreeMap::from([(
                "q0".into(),
                remuda_protocol::QuestionFieldAnswer {
                    option_ids: vec!["继续排查".into()],
                    text: None,
                },
            )]),
        }))
    }

    #[tokio::test]
    async fn a_terminal_answer_closes_the_auto_mode_card_and_both_hooks() {
        // The human answers in the TUI while BOTH hooks are parked: the
        // PostToolUse resolves the card terminal-sourced and retires both
        // waits, so neither relay sits out its full deadline.
        let (bus, mut rx) = bus();
        let bus = Arc::new(bus);
        let pre = Arc::clone(&bus);
        let perm = Arc::clone(&bus);
        let pre_handle = tokio::spawn(async move {
            pre.deliver(envelope("PreToolUse", ask_user_question_pretooluse()))
                .await
        });
        let card = next_interaction(&mut rx).await;
        let perm_handle = tokio::spawn(async move {
            perm.deliver(envelope("PermissionRequest", ask_user_question_request()))
                .await
        });
        while bus.pending().len() < 2 {
            tokio::task::yield_now().await;
        }
        bus.handle(envelope("PostToolUse", post_tool_use_answers()))
            .await;
        for handle in [pre_handle, perm_handle] {
            let reply = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
                .await
                .expect("delivery unblocked")
                .expect("task");
            // Nobody answered through the hooks: fail closed on both.
            assert!(
                reply.to_hook_json().to_string().contains("deny"),
                "{}",
                reply.to_hook_json()
            );
        }
        assert!(!bus.is_parked(&card.meta.id));
    }

    #[tokio::test]
    async fn identical_questions_from_different_agents_open_two_cards() {
        // The main session and a sub-agent may ask the same text concurrently;
        // the fingerprint is scoped by agent_id so they never share one card.
        let (bus, mut rx) = bus();
        let bus = Arc::new(bus);
        let main = Arc::clone(&bus);
        let sub = Arc::clone(&bus);
        let main_handle = tokio::spawn(async move {
            main.deliver(envelope("PreToolUse", auto_pretooluse()))
                .await
        });
        let first = next_interaction(&mut rx).await;
        let mut sub_payload = auto_pretooluse();
        sub_payload["agent_id"] = serde_json::json!("subagent-7");
        let sub_handle =
            tokio::spawn(async move { sub.deliver(envelope("PreToolUse", sub_payload)).await });
        let second = next_interaction(&mut rx).await;
        assert_ne!(first.meta.id, second.meta.id, "two agents, two cards");
        assert_eq!(bus.pending().len(), 2);
        for handle in [main_handle, sub_handle] {
            handle.abort();
        }
    }

    #[tokio::test]
    async fn a_terminal_answer_on_a_lone_pretooluse_card_resolves_it() {
        // The pure auto-mode sequence from the recorded fixture: PreToolUse
        // only, then the human answers in the TUI.
        let (bus, mut rx) = bus();
        let bus = Arc::new(bus);
        let delivery = Arc::clone(&bus);
        let handle = tokio::spawn(async move {
            delivery
                .deliver(envelope("PreToolUse", auto_pretooluse()))
                .await
        });
        let card = next_interaction(&mut rx).await;
        bus.handle(envelope("PostToolUse", auto_posttooluse()))
            .await;
        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("delivery unblocked")
            .expect("task");
        assert_eq!(
            reply.to_hook_json()["hookSpecificOutput"]["permissionDecision"],
            serde_json::json!("deny")
        );
        // The terminal resolution is journaled against the same card.
        let mut resolved = false;
        while let Ok(observation) = rx.try_recv() {
            if let ObservationPayload::Lifecycle(payload) = &observation.body
                && let remuda_protocol::LifecyclePayload::Entity(entity) = payload.as_ref()
                && let remuda_protocol::LifecycleEntity::Interaction(interaction) =
                    &entity.entity_value
                && interaction.meta.id == card.meta.id
            {
                assert_eq!(
                    entity.reason_code, "terminal-answered",
                    "the phone card must show who answered"
                );
                resolved = true;
            }
        }
        assert!(resolved, "a terminal resolution was journaled");
    }
}
