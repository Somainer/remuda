//! Persistent runtime with replaceable local NDJSON controllers.
//!
//! RESILIENCE CONTRACT (spec companion — `docs/design/hub-resilience.md`,
//! 2026-09-22 c-hubresil): the daemon process, its driver processes and the
//! local durable journal outlive every control transport. This controller
//! forwards journals from the Node journal and treats a missing durable
//! acknowledgement as fatal for the *controller* — it exits and reconnects, it
//! never silently skips a seq. The doc's §2.3/§2.4 pins that behavior with
//! line anchors and §3 specifies the buffering water levels
//! (`journal.softfull`, driver back-pressure, follow degradation) that do not
//! yet exist. The hard failure `journal acknowledgement timed out; reconnect
//! required` is a contract and must not be weakened. Docs-only for now.

use crate::inventory::{CollectRequest, collect};
use crate::stdio::rpc_code;
use crate::transport::hubnode;
use crate::{DevNode, NodeError, StdioOptions, enroll};
use remuda_protocol::{Id, U64};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

/// Per-controller-session sequence for Node→Hub gate authority requests.
static GATE_AUTH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A Node→Hub gate authority request (gate.authorize / gate.landGrant) sent
/// from a detached gate run through the durable daemon controller session.
struct DaemonGateAuth {
    method: &'static str,
    job_id: String,
    generation: u32,
    merge_sha: String,
    reply: oneshot::Sender<Result<Value, NodeError>>,
}

/// [`crate::GateAuthorizer`] for the durable daemon: the run task sends
/// the request over a channel; the controller read/write loop writes it and
/// correlates the Hub reply by id. Revoked (channel closed / takeover) →
/// refuse.
type PendingGateAuth = Arc<StdMutex<HashMap<String, oneshot::Sender<Result<Value, NodeError>>>>>;

#[derive(Clone)]
struct DaemonGateAuthorizer {
    tx: mpsc::Sender<DaemonGateAuth>,
}

impl DaemonGateAuthorizer {
    async fn call(
        &self,
        method: &'static str,
        job_id: &str,
        generation: u32,
        merge_sha: &str,
    ) -> Result<Value, NodeError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(DaemonGateAuth {
                method,
                job_id: job_id.to_owned(),
                generation,
                merge_sha: merge_sha.to_owned(),
                reply: tx,
            })
            .await
            .map_err(|_| NodeError::Disconnected)?;
        tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .map_err(|_| NodeError::Disconnected)?
            .map_err(|_| NodeError::Disconnected)?
    }
}

impl crate::GateAuthorizer for DaemonGateAuthorizer {
    fn authorize<'a>(
        &'a self,
        job_id: &'a str,
        generation: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            matches!(
                self.call(remuda_protocol::METHOD_GATE_AUTHORIZE, job_id, generation, "")
                    .await,
                Ok(value) if value.get("authorized").and_then(Value::as_bool) == Some(true)
            )
        })
    }

    fn request_land_grant<'a>(
        &'a self,
        job_id: &'a str,
        generation: u32,
        merge_sha: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::GateLandGrantAnswer> + Send + 'a>>
    {
        Box::pin(async move {
            match self
                .call(
                    remuda_protocol::METHOD_GATE_LAND_GRANT,
                    job_id,
                    generation,
                    merge_sha,
                )
                .await
            {
                Ok(value) => crate::GateLandGrantAnswer {
                    granted: value.get("granted").and_then(Value::as_bool) == Some(true),
                    landing_in_progress: value.get("landingInProgress").and_then(Value::as_bool)
                        == Some(true),
                },
                _ => crate::GateLandGrantAnswer {
                    granted: false,
                    landing_in_progress: false,
                },
            }
        })
    }
}
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::JoinSet;

const MAX_FRAME: usize = 1_048_576;

/// Path clients use to reach the Node daemon.
///
/// Conventionally `<data_dir>/node.sock`. When the data directory is too long
/// for AF_UNIX `sun_path` the daemon binds a short hashed name in the per-user
/// runtime directory and leaves `node.sock` as a symlink; because connect(2)
/// enforces the same length limit *before* symlink resolution, clients must
/// connect to the resolved target rather than the long link.
///
/// Resolution order:
///
/// 1. A usable installed `node.sock` symlink wins. Its target records where
///    *the running daemon* actually bound, which matters when the daemon and
///    the client see different environments (a cron shell versus a systemd
///    user unit differ in `XDG_RUNTIME_DIR`/`TMPDIR`, so either side's
///    derivation could pick a different candidate). The target is accepted
///    only when it is an absolute path within `sun_path`.
/// 2. Otherwise the target is **derived deterministically** with the same
///    chooser the binder uses. That covers before-first-start and the
///    binder's startup window (the link is installed last): without it a
///    status/start probe would hand the kernel the over-long path, get
///    `InvalidInput` instead of `NotFound`, and abort a launch that should
///    simply start the daemon.
/// 3. If both fail, the conventional path (connect fails `NotFound`).
pub fn daemon_socket_path(data_dir: &Path) -> PathBuf {
    let preferred = data_dir.join("node.sock");
    if remuda_signal::runtime_dir::path_fits_sun_path(&preferred) {
        return preferred;
    }
    // 1. Honor the running daemon's installed link when usable.
    if let Ok(metadata) = std::fs::symlink_metadata(&preferred)
        && metadata.file_type().is_symlink()
        && let Ok(target) = std::fs::read_link(&preferred)
        && target.is_absolute()
        && target.as_os_str().len() <= remuda_signal::runtime_dir::SUN_PATH_LIMIT
    {
        return target;
    }
    // 2. Derive the same target the binder would choose, without binding or
    // installing the link. The call only prepares (creates/secures) the
    // runtime directory; the socket itself stays absent until the daemon
    // binds it.
    if let Ok(placement) =
        remuda_signal::runtime_dir::place_socket(&preferred, &daemon_runtime_name(data_dir))
        && placement.redirected()
    {
        return placement.bind_path().to_path_buf();
    }
    // 3.
    preferred
}

/// Stable short socket file name for a data directory: `node-<hash>.sock`.
///
/// A digest, never the path itself: it is unique per data directory, stable
/// across restarts (so the symlink keeps naming the same target), and leaks
/// nothing into a shared runtime directory.
fn daemon_runtime_name(data_dir: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data_dir.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    let hex: String = digest[..10]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("node-{hex}.sock")
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

#[derive(Default, Clone)]
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
    /// Data dir holding the durable controller-generation counter. Empty in
    /// in-process tests (no persistence; generations reset with the process).
    data_dir: PathBuf,
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
        Self::open(PathBuf::new(), inventory)
    }

    /// Build the fence with a durable controller-generation counter in the data
    /// dir, so a controller generation survives a real daemon restart: a new
    /// process starts ABOVE every generation the prior process ever handed out,
    /// and a pre-restart dispatch can never be current again (r14 item 8b).
    pub fn open(
        data_dir: PathBuf,
        inventory: Option<crate::HostSnapshot>,
    ) -> Result<Self, NodeError> {
        let (changed, _) = watch::channel(0);
        let generation = if data_dir.as_os_str().is_empty() {
            0
        } else {
            read_controller_generation(&data_dir)
        };
        Ok(Self(Arc::new(Shared {
            controller: StdMutex::new(Controller {
                generation,
                ..Controller::default()
            }),
            changed,
            dispatch: Arc::new(Mutex::new(())),
            acked: Mutex::new(HashMap::new()),
            epoch: Id::new("epoch")?,
            inventory,
            data_dir,
        })))
    }

    /// Test-only: share the same durable fence with a second in-process
    /// runtime (modeling a restart over one data dir).
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn clone_for_test(&self) -> Self {
        Self(Arc::clone(&self.0))
    }

    /// Test-only: clone the durable fence but swap in a pre-collected
    /// inventory, so the test runtime never runs the (slow, environment-bound)
    /// host probes before completing a controller handshake.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn clone_for_test_with_inventory(
        &self,
        inventory: Option<crate::HostSnapshot>,
    ) -> Result<Self, NodeError> {
        let controller = self
            .0
            .controller
            .lock()
            .map_err(|_| NodeError::StorePoisoned)?
            .clone();
        let acked = self
            .0
            .acked
            .try_lock()
            .map(|map| map.clone())
            .unwrap_or_default();
        let inner = Arc::new(Shared {
            controller: StdMutex::new(controller),
            changed: self.0.changed.clone(),
            dispatch: Arc::new(Mutex::new(())),
            acked: Mutex::new(acked),
            epoch: self.0.epoch.clone(),
            inventory,
            data_dir: self.0.data_dir.clone(),
        });
        Ok(Self(inner))
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

/// Persisted controller-generation counter file within the daemon data dir.
fn controller_generation_file(data_dir: &Path) -> PathBuf {
    data_dir.join("controller-generation")
}

/// Read the durable controller generation (0 when absent/unreadable). It makes
/// a controller generation survive a real daemon restart, so no pre-restart
/// controller identity can be current again after a new process boots.
fn read_controller_generation(data_dir: &Path) -> u64 {
    std::fs::read_to_string(controller_generation_file(data_dir))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

/// Serialises the read-compare-rename below between every control in this
/// process (tests open several controls over one data dir).
static PERSIST_GENERATION: StdMutex<()> = StdMutex::new(());

/// Durably record `generation` as the counter's high-water mark.
///
/// Monotonic: a value at or below what the file already holds is never
/// written, so a slower writer can never move the mark backwards. Atomic: the
/// value goes to a temp file in the data dir, is synced, and is renamed over
/// the counter, so a crash leaves either the old or the new value, never a
/// torn or empty file that would read as 0.
fn persist_controller_generation(data_dir: &Path, generation: u64) {
    if data_dir.as_os_str().is_empty() {
        return;
    }
    let _serial = PERSIST_GENERATION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if read_controller_generation(data_dir) >= generation {
        return;
    }
    let target = controller_generation_file(data_dir);
    let temp = data_dir.join(format!(
        "controller-generation.tmp.{}.{generation}",
        std::process::id()
    ));
    let written = (|| -> std::io::Result<()> {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(generation.to_string().as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, &target)
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temp);
        tracing::warn!(%error, "could not persist the controller generation");
    }
}

/// Read the optional `controllerGeneration` a bridge frame names (string or
/// number). A stale value names a pre-restart controller.
fn frame_controller_generation(params: &Value) -> Option<u64> {
    params
        .get("controllerGeneration")
        .and_then(Value::as_u64)
        .or_else(|| {
            params
                .get("controllerGeneration")
                .and_then(Value::as_str)
                .and_then(|value| value.parse().ok())
        })
}

/// Whether a frame claiming controller generation `claimed` is stale against
/// the daemon's CURRENT generation. An absent claim is not checked (normal
/// stdio/WSS carriers do not name one).
fn is_stale_controller_frame(shared: &Shared, params: &Value) -> bool {
    frame_controller_generation(params).is_some_and(|claimed| {
        shared
            .controller
            .lock()
            .map(|controller| claimed != controller.generation)
            .unwrap_or(true)
    })
}

struct SocketGuard {
    /// Real bound socket (in the per-user runtime dir when redirected).
    socket: PathBuf,
    /// Discovery symlink under the data dir, when the socket was redirected.
    link: Option<PathBuf>,
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
        if let Some(link) = &self.link {
            let _ = std::fs::remove_file(link);
        }
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.pid);
    }
}

/// Serve the persistent runtime until this future is cancelled.
/// Disconnecting any controller drops its transport only; the composition root
/// remains responsible for calling `DevNode::shutdown` on process termination.
pub async fn run_daemon_runtime_opts(node: DevNode, opts: StdioOptions) -> Result<(), NodeError> {
    let control = DaemonControl::open(opts.data_dir.clone(), None)?;
    run_daemon_runtime_controlled(node, opts, control).await
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
    listener: Arc<UnixListener>,
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
    // AF_UNIX placement: bind the short real path; the under-data-dir
    // node.sock is a direct socket or a discovery symlink to it.
    // (The "data dir is long" start-up warning is emitted in `compose`, the
    // common node-start path shared with dev/stdio.)
    let preferred = data_dir.join("node.sock");
    let placement =
        remuda_signal::runtime_dir::place_socket(&preferred, &daemon_runtime_name(data_dir))
            .map_err(|error| {
                NodeError::InvalidConfig(format!("daemon socket placement: {error}"))
            })?;
    let path = placement.bind_path().to_path_buf();
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
    let listener = UnixListener::bind(&path).map_err(|error| {
        NodeError::Transport(remuda_signal::runtime_dir::bind_io_error(&path, error).to_string())
    })?;
    let _socket = SocketGuard {
        socket: path.clone(),
        link: placement.link_path().map(Path::to_path_buf),
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
    // Advertise only once everything else is reserved; a failure here drops
    // the guard, which removes the real socket and pid file.
    placement.install_link().map_err(|error| {
        NodeError::InvalidConfig(format!("daemon socket discovery link: {error}"))
    })?;
    Ok(DaemonListener {
        listener: Arc::new(listener),
        _socket,
        _lock: lock,
    })
}

/// Serve a socket reserved before native runtime initialization.
impl DaemonListener {
    /// Test-only: a shareable handle to the bound socket, so multiple
    /// in-process runtimes (a second controller and a post-restart runtime)
    /// accept on the SAME listener while the owning `DaemonListener` keeps the
    /// lock.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn test_socket_arc(&self) -> Arc<UnixListener> {
        Arc::clone(&self.listener)
    }
}

/// Test-only: serve the daemon accept loop on an already-bound, shared
/// socket (the `DaemonListener` lock stays with the caller).
#[cfg(feature = "test-faults")]
pub async fn run_daemon_runtime_socket(
    node: DevNode,
    opts: StdioOptions,
    control: DaemonControl,
    socket: std::sync::Arc<UnixListener>,
) -> Result<(), NodeError> {
    let shared = control.0;
    // Test runtime: register THIS accept loop's generation with the fence so a
    // connection it accepts is never fenced by a newer generation (matching a
    // single-loop daemon, where accept and generation changes share a state).
    // This test loop is claiming a controller generation on behalf of the
    // bridge it will accept; model the single-loop daemon's ordering by
    // marking the shared fence active at the next generation before the
    // takeover hello increments it again would be wrong — instead the loop
    // simply registers as the live controller owner is handled by
    // accept_controller's own hello. No extra claim needed: accept_controller
    // reads/writes the shared controller, and the loop only serves
    // connections whose generation matches the one accept_controller assigns.
    // (Kept as a no-op marker to document the intent.)
    let _ = &shared;
    let mut sessions = JoinSet::new();
    loop {
        tokio::select! {
            accepted = socket.accept() => {
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

/// Serve the daemon accept loop over a reserved [`DaemonListener`].
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
            persist_controller_generation(&shared.data_dir, controller.generation);
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
    shared: &Arc<Shared>,
    generation: u64,
    read: &mut R,
    write: &mut W,
) -> Result<(), NodeError> {
    let mut changed = shared.changed.subscribe();
    if *changed.borrow() != generation {
        return Ok(());
    }
    let enrollment = enroll::load_or_create(&opts.data_dir)?;
    let snapshot = match &shared.inventory {
        Some(snapshot) => snapshot.clone(),
        None => {
            // Real CLI probes can take minutes in a constrained environment;
            // move an owned request onto the blocking pool so the select can
            // still race the controller watch.
            let request = CollectRequest {
                labels: opts.labels.clone(),
                max_instances: opts.max_instances,
                herdr_socket: None,
            };
            let collect_future = tokio::task::spawn_blocking(move || collect(&request));
            tokio::select! {
                _ = changed.changed() => return Ok(()),
                result = collect_future => result
                    .map_err(|error| NodeError::Driver(format!("inventory task failed: {error}")))?,
            }
        }
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
        // The inventory goes on below with the daemon fields, alongside the
        // watermarks this bridge is authoritative for.
        None,
    );
    params["bridge"] = json!(true);
    params["daemon"] = json!(true);
    params["durable"] = json!(true);
    params["controllerTakeover"] = json!(true);
    params["controllerGeneration"] = json!(generation.to_string());
    // Absent rather than empty when this Node cannot vouch for its store: the
    // Hub settles every live row an inventory omits, so an unwarranted `[]`
    // would exit sessions this process simply cannot see. The attestation
    // travels with the list it belongs to.
    if let Some(inventory) = node.announceable_inventory()? {
        params["instances"] = inventory;
        params["instanceStoreFound"] = json!(true);
    }
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
    // Carrier object pulls for this controller session. The persistent daemon
    // has no HTTP route to the Hub any more than the one-shot stdio process.
    let (carrier_tx, mut carrier_rx) = tokio::sync::mpsc::channel(16);
    let object_broker = crate::carrier_objects::CarrierObjectBroker::new(carrier_tx);
    node.set_object_source(std::sync::Arc::new(object_broker.source()));
    // D-048: the api.* stream table for this controller session. Dropping the
    // controller fails every relay stream and detaches the broker.
    let (api_broker, mut api_rx) = node.api_relay().attach_link();
    // Controller exit (takeover/disconnect) fails every pull immediately; the
    // retry either rides the next controller's source or surfaces the loss.
    struct FailOnDrop(std::sync::Arc<crate::carrier_objects::CarrierObjectBroker>);
    impl Drop for FailOnDrop {
        fn drop(&mut self) {
            self.0.fail_all(NodeError::Disconnected);
        }
    }
    let _object_session = FailOnDrop(object_broker.clone());
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    let mut line = Vec::new();
    // Long carrier methods (a gate run/then/land/unpin, a worker
    // provision/remove, an instance close, a worktree/SCM read) park for
    // minutes and do unbounded blocking work. Awaiting one on this read arm
    // freezes the whole controller: journals stop forwarding and, fatally,
    // `gate.cancel` cannot even be read while a run is in flight — the escape
    // hatch the rest of the design leans on. Spawn each on its own task and let
    // its encoded reply ride this channel back so only this loop writes.
    let (long_tx, mut long_rx) = tokio::sync::mpsc::channel::<Value>(128);
    // r11 item 2: gate.authorize / gate.landGrant from detached gate runs.
    let (gate_auth_tx, mut gate_auth_rx) = mpsc::channel::<DaemonGateAuth>(16);
    let pending_gate_auth: PendingGateAuth = Arc::new(StdMutex::new(HashMap::new()));
    // Fails in-flight gate authority requests when the controller ends
    // (disconnect/takeover), so a paused dispatch delivered to the
    // replacement session cannot proceed (r11 item 2).
    struct GateAuthGuard {
        pending: PendingGateAuth,
    }
    impl Drop for GateAuthGuard {
        fn drop(&mut self) {
            if let Ok(mut map) = self.pending.lock() {
                for (_, waiter) in map.drain() {
                    let _ = waiter.send(Err(NodeError::Disconnected));
                }
            }
        }
    }
    let _gate_auth_guard = GateAuthGuard {
        pending: pending_gate_auth.clone(),
    };
    node.gate_registry()
        .set_authorizer(std::sync::Arc::new(DaemonGateAuthorizer {
            tx: gate_auth_tx.clone(),
        }))
        .await;
    loop {
        if *changed.borrow() != generation {
            return Ok(());
        }
        tokio::select! {
            _ = changed.changed() => return Ok(()),
            frame = carrier_rx.recv() => {
                let Some(frame) = frame else { return Ok(()); };
                tokio::select! {
                    _ = changed.changed() => return Ok(()),
                    result = tokio::time::timeout(Duration::from_secs(30), write_frame(write, &frame)) => {
                        result.map_err(|_| NodeError::Transport("carrier frame write timed out".into()))??;
                    }
                }
            }
            frame = api_rx.recv() => {
                let Some(frame) = frame else { return Ok(()); };
                tokio::select! {
                    _ = changed.changed() => return Ok(()),
                    result = tokio::time::timeout(Duration::from_secs(30), write_frame(write, &frame)) => {
                        result.map_err(|_| NodeError::Transport("api frame write timed out".into()))??;
                    }
                }
            }
            frame = tty_rx.recv(), if ready => {
                if let Some(frame) = frame {write_frame(write, &frame).await?;}
            }
            response = long_rx.recv() => {
                let Some(response) = response else { return Ok(()); };
                if *changed.borrow() != generation {return Ok(());}
                tokio::select! {
                    _ = changed.changed() => return Ok(()),
                    result = tokio::time::timeout(Duration::from_secs(30),write_frame(write,&response)) => {
                        result.map_err(|_|NodeError::Transport("controller response timed out".into()))??;
                    }
                }
            }
            auth = gate_auth_rx.recv(), if ready => {
                let Some(auth) = auth else { return Ok(()); };
                if *changed.borrow() != generation {return Ok(());}
                let id = format!("gateauth-{}", GATE_AUTH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
                if let Ok(mut map) = pending_gate_auth.lock() {
                    map.insert(id.clone(), auth.reply);
                }
                let frame = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": auth.method,
                    "params": {
                        "jobId": auth.job_id,
                        "generation": auth.generation,
                        "hostId": node.host().meta.id.as_id().to_string(),
                        "sessionEpoch": format!("daemon:{}:{}", shared.epoch, generation),
                        "mergeSha": auth.merge_sha,
                    },
                });
                tokio::select! {
                    _ = changed.changed() => return Ok(()),
                    result = tokio::time::timeout(Duration::from_secs(30), write_frame(write, &frame)) => {
                        result.map_err(|_|NodeError::Transport("gate auth write timed out".into()))??;
                    }
                }
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
                // object.pull replies/chunks complete attachment fetches.
                if object_broker.handle_frame(&frame) {
                    continue;
                }
                // D-048: api.* notifications route to the relay stream table.
                if frame
                    .get("method")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(crate::api_relay::is_api_method)
                {
                    api_broker.handle_frame(&frame).await;
                    continue;
                }
                if frame.get("method").is_none() {
                    // r11 item 2: correlate Node→Hub gate.authorize/landGrant replies.
                    if let Some(id) = frame.get("id").and_then(Value::as_str)
                        && id.starts_with("gateauth-")
                    {
                        if let Some(waiter) = pending_gate_auth.lock().ok().and_then(|mut m| m.remove(id)) {
                            let result = if let Some(err) = frame.get("error") {
                                Err(NodeError::InvalidRequest(
                                    err.get("message")
                                        .and_then(Value::as_str)
                                        .unwrap_or("gate authority error")
                                        .to_owned(),
                                ))
                            } else {
                                Ok(frame.get("result").cloned().unwrap_or(Value::Null))
                            };
                            let _ = waiter.send(result);
                        }
                        continue;
                    }
                    if frame.get("id").and_then(Value::as_str) == Some("hello-1") {
                        let result = frame.get("result").ok_or_else(|| NodeError::Transport("Hub rejected daemon hello".into()))?;
                        if let Some(token) = result.get("nodeToken").and_then(Value::as_str).filter(|token|!token.is_empty()) {
                            enroll::persist_host_token(&opts.data_dir,token)?;
                        }
                        hubnode::persist_hello_result(&opts.data_dir, &frame)?;
                        // D-048: adopt the Hub-advertised relay limits.
                        node.api_relay().apply_hello_limits(&frame);
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
                // r14 item 8b: a dispatch frame that still names a pre-restart
                // controller generation is refused outright — it never executes
                // and can never push. The frame reaches the NEW process (its
                // controller bridge connected after the restart) but is not
                // current authority.
                if is_stale_controller_frame(shared, &params) {
                    let response = hubnode::rpc_error(
                        id,
                        -32009,
                        "dispatch carried a stale controller generation; daemon restarted",
                    );
                    let _ = long_tx.send(response).await;
                    continue;
                }
                // A long carrier method (gate run/then/land/unpin, worker
                // provision/remove, instance close, worktree/SCM read) parks for
                // minutes and does unbounded blocking work. Awaiting it on this
                // read arm — or holding the shared dispatch lease across it —
                // freezes journals and every other controller behind the whole
                // body, and blinds this loop to `gate.cancel` while a run is in
                // flight. Take the lease only for the takeover check, drop it,
                // then run the body on its own task whose reply rides `long_tx`
                // back to this loop. `gate.cancel` and every cheap read are not
                // long — they stay inline so the cancel escape hatch fires at
                // once.
                let long_carrier = ready
                    && !crate::interactions::is_interaction_method(&request.method)
                    && crate::gate::is_long_carrier_method(&request.method);
                if long_carrier {
                    {
                        let _dispatch = shared.dispatch.lock().await;
                        if shared.controller.lock().map_err(|_| NodeError::StorePoisoned)?.generation != generation {return Ok(());}
                    }
                    let node = node.clone();
                    let method = request.method.clone();
                    let long_tx = long_tx.clone();
                    tokio::spawn(async move {
                        let result = hubnode::dispatch_method(&node, &method, params).await;
                        if !id.is_null() {
                            // Same code the reply would carry on the inline path
                            // (and on stdio): a not-found or a bad request is
                            // -32602, not the internal -32603 class.
                            let response = match result {
                                Ok(value) => hubnode::rpc_ok(id, value),
                                Err(error) => hubnode::rpc_error(id, rpc_code(&error), &error.to_string()),
                            };
                            let _ = long_tx.send(response).await;
                        }
                    });
                    continue;
                }
                let result = {
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
                    result
                };
                if !id.is_null() {
                    let response = match result {
                        Ok(value) => hubnode::rpc_ok(id,value),
                        Err(error) => hubnode::rpc_error(id,rpc_code(&error),&error.to_string()),
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
        // This tick runs every 50 ms. Skip the read outright when the journal
        // is already at (or behind) what this session has sent, so a caught-up
        // Instance — including a terminal one the Hub is durable at the tail
        // for — costs one watermark query instead of a page read.
        if node.journal_durable_seq(&instance.journal_id)?.0 <= last {
            continue;
        }
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
