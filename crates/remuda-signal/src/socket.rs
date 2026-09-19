//! The per-instance hook socket (D-028 §4.2).
//!
//! One `SOCK_STREAM` unix socket per instance, conventionally at
//! `<instance dir>/hook.sock`; when that path would exceed `sun_path` the
//! driver binds a short name in the per-user runtime dir and leaves a
//! `hook.sock` symlink under the instance dir (see [`crate::runtime_dir`]).
//! Mode 0600, inside a 0700 directory. One connection carries one event: the
//! relay writes a single JSON line and reads a single JSON line back.
//!
//! Security shape, in full:
//!
//! - **Filesystem is the outer boundary.** 0600 on the socket and 0700 on its
//!   directory mean only the Node's own uid can connect at all. On Linux the
//!   socket mode is advisory on some filesystems, which is why the directory
//!   mode carries the guarantee.
//! - **The credential is the inner boundary, and it is per-instance.** It is
//!   minted at launch, lives only in the child's environment and in this
//!   process, and is *not* the Node's device token: a leaked hook credential
//!   authorises appending hook events to one instance's journal and nothing
//!   else. Comparison is length-then-bytes, not `==` on `String`, so a wrong
//!   credential is refused the same way every time.
//! - **Requests are bounded.** A payload over [`MAX_REQUEST`] is refused
//!   before it is parsed, so a runaway hook cannot make the Node buy memory.

use crate::event::{HookEnvelope, HookReply};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// How long one connection may take to send its request line.
///
/// A hook writes its payload immediately; a peer that connects and then says
/// nothing is either wedged or hostile, and either way must not hold a task
/// and an fd for the life of the instance. Generous enough that a large
/// `PostToolBatch` over a loaded machine is never cut off.
pub const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Largest hook request accepted, in bytes.
///
/// A `PostToolBatch` carrying several tool responses is the realistic maximum
/// and lands far below this; 4 MiB leaves room without letting one connection
/// cost real memory.
pub const MAX_REQUEST: usize = 4 * 1024 * 1024;

/// Socket transport failures.
#[derive(Debug, thiserror::Error)]
pub enum SocketError {
    /// Underlying IO failure.
    #[error("hook socket io: {0}")]
    Io(#[from] std::io::Error),
    /// Request or reply was not the JSON we expect.
    #[error("hook socket protocol: {0}")]
    Protocol(String),
    /// The credential did not match this instance's.
    #[error("hook socket credential rejected")]
    Unauthorized,
}

/// Handler the Node installs to turn one event into one reply.
pub trait SignalSink: Send + Sync + 'static {
    /// Handle one authenticated event, returning what the agent should read.
    ///
    /// P1 always answers [`HookReply::empty`]; the return type is a reply, not
    /// `()`, so P5 can answer a `PermissionRequest` without changing the wire.
    fn deliver(
        &self,
        envelope: HookEnvelope,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = HookReply> + Send + '_>>;
}

/// A listening hook socket. Dropping it stops the accept loop and unlinks the
/// socket file, so a purged instance leaves nothing behind.
pub struct HookServer {
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl HookServer {
    /// Bind `path` and serve `sink` until dropped.
    ///
    /// Any stale socket at `path` is removed first: it belongs to a previous
    /// run of this same instance, and a previous run's socket cannot prove
    /// anything about this one.
    pub fn bind(
        path: &Path,
        credential: String,
        sink: Arc<dyn SignalSink>,
    ) -> Result<Self, SocketError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            set_mode(parent, 0o700)?;
        }
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(path)
            .map_err(|error| crate::runtime_dir::bind_io_error(path, error))?;
        set_mode(path, 0o600)?;
        let credential = Arc::new(credential);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let credential = Arc::clone(&credential);
                let sink = Arc::clone(&sink);
                tokio::spawn(async move {
                    if let Err(error) = serve_one(stream, &credential, sink.as_ref()).await {
                        tracing::debug!(%error, "hook connection");
                    }
                });
            }
        });
        Ok(Self {
            path: path.to_path_buf(),
            task,
        })
    }

    /// Path this server is listening on.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for HookServer {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn serve_one(
    stream: UnixStream,
    credential: &str,
    sink: &dyn SignalSink,
) -> Result<(), SocketError> {
    let (reader, mut writer) = stream.into_split();
    // Cap before buffering: a runaway hook must not be able to make the Node
    // allocate. One byte over the cap is enough to tell that it was exceeded.
    let mut reader = BufReader::new(reader.take((MAX_REQUEST + 1) as u64));
    let mut line = String::new();
    let read = tokio::time::timeout(READ_TIMEOUT, reader.read_line(&mut line))
        .await
        .map_err(|_| SocketError::Protocol("request timed out".into()))??;
    if read > MAX_REQUEST {
        return Err(SocketError::Protocol("request exceeds the size cap".into()));
    }
    if line.trim().is_empty() {
        return Ok(());
    }
    let envelope: HookEnvelope = serde_json::from_str(line.trim_end())
        .map_err(|error| SocketError::Protocol(error.to_string()))?;
    if !credential_matches(credential, &envelope.credential) {
        // Say nothing useful back: a caller who cannot authenticate learns
        // only that it failed.
        let mut payload = serde_json::to_vec(&HookReply::empty().to_hook_json())?;
        payload.push(b'\n');
        writer.write_all(&payload).await?;
        writer.flush().await?;
        return Err(SocketError::Unauthorized);
    }
    let reply = sink.deliver(envelope).await;
    let mut payload = serde_json::to_vec(&reply.to_hook_json())?;
    payload.push(b'\n');
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Length-then-bytes comparison, so a mismatch costs the same whatever it is.
fn credential_matches(expected: &str, supplied: &str) -> bool {
    if expected.len() != supplied.len() {
        return false;
    }
    expected
        .as_bytes()
        .iter()
        .zip(supplied.as_bytes())
        .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// What became of one attempt to deliver an event.
///
/// The two failure modes are different answers, not one, and collapsing them
/// is how an approval path goes wrong:
///
/// - [`Delivery::TimedOut`] — the Node was reached and simply never answered.
///   Somebody may well be looking at a card right now, so a blocking event
///   must **fail closed** here (§4.4: "timeout is always deny").
/// - [`Delivery::Unreachable`] — there is no Node: no socket, connection
///   refused, a malformed reply. Remuda is not in the loop at all, so the
///   honest move is no opinion, which leaves the agent's own dialog as the
///   single place the decision gets made.
#[derive(Debug, Clone, PartialEq)]
pub enum Delivery {
    /// The Node answered. May still be [`HookReply::empty`].
    Replied(HookReply),
    /// Reached the Node; it did not answer inside the budget.
    TimedOut,
    /// Never reached a Node.
    Unreachable,
}

/// Send one event and report what became of it, bounded by `timeout`.
///
/// Callers that need to tell a timeout from an absent Node use this;
/// [`send_event`] is the convenience wrapper for those that do not.
pub async fn deliver_event(
    socket: &Path,
    envelope: &HookEnvelope,
    timeout: std::time::Duration,
) -> Delivery {
    match tokio::time::timeout(timeout, exchange(socket, envelope)).await {
        Ok(Ok(reply)) => Delivery::Replied(reply),
        Ok(Err(error)) => {
            tracing::debug!(%error, "hook relay send failed");
            Delivery::Unreachable
        }
        Err(_) => {
            tracing::debug!("hook relay timed out");
            Delivery::TimedOut
        }
    }
}

/// Send one event and read the reply, bounded by `timeout`.
///
/// On any failure — no socket, no Node, a Node that never answers — the caller
/// gets [`HookReply::empty`] and the agent falls back to its own behaviour.
/// A hook that fails must never be able to stall the agent the user is typing
/// into. Use [`deliver_event`] when the difference between those failures
/// matters.
pub async fn send_event(
    socket: &Path,
    envelope: &HookEnvelope,
    timeout: std::time::Duration,
) -> HookReply {
    match deliver_event(socket, envelope, timeout).await {
        Delivery::Replied(reply) => reply,
        Delivery::TimedOut | Delivery::Unreachable => HookReply::empty(),
    }
}

async fn exchange(socket: &Path, envelope: &HookEnvelope) -> Result<HookReply, SocketError> {
    let stream = UnixStream::connect(socket).await?;
    let (reader, mut writer) = stream.into_split();
    let mut payload = serde_json::to_vec(envelope)?;
    payload.push(b'\n');
    writer.write_all(&payload).await?;
    writer.flush().await?;
    let mut line = String::new();
    BufReader::new(reader).read_line(&mut line).await?;
    if line.trim().is_empty() {
        return Ok(HookReply::empty());
    }
    let value: serde_json::Value = serde_json::from_str(line.trim_end())
        .map_err(|error| SocketError::Protocol(error.to_string()))?;
    let empty = value.as_object().is_none_or(serde_json::Map::is_empty);
    Ok(HookReply {
        decision: (!empty).then_some(value),
    })
}

impl From<serde_json::Error> for SocketError {
    fn from(value: serde_json::Error) -> Self {
        Self::Protocol(value.to_string())
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        seen: Mutex<Vec<HookEnvelope>>,
        reply: Mutex<HookReply>,
    }

    impl SignalSink for Arc<Recorder> {
        fn deliver(
            &self,
            envelope: HookEnvelope,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = HookReply> + Send + '_>> {
            let reply = self.reply.lock().unwrap().clone();
            self.seen.lock().unwrap().push(envelope);
            Box::pin(async move { reply })
        }
    }

    fn envelope(credential: &str, event: &str) -> HookEnvelope {
        HookEnvelope {
            credential: credential.into(),
            event: event.into(),
            ppid: 4242,
            payload: serde_json::json!({"session_id": "s-1"}),
        }
    }

    const WAIT: std::time::Duration = std::time::Duration::from_secs(5);

    #[tokio::test]
    async fn an_event_reaches_the_sink_and_its_reply_reaches_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hook.sock");
        let recorder = Arc::new(Recorder::default());
        let server =
            HookServer::bind(&socket, "cred-a".into(), Arc::new(Arc::clone(&recorder))).unwrap();
        let reply = send_event(server.path(), &envelope("cred-a", "SessionStart"), WAIT).await;
        assert_eq!(reply, HookReply::empty());
        let seen = recorder.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].event, "SessionStart");
        assert_eq!(seen[0].ppid, 4242);
    }

    #[tokio::test]
    async fn a_decision_is_handed_back_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hook.sock");
        let recorder = Arc::new(Recorder::default());
        *recorder.reply.lock().unwrap() = HookReply {
            decision: Some(serde_json::json!({"behavior": "allow"})),
        };
        let server =
            HookServer::bind(&socket, "cred-a".into(), Arc::new(Arc::clone(&recorder))).unwrap();
        let reply = send_event(
            server.path(),
            &envelope("cred-a", "PermissionRequest"),
            WAIT,
        )
        .await;
        assert_eq!(
            reply.to_hook_json(),
            serde_json::json!({"behavior": "allow"})
        );
    }

    #[tokio::test]
    async fn a_wrong_credential_is_refused_and_never_reaches_the_sink() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hook.sock");
        let recorder = Arc::new(Recorder::default());
        let server =
            HookServer::bind(&socket, "cred-a".into(), Arc::new(Arc::clone(&recorder))).unwrap();
        let reply = send_event(server.path(), &envelope("cred-b", "SessionStart"), WAIT).await;
        // The agent still gets a usable `{}` — a rejected hook must not hang it.
        assert_eq!(reply.to_hook_json(), serde_json::json!({}));
        assert!(
            recorder.seen.lock().unwrap().is_empty(),
            "an unauthenticated event must not be journaled"
        );
    }

    #[tokio::test]
    async fn a_credential_of_a_different_length_is_refused() {
        assert!(!credential_matches("cred-a", "cred-a-longer"));
        assert!(!credential_matches("cred-a", ""));
        assert!(credential_matches("cred-a", "cred-a"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_socket_and_its_directory_are_private() {
        use std::os::unix::fs::PermissionsExt;
        // Fixed short root so the socket genuinely binds under `instance/`
        // (tempdir honours a long TMPDIR and would force a redirect).
        let dir = crate::runtime_dir::testutil::ShortDir::new();
        let instance = dir.path().join("instance");
        let socket = instance.join("hook.sock");
        let server = HookServer::bind(
            &socket,
            "cred-a".into(),
            Arc::new(Arc::new(Recorder::default())),
        )
        .unwrap();
        let socket_mode = std::fs::metadata(server.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let dir_mode = std::fs::metadata(&instance).unwrap().permissions().mode() & 0o777;
        assert_eq!(socket_mode, 0o600, "hook socket must not be group/world");
        assert_eq!(dir_mode, 0o700, "instance dir must not be group/world");
    }

    #[tokio::test]
    async fn dropping_the_server_unlinks_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hook.sock");
        {
            let _server = HookServer::bind(
                &socket,
                "cred-a".into(),
                Arc::new(Arc::new(Recorder::default())),
            )
            .unwrap();
            assert!(socket.exists());
        }
        assert!(!socket.exists(), "a purged instance must leave no socket");
    }

    #[tokio::test]
    async fn a_missing_socket_falls_back_to_an_empty_reply() {
        let dir = tempfile::tempdir().unwrap();
        let reply = send_event(
            &dir.path().join("absent.sock"),
            &envelope("cred-a", "Stop"),
            WAIT,
        )
        .await;
        assert_eq!(reply.to_hook_json(), serde_json::json!({}));
    }

    #[tokio::test]
    async fn a_node_that_never_answers_times_out_into_an_empty_reply() {
        struct Silent;
        impl SignalSink for Silent {
            fn deliver(
                &self,
                _envelope: HookEnvelope,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = HookReply> + Send + '_>>
            {
                Box::pin(async {
                    // Longer than any test is willing to wait.
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                    HookReply::empty()
                })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hook.sock");
        let server = HookServer::bind(&socket, "cred-a".into(), Arc::new(Silent)).unwrap();
        let reply = send_event(
            server.path(),
            &envelope("cred-a", "PermissionRequest"),
            std::time::Duration::from_millis(150),
        )
        .await;
        assert_eq!(
            reply.to_hook_json(),
            serde_json::json!({}),
            "a stalled Node must not stall the agent"
        );
    }

    #[tokio::test]
    async fn a_peer_that_connects_and_says_nothing_does_not_hold_a_task_forever() {
        // Otherwise a wedged hook leaks a task and an fd for the life of the
        // instance. Uses tokio's clock so the test does not wait 30s.
        tokio::time::pause();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hook.sock");
        let recorder = Arc::new(Recorder::default());
        let server =
            HookServer::bind(&socket, "cred-a".into(), Arc::new(Arc::clone(&recorder))).unwrap();
        let stream = tokio::net::UnixStream::connect(server.path())
            .await
            .unwrap();
        tokio::time::advance(READ_TIMEOUT + std::time::Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        // The silent peer never reached the sink, and the server is still
        // serving: a bad connection must not take the socket down with it.
        assert!(recorder.seen.lock().unwrap().is_empty());
        drop(stream);
        tokio::time::resume();
        let reply = send_event(server.path(), &envelope("cred-a", "Stop"), WAIT).await;
        assert_eq!(reply.to_hook_json(), serde_json::json!({}));
        assert_eq!(recorder.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_oversized_request_is_refused_before_it_is_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hook.sock");
        let recorder = Arc::new(Recorder::default());
        let server =
            HookServer::bind(&socket, "cred-a".into(), Arc::new(Arc::clone(&recorder))).unwrap();
        let mut huge = envelope("cred-a", "PostToolBatch");
        huge.payload = serde_json::json!({ "blob": "x".repeat(MAX_REQUEST + 1024) });
        let reply = send_event(server.path(), &huge, WAIT).await;
        assert_eq!(reply.to_hook_json(), serde_json::json!({}));
        assert!(
            recorder.seen.lock().unwrap().is_empty(),
            "an oversized request must not reach the sink"
        );
    }

    /// A long data directory: the socket binds at a short runtime path and a
    /// round trip works there; the under-instance symlink exists for discovery
    /// but is not itself connectable (the `sun_path` limit applies to connect
    /// as well as bind, which is exactly why clients get the real path).
    #[tokio::test]
    async fn a_socket_in_a_long_dir_serves_through_its_short_runtime_path() {
        let root = tempfile::tempdir().unwrap();
        // 80 filler segments push `<root>/<fill>/instances/<id>/hook.sock`
        // comfortably past the 107-byte Linux limit.
        let instance_dir = root
            .path()
            .join("x".repeat(80))
            .join("instances/ins_01990000-0000-7000-8000-000000000001");
        std::fs::create_dir_all(&instance_dir).unwrap();
        let preferred = instance_dir.join("hook.sock");
        let placement = crate::runtime_dir::place_socket(
            &preferred,
            "01990000-0000-7000-8000-000000000001.sock",
        )
        .unwrap();
        assert!(placement.redirected());
        assert!(placement.bind_path().as_os_str().len() <= crate::runtime_dir::SUN_PATH_LIMIT);
        let recorder = Arc::new(Recorder::default());
        let server = HookServer::bind(
            placement.bind_path(),
            "cred-a".into(),
            Arc::new(Arc::clone(&recorder)),
        )
        .unwrap();
        placement.install_link().unwrap();

        // The discovery symlink is present under the (long) instance dir.
        let link_meta = std::fs::symlink_metadata(&preferred).unwrap();
        assert!(link_meta.file_type().is_symlink());
        assert_eq!(
            std::fs::read_link(&preferred).unwrap(),
            placement.bind_path()
        );

        // Round trip over the real short path.
        let reply = send_event(server.path(), &envelope("cred-a", "SessionStart"), WAIT).await;
        assert_eq!(reply.to_hook_json(), serde_json::json!({}));
        assert_eq!(recorder.seen.lock().unwrap().len(), 1);

        // Connecting via the long symlink path is rejected at the syscall
        // boundary before symlink resolution — clients must use bind_path().
        let via_link = tokio::net::UnixStream::connect(&preferred).await;
        assert!(via_link.is_err(), "long symlink path must not connect");
    }
}
