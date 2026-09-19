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
use crate::launch::shim::ShimSet;
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
    /// Agent this session runs. `Codex` / `Grok` additionally get a shadow
    /// native home with their own hook files (D-028 P6). `Claude` uses the
    /// `--settings` overlay; other kinds get neither.
    pub kind: remuda_protocol::AgentKind,
    /// Granted MCP servers spliced into the codex shadow `config.toml`
    /// (D-045 leg (b)); empty on every other path.
    pub mcp_servers: Vec<crate::launch::ShadowMcpServer>,
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
    /// Shadow `CODEX_HOME` / `GROK_HOME`, when the agent kind has one (P6).
    pub shadow: Option<crate::launch::ShadowHome>,
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
        // P6: codex/grok hooks live in a per-session shadow home rather than a
        // `--settings` merge. Materialized before the shims so their
        // pass-through wrappers can export the shadow `CODEX_HOME` /
        // `GROK_HOME` to a hand-typed command in a promoted terminal.
        let shadow = match options.kind {
            remuda_protocol::AgentKind::Codex | remuda_protocol::AgentKind::Grok => {
                Some(crate::launch::shadow::materialize(
                    options.kind,
                    &crate::launch::ShadowOptions {
                        launch_dir: &launch_dir,
                        relay_binary: &options.relay_binary,
                        socket_path: &socket_path,
                        mcp_servers: &options.mcp_servers,
                    },
                )?)
            }
            _ => None,
        };
        let shim_exports: Vec<(&str, &str, &str)> = shadow
            .as_ref()
            .map(|home| {
                let target = match options.kind {
                    remuda_protocol::AgentKind::Codex => "codex",
                    remuda_protocol::AgentKind::Grok => "grok",
                    _ => unreachable!("shadow exists only for codex/grok"),
                };
                home.env
                    .iter()
                    .map(|(key, value)| (target, key.as_str(), value.as_str()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut shims = crate::launch::shim::materialize_shims_with_env(
            &launch_dir,
            &overlay.path,
            &credential,
            shim_off,
            // No per-session override is supplied on the shell path yet.
            // Native agent launches use the materializer's pinned executable.
            None,
            &shim_exports,
        )?;
        shims.env.insert(
            "REMUDA_HOOK_RELAY".into(),
            options.relay_binary.to_string_lossy().into_owned(),
        );
        Ok(Self {
            _server: server,
            overlay,
            shims,
            socket_path,
            shadow,
            bus,
        })
    }

    /// The native session a `SessionStart` reported on this socket, if any.
    #[must_use]
    pub fn binding(&self) -> Option<SessionBinding> {
        self.bus.binding()
    }

    /// Whether a blocking hook for `id` is parked on this socket right now.
    ///
    /// The driver asks before answering so it can tell a hook-carried
    /// interaction from a screen-carried one without consulting a second
    /// table (D-028 §4.4).
    #[must_use]
    pub fn is_parked(&self, id: &remuda_protocol::InteractionId) -> bool {
        self.bus.is_parked(id)
    }

    /// Deliver a device's answer to the hook parked under interaction `id`.
    ///
    /// The [`Outcome`](remuda_signal::Outcome) is the honesty gate: only
    /// [`Answered`](remuda_signal::Outcome::Answered) means a waiting process
    /// received the decision. [`Abandoned`](remuda_signal::Outcome::Abandoned)
    /// is the ignored/confined case the screen-key fallback exists for — the
    /// decision was real but nothing heard it (§14 risk 1).
    pub fn resolve_answer(
        &self,
        id: &remuda_protocol::InteractionId,
        answer: &remuda_protocol::InteractionAnswer,
    ) -> remuda_signal::Outcome {
        self.bus.resolve_answer(id, answer)
    }

    /// Deny every parked hook (instance is closing).
    ///
    /// Without it each parked hook holds its agent's turn open until its own
    /// deadline, minutes after the session is gone.
    pub fn retire_parked(&self) {
        self.bus.retire_all();
    }

    /// Release only the renderer pin after an authenticated SessionStart binds.
    /// Callers must first verify that the binding belongs to the launched agent.
    pub fn release_tui_pin(&self) -> DriverResult<()> {
        self.overlay.release_tui_pin()
    }

    /// Hook-derived turn state for the exact foreground agent, when observed.
    #[must_use]
    pub fn turn_active(&self, pid: i32) -> Option<bool> {
        self.bus.turn_active(pid)
    }

    /// Wall-clock time of the newest hook record from the exact foreground
    /// agent, when one exists. The promotion poller bounds its hook guard by
    /// this so a stalled hook channel cannot pin the screen to `working`.
    #[must_use]
    pub fn hook_last_seen(&self, pid: i32) -> Option<std::time::Instant> {
        self.bus.hook_last_seen(pid)
    }

    /// The PTY observed a fresh native interruption marker after cancel.
    pub fn confirm_screen_interrupt(&self, pid: i32) {
        self.bus.confirm_screen_interrupt(pid);
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
        // P6: point codex/grok at the shadow home and, for grok, switch off its
        // cross-read of the user's `~/.claude` hooks. Applied after the shim
        // env so it also overrides the recipe's store-home `CODEX_HOME` when
        // the driver layers recipe env earlier in `build_command`.
        if let Some(shadow) = &self.shadow {
            for (key, value) in &shadow.env {
                env.insert(key.clone(), value.clone());
            }
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
    use remuda_protocol::{AgentKind, HostId, Id, InstanceId, RunId};
    use remuda_signal::{BusContext, HookEnvelope};
    use std::fs;
    use tokio::sync::mpsc;

    fn options(dir: &Path) -> HookSessionOptions {
        HookSessionOptions {
            instance_dir: dir.to_path_buf(),
            relay_binary: PathBuf::from("/opt/remuda/bin/remuda"),
            tui: TuiMode::Fullscreen,
            base_settings: None,
            kind: AgentKind::Claude,
            mcp_servers: Vec::new(),
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

    /// D-045 leg (b) real shell-pty path: the granted codex MCP server splices
    /// into the complete shadow `config.toml` HookSession materializes, and a
    /// pre-existing MCP table (e.g. a recipe-written partial file) survives the
    /// full materialization intact rather than being erased.
    #[tokio::test]
    async fn codex_hook_session_splices_granted_mcp_server_into_shadow_config() {
        let dir = tempfile::tempdir().unwrap();
        let launch_dir = dir.path().join("launch");
        let shadow_home = launch_dir.join("codex-home");
        fs::create_dir_all(&shadow_home).unwrap();
        // Simulate the recipe's partial MCP-only config that would otherwise
        // leave the granted server undelivered on a carrier without hooks.
        let preexisting =
            "[mcp_servers.\"codex-computer-use\"]\ncommand = \"/bin/sh\"\nargs = [\"/old\"]\n";
        fs::write(shadow_home.join("config.toml"), preexisting).unwrap();

        let mut options = options(dir.path());
        options.kind = AgentKind::Codex;
        options.mcp_servers = vec![crate::launch::ShadowMcpServer {
            name: "codex-computer-use".to_owned(),
            command: "/bin/sh".to_owned(),
            args: vec!["/x/launch/cua/scripts/launch-cua-repl.sh".to_owned()],
            env: vec![
                ("REMUDA_CAPABILITY_COMPUTER_USE".to_owned(), "1".to_owned()),
                ("REMUDA_CODEX_HOME".to_owned(), "/Users/u/.codex".to_owned()),
            ],
        }];
        let session = HookSession::start(&options, bus().0).expect("hook session starts");
        session.shadow.as_ref().expect("codex has a shadow home");

        let text = fs::read_to_string(shadow_home.join("config.toml")).unwrap();
        let parsed: toml::Table = text.parse().expect("shadow config parses");
        // Features + hook trust (the full materialize_codex file) remain; the
        // trust table key is `<hooks.json>:<event>:0:0`, so assert structurally.
        assert_eq!(parsed["features"]["hooks"].as_bool(), Some(true));
        assert!(
            parsed["hooks"]["state"]
                .as_table()
                .is_some_and(|state| state
                    .keys()
                    .any(|key| key.contains(":permission_request:0:0"))),
            "hook trust entry survives: {text}"
        );
        // Granted server is present exactly once, with the spliced env.
        let server = &parsed["mcp_servers"]["codex-computer-use"];
        assert_eq!(server["command"].as_str(), Some("/bin/sh"));
        assert_eq!(
            server["args"][0].as_str(),
            Some("/x/launch/cua/scripts/launch-cua-repl.sh")
        );
        assert_eq!(
            server["env"]["REMUDA_CAPABILITY_COMPUTER_USE"].as_str(),
            Some("1")
        );
        assert_eq!(
            server["env"]["REMUDA_CODEX_HOME"].as_str(),
            Some("/Users/u/.codex")
        );
        assert_eq!(
            text.matches("[mcp_servers.\"codex-computer-use\"]").count(),
            1,
            "the table must be spliced once, not duplicated: {text}"
        );
        // No stale /old argument survives.
        assert!(!text.contains("/old"));
    }
}
