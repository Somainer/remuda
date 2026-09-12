//! Persistent runtime with replaceable local NDJSON controllers.

use crate::inventory::{CollectRequest, collect};
use crate::transport::hubnode;
use crate::{DevNode, NodeError, StdioOptions, enroll};
use remuda_protocol::{Id, U64};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinSet;

const MAX_FRAME: usize = 1_048_576;

/// Socket owned by the persistent Node, below its data directory.
pub fn daemon_socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join("node.sock")
}

/// Probe the daemon without acquiring or replacing its controller.
pub async fn daemon_is_running(data_dir: &Path) -> Result<bool, NodeError> {
    let stream = match UnixStream::connect(daemon_socket_path(data_dir)).await {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error.into()),
    };
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        let (read, mut write) = stream.into_split();
        write_frame(
            &mut write,
            &hubnode::rpc_request("status", "node.daemon.status", json!({})),
        )
        .await?;
        let response = read_frame(&mut BufReader::new(read), &mut Vec::new()).await?;
        Ok::<_, NodeError>(
            response.is_some_and(|frame| frame.pointer("/result/running") == Some(&json!(true))),
        )
    })
    .await;
    result.map_err(|_| NodeError::Transport("daemon status timed out".into()))?
}

/// Acquire a bridge controller, then return the Hub-facing NDJSON stream.
/// A false `takeover` refuses to replace an attached controller.
pub async fn connect_daemon_bridge(
    data_dir: &Path,
    takeover: bool,
) -> Result<UnixStream, NodeError> {
    let mut stream = UnixStream::connect(daemon_socket_path(data_dir)).await?;
    write_frame(
        &mut stream,
        &hubnode::rpc_request("bridge", "node.bridge.hello", json!({"takeover":takeover})),
    )
    .await?;
    // Read one byte at a time so the returned stream retains every byte of the
    // immediately following Hub hello; a temporary BufReader would lose it.
    let response = {
        let mut reader = BufReader::with_capacity(1, &mut stream);
        tokio::time::timeout(
            Duration::from_secs(5),
            read_frame(&mut reader, &mut Vec::new()),
        )
        .await
        .map_err(|_| NodeError::Transport("daemon bridge hello timed out".into()))??
        .ok_or(NodeError::Disconnected)?
    };
    if let Some(error) = response.get("error") {
        return Err(NodeError::Conflict(
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("controller refused")
                .into(),
        ));
    }
    if response.pointer("/result/bridge") != Some(&json!(true)) {
        return Err(NodeError::Transport("invalid daemon bridge hello".into()));
    }
    Ok(stream)
}

#[derive(Default)]
struct Controller {
    generation: u64,
    active: bool,
}

struct Shared {
    controller: StdMutex<Controller>,
    changed: watch::Sender<u64>,
    dispatch: Arc<Mutex<()>>,
    acked: Mutex<HashMap<String, u64>>,
    epoch: Id,
    inventory: Option<crate::HostSnapshot>,
}

/// Controller fence shared by local bridges and the outbound WSS connection.
#[derive(Clone)]
pub struct DaemonControl(Arc<Shared>);

impl DaemonControl {
    /// Create a controller fence for one daemon runtime.
    pub fn new() -> Result<Self, NodeError> {
        Self::with_inventory(None)
    }

    /// Create a fence with an already collected inventory, avoiding PATH probes
    /// during controller handshakes when the composition root has a snapshot.
    pub fn with_inventory(inventory: Option<crate::HostSnapshot>) -> Result<Self, NodeError> {
        let (changed, _) = watch::channel(0);
        Ok(Self(Arc::new(Shared {
            controller: StdMutex::new(Controller::default()),
            changed,
            dispatch: Arc::new(Mutex::new(())),
            acked: Mutex::new(HashMap::new()),
            epoch: Id::new("epoch")?,
            inventory,
        })))
    }

    /// Wait until no bridge is attached, then acquire the primary WSS controller.
    pub async fn acquire_outbound(&self) -> Result<DaemonWssLease, NodeError> {
        let mut changed = self.0.changed.subscribe();
        loop {
            {
                let mut controller = self
                    .0
                    .controller
                    .lock()
                    .map_err(|_| NodeError::StorePoisoned)?;
                if !controller.active {
                    controller.generation += 1;
                    controller.active = true;
                    self.0.changed.send_replace(controller.generation);
                    return Ok(DaemonWssLease(Arc::new(LeaseGuard {
                        shared: self.0.clone(),
                        generation: controller.generation,
                    })));
                }
            }
            changed
                .changed()
                .await
                .map_err(|_| NodeError::Disconnected)?;
        }
    }
}

struct LeaseGuard {
    shared: Arc<Shared>,
    generation: u64,
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if let Ok(mut controller) = self.shared.controller.lock()
            && controller.generation == self.generation
        {
            controller.active = false;
            controller.generation += 1;
            self.shared.changed.send_replace(controller.generation);
        }
    }
}

/// Revocable outbound controller; dropping its last clone releases ownership.
#[derive(Clone)]
pub struct DaemonWssLease(Arc<LeaseGuard>);

impl DaemonWssLease {
    /// Wait until a bridge explicitly takes over this controller.
    pub async fn revoked(&self) {
        let mut changed = self.0.shared.changed.subscribe();
        while *changed.borrow_and_update() == self.0.generation {
            if changed.changed().await.is_err() {
                break;
            }
        }
    }

    pub(crate) fn fence(&self) -> DaemonWssFence {
        DaemonWssFence {
            shared: self.0.shared.clone(),
            generation: self.0.generation,
            enrollment_dir: None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct DaemonWssFence {
    shared: Arc<Shared>,
    generation: u64,
    pub(crate) enrollment_dir: Option<PathBuf>,
}

impl DaemonWssFence {
    pub(crate) fn epoch(&self) -> Id {
        self.shared.epoch.clone()
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
    pub(crate) async fn revoked(&self) {
        let mut changed = self.shared.changed.subscribe();
        while *changed.borrow_and_update() == self.generation {
            if changed.changed().await.is_err() {
                break;
            }
        }
    }

    pub(crate) async fn dispatch_guard(
        &self,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, NodeError> {
        let guard = self.shared.dispatch.clone().lock_owned().await;
        let controller = self
            .shared
            .controller
            .lock()
            .map_err(|_| NodeError::StorePoisoned)?;
        if controller.generation != self.generation || !controller.active {
            return Err(NodeError::Conflict(
                "outbound controller was replaced by a bridge".into(),
            ));
        }
        Ok(guard)
    }
}

struct SocketGuard {
    socket: PathBuf,
    pid: PathBuf,
}

struct TtyPump(tokio::task::JoinHandle<()>);

impl Drop for TtyPump {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.pid);
    }
}

/// Serve the persistent runtime until this future is cancelled.
/// Disconnecting any controller drops its transport only; the composition root
/// remains responsible for calling `DevNode::shutdown` on process termination.
pub async fn run_daemon_runtime_opts(node: DevNode, opts: StdioOptions) -> Result<(), NodeError> {
    run_daemon_runtime_controlled(node, opts, DaemonControl::new()?).await
}

/// Serve a daemon sharing controller fencing with its outbound WSS link.
pub async fn run_daemon_runtime_controlled(
    node: DevNode,
    opts: StdioOptions,
    control: DaemonControl,
) -> Result<(), NodeError> {
    let listener = bind_daemon(&opts.data_dir).await?;
    run_daemon_runtime_listener(node, opts, control, &listener).await
}

/// Reserved socket and pidfile; dropping it removes both paths.
pub struct DaemonListener {
    listener: UnixListener,
    _socket: SocketGuard,
    _lock: nix::fcntl::Flock<std::fs::File>,
}

/// Reserve the daemon socket before composing or reconciling native drivers.
pub async fn bind_daemon(data_dir: &Path) -> Result<DaemonListener, NodeError> {
    std::fs::create_dir_all(data_dir)?;
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(data_dir.join("node.lock"))?;
    let lock = nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusiveNonblock)
        .map_err(|(_, error)| {
            NodeError::Conflict(format!("Node daemon data directory is locked: {error}"))
        })?;
    let path = daemon_socket_path(data_dir);
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        if !metadata.file_type().is_socket() {
            return Err(NodeError::InvalidConfig(
                "daemon socket path is not a socket".into(),
            ));
        }
        match UnixStream::connect(&path).await {
            Ok(_) => return Err(NodeError::Conflict("Node daemon is already running".into())),
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                std::fs::remove_file(&path)?
            }
            Err(error) => return Err(error.into()),
        }
    }
    let listener = UnixListener::bind(&path)?;
    let _socket = SocketGuard {
        socket: path.clone(),
        pid: data_dir.join("node.pid"),
    };
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let temporary_pid = data_dir.join(format!("node.pid.{}.tmp", std::process::id()));
    let mut pid_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary_pid)?;
    std::io::Write::write_all(
        &mut pid_file,
        format!("{}\n", std::process::id()).as_bytes(),
    )?;
    pid_file.sync_all()?;
    std::fs::rename(temporary_pid, &_socket.pid)?;
    Ok(DaemonListener {
        listener,
        _socket,
        _lock: lock,
    })
}

/// Serve a socket reserved before native runtime initialization.
pub async fn run_daemon_runtime_listener(
    node: DevNode,
    opts: StdioOptions,
    control: DaemonControl,
    listener: &DaemonListener,
) -> Result<(), NodeError> {
    let shared = control.0;
    let mut sessions = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.listener.accept() => {
                let (stream, _) = accepted?;
                let (node, opts, shared) = (node.clone(), opts.clone(), shared.clone());
                sessions.spawn(async move {
                    if let Err(error) = accept_controller(node, opts, shared, stream).await {
                        tracing::debug!(%error, "daemon controller disconnected");
                    }
                });
            }
            _ = sessions.join_next(), if !sessions.is_empty() => {}
        }
    }
}

async fn accept_controller(
    node: DevNode,
    opts: StdioOptions,
    shared: Arc<Shared>,
    stream: UnixStream,
) -> Result<(), NodeError> {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let frame = tokio::time::timeout(
        Duration::from_secs(5),
        read_frame(&mut read, &mut Vec::new()),
    )
    .await
    .map_err(|_| NodeError::Transport("daemon local hello timed out".into()))??
    .ok_or(NodeError::Disconnected)?;
    let id = frame.get("id").cloned().unwrap_or(Value::Null);
    if frame.get("method").and_then(Value::as_str) == Some("node.daemon.status") {
        return write_frame(&mut write, &hubnode::rpc_ok(id, json!({"running":true,"daemon":true,"hostId":node.host().meta.id,"pid":std::process::id()}))).await;
    }
    if frame.get("method").and_then(Value::as_str) != Some("node.bridge.hello") {
        return write_frame(
            &mut write,
            &hubnode::rpc_error(id, -32600, "node.bridge.hello required"),
        )
        .await;
    }
    let generation = {
        let mut controller = shared
            .controller
            .lock()
            .map_err(|_| NodeError::StorePoisoned)?;
        if controller.active && frame.pointer("/params/takeover") != Some(&json!(true)) {
            None
        } else {
            controller.generation += 1;
            controller.active = true;
            shared.changed.send_replace(controller.generation);
            Some(controller.generation)
        }
    };
    let Some(generation) = generation else {
        return write_frame(
            &mut write,
            &hubnode::rpc_error(
                id,
                -32009,
                "an active controller exists; hello takeover flag required",
            ),
        )
        .await;
    };
    let _lease = LeaseGuard {
        shared: shared.clone(),
        generation,
    };
    async {
        write_frame(
            &mut write,
            &hubnode::rpc_ok(
                id,
                json!({"bridge":true,"daemon":true,"controllerGeneration":generation.to_string()}),
            ),
        )
        .await?;
        serve_controller(&node, &opts, &shared, generation, &mut read, &mut write).await
    }
    .await
}

async fn serve_controller<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(
    node: &DevNode,
    opts: &StdioOptions,
    shared: &Shared,
    generation: u64,
    read: &mut R,
    write: &mut W,
) -> Result<(), NodeError> {
    let mut changed = shared.changed.subscribe();
    if *changed.borrow() != generation {
        return Ok(());
    }
    let enrollment = enroll::load_or_create(&opts.data_dir)?;
    let request = CollectRequest {
        labels: opts.labels.clone(),
        max_instances: opts.max_instances,
        herdr_socket: None,
    };
    let snapshot = match &shared.inventory {
        Some(snapshot) => snapshot.clone(),
        None => tokio::select! {
            _ = changed.changed() => return Ok(()),
            result = tokio::task::spawn_blocking(move || collect(&request)) => result
                .map_err(|error|NodeError::Driver(format!("inventory task failed: {error}")))?,
        },
    };
    let label = opts.display_label.as_deref().unwrap_or(&snapshot.hostname);
    let mut params = hubnode::stdio_hello_params(
        &enrollment.host_id,
        label,
        "ssh-stdio",
        env!("CARGO_PKG_VERSION"),
        &shared.epoch,
        serde_json::to_value(&snapshot)?,
        enrollment.node_token.as_deref(),
    );
    params["bridge"] = json!(true);
    params["daemon"] = json!(true);
    params["durable"] = json!(true);
    params["controllerTakeover"] = json!(true);
    params["controllerGeneration"] = json!(generation.to_string());
    params["instances"] = serde_json::to_value(node.list_instances()?.items)?;
    params["instanceWatermarks"] = json!(
        shared
            .acked
            .lock()
            .await
            .iter()
            .map(|(id, seq)| json!({"instanceId":id,"durableSeq":seq.to_string()}))
            .collect::<Vec<_>>()
    );
    if let Some(token) = &enrollment.node_token {
        write_frame(write, &hubnode::encode_auth("auth-1", token)).await?;
    }
    write_frame(write, &hubnode::encode_hello_request("hello-1", params)).await?;
    let mut sent = HashMap::<String, u64>::new();
    let mut ready = false;
    let mut pending = HashMap::<String, (String, u64)>::new();
    let mut last_ack = tokio::time::Instant::now();
    let (tty_tx, mut tty_rx) = tokio::sync::mpsc::channel(128);
    let _tty_pump = TtyPump(crate::stdio::spawn_stdio_tty_pump(node.clone(), tty_tx));
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    let mut line = Vec::new();
    loop {
        if *changed.borrow() != generation {
            return Ok(());
        }
        tokio::select! {
            _ = changed.changed() => return Ok(()),
            frame = tty_rx.recv(), if ready => {
                if let Some(frame) = frame {write_frame(write, &frame).await?;}
            }
            _ = tick.tick(), if ready => {
                if !pending.is_empty() && last_ack.elapsed() > Duration::from_secs(30) {
                    return Err(NodeError::Transport("journal acknowledgement timed out; reconnect required".into()));
                }
                let was_empty = pending.is_empty();
                tokio::select! {
                    _ = changed.changed() => return Ok(()),
                    result = forward_journals(node, write, &mut sent, &mut pending) => result?,
                }
                if was_empty && !pending.is_empty() {last_ack = tokio::time::Instant::now();}
            }
            frame = read_frame(read, &mut line) => {
                let Some(frame) = frame? else {return Ok(());};
                if frame.get("method").is_none() {
                    if frame.get("id").and_then(Value::as_str) == Some("hello-1") {
                        let result = frame.get("result").ok_or_else(|| NodeError::Transport("Hub rejected daemon hello".into()))?;
                        if let Some(token) = result.get("nodeToken").and_then(Value::as_str).filter(|token|!token.is_empty()) {
                            enroll::persist_host_token(&opts.data_dir,token)?;
                        }
                        hubnode::persist_hello_result(&opts.data_dir, &frame)?;
                        sent = resume_watermarks(node, result)?;
                        *shared.acked.lock().await = sent.clone();
                        ready = true;
                    } else if let Some(id) = frame.get("id").and_then(Value::as_str)
                        && let Some((instance, seq)) = pending.remove(id) {
                        let confirmed = acknowledged(&frame).ok_or_else(|| NodeError::Transport("journal append rejected or missing durable acknowledgement; reconnect required".into()))?;
                        if confirmed != (instance.clone(),seq) {
                            return Err(NodeError::Transport("journal acknowledgement does not match sent sequence".into()));
                        }
                        let mut acked = shared.acked.lock().await;
                        let mark = acked.entry(instance).or_default();
                        *mark = (*mark).max(seq);
                        last_ack = tokio::time::Instant::now();
                    }
                    continue;
                }
                let request = hubnode::decode_request(&frame)?;
                let id = request.id.unwrap_or(Value::Null);
                let params = request.params.unwrap_or(json!({}));
                let _dispatch = shared.dispatch.lock().await;
                if shared.controller.lock().map_err(|_| NodeError::StorePoisoned)?.generation != generation {return Ok(());}
                let result = if !ready {
                    Err(NodeError::InvalidRequest("Hub hello must complete before commands".into()))
                } else if crate::interactions::is_interaction_method(&request.method) {
                    node.dispatch_interaction(&request.method,params).await
                } else {
                    hubnode::dispatch_method(node,&request.method,params).await
                };
                // A dispatched command settles before takeover; a blocked old
                // controller's response pipe must never retain this fence.
                drop(_dispatch);
                if !id.is_null() {
                    let response = match result {
                        Ok(value) => hubnode::rpc_ok(id,value),
                        Err(error) => hubnode::rpc_error(id,-32602,&error.to_string()),
                    };
                    if *changed.borrow() != generation {return Ok(());}
                    tokio::select! {
                        _ = changed.changed() => return Ok(()),
                        result = tokio::time::timeout(Duration::from_secs(30),write_frame(write,&response)) => {
                            result.map_err(|_|NodeError::Transport("controller response timed out".into()))??;
                        }
                    }
                }
            }
        }
    }
}

pub(crate) fn resume_watermarks(
    node: &DevNode,
    hello: &Value,
) -> Result<HashMap<String, u64>, NodeError> {
    let instances = node.list_instances()?.items;
    let mut result = HashMap::new();
    for entry in ["instanceWatermarks", "resumeCursors"]
        .into_iter()
        .filter_map(|key| hello.get(key).and_then(Value::as_array))
        .flatten()
    {
        let instance = entry
            .get("instanceId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                let journal = entry.get("journalId").and_then(Value::as_str)?;
                instances
                    .iter()
                    .find(|instance| instance.journal_id.as_str() == journal)
                    .map(|instance| instance.meta.id.as_id().to_string())
            });
        let seq = entry
            .get("durableSeq")
            .or_else(|| entry.get("afterSeq"))
            .or_else(|| entry.get("seq"))
            .and_then(parse_seq);
        if let (Some(instance), Some(seq)) = (instance, seq) {
            result.insert(instance, seq);
        }
    }
    Ok(result)
}

fn parse_seq(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn acknowledged(frame: &Value) -> Option<(String, u64)> {
    let id = frame.get("id")?.as_str()?.strip_prefix("journal-")?;
    let (instance, seq) = id.rsplit_once('-')?;
    let sent: u64 = seq.parse().ok()?;
    let result = frame.get("result")?;
    let ack = result
        .get("durableSeq")
        .or_else(|| result.get("seq"))
        .and_then(parse_seq)?;
    // Never promote a response beyond the event this request actually sent.
    (ack >= sent).then(|| (instance.to_owned(), sent))
}

async fn forward_journals<W: AsyncWrite + Unpin>(
    node: &DevNode,
    write: &mut W,
    sent: &mut HashMap<String, u64>,
    pending: &mut HashMap<String, (String, u64)>,
) -> Result<(), NodeError> {
    for instance in node.list_instances()?.items {
        if pending.len() >= 16 {
            break;
        }
        let key = instance.meta.id.as_id().to_string();
        let last = sent.get(&key).copied().unwrap_or(0);
        for event in node
            .read_journal(&instance.journal_id, Some(U64(last)), 16 - pending.len())?
            .events
        {
            let seq = event.position().1.0;
            let seq_i64 = i64::try_from(seq)
                .map_err(|_| NodeError::InvalidRequest("journal sequence exceeds i64".into()))?;
            let params = hubnode::encode_append(&key, Some(seq_i64), serde_json::to_value(event)?);
            let id = format!("journal-{key}-{seq}");
            write_frame(
                write,
                &hubnode::rpc_request(id.as_str(), "journal.append", params),
            )
            .await?;
            pending.insert(id, (key.clone(), seq));
            sent.insert(key.clone(), seq);
        }
    }
    Ok(())
}

async fn read_frame<R: AsyncBufRead + Unpin>(
    input: &mut R,
    line: &mut Vec<u8>,
) -> Result<Option<Value>, NodeError> {
    loop {
        let buffer = input.fill_buf().await?;
        if buffer.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(NodeError::Disconnected)
            };
        }
        let length = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buffer.len(), |index| index + 1);
        if line.len() + length > MAX_FRAME {
            return Err(NodeError::InvalidRequest(
                "daemon NDJSON frame exceeds local limit".into(),
            ));
        }
        let complete = buffer[length - 1] == b'\n';
        line.extend_from_slice(&buffer[..length]);
        input.consume(length);
        if complete {
            let value = serde_json::from_slice(line)?;
            line.clear();
            return Ok(Some(value));
        }
    }
}

async fn write_frame<W: AsyncWrite + Unpin>(
    output: &mut W,
    frame: &Value,
) -> Result<(), NodeError> {
    output.write_all(&serde_json::to_vec(frame)?).await?;
    output.write_all(b"\n").await?;
    output.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fragmented_frame_survives_select_cancellation() {
        let (input, mut output) = tokio::io::duplex(1024);
        let mut input = BufReader::new(input);
        let mut line = Vec::new();
        output.write_all(b"{\"jsonrpc\":").await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), read_frame(&mut input, &mut line))
                .await
                .is_err()
        );
        output.write_all(b"\"2.0\",\"id\":1}\n").await.unwrap();
        assert_eq!(
            read_frame(&mut input, &mut line).await.unwrap().unwrap()["id"],
            1
        );
    }

    #[test]
    fn only_matching_durable_response_advances_ack() {
        assert!(
            acknowledged(&json!({"id":"journal-ins_a-9","error":{"message":"rejected"}})).is_none()
        );
        assert!(acknowledged(&json!({"id":"journal-ins_a-9","result":{"seq":"8"}})).is_none());
        assert_eq!(
            acknowledged(&json!({"id":"journal-ins_a-9","result":{"durableSeq":"20"}})),
            Some(("ins_a".into(), 9))
        );
    }
}
