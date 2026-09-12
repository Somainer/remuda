//! Device-authenticated Hub HTTP/WS client.
//!
//! Bodies follow `crates/remuda-hub/openapi/openapi.json` (`InstanceCreate`,
//! `CommandRequest`, `JournalPage`, `/v1/follow`). This crate does not depend
//! on `remuda-hub` so the CLI and Feishu dispatcher can share it.

mod error;
mod types;

use std::sync::Mutex;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

pub use error::ClientError;
pub use types::{
    CommandRequest, InstanceCreate, InstanceCreateResult, InstanceRecord, JournalPage,
    LoginRequest, LoginResponse,
};

/// Device-authenticated JSON client for Hub `/v1/*` and `/v1/follow`.
pub struct HubClient {
    base: String,
    token: Mutex<Option<String>>,
    bootstrap: Option<String>,
    http: reqwest::Client,
}

impl HubClient {
    /// Build a client. `base` is `http://host:port` without a trailing slash.
    pub fn new(
        base: impl Into<String>,
        token: Option<String>,
        bootstrap: Option<String>,
    ) -> Result<Self, ClientError> {
        let base = base.into().trim_end_matches('/').to_string();
        let mut http = reqwest::Client::builder()
            .user_agent(format!("remuda-hub-client/{}", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30));
        if loopback_hub(&base) {
            http = http.no_proxy();
        }
        Ok(Self {
            base,
            token: Mutex::new(token.filter(|s| !s.is_empty())),
            bootstrap: bootstrap.filter(|s| !s.is_empty()),
            http: http.build()?,
        })
    }

    /// Hub base URL.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    /// `GET` JSON.
    pub async fn get(&self, path: &str) -> Result<Value, ClientError> {
        self.send(reqwest::Method::GET, path, None).await
    }

    /// `POST` JSON.
    pub async fn post(&self, path: &str, body: &Value) -> Result<Value, ClientError> {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }

    /// `GET /v1/hosts` → `items`.
    pub async fn list_hosts(&self) -> Result<Vec<Value>, ClientError> {
        let body = self.get("/v1/hosts").await?;
        Ok(body
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// `GET /v1/instances` → `items`.
    pub async fn list_instances(&self) -> Result<Vec<Value>, ClientError> {
        let body = self.get("/v1/instances").await?;
        Ok(body
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// `POST /v1/instances` with a raw JSON body (CLI).
    pub async fn create_instance(&self, body: &Value) -> Result<Value, ClientError> {
        self.post("/v1/instances", body).await
    }

    /// `POST /v1/instances` typed against openapi `InstanceCreate`.
    pub async fn create_instance_typed(
        &self,
        body: &InstanceCreate,
    ) -> Result<InstanceCreateResult, ClientError> {
        let value = serde_json::to_value(body)?;
        let raw = self.create_instance(&value).await?;
        Ok(serde_json::from_value(raw)?)
    }

    /// `POST /v1/instances/{id}/commands`.
    pub async fn post_command(
        &self,
        instance_id: &str,
        operation: &str,
        payload: Value,
        command_id: Option<&str>,
    ) -> Result<Value, ClientError> {
        self.post_command_keyed(instance_id, operation, payload, command_id, None)
            .await
    }

    /// `POST /v1/instances/{id}/commands` with an explicit `idempotencyKey`.
    pub async fn post_command_keyed(
        &self,
        instance_id: &str,
        operation: &str,
        payload: Value,
        command_id: Option<&str>,
        idempotency_key: Option<&str>,
    ) -> Result<Value, ClientError> {
        let mut body = json!({
            "operation": operation,
            "payload": payload,
        });
        if let Some(id) = command_id.filter(|s| !s.is_empty()) {
            body["commandId"] = json!(id);
        }
        if let Some(key) = idempotency_key.filter(|s| !s.is_empty()) {
            body["idempotencyKey"] = json!(key);
        }
        self.post(&format!("/v1/instances/{instance_id}/commands"), &body)
            .await
    }

    /// Typed command helper.
    pub async fn post_command_typed(
        &self,
        instance_id: &str,
        request: &CommandRequest,
    ) -> Result<Value, ClientError> {
        self.post_command(
            instance_id,
            &request.operation,
            request.payload.clone(),
            request.command_id.as_deref(),
        )
        .await
    }

    /// `GET /v1/instances/{id}/journal`.
    pub async fn get_journal(
        &self,
        instance_id: &str,
        after_seq: Option<&str>,
    ) -> Result<Value, ClientError> {
        let mut path = format!("/v1/instances/{instance_id}/journal");
        if let Some(seq) = after_seq.filter(|s| !s.is_empty()) {
            path.push_str("?afterSeq=");
            path.push_str(seq);
        }
        self.get(&path).await
    }

    /// Typed journal page.
    pub async fn get_journal_typed(
        &self,
        instance_id: &str,
        after_seq: Option<&str>,
    ) -> Result<JournalPage, ClientError> {
        let raw = self.get_journal(instance_id, after_seq).await?;
        Ok(serde_json::from_value(raw)?)
    }

    /// `POST /v1/fleet/instances`.
    pub async fn create_fleet(&self, body: &Value) -> Result<Value, ClientError> {
        self.post("/v1/fleet/instances", body).await
    }

    /// `POST /v1/fleet/broadcast` — Hub-side fan-out of one command.
    pub async fn fleet_broadcast(&self, body: &Value) -> Result<Value, ClientError> {
        self.post("/v1/fleet/broadcast", body).await
    }

    /// Device follow socket (`GET /v1/follow?instanceId=`).
    pub async fn follow_ws(&self, instance_id: &str) -> Result<FollowSocket, ClientError> {
        self.ensure_auth().await?;
        let token = self.current_token()?.ok_or(ClientError::NoCredentials)?;
        let ws_base = http_to_ws(&self.base);
        let url = format!("{ws_base}/v1/follow?instanceId={instance_id}");
        let mut req = url
            .into_client_request()
            .map_err(|err| ClientError::Websocket(err.to_string()))?;
        req.headers_mut().insert(
            "Authorization",
            format!("Bearer {token}").parse().map_err(
                |err: reqwest::header::InvalidHeaderValue| ClientError::Internal(err.to_string()),
            )?,
        );
        let (ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .map_err(|err| ClientError::Websocket(err.to_string()))?;
        Ok(FollowSocket { ws })
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, ClientError> {
        self.ensure_auth().await?;
        let url = format!("{}{path}", self.base);
        let mut req = self.http.request(method, &url);
        if let Some(token) = self.current_token()? {
            req = req.bearer_auth(token);
        }
        if let Some(body) = body {
            req = req.json(body);
        }
        let response = req.send().await?;
        let status = response.status();
        let text = response.text().await?;
        if is_fleet_path(path) && status.as_u16() == 404 {
            return Err(ClientError::FleetUnavailable {
                status: status.as_u16(),
                path: path.to_string(),
            });
        }
        if !status.is_success() {
            let body = if text.trim().is_empty() {
                status.canonical_reason().unwrap_or("error").to_string()
            } else {
                text
            };
            return Err(ClientError::Http {
                status: status.as_u16(),
                body,
            });
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&text)?)
    }

    async fn ensure_auth(&self) -> Result<(), ClientError> {
        if self.current_token()?.is_some() {
            return Ok(());
        }
        let Some(bootstrap) = self.bootstrap.as_deref() else {
            return Err(ClientError::NoCredentials);
        };
        let url = format!("{}/v1/login", self.base);
        let response = self
            .http
            .post(url)
            .json(&LoginRequest {
                bootstrap_token: bootstrap.to_string(),
                device_name: "remuda-hub-client".into(),
            })
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(ClientError::Http {
                status: status.as_u16(),
                body: text,
            });
        }
        let value: LoginResponse = serde_json::from_str(&text)?;
        let mut guard = self
            .token
            .lock()
            .map_err(|_| ClientError::Internal("token lock poisoned".into()))?;
        *guard = Some(value.token);
        Ok(())
    }

    fn current_token(&self) -> Result<Option<String>, ClientError> {
        self.token
            .lock()
            .map(|g| g.clone())
            .map_err(|_| ClientError::Internal("token lock poisoned".into()))
    }
}

/// Live `/v1/follow` connection.
pub struct FollowSocket {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl FollowSocket {
    /// Next JSON frame (`snapshot` or `event`).
    pub async fn next_json(&mut self) -> Result<Option<Value>, ClientError> {
        while let Some(frame) = self.ws.next().await {
            let frame = frame.map_err(|err| ClientError::Websocket(err.to_string()))?;
            match frame {
                Message::Text(text) => {
                    return Ok(Some(serde_json::from_str(text.as_ref())?));
                }
                Message::Binary(bytes) => {
                    return Ok(Some(serde_json::from_slice(&bytes)?));
                }
                Message::Close(_) => return Ok(None),
                _ => {}
            }
        }
        Ok(None)
    }

    /// Close the socket.
    pub async fn close(&mut self) -> Result<(), ClientError> {
        SinkExt::close(&mut self.ws)
            .await
            .map_err(|err| ClientError::Websocket(err.to_string()))
    }
}

/// Pick the lowest-load online host matching `labels` (empty = any online).
pub fn pick_host(hosts: &[Value], labels: &[String]) -> Result<String, ClientError> {
    let mut ranked: Vec<(i64, i64, &str)> = Vec::new();
    for host in hosts {
        let online = host.get("online").and_then(Value::as_bool).unwrap_or(false);
        if !online {
            continue;
        }
        if !labels.is_empty() && !host_matches_labels(host, labels) {
            continue;
        }
        let Some(id) = host.get("hostId").and_then(Value::as_str) else {
            continue;
        };
        let count = host
            .get("instanceCount")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let max = host
            .get("maxInstances")
            .and_then(Value::as_i64)
            .filter(|n| *n > 0)
            .unwrap_or(1);
        ranked.push((count, max, id));
    }
    ranked.sort_by(|a, b| (a.0 * b.1).cmp(&(b.0 * a.1)).then_with(|| a.2.cmp(b.2)));
    ranked.first().map(|row| row.2.to_string()).ok_or_else(|| {
        ClientError::Placement(if labels.is_empty() {
            "no online hosts".into()
        } else {
            format!("no online host matched labels {labels:?}")
        })
    })
}

/// True when every wanted label is present on the host (case-insensitive).
#[must_use]
pub fn host_matches_labels(host: &Value, wanted: &[String]) -> bool {
    let have = host_label_set(host);
    wanted
        .iter()
        .all(|want| have.iter().any(|h| h.eq_ignore_ascii_case(want)))
}

fn host_label_set(host: &Value) -> Vec<String> {
    let mut out = Vec::new();
    push_label_values(host.get("labels"), &mut out);
    if let Some(caps) = host.get("capabilities") {
        push_label_values(caps.get("labels"), &mut out);
    }
    out
}

fn push_label_values(value: Option<&Value>, out: &mut Vec<String>) {
    let Some(value) = value else {
        return;
    };
    if let Some(arr) = value.as_array() {
        for item in arr {
            if let Some(s) = item.as_str() {
                out.push(s.to_string());
            }
        }
        return;
    }
    if let Some(obj) = value.as_object() {
        for (key, val) in obj {
            if let Some(s) = val.as_str() {
                out.push(format!("{key}={s}"));
            } else if val.as_bool() == Some(true) {
                out.push(key.clone());
            }
        }
    }
}

fn loopback_hub(base: &str) -> bool {
    let rest = base
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    rest.starts_with("127.0.0.1") || rest.starts_with("localhost") || rest.starts_with("[::1]")
}

fn is_fleet_path(path: &str) -> bool {
    path.split('?')
        .next()
        .is_some_and(|p| p.starts_with("/v1/fleet"))
}

fn http_to_ws(base: &str) -> String {
    if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        format!("ws://{base}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(id: &str, online: bool, labels: &[&str], count: i64, max: i64) -> Value {
        json!({
            "hostId": id,
            "online": online,
            "labels": labels,
            "instanceCount": count,
            "maxInstances": max,
        })
    }

    #[test]
    fn pick_host_prefers_lower_load_and_label_match() {
        let hosts = vec![
            host("h1", true, &["region=sg"], 3, 4),
            host("h2", true, &["region=sg"], 0, 4),
            host("h3", false, &["region=sg"], 0, 4),
            host("h4", true, &["region=cn"], 0, 4),
        ];
        assert_eq!(
            pick_host(&hosts, &["region=sg".into()]).expect("host"),
            "h2"
        );
    }

    #[test]
    fn pick_host_reads_capability_label_map() {
        let hosts = vec![json!({
            "hostId": "hcap",
            "online": true,
            "labels": [],
            "capabilities": { "labels": { "region": "sg" } },
            "instanceCount": 0,
            "maxInstances": 1,
        })];
        assert_eq!(
            pick_host(&hosts, &["region=sg".into()]).expect("host"),
            "hcap"
        );
    }

    #[test]
    fn pick_host_unsatisfiable() {
        let hosts = vec![host("h1", true, &["region=cn"], 0, 1)];
        let err = pick_host(&hosts, &["region=sg".into()]).expect_err("unsatisfiable");
        assert!(matches!(err, ClientError::Placement(_)));
    }

    #[test]
    fn http_to_ws_rewrites_scheme() {
        assert_eq!(http_to_ws("http://127.0.0.1:9"), "ws://127.0.0.1:9");
        assert_eq!(http_to_ws("https://hub.example"), "wss://hub.example");
    }
}
