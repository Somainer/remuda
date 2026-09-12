//! Dispatch Hub `instance.*` RPCs into [`crate::DevNode`] and stream journals.

use super::JournalSender;
pub(crate) use crate::transport::hubnode::SeqWatermark;
use crate::{CommandAction, CreateInstanceRequest, DevNode, InstanceCommandRequest, NodeError};
use remuda_protocol::hubnode::{
    HubNodeMethod, InstanceCancelParams, InstanceCreateParams, InstanceRespondParams,
    InstanceSendParams,
};
use remuda_protocol::{AgentKind, DriverKind, InstanceId, JournalEvent, U64};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

/// Wait for FakeDriver output that is persisted after the create/send RPC acks.
const DRIVER_CATCHUP: Duration = Duration::from_millis(30);

pub(crate) struct RuntimeLink {
    pub node: DevNode,
    pub journal: JournalSender,
    pub watermarks: Arc<Mutex<HashMap<String, SeqWatermark>>>,
    pub pumps: Arc<Mutex<HashSet<String>>>,
}

impl RuntimeLink {
    fn clone_link(&self) -> Self {
        Self {
            node: self.node.clone(),
            journal: self.journal.clone(),
            watermarks: self.watermarks.clone(),
            pumps: self.pumps.clone(),
        }
    }
}

pub(crate) fn dispatch_in_background(
    runtime: &RuntimeLink,
    method: String,
    params: Value,
    id: Value,
    reply: mpsc::Sender<super::HubReply>,
) {
    let runtime = runtime.clone_link();
    tokio::spawn(async move {
        let result = dispatch_hub(&runtime, &method, params).await;
        if !id.is_null() {
            let _ = reply.send(super::HubReply { id, result }).await;
        }
    });
}

pub(crate) fn resume_runtime_journals(runtime: &RuntimeLink) {
    let runtime = runtime.clone_link();
    tokio::spawn(async move {
        if let Ok(page) = runtime.node.list_instances() {
            for instance in page.items {
                ensure_pump(&runtime, &instance.meta.id);
                let _ = flush_journal(&runtime, &instance.meta.id).await;
            }
        }
    });
}

async fn dispatch_hub(
    runtime: &RuntimeLink,
    method: &str,
    params: Value,
) -> Result<Value, NodeError> {
    if crate::interactions::is_interaction_method(method) {
        let result = runtime.node.dispatch_interaction(method, params).await?;
        if let Ok(page) = runtime.node.list_instances() {
            for instance in page.items {
                let _ = catch_up(runtime, &instance.meta.id).await;
            }
        }
        return Ok(result);
    }
    match HubNodeMethod::parse(method) {
        Some(HubNodeMethod::InstanceCreate) => {
            let created = runtime
                .node
                .create_instance(create_from_params(&runtime.node, &params)?)
                .await?;
            catch_up(runtime, &created.instance.meta.id).await?;
            Ok(serde_json::to_value(&created)?)
        }
        Some(HubNodeMethod::InstanceSend) => {
            let (instance_id, result) = send_from_params(&runtime.node, params).await?;
            catch_up(runtime, &instance_id).await?;
            Ok(result)
        }
        Some(HubNodeMethod::InstanceCancel) => {
            let (instance_id, result) = cancel_from_params(&runtime.node, params).await?;
            catch_up(runtime, &instance_id).await?;
            Ok(result)
        }
        Some(HubNodeMethod::InstanceRespond | HubNodeMethod::InteractionRespond) => {
            let (instance_id, result) = respond_from_params(&runtime.node, params).await?;
            catch_up(runtime, &instance_id).await?;
            Ok(result)
        }
        _ => Ok(json!({ "ok": true })),
    }
}

async fn catch_up(runtime: &RuntimeLink, instance_id: &InstanceId) -> Result<(), NodeError> {
    ensure_pump(runtime, instance_id);
    flush_journal(runtime, instance_id).await?;
    tokio::time::sleep(DRIVER_CATCHUP).await;
    flush_journal(runtime, instance_id).await
}

fn create_from_params(node: &DevNode, params: &Value) -> Result<CreateInstanceRequest, NodeError> {
    let parsed: InstanceCreateParams =
        serde_json::from_value(params.clone()).or_else(|_| serde_json::from_value(json!({})))?;
    let spec = parsed.spec.as_ref().unwrap_or(params);
    let instance_id = parsed
        .instance_id
        .as_deref()
        .or_else(|| params.get("instanceId").and_then(Value::as_str))
        .or_else(|| spec.get("instanceId").and_then(Value::as_str))
        .map(|raw: &str| InstanceId::try_from(raw.to_owned()))
        .transpose()
        .map_err(|err| NodeError::InvalidRequest(err.to_string()))?;
    let kind = parsed
        .kind
        .as_deref()
        .and_then(|raw| serde_json::from_value(json!(raw)).ok())
        .or_else(|| {
            spec.get("kind")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
        })
        .unwrap_or(AgentKind::Claude);
    let driver = parsed
        .driver
        .as_deref()
        .and_then(|raw| serde_json::from_value(json!(raw)).ok())
        .or_else(|| {
            spec.get("driver")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
        })
        .unwrap_or(DriverKind::ClaudePrint);
    Ok(CreateInstanceRequest {
        instance_id,
        host_id: Some(node.host().meta.id.clone()),
        workspace_id: None,
        kind,
        driver,
        model: "fake".to_owned(),
        provider_profile_id: "dev-fake".to_owned(),
        permission_mode: "dontAsk".to_owned(),
        prompt: parsed
            .prompt
            .clone()
            .or_else(|| prompt_of(params))
            .unwrap_or_default(),
    })
}

async fn send_from_params(node: &DevNode, params: Value) -> Result<(InstanceId, Value), NodeError> {
    let parsed: Option<InstanceSendParams> = serde_json::from_value(params.clone()).ok();
    let instance_id = match parsed.as_ref() {
        Some(parsed) => InstanceId::try_from(parsed.instance_id.clone())
            .map_err(|err| NodeError::InvalidRequest(err.to_string()))?,
        None => instance_id_of(&params)?,
    };
    let prompt = parsed
        .as_ref()
        .and_then(InstanceSendParams::prompt_text)
        .map(str::to_owned)
        .or_else(|| prompt_of(&params))
        .unwrap_or_default();
    let command_id = parsed
        .as_ref()
        .and_then(|parsed| parsed.command_id.as_deref())
        .and_then(|raw: &str| remuda_protocol::CommandId::try_from(raw.to_owned()).ok())
        .or_else(|| command_id_of(&params));
    let run_id = parsed
        .as_ref()
        .and_then(|parsed| parsed.run_id.as_deref())
        .and_then(|raw: &str| remuda_protocol::RunId::try_from(raw.to_owned()).ok());
    let result = node
        .submit_command(
            &instance_id,
            InstanceCommandRequest {
                command_id,
                operation: CommandAction::Send,
                prompt: Some(prompt),
                run_id,
                interaction_id: None,
                answer: None,
            },
        )
        .await?;
    Ok((instance_id, serde_json::to_value(&result)?))
}

async fn cancel_from_params(
    node: &DevNode,
    params: Value,
) -> Result<(InstanceId, Value), NodeError> {
    let parsed: Option<InstanceCancelParams> = serde_json::from_value(params.clone()).ok();
    let instance_id = match parsed.as_ref() {
        Some(parsed) => InstanceId::try_from(parsed.instance_id.clone())
            .map_err(|err| NodeError::InvalidRequest(err.to_string()))?,
        None => instance_id_of(&params)?,
    };
    let command_id = parsed
        .as_ref()
        .and_then(|parsed| parsed.command_id.as_deref())
        .and_then(|raw: &str| remuda_protocol::CommandId::try_from(raw.to_owned()).ok())
        .or_else(|| command_id_of(&params));
    let run_id = parsed
        .as_ref()
        .and_then(|parsed| parsed.run_id.as_deref())
        .and_then(|raw: &str| remuda_protocol::RunId::try_from(raw.to_owned()).ok());
    let result = node
        .submit_command(
            &instance_id,
            InstanceCommandRequest {
                command_id,
                operation: CommandAction::Cancel,
                prompt: None,
                run_id,
                interaction_id: None,
                answer: None,
            },
        )
        .await?;
    Ok((instance_id, serde_json::to_value(&result)?))
}

async fn respond_from_params(
    node: &DevNode,
    params: Value,
) -> Result<(InstanceId, Value), NodeError> {
    let parsed: Option<InstanceRespondParams> = serde_json::from_value(params.clone()).ok();
    let instance_id = parsed
        .as_ref()
        .and_then(|parsed| parsed.instance_id.as_deref())
        .or_else(|| params.get("instanceId").and_then(Value::as_str))
        .ok_or_else(|| NodeError::InvalidRequest("instanceId required".into()))?;
    let instance_id = InstanceId::try_from(instance_id.to_owned())
        .map_err(|err| NodeError::InvalidRequest(err.to_string()))?;
    let interaction_id = parsed
        .as_ref()
        .and_then(|parsed| parsed.interaction_id.as_deref())
        .or_else(|| params.get("interactionId").and_then(Value::as_str))
        .and_then(|raw: &str| remuda_protocol::InteractionId::try_from(raw.to_owned()).ok());
    let command_id = parsed
        .as_ref()
        .and_then(|parsed| parsed.command_id.as_deref())
        .and_then(|raw: &str| remuda_protocol::CommandId::try_from(raw.to_owned()).ok())
        .or_else(|| command_id_of(&params));
    let answer = parsed
        .as_ref()
        .and_then(|parsed| parsed.answer.clone())
        .or_else(|| params.get("answer").cloned())
        .unwrap_or(Value::Null);
    let result = node
        .submit_command(
            &instance_id,
            InstanceCommandRequest {
                command_id,
                operation: CommandAction::RespondInteraction,
                prompt: None,
                run_id: None,
                interaction_id,
                answer: Some(answer),
            },
        )
        .await?;
    Ok((instance_id, serde_json::to_value(&result)?))
}

fn instance_id_of(params: &Value) -> Result<InstanceId, NodeError> {
    let raw = params
        .get("instanceId")
        .or_else(|| params.pointer("/spec/instanceId"))
        .and_then(Value::as_str)
        .ok_or_else(|| NodeError::InvalidRequest("instanceId required".into()))?;
    InstanceId::try_from(raw.to_owned()).map_err(|err| NodeError::InvalidRequest(err.to_string()))
}

fn command_id_of(params: &Value) -> Option<remuda_protocol::CommandId> {
    params
        .get("commandId")
        .and_then(Value::as_str)
        .and_then(|raw| remuda_protocol::CommandId::try_from(raw.to_owned()).ok())
}

fn prompt_of(params: &Value) -> Option<String> {
    params
        .pointer("/initialInput/text")
        .or_else(|| params.get("prompt"))
        .or_else(|| params.pointer("/spec/prompt"))
        .or_else(|| params.pointer("/input/text"))
        .or_else(|| params.get("text"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn ensure_pump(runtime: &RuntimeLink, instance_id: &InstanceId) {
    let key = instance_id.as_id().as_str().to_owned();
    {
        let mut pumps = lock_set(&runtime.pumps);
        if !pumps.insert(key.clone()) {
            return;
        }
    }
    let Ok(rx) = runtime.node.subscribe(instance_id) else {
        lock_set(&runtime.pumps).remove(&key);
        return;
    };
    let runtime = runtime.clone_link();
    let instance_id = instance_id.clone();
    tokio::spawn(async move {
        let _ = flush_journal(&runtime, &instance_id).await;
        pump_live(&runtime, instance_id, rx).await;
    });
}

async fn flush_journal(runtime: &RuntimeLink, instance_id: &InstanceId) -> Result<(), NodeError> {
    let instance = runtime.node.get_instance(instance_id)?;
    let after = current_watermark(&runtime.watermarks, instance_id.as_id().as_str());
    let after_seq = (after > 0).then_some(U64(u64::try_from(after).unwrap_or(0)));
    let page = runtime
        .node
        .read_journal(&instance.journal_id, after_seq, 256)?;
    for event in page.events {
        forward_event(runtime, instance_id, &event).await?;
    }
    Ok(())
}

async fn pump_live(
    runtime: &RuntimeLink,
    instance_id: InstanceId,
    mut rx: broadcast::Receiver<JournalEvent>,
) {
    loop {
        match rx.recv().await {
            Ok(event) => {
                if let Err(error) = forward_event(runtime, &instance_id, &event).await {
                    tracing::debug!(%error, "journal pump forward failed");
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                let _ = flush_journal(runtime, &instance_id).await;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

async fn forward_event(
    runtime: &RuntimeLink,
    instance_id: &InstanceId,
    event: &JournalEvent,
) -> Result<(), NodeError> {
    let seq = i64::try_from(event.position().1.0).unwrap_or(0);
    let key = instance_id.as_id().as_str();
    if seq <= current_watermark(&runtime.watermarks, key) {
        return Ok(());
    }
    let value = serde_json::to_value(event)?;
    let result = runtime.journal.append_seq(key.to_owned(), seq, value).await;
    match result {
        Ok(ack) => {
            let acked = ack
                .get("durableSeq")
                .or_else(|| ack.get("seq"))
                .and_then(super::value_i64)
                .unwrap_or(seq);
            record_watermark(
                &runtime.watermarks,
                key,
                Some(event.position().0.to_string()),
                acked,
            );
            Ok(())
        }
        Err(error) if super::already_durable(&error, seq) => {
            record_watermark(
                &runtime.watermarks,
                key,
                Some(event.position().0.to_string()),
                seq,
            );
            Ok(())
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn record_watermark(
    watermarks: &Arc<Mutex<HashMap<String, SeqWatermark>>>,
    instance_id: &str,
    journal_id: Option<String>,
    seq: i64,
) {
    let mut guard = match watermarks.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let entry = guard
        .entry(instance_id.to_owned())
        .or_insert_with(|| SeqWatermark {
            instance_id: instance_id.to_owned(),
            journal_id: journal_id.clone(),
            seq: 0,
        });
    if entry.seq < seq {
        entry.seq = seq;
    }
    if entry.journal_id.is_none() {
        entry.journal_id = journal_id;
    }
}

pub(crate) fn snapshot_watermarks(
    watermarks: &Arc<Mutex<HashMap<String, SeqWatermark>>>,
) -> HashMap<String, SeqWatermark> {
    match watermarks.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

fn current_watermark(
    watermarks: &Arc<Mutex<HashMap<String, SeqWatermark>>>,
    instance_id: &str,
) -> i64 {
    snapshot_watermarks(watermarks)
        .get(instance_id)
        .map(|mark| mark.seq)
        .unwrap_or(0)
}

fn lock_set(set: &Arc<Mutex<HashSet<String>>>) -> std::sync::MutexGuard<'_, HashSet<String>> {
    match set.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
