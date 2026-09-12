//! NDJSON JSON-RPC codec and fixture-line classification.
//!
//! ACP v1 is one JSON-RPC object per line (not LSP `Content-Length`). Probe
//! captures wrap that object as `{t, dir, transport, rpc}`.

use serde_json::{Map, Value};

use crate::error::Error;
use crate::types::{CaptureMeta, Direction, SessionUpdateKind, TransportKind, WireEvent};

/// Soft cap for a single NDJSON line (~2 MiB). Fixture max is ~69 KiB.
pub const MAX_LINE_BYTES: usize = 2 * 1024 * 1024;

/// One decoded line: capture envelope, raw ACP RPC, or a non-ACP JSON object.
#[derive(Debug, Clone, PartialEq)]
pub enum ParsedLine {
    /// Probe capture `{t, dir, transport, rpc}`.
    Capture {
        /// Envelope metadata.
        meta: CaptureMeta,
        /// Inner JSON-RPC object.
        rpc: Value,
    },
    /// Bare JSON-RPC object (`method` / `result` / `error`).
    Rpc(Value),
    /// JSON object that is not ACP JSON-RPC (headless `streaming-json`).
    Other(Value),
}

impl ParsedLine {
    /// Inner JSON-RPC object when this line is ACP.
    #[must_use]
    pub fn rpc(&self) -> Option<&Value> {
        match self {
            Self::Capture { rpc, .. } | Self::Rpc(rpc) => Some(rpc),
            Self::Other(_) => None,
        }
    }

    /// Classify the RPC, or [`Error::NotAcp`] for `Other`.
    pub fn classify(&self) -> Result<WireEvent, Error> {
        match self.rpc() {
            Some(rpc) => classify_rpc(rpc),
            None => Err(Error::NotAcp),
        }
    }
}

/// Parse one NDJSON line. Empty / whitespace lines are an error.
pub fn parse_line(line: &str) -> Result<ParsedLine, Error> {
    let len = line.len();
    if len > MAX_LINE_BYTES {
        return Err(Error::LineTooLong {
            len,
            max: MAX_LINE_BYTES,
        });
    }
    let value: Value = serde_json::from_str(line)?;
    Ok(parse_value(value))
}

/// Parse an already-decoded JSON value.
#[must_use]
pub fn parse_value(value: Value) -> ParsedLine {
    if let Some(rpc) = value.get("rpc").cloned()
        && value.get("dir").is_some()
    {
        let meta = CaptureMeta {
            t: value.get("t").and_then(Value::as_f64),
            dir: Direction::parse(value.get("dir").and_then(Value::as_str)),
            transport: TransportKind::parse(value.get("transport").and_then(Value::as_str)),
        };
        return ParsedLine::Capture { meta, rpc };
    }
    if is_jsonrpc_object(&value) {
        ParsedLine::Rpc(value)
    } else {
        ParsedLine::Other(value)
    }
}

fn is_jsonrpc_object(value: &Value) -> bool {
    if !value.is_object() {
        return false;
    }
    if value.get("jsonrpc").and_then(Value::as_str) == Some("2.0") {
        return true;
    }
    value.get("method").and_then(Value::as_str).is_some()
        || value.get("result").is_some()
        || value.get("error").is_some()
}

/// Classify a JSON-RPC object into a [`WireEvent`].
pub fn classify_rpc(rpc: &Value) -> Result<WireEvent, Error> {
    if !rpc.is_object() {
        return Err(Error::NotAcp);
    }
    let id = rpc.get("id").cloned();
    if let Some(error) = rpc.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        return Ok(WireEvent::Error {
            id: id.unwrap_or(Value::Null),
            code,
            message,
            data: error.get("data").cloned(),
        });
    }
    if rpc.get("result").is_some() && rpc.get("method").is_none() {
        return Ok(WireEvent::Response {
            id: id.unwrap_or(Value::Null),
            result: rpc.get("result").cloned().unwrap_or(Value::Null),
        });
    }
    let method = rpc
        .get("method")
        .and_then(Value::as_str)
        .ok_or(Error::NotAcp)?;
    let params = rpc.get("params").cloned().unwrap_or(Value::Null);
    if method == "session/update" {
        return Ok(classify_session_update(params));
    }
    if is_ext_method(method) {
        let method = if method.starts_with("x.ai/") {
            format!("_{method}")
        } else {
            method.to_string()
        };
        return Ok(WireEvent::Ext { method, params, id });
    }
    if id.is_some() {
        Ok(WireEvent::Request {
            id: id.unwrap_or(Value::Null),
            method: method.to_string(),
            params,
        })
    } else {
        Ok(WireEvent::Unknown {
            method: method.to_string(),
            params,
            id: None,
        })
    }
}

fn is_ext_method(method: &str) -> bool {
    method.starts_with("_x.ai/") || method.starts_with("x.ai/") || method.starts_with('_')
}

/// Classify `session/update` params. Unknown tags become [`SessionUpdateKind::Unknown`].
#[must_use]
pub fn classify_session_update(params: Value) -> WireEvent {
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let meta = params.get("_meta").cloned();
    let update = params.get("update").cloned().unwrap_or(Value::Null);
    let tag = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("");
    WireEvent::SessionUpdate {
        session_id,
        update_kind: SessionUpdateKind::from_tag(tag),
        update,
        meta,
    }
}

/// Encode one JSON-RPC request line (without trailing newline).
pub fn encode_request(id: impl Into<Value>, method: &str, params: Value) -> Result<String, Error> {
    let mut obj = Map::new();
    obj.insert("jsonrpc".into(), Value::String("2.0".into()));
    obj.insert("id".into(), id.into());
    obj.insert("method".into(), Value::String(method.into()));
    obj.insert("params".into(), params);
    Ok(serde_json::to_string(&Value::Object(obj))?)
}

/// Encode one JSON-RPC notification line (without trailing newline).
pub fn encode_notification(method: &str, params: Value) -> Result<String, Error> {
    let mut obj = Map::new();
    obj.insert("jsonrpc".into(), Value::String("2.0".into()));
    obj.insert("method".into(), Value::String(method.into()));
    obj.insert("params".into(), params);
    Ok(serde_json::to_string(&Value::Object(obj))?)
}

/// Encode a JSON-RPC success response.
pub fn encode_response(id: impl Into<Value>, result: Value) -> Result<String, Error> {
    let mut obj = Map::new();
    obj.insert("jsonrpc".into(), Value::String("2.0".into()));
    obj.insert("id".into(), id.into());
    obj.insert("result".into(), result);
    Ok(serde_json::to_string(&Value::Object(obj))?)
}

/// Pull assistant text out of an `agent_message_chunk` update object.
#[must_use]
pub fn chunk_text(update: &Value) -> Option<&str> {
    update.pointer("/content/text").and_then(Value::as_str)
}

/// Tool title from a `tool_call` update, when present.
#[must_use]
pub fn tool_call_title(update: &Value) -> Option<&str> {
    update.get("title").and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{client_capabilities_declare_fs_or_terminal, initialize_params};

    #[test]
    fn initialize_params_do_not_declare_fs_or_terminal() {
        let params = initialize_params("runtime", "0.1.0");
        assert!(!client_capabilities_declare_fs_or_terminal(&params));
        assert_eq!(params["clientCapabilities"], serde_json::json!({}));
        let line = encode_request(1, "initialize", params).unwrap();
        assert!(!line.contains("readTextFile"));
        assert!(!line.contains("writeTextFile"));
        assert!(!line.contains("\"terminal\":true"));
    }

    #[test]
    fn capture_envelope_unwraps_rpc() {
        let line = r#"{"t":1.0,"dir":"c2a","transport":"stdio","rpc":{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}}"#;
        let parsed = parse_line(line).unwrap();
        match parsed {
            ParsedLine::Capture { meta, rpc } => {
                assert_eq!(meta.dir, Direction::C2a);
                assert_eq!(meta.transport, TransportKind::Stdio);
                assert_eq!(rpc["method"], "initialize");
            }
            other => panic!("expected capture, got {other:?}"),
        }
    }

    #[test]
    fn unknown_session_update_is_unknown_kind() {
        let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"hook_execution","hook":"pre_tool_use"}}}"#;
        let event = parse_line(line).unwrap().classify().unwrap();
        match event {
            WireEvent::SessionUpdate { update_kind, .. } => {
                assert_eq!(
                    update_kind,
                    SessionUpdateKind::Unknown("hook_execution".into())
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn plan_tag_is_typed() {
        let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"plan","entries":[{"content":"step","priority":"medium","status":"pending"}]}}}"#;
        let event = parse_line(line).unwrap().classify().unwrap();
        match event {
            WireEvent::SessionUpdate { update_kind, .. } => {
                assert_eq!(update_kind, SessionUpdateKind::Plan);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn xai_without_underscore_classifies_as_ext() {
        let line = r#"{"jsonrpc":"2.0","id":"1","method":"x.ai/fs/list","params":{}}"#;
        let event = parse_line(line).unwrap().classify().unwrap();
        match event {
            WireEvent::Ext { method, .. } => assert_eq!(method, "_x.ai/fs/list"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn line_too_long_errors() {
        let line = format!("{{\"k\":\"{}\"}}", "x".repeat(MAX_LINE_BYTES));
        match parse_line(&line) {
            Err(Error::LineTooLong { .. }) => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn headless_streaming_json_is_not_acp() {
        let line = r#"{"type":"text","data":"OK"}"#;
        match parse_line(line).unwrap() {
            ParsedLine::Other(v) => assert_eq!(v["type"], "text"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse_line(line).unwrap().classify(),
            Err(Error::NotAcp)
        ));
    }
}
