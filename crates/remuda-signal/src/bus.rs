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
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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
    blocking_wait: std::time::Duration,
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
            blocking_wait: crate::BLOCKING_WAIT,
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
        if let Err(error) = self.emit(&mapped).await {
            tracing::debug!(%error, event = %event.name, "hook observation not journaled");
        }
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
        let (decision, outcome) = self.pending.wait(key, self.blocking_wait).await;
        tracing::debug!(event = %event.name, ?outcome, "blocking hook resolved");
        HookReply {
            decision: Some(decision.to_hook_json(&event.name)),
        }
    }

    /// Build the entity a device answers, plus the key its hook is filed under.
    fn open_interaction(
        &self,
        event: &HookEvent,
    ) -> Option<(remuda_protocol::Interaction, crate::pending::DecisionKey)> {
        let invocation = Id::new("hook").ok()?;
        let context = crate::approval::ApprovalContext {
            instance_id: self.context.instance_id.clone(),
            host_id: self.context.host_id.clone(),
            run_id: self.context.run_id.clone(),
            decision_key: invocation.clone(),
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
            crate::pending::DecisionKey::new(invocation.as_str()),
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
        pending.resolve(
            &key,
            crate::HookDecision::Allow {
                updated_input: None,
                updated_permissions: vec![suggestion.clone()],
            },
        );
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
        pending.resolve(
            &key,
            crate::HookDecision::Deny {
                message: "not that file".into(),
            },
        );
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
