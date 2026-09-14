//! [`SignalBus`]: hook events in, [`Observation`]s out (D-028 §4.3, P1).
//!
//! The bus owns the stamping (`seq`, ids, source envelope) and the small amount
//! of state a hook stream needs: which native session this agent reported, and
//! which pid it reported it from. Classification itself lives in [`crate::map`].
//!
//! In P1 the bus never answers a blocking event — every reply is `{}` and the
//! agent keeps its own prompt on screen. The reply plumbing is real so that P5
//! is a change of decision, not a change of transport.

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
    turn_active: std::sync::Mutex<VecDeque<TurnState>>,
    interrupt_pid: Arc<AtomicI32>,
}

struct TurnState {
    pid: i32,
    session_id: Option<String>,
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
            turn_active: std::sync::Mutex::new(VecDeque::new()),
            interrupt_pid: Arc::new(AtomicI32::new(0)),
        }
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
        // P1 observes only. Answering `PermissionRequest` is P5; until then the
        // agent's own dialog stays the single place a decision is made, so
        // there is never a moment where Remuda thinks it answered and the
        // agent thinks it did not.
        //
        // TODO(x-p5) D-028 P6: codex's `PermissionRequest` and grok's
        // `PreToolUse{deny,ask}` are *blocking* hook events. The P6 adapters
        // journal them (so the waiting interaction is visible) but the verdict
        // transport belongs to P5's `InteractionRuntime`:
        //   1. on a blocking event, insert a pending `InteractionRequested`
        //      (carrier `ClaudeHook`/new `HarnessHook`) and await the broker;
        //   2. return `{"behavior":"allow"|"deny"}` (claude) or the codex
        //      `{"hookSpecificOutput":{"hookEventName":"PermissionRequest",
        //      "decision":{"behavior":…},"message":…}}` wrapper;
        //   3. on broker timeout, reply deny (§4.4: timeout is always deny).
        // Until that lands, `{}` keeps the harness's own screen dialog as the
        // authority — the measured grok path and the confined-session fallback
        // both require exactly that.
        HookReply::empty()
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
        let (tx, rx) = mpsc::channel(32);
        (
            SignalBus::new(context(), tx, Arc::new(AtomicU64::new(0))),
            rx,
        )
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

    #[tokio::test]
    async fn p1_never_answers_a_blocking_event() {
        let (bus, _rx) = bus();
        for event in ["PermissionRequest", "Elicitation"] {
            let reply = bus
                .handle(envelope(event, serde_json::json!({"tool_name": "Write"})))
                .await;
            assert_eq!(
                reply.to_hook_json(),
                serde_json::json!({}),
                "{event} must fall back to the agent's own prompt in P1"
            );
        }
    }

    #[tokio::test]
    async fn a_closed_journal_channel_does_not_stall_the_agent() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let bus = SignalBus::new(context(), tx, Arc::new(AtomicU64::new(0)));
        // The reply still arrives, so the agent is never left waiting on us.
        assert_eq!(
            bus.handle(envelope("Stop", serde_json::json!({})))
                .await
                .to_hook_json(),
            serde_json::json!({})
        );
    }
}
