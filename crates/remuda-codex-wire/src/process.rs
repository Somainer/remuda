//! Spawn `codex app-server --listen stdio://` and drive the JSON-RPC session.

use std::path::PathBuf;
use std::process::Stdio;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::error::WireError;
use crate::peer::{JsonRpcPeer, spawn_buf_reader};
use crate::rpc::{DEFAULT_MAX_LINE_BYTES, Inbound, RequestId};
use crate::types::{
    ClientInfo, InitializeCapabilities, InitializeParams, InitializeResponse, ThreadStartParams,
    ThreadStartResponse, TurnInterruptParams, TurnInterruptResponse, TurnStartParams,
    TurnStartResponse,
};

/// Native effort ladder from the codex-cli 0.154.0 picker on the owner's Mac.
/// `minimal` is accepted only as a legacy input alias for `low`.
/// See `docs/design/evidence/effort-codex-tiers-1.md`.
pub const REASONING_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];

/// Launch recipe for one app-server child. Always `--listen stdio://`.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    /// Absolute path of the `codex` binary. Relative paths are rejected.
    pub binary: PathBuf,
    /// Child working directory (project cwd).
    pub cwd: PathBuf,
    /// Optional isolated `CODEX_HOME`. When set, injected only into the child env.
    pub codex_home: Option<PathBuf>,
    /// `-c model="…"` overlay.
    pub model: Option<String>,
    /// `-c model_reasoning_effort="…"` overlay.
    pub reasoning_effort: Option<String>,
    /// `-c approval_policy="…"` overlay.
    pub approval_policy: Option<String>,
    /// Extra `-c` payloads, each a TOML fragment such as `sandbox_mode="read-only"`.
    pub extra_config: Vec<String>,
    /// Pass `--disable hooks` (probe-only; production launches should leave hooks on).
    pub disable_hooks: bool,
    /// Additional child env (not the parent process env).
    pub extra_env: Vec<(String, String)>,
    /// Extra argv after the standard app-server flags.
    pub extra_args: Vec<String>,
    /// `initialize` clientInfo. Defaults to product name `remuda`.
    pub client_info: ClientInfo,
    /// `initialize` capabilities.
    pub capabilities: InitializeCapabilities,
    /// NDJSON line cap.
    pub max_line_bytes: usize,
}

impl SpawnSpec {
    /// Pin an absolute `codex` binary and a project cwd.
    pub fn new(binary: PathBuf, cwd: PathBuf) -> Result<Self, WireError> {
        if !binary.is_absolute() {
            return Err(WireError::RelativeBinary(binary));
        }
        Ok(Self {
            binary,
            cwd,
            codex_home: None,
            model: None,
            reasoning_effort: None,
            approval_policy: None,
            extra_config: Vec::new(),
            disable_hooks: false,
            extra_env: Vec::new(),
            extra_args: Vec::new(),
            client_info: ClientInfo::default(),
            capabilities: InitializeCapabilities::default(),
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
        })
    }

    fn config_args(&self) -> Result<Vec<String>, WireError> {
        let mut args = Vec::new();
        if let Some(model) = &self.model {
            args.extend(config_flag("model", model)?);
        }
        if let Some(effort) = &self.reasoning_effort {
            let value = if effort == "minimal" {
                "low"
            } else {
                effort.as_str()
            };
            if !REASONING_EFFORTS.contains(&value) {
                return Err(WireError::InvalidReasoningEffort(effort.clone()));
            }
            args.extend(config_flag("model_reasoning_effort", value)?);
        }
        if let Some(policy) = &self.approval_policy {
            args.extend(config_flag("approval_policy", policy)?);
        }
        for fragment in &self.extra_config {
            if fragment.contains('"') {
                return Err(WireError::InvalidConfigOverride(fragment.clone()));
            }
            args.push("-c".into());
            args.push(fragment.clone());
        }
        Ok(args)
    }

    fn argv(&self) -> Result<Vec<String>, WireError> {
        let mut argv = vec!["app-server".into(), "--listen".into(), "stdio://".into()];
        if self.disable_hooks {
            argv.push("--disable".into());
            argv.push("hooks".into());
        }
        argv.extend(self.config_args()?);
        argv.extend(self.extra_args.iter().cloned());
        Ok(argv)
    }
}

fn config_flag(key: &str, value: &str) -> Result<[String; 2], WireError> {
    if value.contains('"') {
        return Err(WireError::InvalidConfigOverride(format!("{key}={value}")));
    }
    Ok(["-c".into(), format!("{key}=\"{value}\"")])
}

/// Live Codex app-server client.
pub struct CodexAppServer {
    peer: JsonRpcPeer,
    child: Option<Child>,
    initialize: Option<InitializeResponse>,
}

impl CodexAppServer {
    /// Spawn the child, start the reader, and complete `initialize` / `initialized`.
    pub async fn spawn(
        spec: SpawnSpec,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Inbound>), WireError> {
        let (mut client, inbound) = Self::spawn_uninitialized(spec.clone()).await?;
        client.handshake(&spec).await?;
        Ok((client, inbound))
    }

    /// Spawn without `initialize`. Used by tests that assert the native
    /// `Not initialized` error.
    pub async fn spawn_uninitialized(
        spec: SpawnSpec,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Inbound>), WireError> {
        if !spec.binary.is_absolute() {
            return Err(WireError::RelativeBinary(spec.binary));
        }
        let argv = spec.argv()?;
        tracing::info!(
            binary = %spec.binary.display(),
            cwd = %spec.cwd.display(),
            ?argv,
            "spawning codex app-server"
        );
        let mut command = Command::new(&spec.binary);
        command
            .args(&argv)
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "error");
        if let Some(home) = &spec.codex_home {
            command.env("CODEX_HOME", home);
        }
        for (key, value) in &spec.extra_env {
            command.env(key, value);
        }
        let mut child = command.spawn().map_err(WireError::Spawn)?;
        let stdin = child.stdin.take().ok_or(WireError::MissingStdio)?;
        let stdout = child.stdout.take().ok_or(WireError::MissingStdio)?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "remuda_codex_wire::stderr", "{line}");
                }
            });
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let peer = spawn_buf_reader(stdin, stdout, tx, spec.max_line_bytes);
        Ok((
            Self {
                peer,
                child: Some(child),
                initialize: None,
            },
            rx,
        ))
    }

    /// Attach to an existing stdio pair (tests).
    pub fn from_stdio<R, W>(
        stdin: W,
        stdout: R,
        max_line_bytes: usize,
    ) -> (Self, mpsc::UnboundedReceiver<Inbound>)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        let peer = spawn_buf_reader(stdin, stdout, tx, max_line_bytes);
        (
            Self {
                peer,
                child: None,
                initialize: None,
            },
            rx,
        )
    }

    /// `initialize` then notification `initialized`.
    pub async fn handshake(&mut self, spec: &SpawnSpec) -> Result<InitializeResponse, WireError> {
        if self.initialize.is_some() {
            return Err(WireError::AlreadyInitialized);
        }
        let response: InitializeResponse = self
            .peer
            .request(
                "initialize",
                InitializeParams {
                    client_info: spec.client_info.clone(),
                    capabilities: Some(spec.capabilities.clone()),
                },
            )
            .await?;
        self.peer.notify("initialized", None).await?;
        self.initialize = Some(response.clone());
        Ok(response)
    }

    /// Handshake result, if `initialize` already succeeded.
    pub fn initialize_result(&self) -> Option<&InitializeResponse> {
        self.initialize.as_ref()
    }

    /// Raw JSON-RPC peer.
    pub fn peer(&self) -> &JsonRpcPeer {
        &self.peer
    }

    /// Typed `thread/start`.
    pub async fn thread_start(
        &self,
        params: ThreadStartParams,
    ) -> Result<ThreadStartResponse, WireError> {
        self.require_init()?;
        self.peer.request("thread/start", params).await
    }

    /// Typed `turn/start`. The RPC result is admission; wait for `turn/completed`.
    pub async fn turn_start(
        &self,
        params: TurnStartParams,
    ) -> Result<TurnStartResponse, WireError> {
        self.require_init()?;
        self.peer.request("turn/start", params).await
    }

    /// Typed `turn/interrupt`. Success means the interrupt was accepted, not that
    /// the turn has ended.
    pub async fn turn_interrupt(
        &self,
        params: TurnInterruptParams,
    ) -> Result<TurnInterruptResponse, WireError> {
        self.require_init()?;
        self.peer.request("turn/interrupt", params).await
    }

    /// Reply to a server→client request with `{id, result}` and no `method`.
    pub async fn reply_result<T: Serialize + Sync>(
        &self,
        id: RequestId,
        result: T,
    ) -> Result<(), WireError> {
        self.peer.reply_result(id, result).await
    }

    /// Generic request helper for methods this crate does not wrap.
    pub async fn request<P, R>(&self, method: &str, params: P) -> Result<R, WireError>
    where
        P: Serialize + Sync,
        R: DeserializeOwned,
    {
        self.require_init()?;
        self.peer.request(method, params).await
    }

    /// Kill the child if this client spawned it.
    pub async fn kill(&mut self) -> Result<(), WireError> {
        self.peer.shutdown().await;
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        Ok(())
    }

    fn require_init(&self) -> Result<(), WireError> {
        if self.initialize.is_some() {
            Ok(())
        } else {
            Err(WireError::NotInitialized)
        }
    }
}

impl Drop for CodexAppServer {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_relative_binary() {
        let error = SpawnSpec::new("codex".into(), "/tmp".into()).unwrap_err();
        assert!(matches!(error, WireError::RelativeBinary(_)));
    }

    #[test]
    fn stdio_argv_pins_listen_and_config() {
        let mut spec = SpawnSpec::new("/usr/bin/codex".into(), "/tmp".into()).expect("abs");
        spec.model = Some("gpt-5.6-sol".into());
        spec.reasoning_effort = Some("low".into());
        spec.approval_policy = Some("never".into());
        spec.disable_hooks = true;
        let argv = spec.argv().expect("argv");
        assert_eq!(
            argv,
            [
                "app-server",
                "--listen",
                "stdio://",
                "--disable",
                "hooks",
                "-c",
                "model=\"gpt-5.6-sol\"",
                "-c",
                "model_reasoning_effort=\"low\"",
                "-c",
                "approval_policy=\"never\"",
            ]
        );
    }

    #[test]
    fn rejects_effort_outside_the_verified_vocabulary() {
        for value in ["none", "ultracode", "bogus", "HIGH"] {
            let mut spec = SpawnSpec::new("/usr/bin/codex".into(), "/tmp".into()).expect("abs");
            spec.reasoning_effort = Some(value.into());
            let error = spec.argv().unwrap_err();
            assert!(
                matches!(error, WireError::InvalidReasoningEffort(ref v) if v == value),
                "{value}: {error:?}"
            );
        }
    }

    #[test]
    fn six_native_efforts_pass_through_unchanged_and_minimal_maps_to_low() {
        assert_eq!(
            REASONING_EFFORTS,
            &["low", "medium", "high", "xhigh", "max", "ultra"]
        );
        for (input, expected) in REASONING_EFFORTS
            .iter()
            .map(|value| (*value, *value))
            .chain([("minimal", "low")])
        {
            let mut spec = SpawnSpec::new("/usr/bin/codex".into(), "/tmp".into()).expect("abs");
            spec.reasoning_effort = Some(input.into());
            assert_eq!(
                spec.argv().expect(input),
                vec![
                    "app-server".to_owned(),
                    "--listen".to_owned(),
                    "stdio://".to_owned(),
                    "-c".to_owned(),
                    format!("model_reasoning_effort=\"{expected}\""),
                ],
                "{input}"
            );
        }
    }
}
