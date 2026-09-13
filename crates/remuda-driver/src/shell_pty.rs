//! Login `$SHELL` in a portable-pty (kind `terminal`, driver `shell-pty`).

use crate::binary::{BinaryPin, pin_binary};
use crate::capabilities::capability_snapshot;
use crate::driver::{Driver, DriverAck, RunHandle};
use crate::error::{DriverError, DriverResult};
use crate::profile::{Delegation, ProviderKind};
use crate::recipe::{LaunchAudit, LaunchRecipe, RecipePermission, RecipeProvider};
use crate::tty::{LocalPty, TTY_SNAPSHOT_MAX, TtyBridge, logical_keys_to_bytes};
use async_trait::async_trait;
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use remuda_protocol::{
    ApprovalAuthority, BoolLiteral, DriverInput, DriverKind, Id, InputDelivery, InstanceSpec, U64,
};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use tokio::sync::{Mutex, broadcast, mpsc};

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// Resolve `$SHELL`, falling back to `/bin/sh`.
#[must_use]
pub fn default_shell() -> PathBuf {
    std::env::var_os("SHELL")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

/// Launch options for [`ShellPtyDriver`].
#[derive(Debug, Clone)]
pub struct ShellPtyOptions {
    /// Working directory (workspace / worktree).
    pub cwd: PathBuf,
    /// Shell executable. Default [`default_shell`].
    pub shell: PathBuf,
    /// When empty, spawn `shell -l`. Otherwise spawn `args` as argv (tests).
    pub args: Vec<String>,
    /// Extra non-secret environment.
    pub extra_env: std::collections::BTreeMap<String, String>,
    /// Authenticated instance context, injected only after environment filtering.
    pub agent_mcp: Option<crate::agent_mcp::AgentMcpContext>,
    /// Initial columns.
    pub cols: u16,
    /// Initial rows.
    pub rows: u16,
}

impl ShellPtyOptions {
    /// Login shell in `cwd`.
    #[must_use]
    pub fn login(cwd: PathBuf) -> Self {
        Self {
            cwd,
            shell: default_shell(),
            args: Vec::new(),
            extra_env: std::collections::BTreeMap::new(),
            agent_mcp: None,
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
        }
    }
}

struct PtyState {
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send>>,
    output: broadcast::Sender<Vec<u8>>,
    ring: std::sync::Mutex<VecDeque<u8>>,
    cols: AtomicU16,
    rows: AtomicU16,
    closed: AtomicBool,
}

/// Plain login shell in a PTY. No agent detection.
pub struct ShellPtyDriver {
    options: ShellPtyOptions,
    inner: Mutex<Option<Arc<PtyState>>>,
}

impl ShellPtyDriver {
    /// Build an unstarted driver.
    #[must_use]
    pub fn new(options: ShellPtyOptions) -> Self {
        Self {
            options,
            inner: Mutex::new(None),
        }
    }

    async fn state(&self) -> DriverResult<Arc<PtyState>> {
        self.inner
            .lock()
            .await
            .clone()
            .ok_or(DriverError::ControlUnavailable)
    }

    /// Spawn the PTY without an [`InstanceSpec`] (Node fake registry / tests).
    pub async fn spawn(&self) -> DriverResult<RunHandle> {
        self.spawn_at(&self.options.cwd.to_string_lossy()).await
    }

    async fn spawn_at(&self, cwd: &str) -> DriverResult<RunHandle> {
        let cols = self.options.cols.max(1);
        let rows = self.options.rows.max(1);
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(pty_err)?;
        let cmd = build_command(&self.options, cwd)?;
        let child = pair.slave.spawn_command(cmd).map_err(pty_err)?;
        drop(pair.slave);
        let master = pair.master;
        let writer = master.take_writer().map_err(pty_err)?;
        let reader = master.try_clone_reader().map_err(pty_err)?;
        let (output, _) = broadcast::channel(64);
        let state = Arc::new(PtyState {
            writer: Mutex::new(writer),
            master: Mutex::new(master),
            child: Mutex::new(child),
            output: output.clone(),
            ring: std::sync::Mutex::new(VecDeque::new()),
            cols: AtomicU16::new(cols),
            rows: AtomicU16::new(rows),
            closed: AtomicBool::new(false),
        });
        let pump = Arc::clone(&state);
        std::thread::Builder::new()
            .name("remuda-shell-pty".into())
            .spawn(move || read_pty(pump, reader))
            .map_err(DriverError::Io)?;
        *self.inner.lock().await = Some(Arc::clone(&state));
        let recipe = shell_recipe(&self.options, cwd)?;
        let (tx, rx) = mpsc::channel(8);
        drop(tx);
        Ok(RunHandle::new(recipe, DriverAck::transport_written(), rx))
    }
}

#[async_trait]
impl LocalPty for PtyState {
    fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.output.subscribe()
    }

    fn snapshot(&self) -> Vec<u8> {
        self.ring
            .lock()
            .map(|ring| ring.iter().copied().collect())
            .unwrap_or_default()
    }

    async fn write_bytes(&self, bytes: &[u8]) -> DriverResult<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(DriverError::ControlUnavailable);
        }
        let mut writer = self.writer.lock().await;
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    async fn resize(&self, cols: u16, rows: u16) -> DriverResult<()> {
        if cols == 0 || rows == 0 {
            return Err(DriverError::InvalidLaunchSpec(
                "tty.resize cols and rows must be positive".into(),
            ));
        }
        let master = self.master.lock().await;
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(pty_err)?;
        self.cols.store(cols, Ordering::SeqCst);
        self.rows.store(rows, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl Driver for ShellPtyDriver {
    async fn capabilities(&self) -> DriverResult<remuda_protocol::CapabilitySnapshot> {
        let pin = pin_binary(&self.options.shell)?;
        Ok(capability_snapshot(
            DriverKind::ShellPty,
            &pin,
            U64(1),
            U64(1),
        )?)
    }

    async fn start(&self, spec: InstanceSpec) -> DriverResult<RunHandle> {
        if spec.driver != DriverKind::ShellPty {
            return Err(DriverError::InvalidLaunchSpec(
                "ShellPtyDriver requires driverKind shell-pty".into(),
            ));
        }
        self.spawn_at(&spec.cwd).await
    }

    async fn attach(&self, _native_ref: remuda_protocol::NativeRef) -> DriverResult<DriverAck> {
        let _ = self.state().await?;
        Ok(DriverAck::transport_written())
    }

    async fn send(&self, input: DriverInput) -> DriverResult<DriverAck> {
        let text = match input {
            DriverInput::Prompt(prompt) => prompt
                .blocks
                .iter()
                .filter_map(|block| match block {
                    remuda_protocol::ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => {
                return Err(DriverError::CapabilityUnsupported(
                    "shell-pty only accepts prompt input as typed bytes".into(),
                ));
            }
        };
        let mut bytes = text.into_bytes();
        if !bytes.ends_with(b"\r") && !bytes.ends_with(b"\n") {
            bytes.push(b'\r');
        }
        self.write_tty(&bytes).await
    }

    async fn send_keys(&self, keys: Vec<String>) -> DriverResult<DriverAck> {
        self.write_tty(&logical_keys_to_bytes(&keys)).await
    }

    async fn write_tty(&self, bytes: &[u8]) -> DriverResult<DriverAck> {
        self.state().await?.write_bytes(bytes).await?;
        Ok(DriverAck::transport_written())
    }

    async fn resize_tty(&self, cols: u16, rows: u16) -> DriverResult<DriverAck> {
        self.state().await?.resize(cols, rows).await?;
        Ok(DriverAck::transport_written())
    }

    async fn tty_bridge(&self) -> Option<TtyBridge> {
        let state = self.inner.lock().await.clone()?;
        Some(TtyBridge::Local(state))
    }

    async fn cancel(&self) -> DriverResult<DriverAck> {
        self.write_tty(b"\x03").await
    }

    async fn respond_interaction(
        &self,
        _id: remuda_protocol::InteractionId,
        _answer: remuda_protocol::InteractionAnswer,
    ) -> DriverResult<DriverAck> {
        Err(DriverError::CapabilityUnsupported(
            "shell-pty has no structured interaction channel".into(),
        ))
    }

    async fn close(&self) -> DriverResult<DriverAck> {
        let Some(state) = self.inner.lock().await.take() else {
            return Ok(DriverAck::not_dispatched());
        };
        state.closed.store(true, Ordering::SeqCst);
        if let Ok(mut child) = state.child.try_lock() {
            let _ = child.kill();
        }
        Ok(DriverAck::not_dispatched())
    }

    async fn resume(&self, _native_ref: remuda_protocol::NativeRef) -> DriverResult<RunHandle> {
        Err(DriverError::CapabilityUnsupported(
            "shell-pty has no semantic resume".into(),
        ))
    }
}

fn pty_err(err: impl std::fmt::Display) -> DriverError {
    DriverError::Io(io::Error::other(err.to_string()))
}

fn build_command(options: &ShellPtyOptions, spec_cwd: &str) -> DriverResult<CommandBuilder> {
    let cwd = if Path::new(spec_cwd).is_dir() {
        PathBuf::from(spec_cwd)
    } else {
        options.cwd.clone()
    };
    let mut cmd = if options.args.is_empty() {
        let mut cmd = CommandBuilder::new(&options.shell);
        cmd.arg("-l");
        cmd
    } else {
        let mut cmd = CommandBuilder::new(&options.args[0]);
        for arg in options.args.iter().skip(1) {
            cmd.arg(arg);
        }
        cmd
    };
    cmd.cwd(cwd);
    cmd.env_clear();
    for (key, value) in crate::child_env::base_env() {
        cmd.env(key, value);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    for (key, value) in &options.extra_env {
        if !crate::child_env::is_denied(key) {
            cmd.env(key, value);
        }
    }
    if let Some(context) = &options.agent_mcp {
        for (key, value) in context.environment()? {
            cmd.env(key, value);
        }
    }
    Ok(cmd)
}

fn read_pty(state: Arc<PtyState>, mut reader: Box<dyn Read + Send>) {
    let mut buf = [0_u8; 4096];
    loop {
        if state.closed.load(Ordering::SeqCst) {
            break;
        }
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = buf[..n].to_vec();
                if let Ok(mut ring) = state.ring.lock() {
                    ring.extend(chunk.iter().copied());
                    while ring.len() > TTY_SNAPSHOT_MAX {
                        ring.pop_front();
                    }
                }
                let _ = state.output.send(chunk);
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn shell_recipe(options: &ShellPtyOptions, cwd: &str) -> DriverResult<LaunchRecipe> {
    let pin = pin_shell(&options.shell, &options.args)?;
    let argv = if options.args.is_empty() {
        vec![options.shell.to_string_lossy().into_owned(), "-l".into()]
    } else {
        options.args.clone()
    };
    Ok(LaunchRecipe {
        launch_id: Id::new("launch")?,
        driver: DriverKind::ShellPty,
        binary: pin,
        cwd: cwd.to_owned(),
        argv,
        env_allowlist: vec![],
        materialized_files: vec![],
        setting_sources: vec![],
        session_id: None,
        native_home: cwd.to_owned(),
        input_delivery: InputDelivery::Tty,
        provider: RecipeProvider {
            profile_id: Id::new("pvp")?,
            kind: ProviderKind::Anthropic,
            base_url: String::new(),
            delegation: Delegation::None,
            secret_ref: None,
            model_requested: String::new(),
        },
        permission: RecipePermission {
            cli_mode: None,
            prompts: None,
            extra_flags: vec![],
        },
        technical_debt: vec![],
        audit: LaunchAudit {
            env_names: vec!["TERM".into(), "COLORTERM".into()],
            credential_refs: vec![],
            redacted_argv: argv_redacted(&options.args, &options.shell),
            settings_digest: None,
            prohibited_options_checked: BoolLiteral,
            approval_authority: ApprovalAuthority::NativeTty,
        },
    })
}

fn pin_shell(shell: &Path, args: &[String]) -> DriverResult<BinaryPin> {
    let path = if args.is_empty() {
        shell
    } else {
        Path::new(&args[0])
    };
    pin_binary(path)
}

fn argv_redacted(args: &[String], shell: &Path) -> Vec<String> {
    if args.is_empty() {
        vec![shell.to_string_lossy().into_owned(), "-l".into()]
    } else {
        args.to_vec()
    }
}
