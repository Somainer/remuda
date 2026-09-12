//! Hub HTTP client used by `remuda instance`, `fleet`, and `mcp`.
//!
//! Talks to the Hub REST surface in `crates/remuda-hub` (see that crate's
//! README). Fleet routes are specified in `docs/design/proposal.md` §4.6 and
//! may not be deployed yet; callers map HTTP 404 on `/v1/fleet/*` to
//! [`ClientError::FleetUnavailable`].

use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use clap::Args;
use serde_json::{Value, json};
use thiserror::Error;

/// Shared `--hub` / `--token` / `--bootstrap-token` flags (`REMUDA_*` env).
#[derive(Debug, Clone, Args)]
pub(crate) struct HubOpts {
    /// Hub base URL. Defaults to `REMUDA_HUB` or `http://127.0.0.1:8080`.
    #[arg(long, global = true)]
    pub hub: Option<String>,
    /// Device bearer token. Defaults to `REMUDA_TOKEN`.
    #[arg(long, global = true)]
    pub token: Option<String>,
    /// Bootstrap token used to mint a device token (`REMUDA_BOOTSTRAP_TOKEN`).
    #[arg(long, global = true)]
    pub bootstrap_token: Option<String>,
}

impl HubOpts {
    fn hub_url(&self) -> String {
        self.hub
            .clone()
            .or_else(|| std::env::var("REMUDA_HUB").ok())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "http://127.0.0.1:8080".into())
            .trim_end_matches('/')
            .to_string()
    }

    fn token(&self) -> Option<String> {
        self.token
            .clone()
            .or_else(|| std::env::var("REMUDA_TOKEN").ok())
            .filter(|s| !s.is_empty())
    }

    fn bootstrap_token(&self) -> Option<String> {
        self.bootstrap_token
            .clone()
            .or_else(|| std::env::var("REMUDA_BOOTSTRAP_TOKEN").ok())
            .filter(|s| !s.is_empty())
    }
}

/// Failures talking to Hub HTTP.
#[derive(Debug, Error)]
pub(crate) enum ClientError {
    /// Neither a device token nor a bootstrap token was provided.
    #[error("set --token / REMUDA_TOKEN or --bootstrap-token / REMUDA_BOOTSTRAP_TOKEN")]
    NoCredentials,
    /// Hub returned a non-success status.
    #[error("hub HTTP {status}: {body}")]
    Http { status: u16, body: String },
    /// `POST /v1/fleet/*` is specified in proposal.md §4.6 but not on this Hub.
    #[error(
        "Hub fleet HTTP is not deployed yet (HTTP {status} on {path}). \
         TODO: POST /v1/fleet/instances, GET /v1/fleet/:id, POST /v1/fleet/:id/commands \
         as specified in docs/design/proposal.md §4.6"
    )]
    FleetUnavailable { status: u16, path: String },
    /// Client-side placement found no online host.
    #[error("PLACEMENT_UNSATISFIABLE: {0}")]
    Placement(String),
    /// Outbound HTTP.
    #[error("hub request: {0}")]
    Transport(#[from] reqwest::Error),
    /// Response was not JSON.
    #[error("hub JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// Internal lock / invariant.
    #[error("{0}")]
    Internal(String),
}

/// Device-authenticated JSON client for Hub `/v1/*`.
pub(crate) struct HubClient {
    base: String,
    token: Mutex<Option<String>>,
    bootstrap: Option<String>,
    http: reqwest::Client,
}

impl HubClient {
    /// Build a client from CLI flags and `REMUDA_*` environment variables.
    pub(crate) fn connect(opts: &HubOpts) -> Result<Self, ClientError> {
        Self::new(opts.hub_url(), opts.token(), opts.bootstrap_token())
    }

    fn new(
        base: String,
        token: Option<String>,
        bootstrap: Option<String>,
    ) -> Result<Self, ClientError> {
        let mut http = reqwest::Client::builder()
            .user_agent(format!("remuda/{}", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30));
        if loopback_hub(&base) {
            http = http.no_proxy();
        }
        let http = http.build()?;
        Ok(Self {
            base,
            token: Mutex::new(token),
            bootstrap,
            http,
        })
    }

    pub(crate) async fn get(&self, path: &str) -> Result<Value, ClientError> {
        self.send(reqwest::Method::GET, path, None).await
    }

    pub(crate) async fn post(&self, path: &str, body: &Value) -> Result<Value, ClientError> {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }

    pub(crate) async fn list_hosts(&self) -> Result<Vec<Value>, ClientError> {
        let body = self.get("/v1/hosts").await?;
        Ok(body
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    pub(crate) async fn list_instances(&self) -> Result<Vec<Value>, ClientError> {
        let body = self.get("/v1/instances").await?;
        Ok(body
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    pub(crate) async fn create_instance(&self, body: &Value) -> Result<Value, ClientError> {
        self.post("/v1/instances", body).await
    }

    pub(crate) async fn post_command(
        &self,
        instance_id: &str,
        operation: &str,
        payload: Value,
        command_id: Option<&str>,
    ) -> Result<Value, ClientError> {
        let mut body = json!({
            "operation": operation,
            "payload": payload,
        });
        if let Some(id) = command_id.filter(|s| !s.is_empty()) {
            body["commandId"] = json!(id);
        }
        self.post(&format!("/v1/instances/{instance_id}/commands"), &body)
            .await
    }

    pub(crate) async fn get_journal(
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

    pub(crate) async fn create_fleet(&self, body: &Value) -> Result<Value, ClientError> {
        self.post("/v1/fleet/instances", body).await
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
            .json(&json!({
                "bootstrapToken": bootstrap,
                "deviceName": "remuda-cli",
            }))
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
        let value: Value = serde_json::from_str(&text)?;
        let token = value
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| ClientError::Internal("login response missing token".into()))?
            .to_string();
        let mut guard = self
            .token
            .lock()
            .map_err(|_| ClientError::Internal("token lock poisoned".into()))?;
        *guard = Some(token);
        Ok(())
    }

    fn current_token(&self) -> Result<Option<String>, ClientError> {
        self.token
            .lock()
            .map(|g| g.clone())
            .map_err(|_| ClientError::Internal("token lock poisoned".into()))
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

/// Pick the lowest-load online host matching `labels` (empty = any online).
///
/// TODO: Hub placement (`docs/design/proposal.md` §4.6) should resolve
/// `placement.labels` / `placement.any` server-side. Until that ships, the CLI
/// still sends `hostId` because `POST /v1/instances` currently requires it.
pub(crate) fn pick_host(hosts: &[Value], labels: &[String]) -> Result<String, ClientError> {
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

pub(crate) fn host_matches_labels(host: &Value, wanted: &[String]) -> bool {
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

/// Convert `key=value` / bare-key CLI labels into a placement map.
#[cfg(test)]
pub(crate) fn labels_to_map(labels: &[String]) -> Value {
    let mut map = serde_json::Map::new();
    for label in labels {
        if let Some((key, value)) = label.split_once('=') {
            map.insert(key.to_string(), json!(value));
        } else {
            map.insert(label.clone(), json!(true));
        }
    }
    Value::Object(map)
}

pub(crate) fn print_json(value: &Value) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub(crate) fn block_on<T>(
    fut: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("tokio runtime")?
        .block_on(fut)
}

#[cfg(test)]
pub(crate) fn connect_for_test(base: String, token: String) -> Result<HubClient, ClientError> {
    HubClient::new(base, Some(token), None)
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
    fn labels_to_map_splits_key_value() {
        let value = labels_to_map(&["region=sg".into(), "gpu".into()]);
        assert_eq!(value["region"], json!("sg"));
        assert_eq!(value["gpu"], json!(true));
    }
}
