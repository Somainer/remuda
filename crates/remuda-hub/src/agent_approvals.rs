//! One-shot exact-action human approval using the shared InteractionBroker.

use crate::HubError;
use async_trait::async_trait;
use axum::http::HeaderMap;
use remuda_driver::interaction::{
    BrokerConfig, BrokerError, InteractionBroker, InteractionOwner, NativeRequestId, PendingSpec,
};
use remuda_protocol::{
    ApprovalRequest, CommandId, DecisionEffect, DecisionOption, Digest, HostId, Id, InstanceId,
    InteractionAnswer, InteractionId, InteractionRequest, U64,
};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

/// Authenticated caller and its immutable owner context.
#[derive(Clone, PartialEq, Eq)]
pub struct ApprovalCaller {
    pub device_id: String,
    pub instance_id: InstanceId,
    pub host_id: HostId,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GrantState {
    Pending,
    Allowed,
    Denied,
    Consumed,
}

struct Grant {
    caller: ApprovalCaller,
    request: Value,
    digest: Digest,
    view: Value,
    state: GrantState,
    expires: Instant,
}

struct ApprovalOwner {
    grants: Arc<Mutex<HashMap<InteractionId, Grant>>>,
}

#[async_trait]
impl InteractionOwner for ApprovalOwner {
    async fn apply_answer(
        &self,
        id: InteractionId,
        answer: InteractionAnswer,
    ) -> Result<(), BrokerError> {
        let mut grants = self.grants.lock().await;
        let grant = grants.get_mut(&id).ok_or(BrokerError::NotFound)?;
        grant.state = if matches!(answer, InteractionAnswer::Approval(ref a) if a.option_id == "allow-once" && a.input_digest == grant.digest)
        {
            GrantState::Allowed
        } else {
            GrantState::Denied
        };
        Ok(())
    }
    async fn deny_or_cancel(&self, id: InteractionId) -> Result<(), BrokerError> {
        if let Some(grant) = self.grants.lock().await.get_mut(&id) {
            grant.state = GrantState::Denied;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct AgentApprovals {
    broker: Arc<InteractionBroker>,
    grants: Arc<Mutex<HashMap<InteractionId, Grant>>>,
}

impl AgentApprovals {
    pub fn new() -> Result<Self, HubError> {
        let (broker, mut observations) =
            InteractionBroker::new(BrokerConfig::default()).map_err(broker_error)?;
        // This broker's observations do not belong to a Node journal. The pending
        // tickets are exposed in the same /v1/interactions UI and expire closed.
        tokio::spawn(async move { while observations.recv().await.is_some() {} });
        Ok(Self {
            broker,
            grants: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn require(
        &self,
        headers: &HeaderMap,
        caller: &ApprovalCaller,
        request: Value,
    ) -> Result<(), HubError> {
        self.broker.sweep_expired().await;
        let mut grants = self.grants.lock().await;
        grants.retain(|_, grant| grant.expires > Instant::now());
        if let Some(raw) = headers.get("x-remuda-approval-id") {
            let id: InteractionId = raw
                .to_str()
                .map_err(|_| HubError::Forbidden)?
                .parse()
                .map_err(|_| HubError::Forbidden)?;
            let grant = grants.get_mut(&id).ok_or(HubError::Forbidden)?;
            if grant.caller != *caller
                || grant.request != request
                || grant.state != GrantState::Allowed
            {
                return Err(HubError::Forbidden);
            }
            grant.state = GrantState::Consumed;
            return Ok(());
        }
        if let Some((id, _)) = grants.iter().find(|(_, grant)| {
            grant.caller == *caller
                && grant.request == request
                && grant.state == GrantState::Pending
        }) {
            return Err(HubError::ApprovalRequired {
                interaction_id: id.as_id().as_str().to_string(),
            });
        }
        if grants.len() >= 1024 {
            return Err(HubError::BadRequest(
                "too many pending agent approvals".into(),
            ));
        }
        let instance_id = caller.instance_id.clone();
        let host_id = caller.host_id.clone();
        let digest: Digest = format!(
            "sha256:{:x}",
            Sha256::digest(
                serde_json::to_vec(&request).map_err(|e| HubError::Internal(e.to_string()))?
            )
        )
        .try_into()
        .map_err(|e: remuda_protocol::WireValueError| HubError::Internal(e.to_string()))?;
        let approval = InteractionRequest::Approval(Box::new(ApprovalRequest {
            title: "Agent control requires human approval".into(),
            description: request.to_string(),
            tool_call_id: None,
            action_ref: new_id("obj")?,
            requested_permissions_ref: None,
            input_digest: digest.clone(),
            options: vec![
                DecisionOption {
                    id: "allow-once".into(),
                    label: "Allow once".into(),
                    effect: DecisionEffect::AllowOnce,
                    native_value_ref: new_id("obj")?,
                },
                DecisionOption {
                    id: "deny".into(),
                    label: "Deny".into(),
                    effect: DecisionEffect::Deny,
                    native_value_ref: new_id("obj")?,
                },
            ],
        }));
        self.broker
            .register_owner(
                instance_id.clone(),
                Arc::new(ApprovalOwner {
                    grants: self.grants.clone(),
                }),
            )
            .await;
        let id = self
            .broker
            .insert(PendingSpec {
                instance_id: instance_id.clone(),
                host_id: host_id.clone(),
                native_request_id: NativeRequestId {
                    request_id: uuid::Uuid::new_v4().to_string(),
                    tool_use_id: None,
                },
                payload: approval.clone(),
                run_generation: U64(1),
                tool_name: None,
            })
            .await
            .map_err(broker_error)?;
        let now = crate::config::now_rfc3339();
        let interaction = json!({"id": id, "revision": "1", "createdAt": now, "updatedAt": now,
            "requestKey":{"native":{"type":"rpc", "valueType":"string", "value":id.as_id().as_str()},"processGeneration":"1", "runGeneration":"1", "connectionEpoch":new_id("epoch")?},
            "instanceId": instance_id, "hostId": host_id, "runId": null, "kind": "approval", "state": "pending",
            "blocking": true, "answerable": true, "carrier": "unsupported", "requestVersion": "1", "request": approval,
            "deadline": {"state":"unknown", "reason":"broker-ttl", "evidenceEventIds":[]}, "deadlineSource": "none",
            "answer": {"state":"unknown", "reason":"pending", "evidenceEventIds":[]}, "delivery": "not-sent", "resolution": {"state":"unknown", "reason":"pending", "evidenceEventIds":[]}});
        let mut view = interaction.clone();
        view["interactionId"] = json!(id);
        view["interaction"] = interaction;
        grants.insert(
            id.clone(),
            Grant {
                caller: caller.clone(),
                request,
                digest,
                view,
                state: GrantState::Pending,
                expires: Instant::now() + Duration::from_secs(900),
            },
        );
        Err(HubError::ApprovalRequired {
            interaction_id: id.as_id().as_str().to_string(),
        })
    }

    pub async fn list(&self) -> Vec<Value> {
        self.broker.sweep_expired().await;
        self.grants
            .lock()
            .await
            .values()
            .filter(|g| g.state == GrantState::Pending && g.expires > Instant::now())
            .map(|g| g.view.clone())
            .collect()
    }

    pub async fn answer(
        &self,
        id: &InteractionId,
        answer: InteractionAnswer,
        by_device: Id,
        command: CommandId,
    ) -> Result<Option<Value>, HubError> {
        {
            let grants = self.grants.lock().await;
            let Some(grant) = grants.get(id) else {
                return Ok(None);
            };
            if grant.expires <= Instant::now() {
                return Err(HubError::Expired);
            }
            if !matches!(&answer, InteractionAnswer::Approval(a) if (a.option_id == "allow-once" || a.option_id == "deny") && a.input_digest == grant.digest)
            {
                return Err(HubError::BadRequest(
                    "approval must match this action's inputDigest and allow-once/deny option"
                        .into(),
                ));
            }
        }
        self.broker
            .answer(id.clone(), answer, by_device, command)
            .await
            .map_err(broker_error)?;
        Ok(Some(json!({"interactionId": id, "state": "answered"})))
    }
}

fn new_id(prefix: &str) -> Result<Id, HubError> {
    Id::new(prefix).map_err(|e| HubError::Internal(e.to_string()))
}
fn broker_error(error: BrokerError) -> HubError {
    match error {
        BrokerError::NotFound => HubError::NotFound,
        BrokerError::Expired | BrokerError::StaleGeneration => HubError::Expired,
        BrokerError::Superseded { winner } => HubError::Superseded {
            winner: winner.as_id().as_str().to_string(),
        },
        other => HubError::Internal(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn approval_binds_caller_and_exact_action_and_is_consumed_once() {
        let approvals = AgentApprovals::new().unwrap();
        let caller = ApprovalCaller {
            device_id: "caller-device".into(),
            instance_id: InstanceId::new(),
            host_id: HostId::new(),
        };
        let request = json!({"operation":"instance.send", "target":"sibling", "text":"hello"});
        let HubError::ApprovalRequired { interaction_id } = approvals
            .require(&HeaderMap::new(), &caller, request.clone())
            .await
            .unwrap_err()
        else {
            panic!("unapproved action must open an Interaction")
        };
        let pending = approvals.list().await;
        let id: InteractionId = interaction_id.parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-remuda-approval-id", id.as_id().as_str().parse().unwrap());
        assert!(
            approvals
                .require(&headers, &caller, request.clone())
                .await
                .is_err()
        );
        let answer: InteractionAnswer = serde_json::from_value(json!({
            "kind":"approval", "optionId":"allow-once",
            "inputDigest":pending[0]["request"]["inputDigest"]
        }))
        .unwrap();
        approvals
            .answer(
                &id,
                answer.clone(),
                Id::new("dev").unwrap(),
                CommandId::new(),
            )
            .await
            .unwrap();
        assert!(matches!(
            approvals
                .answer(&id, answer, Id::new("dev").unwrap(), CommandId::new())
                .await,
            Err(HubError::Superseded { .. })
        ));
        let mut other = caller.clone();
        other.instance_id = InstanceId::new();
        assert!(
            approvals
                .require(&headers, &other, request.clone())
                .await
                .is_err()
        );
        other = caller.clone();
        other.device_id = "another-device".into();
        assert!(
            approvals
                .require(&headers, &other, request.clone())
                .await
                .is_err()
        );
        let mut changed = request.clone();
        changed["text"] = json!("different input");
        assert!(approvals.require(&headers, &caller, changed).await.is_err());
        approvals
            .require(&headers, &caller, request.clone())
            .await
            .unwrap();
        assert!(approvals.require(&headers, &caller, request).await.is_err());
        assert!(approvals.list().await.is_empty());
    }
}
