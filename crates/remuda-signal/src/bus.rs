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
use crate::map::{Mapped, MappedKind, map_event};
use crate::socket::SignalSink;
use remuda_protocol::{
    Completeness, EventId, HostId, Id, InstanceId, Knowledge, NativeRequestKey, Observation,
    ObservationPayload, ObservationSource, RunId, RuntimeCursor, SchemaVersion, SourceChannel,
    SourceCursor, SourceDelivery, Timestamp, U64,
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
    blocking_wait: std::time::Duration,
    /// P5 (c-hookgap): turns observed on the hook channel, most recent last,
    /// plus the pid a `Stop`/`Esc` interrupt is aimed at.
    turn_active: std::sync::Mutex<VecDeque<TurnState>>,
    interrupt_pid: Arc<AtomicI32>,
}

/// The context kept for one parked blocking hook, so an answer arriving later
/// can be turned back into the decision the harness reads.
#[derive(Clone)]
struct ParkedHook {
    /// Suggestions the harness offered; an allow-always answer echoes one.
    suggestions: Vec<crate::PermissionSuggestion>,
}

/// One turn as seen on the hook channel (P5, c-hookgap).
struct TurnState {
    /// Agent pid for the turn, from the relay's parent.
    pid: i32,
    /// Native session id, once a `SessionStart` named it.
    session_id: Option<String>,
    /// Whether the turn is currently active.
    active: Option<bool>,
}

impl SignalBus {
    /// Build a bus that emits on `events`.
    #[must_use]
    pub fn new(
        context: BusContext,
        events: mpsc::Sender<Observation>,
        seq: Arc<AtomicU64>,
    ) -> Self {
        Self {
            context,
            events,
            seq,
            binding: std::sync::Mutex::new(None),
            pending: crate::pending::PendingDecisions::new(),
            parked: std::sync::Mutex::new(std::collections::HashMap::new()),
            blocking_wait: crate::BLOCKING_WAIT,
            turn_active: std::sync::Mutex::new(VecDeque::new()),
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
        let parked = self
            .parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned();
        let Some(parked) = parked else {
            // No parked context: the hook already ended. Still report
            // honestly rather than claiming a delivery we cannot prove.
            return self.pending.resolve(
                &key,
                crate::HookDecision::Deny {
                    message: "Remuda: the answer arrived after the request closed".to_owned(),
                },
            );
        };
        let Some(decision) = parked.to_decision(answer) else {
            tracing::warn!(?answer, "a hook answer did not match its interaction kind");
            return self.pending.resolve(
                &key,
                crate::HookDecision::Deny {
                    message: "Remuda: the answer did not match the request".to_owned(),
                },
            );
        };
        self.pending.resolve(&key, decision)
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
            });
        } else {
            slots.push_back(TurnState {
                pid,
                session_id,
                active,
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

    /// Handle one authenticated event: journal it, then answer the agent.
    pub async fn handle(&self, envelope: HookEnvelope) -> HookReply {
        let event = HookEvent::from_envelope(envelope);
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
        if let Err(error) = self.emit(&mapped).await {
            tracing::debug!(%error, event = %event.name, "hook observation not journaled");
        }
        if interrupted {
            let mut confirmed = mapped.clone();
            if let ObservationPayload::Lifecycle(payload) = &mut confirmed.payload
                && let remuda_protocol::LifecyclePayload::Native(native) = payload.as_mut()
            {
                native.native_name = "interrupted".into();
                native
                    .related_ids
                    .insert("afterEvent".into(), event.name.clone());
                native
                    .related_ids
                    .insert("requestedBy".into(), "instance.cancel".into());
            }
            if let Err(error) = self.emit(&confirmed).await {
                tracing::debug!(%error, "confirmed interrupt not journaled");
            }
        }
        // Blocking events park on the socket and are answered from the
        // interaction card (tier A). Everything else returns no opinion.
        if crate::event::is_blocking(&event.name) {
            return self.adjudicate(&event).await;
        }
        HookReply::empty()
    }

    /// Open an interaction for a blocking event and wait for its decision.
    ///
    /// The agent is parked on this reply, so every path out of here has to
    /// produce one: if the interaction cannot be opened at all, the reply is
    /// `{}` and the agent falls back to its own on-screen dialog, which is the
    /// honest outcome — Remuda could not take the decision, so it must not
    /// pretend to have taken it.
    async fn adjudicate(&self, event: &HookEvent) -> HookReply {
        let Some((interaction, key)) = self.open_interaction(event) else {
            tracing::debug!(
                event = %event.name,
                "no interaction could be opened; the agent keeps its own prompt"
            );
            return HookReply::empty();
        };
        if !self.emit_interaction(interaction).await {
            // Nobody can see a card that never reached the journal. Answering
            // it is impossible, so waiting on it would hang the agent for the
            // full TTL with no way for a human to intervene.
            tracing::debug!(event = %event.name, "interaction not journaled; not waiting on it");
            return HookReply::empty();
        }
        let suggestions = crate::decision::PermissionRequestEvent::from_event(event)
            .map(|request| request.suggestions)
            .unwrap_or_default();
        self.parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), ParkedHook { suggestions });
        let (decision, outcome) = self.pending.wait(key.clone(), self.blocking_wait).await;
        self.parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
        tracing::debug!(event = %event.name, ?outcome, "blocking hook resolved");
        HookReply {
            decision: Some(decision.to_hook_json(&event.name)),
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
                crate::approval::approval_interaction(&request, &context).ok()?
            }
            "Elicitation" => crate::approval::elicitation_interaction(event, &context).ok()?,
            _ => return None,
        };
        Some((
            interaction,
            crate::pending::DecisionKey::new(interaction_id.as_id().as_str()),
        ))
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

    fn build(&self, seq: u64, mapped: &Mapped) -> Option<Observation> {
        let observed_at = now_ts()?;
        Some(Observation {
            schema_version: SchemaVersion,
            event_id: EventId::new(),
            journal_id: self.context.journal_id.clone(),
            instance_id: self.context.instance_id.clone(),
            run_id: Some(self.context.run_id.clone()),
            host_id: self.context.host_id.clone(),
            process_generation: U64(1),
            run_generation: Some(U64(1)),
            seq: U64(seq),
            observed_at: observed_at.clone(),
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
                native_session_id: match &mapped.session_id {
                    Some(value) => Knowledge::Known {
                        value: value.clone(),
                    },
                    None => Knowledge::Unknown {
                        reason: "not-emitted".into(),
                        evidence_event_ids: Vec::new(),
                    },
                },
                native_turn_id: Knowledge::NotApplicable,
                native_agent_id: Knowledge::NotApplicable,
                native_item_id: Knowledge::NotApplicable,
                native_event_id: Knowledge::NotApplicable,
                native_request_id: NativeRequestKey::None,
                source_cursor: SourceCursor::Runtime(Box::new(RuntimeCursor {
                    ledger_revision: U64(seq),
                })),
            },
            completeness: mapped.completeness,
            raw_ref: None,
            evidence_event_ids: Vec::new(),
            body: mapped.payload.clone(),
        })
    }
}

impl SignalSink for SignalBus {
    fn deliver(
        &self,
        envelope: HookEnvelope,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = HookReply> + Send + '_>> {
        Box::pin(self.handle(envelope))
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
        &self,
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
                    let suggestion = self.suggestions.get(index)?.value.clone();
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
        let (tx, rx) = mpsc::channel(64);
        (
            SignalBus::new(context(), tx, Arc::new(AtomicU64::new(0))),
            rx,
        )
    }

    /// A bus with a short blocking wait, so timeout tests do not sleep.
    fn fast_bus() -> (SignalBus, mpsc::Receiver<Observation>) {
        let (tx, rx) = mpsc::channel(64);
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

    #[tokio::test]
    async fn cancel_is_confirmed_only_by_a_matching_native_turn_end() {
        let pending = Arc::new(AtomicI32::new(4242));
        let (bus, mut rx) = bus();
        let bus = bus.with_interrupt_tracker(Arc::clone(&pending));
        let mut other = envelope("Stop", serde_json::json!({}));
        other.ppid = 99;
        bus.handle(other).await;
        assert_eq!(
            native_lifecycle(&rx.recv().await.unwrap().body)
                .unwrap()
                .native_name,
            "Stop"
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(pending.load(Ordering::SeqCst), 4242);

        bus.handle(envelope("StopFailure", serde_json::json!({})))
            .await;
        assert_eq!(
            native_lifecycle(&rx.recv().await.unwrap().body)
                .unwrap()
                .native_name,
            "StopFailure"
        );
        let confirmed = rx.recv().await.unwrap();
        assert_eq!(confirmed.source.channel, SourceChannel::Hook);
        let native = native_lifecycle(&confirmed.body).unwrap();
        assert_eq!(native.native_name, "interrupted");
        assert_eq!(native.related_ids["afterEvent"], "StopFailure");
        assert_eq!(pending.load(Ordering::SeqCst), 0);
        assert_eq!(bus.turn_active(4242), Some(false));

        bus.handle(envelope("Stop", serde_json::json!({}))).await;
        rx.recv().await.unwrap();
        assert!(
            rx.try_recv().is_err(),
            "duplicate Stop cannot confirm twice"
        );
    }

    #[tokio::test]
    async fn a_new_turn_cannot_settle_a_previous_turns_pending_cancel() {
        let pending = Arc::new(AtomicI32::new(4242));
        let (bus, mut rx) = bus();
        let bus = bus.with_interrupt_tracker(pending);
        bus.handle(envelope("UserPromptSubmit", serde_json::json!({})))
            .await;
        rx.recv().await.unwrap();
        assert_eq!(bus.turn_active(4242), Some(true));
        bus.handle(envelope("Stop", serde_json::json!({}))).await;
        rx.recv().await.unwrap();
        assert!(rx.try_recv().is_err());
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
            rx.recv().await.unwrap();
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
            rx.recv().await.unwrap();
            assert!(
                rx.try_recv().is_err(),
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
        rx.recv().await.unwrap();
        assert_eq!(
            native_lifecycle(&rx.recv().await.unwrap().body)
                .unwrap()
                .native_name,
            "interrupted"
        );
        assert_eq!(bus.turn_active(4242), Some(false));
        bus.handle(envelope(
            "SessionStart",
            serde_json::json!({"session_id":"replacement"}),
        ))
        .await;
        rx.recv().await.unwrap();
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
    async fn sequence_numbers_are_monotonic_across_events() {
        let (bus, mut rx) = bus();
        for event in ["SessionStart", "UserPromptSubmit", "Stop"] {
            bus.handle(envelope(event, serde_json::json!({}))).await;
        }
        let mut seqs = Vec::new();
        for _ in 0..3 {
            seqs.push(rx.recv().await.unwrap().seq.0);
        }
        assert_eq!(seqs, vec![1, 2, 3]);
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
            bus.handle(envelope("PermissionRequest", permission_request()))
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

    #[tokio::test]
    async fn an_unanswered_request_denies_rather_than_allowing() {
        // §4.4 fail-closed, through the real bus wait.
        let (bus, mut rx) = fast_bus();
        let handle = tokio::spawn(async move {
            bus.handle(envelope("PermissionRequest", permission_request()))
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
            bus.handle(envelope(
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
            bus.handle(envelope("PermissionRequest", permission_request()))
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
            bus.handle(envelope("PermissionRequest", permission_request()))
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
}
