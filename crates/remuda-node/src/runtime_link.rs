//! Bind a Hub WSS session to a local [`DevNode`] without depending on the
//! outbound-WSS runtime rewrite.

use crate::{
    CommandAction, CreateInstanceRequest, DevNode, InstanceCommandRequest, NodeError, WssLink,
};
use remuda_protocol::{AgentKind, DriverKind, InstanceId, U64};
use serde_json::{Value, json};
use std::collections::HashMap;

/// Serve Hub→Node RPCs on `link` against `node` and mirror journals.
pub async fn attach_runtime(mut link: WssLink, node: DevNode) -> Result<(), NodeError> {
    let mut watermarks = HashMap::<String, i64>::new();
    loop {
        let Some(request) = link.next_hub_request().await else {
            break;
        };
        let result = dispatch(&node, &request.method, request.params.clone()).await;
        let _ = flush(&node, &link, &mut watermarks).await;
        let _ = request.respond(result).await;
    }
    Ok(())
}

async fn dispatch(node: &DevNode, method: &str, params: Value) -> Result<Value, NodeError> {
    if crate::interactions::is_interaction_method(method) {
        return node.dispatch_interaction(method, params).await;
    }
    match method {
        "instance.create" => {
            let created = node
                .create_instance(create_from_params(node, &params)?)
                .await?;
            serde_json::to_value(&created).map_err(NodeError::from)
        }
        "instance.send" => {
            let instance_id = instance_id_of(&params)?;
            let result = node
                .submit_command(
                    &instance_id,
                    InstanceCommandRequest {
                        command_id: command_id_of(&params),
                        operation: CommandAction::Send,
                        prompt: Some(prompt_of(&params).unwrap_or_default()),
                        run_id: None,
                        interaction_id: None,
                        answer: None,
                        keys: None,
                    },
                )
                .await?;
            serde_json::to_value(&result).map_err(NodeError::from)
        }
        "instance.cancel" => {
            let instance_id = instance_id_of(&params)?;
            let result = node
                .submit_command(
                    &instance_id,
                    InstanceCommandRequest {
                        command_id: command_id_of(&params),
                        operation: CommandAction::Cancel,
                        prompt: None,
                        run_id: None,
                        interaction_id: None,
                        answer: None,
                        keys: None,
                    },
                )
                .await?;
            serde_json::to_value(&result).map_err(NodeError::from)
        }
        "instance.respond" | "interaction.respond" => {
            let instance_id = instance_id_of(&params)?;
            let interaction_id = params
                .get("interactionId")
                .and_then(Value::as_str)
                .and_then(|id| id.parse().ok());
            let result = node
                .submit_command(
                    &instance_id,
                    InstanceCommandRequest {
                        command_id: command_id_of(&params),
                        operation: CommandAction::RespondInteraction,
                        prompt: None,
                        run_id: None,
                        interaction_id,
                        answer: Some(params.get("answer").cloned().unwrap_or(Value::Null)),
                        keys: None,
                    },
                )
                .await?;
            serde_json::to_value(&result).map_err(NodeError::from)
        }
        "tty.write" | "instance.keys" => {
            crate::transport::hubnode::dispatch_method(node, method, params).await
        }
        method if crate::worktree::is_worktree_method(method) => crate::worktree::handle_rpc(
            std::path::Path::new(&node.workspace().root_path),
            method,
            &params,
        )
        .ok_or_else(|| NodeError::InvalidRequest(format!("unknown method {method}")))?,
        _ => Ok(json!({ "ok": true })),
    }
}

async fn flush(
    node: &DevNode,
    link: &WssLink,
    watermarks: &mut HashMap<String, i64>,
) -> Result<(), NodeError> {
    let page = node.list_instances()?;
    for instance in page.items {
        let key = instance.meta.id.as_id().as_str().to_owned();
        let after = watermarks.get(&key).copied().unwrap_or(0);
        let after_seq = (after > 0).then_some(U64(u64::try_from(after).unwrap_or(0)));
        let events = node.read_journal(&instance.journal_id, after_seq, 256)?;
        for event in events.events {
            let seq = i64::try_from(event.position().1.0).unwrap_or(0);
            let value = serde_json::to_value(&event)?;
            link.append_journal(&key, value).await?;
            watermarks.insert(key.clone(), seq);
        }
    }
    Ok(())
}

fn create_from_params(node: &DevNode, params: &Value) -> Result<CreateInstanceRequest, NodeError> {
    let spec = params.get("spec").unwrap_or(params);
    let instance_id = params
        .get("instanceId")
        .or_else(|| spec.get("instanceId"))
        .and_then(Value::as_str)
        .map(|raw| InstanceId::try_from(raw.to_owned()))
        .transpose()
        .map_err(|err| NodeError::InvalidRequest(err.to_string()))?;
    let kind = spec
        .get("kind")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or(AgentKind::Claude);
    let driver = spec
        .get("driver")
        .and_then(|value| match value.as_str() {
            Some("pty" | "generic-pty" | "generic_pty" | "genericPty") => {
                Some(DriverKind::GenericPty)
            }
            _ => serde_json::from_value(value.clone()).ok(),
        })
        .unwrap_or(DriverKind::ClaudePrint);
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
    let cwd = spec
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|raw| !raw.is_empty())
        .or_else(|| {
            spec.get("workspaceId")
                .and_then(Value::as_str)
                .filter(|raw| !raw.is_empty() && !raw.starts_with("ws_"))
        })
        .map(str::to_string);
    Ok(CreateInstanceRequest {
        command_id: command_id_of(params),
        instance_id,
        host_id: Some(node.host().meta.id.clone()),
        workspace_id: None,
        kind,
        driver,
        model: spec
            .get("model")
            .or_else(|| spec.get("modelId"))
            .and_then(Value::as_str)
            .unwrap_or("fake")
            .to_owned(),
        args,
        provider_profile_id: spec
            .get("providerProfileId")
            .and_then(Value::as_str)
            .unwrap_or("native")
            .to_owned(),
        permission_mode: spec
            .get("permissionMode")
            .and_then(Value::as_str)
            .unwrap_or("dontAsk")
            .to_owned(),
        prompt: prompt_of(params).unwrap_or_default(),
        cwd,
    })
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
        .or_else(|| params.get("text"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}
