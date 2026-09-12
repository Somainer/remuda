//! High-level ACP client over stdio (or any [`ConnectTo`] transport).

use agent_client_protocol::schema::v1::{
    CancelNotification, InitializeResponse, NewSessionRequest, NewSessionResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, SessionId,
};
use agent_client_protocol::{
    ActiveSession, Agent, ByteStreams, Client, ConnectTo, ConnectionTo, Handled, SessionMessage,
    UntypedMessage, on_receive_notification, on_receive_request,
};
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{debug, warn};

use crate::codec::classify_session_update;
use crate::error::Error;
use crate::spawn::{GrokChild, drain_stderr};
use crate::types::{
    CLIENT_NAME, InboundEvent, PromptTurn, SessionSpec, SpawnSpec, adapter_version,
    initialize_params,
};

/// Live connection to an ACP agent plus a side channel for `_x.ai/*` / unknown notifications.
pub struct AcpConn {
    inner: ConnectionTo<Agent>,
    inbound: mpsc::UnboundedReceiver<InboundEvent>,
}

impl AcpConn {
    /// Underlying SDK connection.
    #[must_use]
    pub fn inner(&self) -> &ConnectionTo<Agent> {
        &self.inner
    }

    /// `initialize` with protocolVersion 1 and empty `clientCapabilities`.
    pub async fn initialize(&self) -> Result<InitializeResponse, Error> {
        let params = initialize_params(CLIENT_NAME, adapter_version());
        debug!(params = %params, "acp initialize");
        let raw = self
            .inner
            .send_request(UntypedMessage::new("initialize", params)?)
            .block_task()
            .await?;
        Ok(serde_json::from_value(raw)?)
    }

    /// `session/new` and attach an [`AcpSession`].
    pub async fn new_session(&self, spec: SessionSpec) -> Result<AcpSession, Error> {
        let mut request = NewSessionRequest::new(&spec.cwd).mcp_servers(Vec::new());
        if let Some(meta) = spec.meta_map() {
            request = request.meta(meta);
        }
        let session = self
            .inner
            .build_session_from(request)
            .block_task()
            .start_session()
            .await?;
        Ok(AcpSession { inner: session })
    }

    /// `session/cancel` notification. The original `session/prompt` result carries `cancelled`.
    pub fn cancel(&self, session_id: impl Into<SessionId>) -> Result<(), Error> {
        self.inner
            .send_notification(CancelNotification::new(session_id))
            .map_err(Error::from)
    }

    /// Next connection-level inbound event (`_x.ai/*` or unknown). Does not include `session/update`.
    pub async fn next_inbound(&mut self) -> Option<InboundEvent> {
        self.inbound.recv().await
    }
}

/// Active ACP session from `session/new`.
pub struct AcpSession {
    inner: ActiveSession<'static, Agent>,
}

impl AcpSession {
    /// Native session id.
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        self.inner.session_id()
    }

    /// `session/new` response reconstructed from session state.
    #[must_use]
    pub fn response(&self) -> NewSessionResponse {
        self.inner.response()
    }

    /// SDK session handle.
    #[must_use]
    pub fn inner(&self) -> &ActiveSession<'static, Agent> {
        &self.inner
    }

    /// Mutable SDK session handle.
    pub fn inner_mut(&mut self) -> &mut ActiveSession<'static, Agent> {
        &mut self.inner
    }

    /// Send a text prompt and collect `session/update` until `stopReason`.
    pub async fn prompt(&mut self, text: impl ToString) -> Result<PromptTurn, Error> {
        self.inner.send_prompt(text).map_err(Error::from)?;
        self.collect_turn().await
    }

    async fn collect_turn(&mut self) -> Result<PromptTurn, Error> {
        let mut updates = Vec::new();
        loop {
            let message = self.inner.read_update().await.map_err(Error::from)?;
            match message {
                SessionMessage::StopReason(stop_reason) => {
                    return Ok(PromptTurn {
                        stop_reason,
                        updates,
                    });
                }
                SessionMessage::SessionMessage(dispatch) => {
                    if let Ok(msg) = dispatch.to_untyped_message()
                        && msg.method() == "session/update"
                    {
                        updates.push(classify_session_update(msg.params));
                    }
                }
                _ => {}
            }
        }
    }
}

/// Run `op` on a connected ACP client. `session/update` is left for [`AcpSession`];
/// `_x.ai/*` and unknown notifications are forwarded on [`AcpConn::next_inbound`].
pub async fn connect_transport<C, T>(
    transport: C,
    op: impl AsyncFnOnce(AcpConn) -> Result<T, Error>,
) -> Result<T, Error>
where
    C: ConnectTo<Client> + 'static,
{
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
    let (result_tx, mut result_rx) = tokio::sync::oneshot::channel();

    let connect = Client
        .builder()
        .name("runtime")
        .on_receive_notification(
            {
                let inbound_tx = inbound_tx.clone();
                async move |msg: UntypedMessage, cx| {
                    if msg.method() == "session/update" {
                        return Ok(Handled::No {
                            message: (msg, cx),
                            retry: true,
                        });
                    }
                    let event = if msg.method().starts_with('_') {
                        InboundEvent::Ext {
                            method: msg.method,
                            params: msg.params,
                        }
                    } else {
                        InboundEvent::Unknown {
                            method: msg.method,
                            params: msg.params,
                        }
                    };
                    if inbound_tx.send(event).is_err() {
                        debug!("inbound event dropped (receiver closed)");
                    }
                    Ok(Handled::Yes)
                }
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            async |request: RequestPermissionRequest, responder, _cx| {
                warn!(
                    session = %request.session_id,
                    "session/request_permission received; cancelling (no fs/terminal, always-approve path)"
                );
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ))
            },
            on_receive_request!(),
        )
        .connect_with(transport, async move |inner| {
            let conn = AcpConn {
                inner,
                inbound: inbound_rx,
            };
            let result = op(conn).await;
            let _ = result_tx.send(result);
            Ok(())
        });

    match connect.await {
        Ok(()) => result_rx.await.unwrap_or(Err(Error::TransportClosed)),
        Err(err) => {
            if let Ok(result) = result_rx.try_recv() {
                result
            } else {
                Err(Error::from(err))
            }
        }
    }
}

/// Spawn `grok agent … stdio` and run `op`.
pub async fn connect_stdio<T>(
    spec: SpawnSpec,
    op: impl AsyncFnOnce(AcpConn) -> Result<T, Error>,
) -> Result<T, Error> {
    let (mut child, stdin, stdout, stderr) = GrokChild::spawn(&spec)?;
    tokio::spawn(drain_stderr(stderr));
    let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
    let result = connect_transport(transport, op).await;
    child.kill().await;
    result
}

/// Connect over an already-split byte transport (tests / custom pipes).
pub async fn connect_byte_streams<T, W, R>(
    outgoing: W,
    incoming: R,
    op: impl AsyncFnOnce(AcpConn) -> Result<T, Error>,
) -> Result<T, Error>
where
    W: futures::AsyncWrite + Send + 'static,
    R: futures::AsyncRead + Send + 'static,
{
    connect_transport(ByteStreams::new(outgoing, incoming), op).await
}
