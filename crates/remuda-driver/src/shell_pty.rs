//! Login `$SHELL` in a portable-pty (kind `terminal`, driver `shell-pty`).
//!
//! The shell itself has no agent semantics, but a human can start one inside it.
//! [`promotion`] watches the PTY's foreground process group and, when a known
//! agent CLI takes it over, promotes the instance (D-025): `kind` becomes that
//! agent, the driver stays `shell-pty`, and for Claude the native transcript is
//! tailed so the structured view is the real conversation rather than a screen
//! scrape.

use crate::binary::{BinaryPin, pin_binary};
use crate::capabilities::capability_snapshot;
use crate::driver::{Driver, DriverAck, RunHandle};
use crate::error::{DriverError, DriverResult};
use crate::profile::{Delegation, ProviderKind};
use crate::promote::{Detected, ProcessTable, ScreenStatus, SystemProcessTable};
use crate::recipe::{LaunchAudit, LaunchRecipe, RecipePermission, RecipeProvider};
use crate::tty::{LocalPty, TTY_SNAPSHOT_MAX, TtyBridge, logical_keys_to_bytes};
use async_trait::async_trait;
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use remuda_protocol::{
    AgentKind, ApprovalAuthority, BoolLiteral, DriverInput, DriverKind, HostId, Id, InputDelivery,
    InstanceId, InstanceSpec, RunId, U64,
};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::task::JoinHandle;

mod promotion;

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// How often the promotion poller samples the foreground process group.
/// Detection is then visible within ~2 poll intervals (D-025 budget: ~2 s).
const PROMOTE_POLL: std::time::Duration = std::time::Duration::from_millis(800);

/// Gap between a promoted prompt's body and its Enter.
///
/// The TUI debounces its composer input; an Enter that arrives before it has
/// settled is dropped along with the body. 120 ms was measured as too short
/// against a live Claude TUI and 500 ms as reliable (terminal-promote-1).
const SUBMIT_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

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
    /// Watch the foreground process group and promote on a known agent CLI
    /// (D-025). Off by default so unit tests get a plain shell.
    pub promote: bool,
    /// Claude config dir used to locate a promoted session's transcript.
    /// Defaults to `$HOME/.claude`.
    pub claude_home: Option<PathBuf>,
    /// Hook path for this instance (D-028 §4.2), when `REMUDA_PTY_HOOKS` is on.
    ///
    /// `None` — the P1 default — means no socket, no overlay and no shim: a
    /// Node that has not opted in behaves exactly as it did before.
    pub hooks: Option<HookConfig>,
}

/// What the driver needs to stand up this instance's hook path.
#[derive(Debug, Clone)]
pub struct HookConfig {
    /// `<data dir>/instances/<id>`, where the socket and launch dir live.
    pub instance_dir: PathBuf,
    /// The `remuda` binary the relay runs as.
    pub relay_binary: PathBuf,
    /// Renderer pinned for the session (§9.2).
    pub tui: crate::launch::TuiMode,
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
            promote: false,
            claude_home: None,
            hooks: None,
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

/// Login shell in a PTY, with optional agent promotion (D-025).
pub struct ShellPtyDriver {
    options: ShellPtyOptions,
    inner: Mutex<Option<Arc<PtyState>>>,
    /// Set by the poller while a known agent CLI holds the foreground.
    promoted: Arc<std::sync::Mutex<Option<Detected>>>,
    /// Screen-derived readiness of that agent, when promoted.
    status: Arc<std::sync::Mutex<Option<ScreenStatus>>>,
    /// Promotion poller, stopped on close.
    poller: Mutex<Option<JoinHandle<()>>>,
    /// Process table behind detection; a fixture in tests.
    table: Arc<dyn ProcessTable>,
    /// Live hook path, when the instance opted in. Dropped on close, which
    /// unbinds the socket.
    hooks: Mutex<Option<Arc<crate::launch::HookSession>>>,
    seq: Arc<AtomicU64>,
}

impl ShellPtyDriver {
    /// Build an unstarted driver.
    #[must_use]
    pub fn new(options: ShellPtyOptions) -> Self {
        Self::with_process_table(options, Arc::new(SystemProcessTable))
    }

    /// Build a driver whose promotion detection reads `table` instead of the
    /// host process table. Tests drive detection from a fixture this way.
    #[must_use]
    pub fn with_process_table(options: ShellPtyOptions, table: Arc<dyn ProcessTable>) -> Self {
        Self {
            options,
            inner: Mutex::new(None),
            promoted: Arc::new(std::sync::Mutex::new(None)),
            status: Arc::new(std::sync::Mutex::new(None)),
            poller: Mutex::new(None),
            table,
            hooks: Mutex::new(None),
            seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Currently promoted agent kind, if the PTY's foreground is an agent CLI.
    #[must_use]
    pub fn promoted_kind(&self) -> Option<AgentKind> {
        self.promoted
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|found| found.kind))
    }

    /// Stand up this instance's hook path, when one was configured.
    ///
    /// The [`SignalBus`](remuda_signal::SignalBus) shares the promotion
    /// poller's `seq` counter and identity, so hook and screen observations
    /// land in one ordered stream rather than two that have to be interleaved
    /// after the fact.
    fn start_hooks(
        &self,
        ctx: &promotion::PromoteCtx,
        events: &mpsc::Sender<remuda_protocol::Observation>,
    ) -> DriverResult<Option<Arc<crate::launch::HookSession>>> {
        let Some(config) = self.options.hooks.clone() else {
            return Ok(None);
        };
        let bus = Arc::new(remuda_signal::SignalBus::new(
            remuda_signal::BusContext {
                instance_id: ctx.instance_id.clone(),
                host_id: ctx.host_id.clone(),
                journal_id: ctx.journal_id.clone(),
                run_id: ctx.run_id.clone(),
                driver_kind: DriverKind::ShellPty,
                adapter_version: crate::capabilities::ADAPTER_VERSION.to_owned(),
            },
            events.clone(),
            Arc::clone(&self.seq),
        ));
        Ok(Some(Arc::new(crate::launch::HookSession::start(
            &crate::launch::HookSessionOptions {
                instance_dir: config.instance_dir,
                relay_binary: config.relay_binary,
                tui: config.tui,
                base_settings: None,
            },
            bus,
        )?)))
    }

    /// The native session a hook reported, if `SessionStart` has fired.
    pub async fn hook_session(&self) -> Option<remuda_signal::SessionBinding> {
        self.hooks.lock().await.as_ref()?.binding()
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
        let cwd = self.options.cwd.to_string_lossy().into_owned();
        self.spawn_at(&cwd, None).await
    }

    async fn spawn_at(&self, cwd: &str, spec: Option<&InstanceSpec>) -> DriverResult<RunHandle> {
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
        // The event channel is created before the child so the hook socket is
        // already accepting when the shell starts: a human who types `claude`
        // immediately must not lose their SessionStart to a race.
        let (tx, rx) = mpsc::channel(256);
        let hook_ctx = promote_ctx(&self.options, cwd, spec)?;
        let hooks = self.start_hooks(&hook_ctx, &tx)?;
        let cmd = build_command(&self.options, cwd, hooks.as_ref())?;
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
        *self.hooks.lock().await = hooks;
        let recipe = shell_recipe(&self.options, cwd)?;
        if self.options.promote {
            *self.poller.lock().await = Some(promotion::spawn(
                Arc::clone(&state),
                hook_ctx,
                Arc::clone(&self.table),
                Arc::clone(&self.promoted),
                Arc::clone(&self.status),
                tx,
                Arc::clone(&self.seq),
            ));
        } else {
            drop(tx);
        }
        Ok(RunHandle::new(recipe, DriverAck::transport_written(), rx))
    }
}

/// Identity the promotion poller and the signal bus both stamp Observations
/// with.
///
/// One context for both so hook and screen evidence share a journal, a run and
/// a sequence counter. Node rebinds instance/journal/host on commit; the driver
/// only needs locally consistent ids (same contract as claude-pty).
fn promote_ctx(
    options: &ShellPtyOptions,
    cwd: &str,
    spec: Option<&InstanceSpec>,
) -> DriverResult<promotion::PromoteCtx> {
    Ok(promotion::PromoteCtx {
        instance_id: InstanceId::new(),
        host_id: spec.map_or_else(HostId::new, |spec| spec.host.clone()),
        journal_id: Id::new("obj")?,
        run_id: RunId::new(),
        cwd: PathBuf::from(cwd),
        claude_home: options
            .claude_home
            .clone()
            .unwrap_or_else(default_claude_home),
    })
}

/// `$CLAUDE_CONFIG_DIR`, else `$HOME/.claude`.
fn default_claude_home() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
        .unwrap_or_else(|| PathBuf::from(".claude"))
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
        self.spawn_at(&spec.cwd.clone(), Some(&spec)).await
    }

    /// A plain shell always takes bytes. A promoted agent TUI only takes a
    /// prompt when its screen says it is idle: typing into a running turn or a
    /// native dialog would be swallowed or would answer the dialog by accident,
    /// so the D-022 queue holds the prompt until the screen is ready.
    async fn wait_control(&self) -> DriverResult<()> {
        let _ = self.state().await?;
        if self.promoted_kind().is_none() {
            return Ok(());
        }
        match self.status.lock().ok().and_then(|slot| *slot) {
            Some(ScreenStatus::Idle) => Ok(()),
            // Booting, working, or blocked: not ready, and never a reason to
            // tear down a healthy PTY.
            _ => Err(DriverError::ControlUnavailable),
        }
    }

    async fn attach(&self, _native_ref: remuda_protocol::NativeRef) -> DriverResult<DriverAck> {
        let _ = self.state().await?;
        Ok(DriverAck::transport_written())
    }

    async fn send(&self, input: DriverInput) -> DriverResult<DriverAck> {
        let text = match input {
            DriverInput::Prompt(prompt) => {
                let mut text = prompt
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        remuda_protocol::ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                // D-027: this is a raw shell, not an agent. Nothing here can
                // read an image, so an attachment only contributes its path —
                // shell-quoted, because these bytes are typed into a terminal.
                for attachment in crate::attachment::attachments_of(&prompt.blocks) {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(&shell_quote(&attachment.display_path()));
                }
                text
            }
            _ => {
                return Err(DriverError::CapabilityUnsupported(
                    "shell-pty only accepts prompt input as typed bytes".into(),
                ));
            }
        };
        let promoted = self.promoted_kind().is_some();
        let (body, submit) = prompt_writes(&text, promoted);
        self.write_tty(&body).await?;
        let Some(submit) = submit else {
            return Ok(DriverAck::transport_written());
        };
        // A promoted agent TUI reads its composer and its Enter as two separate
        // reads: a single write carrying both is accepted as bytes but never
        // submitted. Verified against a live Claude TUI (terminal-promote-1).
        tokio::time::sleep(SUBMIT_DELAY).await;
        self.write_tty(&submit).await
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
        if let Some(poller) = self.poller.lock().await.take() {
            poller.abort();
        }
        if let Ok(mut slot) = self.promoted.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = self.status.lock() {
            *slot = None;
        }
        // Unbinds the socket and removes the socket file. The overlay and shims
        // stay for `instance.purge` to remove with the rest of the instance
        // directory — they are launch audit evidence until then.
        self.hooks.lock().await.take();
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

fn build_command(
    options: &ShellPtyOptions,
    spec_cwd: &str,
    hooks: Option<&Arc<crate::launch::HookSession>>,
) -> DriverResult<CommandBuilder> {
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
    let base = crate::child_env::base_env();
    for (key, value) in &base {
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
    // Last, and deliberately past the allowlist: the shim PATH and the hook
    // credential are values the *driver computed*, not values it inherited
    // (D-028 §4.2). The inherit allowlist exists to keep the Node's own
    // credentials and any `LD_PRELOAD`/proxy/CA injection out of a process the
    // model can read; it is not the channel for something we minted ourselves.
    if let Some(session) = hooks {
        let inherited = base
            .get("PATH")
            .cloned()
            .unwrap_or_else(|| std::env::var("PATH").unwrap_or_default());
        for (key, value) in session.child_env(&inherited) {
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

/// Encode a prompt for the PTY as (body, optional separate submit).
///
/// A plain shell takes one write: the text with a trailing `\r`, as before.
/// A promoted agent TUI takes two — the composer body, then the Enter — because
/// it consumes its input line and its submit key as separate reads. Multi-line
/// bodies are bracketed-paste wrapped so the TUI treats them as one paste
/// rather than one submit per line.
fn prompt_writes(text: &str, promoted: bool) -> (Vec<u8>, Option<Vec<u8>>) {
    if !promoted {
        let mut bytes = text.as_bytes().to_vec();
        if !bytes.ends_with(b"\r") && !bytes.ends_with(b"\n") {
            bytes.push(b'\r');
        }
        return (bytes, None);
    }
    let body = text.trim_end_matches(['\r', '\n']);
    let multiline = body.contains('\n') || body.contains('\r');
    let mut bytes = Vec::with_capacity(body.len() + 16);
    if multiline {
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend_from_slice(body.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
    } else {
        bytes.extend_from_slice(body.as_bytes());
    }
    (bytes, Some(vec![b'\r']))
}

/// Single-quote a path for a POSIX shell, closing and reopening the quote
/// around any embedded single quote.
fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_shell_prompt_is_one_write_of_text_plus_a_carriage_return() {
        assert_eq!(prompt_writes("ls -la", false), (b"ls -la\r".to_vec(), None));
        // An explicit terminator is not doubled.
        assert_eq!(prompt_writes("ls\n", false), (b"ls\n".to_vec(), None));
    }

    #[test]
    fn a_promoted_prompt_sends_its_enter_as_a_separate_write() {
        // A live Claude TUI accepts "text\r" as bytes but never submits it; the
        // Enter has to arrive as its own read (terminal-promote-1).
        let (body, submit) = prompt_writes("hello", true);
        assert_eq!(body, b"hello".to_vec());
        assert_eq!(submit, Some(b"\r".to_vec()));
    }

    #[test]
    fn a_promoted_multiline_prompt_is_bracketed_so_the_tui_sees_one_paste() {
        let (body, submit) = prompt_writes("first\nsecond", true);
        assert_eq!(
            body,
            b"\x1b[200~first\nsecond\x1b[201~".to_vec(),
            "multi-line prompts must not submit once per line"
        );
        assert_eq!(submit, Some(b"\r".to_vec()));
    }

    #[test]
    fn a_trailing_newline_in_a_promoted_prompt_does_not_double_submit() {
        let (body, submit) = prompt_writes("hello\n", true);
        assert_eq!(body, b"hello".to_vec(), "the terminator becomes the submit");
        assert_eq!(submit, Some(b"\r".to_vec()));
    }

    #[tokio::test]
    async fn a_driver_without_hooks_configured_starts_nothing() {
        // P1 default: a Node that has not set REMUDA_PTY_HOOKS behaves exactly
        // as it did before — no socket, no overlay, no shim.
        let dir = tempfile::tempdir().unwrap();
        let mut options = ShellPtyOptions::login(dir.path().to_path_buf());
        options.args = vec!["/bin/sh".into(), "-c".into(), "exit 0".into()];
        let driver = ShellPtyDriver::new(options);
        driver.spawn().await.expect("spawns");
        assert!(driver.hook_session().await.is_none());
        assert!(
            !dir.path().join("hook.sock").exists(),
            "no hook path was configured, so none should exist"
        );
        let _ = Driver::close(&driver).await;
    }

    #[tokio::test]
    async fn a_configured_hook_path_binds_a_socket_the_child_can_reach() {
        // The seam P2 and P3 read the native session through. Exercised here so
        // the wiring cannot rot while it has no production caller yet.
        let dir = tempfile::tempdir().unwrap();
        let instance_dir = dir.path().join("instance");
        let mut options = ShellPtyOptions::login(dir.path().to_path_buf());
        options.args = vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()];
        options.hooks = Some(HookConfig {
            instance_dir: instance_dir.clone(),
            relay_binary: PathBuf::from("/nonexistent/remuda"),
            tui: crate::launch::TuiMode::Fullscreen,
        });
        let driver = ShellPtyDriver::new(options);
        driver.spawn().await.expect("spawns");
        assert!(
            instance_dir.join("hook.sock").exists(),
            "socket is listening"
        );
        assert!(instance_dir.join("launch/settings.json").is_file());
        assert!(instance_dir.join("launch/bin/claude").is_file());
        // Nothing has reported a session yet.
        assert!(driver.hook_session().await.is_none());
        let _ = Driver::close(&driver).await;
        assert!(
            !instance_dir.join("hook.sock").exists(),
            "close must unbind the socket"
        );
    }

    #[test]
    fn paths_are_quoted_for_a_posix_shell() {
        assert_eq!(shell_quote("/data/shot.png"), "'/data/shot.png'");
        assert_eq!(
            shell_quote("/data/a b/shot.png"),
            "'/data/a b/shot.png'",
            "a space must not split the argument"
        );
        assert_eq!(
            shell_quote("/data/it's.png"),
            "'/data/it'\\''s.png'",
            "an embedded quote must not end the quoting"
        );
    }
}
