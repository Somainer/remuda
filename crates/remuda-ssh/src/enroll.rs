//! Bridge `remuda node --stdio` NDJSON onto Hub `GET /v1/node` JSON-RPC.
//!
//! Node stdio emits an unsolicited `node.hello` and then waits for
//! `{type: hub.hello}`. Hub `/v1/node` expects JSON-RPC with an `id` and a
//! Bearer bootstrap/host token. This module is the M1 translator; it does not
//! live in remuda-hub (stdio spawn stays with OpenSSH).

use serde_json::{Value, json};

use crate::error::Error;
use crate::transport::{NodeTransport, StdioTransport, WssTransport};

/// How to reach a running Hub Node socket.
#[derive(Debug, Clone)]
pub struct HubEnroll {
    /// `ws://127.0.0.1:8080/v1/node` (see [`node_socket_url`]).
    pub hub_ws_url: String,
    /// Bootstrap or host token presented as `Authorization: Bearer`.
    pub bootstrap_token: String,
    /// Registry display name (`hosts[].label`).
    pub display_label: String,
}

/// Outcome of the first hello exchange.
#[derive(Debug, Clone)]
pub struct EnrollResult {
    /// Adapted hello that was sent to Hub.
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

/// Lift nested inventory and mark the carrier as `ssh-stdio` for Hub registry.
#[must_use]
pub fn adapt_hello_for_hub(mut hello: Value, label: &str) -> Value {
    if hello.get("jsonrpc").is_none() {
        hello["jsonrpc"] = json!("2.0");
    }
    if hello.get("method").and_then(Value::as_str) != Some("node.hello")
        && hello.get("method").and_then(Value::as_str) != Some("runtime.hello")
    {
        hello["method"] = json!("node.hello");
    }
    hello["id"] = json!("hello-1");
    let params = hello
        .as_object_mut()
        .and_then(|obj| obj.get_mut("params"))
        .and_then(Value::as_object_mut);
    if let Some(params) = params {
        if !params.contains_key("hostId")
            && let Some(id) = params
                .get("host")
                .and_then(|host| host.get("hostId"))
                .cloned()
        {
            params.insert("hostId".into(), id);
        }
        params.insert("label".into(), json!(label));
        params.insert("transport".into(), json!("ssh-stdio"));
        if !params.contains_key("nodeVersion") {
            params.insert("nodeVersion".into(), json!("0.1.0"));
        }
    }
    hello
}

/// Read stdio `node.hello`, enroll on Hub `/v1/node`, ack the Node with `hub.hello`.
pub async fn enroll_stdio(
    stdio: &mut StdioTransport,
    hub: &HubEnroll,
) -> Result<(EnrollResult, WssTransport), Error> {
    let raw = stdio
        .recv_json()
        .await?
        .ok_or_else(|| Error::Enroll("stdio closed before node.hello".into()))?;
    let hello = adapt_hello_for_hub(raw, &hub.display_label);
    let host_id = hello
        .pointer("/params/hostId")
        .and_then(Value::as_str)
        .or_else(|| hello.pointer("/params/host/hostId").and_then(Value::as_str))
        .unwrap_or("unknown")
        .to_string();

    let mut ws =
        WssTransport::connect_with_bearer(&hub.hub_ws_url, Some(hub.bootstrap_token.as_str()))
            .await?;
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

    let ack = json!({
        "type": "hub.hello",
        "hostId": host_id,
        "carrier": "ssh-stdio",
    });
    let _ = stdio.send_json(&ack).await;

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
                    Some(frame) => hub.send_json(&frame).await?,
                }
            }
            from_hub = hub.recv_json() => {
                match from_hub? {
                    None => return Ok(()),
                    Some(frame) => {
                        let _ = stdio.send_json(&to_stdio_frame(frame)).await;
                    }
                }
            }
        }
    }
}

fn to_stdio_frame(frame: Value) -> Value {
    if frame.get("type").is_some() {
        return frame;
    }
    if frame.get("method").and_then(Value::as_str) == Some("hub.ping")
        || frame.get("result").is_some()
    {
        json!({
            "type": "hub.ping",
            "id": frame.get("id").cloned().unwrap_or(Value::Null),
        })
    } else {
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn adapt_hello_lifts_host_id_and_marks_stdio() {
        let raw = json!({
            "jsonrpc": "2.0",
            "method": "node.hello",
            "params": {
                "nodeEpoch": "epoch_1",
                "host": {
                    "hostId": "hst_01993ab0-0000-7000-8000-000000000004",
                    "hostname": "devbox",
                    "labels": { "region": "sg" }
                }
            }
        });
        let adapted = adapt_hello_for_hub(raw, "devbox-sg");
        assert_eq!(adapted["id"], "hello-1");
        assert_eq!(
            adapted["params"]["hostId"],
            "hst_01993ab0-0000-7000-8000-000000000004"
        );
        assert_eq!(adapted["params"]["label"], "devbox-sg");
        assert_eq!(adapted["params"]["transport"], "ssh-stdio");
    }
}
