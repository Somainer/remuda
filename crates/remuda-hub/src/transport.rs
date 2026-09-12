//! Hub↔Node RPC channels for outbound WSS and supervised SSH stdio.
//!
//! `ssh_hosts` owns the SSH process and attaches the same request/reply channel
//! used by the WebSocket dispatcher, with [`TransportKind::SshStdio`].

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

/// Live Node RPC channel, backed by a WebSocket or the SSH supervisor's writer.
pub struct WssTransport {
    kind: TransportKind,
    outbound: mpsc::Sender<Value>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
}

impl WssTransport {
    /// Pair with the WS writer task in `ws`.
    pub fn new(
        outbound: mpsc::Sender<Value>,
        pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    ) -> Self {
        Self {
            outbound,
            pending,
            kind: TransportKind::OutboundWss,
        }
    }
}

impl WssTransport {
    pub(crate) fn with_kind(mut self, kind: TransportKind) -> Self {
        self.kind = kind;
        self
    }
}

impl NodeTransport for WssTransport {
    fn kind(&self) -> TransportKind {
        self.kind
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
            {
                let mut pending = self.pending.lock().await;
                if pending.len() >= 32 {
                    return Err(HubError::Internal("too many in-flight node rpcs".into()));
                }
                pending.insert(rpc_id.clone(), tx);
            }
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

/// Legacy disconnected placeholder. Managed SSH uses the shared live RPC channel.
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

struct NodeSlot {
    generation: u64,
    link: Arc<dyn NodeTransport>,
}

/// Connected Node transports, keyed by host id.
#[derive(Clone, Default)]
pub struct ConnectedNodes {
    inner: Arc<Mutex<HashMap<String, NodeSlot>>>,
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
        let Some(link) = self
            .inner
            .lock()
            .await
            .get(host_id)
            .map(|slot| slot.link.clone())
        else {
            return Ok(None);
        };
        link.call(method, params, timeout).await
    }

    /// Record a live session. Returns a generation used to retire only this session.
    pub async fn insert(&self, host_id: String, link: Arc<dyn NodeTransport>) -> u64 {
        let mut inner = self.inner.lock().await;
        let generation = inner
            .get(&host_id)
            .map(|slot| slot.generation.wrapping_add(1))
            .unwrap_or(1);
        inner.insert(host_id, NodeSlot { generation, link });
        generation
    }

    /// Drop the live session (does not kill the native Node process).
    pub async fn remove(&self, host_id: &str) {
        self.inner.lock().await.remove(host_id);
    }

    /// Drop the session only if `generation` is still current (stale WS must not offline a live host).
    pub async fn remove_generation(&self, host_id: &str, generation: u64) -> bool {
        let mut inner = self.inner.lock().await;
        match inner.get(host_id) {
            Some(slot) if slot.generation == generation => {
                inner.remove(host_id);
                true
            }
            _ => false,
        }
    }

    /// Carrier for a connected host, if any.
    pub async fn kind_of(&self, host_id: &str) -> Option<TransportKind> {
        self.inner
            .lock()
            .await
            .get(host_id)
            .map(|slot| slot.link.kind())
    }

    /// Host ids with a live Node session.
    pub async fn host_ids(&self) -> Vec<String> {
        self.inner.lock().await.keys().cloned().collect()
    }
}
