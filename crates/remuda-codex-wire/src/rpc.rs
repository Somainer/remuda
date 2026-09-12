//! JSON-RPC frames as spoken by `codex app-server --listen stdio://`.
//!
//! The native line protocol omits `"jsonrpc":"2.0"`. Classification does not
//! treat an extra `jsonrpc` field as fatal, but encoders never write it.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

use crate::notification::ServerNotification;
use crate::server_request::ServerRequest;

/// Default NDJSON line cap (16 MiB). Larger lines are a protocol error.
pub const DEFAULT_MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// JSON-RPC request id: integer or string. `1` and `"1"` are distinct.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric id, as used by the 0.154.0 probe.
    Integer(i64),
    /// String id.
    String(String),
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer(value) => write!(f, "{value}"),
            Self::String(value) => f.write_str(value),
        }
    }
}

/// Client or server request: `{id, method, params?}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// Request id.
    pub id: RequestId,
    /// Method name.
    pub method: String,
    /// Optional params object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// Notification: `{method, params?}` and no `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    /// Method name.
    pub method: String,
    /// Optional params object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// Success response: `{id, result}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// Matching request id.
    pub id: RequestId,
    /// Result payload.
    pub result: Value,
}

/// Native JSON-RPC error body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcErrorBody {
    /// Numeric code, for example `-32600`.
    pub code: i64,
    /// Message text; not a Remuda error discriminant.
    pub message: String,
    /// Optional native data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Error response: `{id, error}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Matching request id.
    pub id: RequestId,
    /// Error body.
    pub error: JsonRpcErrorBody,
}

/// One decoded NDJSON object.
#[derive(Debug, Clone, PartialEq)]
pub enum WireFrame {
    /// `{id, method}` — client or server request.
    Request(JsonRpcRequest),
    /// `{method}` without `id` — notification.
    Notification(JsonRpcNotification),
    /// `{id, result}`.
    Response(JsonRpcResponse),
    /// `{id, error}`.
    Error(JsonRpcError),
    /// Object that is JSON but not a JSON-RPC frame.
    Unknown(Value),
}

impl WireFrame {
    /// Classify a decoded JSON object. Extra fields such as `emittedAtMs` are kept
    /// only on [`WireFrame::Unknown`] or ignored by typed structs.
    pub fn from_value(value: Value) -> Self {
        let Some(object) = value.as_object() else {
            return Self::Unknown(value);
        };
        let id = object.get("id").cloned();
        let method = object.get("method").and_then(Value::as_str);
        let has_result = object.contains_key("result");
        let has_error = object.contains_key("error");

        if let (Some(id_value), Some(method_name)) = (id.clone(), method) {
            match serde_json::from_value::<RequestId>(id_value) {
                Ok(request_id) => {
                    return Self::Request(JsonRpcRequest {
                        id: request_id,
                        method: method_name.to_owned(),
                        params: object.get("params").cloned(),
                    });
                }
                Err(_) => return Self::Unknown(value),
            }
        }
        if let Some(method_name) = method {
            return Self::Notification(JsonRpcNotification {
                method: method_name.to_owned(),
                params: object.get("params").cloned(),
            });
        }
        if let (Some(id_value), true) = (id.clone(), has_error) {
            if let Ok(frame) = serde_json::from_value::<JsonRpcError>(value.clone()) {
                return Self::Error(frame);
            }
            let _ = id_value;
        }
        if let (Some(_), true) = (id, has_result)
            && let Ok(frame) = serde_json::from_value::<JsonRpcResponse>(value.clone())
        {
            return Self::Response(frame);
        }
        Self::Unknown(value)
    }

    /// Encode this frame as a JSON object **without** a `jsonrpc` field.
    pub fn to_value(&self) -> Result<Value, serde_json::Error> {
        match self {
            Self::Request(frame) => serde_json::to_value(frame),
            Self::Notification(frame) => serde_json::to_value(frame),
            Self::Response(frame) => serde_json::to_value(frame),
            Self::Error(frame) => serde_json::to_value(frame),
            Self::Unknown(value) => Ok(value.clone()),
        }
    }
}

/// Encode `value` as one NDJSON line without a trailing-only requirement on a
/// `jsonrpc` member. Returns the JSON object bytes plus a newline.
pub fn encode_line<T: Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Typed inbound traffic that is not a response to a client request.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    /// Server notification (`item/*`, `turn/completed`, …) or [`ServerNotification::Unknown`].
    Notification(ServerNotification),
    /// Server→client request that **must** be answered with `{id, result}`.
    ServerRequest(ServerRequest),
    /// A JSON object that was not a request, notification, response, or error.
    UnknownFrame(Value),
    /// A non-empty stdout line that was not JSON.
    NonJson(String),
}

impl Inbound {
    pub(crate) fn from_frame(frame: WireFrame, raw: Value) -> Option<Self> {
        match frame {
            WireFrame::Notification(notification) => {
                Some(Self::Notification(ServerNotification::from_method_params(
                    &notification.method,
                    notification.params,
                    raw,
                )))
            }
            WireFrame::Request(request) => Some(Self::ServerRequest(ServerRequest::from_request(
                request, raw,
            ))),
            WireFrame::Unknown(value) => Some(Self::UnknownFrame(value)),
            WireFrame::Response(_) | WireFrame::Error(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_request_omits_jsonrpc() {
        let request = JsonRpcRequest {
            id: RequestId::Integer(2),
            method: "initialize".into(),
            params: Some(serde_json::json!({"clientInfo":{"name":"remuda","version":"0.1.0"}})),
        };
        let value = serde_json::to_value(&request).expect("serialize");
        assert!(value.get("jsonrpc").is_none());
        assert_eq!(value["id"], 2);
        assert_eq!(value["method"], "initialize");
    }

    #[test]
    fn classify_error_without_method() {
        let value = serde_json::json!({"error":{"code":-32600,"message":"Not initialized"},"id":1});
        match WireFrame::from_value(value) {
            WireFrame::Error(error) => {
                assert_eq!(error.id, RequestId::Integer(1));
                assert_eq!(error.error.code, -32600);
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn classify_notification_with_emitted_at() {
        let value = serde_json::json!({
            "method":"thread/status/changed",
            "params":{"threadId":"t","status":{"type":"idle"}},
            "emittedAtMs":1
        });
        match WireFrame::from_value(value) {
            WireFrame::Notification(notification) => {
                assert_eq!(notification.method, "thread/status/changed");
            }
            other => panic!("expected notification, got {other:?}"),
        }
    }

    #[test]
    fn integer_and_string_ids_do_not_collide() {
        assert_ne!(RequestId::Integer(1), RequestId::String("1".into()));
    }
}
