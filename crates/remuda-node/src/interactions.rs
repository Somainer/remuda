//! InteractionBroker glue: pending table, driver forwarding, and Hub RPC.

use crate::{Driver, DriverRequest, LocalStore, NodeError};
use async_trait::async_trait;
use remuda_driver::interaction::{
    BrokerConfig, BrokerError, InstancePolicy, InteractionBroker, InteractionOwner,
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
        });
        let pump = Arc::clone(&runtime);
        tokio::spawn(async move {
            while let Some(observation) = broker_rx.recv().await {
                if let Err(error) = pump.commit_broker_observation(observation).await {
                    tracing::debug!(%error, "interaction broker observation commit failed");
                }
            }
        });
        broker.spawn_sweeper(SWEEP_INTERVAL);
        Ok(runtime)
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
        by_device: Id,
        command_id: CommandId,
    ) -> Result<Value, NodeError> {
        let ticket = {
            let glue = self.glue.lock().await;
            if let Some(row) = glue.pending.get(&interaction_id)
                && matches!(
                    row.interaction.carrier,
                    InteractionCarrier::NativeTty | InteractionCarrier::HarnessHook
                )
            {
                // Both carriers answer a card that named its options and
                // carried an input digest, so the answer has to match them —
                // otherwise a stale or forged answer reaches the agent.
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
            .answer(ticket, answer, by_device, command_id.clone())
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
                let answer: InteractionAnswer =
                    serde_json::from_value(params.get("answer").cloned().unwrap_or(Value::Null))?;
                self.answer(interaction_id, answer, by_device, command_id)
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
