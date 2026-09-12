// Copyright 2025 Bloop AI Labs Ltd
// SPDX-License-Identifier: Apache-2.0
//
// Lifted from vibe-kanban crates/executors/src/executors/codex/jsonrpc.rs
// (git 4deb7eca8f381f7cbc1f9d15515a9ab8f8009053).
//
// Modifications for Remuda:
// - omit the `jsonrpc` field on encode (native stdio JSONL)
// - Remuda `WireError` instead of vibe-kanban `ExecutorError`
// - classify inbound into notifications / server requests / unknown
// - unbounded mpsc inbound stream instead of JsonRpcCallbacks
// - max NDJSON line length
// - skip `#` fixture comments and empty lines

//! JSON-RPC peer over a pair of async reader/writer streams.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::codec::{decode_line, is_skippable_line, line_to_str, read_capped_line};
use crate::error::WireError;
use crate::rpc::{
    Inbound, JsonRpcError, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, RequestId,
    WireFrame, encode_line,
};

enum PendingResponse {
    Result(Value),
    Error(JsonRpcError),
    Closed,
}

/// Bidirectional JSON-RPC peer for Codex app-server stdio.
#[derive(Clone)]
pub struct JsonRpcPeer {
    stdin: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<PendingResponse>>>>,
    id_counter: Arc<AtomicI64>,
}

impl JsonRpcPeer {
    /// Spawn the stdout reader. `inbound_tx` receives notifications and
    /// server→client requests.
    pub fn spawn<R, W>(
        stdin: W,
        stdout: R,
        inbound_tx: mpsc::UnboundedSender<Inbound>,
        max_line_bytes: usize,
    ) -> Self
    where
        R: AsyncBufRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let peer = Self {
            stdin: Arc::new(Mutex::new(Box::new(stdin))),
            pending: Arc::new(Mutex::new(HashMap::new())),
            id_counter: Arc::new(AtomicI64::new(1)),
        };
        let reader_peer = peer.clone();
        tokio::spawn(async move {
            read_loop(stdout, reader_peer, inbound_tx, max_line_bytes).await;
        });
        peer
    }

    /// Next integer request id.
    pub fn next_request_id(&self) -> RequestId {
        RequestId::Integer(self.id_counter.fetch_add(1, Ordering::Relaxed))
    }

    /// Write one JSON object plus newline, without a `jsonrpc` field.
    pub async fn send<T: Serialize + Sync>(&self, message: &T) -> Result<(), WireError> {
        let bytes = encode_line(message)?;
        let mut guard = self.stdin.lock().await;
        guard.write_all(&bytes).await.map_err(WireError::Io)?;
        guard.flush().await.map_err(WireError::Io)?;
        Ok(())
    }

    /// Send `{id, method, params}` and wait for `{id, result}` or `{id, error}`.
    pub async fn request<P, R>(&self, method: &str, params: P) -> Result<R, WireError>
    where
        P: Serialize + Sync,
        R: DeserializeOwned,
    {
        let id = self.next_request_id();
        let params_value = serde_json::to_value(&params)?;
        let frame = JsonRpcRequest {
            id: id.clone(),
            method: method.to_owned(),
            params: if params_value.is_null() {
                None
            } else {
                Some(params_value)
            },
        };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        if let Err(error) = self.send(&frame).await {
            self.pending.lock().await.remove(&frame.id);
            return Err(error);
        }
        match rx.await {
            Ok(PendingResponse::Result(value)) => Ok(serde_json::from_value(value)?),
            Ok(PendingResponse::Error(error)) => Err(WireError::rpc(
                method,
                error.error.code,
                error.error.message,
                error.error.data,
            )),
            Ok(PendingResponse::Closed) | Err(_) => Err(WireError::Closed(method.to_owned())),
        }
    }

    /// Send a notification (`initialized`, …).
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), WireError> {
        self.send(&JsonRpcNotification {
            method: method.to_owned(),
            params,
        })
        .await
    }

    /// Reply to a server→client request. Encodes `{id, result}` with no `method`.
    pub async fn reply_result<T: Serialize + Sync>(
        &self,
        id: RequestId,
        result: T,
    ) -> Result<(), WireError> {
        self.send(&JsonRpcResponse {
            id,
            result: serde_json::to_value(&result)?,
        })
        .await
    }

    /// Fail remaining in-flight client requests (child exit / reader EOF).
    pub async fn shutdown(&self) {
        let mut pending = self.pending.lock().await;
        for (_, sender) in pending.drain() {
            let _ = sender.send(PendingResponse::Closed);
        }
    }

    async fn resolve(&self, id: RequestId, response: PendingResponse) {
        if let Some(sender) = self.pending.lock().await.remove(&id) {
            let _ = sender.send(response);
        } else {
            tracing::debug!(%id, "codex app-server response for unknown request id");
        }
    }
}

async fn read_loop<R>(
    stdout: R,
    peer: JsonRpcPeer,
    inbound_tx: mpsc::UnboundedSender<Inbound>,
    max_line_bytes: usize,
) where
    R: AsyncBufRead + Unpin,
{
    let mut reader = stdout;
    loop {
        match read_capped_line(&mut reader, max_line_bytes).await {
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(?error, "codex app-server reader stopped");
                break;
            }
            Ok(Some(bytes)) => {
                let line = line_to_str(&bytes);
                if is_skippable_line(&line) {
                    continue;
                }
                match decode_line(&line) {
                    Ok(frame) => {
                        let raw: Value = match serde_json::from_str(&line) {
                            Ok(value) => value,
                            Err(_) => serde_json::Value::String(line.clone()),
                        };
                        match &frame {
                            WireFrame::Response(response) => {
                                peer.resolve(
                                    response.id.clone(),
                                    PendingResponse::Result(response.result.clone()),
                                )
                                .await;
                            }
                            WireFrame::Error(error) => {
                                peer.resolve(
                                    error.id.clone(),
                                    PendingResponse::Error(error.clone()),
                                )
                                .await;
                            }
                            WireFrame::Request(_)
                            | WireFrame::Notification(_)
                            | WireFrame::Unknown(_) => {
                                if let Some(inbound) = Inbound::from_frame(frame, raw)
                                    && inbound_tx.send(inbound).is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                    Err(_) => {
                        if inbound_tx.send(Inbound::NonJson(line)).is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }
    peer.shutdown().await;
}

/// Helper used by tests that already have a [`BufReader`].
pub fn spawn_buf_reader<R, W>(
    stdin: W,
    stdout: R,
    inbound_tx: mpsc::UnboundedSender<Inbound>,
    max_line_bytes: usize,
) -> JsonRpcPeer
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    JsonRpcPeer::spawn(stdin, BufReader::new(stdout), inbound_tx, max_line_bytes)
}
