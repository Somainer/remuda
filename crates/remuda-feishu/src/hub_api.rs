//! [`InstanceApi`] over [`remuda_hub_client::HubClient`].

use std::future::Future;

use remuda_hub_client::{HubClient, InstanceCreate, pick_host};
use serde_json::{Value, json};

use crate::dispatcher::{
    CreateRequest, CreatedInstance, FollowEvent, FollowPage, InstanceApi, RespondRequest,
    SendRequest,
};
use crate::error::Error;
use remuda_protocol::{AgentKind, InstanceId, Interaction};

/// Hub-backed [`InstanceApi`] (create/send/cancel/respond/follow).
pub struct HubInstanceApi {
    client: HubClient,
}

impl HubInstanceApi {
    /// Wrap an authenticated (or bootstrap) Hub client.
    #[must_use]
    pub fn new(client: HubClient) -> Self {
        Self { client }
    }

    /// Inner HTTP client.
    #[must_use]
    pub fn client(&self) -> &HubClient {
        &self.client
    }
}

impl InstanceApi for HubInstanceApi {
    fn create(
        &self,
        req: CreateRequest,
    ) -> impl Future<Output = Result<CreatedInstance, Error>> + Send {
        let client = &self.client;
        async move {
            let mut body = InstanceCreate {
                kind: Some(agent_wire(req.agent).to_string()),
                driver: Some(driver_for(req.agent).to_string()),
                prompt: Some(req.prompt.clone()),
                title: Some(req.session_key.as_str().to_string()),
                model: req.model.clone(),
                ..InstanceCreate::default()
            };
            if req.host.starts_with("hst_") {
                body.host_id = Some(req.host.clone());
                body.placement = Some(json!({ "host": req.host }));
            } else if !req.host.is_empty() {
                let labels = vec![req.host.clone()];
                body.placement = Some(json!({ "labels": labels }));
                if let Ok(hosts) = client.list_hosts().await
                    && let Ok(host_id) = pick_host(&hosts, &labels)
                {
                    body.host_id = Some(host_id);
                }
            } else {
                body.placement = Some(json!({ "kind": "any" }));
                if let Ok(hosts) = client.list_hosts().await
                    && let Ok(host_id) = pick_host(&hosts, &[])
                {
                    body.host_id = Some(host_id);
                }
            }
            let created = client.create_instance_typed(&body).await.map_err(hub_err)?;
            let instance_id = InstanceId::try_from(created.instance.instance_id)
                .map_err(|err| Error::InstanceApi(err.to_string()))?;
            Ok(CreatedInstance { instance_id })
        }
    }

    fn send(&self, req: SendRequest) -> impl Future<Output = Result<(), Error>> + Send {
        let client = &self.client;
        async move {
            let id = req.instance_id.as_id().as_str();
            let payload = json!({
                "instanceId": id,
                "input": {
                    "type": "prompt",
                    "mode": "new-turn",
                    "blocks": [{ "type": "text", "text": req.prompt }],
                    "origin": "bot",
                },
                "completionScope": "native-turn",
            });
            client
                .post_command(id, "instance.send", payload, Some(&req.idempotency_key))
                .await
                .map_err(hub_err)?;
            Ok(())
        }
    }

    fn cancel(&self, instance_id: &InstanceId) -> impl Future<Output = Result<(), Error>> + Send {
        let client = &self.client;
        let id = instance_id.as_id().as_str().to_string();
        async move {
            let payload = json!({ "instanceId": id });
            client
                .post_command(&id, "instance.cancel", payload, None)
                .await
                .map_err(hub_err)?;
            Ok(())
        }
    }

    fn respond(&self, req: RespondRequest) -> impl Future<Output = Result<(), Error>> + Send {
        let client = &self.client;
        async move {
            let id = req.instance_id.as_id().as_str();
            let payload = json!({
                "instanceId": id,
                "interactionId": req.interaction_id.as_id().as_str(),
                "requestVersion": req.request_version,
                "processGeneration": req.process_generation,
                "answer": req.answer,
            });
            client
                .post(
                    &format!("/v1/interactions/{}/answer", req.interaction_id.as_id()),
                    &payload,
                )
                .await
                .map_err(hub_err)?;
            Ok(())
        }
    }

    fn follow(
        &self,
        instance_id: &InstanceId,
        after_seq: u64,
    ) -> impl Future<Output = Result<FollowPage, Error>> + Send {
        let client = &self.client;
        let id = instance_id.as_id().as_str().to_string();
        async move {
            let page = client
                .get_journal_typed(&id, Some(&after_seq.to_string()))
                .await
                .map_err(hub_err)?;
            let next_seq = page.durable_seq.parse::<u64>().unwrap_or(after_seq);
            let events = page.events.iter().filter_map(map_journal_event).collect();
            Ok(FollowPage { events, next_seq })
        }
    }
}

fn hub_err(err: remuda_hub_client::ClientError) -> Error {
    Error::InstanceApi(err.to_string())
}

fn agent_wire(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Claude => "claude",
        AgentKind::Codex => "codex",
        AgentKind::Grok => "grok",
        AgentKind::Agy => "agy",
        AgentKind::Generic => "generic",
        AgentKind::Terminal => "terminal",
    }
}

fn driver_for(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Claude => "claude-print",
        AgentKind::Codex => "codex-appserver",
        AgentKind::Grok => "grok-acp",
        AgentKind::Agy => "agy-print",
        AgentKind::Generic => "generic-pty",
        AgentKind::Terminal => "shell-pty",
    }
}

fn map_journal_event(record: &Value) -> Option<FollowEvent> {
    let event = record.get("event").unwrap_or(record);
    let kind = event
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let payload = event.get("payload").cloned().unwrap_or(Value::Null);
    match kind {
        "tool" | "tool_use" | "tool-boundary" => Some(FollowEvent::ToolBoundary {
            name: payload
                .get("name")
                .or_else(|| payload.get("tool"))
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string(),
            summary: payload
                .get("summary")
                .or_else(|| payload.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            elapsed_secs: payload
                .get("elapsedSecs")
                .or_else(|| payload.get("elapsed_secs"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
        }),
        "interaction" | "interaction.requested" => {
            let value = payload.get("interaction").cloned().unwrap_or(payload);
            serde_json::from_value::<Interaction>(value)
                .ok()
                .map(|interaction| FollowEvent::Interaction(Box::new(interaction)))
        }
        "result" | "completion" => Some(FollowEvent::Completed {
            conclusion: payload
                .get("text")
                .or_else(|| payload.get("conclusion"))
                .and_then(Value::as_str)
                .unwrap_or("done")
                .to_string(),
            ok: payload.get("ok").and_then(Value::as_bool).unwrap_or(true),
        }),
        _ => {
            let text = payload
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if text.is_empty() {
                None
            } else {
                Some(FollowEvent::Output { text })
            }
        }
    }
}
