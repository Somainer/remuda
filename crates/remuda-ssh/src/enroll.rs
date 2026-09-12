//! Bridge `remuda node --stdio` NDJSON onto Hub `GET /v1/node`.
//!
//! Both sides speak the same JSON-RPC 2.0 Hub↔Node frames. Stdio may send
//! `node.auth` first (used as WS Bearer and not forwarded). Remaining frames
//! are copied as-is.

use serde_json::Value;

use crate::error::Error;
use crate::transport::{NodeTransport, StdioTransport, WssTransport};

/// How to reach a running Hub Node socket.
#[derive(Debug, Clone)]
pub struct HubEnroll {
    /// `ws://127.0.0.1:8080/v1/node` (see [`node_socket_url`]).
    pub hub_ws_url: String,
    /// Bootstrap or host token presented as `Authorization: Bearer`.
    pub bootstrap_token: String,
    /// Registry display name (`hosts[].label`); Node should already send this.
    pub display_label: String,
}

/// Outcome of the first hello exchange.
#[derive(Debug, Clone)]
pub struct EnrollResult {
    /// Hello frame forwarded to Hub.
    pub hello: Value,
    /// Host id Hub stored (from hello or Hub-assigned).
    pub host_id: String,
    /// JSON-RPC result from Hub, if it replied.
    pub hub_result: Option<Value>,
}

/// Map an HTTP(S) Hub base URL to the Node WebSocket path.
#[must_use]
pub fn node_socket_url(hub: &str) -> String {
    let trimmed = hub.trim().trim_end_matches('/');
    let ws = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        trimmed.to_string()
    } else {
        format!("ws://{trimmed}")
    };
    if ws.ends_with("/v1/node") || ws.ends_with("/node/v1/connect") {
        ws
    } else {
        format!("{ws}/v1/node")
    }
}

fn is_auth_frame(frame: &Value) -> bool {
    frame.get("method").and_then(Value::as_str) == Some("node.auth")
}

fn is_hello_frame(frame: &Value) -> bool {
    matches!(
        frame.get("method").and_then(Value::as_str),
        Some("node.hello") | Some("runtime.hello")
    )
}

fn token_from_auth(frame: &Value) -> Option<String> {
    frame
        .pointer("/params/token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
}

fn host_id_from_hello(hello: &Value) -> String {
    hello
        .pointer("/params/hostId")
        .and_then(Value::as_str)
        .or_else(|| hello.pointer("/params/host/hostId").and_then(Value::as_str))
        .unwrap_or("unknown")
        .to_string()
}

/// Read stdio frames, enroll on Hub `/v1/node`, forward Hub's JSON-RPC reply.
pub async fn enroll_stdio(
    stdio: &mut StdioTransport,
    hub: &HubEnroll,
) -> Result<(EnrollResult, WssTransport), Error> {
    let mut token = hub.bootstrap_token.clone();
    let hello = loop {
        let frame = stdio
            .recv_json()
            .await?
            .ok_or_else(|| Error::Enroll("stdio closed before node.hello".into()))?;
        if is_auth_frame(&frame) {
            if let Some(auth_token) = token_from_auth(&frame) {
                token = auth_token;
            }
            continue;
        }
        break frame;
    };
    if !is_hello_frame(&hello) {
        return Err(Error::Enroll(format!(
            "expected node.hello, got {}",
            hello
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("none")
        )));
    }
    if hello.get("id").is_none() {
        return Err(Error::Enroll(
            "node.hello is missing jsonrpc id; Node and Hub must speak the same codec".into(),
        ));
    }
    let host_id = host_id_from_hello(&hello);

    let mut ws = WssTransport::connect_with_bearer(&hub.hub_ws_url, Some(token.as_str())).await?;
    ws.send_json(&hello).await?;
    let hub_result =
        match tokio::time::timeout(std::time::Duration::from_secs(8), ws.recv_json()).await {
            Ok(Ok(value)) => value,
            Ok(Err(err)) => return Err(err),
            Err(_) => None,
        };
    if let Some(err) = hub_result.as_ref().and_then(|v| v.get("error")) {
        return Err(Error::Enroll(err.to_string()));
    }
    let host_id = hub_result
        .as_ref()
        .and_then(|v| v.pointer("/result/hostId"))
        .and_then(Value::as_str)
        .unwrap_or(host_id.as_str())
        .to_string();

    if let Some(result) = &hub_result {
        let _ = stdio.send_json(result).await;
    }

    Ok((
        EnrollResult {
            hello,
            host_id,
            hub_result,
        },
        ws,
    ))
}

/// Forward Node stdio ↔ Hub WS until either side closes.
pub async fn bridge_until_close(
    stdio: &mut StdioTransport,
    hub: &mut WssTransport,
) -> Result<(), Error> {
    loop {
        tokio::select! {
            from_node = stdio.recv_json() => {
                match from_node? {
                    None => return Ok(()),
                    Some(frame) => {
                        if is_auth_frame(&frame) {
                            continue;
                        }
                        hub.send_json(&frame).await?;
                    }
                }
            }
            from_hub = hub.recv_json() => {
                match from_hub? {
                    None => return Ok(()),
                    Some(frame) => {
                        let _ = stdio.send_json(&frame).await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn node_socket_url_from_http_base() {
        assert_eq!(
            node_socket_url("http://127.0.0.1:8080"),
            "ws://127.0.0.1:8080/v1/node"
        );
        assert_eq!(
            node_socket_url("ws://127.0.0.1:9/v1/node"),
            "ws://127.0.0.1:9/v1/node"
        );
    }

    #[test]
    fn hello_host_id_reads_nested_or_flat() {
        let nested = json!({
            "jsonrpc": "2.0",
            "id": "hello-1",
            "method": "node.hello",
            "params": {
                "host": { "hostId": "hst_01993ab0-0000-7000-8000-000000000004" }
            }
        });
        assert_eq!(
            host_id_from_hello(&nested),
            "hst_01993ab0-0000-7000-8000-000000000004"
        );
        assert!(is_hello_frame(&nested));
        assert!(is_auth_frame(&json!({
            "jsonrpc": "2.0",
            "id": "auth-1",
            "method": "node.auth",
            "params": { "token": "t" }
        })));
    }
}
