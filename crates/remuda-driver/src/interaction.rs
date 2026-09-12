//! Driver-agnostic Interaction broker (M2). `protocol.md` §2.6/§5.4/§6, probe §0/§5, dispatcher §3.2.

use crate::capabilities::ADAPTER_VERSION;
use crate::materializer::LaunchOrigin;
use async_trait::async_trait;
use remuda_protocol::{
    ActorRef, ActorType, CommandId, Completeness, DeliveryState, DriverKind, EventId, HostId, Id,
    InstanceId, InteractionAnswer, InteractionAnsweredPayload, InteractionExpiredPayload,
    InteractionExpiredReason, InteractionId, InteractionKind, InteractionRequest, Knowledge,
    NativeRequestKey, NativeRequestValueType, Observation, ObservationPayload, ObservationSource,
    SchemaVersion, SourceChannel, SourceCursor, SourceDelivery, Timestamp, U64,
};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use tokio::sync::{Mutex, mpsc};

/// Default pending lifetime. Dispatcher cards expire in 10–15 min (`bot-dispatcher.md` §3.2).
pub const DEFAULT_TTL: Duration = Duration::from_secs(15 * 60);

/// Claude native request identity: control `request_id` plus `tool_use_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeRequestId {
    /// `control_request.request_id`.
    pub request_id: String,
    /// `can_use_tool.tool_use_id`, when the native frame has one.
    pub tool_use_id: Option<String>,
}

/// Row in the pending table, keyed by [`InteractionId`].
#[derive(Debug, Clone)]
pub struct PendingTicket {
    /// Owning instance.
    pub instance_id: InstanceId,
    /// Native request identity.
    pub native_request_id: NativeRequestId,
    /// UI/answer kind.
    pub kind: InteractionKind,
    /// Request schema payload.
    pub payload: InteractionRequest,
    /// When the ticket was created.
    pub created_at: Timestamp,
    /// Runtime answer deadline.
    pub expires_at: Timestamp,
    /// Run generation the request belongs to.
    pub run_generation: U64,
}

/// Per-instance policy applied at insert. `protocol.md` §6, D-011.
#[derive(Debug, Clone)]
pub struct InstancePolicy {
    /// `dontAsk` / bypass specs auto-allow approvals.
    pub auto_allow: bool,
    /// Bot/dispatcher origin auto-denies dangerous tools.
    pub origin: LaunchOrigin,
    /// Tool names treated as dangerous for bot auto-deny.
    pub dangerous_tools: BTreeSet<String>,
    /// Current run generation; tickets from older generations are stale.
    pub run_generation: U64,
}

impl InstancePolicy {
    /// Policy for a human instance that still asks.
    pub fn ask(run_generation: U64) -> Self {
        Self {
            auto_allow: false,
            origin: LaunchOrigin::Human,
            dangerous_tools: default_dangerous_tools(),
            run_generation,
        }
    }

    /// Policy for dontAsk / bypass specs.
    pub fn auto_allow(run_generation: U64) -> Self {
        Self {
            auto_allow: true,
            origin: LaunchOrigin::Human,
            dangerous_tools: default_dangerous_tools(),
            run_generation,
        }
    }
}

fn default_dangerous_tools() -> BTreeSet<String> {
    [
        "Bash",
        "bash",
        "Shell",
        "Write",
        "Edit",
        "MultiEdit",
        "NotebookEdit",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// Fields required to open a pending ticket.
#[derive(Debug, Clone)]
pub struct PendingSpec {
    /// Instance that owns the waiter.
    pub instance_id: InstanceId,
    /// Host for Observation envelopes.
    pub host_id: HostId,
    /// Native request identity.
    pub native_request_id: NativeRequestId,
    /// Request payload; kind is taken from this enum.
    pub payload: InteractionRequest,
    /// Generation at native request time.
    pub run_generation: U64,
    /// Native tool name for policy (Claude `tool_name`).
    pub tool_name: Option<String>,
}

/// Broker configuration.
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    /// Pending lifetime used when insert does not override expiry.
    pub ttl: Duration,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self { ttl: DEFAULT_TTL }
    }
}

/// Owning driver (trait object) that receives the unique committed answer.
#[async_trait]
pub trait InteractionOwner: Send + Sync {
    /// Forward the unique committed answer to native control/hook/RPC.
    async fn apply_answer(
        &self,
        id: InteractionId,
        answer: InteractionAnswer,
    ) -> Result<(), BrokerError>;

    /// Native deny/cancel after expiry or policy auto-deny.
    async fn deny_or_cancel(&self, id: InteractionId) -> Result<(), BrokerError>;
}

/// Outcome of a successful [`InteractionBroker::answer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerOutcome {
    /// This commandId won the CAS and was forwarded.
    Accepted,
    /// Same commandId retry after a prior win.
    Idempotent,
}

/// Broker errors. Later answers from other devices are [`BrokerError::Superseded`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrokerError {
    /// No pending or remembered ticket for this id.
    #[error("interaction not found")]
    NotFound,
    /// Deadline already passed.
    #[error("interaction expired")]
    Expired,
    /// Ticket runGeneration does not match the instance's current generation.
    #[error("stale run generation")]
    StaleGeneration,
    /// Another commandId already committed the unique answer.
    #[error("interaction already answered")]
    Superseded {
        /// Winning command.
        winner: CommandId,
    },
    /// No driver is registered for the instance.
    #[error("no interaction owner for instance")]
    OwnerMissing,
    /// Protocol scalar encoding failed.
    #[error("protocol value: {0}")]
    Protocol(String),
    /// Forwarding to the owner failed.
    #[error("forward: {0}")]
    Forward(String),
}

/// Insert-time policy decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyAction {
    /// Leave the ticket pending for devices.
    Pend,
    /// Auto-allow (dontAsk / bypass).
    AutoAllow,
    /// Auto-deny (bot origin + dangerous tool).
    AutoDeny,
}

/// M2 Interaction broker: one pending table, first valid answer wins.
pub struct InteractionBroker {
    inner: Mutex<Inner>,
    config: BrokerConfig,
    observations: mpsc::UnboundedSender<Observation>,
    seq: std::sync::atomic::AtomicU64,
    journal_id: Id,
}

struct Inner {
    pending: HashMap<InteractionId, LiveTicket>,
    answered: HashMap<InteractionId, CommandId>,
    owners: HashMap<InstanceId, Arc<dyn InteractionOwner>>,
    policies: HashMap<InstanceId, InstancePolicy>,
}

struct LiveTicket {
    table: PendingTicket,
    host_id: HostId,
    request_version: U64,
    deadline: Instant,
}

impl InteractionBroker {
    /// Create a broker and the Observation stream it publishes.
    pub fn new(
        config: BrokerConfig,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<Observation>), BrokerError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let journal_id = Id::new("obj").map_err(|e| BrokerError::Protocol(e.0))?;
        Ok((
            Arc::new(Self {
                inner: Mutex::new(Inner {
                    pending: HashMap::new(),
                    answered: HashMap::new(),
                    owners: HashMap::new(),
                    policies: HashMap::new(),
                }),
                config,
                observations: tx,
                seq: std::sync::atomic::AtomicU64::new(0),
                journal_id,
            }),
            rx,
        ))
    }

    /// Bind the instance to a driver trait object.
    pub async fn register_owner(&self, instance_id: InstanceId, owner: Arc<dyn InteractionOwner>) {
        self.inner.lock().await.owners.insert(instance_id, owner);
    }

    /// Install or replace per-instance policy.
    pub async fn set_policy(&self, instance_id: InstanceId, policy: InstancePolicy) {
        self.inner.lock().await.policies.insert(instance_id, policy);
    }

    /// Advance the instance generation; older tickets become stale.
    pub async fn set_run_generation(&self, instance_id: InstanceId, run_generation: U64) {
        let mut inner = self.inner.lock().await;
        inner
            .policies
            .entry(instance_id)
            .or_insert_with(|| InstancePolicy::ask(run_generation))
            .run_generation = run_generation;
    }

    /// Open a pending ticket and apply instance policy.
    pub async fn insert(&self, spec: PendingSpec) -> Result<InteractionId, BrokerError> {
        let kind = kind_of(&spec.payload);
        let tool_name = spec.tool_name.clone();
        let now = Instant::now();
        let created_at = timestamp_now()?;
        let expires_at = timestamp_after(self.config.ttl)?;
        let id = InteractionId::new();
        let table = PendingTicket {
            instance_id: spec.instance_id.clone(),
            native_request_id: spec.native_request_id,
            kind,
            payload: spec.payload,
            created_at,
            expires_at,
            run_generation: spec.run_generation,
        };
        let live = LiveTicket {
            table,
            host_id: spec.host_id,
            request_version: U64(1),
            deadline: now + self.config.ttl,
        };
        let policy = {
            let mut inner = self.inner.lock().await;
            let policy = inner
                .policies
                .get(&spec.instance_id)
                .cloned()
                .unwrap_or_else(|| InstancePolicy::ask(spec.run_generation));
            inner.pending.insert(id.clone(), live);
            policy
        };
        match policy_action(&policy, tool_name.as_deref(), kind) {
            PolicyAction::Pend => Ok(id),
            PolicyAction::AutoAllow => {
                let digest = policy_payload_digest(&id, &self.inner).await?;
                let answer = allow_answer(&digest);
                let cmd = CommandId::new();
                let device = Id::new("dev").map_err(|e| BrokerError::Protocol(e.0))?;
                self.answer(id.clone(), answer, device, cmd).await?;
                Ok(id)
            }
            PolicyAction::AutoDeny => {
                self.expire_one(&id, InteractionExpiredReason::NativeCancelled)
                    .await?;
                Ok(id)
            }
        }
    }

    /// First valid answer wins. Later commandIds are [`BrokerError::Superseded`].
    pub async fn answer(
        &self,
        interaction_id: InteractionId,
        answer: InteractionAnswer,
        by_device: Id,
        command_id: CommandId,
    ) -> Result<AnswerOutcome, BrokerError> {
        let (owner, ticket, actor) = {
            let mut inner = self.inner.lock().await;
            if let Some(winner) = inner.answered.get(&interaction_id) {
                if winner == &command_id {
                    return Ok(AnswerOutcome::Idempotent);
                }
                return Err(BrokerError::Superseded {
                    winner: winner.clone(),
                });
            }
            let Some(ticket) = inner.pending.get(&interaction_id) else {
                return Err(BrokerError::NotFound);
            };
            if Instant::now() >= ticket.deadline {
                return Err(BrokerError::Expired);
            }
            if let Some(policy) = inner.policies.get(&ticket.table.instance_id)
                && policy.run_generation != ticket.table.run_generation
            {
                return Err(BrokerError::StaleGeneration);
            }
            let owner = inner
                .owners
                .get(&ticket.table.instance_id)
                .cloned()
                .ok_or(BrokerError::OwnerMissing)?;
            let actor = ActorRef {
                principal_id: Id::new("prn").map_err(|e| BrokerError::Protocol(e.0))?,
                actor_type: ActorType::Human,
                device_id: Some(by_device),
                instance_id: Some(ticket.table.instance_id.clone()),
            };
            let ticket = inner
                .pending
                .remove(&interaction_id)
                .ok_or(BrokerError::NotFound)?;
            inner
                .answered
                .insert(interaction_id.clone(), command_id.clone());
            (owner, ticket, actor)
        };
        owner
            .apply_answer(interaction_id.clone(), answer)
            .await
            .map_err(|e| BrokerError::Forward(e.to_string()))?;
        let observation = answered_observation(
            &self.journal_id,
            &ticket,
            &interaction_id,
            &command_id,
            actor,
            self.next_seq(),
        )?;
        let _ = self.observations.send(observation);
        Ok(AnswerOutcome::Accepted)
    }

    /// Expire due tickets, emit `interaction.expired`, and tell the driver to deny/cancel.
    pub async fn sweep_expired(&self) -> usize {
        let due: Vec<InteractionId> = {
            let inner = self.inner.lock().await;
            let now = Instant::now();
            inner
                .pending
                .iter()
                .filter(|(_, t)| now >= t.deadline)
                .map(|(id, _)| id.clone())
                .collect()
        };
        let mut n = 0;
        for id in due {
            if self
                .expire_one(&id, InteractionExpiredReason::Deadline)
                .await
                .is_ok()
            {
                n += 1;
            }
        }
        n
    }

    /// Native request cleared/replaced. Retire the waiter without sending keys.
    pub async fn retire(&self, id: &InteractionId) {
        self.inner.lock().await.pending.remove(id);
    }

    /// Background sweeper. Interval should be well under [`DEFAULT_TTL`].
    pub fn spawn_sweeper(self: Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            loop {
                tick.tick().await;
                let _ = self.sweep_expired().await;
            }
        })
    }

    async fn expire_one(
        &self,
        id: &InteractionId,
        reason: InteractionExpiredReason,
    ) -> Result<(), BrokerError> {
        let (owner, ticket) = {
            let mut inner = self.inner.lock().await;
            let Some(ticket) = inner.pending.remove(id) else {
                return Err(BrokerError::NotFound);
            };
            let owner = inner.owners.get(&ticket.table.instance_id).cloned();
            (owner, ticket)
        };
        if let Some(owner) = owner {
            owner
                .deny_or_cancel(id.clone())
                .await
                .map_err(|e| BrokerError::Forward(e.to_string()))?;
        }
        let observation =
            expired_observation(&self.journal_id, &ticket, id, reason, self.next_seq())?;
        let _ = self.observations.send(observation);
        Ok(())
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
    }
}

/// Validate the displayed schema before consuming the first-answer CAS.
pub fn validate_answer(
    request: &InteractionRequest,
    answer: &InteractionAnswer,
) -> Result<(), BrokerError> {
    let invalid = || BrokerError::Protocol("answer does not match the interaction request".into());
    match (request, answer) {
        (InteractionRequest::Approval(request), InteractionAnswer::Approval(answer)) => {
            if request.input_digest != answer.input_digest
                || !request
                    .options
                    .iter()
                    .any(|option| option.id == answer.option_id)
            {
                return Err(invalid());
            }
        }
        (InteractionRequest::Question(request), InteractionAnswer::Question(answer)) => {
            if answer
                .answers
                .keys()
                .any(|id| !request.fields.iter().any(|field| &field.id == id))
            {
                return Err(invalid());
            }
            for field in &request.fields {
                let Some(value) = answer.answers.get(&field.id) else {
                    if field.required {
                        return Err(invalid());
                    }
                    continue;
                };
                let text = value.text.as_deref().unwrap_or("");
                if value
                    .option_ids
                    .iter()
                    .any(|id| !field.options.iter().any(|option| &option.id == id))
                    || value.option_ids.iter().collect::<BTreeSet<_>>().len()
                        != value.option_ids.len()
                    || (field.input != remuda_protocol::QuestionInput::MultiSelect
                        && value.option_ids.len() > 1)
                    || (field.input == remuda_protocol::QuestionInput::Text
                        && !value.option_ids.is_empty())
                    || (!field.allow_free_text && value.text.is_some())
                    || (field.required && value.option_ids.is_empty() && text.is_empty())
                {
                    return Err(invalid());
                }
            }
        }
        (InteractionRequest::PlanReview(_), InteractionAnswer::PlanReview(_))
        | (InteractionRequest::Elicitation(_), InteractionAnswer::Elicitation(_)) => {}
        _ => return Err(invalid()),
    }
    Ok(())
}

fn kind_of(payload: &InteractionRequest) -> InteractionKind {
    match payload {
        InteractionRequest::Approval(_) => InteractionKind::Approval,
        InteractionRequest::Question(_) => InteractionKind::Question,
        InteractionRequest::PlanReview(_) => InteractionKind::PlanReview,
        InteractionRequest::Elicitation(_) => InteractionKind::Elicitation,
    }
}

fn policy_action(
    policy: &InstancePolicy,
    tool_name: Option<&str>,
    kind: InteractionKind,
) -> PolicyAction {
    if policy.auto_allow && kind == InteractionKind::Approval {
        return PolicyAction::AutoAllow;
    }
    if policy.origin == LaunchOrigin::Bot
        && kind == InteractionKind::Approval
        && tool_name.is_some_and(|name| policy.dangerous_tools.contains(name))
    {
        return PolicyAction::AutoDeny;
    }
    PolicyAction::Pend
}

fn allow_answer(digest: &remuda_protocol::Digest) -> InteractionAnswer {
    InteractionAnswer::Approval(Box::new(remuda_protocol::ApprovalAnswer {
        option_id: "allow-once".into(),
        input_digest: digest.clone(),
    }))
}

async fn policy_payload_digest(
    id: &InteractionId,
    inner: &Mutex<Inner>,
) -> Result<remuda_protocol::Digest, BrokerError> {
    let inner = inner.lock().await;
    Ok(inner
        .pending
        .get(id)
        .and_then(|t| match &t.table.payload {
            InteractionRequest::Approval(a) => Some(a.input_digest.clone()),
            _ => None,
        })
        .unwrap_or(zero_digest()?))
}

fn zero_digest() -> Result<remuda_protocol::Digest, BrokerError> {
    remuda_protocol::Digest::try_from(
        "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string(),
    )
    .map_err(|e| BrokerError::Protocol(e.0))
}

fn timestamp_now() -> Result<Timestamp, BrokerError> {
    timestamp_from(OffsetDateTime::now_utc())
}

fn timestamp_after(ttl: Duration) -> Result<Timestamp, BrokerError> {
    let now = OffsetDateTime::now_utc();
    let delta = time::Duration::seconds(ttl.as_secs() as i64)
        + time::Duration::milliseconds((ttl.subsec_millis()) as i64);
    timestamp_from(now + delta)
}

fn timestamp_from(now: OffsetDateTime) -> Result<Timestamp, BrokerError> {
    let text = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.millisecond()
    );
    Timestamp::try_from(text).map_err(|e| BrokerError::Protocol(e.0))
}

fn unknown_knowledge<T>() -> Knowledge<T> {
    Knowledge::Unknown {
        reason: "broker".into(),
        evidence_event_ids: vec![],
    }
}

fn envelope(
    journal_id: &Id,
    ticket: &LiveTicket,
    seq: u64,
    observed_at: Timestamp,
    body: ObservationPayload,
) -> Result<Observation, BrokerError> {
    let native = NativeRequestKey::Rpc {
        value_type: NativeRequestValueType::String,
        value: ticket.table.native_request_id.request_id.clone(),
    };
    Ok(Observation {
        schema_version: SchemaVersion,
        event_id: EventId::new(),
        journal_id: journal_id.clone(),
        instance_id: ticket.table.instance_id.clone(),
        run_id: None,
        host_id: ticket.host_id.clone(),
        process_generation: U64(1),
        run_generation: Some(ticket.table.run_generation),
        seq: U64(seq),
        observed_at,
        native_at: unknown_knowledge(),
        source: ObservationSource {
            driver_kind: DriverKind::ClaudePrint,
            driver_version: "broker".into(),
            adapter_version: ADAPTER_VERSION.into(),
            channel: SourceChannel::Runtime,
            delivery: SourceDelivery::Live,
            native_session_id: unknown_knowledge(),
            native_turn_id: unknown_knowledge(),
            native_agent_id: Knowledge::NotApplicable,
            native_item_id: ticket
                .table
                .native_request_id
                .tool_use_id
                .clone()
                .map(|value| Knowledge::Known { value })
                .unwrap_or_else(unknown_knowledge),
            native_event_id: unknown_knowledge(),
            native_request_id: native,
            source_cursor: SourceCursor::Runtime(Box::new(remuda_protocol::RuntimeCursor {
                ledger_revision: U64(seq),
            })),
        },
        completeness: Completeness::Structured,
        raw_ref: None,
        evidence_event_ids: vec![],
        body,
    })
}

fn answered_observation(
    journal_id: &Id,
    ticket: &LiveTicket,
    interaction_id: &InteractionId,
    command_id: &CommandId,
    actor: ActorRef,
    seq: u64,
) -> Result<Observation, BrokerError> {
    let observed_at = timestamp_now()?;
    envelope(
        journal_id,
        ticket,
        seq,
        observed_at,
        ObservationPayload::InteractionAnswered(Box::new(InteractionAnsweredPayload {
            interaction_id: interaction_id.clone(),
            request_version: ticket.request_version,
            answer_command_id: command_id.clone(),
            actor,
            answer_ref: Id::new("obj").map_err(|e| BrokerError::Protocol(e.0))?,
            delivery: DeliveryState::Written,
        })),
    )
}

fn expired_observation(
    journal_id: &Id,
    ticket: &LiveTicket,
    interaction_id: &InteractionId,
    reason: InteractionExpiredReason,
    seq: u64,
) -> Result<Observation, BrokerError> {
    let observed_at = timestamp_now()?;
    envelope(
        journal_id,
        ticket,
        seq,
        observed_at,
        ObservationPayload::InteractionExpired(Box::new(InteractionExpiredPayload {
            interaction_id: interaction_id.clone(),
            request_version: ticket.request_version,
            reason,
            evidence_event_ids: vec![],
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_protocol::{ApprovalAnswer, ApprovalRequest, Digest};
    use tokio::sync::Mutex as TokioMutex;

    #[derive(Default)]
    struct RecordingOwner {
        answers: TokioMutex<Vec<(InteractionId, InteractionAnswer)>>,
        denied: TokioMutex<Vec<InteractionId>>,
    }

    #[async_trait]
    impl InteractionOwner for RecordingOwner {
        async fn apply_answer(
            &self,
            id: InteractionId,
            answer: InteractionAnswer,
        ) -> Result<(), BrokerError> {
            self.answers.lock().await.push((id, answer));
            Ok(())
        }

        async fn deny_or_cancel(&self, id: InteractionId) -> Result<(), BrokerError> {
            self.denied.lock().await.push(id);
            Ok(())
        }
    }

    fn digest() -> Digest {
        zero_digest().unwrap()
    }

    fn approval_payload() -> InteractionRequest {
        InteractionRequest::Approval(Box::new(ApprovalRequest {
            title: "Bash".into(),
            description: "run".into(),
            tool_call_id: None,
            action_ref: Id::new("obj").unwrap(),
            options: vec![],
            requested_permissions_ref: None,
            input_digest: digest(),
        }))
    }

    fn spec(instance: &InstanceId, host: &HostId, generation: u64) -> PendingSpec {
        PendingSpec {
            instance_id: instance.clone(),
            host_id: host.clone(),
            native_request_id: NativeRequestId {
                request_id: "native-perm-1".into(),
                tool_use_id: Some("toolu_example".into()),
            },
            payload: approval_payload(),
            run_generation: U64(generation),
            tool_name: Some("Bash".into()),
        }
    }

    fn allow() -> InteractionAnswer {
        InteractionAnswer::Approval(Box::new(ApprovalAnswer {
            option_id: "allow-once".into(),
            input_digest: digest(),
        }))
    }

    async fn harness(
        ttl: Duration,
        policy: InstancePolicy,
    ) -> (
        Arc<InteractionBroker>,
        mpsc::UnboundedReceiver<Observation>,
        Arc<RecordingOwner>,
        InstanceId,
        HostId,
    ) {
        let (broker, rx) = InteractionBroker::new(BrokerConfig { ttl }).unwrap();
        let owner = Arc::new(RecordingOwner::default());
        let instance = InstanceId::new();
        let host = HostId::new();
        broker.register_owner(instance.clone(), owner.clone()).await;
        broker.set_policy(instance.clone(), policy).await;
        (broker, rx, owner, instance, host)
    }

    #[tokio::test]
    async fn first_answer_wins_across_two_devices() {
        let (broker, mut rx, owner, instance, host) =
            harness(DEFAULT_TTL, InstancePolicy::ask(U64(1))).await;
        let id = broker.insert(spec(&instance, &host, 1)).await.unwrap();
        let cmd_a = CommandId::new();
        let cmd_b = CommandId::new();
        let dev_a = Id::new("dev").unwrap();
        let dev_b = Id::new("dev").unwrap();
        let (left, right) = tokio::join!(
            broker.answer(id.clone(), allow(), dev_a, cmd_a.clone()),
            broker.answer(id.clone(), allow(), dev_b, cmd_b.clone()),
        );
        let wins = [&left, &right]
            .iter()
            .filter(|r| matches!(r, Ok(AnswerOutcome::Accepted)))
            .count();
        let lost = [&left, &right]
            .iter()
            .filter(|r| matches!(r, Err(BrokerError::Superseded { .. })))
            .count();
        assert_eq!(wins, 1);
        assert_eq!(lost, 1);
        assert_eq!(owner.answers.lock().await.len(), 1);
        let event = rx.recv().await.expect("answered observation");
        assert!(matches!(
            event.body,
            ObservationPayload::InteractionAnswered(_)
        ));
        let winner = match (&left, &right) {
            (Ok(_), Err(BrokerError::Superseded { winner })) => winner,
            (Err(BrokerError::Superseded { winner }), Ok(_)) => winner,
            other => panic!("unexpected pair {other:?}"),
        };
        assert!(winner == &cmd_a || winner == &cmd_b);
    }

    #[tokio::test]
    async fn expiry_sweeper_emits_expired_and_denies() {
        let (broker, mut rx, owner, instance, host) =
            harness(Duration::from_millis(0), InstancePolicy::ask(U64(1))).await;
        let id = broker.insert(spec(&instance, &host, 1)).await.unwrap();
        assert_eq!(broker.sweep_expired().await, 1);
        assert_eq!(*owner.denied.lock().await, vec![id.clone()]);
        let event = rx.recv().await.expect("expired observation");
        assert!(matches!(
            event.body,
            ObservationPayload::InteractionExpired(_)
        ));
        let err = broker
            .answer(id, allow(), Id::new("dev").unwrap(), CommandId::new())
            .await
            .unwrap_err();
        assert!(matches!(err, BrokerError::NotFound | BrokerError::Expired));
    }

    #[tokio::test]
    async fn stale_generation_is_rejected() {
        let (broker, _rx, _owner, instance, host) =
            harness(DEFAULT_TTL, InstancePolicy::ask(U64(1))).await;
        let id = broker.insert(spec(&instance, &host, 1)).await.unwrap();
        broker.set_run_generation(instance, U64(2)).await;
        let err = broker
            .answer(id, allow(), Id::new("dev").unwrap(), CommandId::new())
            .await
            .unwrap_err();
        assert_eq!(err, BrokerError::StaleGeneration);
    }

    #[tokio::test]
    async fn policy_auto_allow_forwards_without_waiting() {
        let (broker, mut rx, owner, instance, host) =
            harness(DEFAULT_TTL, InstancePolicy::auto_allow(U64(1))).await;
        let id = broker.insert(spec(&instance, &host, 1)).await.unwrap();
        assert_eq!(owner.answers.lock().await.len(), 1);
        assert_eq!(owner.answers.lock().await[0].0, id);
        let event = rx.recv().await.expect("auto-allow answered");
        assert!(matches!(
            event.body,
            ObservationPayload::InteractionAnswered(_)
        ));
    }
}
