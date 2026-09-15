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
    if crate::workspace::is_workspace_method(method) {
        return node.workspace_rpc(method, params);
    }
    if crate::interactions::is_interaction_method(method) {
        return node.dispatch_interaction(method, params).await;
    }
    match method {
        "host.doctor" => node.doctor().await,
        "instance.create" => {
            let created = node
                .create_instance(create_from_params(node, &params)?)
                .await?;
            serde_json::to_value(&created).map_err(NodeError::from)
        }
        // D-026: resume is a create whose spec names the native session to
        // continue. Falling through to the catch-all below is what made the
        // web Resume button silently do nothing.
        "instance.resume" => {
            let request = resume_from_params(node, &params)?;
            let created = node.create_instance(request).await?;
            serde_json::to_value(&created).map_err(NodeError::from)
        }
        "instance.configure" => {
            let instance_id = instance_id_of(&params)?;
            let result = node
                .submit_command(
                    &instance_id,
                    InstanceCommandRequest {
                        origin: crate::origin::wire_origin(&params),
                        command_id: command_id_of(&params),
                        operation: CommandAction::Configure,
                        prompt: None,
                        attachments: Vec::new(),
                        run_id: None,
                        interaction_id: None,
                        answer: None,
                        keys: None,
                        model: None,
                        effort_name: None,
                        effort_index: None,
                    }
                    .with_configure(&params),
                )
                .await?;
            serde_json::to_value(&result).map_err(NodeError::from)
        }
        "instance.send" => {
            let instance_id = instance_id_of(&params)?;
            let result = node
                .submit_command(
                    &instance_id,
                    InstanceCommandRequest {
                        origin: crate::origin::wire_origin(&params),
                        command_id: command_id_of(&params),
                        operation: CommandAction::Send,
                        prompt: Some(prompt_of(&params).unwrap_or_default()),
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
            serde_json::to_value(&result).map_err(NodeError::from)
        }
        "instance.purge" => {
            // Must be explicit: the catch-all below answers `{"ok": true}`,
            // which would report a successful purge while removing nothing.
            let instance_id = instance_id_of(&params)?;
            node.purge_instance(&instance_id).await
        }
        "instance.cancel" => {
            let instance_id = instance_id_of(&params)?;
            let result = node
                .submit_command(
                    &instance_id,
                    InstanceCommandRequest {
                        origin: crate::origin::wire_origin(&params),
                        command_id: command_id_of(&params),
                        operation: CommandAction::Cancel,
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
                        origin: crate::origin::wire_origin(&params),
                        command_id: command_id_of(&params),
                        operation: CommandAction::RespondInteraction,
                        prompt: None,
                        attachments: Vec::new(),
                        run_id: None,
                        interaction_id,
                        answer: Some(params.get("answer").cloned().unwrap_or(Value::Null)),
                        keys: None,
                        model: None,
                        effort_name: None,
                        effort_index: None,
                    },
                )
                .await?;
            serde_json::to_value(&result).map_err(NodeError::from)
        }
        "tty.write" | "instance.keys" | "tty.resize" | "tty.attach" | "tty.screen" => {
            crate::transport::hubnode::dispatch_method(node, method, params).await
        }
        method if crate::worktree::is_worktree_method(method) => node.worktree_rpc(method, &params),
        // Read-only SCM RPCs must be explicit; the catch-all below would
        // otherwise answer `{"ok":true}` and fake a successful read.
        method if crate::workspace_scm::is_scm_method(method) => {
            crate::workspace_scm::handle_rpc(node, method, &params)
        }
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
            Some("shell" | "shell-pty" | "terminal") => Some(DriverKind::ShellPty),
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
                .filter(|raw| {
                    !raw.is_empty() && raw.parse::<remuda_protocol::WorkspaceId>().is_err()
                })
        })
        .map(str::to_string);
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
        command_id: command_id_of(params),
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
        model: spec
            .get("model")
            .or_else(|| spec.get("modelId"))
            .and_then(Value::as_str)
            .unwrap_or("fake")
            .to_owned(),
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
        prompt: prompt_of(params).unwrap_or_default(),
        cwd,
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
    };
    request.apply_spec_launch_fields(spec);
    Ok(request)
}

/// Build the child create request behind `instance.resume` (D-026).
///
/// The Hub sends the parent's stored spec plus `resumeSessionId`, so the child
/// inherits provider, permission, model and cwd; only the driver may differ,
/// which is what "continue in a terminal" means.
fn resume_from_params(node: &DevNode, params: &Value) -> Result<CreateInstanceRequest, NodeError> {
    let mut request = create_from_params(node, params)?;
    let spec = params.get("spec").unwrap_or(params);
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
            .map_err(|error| NodeError::InvalidRequest(error.to_string()))?;
    }
    Ok(request)
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
