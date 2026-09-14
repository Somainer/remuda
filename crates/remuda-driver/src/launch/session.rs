//! One instance's hook plumbing, assembled (D-028 §4.2, P1).
//!
//! [`HookSession`] is what the shell-pty driver holds: a bound socket, a
//! materialized overlay, a shim directory, and the environment the child needs
//! to reach all three. Its lifetime is the instance's — dropping it unbinds the
//! socket and leaves the files for `instance.purge` to remove with the rest of
//! the instance directory.
//!
//! The environment it hands back is the reason this type exists rather than
//! three loose calls. `child_env` inherits a closed allowlist from the Node,
//! and none of these variables can come from there: `PATH` has to be rewritten
//! to put the shim first, and the hook credential is a value the driver
//! *computed*, not one it inherited. §4.2 draws exactly that line — new
//! variables go on the driver-computed side of it, because the inherited side
//! is what keeps `LD_PRELOAD`, proxies and the Node's own tokens out of the
//! child.

use crate::error::DriverResult;
use crate::launch::overlay::{HookOverlay, OverlayOptions, TuiMode, materialize_overlay};
use crate::launch::shim::{ShimSet, materialize_shims};
use remuda_signal::{HookServer, SessionBinding, SignalBus};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Bytes of entropy in a hook credential.
///
/// The credential authorises appending hook events to one instance's journal
/// for as long as that instance lives — minutes to hours, never persisted.
/// 32 bytes is far past what that exposure justifies and costs nothing.
const CREDENTIAL_BYTES: usize = 32;

/// What the caller must supply to stand up an instance's hook path.
pub struct HookSessionOptions {
    /// `<data dir>/instances/<id>`.
    pub instance_dir: PathBuf,
    /// The `remuda` binary the relay runs as.
    pub relay_binary: PathBuf,
    /// Renderer to pin, from the launch request (§9.2).
    pub tui: TuiMode,
    /// An existing settings overlay to merge into.
    pub base_settings: Option<serde_json::Value>,
}

/// A live hook path for one instance.
pub struct HookSession {
    /// Bound socket. Dropping it unlinks the socket file.
    _server: HookServer,
    /// The merged per-session settings file.
    pub overlay: HookOverlay,
    /// Generated shims and their directory.
    pub shims: ShimSet,
    /// `<instance dir>/hook.sock`.
    pub socket_path: PathBuf,
    /// The bus serving this socket, so the driver can read what it learned.
    bus: Arc<SignalBus>,
}

impl HookSession {
    /// Bind the socket, write the overlay and shims, and return the whole set.
    ///
    /// Order matters: the socket is bound first so that a `claude` started
    /// microseconds after the shim appears on PATH has somewhere to deliver
    /// its `SessionStart`.
    pub fn start(options: &HookSessionOptions, bus: Arc<SignalBus>) -> DriverResult<Self> {
        let launch_dir = options.instance_dir.join("launch");
        let socket_path = options.instance_dir.join("hook.sock");
        let credential = mint_credential();
        // `REMUDA_SHIM=off` is honoured twice: the generated shim reads it at
        // run time (so a session already under way degrades cleanly), and here
        // so an operator who set it before the Node started gets no shim
        // directory at all rather than a dormant one on PATH.
        let shim_off = crate::launch::shim_disabled(
            std::env::var(crate::launch::SHIM_DISABLE_ENV)
                .ok()
                .as_deref(),
        );
        let server = HookServer::bind(&socket_path, credential.clone(), Arc::clone(&bus) as _)
            .map_err(|error| {
                crate::error::DriverError::SettingsIsolationUnavailable(format!(
                    "hook socket could not be bound: {error}"
                ))
            })?;
        let overlay = materialize_overlay(&OverlayOptions {
            launch_dir: launch_dir.clone(),
            relay_binary: options.relay_binary.clone(),
            socket_path: socket_path.clone(),
            tui: options.tui,
            base: options.base_settings.clone(),
        })?;
        let shims = materialize_shims(
            &launch_dir,
            &overlay.path,
            &credential,
            shim_off,
            // No override plumbed through this path yet: the shell-pty session
            // builder that owns these options is in flight elsewhere. `None`
            // is the pre-existing behaviour (search PATH), not a regression.
            None,
        )?;
        Ok(Self {
            _server: server,
            overlay,
            shims,
            socket_path,
            bus,
        })
    }

    /// The native session a `SessionStart` reported on this socket, if any.
    #[must_use]
    pub fn binding(&self) -> Option<SessionBinding> {
        self.bus.binding()
    }

    /// Environment additions for the PTY child, given the `PATH` it inherited.
    ///
    /// Deliberately injected rather than inherited (§4.2): `PATH` is rewritten
    /// so the shim resolves first, and `REMUDA_HOOK_CREDENTIAL` is a
    /// driver-computed secret. Both would be refused by the inherit allowlist,
    /// which is the point — that allowlist is what keeps the Node's own
    /// credentials out of a process the model can read.
    #[must_use]
    pub fn child_env(&self, inherited_path: &str) -> BTreeMap<String, String> {
        self.child_env_with(inherited_path, std::env::var("ZDOTDIR").ok().as_deref())
    }

    /// [`child_env`](Self::child_env) over an explicit inherited `ZDOTDIR`, so
    /// tests do not have to mutate the real process environment.
    ///
    /// The user's own `ZDOTDIR` is carried through as `REMUDA_USER_ZDOTDIR`
    /// rather than dropped: our shadow rc files source theirs by that path, so
    /// somebody who keeps their zsh configuration outside `$HOME` still gets
    /// exactly the shell they configured.
    #[must_use]
    pub fn child_env_with(
        &self,
        inherited_path: &str,
        user_zdotdir: Option<&str>,
    ) -> BTreeMap<String, String> {
        let mut env = self.shims.env.clone();
        env.insert("PATH".into(), self.shims.path_with(inherited_path));
        if let Some(value) = user_zdotdir.map(str::trim).filter(|v| !v.is_empty()) {
            env.insert("REMUDA_USER_ZDOTDIR".into(), value.to_owned());
        }
        env
    }

    /// Path the shims live in.
    #[must_use]
    pub fn bin_dir(&self) -> &Path {
        &self.shims.bin_dir
    }
}

/// Mint a per-instance credential.
///
/// Independent of the Node's device and bootstrap tokens by construction: it is
/// generated here and never read from configuration, so there is no path by
/// which a hook credential could be a device credential.
fn mint_credential() -> String {
    use rand_core::{OsRng, RngCore};
    let mut bytes = [0_u8; CREDENTIAL_BYTES];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().fold(
        String::with_capacity(CREDENTIAL_BYTES * 2),
        |mut out, byte| {
            use std::fmt::Write;
            let _ = write!(out, "{byte:02x}");
            out
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_protocol::{HostId, Id, InstanceId, RunId};
    use remuda_signal::{BusContext, HookEnvelope};
    use tokio::sync::mpsc;

    fn options(dir: &Path) -> HookSessionOptions {
        HookSessionOptions {
            instance_dir: dir.to_path_buf(),
            relay_binary: PathBuf::from("/opt/remuda/bin/remuda"),
            tui: TuiMode::Fullscreen,
            base_settings: None,
        }
    }

    /// A bus writing into a channel the caller keeps, so a test can read what
    /// the socket actually journaled.
    fn bus() -> (Arc<SignalBus>, mpsc::Receiver<remuda_protocol::Observation>) {
        let (tx, rx) = mpsc::channel(32);
        let bus = SignalBus::new(
            BusContext {
                instance_id: InstanceId::new(),
                host_id: HostId::new(),
                journal_id: Id::new("obj").unwrap(),
                run_id: RunId::new(),
                driver_kind: remuda_protocol::DriverKind::ShellPty,
                adapter_version: "test".into(),
            },
            tx,
            Arc::new(std::sync::atomic::AtomicU64::new(0)),
        );
        (Arc::new(bus), rx)
    }

    fn start(dir: &Path) -> HookSession {
        HookSession::start(&options(dir), bus().0).expect("hook session starts")
    }

    #[tokio::test]
    async fn starting_a_session_binds_the_socket_and_writes_both_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let session = start(dir.path());
        assert!(session.socket_path.exists(), "the socket must be listening");
        assert!(session.overlay.path.is_file());
        assert!(session.bin_dir().join("claude").is_file());
    }

    #[tokio::test]
    async fn the_child_environment_puts_the_shim_first_and_carries_the_credential() {
        let dir = tempfile::tempdir().unwrap();
        let session = start(dir.path());
        let env = session.child_env("/usr/bin:/bin");
        let path = env.get("PATH").expect("PATH is rewritten");
        assert!(
            path.starts_with(&session.bin_dir().to_string_lossy().into_owned()),
            "{path}"
        );
        assert!(path.ends_with("/usr/bin:/bin"), "the rest of PATH survives");
        assert!(
            env.get("REMUDA_HOOK_CREDENTIAL")
                .is_some_and(|value| value.len() == CREDENTIAL_BYTES * 2),
            "the credential must reach the child"
        );
    }

    #[tokio::test]
    async fn each_instance_gets_its_own_credential() {
        // A shared credential would let one instance's hooks write another's
        // journal.
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let a = start(first.path());
        let b = start(second.path());
        assert_ne!(
            a.child_env("").get("REMUDA_HOOK_CREDENTIAL"),
            b.child_env("").get("REMUDA_HOOK_CREDENTIAL")
        );
    }

    #[tokio::test]
    async fn dropping_the_session_takes_the_socket_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let socket = start(dir.path()).socket_path.clone();
        assert!(
            !socket.exists(),
            "a closed instance must not leave a live socket behind"
        );
    }

    #[tokio::test]
    async fn a_hook_delivered_on_the_generated_socket_is_journaled_and_bound() {
        // The full P1 path in one test: the credential the shim exports
        // authenticates against the socket the overlay points at, the event
        // reaches the journal, and the session becomes resumable.
        let dir = tempfile::tempdir().unwrap();
        let (bus, mut events) = bus();
        let session = HookSession::start(&options(dir.path()), bus).unwrap();
        let credential = session
            .child_env("")
            .get("REMUDA_HOOK_CREDENTIAL")
            .cloned()
            .expect("credential");

        let reply = remuda_signal::send_event(
            &session.socket_path,
            &HookEnvelope {
                credential,
                event: "SessionStart".into(),
                ppid: 4242,
                payload: serde_json::json!({
                    "session_id": "0199a1f0-0000-7000-8000-000000000000",
                    "transcript_path": "/w/s.jsonl",
                }),
            },
            std::time::Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            reply.to_hook_json(),
            serde_json::json!({}),
            "P1 observes only"
        );

        let observation = events.recv().await.expect("the event reached the journal");
        assert_eq!(
            observation.source.channel,
            remuda_protocol::SourceChannel::Hook
        );
        let binding = session.binding().expect("SessionStart binds the session");
        assert_eq!(binding.pid, 4242);
        assert_eq!(binding.session_id, "0199a1f0-0000-7000-8000-000000000000");
        assert_eq!(binding.transcript_path.as_deref(), Some("/w/s.jsonl"));
    }
}
