//! InteractionBroker glue: pending table, driver forwarding, and Hub RPC.

use crate::{Driver, DriverRequest, LocalStore, NodeError};
use async_trait::async_trait;
use remuda_driver::interaction::{
    AnswerCaller, BrokerConfig, BrokerError, InstancePolicy, InteractionBroker, InteractionOwner,
    NativeRequestId, PendingSpec,
};
use remuda_protocol::{
    ApprovalRequest, CommandId, Completeness, DecisionEffect, DecisionOption, Digest, EntityMeta,
    Id, Instance, InstanceId, Interaction, InteractionAnswer, InteractionCarrier, InteractionId,
    InteractionKind, InteractionRequest, InteractionRequestKey, InteractionState, NativeRequestKey,
    NativeRequestValueType, Observation, ObservationPayload, U64,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Default broker expiry sweep interval.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// One pending Interaction as listed to Hub.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingInteraction {
    /// Public interaction identity (driver/journal id).
    pub interaction_id: InteractionId,
    /// Owning instance.
    pub instance_id: InstanceId,
    /// Owning host.
    pub host_id: remuda_protocol::HostId,
    /// UI kind.
    pub kind: InteractionKind,
    /// Full request entity from `interaction.requested`.
    pub interaction: Interaction,
}

/// Per-Node Interaction broker, pending table, and driver owners.
pub struct InteractionRuntime {
    broker: Arc<InteractionBroker>,
    store: Arc<dyn LocalStore>,
    glue: Arc<Mutex<Glue>>,
    /// Signals the broker pump and the expiry sweeper to exit on teardown.
    shutdown: Arc<tokio::sync::Notify>,
    /// Detached tasks that hold `store` clones (the pump and the sweeper both
    /// append observations); aborted on shutdown so they cannot outlive the
    /// Node that owns them.
    tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

struct Glue {
    pending: HashMap<InteractionId, PendingInteraction>,
    native_to_broker: HashMap<InteractionId, InteractionId>,
    broker_to_native: HashMap<InteractionId, InteractionId>,
    seen_native: HashSet<InteractionId>,
}

struct DriverOwner {
    driver: Arc<dyn Driver>,
    glue: Arc<Mutex<Glue>>,
    store: Arc<dyn LocalStore>,
}

#[async_trait]
impl InteractionOwner for DriverOwner {
    async fn apply_answer(
        &self,
        id: InteractionId,
        answer: InteractionAnswer,
    ) -> Result<(), BrokerError> {
        let native = {
            let glue = self.glue.lock().await;
            glue.broker_to_native
                .get(&id)
                .cloned()
                .ok_or(BrokerError::NotFound)?
        };
        // Persist a consumed ticket before any possible native key write. A
        // restart cannot re-create an answerable waiter from this record.
        if let Some(mut row) = self.glue.lock().await.pending.get(&native).cloned() {
            row.interaction.state = InteractionState::AnswerCommitted;
            row.interaction.delivery = remuda_protocol::DeliveryState::IntentDurable;
            self.store
                .put_pending_interaction(&row)
                .map_err(|error| BrokerError::Forward(error.to_string()))?;
        }
        let value =
            serde_json::to_value(&answer).map_err(|err| BrokerError::Forward(err.to_string()))?;
        let emissions = self
            .driver
            .execute(DriverRequest::RespondInteraction {
                interaction_id: native.as_id().to_string(),
                answer: value,
            })
            .await
            .map_err(|err| BrokerError::Forward(err.to_string()))?;
        let instance_id = {
            let glue = self.glue.lock().await;
            glue.pending.get(&native).map(|row| row.instance_id.clone())
        };
        if let Some(instance_id) = instance_id {
            for emission in emissions {
                let payload = emission
                    .into_payload()
                    .map_err(|err| BrokerError::Forward(err.to_string()))?;
                self.store
                    .append_observation(&instance_id, None, Completeness::Structured, payload)
                    .map_err(|err| BrokerError::Forward(err.to_string()))?;
            }
        }
        Ok(())
    }

    async fn deny_or_cancel(&self, _id: InteractionId) -> Result<(), BrokerError> {
        Ok(())
    }
}

impl InteractionRuntime {
    /// Bind a broker, journal sink, and expiry sweeper to this Node store.
    pub fn spawn(store: Arc<dyn LocalStore>) -> Result<Arc<Self>, NodeError> {
        let (broker, mut broker_rx) = InteractionBroker::new(BrokerConfig::default())
            .map_err(|err| NodeError::Driver(err.to_string()))?;
        let mut pending = HashMap::new();
        let mut seen_native = HashSet::new();
        for row in store.pending_interactions()? {
            seen_native.insert(row.interaction_id.clone());
            if row.interaction.state == InteractionState::Pending {
                // A restarted Node has no proven native waiter; keep it visible,
                // but do not claim this old ticket is answerable.
                let mut row = row;
                row.interaction.answerable = false;
                pending.insert(row.interaction_id.clone(), row);
            }
        }
        let runtime = Arc::new(Self {
            broker: Arc::clone(&broker),
            store,
            glue: Arc::new(Mutex::new(Glue {
                pending,
                native_to_broker: HashMap::new(),
                broker_to_native: HashMap::new(),
                seen_native,
            })),
            shutdown: Arc::new(tokio::sync::Notify::new()),
            tasks: std::sync::Mutex::new(Vec::new()),
        });
        let pump = Arc::clone(&runtime);
        let pump_shutdown = Arc::clone(&runtime.shutdown);
        let pump_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = pump_shutdown.notified() => break,
                    observation = broker_rx.recv() => {
                        let Some(observation) = observation else { break };
                        if let Err(error) =
                            pump.commit_broker_observation(observation).await
                        {
                            tracing::debug!(%error, "interaction broker observation commit failed");
                        }
                    }
                }
            }
        });
        let sweeper_task = broker.spawn_sweeper(SWEEP_INTERVAL);
        runtime
            .tasks
            .lock()
            .expect("interaction tasks")
            .extend([pump_task, sweeper_task]);
        Ok(runtime)
    }

    /// Stop the broker pump and the expiry sweeper synchronously.
    ///
    /// Both append journal observations through the shared store, so a Node
    /// being torn down aborts them before closing the journal; otherwise a
    /// dropped Node could keep journaling after its successor reopened the same
    /// data dir. Safe to call from a drop: notification and abort are
    /// non-async.
    pub(crate) fn shutdown(&self) {
        self.shutdown.notify_waiters();
        let mut tasks = self.tasks.lock().expect("interaction tasks");
        for task in tasks.drain(..) {
            task.abort();
        }
    }

    /// Register the instance driver as the unique Interaction owner.
    pub async fn register_driver(&self, instance_id: InstanceId, driver: Arc<dyn Driver>) {
        self.broker
            .register_owner(
                instance_id.clone(),
                Arc::new(DriverOwner {
                    driver,
                    glue: Arc::clone(&self.glue),
                    store: Arc::clone(&self.store),
                }),
            )
            .await;
        self.broker
            .set_policy(instance_id, InstancePolicy::ask(U64(1)))
            .await;
    }

    /// Fold a committed observation into the broker pending table.
    pub async fn ingest(&self, observation: &Observation) -> Result<(), NodeError> {
        if let ObservationPayload::Lifecycle(payload) = &observation.body {
            if let remuda_protocol::LifecyclePayload::Native(native) = payload.as_ref()
                && matches!(native.native_name.as_str(), "agent_status" | "session")
                && let remuda_protocol::Knowledge::Known { value } = &native.status
                && self
                    .store
                    .get_instance(&observation.instance_id)?
                    .native_ref
                    .signal_tier
                    != Some(remuda_protocol::SignalTier::Hook)
            {
                let activity = match value.as_str() {
                    "blocked" => Some(remuda_protocol::Activity::WaitingInteraction),
                    "idle" | "done" => Some(remuda_protocol::Activity::Idle),
                    "working" => Some(remuda_protocol::Activity::Working),
                    _ => None,
                };
                if let Some(activity) = activity {
                    self.store.set_instance_state(
                        &observation.instance_id,
                        None,
                        Some(remuda_protocol::Knowledge::Known { value: activity }),
                    )?;
                }
            }
            if let remuda_protocol::LifecyclePayload::Entity(entity) = payload.as_ref()
                && let remuda_protocol::LifecycleEntity::Interaction(interaction) =
                    &entity.entity_value
                && interaction.state != InteractionState::Pending
            {
                let native = &interaction.meta.id;
                let mut glue = self.glue.lock().await;
                let ticket = glue.native_to_broker.get(native).cloned();
                glue.pending.remove(native);
                drop(glue);
                if let Some(ticket) = ticket {
                    self.broker.retire(&ticket).await;
                }
                self.store.put_pending_interaction(&PendingInteraction {
                    interaction_id: native.clone(),
                    instance_id: interaction.instance_id.clone(),
                    host_id: interaction.host_id.clone(),
                    kind: interaction.kind,
                    interaction: *interaction.clone(),
                })?;
            }
        }
        if let ObservationPayload::InteractionExpired(payload) = &observation.body {
            let mut glue = self.glue.lock().await;
            let ticket = glue.native_to_broker.get(&payload.interaction_id).cloned();
            if let Some(mut row) = glue.pending.remove(&payload.interaction_id) {
                row.interaction.state = InteractionState::Expired;
                self.store.put_pending_interaction(&row)?;
            }
            drop(glue);
            if let Some(ticket) = ticket {
                self.broker.retire(&ticket).await;
            }
        }
        let ObservationPayload::InteractionRequested(payload) = &observation.body else {
            return Ok(());
        };
        let interaction = payload.interaction.clone();
        let native = interaction.meta.id.clone();
        {
            let mut glue = self.glue.lock().await;
            if !glue.seen_native.insert(native.clone()) {
                return Ok(());
            }
        }
        let spec = pending_spec(&interaction);
        let ticket = self
            .broker
            .insert(spec)
            .await
            .map_err(|err| NodeError::Driver(err.to_string()))?;
        let mut glue = self.glue.lock().await;
        glue.native_to_broker.insert(native.clone(), ticket.clone());
        glue.broker_to_native.insert(ticket, native.clone());
        glue.pending.insert(
            native.clone(),
            PendingInteraction {
                interaction_id: native.clone(),
                instance_id: interaction.instance_id.clone(),
                host_id: interaction.host_id.clone(),
                kind: interaction.kind,
                interaction,
            },
        );
        let pending = glue.pending.get(&native).cloned();
        drop(glue);
        if let Some(pending) = pending {
            self.store.put_pending_interaction(&pending)?;
        }
        Ok(())
    }

    /// Pending rows, optionally filtered by instance and kind.
    pub async fn list(
        &self,
        instance_id: Option<&InstanceId>,
        kind: Option<InteractionKind>,
    ) -> Vec<PendingInteraction> {
        let glue = self.glue.lock().await;
        glue.pending
            .values()
            .filter(|row| instance_id.is_none_or(|id| row.instance_id == *id))
            .filter(|row| kind.is_none_or(|want| row.kind == want))
            .cloned()
            .collect()
    }

    /// First-answer-wins CAS. `interaction_id` is the public/driver id.
    pub async fn answer(
        &self,
        interaction_id: InteractionId,
        answer: InteractionAnswer,
        caller: AnswerCaller,
        command_id: CommandId,
    ) -> Result<Value, NodeError> {
        let ticket = {
            let glue = self.glue.lock().await;
            if let Some(row) = glue.pending.get(&interaction_id)
                && (matches!(
                    row.interaction.carrier,
                    InteractionCarrier::NativeTty | InteractionCarrier::HarnessHook
                ) || row.interaction.kind == InteractionKind::PlanReview)
            {
                // TTY/hook carriers answer a card that named its options and
                // carried an input digest, so the answer has to match them —
                // otherwise a stale or forged answer reaches the agent.
                // PlanReview is validated for EVERY carrier (D-051 (6e)):
                // print/sdk ClaudeControl is otherwise unchecked, and the
                // broker consumes the ticket before it reaches the driver, so
                // a bad plan answer would leave the child parked forever.
                remuda_driver::interaction::validate_answer(&row.interaction.request, &answer)
                    .map_err(map_broker)?;
            }
            if glue
                .pending
                .get(&interaction_id)
                .is_some_and(|row| row.interaction.carrier == InteractionCarrier::NativeTty)
                && let InteractionAnswer::Question(question) = &answer
                && question.answers.values().any(|field| {
                    field
                        .text
                        .as_ref()
                        .is_some_and(|text| text.len() > 1024 || text.chars().any(char::is_control))
                })
            {
                return Err(NodeError::InvalidRequest(
                    "PTY text reply must be a single line of at most 1024 bytes".into(),
                ));
            }
            if glue
                .pending
                .get(&interaction_id)
                .is_some_and(|row| !row.interaction.answerable)
            {
                return Err(NodeError::InvalidRequest(
                    "interaction is not answerable".into(),
                ));
            }
            glue.native_to_broker
                .get(&interaction_id)
                .cloned()
                .ok_or_else(|| NodeError::NotFound {
                    entity: "interaction",
                    id: interaction_id.as_id().to_string(),
                })?
        };
        let outcome = self
            .broker
            .answer_for(ticket, answer, caller, command_id.clone())
            .await
            .map_err(map_broker)?;
        {
            let mut glue = self.glue.lock().await;
            glue.pending.remove(&interaction_id);
        }
        let outcome = match outcome {
            remuda_driver::interaction::AnswerOutcome::Accepted => "accepted",
            remuda_driver::interaction::AnswerOutcome::Idempotent => "idempotent",
        };
        Ok(json!({
            "outcome": outcome,
            "interactionId": interaction_id.as_id().as_str(),
            "commandId": command_id.as_id().as_str(),
        }))
    }

    async fn commit_broker_observation(
        &self,
        mut observation: Observation,
    ) -> Result<(), NodeError> {
        let native = {
            let glue = self.glue.lock().await;
            match &observation.body {
                ObservationPayload::InteractionAnswered(payload) => {
                    glue.broker_to_native.get(&payload.interaction_id).cloned()
                }
                ObservationPayload::InteractionExpired(payload) => {
                    glue.broker_to_native.get(&payload.interaction_id).cloned()
                }
                _ => None,
            }
        };
        if let Some(native) = native {
            match &mut observation.body {
                ObservationPayload::InteractionAnswered(payload) => {
                    payload.interaction_id = native.clone();
                }
                ObservationPayload::InteractionExpired(payload) => {
                    payload.interaction_id = native.clone();
                }
                _ => {}
            }
            let mut glue = self.glue.lock().await;
            if let Some(mut row) = glue.pending.remove(&native) {
                row.interaction.state =
                    if matches!(observation.body, ObservationPayload::InteractionExpired(_)) {
                        InteractionState::Expired
                    } else {
                        InteractionState::AnswerCommitted
                    };
                self.store.put_pending_interaction(&row)?;
            }
        }
        let instance_id = observation.instance_id.clone();
        if self.store.get_instance(&instance_id).is_err() {
            return Ok(());
        }
        self.store
            .append_driver_observation(&instance_id, observation)?;
        Ok(())
    }
}

impl InteractionRuntime {
    /// Dispatch Hub→Node `interaction.list` / `interaction.answer`.
    pub async fn dispatch_rpc(&self, method: &str, params: Value) -> Result<Value, NodeError> {
        match method {
            "interaction.list" => {
                let instance_id = params
                    .get("instanceId")
                    .and_then(Value::as_str)
                    .map(|raw| InstanceId::try_from(raw.to_owned()))
                    .transpose()
                    .map_err(|err| NodeError::InvalidRequest(err.to_string()))?;
                let kind = params
                    .get("kind")
                    .and_then(Value::as_str)
                    .and_then(|raw| serde_json::from_value(json!(raw)).ok());
                let items = self.list(instance_id.as_ref(), kind).await;
                serde_json::to_value(json!({ "items": items })).map_err(NodeError::from)
            }
            "interaction.answer" => {
                let interaction_id = params
                    .get("interactionId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        NodeError::InvalidRequest(
                            "interaction.answer requires interactionId".into(),
                        )
                    })?;
                let interaction_id = InteractionId::try_from(interaction_id.to_owned())
                    .map_err(|err| NodeError::InvalidRequest(err.to_string()))?;
                let command_id = params
                    .get("commandId")
                    .and_then(Value::as_str)
                    .map(|raw| CommandId::try_from(raw.to_owned()))
                    .transpose()
                    .map_err(|err| NodeError::InvalidRequest(err.to_string()))?
                    .unwrap_or_else(CommandId::new);
                let by_device = params
                    .get("byDevice")
                    .and_then(Value::as_str)
                    .map(|raw| Id::try_from(raw.to_owned()))
                    .transpose()
                    .map_err(|err| NodeError::InvalidRequest(err.to_string()))?
                    .unwrap_or(Id::new("dev")?);
                // D-051 c-deleg2: the committed actor reflects the caller's
                // device truthfully. Origin comes from the same Hub-stamped
                // envelope field every other privileged frame uses
                // (`crates/remuda-node/src/origin.rs:39` wire_origin);
                // `byInstanceId` is the Agent device's bound instance (the
                // answering parent), never parsed off the answer body.
                let origin = crate::origin::wire_origin(&params);
                let by_instance = params
                    .get("byInstanceId")
                    .and_then(Value::as_str)
                    .map(|raw| InstanceId::try_from(raw.to_owned()))
                    .transpose()
                    .map_err(|err| NodeError::InvalidRequest(err.to_string()))?;
                let caller_origin = match origin {
                    remuda_protocol::InputOrigin::Human => remuda_driver::LaunchOrigin::Human,
                    remuda_protocol::InputOrigin::Bot => remuda_driver::LaunchOrigin::Bot,
                    remuda_protocol::InputOrigin::Agent => remuda_driver::LaunchOrigin::Agent,
                };
                let answer: InteractionAnswer =
                    serde_json::from_value(params.get("answer").cloned().unwrap_or(Value::Null))?;
                self.answer(
                    interaction_id,
                    answer,
                    AnswerCaller {
                        device_id: by_device,
                        origin: caller_origin,
                        instance_id: by_instance,
                    },
                    command_id,
                )
                .await
            }
            other => Err(NodeError::InvalidRequest(format!(
                "unknown interaction method {other}"
            ))),
        }
    }
}

/// Hub→Node methods owned by this module.
pub fn is_interaction_method(method: &str) -> bool {
    matches!(method, "interaction.list" | "interaction.answer")
}

/// Deterministic FakeDriver `can_use_tool` approval card.
pub(crate) fn fake_can_use_tool(instance: &Instance) -> Result<Interaction, NodeError> {
    let now = crate::store::timestamp_now()?;
    let digest = Digest::try_from(format!("sha256:{:0>64}", "b"))?;
    let interaction_id = InteractionId::new();
    Ok(Interaction {
        meta: EntityMeta {
            id: interaction_id,
            revision: U64(1),
            created_at: now.clone(),
            updated_at: now,
        },
        instance_id: instance.meta.id.clone(),
        run_id: None,
        host_id: instance.host_id.clone(),
        kind: InteractionKind::Approval,
        request_key: InteractionRequestKey {
            native: NativeRequestKey::Rpc {
                value_type: NativeRequestValueType::String,
                value: "perm-fake-bash".into(),
            },
            process_generation: U64(1),
            run_generation: Some(U64(1)),
            connection_epoch: Id::new("epoch")?,
        },
        request_version: U64(1),
        state: InteractionState::Pending,
        blocking: true,
        answerable: true,
        carrier: InteractionCarrier::ClaudeControl,
        request: InteractionRequest::Approval(Box::new(ApprovalRequest {
            title: "Bash".into(),
            description: "can_use_tool".into(),
            tool_call_id: None,
            action_ref: Id::new("obj")?,
            options: vec![
                DecisionOption {
                    id: "allow".into(),
                    label: "Allow".into(),
                    effect: DecisionEffect::AllowOnce,
                    native_value_ref: Id::new("obj")?,
                },
                DecisionOption {
                    id: "deny".into(),
                    label: "Deny".into(),
                    effect: DecisionEffect::Deny,
                    native_value_ref: Id::new("obj")?,
                },
            ],
            requested_permissions_ref: None,
            input_digest: digest,
        })),
        deadline: crate::store::unknown("none"),
        deadline_source: remuda_protocol::DeadlineSource::None,
        answer: crate::store::unknown("pending"),
        delivery: remuda_protocol::DeliveryState::NotSent,
        resolution: crate::store::unknown("pending"),
    })
}

fn pending_spec(interaction: &Interaction) -> PendingSpec {
    let request_id = match &interaction.request_key.native {
        NativeRequestKey::Rpc { value, .. } => value.clone(),
        NativeRequestKey::Hook { invocation_id } => invocation_id.as_str().to_string(),
        NativeRequestKey::None => String::new(),
    };
    PendingSpec {
        instance_id: interaction.instance_id.clone(),
        host_id: interaction.host_id.clone(),
        native_request_id: NativeRequestId {
            request_id,
            tool_use_id: None,
        },
        payload: interaction.request.clone(),
        run_generation: interaction.request_key.run_generation.unwrap_or(U64(1)),
        tool_name: match &interaction.request {
            InteractionRequest::Approval(approval) => Some(approval.title.clone()),
            InteractionRequest::Question(_) => Some("AskUserQuestion".into()),
            _ => None,
        },
    }
}

fn map_broker(err: BrokerError) -> NodeError {
    match err {
        BrokerError::NotFound => NodeError::NotFound {
            entity: "interaction",
            id: "unknown".into(),
        },
        BrokerError::Expired => NodeError::InteractionExpired,
        BrokerError::Protocol(message) => NodeError::InvalidRequest(message),
        BrokerError::Superseded { winner } => NodeError::InteractionSuperseded {
            winner: winner.as_id().to_string(),
        },
        other => NodeError::Driver(other.to_string()),
    }
}

#[cfg(test)]
mod plan_review_node_cas_tests {
    use super::*;
    use crate::MemoryStore;
    use crate::driver::FakeDriver;
    use remuda_protocol::{
        DriverKind, HostId, InstanceLifecycle, Knowledge, PlanReviewRequest, U64 as P64,
        WorkspaceId,
    };
    use std::sync::Arc;
    use tempfile::TempDir;

    async fn seeded_runtime() -> (
        TempDir,
        Arc<dyn LocalStore>,
        InstanceId,
        Arc<InteractionRuntime>,
    ) {
        let dir = tempfile::tempdir().expect("tmp");
        // open_journaled opens the dir itself.
        let store = Arc::new(MemoryStore::open_journaled(dir.path(), 128).unwrap());
        let runtime = InteractionRuntime::spawn(store.clone()).expect("runtime");
        let instance_id = InstanceId::new();
        let mut instance = crate::runtime::fixture_instance(
            instance_id.clone(),
            HostId::new(),
            WorkspaceId::new(),
            DriverKind::ClaudePrint,
        )
        .expect("fixture instance");
        instance.lifecycle = InstanceLifecycle::Ready;
        store.insert_instance(instance).expect("instance");
        runtime
            .register_driver(
                instance_id.clone(),
                Arc::new(FakeDriver::new(DriverKind::ClaudePrint)),
            )
            .await;
        (dir, store, instance_id, runtime)
    }

    fn plan_review_interaction(
        instance_id: &InstanceId,
        host_id: HostId,
        digest: &str,
    ) -> Interaction {
        let now =
            remuda_protocol::Timestamp::try_from("2026-09-23T00:00:00.000Z".to_string()).unwrap();
        Interaction {
            meta: EntityMeta {
                id: InteractionId::new(),
                revision: P64(1),
                created_at: now.clone(),
                updated_at: now,
            },
            instance_id: instance_id.clone(),
            run_id: None,
            host_id,
            kind: InteractionKind::PlanReview,
            request_key: InteractionRequestKey {
                native: NativeRequestKey::Rpc {
                    value_type: NativeRequestValueType::String,
                    value: "perm-plan".into(),
                },
                process_generation: P64(1),
                run_generation: Some(P64(1)),
                connection_epoch: Id::new("epoch").unwrap(),
            },
            request_version: P64(1),
            state: InteractionState::Pending,
            blocking: true,
            answerable: true,
            // print/sdk carrier — validated only because kind == PlanReview.
            carrier: InteractionCarrier::ClaudeControl,
            request: InteractionRequest::PlanReview(Box::new(PlanReviewRequest {
                title: "Plan review".into(),
                plan_ref: Id::new("obj").unwrap(),
                plan_revision: P64(1),
                plan_digest: Digest::try_from(digest.to_string()).unwrap(),
                options: vec![
                    DecisionOption {
                        id: "approve".into(),
                        label: "Approve".into(),
                        effect: DecisionEffect::AllowOnce,
                        native_value_ref: Id::new("obj").unwrap(),
                    },
                    DecisionOption {
                        id: "deny".into(),
                        label: "Deny".into(),
                        effect: DecisionEffect::Deny,
                        native_value_ref: Id::new("obj").unwrap(),
                    },
                ],
                allow_feedback: true,
                plan: Some("# the plan".into()),
            })),
            deadline: Knowledge::Unknown {
                reason: "none".into(),
                evidence_event_ids: vec![],
            },
            deadline_source: remuda_protocol::DeadlineSource::None,
            answer: Knowledge::Unknown {
                reason: "pending".into(),
                evidence_event_ids: vec![],
            },
            delivery: remuda_protocol::DeliveryState::NotSent,
            resolution: Knowledge::Unknown {
                reason: "pending".into(),
                evidence_event_ids: vec![],
            },
        }
    }

    fn answer_json(
        interaction_id: &str,
        option: &str,
        digest: &str,
        feedback: Option<&str>,
    ) -> Value {
        json!({
            "interactionId": interaction_id,
            "commandId": CommandId::new().as_id().as_str(),
            "origin": "human",
            "answer": {
                "kind": "plan-review",
                "optionId": option,
                "planRevision": "1",
                "planDigest": digest,
                "feedback": feedback
            }
        })
    }

    #[tokio::test]
    async fn a_bad_plan_review_answer_on_claude_control_is_rejected_and_the_ticket_stays_pending() {
        let (_dir, store, instance_id, runtime) = seeded_runtime().await;
        let good = format!("sha256:{}", "a".repeat(64));
        let bad = format!("sha256:{}", "b".repeat(64));
        let interaction = plan_review_interaction(
            &instance_id,
            store.get_instance(&instance_id).unwrap().host_id.clone(),
            &good,
        );
        let obs = store
            .append_observation(
                &instance_id,
                None,
                Completeness::Structured,
                ObservationPayload::InteractionRequested(Box::new(
                    remuda_protocol::InteractionRequestedPayload { interaction },
                )),
            )
            .expect("append");
        runtime.ingest(&obs).await.expect("ingest");
        let plan_id = runtime.list(Some(&instance_id), None).await[0]
            .interaction_id
            .clone();

        // Wrong digest is rejected before the broker consumes the ticket.
        let err = runtime
            .dispatch_rpc(
                "interaction.answer",
                answer_json(plan_id.as_id().as_str(), "approve", &bad, None),
            )
            .await
            .expect_err("bad digest must be rejected");
        assert!(matches!(err, NodeError::InvalidRequest(_)), "{err:?}");

        // The ticket survived: it is still listed and a correct answer wins.
        assert_eq!(runtime.list(Some(&instance_id), None).await.len(), 1);
        let accepted = runtime
            .dispatch_rpc(
                "interaction.answer",
                answer_json(plan_id.as_id().as_str(), "approve", &good, None),
            )
            .await
            .expect("a correct answer must still win");
        assert_eq!(accepted["outcome"], json!("accepted"));
        assert!(runtime.list(Some(&instance_id), None).await.is_empty());
    }

    #[tokio::test]
    async fn approve_with_feedback_is_rejected_for_a_plan_review() {
        let (_dir, store, instance_id, runtime) = seeded_runtime().await;
        let digest = format!("sha256:{}", "a".repeat(64));
        let interaction = plan_review_interaction(
            &instance_id,
            store.get_instance(&instance_id).unwrap().host_id.clone(),
            &digest,
        );
        let obs = store
            .append_observation(
                &instance_id,
                None,
                Completeness::Structured,
                ObservationPayload::InteractionRequested(Box::new(
                    remuda_protocol::InteractionRequestedPayload { interaction },
                )),
            )
            .expect("append");
        runtime.ingest(&obs).await.expect("ingest");
        let plan_id = runtime.list(Some(&instance_id), None).await[0]
            .interaction_id
            .clone();
        let err = runtime
            .dispatch_rpc(
                "interaction.answer",
                answer_json(plan_id.as_id().as_str(), "approve", &digest, Some("note")),
            )
            .await
            .expect_err("approve cannot carry feedback");
        assert!(matches!(err, NodeError::InvalidRequest(_)), "{err:?}");
        assert_eq!(runtime.list(Some(&instance_id), None).await.len(), 1);
    }
}
