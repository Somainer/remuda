//! Typed Herdr JSON-RPC client.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Stream;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::debug;

use crate::error::Error;
use crate::events::{Event, EventsSubscribeParams, Subscription};
use crate::rpc::{Incoming, call, connect_subscribe, next_id, parse_line};
use crate::types::*;

/// Default RPC timeout for methods that do not wait on an agent turn.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Client for one Herdr API socket.
///
/// Ordinary methods open a short-lived Unix connection (herdrx pattern).
/// [`Self::subscribe`] holds a dedicated connection for the event stream.
#[derive(Debug, Clone)]
pub struct Client {
    socket_path: PathBuf,
    timeout: Duration,
    /// Optional `herdr --session` name, used when spawning terminal observe.
    session_name: Option<String>,
    /// `herdr` binary for terminal observe/control.
    binary: PathBuf,
}

impl Client {
    /// Bind a client to `socket_path`. Does not dial until the first call.
    #[must_use]
    pub fn connect(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            timeout: DEFAULT_TIMEOUT,
            session_name: None,
            binary: herdr_binary(),
        }
    }

    /// Override the RPC timeout (agent.wait uses max(this, timeout_ms + slack)).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Named session for `herdr --session` when opening a terminal observer.
    #[must_use]
    pub fn with_session_name(mut self, name: impl Into<String>) -> Self {
        self.session_name = Some(name.into());
        self
    }

    /// Override the `herdr` binary (default: `$HERDR_BINARY` or `herdr`).
    #[must_use]
    pub fn with_binary(mut self, binary: impl Into<PathBuf>) -> Self {
        self.binary = binary.into();
        self
    }

    /// API socket path.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Session name, if any.
    #[must_use]
    pub fn session_name(&self) -> Option<&str> {
        self.session_name.as_deref()
    }

    /// `herdr` binary used for terminal observe/control.
    #[must_use]
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// Raw RPC. `params` may be `null` (sent as `{}`).
    pub async fn call_raw(&self, method: &str, params: Value) -> Result<Value, Error> {
        call(&self.socket_path, method, params, self.timeout).await
    }

    async fn typed<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
        wanted: &'static str,
    ) -> Result<T, Error> {
        self.typed_timeout(method, params, wanted, self.timeout)
            .await
    }

    async fn typed_timeout<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
        wanted: &'static str,
        max_wait: Duration,
    ) -> Result<T, Error> {
        let value = call(&self.socket_path, method, params, max_wait).await?;
        let found = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("<missing>")
            .to_string();
        if found != wanted && wanted != "*" {
            return Err(Error::UnexpectedResult { wanted, found });
        }
        Ok(serde_json::from_value(value)?)
    }

    /// `ping`.
    pub async fn ping(&self) -> Result<Pong, Error> {
        self.typed("ping", json!({}), "pong").await
    }

    /// `workspace.create`.
    pub async fn workspace_create(
        &self,
        params: WorkspaceCreateParams,
    ) -> Result<WorkspaceCreated, Error> {
        self.typed(
            "workspace.create",
            serde_json::to_value(params)?,
            "workspace_created",
        )
        .await
    }

    /// `workspace.list`.
    pub async fn workspace_list(&self) -> Result<WorkspaceList, Error> {
        self.typed("workspace.list", json!({}), "workspace_list")
            .await
    }

    /// `tab.create`.
    pub async fn tab_create(&self, params: TabCreateParams) -> Result<TabCreated, Error> {
        self.typed("tab.create", serde_json::to_value(params)?, "tab_created")
            .await
    }

    /// `tab.list`.
    pub async fn tab_list(&self, params: TabListParams) -> Result<TabList, Error> {
        self.typed("tab.list", serde_json::to_value(params)?, "tab_list")
            .await
    }

    /// `tab.close`.
    pub async fn tab_close(&self, tab_id: impl Into<String>) -> Result<OkResult, Error> {
        self.typed(
            "tab.close",
            serde_json::to_value(TabTarget {
                tab_id: tab_id.into(),
            })?,
            "ok",
        )
        .await
    }

    /// `pane.split`.
    pub async fn pane_split(&self, params: PaneSplitParams) -> Result<PaneInfoResult, Error> {
        self.typed("pane.split", serde_json::to_value(params)?, "pane_info")
            .await
    }

    /// `pane.close`.
    pub async fn pane_close(&self, pane_id: impl Into<String>) -> Result<OkResult, Error> {
        self.typed(
            "pane.close",
            serde_json::to_value(PaneTarget {
                pane_id: pane_id.into(),
            })?,
            "ok",
        )
        .await
    }

    /// `pane.read`.
    pub async fn pane_read(&self, params: PaneReadParams) -> Result<PaneReadResult, Error> {
        self.typed("pane.read", serde_json::to_value(params)?, "*")
            .await
    }

    /// `pane.send_text`.
    pub async fn pane_send_text(
        &self,
        pane_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<OkResult, Error> {
        self.typed(
            "pane.send_text",
            serde_json::to_value(PaneSendTextParams {
                pane_id: pane_id.into(),
                text: text.into(),
            })?,
            "ok",
        )
        .await
    }

    /// `pane.send_keys`.
    pub async fn pane_send_keys(
        &self,
        pane_id: impl Into<String>,
        keys: Vec<String>,
    ) -> Result<OkResult, Error> {
        self.typed(
            "pane.send_keys",
            serde_json::to_value(PaneSendKeysParams {
                pane_id: pane_id.into(),
                keys,
            })?,
            "ok",
        )
        .await
    }

    /// `pane.process_info`.
    pub async fn pane_process_info(
        &self,
        pane_id: Option<String>,
    ) -> Result<PaneProcessInfoResult, Error> {
        self.typed(
            "pane.process_info",
            serde_json::to_value(PaneProcessInfoParams { pane_id })?,
            "pane_process_info",
        )
        .await
    }

    /// `agent.start`. Herdr does **not** persist `args` across restore.
    pub async fn agent_start(&self, params: AgentStartParams) -> Result<AgentStarted, Error> {
        let wait = params
            .timeout_ms
            .map(|ms| Duration::from_millis(ms.saturating_add(5_000)))
            .unwrap_or(self.timeout);
        self.typed_timeout(
            "agent.start",
            serde_json::to_value(params)?,
            "agent_started",
            wait.max(self.timeout),
        )
        .await
    }

    /// `agent.prompt`.
    pub async fn agent_prompt(&self, params: AgentPromptParams) -> Result<AgentInfoResult, Error> {
        let wait = params
            .wait
            .as_ref()
            .and_then(|wait| wait.timeout_ms)
            .map(|ms| Duration::from_millis(ms.saturating_add(5_000)))
            .unwrap_or(self.timeout);
        self.typed_timeout(
            "agent.prompt",
            serde_json::to_value(params)?,
            "agent_prompted",
            wait.max(self.timeout),
        )
        .await
    }

    /// `agent.wait`.
    pub async fn agent_wait(&self, params: AgentWaitParams) -> Result<AgentInfoResult, Error> {
        let wait = params
            .timeout_ms
            .map(|ms| Duration::from_millis(ms.saturating_add(5_000)))
            .unwrap_or(Duration::from_secs(120));
        self.typed_timeout(
            "agent.wait",
            serde_json::to_value(params)?,
            "agent_info",
            wait.max(self.timeout),
        )
        .await
    }

    /// `agent.read`.
    pub async fn agent_read(&self, params: AgentReadParams) -> Result<PaneReadResult, Error> {
        self.typed("agent.read", serde_json::to_value(params)?, "*")
            .await
    }

    /// `agent.send_keys`.
    pub async fn agent_send_keys(
        &self,
        target: impl Into<String>,
        keys: Vec<String>,
    ) -> Result<OkResult, Error> {
        self.typed(
            "agent.send_keys",
            serde_json::to_value(AgentSendKeysParams {
                target: target.into(),
                keys,
            })?,
            "ok",
        )
        .await
    }

    /// `agent.get`.
    pub async fn agent_get(&self, target: impl Into<String>) -> Result<AgentInfoResult, Error> {
        self.typed(
            "agent.get",
            serde_json::to_value(AgentTarget {
                target: target.into(),
            })?,
            "agent_info",
        )
        .await
    }

    /// `agent.list`.
    pub async fn agent_list(&self) -> Result<AgentList, Error> {
        self.typed("agent.list", json!({}), "agent_list").await
    }

    /// `session.snapshot`. Call **after** subscribe ack, then replay buffered events.
    pub async fn session_snapshot(&self) -> Result<SessionSnapshot, Error> {
        let result: SessionSnapshotResult = self
            .typed("session.snapshot", json!({}), "session_snapshot")
            .await?;
        Ok(result.snapshot)
    }

    /// `events.subscribe`. The returned stream stays open until dropped.
    ///
    /// `pane.agent_status_changed` requires `pane_id` per subscription. New
    /// panes need a new subscribe connection (Herdr does not append to an
    /// existing one).
    pub async fn subscribe(&self, subscriptions: Vec<Subscription>) -> Result<EventStream, Error> {
        let mut conn = connect_subscribe(&self.socket_path).await?;
        let request_id = next_id();
        let request = crate::rpc::RpcRequest {
            id: request_id.clone(),
            method: "events.subscribe".into(),
            params: serde_json::to_value(EventsSubscribeParams { subscriptions })?,
        };
        let mut encoded = serde_json::to_vec(&request)?;
        encoded.push(b'\n');
        conn.writer.write_all(&encoded).await?;
        conn.writer.flush().await?;

        let mut lines = conn.reader.lines();
        let mut buffered = Vec::new();
        loop {
            let line = lines
                .next_line()
                .await?
                .ok_or_else(|| Error::Disconnected {
                    socket: self.socket_path.clone(),
                })?;
            if line.trim().is_empty() {
                continue;
            }
            match parse_line(&line)? {
                Incoming::Response { id, result, error } if id == request_id || id.is_empty() => {
                    if let Some(error) = error {
                        return Err(Error::Api {
                            method: "events.subscribe".into(),
                            code: error.code,
                            message: error.message,
                        });
                    }
                    let kind = result
                        .as_ref()
                        .and_then(|value| value.get("type"))
                        .and_then(Value::as_str)
                        .unwrap_or("<missing>");
                    if kind != "subscription_started" {
                        return Err(Error::UnexpectedResult {
                            wanted: "subscription_started",
                            found: kind.to_string(),
                        });
                    }
                    break;
                }
                Incoming::Event(event) => buffered.push(event),
                Incoming::Response { .. } => continue,
            }
        }

        let (tx, rx) = mpsc::channel(64);
        for event in buffered {
            if tx.send(Ok(event)).await.is_err() {
                break;
            }
        }
        let socket = self.socket_path.clone();
        tokio::spawn(read_events(lines, tx, socket));
        Ok(EventStream {
            rx,
            _writer: conn.writer,
        })
    }
}

async fn read_events(
    mut lines: tokio::io::Lines<tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>>,
    tx: mpsc::Sender<Result<Event, Error>>,
    socket: PathBuf,
) {
    loop {
        match lines.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => continue,
            Ok(Some(line)) => match parse_line(&line) {
                Ok(Incoming::Event(event)) => {
                    debug!(name = %event.name, "herdr event");
                    if tx.send(Ok(event)).await.is_err() {
                        break;
                    }
                }
                Ok(Incoming::Response { .. }) => continue,
                Err(err) => {
                    let _ = tx.send(Err(err)).await;
                    break;
                }
            },
            Ok(None) => {
                let _ = tx
                    .send(Err(Error::Disconnected {
                        socket: socket.clone(),
                    }))
                    .await;
                break;
            }
            Err(err) => {
                let _ = tx.send(Err(Error::Io(err))).await;
                break;
            }
        }
    }
}

/// Stream of Herdr events from one subscribe connection.
pub struct EventStream {
    rx: mpsc::Receiver<Result<Event, Error>>,
    /// Held so the Unix socket is not half-closed while we read events.
    _writer: tokio::net::unix::OwnedWriteHalf,
}

impl EventStream {
    /// Next event, or `None` at end of stream.
    pub async fn next_event(&mut self) -> Option<Result<Event, Error>> {
        self.rx.recv().await
    }
}

impl Stream for EventStream {
    type Item = Result<Event, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

pub(crate) fn herdr_binary() -> PathBuf {
    std::env::var_os("HERDR_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("herdr"))
}

/// Resolve `$XDG_CONFIG_HOME/herdr` or `~/.config/herdr`.
#[must_use]
pub fn herdr_config_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("herdr");
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
    PathBuf::from(home).join(".config").join("herdr")
}

/// Default API socket (`~/.config/herdr/herdr.sock`).
#[must_use]
pub fn default_api_socket() -> PathBuf {
    herdr_config_dir().join("herdr.sock")
}

/// Named-session API + client sockets.
#[must_use]
pub fn session_sockets(session_name: &str) -> SocketPaths {
    let dir = herdr_config_dir().join("sessions").join(session_name);
    SocketPaths {
        api: dir.join("herdr.sock"),
        client: dir.join("herdr-client.sock"),
    }
}
