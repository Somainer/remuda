//! Dispatch Hub `instance.*` RPCs into [`crate::DevNode`] and stream journals.

use super::JournalSender;
pub(crate) use crate::transport::hubnode::SeqWatermark;
use crate::{CommandAction, CreateInstanceRequest, DevNode, InstanceCommandRequest, NodeError};
use remuda_protocol::hubnode::{
    HubNodeMethod, InstanceCancelParams, InstanceCreateParams, InstanceRespondParams,
    InstanceSendParams, TtyWriteParams,
};
use remuda_protocol::{AgentKind, DriverKind, InstanceId, JournalEvent, U64};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};

pub(crate) struct RuntimeLink {
    pub controller: Option<super::RuntimeController>,
    pub node: DevNode,
    pub hub_url: String,
    pub journal: JournalSender,
    pub watermarks: Arc<Mutex<HashMap<String, SeqWatermark>>>,
    pub pumps: Arc<Mutex<HashSet<String>>>,
}

impl RuntimeLink {
    fn clone_link(&self) -> Self {
        Self {
            controller: self.controller.clone(),
            node: self.node.clone(),
            hub_url: self.hub_url.clone(),
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

pub(crate) fn apply_resume_watermarks(runtime: &RuntimeLink, hello: &Value) {
    if hello.get("instanceWatermarks").is_some() || hello.get("resumeCursors").is_some() {
        match runtime.watermarks.lock() {
            Ok(mut marks) => marks.clear(),
            Err(poisoned) => poisoned.into_inner().clear(),
        }
    }
    for entry in ["instanceWatermarks", "resumeCursors"]
        .into_iter()
        .filter_map(|key| hello.get(key).and_then(Value::as_array))
        .flatten()
    {
        let instance_id = entry
            .get("instanceId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                let journal = entry.get("journalId").and_then(Value::as_str)?;
                let journal = journal.parse().ok()?;
                runtime
                    .node
                    .instance_for_journal(&journal)
                    .ok()
                    .map(|id| id.as_id().to_string())
            });
        let seq = entry
            .get("durableSeq")
            .or_else(|| entry.get("afterSeq"))
            .or_else(|| entry.get("seq"))
            .and_then(super::value_i64);
        if let (Some(id), Some(seq)) = (instance_id, seq) {
            record_watermark(
                &runtime.watermarks,
                &id,
                entry
                    .get("journalId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                seq,
            );
        }
    }
}

async fn dispatch_hub(
    runtime: &RuntimeLink,
    method: &str,
    params: Value,
) -> Result<Value, NodeError> {
    #[cfg(unix)]
    let _controller = match &runtime.controller {
        Some(controller) => Some(controller.dispatch_guard().await?),
        None => None,
    };
    if method == "host.doctor" {
        return runtime.node.doctor().await;
    }
    if crate::workspace::is_workspace_method(method) {
        return runtime.node.workspace_rpc(method, params);
    }
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
            let mut request = create_from_params(&runtime.node, &params)?;
            if let Some(credential) = &mut request.agent_credential {
                credential.hub = Some(runtime.hub_url.clone());
            }
            let created = runtime.node.create_instance(request).await?;
            // The durable accept is the reply. Journal mirroring continues
            // through the per-instance pump the accept started; awaiting it
            // here would put a Hub round-trip on the accept's critical path —
            // exactly the serialization a loaded link tripped over.
            ensure_pump(runtime, &created.instance.meta.id);
            Ok(serde_json::to_value(&created)?)
        }
        // D-026: resume takes the same path as create, with the session to
        // continue. Without this arm it fell to the catch-all below, which
        // answered {ok:true} while creating nothing.
        Some(HubNodeMethod::InstanceResume) => {
            let mut request = create_from_params(&runtime.node, &params)?;
            let spec = params.get("spec").unwrap_or(&params);
            let session_id = params
                .get("resumeSessionId")
                .or_else(|| spec.get("resumeSessionId"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    NodeError::InvalidRequest("instance.resume requires resumeSessionId".into())
                })?;
            request.resume_session_id = Some(session_id.to_owned());
            if request.resumed_from.is_none() {
                request.resumed_from = params
                    .get("resumedFrom")
                    .or_else(|| spec.get("resumedFrom"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(|value| InstanceId::try_from(value.to_owned()))
                    .transpose()
                    .map_err(|err| NodeError::InvalidRequest(err.to_string()))?;
            }
            if let Some(credential) = &mut request.agent_credential {
                credential.hub = Some(runtime.hub_url.clone());
            }
            let created = runtime.node.create_instance(request).await?;
            ensure_pump(runtime, &created.instance.meta.id);
            Ok(serde_json::to_value(&created)?)
        }
        Some(HubNodeMethod::InstanceSend) => {
            let (instance_id, result) = send_from_params(&runtime.node, params).await?;
            ensure_pump(runtime, &instance_id);
            Ok(result)
        }
        Some(HubNodeMethod::InstanceConfigure) => {
            let instance_id = params
                .get("instanceId")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    NodeError::InvalidRequest("instance.configure requires instanceId".into())
                })?;
            let instance_id = InstanceId::try_from(instance_id.to_owned())
                .map_err(|err| NodeError::InvalidRequest(err.to_string()))?;
            let result =
                crate::transport::hubnode::dispatch_method(&runtime.node, method, params).await?;
            ensure_pump(runtime, &instance_id);
            Ok(result)
        }
        Some(HubNodeMethod::InstanceCancel) => {
            let (instance_id, result) = cancel_from_params(&runtime.node, params).await?;
            ensure_pump(runtime, &instance_id);
            Ok(result)
        }
        Some(HubNodeMethod::InstanceRespond | HubNodeMethod::InteractionRespond) => {
            let (instance_id, result) = respond_from_params(&runtime.node, params).await?;
            ensure_pump(runtime, &instance_id);
            Ok(result)
        }
        Some(HubNodeMethod::TtyWrite | HubNodeMethod::InstanceKeys) => {
            let (instance_id, result) = keys_from_params(&runtime.node, params).await?;
            ensure_pump(runtime, &instance_id);
            Ok(result)
        }
        Some(HubNodeMethod::TtyResize | HubNodeMethod::TtyAttach) => {
            crate::transport::hubnode::dispatch_method(&runtime.node, method, params).await
        }
        _ if method == "instance.purge" => {
            // Must be explicit: the catch-all below answers `{"ok": true}`,
            // which would report a successful purge while removing nothing.
            let instance_id = params
                .get("instanceId")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    NodeError::InvalidRequest("instance.purge requires instanceId".into())
                })?;
            let instance_id = InstanceId::try_from(instance_id.to_owned())
                .map_err(|err| NodeError::InvalidRequest(err.to_string()))?;
            runtime.node.purge_instance(&instance_id).await
        }
        _ if method == "instance.close" => {
            let (instance_id, result) = close_from_params(&runtime.node, params).await?;
            ensure_pump(runtime, &instance_id);
            Ok(result)
        }
        _ if crate::worktree::is_worktree_method(method) => {
            runtime.node.worktree_rpc(method, &params)
        }
        // M1 batch 5a worker lifecycle RPCs over the outbound Hub link.
        Some(HubNodeMethod::WorkerProvision) => runtime.node.provision_worker(&params).await,
        Some(HubNodeMethod::WorkerRemove) => runtime.node.remove_worker(&params).await,
        _ => Ok(json!({ "ok": true })),
    }
}

async fn catch_up(runtime: &RuntimeLink, instance_id: &InstanceId) -> Result<(), NodeError> {
    ensure_pump(runtime, instance_id);
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
        .and_then(driver_kind_from_str)
        .or_else(|| spec.get("driver").and_then(driver_kind_from_value))
        .unwrap_or(DriverKind::ClaudePrint);
    let model = spec
        .get("model")
        .or_else(|| spec.get("modelId"))
        .and_then(Value::as_str)
        .unwrap_or("fake")
        .to_owned();
    let args = match spec.get("args") {
        None => Vec::new(),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value.as_str().map(str::to_owned).ok_or_else(|| {
                    NodeError::InvalidRequest("instance.create args must be strings".into())
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => {
            return Err(NodeError::InvalidRequest(
                "instance.create args must be an array".into(),
            ));
        }
    };
    // A non-string binaryPath is a caller bug, not something to coerce: the
    // value ends up on an exec, so guessing at it is the wrong instinct.
    let binary_path = match spec.get("binaryPath") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.trim().to_owned()).filter(|v| !v.is_empty()),
        Some(_) => {
            return Err(NodeError::InvalidRequest(
                "instance.create binaryPath must be a string".into(),
            ));
        }
    };
    let binary_sha256 = match spec.get("binarySha256") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.trim().to_owned()).filter(|v| !v.is_empty()),
        Some(_) => {
            return Err(NodeError::InvalidRequest(
                "instance.create binarySha256 must be a string".into(),
            ));
        }
    };
    let mut request = CreateInstanceRequest {
        origin: crate::origin::wire_origin(params),
        agent_credential: params
            .get("agentCredential")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?,
        command_id: parsed
            .command_id
            .as_deref()
            .or_else(|| params.get("commandId").and_then(Value::as_str))
            .map(|raw| remuda_protocol::CommandId::try_from(raw.to_owned()))
            .transpose()
            .map_err(|err| NodeError::InvalidRequest(err.to_string()))?,
        instance_id,
        host_id: Some(node.host().meta.id.clone()),
        workspace_id: spec
            .get("workspaceId")
            .and_then(Value::as_str)
            .filter(|raw| raw.parse::<remuda_protocol::WorkspaceId>().is_ok())
            .map(str::parse)
            .transpose()?,
        kind,
        driver,
        model,
        args,
        binary_path,
        binary_sha256,
        provider_profile_id: spec
            .get("providerProfileId")
            .and_then(Value::as_str)
            .unwrap_or("native")
            .to_owned(),
        permission_mode: spec
            .get("permissionMode")
            .and_then(Value::as_str)
            .unwrap_or("manual")
            .to_owned(),
        prompt: parsed
            .prompt
            .clone()
            .or_else(|| prompt_of(params))
            .unwrap_or_default(),
        cwd: cwd_of(spec),
        delegation: None,
        settings_overlay_path: None,
        claude_config_dir: None,
        max_budget_usd: None,
        provider_overlay: None,
        provider_auth_token: None,
        resume_session_id: None,
        resumed_from: None,
        // Filled by apply_spec_launch_fields from the Hub spec.
        effort: None,
        tui: None,
        extra_env: std::collections::BTreeMap::new(),
    };
    request.apply_spec_launch_fields(spec);
    Ok(request)
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
    let attachments = parsed
        .as_ref()
        .map(InstanceSendParams::attachments)
        .unwrap_or_default();
    let result = node
        .submit_command(
            &instance_id,
            InstanceCommandRequest {
                origin: crate::origin::wire_origin(&params),
                command_id,
                operation: CommandAction::Send,
                prompt: Some(prompt),
                attachments,
                run_id,
                interaction_id: None,
                answer: None,
                keys: None,
                model: None,
                effort_name: None,
                effort_index: None,
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
                origin: crate::origin::wire_origin(&params),
                command_id,
                operation: CommandAction::Cancel,
                prompt: None,
                attachments: Vec::new(),
                run_id,
                interaction_id: None,
                answer: None,
                keys: None,
                model: None,
                effort_name: None,
                effort_index: None,
            },
        )
        .await?;
    Ok((instance_id, serde_json::to_value(&result)?))
}

async fn close_from_params(
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
    let result = node
        .submit_command(
            &instance_id,
            InstanceCommandRequest {
                origin: crate::origin::wire_origin(&params),
                command_id,
                operation: CommandAction::Close,
                prompt: None,
                attachments: Vec::new(),
                run_id: None,
                interaction_id: None,
                answer: None,
                keys: None,
                model: None,
                effort_name: None,
                effort_index: None,
            },
        )
        .await?;
    Ok((instance_id, serde_json::to_value(&result)?))
}

async fn keys_from_params(node: &DevNode, params: Value) -> Result<(InstanceId, Value), NodeError> {
    let parsed: TtyWriteParams = serde_json::from_value(params.clone()).unwrap_or_default();
    let instance_id = parsed
        .instance_id
        .as_deref()
        .or_else(|| params.get("instanceId").and_then(Value::as_str))
        .ok_or_else(|| NodeError::InvalidRequest("tty.write requires instanceId".into()))?;
    let instance_id = InstanceId::try_from(instance_id.to_owned())
        .map_err(|err| NodeError::InvalidRequest(err.to_string()))?;
    if let Some(raw) = parsed
        .data_base64
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        let bytes = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(raw.as_bytes())
                .map_err(|error| {
                    NodeError::InvalidRequest(format!("invalid dataBase64: {error}"))
                })?
        };
        node.tty().write_bytes(&instance_id, &bytes).await?;
        return Ok((instance_id, json!({ "ok": true, "accepted": "tty-bytes" })));
    }
    let keys = parsed.key_names();
    if keys.is_empty() {
        return Err(NodeError::InvalidRequest(
            "tty.write requires keys or dataBase64".into(),
        ));
    }
    let mapped = remuda_driver::logical_keys_to_bytes(&keys);
    if node.tty().write_bytes(&instance_id, &mapped).await.is_ok() {
        return Ok((instance_id, json!({ "ok": true, "accepted": "tty-bytes" })));
    }
    let command_id = parsed
        .command_id
        .as_deref()
        .and_then(|raw| remuda_protocol::CommandId::try_from(raw.to_owned()).ok())
        .or_else(|| command_id_of(&params));
    let result = node
        .submit_command(
            &instance_id,
            InstanceCommandRequest {
                origin: crate::origin::wire_origin(&params),
                command_id,
                operation: CommandAction::WriteTty,
                prompt: None,
                attachments: Vec::new(),
                run_id: None,
                interaction_id: None,
                answer: None,
                keys: Some(keys),
                model: None,
                effort_name: None,
                effort_index: None,
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
                origin: crate::origin::wire_origin(&params),
                command_id,
                operation: CommandAction::RespondInteraction,
                prompt: None,
                attachments: Vec::new(),
                run_id: None,
                interaction_id,
                answer: Some(answer),
                keys: None,
                model: None,
                effort_name: None,
                effort_index: None,
            },
        )
        .await?;
    Ok((instance_id, serde_json::to_value(&result)?))
}

fn driver_kind_from_str(raw: &str) -> Option<DriverKind> {
    match raw {
        "pty" | "generic-pty" | "generic_pty" | "genericPty" => Some(DriverKind::GenericPty),
        "shell" | "shell-pty" | "terminal" => Some(DriverKind::ShellPty),
        other => serde_json::from_value(json!(other)).ok(),
    }
}

fn driver_kind_from_value(value: &Value) -> Option<DriverKind> {
    match value {
        Value::String(raw) => driver_kind_from_str(raw),
        other => serde_json::from_value(other.clone()).ok(),
    }
}

fn cwd_of(spec: &Value) -> Option<String> {
    for key in ["cwd", "workspaceId"] {
        let Some(raw) = spec.get(key).and_then(Value::as_str) else {
            continue;
        };
        if raw.is_empty() || raw.parse::<remuda_protocol::WorkspaceId>().is_ok() {
            continue;
        }
        return Some(raw.to_owned());
    }
    None
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
    if let Some(text) = params
        .pointer("/initialInput/text")
        .or_else(|| params.get("prompt"))
        .or_else(|| params.pointer("/spec/prompt"))
        .or_else(|| params.pointer("/input/text"))
        .or_else(|| params.get("text"))
        .and_then(Value::as_str)
    {
        return Some(text.to_owned());
    }
    let blocks = params.pointer("/input/blocks")?.as_array()?;
    let mut out = String::new();
    for block in blocks {
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    if out.is_empty() { None } else { Some(out) }
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
        let needs_replay = flush_journal(&runtime, &instance_id).await.is_err();
        pump_live(&runtime, instance_id, rx, needs_replay).await;
    });
}

/// Maximum unacknowledged `journal.append` frames per instance.
///
/// Bounds memory and Hub-side queue depth while removing the per-event RTT
/// serialization. 16 matches the daemon NDJSON uplink's in-flight budget.
const UPLINK_WINDOW: usize = 16;

async fn flush_journal(runtime: &RuntimeLink, instance_id: &InstanceId) -> Result<(), NodeError> {
    let instance = runtime.node.get_instance(instance_id)?;
    // Replay is also pipelined: fill the window directly from durable rows.
    let mut window = super::uplink::UplinkWindow::new(UPLINK_WINDOW);
    loop {
        if window.is_empty() {
            let after = current_watermark(&runtime.watermarks, instance_id.as_id().as_str());
            let after_seq = (after > 0).then_some(U64(u64::try_from(after).unwrap_or(0)));
            let page = runtime
                .node
                .read_journal(&instance.journal_id, after_seq, 256)?;
            if page.events.is_empty() {
                break;
            }
            for event in &page.events {
                submit_forward(runtime, instance_id, event, &mut window)?;
                if !window.has_capacity() {
                    break;
                }
            }
        }
        while let Some(result) = window.next().await {
            acknowledge(runtime, instance_id, result)?;
        }
    }
    Ok(())
}

async fn pump_live(
    runtime: &RuntimeLink,
    instance_id: InstanceId,
    mut rx: broadcast::Receiver<JournalEvent>,
    mut needs_replay: bool,
) {
    let mut retry = tokio::time::interval(std::time::Duration::from_millis(250));
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut window = super::uplink::UplinkWindow::new(UPLINK_WINDOW);
    loop {
        // Events are submitted whenever there is window capacity; ACKs drain
        // in order whenever they are ready. The two are independent, so a
        // burst of six fills the wire immediately instead of queueing behind
        // six sequential RTTs.
        let can_send = window.has_capacity();
        tokio::select! {
            biased;
            // Collect in-order ACKs whenever any are outstanding; this frees
            // window capacity and advances the confirmed watermark.
            ack = window.next(), if !window.is_empty() => {
                let Some(ack) = ack else { break };
                if let Err(error) = acknowledge(runtime, &instance_id, ack) {
                    tracing::debug!(%error, "journal pump forward failed");
                    window = super::uplink::UplinkWindow::new(UPLINK_WINDOW);
                    needs_replay = true;
                }
            }
            event = rx.recv(), if can_send && !needs_replay => {
                match event {
                    Ok(event) => {
                        if let Err(error) = submit_forward(runtime, &instance_id, &event, &mut window)
                        {
                            tracing::debug!(%error, "journal pump forward failed");
                            window = super::uplink::UplinkWindow::new(UPLINK_WINDOW);
                            needs_replay = true;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => needs_replay = true,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = retry.tick(), if needs_replay => {
                needs_replay = flush_journal(runtime, &instance_id).await.is_err();
                // Rows appended by live observations during replay are either
                // already in the window or behind the new watermark; resume
                // the broadcast.
            }
            _ = runtime.journal.tx.closed() => break,
        }
    }
    // Drain in-flight appends on shutdown so confirmed rows are not replayed
    // needlessly on the next connection (best effort).
    while let Some(ack) = window.next().await {
        let _ = acknowledge(runtime, &instance_id, ack);
    }
}

/// One submitted append plus its identity, resolved in submission order.
type UplinkResult = Result<(Option<String>, i64, i64), NodeError>;

/// Submit one event into the pipeline without awaiting its ACK.
fn submit_forward(
    runtime: &RuntimeLink,
    instance_id: &InstanceId,
    event: &JournalEvent,
    window: &mut super::uplink::UplinkWindow<UplinkResult>,
) -> Result<(), NodeError> {
    let seq = i64::try_from(event.position().1.0).unwrap_or(0);
    let key = instance_id.as_id().as_str();
    if seq <= current_watermark(&runtime.watermarks, key) {
        return Ok(());
    }
    let value = serde_json::to_value(event)?;
    let runtime = runtime.clone_link();
    let key = key.to_owned();
    let instance = runtime.node.get_instance(instance_id)?;
    let journal_id = instance.journal_id.as_str().to_owned();
    window.push(Box::pin(async move {
        let result = runtime.journal.append_seq(key.clone(), seq, value).await;
        match result {
            Ok(ack) => {
                let acked = ack
                    .get("durableSeq")
                    .or_else(|| ack.get("seq"))
                    .and_then(super::value_i64)
                    .unwrap_or(seq);
                Ok((Some(journal_id), seq, acked))
            }
            Err(error) if super::already_durable(&error, seq) => Ok((Some(journal_id), seq, seq)),
            Err(error) => Err(error),
        }
    }));
    Ok(())
}

/// Advance the watermark for an append resolved in journal order.
fn acknowledge(
    runtime: &RuntimeLink,
    instance_id: &InstanceId,
    result: UplinkResult,
) -> Result<(), NodeError> {
    let (journal_id, seq, acked) = result?;
    record_watermark(
        &runtime.watermarks,
        instance_id.as_id().as_str(),
        journal_id,
        acked.max(seq),
    );
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DevServerConfig;

    #[tokio::test]
    async fn failed_final_event_replays_from_confirmed_watermark_without_new_broadcast() {
        let dir = tempfile::tempdir().expect("data directory");
        let node = crate::compose(&crate::ServeConfig::fake(
            DevServerConfig::loopback(0)
                .with_workspace_roots(remuda_testing::test_workspace_roots!()),
            dir.path().to_path_buf(),
        ))
        .expect("node");
        let created = node
            .create_instance(
                serde_json::from_value(json!({"prompt":"final event retry"})).expect("request"),
            )
            .await
            .expect("create");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let command = node
                    .get_command(&created.command.command_id)
                    .expect("command");
                if serde_json::to_value(command).expect("command JSON")["state"] == "settled" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fake instance settles");
        let page = node
            .read_journal(&created.instance.journal_id, None, 256)
            .expect("journal");
        let last = page.events.last().expect("final durable event").clone();
        let seq = i64::try_from(last.position().1.0).expect("sequence");
        let key = created.instance.meta.id.as_id().to_string();
        let watermarks = Arc::new(Mutex::new(HashMap::new()));
        record_watermark(&watermarks, &key, None, seq - 1);
        let (journal, mut jobs, _) = super::super::journal_channel(4);
        let runtime = RuntimeLink {
            controller: None,
            hub_url: "http://127.0.0.1:1".into(),
            node: node.clone(),
            journal,
            watermarks: watermarks.clone(),
            pumps: Arc::new(Mutex::new(HashSet::new())),
        };
        let (live, receiver) = broadcast::channel(2);
        let instance_id = created.instance.meta.id;
        let pump = tokio::spawn(async move {
            pump_live(&runtime, instance_id, receiver, false).await;
        });
        live.send(last).expect("last event broadcast");
        let failed = tokio::time::timeout(std::time::Duration::from_secs(2), jobs.recv())
            .await
            .expect("first send")
            .expect("append");
        assert_eq!(failed.seq, Some(seq));
        failed
            .reply
            .send(Err(NodeError::Disconnected))
            .expect("failed ACK");
        let retried = tokio::time::timeout(std::time::Duration::from_secs(2), jobs.recv())
            .await
            .expect("replay without another broadcast")
            .expect("append retry");
        assert_eq!(retried.seq, Some(seq));
        assert_eq!(
            current_watermark(&watermarks, &key),
            seq - 1,
            "failed append must not promote ACK"
        );
        retried
            .reply
            .send(Ok(json!({"durableSeq":seq.to_string()})))
            .expect("durable ACK");
        drop(jobs);
        tokio::time::timeout(std::time::Duration::from_secs(2), pump)
            .await
            .expect("closed transport stops replay")
            .expect("pump task");
        assert_eq!(current_watermark(&watermarks, &key), seq);
        node.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn create_params_preserve_native_model_permission_and_allowlisted_args() {
        let node = DevNode::new(
            &DevServerConfig::loopback(0)
                .with_workspace_roots(remuda_testing::test_workspace_roots!()),
        )
        .expect("node");
        let request = create_from_params(
            &node,
            &json!({
                "instanceId": InstanceId::new(),
                "spec": {
                    "kind": "claude",
                    "driver": "claude-print",
                    "model": "haiku",
                    "args": ["--max-budget-usd", "0.3"],
                    "providerProfileId": "native-login",
                    "permissionMode": "dontAsk"
                },
                "initialInput": { "text": "one bounded turn" }
            }),
        )
        .expect("create params");
        assert_eq!(request.model, "haiku");
        assert_eq!(request.args, ["--max-budget-usd", "0.3"]);
        assert_eq!(request.provider_profile_id, "native-login");
        assert_eq!(request.permission_mode, "dontAsk");
        assert_eq!(request.prompt, "one bounded turn");
    }

    #[tokio::test]
    async fn create_params_preserve_gateway_overlay_config_dir_and_budget() {
        let node = DevNode::new(
            &DevServerConfig::loopback(0)
                .with_workspace_roots(remuda_testing::test_workspace_roots!()),
        )
        .expect("node");
        let request = create_from_params(
            &node,
            &json!({
                "instanceId": InstanceId::new(),
                "spec": {
                    "kind": "claude",
                    "driver": "claude-print",
                    "model": "haiku",
                    "providerProfileId": "gateway",
                    "permissionMode": "bypassPermissions",
                    "delegation": "gateway",
                    "settingsOverlayPath": "/tmp/remuda-settings.overlay.json",
                    "claudeConfigDir": "/tmp/remuda-claude-home",
                    "maxBudgetUsd": 0.3
                },
                "initialInput": { "text": "你好，你是什么模型" }
            }),
        )
        .expect("create params");
        assert_eq!(request.delegation.as_deref(), Some("gateway"));
        assert_eq!(request.provider_profile_id, "gateway");
        assert_eq!(
            request.settings_overlay_path.as_deref(),
            Some("/tmp/remuda-settings.overlay.json")
        );
        assert_eq!(
            request.claude_config_dir.as_deref(),
            Some("/tmp/remuda-claude-home")
        );
        assert_eq!(request.max_budget_usd.as_deref(), Some("0.3"));
        assert_eq!(request.prompt, "你好，你是什么模型");
    }

    #[tokio::test]
    async fn create_params_round_trip_binary_path_and_reject_a_non_string() {
        let node = DevNode::new(
            &DevServerConfig::loopback(0)
                .with_workspace_roots(remuda_testing::test_workspace_roots!()),
        )
        .expect("node");
        let request = create_from_params(
            &node,
            &json!({
                "instanceId": InstanceId::new(),
                "spec": {
                    "kind": "claude",
                    "driver": "claude-print",
                    "model": "haiku",
                    "binaryPath": "/opt/claude/bin/claude",
                    "binarySha256": "sha256:abc123",
                },
                "initialInput": { "text": "custom executable" }
            }),
        )
        .expect("create params");
        assert_eq!(
            request.binary_path.as_deref(),
            Some("/opt/claude/bin/claude")
        );
        assert_eq!(request.binary_sha256.as_deref(), Some("sha256:abc123"));

        // Absent stays absent, so the Node's own resolution order is untouched
        // for every request that does not ask for a specific binary.
        let plain = create_from_params(
            &node,
            &json!({
                "instanceId": InstanceId::new(),
                "spec": { "kind": "claude", "driver": "claude-print", "model": "haiku" },
                "initialInput": { "text": "stock" }
            }),
        )
        .expect("create params");
        assert_eq!(plain.binary_path, None);

        // A non-string is a caller bug. The value ends up on an exec, so
        // coercing it is the wrong instinct.
        let error = create_from_params(&node, &json!({ "spec": { "binaryPath": 7 } }))
            .expect_err("numeric binaryPath must fail closed");
        assert!(error.to_string().contains("binaryPath must be a string"));
    }

    #[tokio::test]
    async fn create_params_reject_non_string_native_args() {
        let node = DevNode::new(
            &DevServerConfig::loopback(0)
                .with_workspace_roots(remuda_testing::test_workspace_roots!()),
        )
        .expect("node");
        let error = create_from_params(
            &node,
            &json!({ "spec": { "args": ["--max-budget-usd", 0.3] } }),
        )
        .expect_err("numeric argv must fail closed");
        assert!(error.to_string().contains("args must be strings"));
    }
}
