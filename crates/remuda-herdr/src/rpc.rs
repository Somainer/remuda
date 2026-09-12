//! NDJSON JSON-RPC over a Unix domain socket.
//!
//! One request per connection for ordinary RPCs (same as herdrx). Subscribe
//! holds a dedicated connection because Herdr does not multiplex a subscribe
//! stream with later requests on that socket.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::timeout;
use tracing::debug;

use crate::error::Error;
use crate::events::Event;

/// Wire request. `method` and `params` are both required (empty params = `{}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    /// Correlation id.
    pub id: String,
    /// Method name (`ping`, `agent.start`, …).
    pub method: String,
    /// Params object.
    pub params: Value,
}

/// Herdr error body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcErrorBody {
    /// Code string.
    pub code: String,
    /// Message.
    pub message: String,
}

/// One NDJSON line: a response **or** an event. Events have no `id`.
#[derive(Debug, Clone)]
pub enum Incoming {
    /// JSON-RPC response.
    Response {
        /// Request id.
        id: String,
        /// Success payload.
        result: Option<Value>,
        /// Error payload.
        error: Option<RpcErrorBody>,
    },
    /// Push event (`{"event":"…","data":{…}}`).
    Event(Event),
}

/// Parse one NDJSON line. Exported for fixture tests.
pub fn parse_line(line: &str) -> Result<Incoming, Error> {
    let value: Value = serde_json::from_str(line)?;
    parse_value(value)
}

fn parse_value(value: Value) -> Result<Incoming, Error> {
    if let Some(event) = value.get("event").and_then(Value::as_str) {
        let data = value.get("data").cloned().unwrap_or(Value::Null);
        return Ok(Incoming::Event(Event::from_wire(event.to_string(), data)));
    }
    let id = value
        .get("id")
        .and_then(|id| {
            id.as_str()
                .map(ToOwned::to_owned)
                .or_else(|| id.as_i64().map(|n| n.to_string()))
                .or_else(|| id.as_u64().map(|n| n.to_string()))
        })
        .unwrap_or_default();
    let result = value.get("result").cloned();
    let error = value
        .get("error")
        .cloned()
        .and_then(|err| serde_json::from_value(err).ok());
    Ok(Incoming::Response { id, result, error })
}

pub(crate) fn next_id() -> String {
    format!("r-{}", uuid::Uuid::now_v7())
}

pub(crate) async fn call(
    socket: &Path,
    method: &str,
    params: Value,
    max_wait: Duration,
) -> Result<Value, Error> {
    let params = if params.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        params
    };
    let request = RpcRequest {
        id: next_id(),
        method: method.to_string(),
        params,
    };
    debug!(method, id = %request.id, socket = %socket.display(), "herdr rpc");
    timeout(max_wait, call_inner(socket, &request))
        .await
        .map_err(|_| Error::Timeout {
            method: method.to_string(),
            timeout: max_wait,
        })?
}

async fn call_inner(socket: &Path, request: &RpcRequest) -> Result<Value, Error> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|err| map_connect(socket, err))?;
    let (reader, mut writer) = stream.into_split();
    let mut encoded = serde_json::to_vec(request)?;
    encoded.push(b'\n');
    writer.write_all(&encoded).await?;
    writer.shutdown().await.ok();
    let mut lines = BufReader::new(reader).lines();
    loop {
        let line = match lines.next_line().await? {
            Some(line) => line,
            None => {
                return Err(Error::Disconnected {
                    socket: socket.to_path_buf(),
                });
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match parse_line(&line)? {
            Incoming::Response { id, result, error } if id == request.id || id.is_empty() => {
                if let Some(error) = error {
                    return Err(Error::Api {
                        method: request.method.clone(),
                        code: error.code,
                        message: error.message,
                    });
                }
                return Ok(result.unwrap_or(Value::Null));
            }
            Incoming::Response { .. } | Incoming::Event(_) => {
                // A leftover event on a one-shot socket is ignored.
                continue;
            }
        }
    }
}

fn map_connect(socket: &Path, err: std::io::Error) -> Error {
    match err.kind() {
        std::io::ErrorKind::NotFound
        | std::io::ErrorKind::ConnectionRefused
        | std::io::ErrorKind::BrokenPipe => Error::Disconnected {
            socket: socket.to_path_buf(),
        },
        _ => Error::Io(err),
    }
}

/// Long-lived subscribe connection.
pub(crate) struct SubscribeConn {
    pub(crate) reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    pub(crate) writer: tokio::net::unix::OwnedWriteHalf,
}

pub(crate) async fn connect_subscribe(socket: &Path) -> Result<SubscribeConn, Error> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|err| map_connect(socket, err))?;
    let (reader, writer) = stream.into_split();
    Ok(SubscribeConn {
        reader: BufReader::new(reader),
        writer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_events_from_responses() {
        let ping =
            parse_line(r#"{"id":"r1","result":{"type":"pong","version":"0.9.0","protocol":22}}"#)
                .unwrap();
        match ping {
            Incoming::Response { id, result, error } => {
                assert_eq!(id, "r1");
                assert!(error.is_none());
                assert_eq!(result.unwrap()["type"], "pong");
            }
            Incoming::Event(_) => panic!("expected response"),
        }

        let event = parse_line(
            r#"{"data":{"agent":"claude","agent_status":"working","pane_id":"w1:p2","workspace_id":"w1"},"event":"pane.agent_status_changed"}"#,
        )
        .unwrap();
        match event {
            Incoming::Event(event) => {
                assert_eq!(event.kind, crate::events::EventKind::PaneAgentStatusChanged);
                assert_eq!(event.pane_id(), Some("w1:p2"));
                assert_eq!(
                    event.agent_status(),
                    Some(crate::types::AgentStatus::Working)
                );
            }
            Incoming::Response { .. } => panic!("expected event"),
        }
    }

    #[test]
    fn parses_api_error() {
        let incoming = parse_line(
            r#"{"id":"","error":{"code":"invalid_request","message":"missing field `pane_id`"}}"#,
        )
        .unwrap();
        match incoming {
            Incoming::Response { error, .. } => {
                let error = error.expect("error");
                assert_eq!(error.code, "invalid_request");
            }
            Incoming::Event(_) => panic!("expected response"),
        }
    }
}
