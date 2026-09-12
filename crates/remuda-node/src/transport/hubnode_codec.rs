//! Encode/decode `remuda_protocol::hubnode` frames for stdio NDJSON and outbound WSS.

use crate::{CommandAction, CreateInstanceRequest, DevNode, InstanceCommandRequest, NodeError};
use remuda_protocol::hubnode::{
    self, HubNodeMethod, HubNodeRequest, InstanceCancelParams, InstanceCreateParams,
    InstanceRespondParams, InstanceSendParams, JournalAppendParams, METHOD_NODE_AUTH,
    METHOD_NODE_HELLO, NodeAuthParams, NodeHelloParams,
};
use remuda_protocol::{
    AgentKind, DriverKind, HeartbeatParams, HelloParams, HostId, Id, InstanceId, PROTOCOL_VERSION,
    ProtocolRange, ResumeCursor, U64,
};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::str::FromStr;

/// Per-instance Hub-acknowledged durable sequence.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SeqWatermark {
    /// Instance identity (`ins_…`).
    pub instance_id: String,
    /// Journal identity (`obj_…`) when known.
    pub journal_id: Option<String>,
    /// Last Hub-acked sequence (inclusive).
    pub seq: i64,
}

/// `node.hello` params: protocol HelloParams plus Hub inventory.
#[allow(dead_code)]
pub fn encode_hello(
    host_id: &str,
    node_version: &str,
    label: &str,
    cli: &Value,
    node_epoch: &Id,
    watermarks: &HashMap<String, SeqWatermark>,
) -> Value {
    encode_hello_transport(
        host_id,
        node_version,
        label,
        cli,
        node_epoch,
        watermarks,
        "outbound-wss",
        None,
        None,
    )
}

/// Hello params with an explicit carrier and optional nested host inventory.
#[allow(dead_code, clippy::too_many_arguments)]
pub fn encode_hello_transport(
    host_id: &str,
    node_version: &str,
    label: &str,
    cli: &Value,
    node_epoch: &Id,
    watermarks: &HashMap<String, SeqWatermark>,
    transport: &str,
    enrollment_token: Option<&str>,
    host: Option<Value>,
) -> Value {
    let typed = typed_hello(host_id, node_version, node_epoch, watermarks);
    let mut params = match typed {
        Ok(value) => value,
        Err(_) => json!({
            "hostId": host_id,
            "nodeVersion": node_version,
            "nodeEpoch": node_epoch.to_string(),
        }),
    };
    let Some(object) = params.as_object_mut() else {
        return params;
    };
    object.insert("label".into(), json!(label));
    object.insert("transport".into(), json!(transport));
    object.insert("cli".into(), cli.clone());
    object.insert("version".into(), json!(PROTOCOL_VERSION));
    object.insert("resumeCursors".into(), json!(resume_cursors(watermarks)));
    if let Some(token) = enrollment_token.filter(|token| !token.is_empty()) {
        object.insert("enrollmentToken".into(), json!(token));
    }
    if let Some(host) = host {
        object.insert("host".into(), host);
    }
    params
}

/// Heartbeat params: protocol HeartbeatParams plus Hub inventory refresh.
#[allow(dead_code)]
pub fn encode_heartbeat(
    node_version: &str,
    cli: &Value,
    connection_id: Option<&str>,
    lease_id: Option<&str>,
    watermarks: &HashMap<String, SeqWatermark>,
) -> Value {
    let mut params = json!({
        "nodeVersion": node_version,
        "cli": cli,
        "transport": "outbound-wss",
        "version": PROTOCOL_VERSION,
    });
    if let (Some(connection_id), Some(lease_id)) = (connection_id, lease_id)
        && let Ok(typed) = typed_heartbeat(connection_id, lease_id, watermarks)
        && let Some(object) = params.as_object_mut()
        && let Some(typed_object) = typed.as_object()
    {
        for (key, value) in typed_object {
            object.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    params
}

/// `journal.append` params with an optional sequence watermark and a batch array.
#[allow(dead_code)]
pub fn encode_append(instance_id: &str, seq: Option<i64>, event: Value) -> Value {
    let mut params = Map::new();
    params.insert("instanceId".into(), json!(instance_id));
    params.insert("event".into(), event.clone());
    params.insert("events".into(), json!([event]));
    if let Some(seq) = seq {
        params.insert("seq".into(), json!(seq.to_string()));
        params.insert(
            "watermark".into(),
            json!({
                "instanceId": instance_id,
                "durableSeq": seq.to_string(),
            }),
        );
    }
    Value::Object(params)
}

/// JSON-RPC request with protocol `version`.
#[must_use]
pub fn rpc_request(id: impl Into<Value>, method: &str, params: Value) -> Value {
    hubnode::rpc_request(id, method, params)
}

/// JSON-RPC success with protocol `version`.
#[must_use]
pub fn rpc_ok(id: impl Into<Value>, result: Value) -> Value {
    hubnode::rpc_ok(id, result)
}

/// JSON-RPC error with protocol `version`.
#[must_use]
pub fn rpc_error(id: impl Into<Value>, code: i64, message: &str) -> Value {
    hubnode::rpc_error(id, code, message)
}

/// Stdio first-frame `node.auth`.
#[must_use]
pub fn encode_auth(id: &str, token: &str) -> Value {
    let params = NodeAuthParams {
        token: token.to_owned(),
        scheme: Some(hubnode::AUTH_SCHEME_BEARER.into()),
    };
    rpc_request(id, METHOD_NODE_AUTH, json!(params))
}

/// Stdio/WSS `node.hello` request (has `id` + `version`).
#[must_use]
pub fn encode_hello_request(id: &str, params: Value) -> Value {
    rpc_request(id, METHOD_NODE_HELLO, params)
}

/// Decode a Hub↔Node request.
pub fn decode_request(value: &Value) -> Result<HubNodeRequest, NodeError> {
    HubNodeRequest::from_value(value).map_err(NodeError::from)
}

/// Dispatch `instance.*` (and journal/tty acks) onto the local runtime.
pub async fn dispatch_method(
    node: &DevNode,
    method: &str,
    params: Value,
) -> Result<Value, NodeError> {
    match HubNodeMethod::parse(method) {
        Some(HubNodeMethod::InstanceCreate) => dispatch_create(node, params).await,
        Some(HubNodeMethod::InstanceSend) => dispatch_send(node, params).await,
        Some(HubNodeMethod::InstanceCancel) => dispatch_cancel(node, params).await,
        Some(HubNodeMethod::InstanceRespond | HubNodeMethod::InteractionRespond) => {
            dispatch_respond(node, params).await
        }
        Some(HubNodeMethod::JournalAppend) => {
            let parsed: JournalAppendParams = serde_json::from_value(params)?;
            Ok(json!({
                "ok": true,
                "instanceId": parsed.instance_id,
                "accepted": parsed.events_to_append().len(),
            }))
        }
        Some(HubNodeMethod::TtyFrame) => Ok(json!({ "ok": true })),
        Some(HubNodeMethod::NodeHeartbeat | HubNodeMethod::RuntimeHeartbeat) => {
            Ok(json!({ "ok": true }))
        }
        Some(kind) => {
            let name: &str = HubNodeMethod::as_str(kind);
            Err(NodeError::InvalidRequest(format!(
                "stdio/wss runtime does not handle {name}"
            )))
        }
        None => Err(NodeError::InvalidRequest(format!(
            "unknown hubnode method {method}"
        ))),
    }
}

/// Persist Hub hello result into `enrollment.json` when `hostId`/`nodeToken` are present.
pub fn persist_hello_result(data_dir: &std::path::Path, frame: &Value) -> Result<(), NodeError> {
    let result = frame.get("result").unwrap_or(frame);
    if result.get("hostId").is_some() || result.get("nodeToken").is_some() {
        crate::enroll::apply_hello_result(data_dir, result)?;
    }
    Ok(())
}

async fn dispatch_create(node: &DevNode, params: Value) -> Result<Value, NodeError> {
    let parsed: InstanceCreateParams =
        serde_json::from_value(params.clone()).unwrap_or(InstanceCreateParams {
            instance_id: None,
            command_id: None,
            spec: None,
            initial_input: None,
            kind: None,
            driver: None,
            prompt: None,
            host_id: None,
            workspace_id: None,
        });
    let spec = parsed.spec.clone().unwrap_or_else(|| params.clone());
    let mut request: CreateInstanceRequest = match serde_json::from_value(spec) {
        Ok(request) => request,
        Err(_) => CreateInstanceRequest {
            instance_id: None,
            host_id: None,
            workspace_id: None,
            kind: AgentKind::Claude,
            driver: DriverKind::ClaudePrint,
            model: "fake".into(),
            args: Vec::new(),
            provider_profile_id: "dev-fake".into(),
            permission_mode: "dontAsk".into(),
            prompt: String::new(),
        },
    };
    if request.instance_id.is_none()
        && let Some(id) = parsed
            .instance_id
            .as_deref()
            .or_else(|| params.get("instanceId").and_then(Value::as_str))
    {
        request.instance_id = Some(InstanceId::from_str(id)?);
    }
    if request.host_id.is_none() {
        let raw = parsed
            .host_id
            .as_deref()
            .or_else(|| params.get("hostId").and_then(Value::as_str));
        if let Some(raw) = raw {
            request.host_id = Some(HostId::from_str(raw)?);
        }
    }
    if request.prompt.is_empty() {
        if let Some(prompt) = parsed.prompt.clone() {
            request.prompt = prompt;
        } else {
            let from_input = parsed
                .initial_input
                .as_ref()
                .and_then(|input: &Value| input.get("text").and_then(Value::as_str));
            let from_params = params.pointer("/initialInput/text").and_then(Value::as_str);
            if let Some(text) = from_input.or(from_params) {
                request.prompt = String::from(text);
            }
        }
    }
    // Hub routed this RPC to this Node; bind create to the local Host/Workspace.
    request.host_id = Some(node.host().meta.id.clone());
    request.workspace_id = Some(node.workspace().meta.id.clone());
    serde_json::to_value(node.create_instance(request).await?).map_err(NodeError::from)
}

async fn dispatch_send(node: &DevNode, params: Value) -> Result<Value, NodeError> {
    let parsed: InstanceSendParams = serde_json::from_value(params.clone())?;
    let prompt = parsed
        .prompt_text()
        .ok_or_else(|| NodeError::InvalidRequest("instance.send requires input.text".into()))?
        .to_owned();
    let instance_id = InstanceId::from_str(&parsed.instance_id)?;
    submit(
        node,
        &instance_id,
        CommandAction::Send,
        Some(prompt),
        parsed.command_id.as_deref(),
        parsed.run_id.as_deref(),
        None,
        None,
    )
    .await
}

async fn dispatch_cancel(node: &DevNode, params: Value) -> Result<Value, NodeError> {
    let parsed: InstanceCancelParams = serde_json::from_value(params)?;
    let instance_id = InstanceId::from_str(&parsed.instance_id)?;
    submit(
        node,
        &instance_id,
        CommandAction::Cancel,
        None,
        parsed.command_id.as_deref(),
        parsed.run_id.as_deref(),
        None,
        None,
    )
    .await
}

async fn dispatch_respond(node: &DevNode, params: Value) -> Result<Value, NodeError> {
    let parsed: InstanceRespondParams = serde_json::from_value(params.clone())?;
    let instance_id = parsed
        .instance_id
        .as_deref()
        .or_else(|| params.get("instanceId").and_then(Value::as_str))
        .ok_or_else(|| NodeError::InvalidRequest("instance.respond requires instanceId".into()))?;
    let instance_id = InstanceId::from_str(instance_id)?;
    submit(
        node,
        &instance_id,
        CommandAction::RespondInteraction,
        None,
        parsed.command_id.as_deref(),
        None,
        parsed.interaction_id.as_deref(),
        parsed.answer.clone(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn submit(
    node: &DevNode,
    instance_id: &InstanceId,
    operation: CommandAction,
    prompt: Option<String>,
    command_id: Option<&str>,
    run_id: Option<&str>,
    interaction_id: Option<&str>,
    answer: Option<Value>,
) -> Result<Value, NodeError> {
    let result = node
        .submit_command(
            instance_id,
            InstanceCommandRequest {
                command_id: command_id.map(str::parse).transpose()?,
                operation,
                prompt,
                run_id: run_id.map(str::parse).transpose()?,
                interaction_id: interaction_id.map(str::parse).transpose()?,
                answer,
            },
        )
        .await?;
    serde_json::to_value(result).map_err(NodeError::from)
}

#[allow(dead_code)]
fn typed_hello(
    host_id: &str,
    node_version: &str,
    node_epoch: &Id,
    watermarks: &HashMap<String, SeqWatermark>,
) -> Result<Value, remuda_protocol::WireValueError> {
    let host_id = HostId::try_from(host_id.to_owned())?;
    let params = HelloParams {
        host_id,
        node_epoch: node_epoch.clone(),
        node_version: node_version.to_owned(),
        protocol: ProtocolRange {
            major: PROTOCOL_VERSION.major,
            min_minor: 0,
            max_minor: PROTOCOL_VERSION.minor,
        },
        observation_schema_majors: vec![1],
        features: vec!["snapshot-follow-v1".into(), "tty-binary-v1".into()],
        resume_cursors: typed_resume(watermarks),
    };
    serde_json::to_value(params).map_err(|err| remuda_protocol::WireValueError(err.to_string()))
}

#[allow(dead_code)]
fn typed_heartbeat(
    connection_id: &str,
    lease_id: &str,
    watermarks: &HashMap<String, SeqWatermark>,
) -> Result<Value, remuda_protocol::WireValueError> {
    let params = HeartbeatParams {
        connection_id: Id::try_from(connection_id.to_owned())?,
        registry_watermarks: Vec::new(),
        instance_watermarks: watermarks
            .values()
            .filter_map(|mark| {
                let journal_id = mark.journal_id.clone()?;
                let journal_id = Id::try_from(journal_id).ok()?;
                Some(remuda_protocol::JournalWatermark {
                    journal_id,
                    durable_seq: U64(u64::try_from(mark.seq.max(0)).unwrap_or(0)),
                })
            })
            .collect(),
        lease_id: Id::try_from(lease_id.to_owned())?,
    };
    serde_json::to_value(params).map_err(|err| remuda_protocol::WireValueError(err.to_string()))
}

#[allow(dead_code)]
fn typed_resume(watermarks: &HashMap<String, SeqWatermark>) -> Vec<ResumeCursor> {
    watermarks
        .values()
        .filter_map(|mark| {
            let raw = mark
                .journal_id
                .clone()
                .unwrap_or_else(|| mark.instance_id.clone());
            let journal_id = Id::try_from(raw).ok()?;
            Some(ResumeCursor {
                journal_id,
                after_seq: U64(u64::try_from(mark.seq.max(0)).unwrap_or(0)),
            })
        })
        .collect()
}

#[allow(dead_code)]
fn resume_cursors(watermarks: &HashMap<String, SeqWatermark>) -> Vec<Value> {
    watermarks
        .values()
        .map(|mark| {
            json!({
                "journalId": mark.journal_id.as_deref().unwrap_or(mark.instance_id.as_str()),
                "afterSeq": mark.seq.to_string(),
                "instanceId": mark.instance_id,
            })
        })
        .collect()
}

/// Build stdio hello params from a nested host inventory value.
#[must_use]
pub fn stdio_hello_params(
    host_id: &HostId,
    label: &str,
    transport: &str,
    node_version: &str,
    node_epoch: &Id,
    host: Value,
    enrollment_token: Option<&str>,
) -> Value {
    let mut params = json!(NodeHelloParams {
        host_id: Some(host_id.as_id().as_str().to_owned()),
        label: Some(label.to_owned()),
        node_version: Some(node_version.to_owned()),
        node_epoch: Some(node_epoch.to_string()),
        enrollment_token: enrollment_token.map(str::to_owned),
        transport: Some(transport.to_owned()),
        version: Some(PROTOCOL_VERSION),
        protocol: Some(json!({
            "major": PROTOCOL_VERSION.major,
            "minor": PROTOCOL_VERSION.minor,
            "framing": "ndjson",
            "maxFrameBytes": 1_048_576,
        })),
        host: serde_json::from_value(host.clone()).ok(),
        capabilities: None,
        cli: host.get("cli").cloned(),
    });
    if let Some(object) = params.as_object_mut() {
        object.insert("host".into(), host);
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_includes_protocol_resume_and_inventory() {
        let epoch = Id::new("epoch").expect("epoch");
        let host = HostId::new();
        let mut marks = HashMap::new();
        marks.insert(
            "ins_x".into(),
            SeqWatermark {
                instance_id: "ins_x".into(),
                journal_id: Some("obj_00000000-0000-7000-8000-000000000001".into()),
                seq: 4,
            },
        );
        let value = encode_hello(
            host.as_id().as_str(),
            "0.1.0",
            "lab",
            &json!([{"kind":"claude"}]),
            &epoch,
            &marks,
        );
        assert_eq!(value["hostId"], json!(host.as_id().as_str()));
        assert_eq!(value["protocol"]["major"], json!(1));
        assert_eq!(value["label"], json!("lab"));
        assert_eq!(value["transport"], json!("outbound-wss"));
        assert_eq!(value["cli"][0]["kind"], json!("claude"));
        assert!(
            value["resumeCursors"]
                .as_array()
                .is_some_and(|c| !c.is_empty())
        );
        let append = encode_append("ins_x", Some(4), json!({"kind":"message"}));
        assert_eq!(append["events"].as_array().map(Vec::len), Some(1));
        assert_eq!(append["watermark"]["durableSeq"], json!("4"));
    }

    #[test]
    fn request_carries_version() {
        let frame = rpc_request("hello-1", METHOD_NODE_HELLO, json!({"hostId":"hst_x"}));
        assert_eq!(frame["version"]["major"], 1);
        assert_eq!(frame["method"], METHOD_NODE_HELLO);
        assert_eq!(frame["id"], "hello-1");
    }
}
