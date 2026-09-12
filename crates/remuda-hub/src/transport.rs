//! Hub↔Node carriers. WSS is implemented; SSH stdio plugs in later (D-013).
//!
//! `ssh <alias> remuda node --stdio` is the no-port path. This crate does not
//! spawn SSH; a later module inserts a [`NodeTransport`] that speaks JSON-RPC
//! over that pipe.

use crate::error::HubError;
use futures::Future;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot};

/// How a Node is attached to this Hub process.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransportKind {
    /// Node dials `WS /v1/node` (production outbound WSS).
    OutboundWss,
    /// Hub (or an operator) runs `ssh <host> remuda node --stdio`.
    SshStdio,
}

impl TransportKind {
    /// Wire / SQLite spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OutboundWss => "outbound-wss",
            Self::SshStdio => "ssh-stdio",
        }
    }

    /// Parse hello/heartbeat `transport` / `carrier` fields.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "outbound-wss" | "wss" => Some(Self::OutboundWss),
            "ssh-stdio" | "stdio" | "ssh" => Some(Self::SshStdio),
            _ => None,
        }
    }
}

/// JSON-RPC session to one enrolled Node, independent of the byte carrier.
pub trait NodeTransport: Send + Sync {
    /// Carrier advertised on the host registry.
    fn kind(&self) -> TransportKind;

    /// One request/response. `Ok(None)` means the session is not writable.
    fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Value>, HubError>> + Send + '_>>;
}

/// Live WSS Node (JSON text frames on `/v1/node`).
pub struct WssTransport {
    outbound: mpsc::Sender<Value>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
}

impl WssTransport {
    /// Pair with the WS writer task in `ws`.
    pub fn new(
        outbound: mpsc::Sender<Value>,
        pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    ) -> Self {
        Self { outbound, pending }
    }
}

impl NodeTransport for WssTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::OutboundWss
    }

    fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Value>, HubError>> + Send + '_>> {
        let method = method.to_string();
        Box::pin(async move {
            let rpc_id = uuid::Uuid::new_v4().to_string();
            let (tx, rx) = oneshot::channel();
            self.pending.lock().await.insert(rpc_id.clone(), tx);
            let frame = json!({
                "jsonrpc": "2.0",
                "id": rpc_id,
                "method": method,
                "params": params,
            });
            if self.outbound.send(frame).await.is_err() {
                self.pending.lock().await.remove(&rpc_id);
                return Ok(None);
            }
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(value)) => Ok(Some(value)),
                Ok(Err(_)) => Err(HubError::Internal("node rpc dropped".into())),
                Err(_) => {
                    self.pending.lock().await.remove(&rpc_id);
                    Err(HubError::Internal("node rpc timeout".into()))
                }
            }
        })
    }
}

/// Placeholder for `ssh <host> remuda node --stdio`. Not attached in this crate.
pub struct StdioTransport {
    host_id: String,
}

impl StdioTransport {
    /// Named host that will speak JSON-RPC on a stdio pipe once wired.
    pub fn pending(host_id: impl Into<String>) -> Self {
        Self {
            host_id: host_id.into(),
        }
    }

    /// Host this placeholder is reserved for.
    pub fn host_id(&self) -> &str {
        &self.host_id
    }
}

impl NodeTransport for StdioTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::SshStdio
    }

    fn call(
        &self,
        _method: &str,
        _params: Value,
        _timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Value>, HubError>> + Send + '_>> {
        Box::pin(async { Ok(None) })
    }
}

/// Connected Node transports, keyed by host id.
#[derive(Clone, Default)]
pub struct ConnectedNodes {
    inner: Arc<Mutex<HashMap<String, Arc<dyn NodeTransport>>>>,
}

impl ConnectedNodes {
    /// Call a JSON-RPC method on a connected Node. `Ok(None)` = not connected.
    pub async fn call(
        &self,
        host_id: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Option<Value>, HubError> {
        let Some(link) = self.inner.lock().await.get(host_id).cloned() else {
            return Ok(None);
        };
        link.call(method, params, timeout).await
    }

    /// Record a live session (WSS today; stdio later).
    pub async fn insert(&self, host_id: String, link: Arc<dyn NodeTransport>) {
        self.inner.lock().await.insert(host_id, link);
    }

    /// Drop the live session (does not kill the native Node process).
    pub async fn remove(&self, host_id: &str) {
        self.inner.lock().await.remove(host_id);
    }

    /// Carrier for a connected host, if any.
    pub async fn kind_of(&self, host_id: &str) -> Option<TransportKind> {
        self.inner.lock().await.get(host_id).map(|link| link.kind())
    }

    /// Host ids with a live Node session.
    pub async fn host_ids(&self) -> Vec<String> {
        self.inner.lock().await.keys().cloned().collect()
    }
}
