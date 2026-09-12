//! In-process and CLI fake for the Herdr JSON-RPC Unix socket.
//!
//! Speaks the same NDJSON as herdr 0.9.0 / protocol 22 for the subset
//! `remuda-herdr` uses. Result objects are serialized from `remuda_herdr`
//! types so the client can decode them.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use remuda_herdr::{
    AgentInfo, AgentInfoResult, AgentList, AgentSessionInfo, AgentSessionRefKind, AgentStartParams,
    AgentStarted, AgentStatus, AgentWaitParams, OkResult, PaneInfo, PaneInfoResult,
    PaneProcessInfo, PaneProcessInfoProcess, PaneProcessInfoResult, PaneReadBody, PaneReadResult,
    PaneScrollInfo, Pong, ReadFormat, ReadSource, ServerCapabilities, SessionSnapshot,
    SessionSnapshotResult, SplitDirection, SubscriptionStarted, TabCreated, TabInfo, TabList,
    WorkspaceCreated, WorkspaceInfo, WorkspaceList,
};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};

use crate::paths::{FIXED_SESSION_ID, fixtures_dir};

/// Failures while impersonating Herdr.
#[derive(Debug, Error)]
pub enum FakeHerdrError {
    /// Socket or stdio I/O.
    #[error("io: {0}")]
    Io(#[from] io::Error),
    /// JSON encode/decode.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Bad CLI flags.
    #[error("{0}")]
    Args(String),
}

/// Playback script.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FakeHerdrScript {
    /// `agent.start` goes idle; `agent.prompt` emits working → idle and `agent.read` contains `OK`.
    Ok,
    /// `agent.start` is blocked on a trust dialog until `agent.send_keys`.
    Trust,
    /// `agent.start` RPC succeeds, then the process is gone / pane is a shell (startup crash).
    StartFail,
    /// `agent.start` succeeds with launch still pending; becomes idle after a short delay.
    SlowStart,
}

impl FakeHerdrScript {
    /// Parse `ok` / `trust` / `start-fail` / `slow-start`.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim() {
            "ok" | "OK" => Some(Self::Ok),
            "trust" | "trust-dialog" | "blocked" => Some(Self::Trust),
            "start-fail" | "start_fail" | "crash" | "die" => Some(Self::StartFail),
            "slow-start" | "slow_start" | "slow" => Some(Self::SlowStart),
            _ => None,
        }
    }
}

/// How to bind the fake server.
#[derive(Clone, Debug)]
pub struct FakeHerdrOptions {
    /// Listen path.
    pub socket: PathBuf,
    /// State machine.
    pub script: FakeHerdrScript,
    /// JSONL of `terminal.frame` lines for `terminal observe`.
    pub frames: PathBuf,
    /// Deterministic delay before replying to `agent.start`.
    pub agent_start_delay: Duration,
}

impl FakeHerdrOptions {
    /// Default script and bundled frames, caller supplies the socket.
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            script: FakeHerdrScript::Ok,
            frames: herdr_frames_path(),
            agent_start_delay: Duration::ZERO,
        }
    }

    /// Delay `agent.start` to model a cold native launch.
    #[must_use]
    pub fn with_agent_start_delay(mut self, delay: Duration) -> Self {
        self.agent_start_delay = delay;
        self
    }
}

/// Bundled `terminal.frame` JSONL.
pub fn herdr_frames_path() -> PathBuf {
    fixtures_dir().join("herdr").join("terminal-observe.jsonl")
}

/// Captured live session (sanitized).
pub fn herdr_session_ok_path() -> PathBuf {
    fixtures_dir().join("herdr").join("session-ok.jsonl")
}

/// Path to the `fake-herdr` binary built for this package.
pub fn fake_herdr_bin() -> PathBuf {
    crate::ensure_workspace_bin("fake-herdr")
}

/// Background fake server. Dropping it unlinks the socket and joins the thread.
pub struct FakeHerdrServer {
    socket: PathBuf,
    shutdown: watch::Sender<bool>,
    join: Option<JoinHandle<Result<(), FakeHerdrError>>>,
}

impl FakeHerdrServer {
    /// Bind `options.socket` on a background thread.
    pub fn spawn(options: FakeHerdrOptions) -> Result<Self, FakeHerdrError> {
        if let Some(parent) = options.socket.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(&options.socket);
        let (shutdown, rx) = watch::channel(false);
        let socket = options.socket.clone();
        let join = std::thread::Builder::new()
            .name("fake-herdr".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                rt.block_on(serve(options, rx))
            })
            .map_err(FakeHerdrError::Io)?;
        wait_for_socket(&socket)?;
        Ok(Self {
            socket,
            shutdown,
            join: Some(join),
        })
    }

    /// Socket path.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// Signal shutdown and wait.
    pub fn shutdown(mut self) -> Result<(), FakeHerdrError> {
        let _ = self.shutdown.send(true);
        if let Some(join) = self.join.take() {
            match join.join() {
                Ok(result) => result,
                Err(_) => Err(FakeHerdrError::Args("fake-herdr thread panicked".into())),
            }
        } else {
            Ok(())
        }
    }
}

impl Drop for FakeHerdrServer {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn wait_for_socket(path: &Path) -> Result<(), FakeHerdrError> {
    for _ in 0..200 {
        if path.exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(FakeHerdrError::Args(format!(
        "fake-herdr socket not ready: {}",
        path.display()
    )))
}

/// CLI entry used by `fake-herdr`.
pub fn run_fake_herdr() -> Result<i32, FakeHerdrError> {
    match parse_args(std::env::args().skip(1).collect())? {
        Cli::Serve(options) => {
            let (shutdown, rx) = watch::channel(false);
            let _ = ctrlc_ignore(shutdown);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(serve(options, rx))?;
            Ok(0)
        }
        Cli::Observe { frames, cols, rows } => {
            write_observe_frames(&frames, cols, rows)?;
            Ok(0)
        }
        Cli::Help => {
            print_help();
            Ok(0)
        }
    }
}

fn ctrlc_ignore(_shutdown: watch::Sender<bool>) -> Result<(), FakeHerdrError> {
    Ok(())
}

enum Cli {
    Serve(FakeHerdrOptions),
    Observe {
        frames: PathBuf,
        cols: u16,
        rows: u16,
    },
    Help,
}

fn parse_args(args: Vec<String>) -> Result<Cli, FakeHerdrError> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(Cli::Help);
    }
    let mut socket = std::env::var("FAKE_HERDR_SOCKET")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("fake-herdr.sock"));
    let mut script = FakeHerdrScript::Ok;
    let mut frames = std::env::var("FAKE_HERDR_FRAMES")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(herdr_frames_path);
    let mut cols: u16 = 80;
    let mut rows: u16 = 24;
    let mut positional = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" => {
                i += 1;
                socket = PathBuf::from(
                    args.get(i)
                        .ok_or_else(|| FakeHerdrError::Args("missing --socket".into()))?,
                );
            }
            "--script" => {
                i += 1;
                let name = args
                    .get(i)
                    .ok_or_else(|| FakeHerdrError::Args("missing --script".into()))?;
                script = FakeHerdrScript::parse(name)
                    .ok_or_else(|| FakeHerdrError::Args(format!("unknown script {name}")))?;
            }
            "--frames" => {
                i += 1;
                frames = PathBuf::from(
                    args.get(i)
                        .ok_or_else(|| FakeHerdrError::Args("missing --frames".into()))?,
                );
            }
            "--cols" => {
                i += 1;
                cols = args
                    .get(i)
                    .ok_or_else(|| FakeHerdrError::Args("missing --cols".into()))?
                    .parse()
                    .map_err(|_| FakeHerdrError::Args("bad --cols".into()))?;
            }
            "--rows" => {
                i += 1;
                rows = args
                    .get(i)
                    .ok_or_else(|| FakeHerdrError::Args("missing --rows".into()))?
                    .parse()
                    .map_err(|_| FakeHerdrError::Args("bad --rows".into()))?;
            }
            "--session" | "--takeover" => {
                if args[i] == "--session" {
                    i += 1;
                }
            }
            other if other.starts_with('-') => {
                return Err(FakeHerdrError::Args(format!("unknown flag {other}")));
            }
            other => positional.push(other.to_string()),
        }
        i += 1;
    }
    if positional.first().map(String::as_str) == Some("terminal") {
        return Ok(Cli::Observe { frames, cols, rows });
    }
    if positional.first().map(String::as_str) == Some("serve") || positional.is_empty() {
        return Ok(Cli::Serve(FakeHerdrOptions {
            socket,
            script,
            frames,
            agent_start_delay: Duration::ZERO,
        }));
    }
    Err(FakeHerdrError::Args(format!(
        "unknown command {}",
        positional.join(" ")
    )))
}

fn print_help() {
    eprintln!(
        "fake-herdr — Herdr JSON-RPC test double\n\n\
         Usage:\n  fake-herdr [--socket PATH] [--script ok|trust|start-fail|slow-start] [--frames PATH]\n  \
         fake-herdr terminal [session] observe <pane> [--cols N] [--rows N] [--frames PATH]\n"
    );
}

/// Replay bundled or recorded `terminal.frame` JSONL to stdout.
pub fn write_observe_frames(frames: &Path, cols: u16, rows: u16) -> Result<(), FakeHerdrError> {
    let body = std::fs::read_to_string(frames)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut value: Value = serde_json::from_str(trimmed)?;
        if let Some(obj) = value.as_object_mut()
            && obj.get("type").and_then(Value::as_str) == Some("terminal.frame")
        {
            obj.insert("width".into(), json!(cols));
            obj.insert("height".into(), json!(rows));
        }
        writeln!(out, "{}", serde_json::to_string(&value)?)?;
    }
    out.flush()?;
    Ok(())
}

async fn serve(
    options: FakeHerdrOptions,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), FakeHerdrError> {
    let _ = std::fs::remove_file(&options.socket);
    if let Some(parent) = options.socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&options.socket)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&options.socket, std::fs::Permissions::from_mode(0o600));
    }
    eprintln!("api socket: {}", options.socket.display());
    let state = Arc::new(Mutex::new(State::new(
        options.script,
        options.agent_start_delay,
    )));
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(err) = handle_conn(stream, state).await {
                        tracing::debug!(error = %err, "fake-herdr connection ended");
                    }
                });
            }
        }
    }
    let _ = std::fs::remove_file(&options.socket);
    Ok(())
}

struct State {
    script: FakeHerdrScript,
    agent_start_delay: Duration,
    next_ws: u32,
    next_tab: u32,
    next_pane: u32,
    workspaces: HashMap<String, WorkspaceInfo>,
    tabs: HashMap<String, TabInfo>,
    panes: HashMap<String, PaneInfo>,
    agents: HashMap<String, AgentInfo>,
    screens: HashMap<String, String>,
    subscribers: Vec<mpsc::UnboundedSender<Value>>,
    dead_agents: HashSet<String>,
    shell_panes: HashSet<String>,
    slow_until: HashMap<String, Instant>,
}

impl State {
    fn new(script: FakeHerdrScript, agent_start_delay: Duration) -> Self {
        Self {
            script,
            agent_start_delay,
            next_ws: 1,
            next_tab: 1,
            next_pane: 1,
            workspaces: HashMap::new(),
            tabs: HashMap::new(),
            panes: HashMap::new(),
            agents: HashMap::new(),
            screens: HashMap::new(),
            subscribers: Vec::new(),
            dead_agents: HashSet::new(),
            shell_panes: HashSet::new(),
            slow_until: HashMap::new(),
        }
    }

    fn alloc_workspace(&mut self) -> String {
        let id = format!("w{}", self.next_ws);
        self.next_ws += 1;
        id
    }

    fn alloc_tab(&mut self, ws: &str) -> String {
        let id = format!("{ws}:t{}", self.next_tab);
        self.next_tab += 1;
        id
    }

    fn alloc_pane(&mut self, ws: &str) -> String {
        let id = format!("{ws}:p{}", self.next_pane);
        self.next_pane += 1;
        id
    }
}

#[derive(Deserialize)]
struct WireRequest {
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

fn request_id(id: &Value) -> String {
    id.as_str()
        .map(ToOwned::to_owned)
        .or_else(|| id.as_i64().map(|n| n.to_string()))
        .or_else(|| id.as_u64().map(|n| n.to_string()))
        .unwrap_or_default()
}

async fn handle_conn(stream: UnixStream, state: Arc<Mutex<State>>) -> Result<(), FakeHerdrError> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: WireRequest = serde_json::from_str(&line)?;
        if req.method == "events.subscribe" {
            handle_subscribe(&req, &mut writer, &state).await?;
            return Ok(());
        }
        if req.method == "server.stop" {
            let body = success(&req, serde_json::to_value(OkResult { kind: "ok".into() })?);
            writer.write_all(body.as_bytes()).await?;
            writer.flush().await?;
            return Ok(());
        }
        if req.method == "agent.start" {
            let delay = lock_state(&state)?.agent_start_delay;
            tokio::time::sleep(delay).await;
        }
        let reply = match handle_rpc(&state, &req) {
            Ok(value) => success(&req, value),
            Err((code, message)) => error_reply(&req, code, message),
        };
        writer.write_all(reply.as_bytes()).await?;
        writer.flush().await?;
    }
    Ok(())
}

async fn handle_subscribe(
    req: &WireRequest,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    state: &Arc<Mutex<State>>,
) -> Result<(), FakeHerdrError> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    {
        let mut st = lock_state(state)?;
        st.subscribers.push(tx);
    }
    let ack = success(
        req,
        serde_json::to_value(SubscriptionStarted {
            kind: "subscription_started".into(),
        })?,
    );
    writer.write_all(ack.as_bytes()).await?;
    writer.flush().await?;
    while let Some(event) = rx.recv().await {
        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');
        if writer.write_all(&line).await.is_err() {
            break;
        }
        if writer.flush().await.is_err() {
            break;
        }
    }
    Ok(())
}

fn lock_state(
    state: &Arc<Mutex<State>>,
) -> Result<std::sync::MutexGuard<'_, State>, FakeHerdrError> {
    state
        .lock()
        .map_err(|_| FakeHerdrError::Args("state poisoned".into()))
}

fn handle_rpc(
    state: &Arc<Mutex<State>>,
    req: &WireRequest,
) -> Result<Value, (&'static str, String)> {
    let mut st = state
        .lock()
        .map_err(|_| ("internal_error", "state poisoned".into()))?;
    match req.method.as_str() {
        "ping" => serde_json::to_value(pong()).map_err(json_err),
        "session.snapshot" => snapshot(&st),
        "workspace.create" => workspace_create(&mut st, &req.params),
        "workspace.list" => serde_json::to_value(WorkspaceList {
            kind: "workspace_list".into(),
            workspaces: st.workspaces.values().cloned().collect(),
        })
        .map_err(json_err),
        "tab.create" => tab_create(&mut st, &req.params),
        "tab.list" => serde_json::to_value(TabList {
            kind: "tab_list".into(),
            tabs: st.tabs.values().cloned().collect(),
        })
        .map_err(json_err),
        "tab.close" => tab_close(&mut st, &req.params),
        "pane.split" => pane_split(&mut st, &req.params),
        "pane.close" => pane_close(&mut st, &req.params),
        "pane.read" => pane_read(&st, &req.params),
        "pane.send_text" => pane_send_text(&mut st, &req.params),
        "pane.send_keys" => pane_send_keys(&mut st, &req.params),
        "pane.process_info" => pane_process_info(&mut st, &req.params),
        "agent.start" => agent_start(&mut st, &req.params),
        "agent.prompt" => agent_prompt(&mut st, &req.params),
        "agent.wait" => agent_wait_immediate(&mut st, &req.params),
        "agent.read" => agent_read(&mut st, &req.params),
        "agent.get" => agent_get(&mut st, &req.params),
        "agent.list" => serde_json::to_value(AgentList {
            kind: "agent_list".into(),
            agents: st.agents.values().cloned().collect(),
        })
        .map_err(json_err),
        "agent.send_keys" => agent_send_keys(&mut st, &req.params),
        other => Err(("invalid_request", format!("unknown method {other}"))),
    }
}

fn json_err(err: serde_json::Error) -> (&'static str, String) {
    ("internal_error", err.to_string())
}

fn pong() -> Pong {
    Pong {
        kind: "pong".into(),
        version: "0.9.0-fake".into(),
        protocol: 22,
        capabilities: Some(ServerCapabilities {
            live_handoff: true,
            detached_server_daemon: false,
            endpoint_protocol_generation: Some(1),
            surface_interest: true,
            health_check: true,
        }),
    }
}

fn snapshot(st: &State) -> Result<Value, (&'static str, String)> {
    serde_json::to_value(SessionSnapshotResult {
        kind: "session_snapshot".into(),
        snapshot: SessionSnapshot {
            version: "0.9.0-fake".into(),
            protocol: 22,
            focused_workspace_id: st.workspaces.keys().next().cloned(),
            focused_tab_id: st.tabs.keys().next().cloned(),
            focused_pane_id: st.panes.keys().next().cloned(),
            workspaces: st.workspaces.values().cloned().collect(),
            tabs: st.tabs.values().cloned().collect(),
            panes: st.panes.values().cloned().collect(),
            agents: st.agents.values().cloned().collect(),
        },
    })
    .map_err(json_err)
}

fn workspace_create(st: &mut State, params: &Value) -> Result<Value, (&'static str, String)> {
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or("/tmp/remuda-herdr")
        .to_string();
    let label = params
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or("workspace")
        .to_string();
    let ws_id = st.alloc_workspace();
    let tab_id = st.alloc_tab(&ws_id);
    let pane_id = st.alloc_pane(&ws_id);
    let pane = blank_pane(&ws_id, &tab_id, &pane_id, &cwd, true);
    let tab = TabInfo {
        tab_id: tab_id.clone(),
        workspace_id: ws_id.clone(),
        number: 1,
        label: "1".into(),
        focused: true,
        pane_count: 1,
        agent_status: AgentStatus::Unknown,
    };
    let workspace = WorkspaceInfo {
        workspace_id: ws_id.clone(),
        number: 1,
        label,
        focused: true,
        pane_count: 1,
        tab_count: 1,
        active_tab_id: tab_id.clone(),
        agent_status: AgentStatus::Unknown,
    };
    emit(
        st,
        "pane_created",
        json!({"type":"pane_created","pane": pane}),
    );
    st.screens.insert(pane_id.clone(), String::new());
    st.panes.insert(pane_id.clone(), pane.clone());
    st.tabs.insert(tab_id, tab.clone());
    st.workspaces.insert(ws_id, workspace.clone());
    serde_json::to_value(WorkspaceCreated {
        kind: "workspace_created".into(),
        workspace,
        tab,
        root_pane: pane,
    })
    .map_err(json_err)
}

fn tab_create(st: &mut State, params: &Value) -> Result<Value, (&'static str, String)> {
    let ws_id = params
        .get("workspace_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| st.workspaces.keys().next().cloned())
        .ok_or(("invalid_request", "no workspace".into()))?;
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or("/tmp/remuda-herdr")
        .to_string();
    let tab_id = st.alloc_tab(&ws_id);
    let pane_id = st.alloc_pane(&ws_id);
    let pane = blank_pane(&ws_id, &tab_id, &pane_id, &cwd, true);
    let tab = TabInfo {
        tab_id: tab_id.clone(),
        workspace_id: ws_id.clone(),
        number: st.tabs.len() + 1,
        label: params
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("tab")
            .to_string(),
        focused: false,
        pane_count: 1,
        agent_status: AgentStatus::Unknown,
    };
    st.screens.insert(pane_id.clone(), String::new());
    st.panes.insert(pane_id, pane.clone());
    st.tabs.insert(tab_id, tab.clone());
    if let Some(ws) = st.workspaces.get_mut(&ws_id) {
        ws.tab_count = st.tabs.values().filter(|t| t.workspace_id == ws_id).count();
        ws.pane_count = st
            .panes
            .values()
            .filter(|p| p.workspace_id == ws_id)
            .count();
    }
    serde_json::to_value(TabCreated {
        kind: "tab_created".into(),
        tab,
        root_pane: pane,
    })
    .map_err(json_err)
}

fn tab_close(st: &mut State, params: &Value) -> Result<Value, (&'static str, String)> {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or(("invalid_request", "missing tab_id".into()))?;
    st.tabs.remove(tab_id);
    let panes: Vec<String> = st
        .panes
        .values()
        .filter(|p| p.tab_id == tab_id)
        .map(|p| p.pane_id.clone())
        .collect();
    for pane_id in panes {
        remove_pane(st, &pane_id);
    }
    ok()
}

fn pane_split(st: &mut State, params: &Value) -> Result<Value, (&'static str, String)> {
    let _direction = params
        .get("direction")
        .and_then(Value::as_str)
        .and_then(|d| match d {
            "right" => Some(SplitDirection::Right),
            "down" => Some(SplitDirection::Down),
            _ => None,
        })
        .ok_or(("invalid_request", "missing direction".into()))?;
    let target = params
        .get("target_pane_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| st.panes.keys().next().cloned())
        .ok_or(("invalid_request", "no pane to split".into()))?;
    let parent = st
        .panes
        .get(&target)
        .cloned()
        .ok_or(("invalid_request", format!("unknown pane {target}")))?;
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| parent.cwd.clone())
        .unwrap_or_else(|| "/tmp/remuda-herdr".into());
    let pane_id = st.alloc_pane(&parent.workspace_id);
    let pane = blank_pane(&parent.workspace_id, &parent.tab_id, &pane_id, &cwd, false);
    emit(
        st,
        "pane_created",
        json!({"type":"pane_created","pane": pane}),
    );
    st.screens.insert(pane_id.clone(), String::new());
    st.panes.insert(pane_id, pane.clone());
    if let Some(tab) = st.tabs.get_mut(&parent.tab_id) {
        tab.pane_count = st
            .panes
            .values()
            .filter(|p| p.tab_id == parent.tab_id)
            .count();
    }
    serde_json::to_value(PaneInfoResult {
        kind: "pane_info".into(),
        pane,
    })
    .map_err(json_err)
}

fn pane_close(st: &mut State, params: &Value) -> Result<Value, (&'static str, String)> {
    let pane_id = params
        .get("pane_id")
        .and_then(Value::as_str)
        .ok_or(("invalid_request", "missing pane_id".into()))?;
    remove_pane(st, pane_id);
    emit(
        st,
        "pane_exited",
        json!({"type":"pane_exited","pane_id": pane_id, "workspace_id": "w1"}),
    );
    ok()
}

fn remove_pane(st: &mut State, pane_id: &str) {
    st.panes.remove(pane_id);
    st.screens.remove(pane_id);
    st.agents.retain(|_, agent| agent.pane_id != pane_id);
}

fn pane_read(st: &State, params: &Value) -> Result<Value, (&'static str, String)> {
    let pane_id = params
        .get("pane_id")
        .and_then(Value::as_str)
        .ok_or(("invalid_request", "missing pane_id".into()))?;
    read_pane(st, pane_id, source_of(params))
}

fn pane_send_text(st: &mut State, params: &Value) -> Result<Value, (&'static str, String)> {
    let pane_id = params
        .get("pane_id")
        .and_then(Value::as_str)
        .ok_or(("invalid_request", "missing pane_id".into()))?;
    let text = params.get("text").and_then(Value::as_str).unwrap_or("");
    st.screens
        .entry(pane_id.to_string())
        .or_default()
        .push_str(text);
    ok()
}

fn pane_send_keys(st: &mut State, params: &Value) -> Result<Value, (&'static str, String)> {
    let pane_id = params
        .get("pane_id")
        .and_then(Value::as_str)
        .ok_or(("invalid_request", "missing pane_id".into()))?;
    if let Some(name) = st
        .agents
        .iter()
        .find(|(_, a)| a.pane_id == pane_id)
        .map(|(n, _)| n.clone())
    {
        unblock_if_needed(st, &name);
    }
    ok()
}

fn pane_process_info(st: &mut State, params: &Value) -> Result<Value, (&'static str, String)> {
    promote_slow_agents(st);
    let pane_id = params
        .get("pane_id")
        .and_then(Value::as_str)
        .or_else(|| st.panes.keys().next().map(String::as_str))
        .ok_or(("invalid_request", "missing pane_id".into()))?
        .to_string();
    let foreground_processes = if st.shell_panes.contains(&pane_id) {
        vec![PaneProcessInfoProcess {
            pid: 4242,
            name: "zsh".into(),
            argv0: Some("zsh".into()),
            argv: Some(vec!["zsh".into()]),
        }]
    } else if pane_is_slow_starting(st, &pane_id) {
        vec![PaneProcessInfoProcess {
            pid: 4243,
            name: "grok".into(),
            argv0: Some("grok".into()),
            argv: Some(vec!["grok".into()]),
        }]
    } else {
        vec![PaneProcessInfoProcess {
            pid: 4242,
            name: "fake-herdr".into(),
            argv0: Some("fake-herdr".into()),
            argv: Some(vec!["fake-herdr".into()]),
        }]
    };
    serde_json::to_value(PaneProcessInfoResult {
        kind: "pane_process_info".into(),
        process_info: Some(PaneProcessInfo {
            pane_id,
            shell_pid: Some(4242),
            foreground_processes,
        }),
    })
    .map_err(json_err)
}

fn agent_start(st: &mut State, params: &Value) -> Result<Value, (&'static str, String)> {
    let start: AgentStartParams = serde_json::from_value(params.clone())
        .map_err(|err| ("invalid_request", err.to_string()))?;
    if !st.panes.contains_key(&start.pane_id) {
        return Err(("invalid_request", format!("unknown pane {}", start.pane_id)));
    }
    let blocked = st.script == FakeHerdrScript::Trust;
    let slow = st.script == FakeHerdrScript::SlowStart;
    let status = if blocked {
        AgentStatus::Blocked
    } else if slow {
        AgentStatus::Unknown
    } else {
        AgentStatus::Idle
    };
    let screen = if blocked {
        TRUST_DIALOG.to_string()
    } else {
        idle_screen("")
    };
    let agent = AgentInfo {
        terminal_id: format!("term_{}", start.pane_id.replace(':', "")),
        name: Some(start.name.clone()),
        agent: Some(start.kind.clone()),
        agent_status: status,
        workspace_id: st
            .panes
            .get(&start.pane_id)
            .map(|p| p.workspace_id.clone())
            .unwrap_or_else(|| "w1".into()),
        tab_id: st
            .panes
            .get(&start.pane_id)
            .map(|p| p.tab_id.clone())
            .unwrap_or_else(|| "w1:t1".into()),
        pane_id: start.pane_id.clone(),
        focused: false,
        agent_session: if blocked {
            None
        } else {
            Some(session_ref(&start.kind))
        },
        interactive_ready: !blocked && !slow,
        launch_pending: blocked || slow,
        cwd: st.panes.get(&start.pane_id).and_then(|p| p.cwd.clone()),
        terminal_title: Some("Claude Code".into()),
        terminal_title_stripped: Some("Claude Code".into()),
        state_change_seq: 1,
        revision: 1,
    };
    st.screens.insert(start.pane_id.clone(), screen);
    if let Some(pane) = st.panes.get_mut(&start.pane_id) {
        pane.agent = Some(start.kind.clone());
        pane.agent_status = status;
        pane.revision = 1;
    }
    st.agents.insert(start.name.clone(), agent.clone());
    emit_status(st, &agent);
    if blocked {
        return Err((
            "agent_not_ready",
            format!(
                "agent {} is blocked during startup and is not ready for prompts",
                start.name
            ),
        ));
    }
    if st.script == FakeHerdrScript::StartFail {
        mark_agent_crashed(st, &start.name, &start.pane_id, &start.args);
    }
    if st.script == FakeHerdrScript::SlowStart {
        mark_agent_slow(st, &start.name);
    }
    let mut argv = vec![start.kind.clone()];
    argv.extend(start.args);
    serde_json::to_value(AgentStarted {
        kind: "agent_started".into(),
        agent,
        argv,
    })
    .map_err(json_err)
}

fn mark_agent_slow(st: &mut State, name: &str) {
    st.slow_until.insert(
        name.to_string(),
        Instant::now() + Duration::from_millis(750),
    );
}

fn pane_is_slow_starting(st: &State, pane_id: &str) -> bool {
    let now = Instant::now();
    st.agents.values().any(|agent| {
        agent.pane_id == pane_id
            && agent
                .name
                .as_ref()
                .is_some_and(|name| st.slow_until.get(name).is_some_and(|until| now < *until))
    })
}

fn promote_slow_agents(st: &mut State) {
    let now = Instant::now();
    let ready: Vec<String> = st
        .slow_until
        .iter()
        .filter(|(_, until)| now >= **until)
        .map(|(name, _)| name.clone())
        .collect();
    for name in ready {
        st.slow_until.remove(&name);
        set_status(st, &name, AgentStatus::Idle, &idle_screen(""));
        if let Some(agent) = st.agents.get_mut(&name) {
            agent.interactive_ready = true;
            agent.launch_pending = false;
        }
    }
}

fn mark_agent_crashed(st: &mut State, name: &str, pane_id: &str, args: &[String]) {
    st.dead_agents.insert(name.to_string());
    st.shell_panes.insert(pane_id.to_string());
    st.screens.insert(pane_id.to_string(), crash_screen(args));
    if let Some(agent) = st.agents.get_mut(name) {
        agent.interactive_ready = false;
        agent.launch_pending = false;
        agent.agent_status = AgentStatus::Unknown;
        agent.agent_session = None;
    }
    if let Some(pane) = st.panes.get_mut(pane_id) {
        pane.agent_status = AgentStatus::Unknown;
        pane.agent = None;
    }
}

fn crash_screen(args: &[String]) -> String {
    if let Some(bin) = args.iter().find(|token| {
        let path = std::path::Path::new(token.as_str());
        path.is_file()
    }) && let Ok(output) = std::process::Command::new(bin)
        .args(args.iter().filter(|token| token.as_str() != bin.as_str()))
        .output()
    {
        let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stdout));
        if !text.ends_with('\n') {
            text.push('\n');
        }
        if !text.contains('%') {
            text.push('%');
            text.push('\n');
        }
        return text;
    }
    "error: unexpected argument '--name' found\nUsage: grok [OPTIONS] [PROMPT]\n%\n".into()
}

fn agent_prompt(st: &mut State, params: &Value) -> Result<Value, (&'static str, String)> {
    let target = params
        .get("target")
        .and_then(Value::as_str)
        .ok_or(("invalid_request", "missing target".into()))?;
    let text = params.get("text").and_then(Value::as_str).unwrap_or("");
    let name = resolve_agent(st, target)?;
    if st.dead_agents.contains(&name) {
        return Err((
            "agent_not_ready",
            format!("agent {name} exited during startup and is not ready for prompts"),
        ));
    }
    {
        let agent = st
            .agents
            .get(&name)
            .ok_or(("invalid_request", format!("unknown agent {name}")))?;
        if agent.agent_status == AgentStatus::Blocked {
            return Err((
                "agent_not_ready",
                format!("agent {name} is blocked and is not ready for prompts"),
            ));
        }
    }
    set_status(st, &name, AgentStatus::Working, &format!("working\n{text}"));
    set_status(st, &name, AgentStatus::Idle, &idle_screen("OK"));
    let agent = st.agents[&name].clone();
    serde_json::to_value(AgentInfoResult {
        kind: "agent_prompted".into(),
        agent,
    })
    .map_err(json_err)
}

fn agent_wait_immediate(st: &mut State, params: &Value) -> Result<Value, (&'static str, String)> {
    promote_slow_agents(st);
    let wait: AgentWaitParams = serde_json::from_value(params.clone())
        .map_err(|err| ("invalid_request", err.to_string()))?;
    let name = resolve_agent(st, &wait.target)?;
    let until = if wait.until.is_empty() {
        vec![AgentStatus::Idle, AgentStatus::Done]
    } else {
        wait.until
    };
    let agent = st
        .agents
        .get(&name)
        .cloned()
        .ok_or(("invalid_request", format!("unknown agent {name}")))?;
    if until.contains(&agent.agent_status)
        || (until.contains(&AgentStatus::Done) && agent.agent_status == AgentStatus::Idle)
    {
        return serde_json::to_value(AgentInfoResult {
            kind: "agent_info".into(),
            agent,
        })
        .map_err(json_err);
    }
    Err((
        "timeout",
        format!(
            "agent {name} still {:?} (wait is synchronous in fake-herdr unless already matching)",
            agent.agent_status
        ),
    ))
}

fn agent_read(st: &mut State, params: &Value) -> Result<Value, (&'static str, String)> {
    promote_slow_agents(st);
    let target = params
        .get("target")
        .and_then(Value::as_str)
        .ok_or(("invalid_request", "missing target".into()))?;
    let name = resolve_agent(st, target)?;
    let pane_id = st.agents[&name].pane_id.clone();
    read_pane(st, &pane_id, source_of(params))
}

fn agent_get(st: &mut State, params: &Value) -> Result<Value, (&'static str, String)> {
    promote_slow_agents(st);
    let target = params
        .get("target")
        .and_then(Value::as_str)
        .ok_or(("invalid_request", "missing target".into()))?;
    let name = resolve_agent(st, target)?;
    if st.dead_agents.contains(&name) {
        return Err((
            "agent_not_ready",
            format!("agent {name} exited during startup and is not ready"),
        ));
    }
    serde_json::to_value(AgentInfoResult {
        kind: "agent_info".into(),
        agent: st.agents[&name].clone(),
    })
    .map_err(json_err)
}

fn agent_send_keys(st: &mut State, params: &Value) -> Result<Value, (&'static str, String)> {
    let target = params
        .get("target")
        .and_then(Value::as_str)
        .ok_or(("invalid_request", "missing target".into()))?;
    let name = resolve_agent(st, target)?;
    let keys: Vec<String> = params
        .get("keys")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if let Some(agent) = st.agents.get(&name) {
        let pane = agent.pane_id.clone();
        let line = format!("KEYS {}", keys.join(" "));
        st.screens
            .entry(pane)
            .and_modify(|screen| {
                screen.push('\n');
                screen.push_str(&line);
            })
            .or_insert(line);
    }
    unblock_if_needed(st, &name);
    ok()
}

fn unblock_if_needed(st: &mut State, name: &str) {
    let blocked = st
        .agents
        .get(name)
        .is_some_and(|a| a.agent_status == AgentStatus::Blocked);
    if blocked {
        set_status(st, name, AgentStatus::Idle, &idle_screen(""));
        if let Some(agent) = st.agents.get_mut(name) {
            agent.launch_pending = false;
            agent.interactive_ready = true;
            agent.agent_session = Some(session_ref(agent.agent.as_deref().unwrap_or("claude")));
        }
    }
}

fn set_status(st: &mut State, name: &str, status: AgentStatus, screen: &str) {
    let Some(mut agent) = st.agents.get(name).cloned() else {
        return;
    };
    agent.agent_status = status;
    agent.state_change_seq = agent.state_change_seq.saturating_add(1);
    agent.revision = agent.revision.saturating_add(1);
    agent.interactive_ready = status == AgentStatus::Idle;
    st.screens.insert(agent.pane_id.clone(), screen.to_string());
    if let Some(pane) = st.panes.get_mut(&agent.pane_id) {
        pane.agent_status = status;
        pane.revision = agent.revision;
        pane.agent = agent.agent.clone();
    }
    st.agents.insert(name.to_string(), agent.clone());
    emit_status(st, &agent);
}

fn emit_status(st: &mut State, agent: &AgentInfo) {
    emit(
        st,
        "pane.agent_status_changed",
        json!({
            "type": "pane_agent_status_changed",
            "agent": agent.agent,
            "agent_status": agent.agent_status,
            "pane_id": agent.pane_id,
            "workspace_id": agent.workspace_id,
            "title": agent.terminal_title_stripped,
        }),
    );
}

fn emit(st: &mut State, event: &str, data: Value) {
    let envelope = json!({"event": event, "data": data});
    st.subscribers
        .retain(|tx| tx.send(envelope.clone()).is_ok());
}

fn resolve_agent(st: &State, target: &str) -> Result<String, (&'static str, String)> {
    if st.agents.contains_key(target) {
        return Ok(target.to_string());
    }
    st.agents
        .iter()
        .find(|(_, agent)| agent.pane_id == target)
        .map(|(name, _)| name.clone())
        .ok_or(("invalid_request", format!("unknown agent {target}")))
}

fn read_pane(
    st: &State,
    pane_id: &str,
    source: ReadSource,
) -> Result<Value, (&'static str, String)> {
    let pane = st
        .panes
        .get(pane_id)
        .ok_or(("invalid_request", format!("unknown pane {pane_id}")))?;
    let text = st.screens.get(pane_id).cloned().unwrap_or_default();
    let body = PaneReadBody {
        pane_id: pane_id.to_string(),
        workspace_id: pane.workspace_id.clone(),
        tab_id: pane.tab_id.clone(),
        source,
        format: ReadFormat::Text,
        text: text.clone(),
        revision: pane.revision,
        truncated: false,
    };
    serde_json::to_value(PaneReadResult {
        kind: "pane_read".into(),
        read: Some(body),
        pane_id: Some(pane_id.to_string()),
        text: Some(text),
        truncated: Some(false),
    })
    .map_err(json_err)
}

fn source_of(params: &Value) -> ReadSource {
    match params.get("source").and_then(Value::as_str) {
        Some("visible") => ReadSource::Visible,
        Some("recent") => ReadSource::Recent,
        Some("detection") => ReadSource::Detection,
        _ => ReadSource::RecentUnwrapped,
    }
}

fn blank_pane(ws: &str, tab: &str, pane: &str, cwd: &str, focused: bool) -> PaneInfo {
    PaneInfo {
        pane_id: pane.to_string(),
        terminal_id: format!("term_{}", pane.replace(':', "")),
        workspace_id: ws.to_string(),
        tab_id: tab.to_string(),
        focused,
        cwd: Some(cwd.to_string()),
        label: None,
        agent: None,
        agent_status: AgentStatus::Unknown,
        terminal_title: None,
        terminal_title_stripped: None,
        revision: 0,
        scroll: Some(PaneScrollInfo {
            offset_from_bottom: 0,
            max_offset_from_bottom: 0,
            viewport_rows: 40,
        }),
    }
}

fn session_ref(kind: &str) -> AgentSessionInfo {
    AgentSessionInfo {
        source: format!("herdr:{kind}"),
        agent: kind.to_string(),
        kind: AgentSessionRefKind::Id,
        value: FIXED_SESSION_ID.to_string(),
    }
}

fn idle_screen(reply: &str) -> String {
    if reply.is_empty() {
        "❯ \n".into()
    } else {
        format!("{reply}\n❯ \n")
    }
}

const TRUST_DIALOG: &str = "\
 Quick safety check: Is this a project you created or one you trust?\
\n ❯ No, exit\
\n   Yes, I trust this folder\
\n Enter to confirm · Esc to cancel\
";

fn ok() -> Result<Value, (&'static str, String)> {
    serde_json::to_value(OkResult { kind: "ok".into() }).map_err(json_err)
}

fn success(req: &WireRequest, result: Value) -> String {
    json!({"id": request_id(&req.id), "result": result}).to_string() + "\n"
}

fn error_reply(req: &WireRequest, code: &str, message: String) -> String {
    json!({"id": request_id(&req.id), "error": {"code": code, "message": message}}).to_string()
        + "\n"
}
