//! A portable-pty running either a login `$SHELL` or an agent CLI
//! (driver `shell-pty`).
//!
//! D-028 §1.0: *an agent session is a terminal session with the agent command
//! running in it*. This driver is where that stops being a slogan. It has one
//! spawn path, one byte pump, one promotion supervisor and one stop ladder;
//! the only thing that differs between "Remuda started `claude`" and "a human
//! typed `claude`" is [`launch::Target`] — whether the first process in the PTY
//! is the agent or the shell that the human then typed the agent into.
//!
//! Everything downstream is deliberately blind to that difference. [`promotion`]
//! watches the foreground process group either way, so a Remuda-launched agent
//! is *promoted* by exactly the same code that promotes a hand-typed one
//! (§1.0 rule 2: promotion is the only detection path, no shortcuts for the
//! launch we happen to control). The journal consequence is the acceptance
//! criterion: the two paths produce the same event stream, differing only in
//! `launchedBy`.
//!
//! Submodules:
//!
//! * [`launch`] — what to run and how to spell it (§5.1, §5.6).
//! * [`send`] — the prompt ready ladder (§5.2).
//! * [`keys`] — per-harness steer / queue / interrupt semantics (§6).
//! * [`lifecycle`] — the process-group stop ladder and exit detection
//!   (§5.3, §5.5).
//! * [`promotion`] — D-025 detection and transcript hydration.

use crate::binary::{BinaryPin, pin_binary};
use crate::capabilities::capability_snapshot;
use crate::driver::{Driver, DriverAck, RunHandle};
use crate::error::{DriverError, DriverResult};
use crate::profile::{Delegation, ProviderKind};
use crate::promote::{Detected, ProcessTable, ScreenStatus, SystemProcessTable};
use crate::recipe::{LaunchAudit, LaunchRecipe, RecipePermission, RecipeProvider};
use crate::tty::{LocalPty, PtySnapshot, TTY_SNAPSHOT_MAX, TtyBridge, logical_keys_to_bytes};
use async_trait::async_trait;
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use remuda_protocol::{
    AgentKind, ApprovalAuthority, BoolLiteral, Completeness, DriverInput, DriverKind, HostId, Id,
    InputDelivery, InstanceId, InstanceSpec, RunId, SourceChannel, U64,
};
use remuda_screen::{Emulator, ModeSet, ScreenGrid};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU64, Ordering};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::task::JoinHandle;

pub mod answer;
pub mod keys;
pub mod launch;
pub mod lifecycle;
mod promotion;
pub mod send;

pub use launch::{AgentLaunch, CARRIER_ENV, Target, native_carrier_enabled};
pub use lifecycle::{ExitEvidence, StopOutcome, StopRung};
pub use send::ReadyEvidence;

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// Env flag that turns the terminal emulator on (D-028 §13 P0).
///
/// Off by default this phase: with it off the ring is the only screen source
/// and every signature is byte-identical to pre-D-028. Rollback is `unset` and
/// a Node restart — no schema migration, orthogonal to the other four flags
/// (§13 conflict rule ⑤).
pub const EMULATOR_ENV: &str = "REMUDA_PTY_EMULATOR";

/// Whether [`EMULATOR_ENV`] asks for the emulator.
#[must_use]
pub fn emulator_enabled() -> bool {
    matches!(
        std::env::var(EMULATOR_ENV).as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    )
}

/// How often the promotion poller samples the foreground process group.
/// Detection is then visible within ~2 poll intervals (D-025 budget: ~2 s).
const PROMOTE_POLL: std::time::Duration = std::time::Duration::from_millis(800);

/// How often the exit waiter asks whether the child has been reaped (§5.5).
const EXIT_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// How long EOF waits for the reaper to upgrade it to a real status (§5.5).
///
/// Well inside §5.5's 2 s budget from process death to lifecycle event, and
/// long enough for a normally-exiting child to be reaped.
const EXIT_STATUS_GRACE: std::time::Duration = std::time::Duration::from_millis(400);

/// How long `close` keeps waiting for an exiting leader (macOS state `E`) to
/// become reapable after the stop ladder has confirmed the group is dead.
///
/// The normal `E` → zombie transition takes milliseconds; this generous bound
/// covers a loaded machine without stalling a close indefinitely on a process
/// wedged in an uninterruptible kernel exit (which only init can outlast).
const REAP_EXITING_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Native lifecycle name for a PTY process that ended (§5.5).
pub const NATIVE_EXIT: &str = "native_exit";

/// Native lifecycle name for a turn interrupt (§5.3).
pub const NATIVE_INTERRUPTED: &str = "turn_interrupted";

/// Native lifecycle name for the process stop ladder's outcome (§5.3).
pub const NATIVE_STOPPED: &str = "process_stopped";

/// Gap between the two keys of a multi-key interrupt.
///
/// Grok's cancel is `Ctrl+C` twice, where the first clears the draft. Written
/// back to back they arrive in one `read` and the TUI sees a single keypress;
/// the gap is what makes them two.
const INTERRUPT_KEY_GAP: std::time::Duration = std::time::Duration::from_millis(60);

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
    /// Launching user's real Claude config dir to seed a scoped native home
    /// from (native-config-1, 2026-09-16).
    ///
    /// When the agent is pinned at a Remuda-managed native home, the harness's
    /// user-settings layer points at an empty directory; the driver copies the
    /// user's effective `settings.json` / `settings.local.json` out of this
    /// dir into the per-instance overlay so gateway models, `statusLine`,
    /// plugins and friends behave like a plain terminal.
    ///
    /// `None` when the child reads its user settings natively — an inherited
    /// operator home or an explicitly chosen `CLAUDE_CONFIG_DIR` — so hooks
    /// from a copied layer can never fire twice.
    pub user_settings_home: Option<PathBuf>,
    /// Hook path for this instance (D-028 §4.2), when `REMUDA_PTY_HOOKS` is on.
    ///
    /// `None` — the P1 default — means no socket, no overlay and no shim: a
    /// Node that has not opted in behaves exactly as it did before.
    pub hooks: Option<HookConfig>,
    /// Run a terminal emulator alongside the byte ring (D-028 §4.1, §13 P0).
    ///
    /// Defaults to [`emulator_enabled`], so production is driven by
    /// [`EMULATOR_ENV`] and off unless it is set. Tests set it directly rather
    /// than mutating process environment, which this workspace forbids and
    /// which would leak across tests sharing a process anyway.
    ///
    /// Orthogonal to `hooks`: §13 rule ⑤ makes the five flags independent, so
    /// either, both or neither may be on.
    pub emulator: bool,
    /// What the PTY runs first (D-028 §5.1).
    ///
    /// [`Target::Shell`] is the pre-D-028 behaviour and stays the default, so a
    /// Node that has not opted into `REMUDA_PTY_CARRIER=native` gets exactly
    /// the login shell it got before. [`Target::Agent`] launches the agent CLI
    /// directly from a materialized recipe.
    pub target: Target,
    /// Pin the agent at `agent.native_home` via its harness config-dir env var
    /// (`CLAUDE_CONFIG_DIR` for Claude, the recipe's home var otherwise).
    ///
    /// Set by the Node whenever it prepared a scoped native home. Unset when
    /// the launch inherits the operator's default config — exporting a pin then
    /// would point the CLI at an empty directory and it would report "not
    /// logged in". This pin is also what makes the transcript poller look in
    /// the same directory the harness writes its session files into.
    pub pin_native_home: bool,
    /// Whether the promotion supervisor may answer Claude's exact folder-trust
    /// dialog with its documented key sequence (once per foreground agent).
    ///
    /// The Node grants this only for a cwd inside a registered workspace root
    /// (the same gate the herdr carrier uses, D-022). Without it the dialog is
    /// detected, reported as blocked, and left for a human.
    pub auto_trust_workspace: bool,
    /// Recipe inputs for [`Target::Agent`]. Required for that target and
    /// ignored for a shell.
    ///
    /// Boxed: it holds a `ProviderProfile`, and `ShellPtyOptions` is cloned
    /// into every driver.
    pub agent: Option<Box<AgentLaunch>>,
    /// The Node instance this driver serves, when a Node built it.
    ///
    /// Every observation this driver mints is scoped to it, and — unlike the
    /// envelope identity the store rewrites on append — the *derived* node ids
    /// inside the payloads are not rewritable: `remuda_signal` derives each
    /// hook tool node as `Id::derive("obj", <instance id>, <tool_use_id>)`,
    /// and the Node's workflow producer derives `workflow.run.toolCallId` from
    /// the same native id under the real instance. Minting a throwaway id here
    /// made those two derivations disagree, so a run could never name a tool
    /// call that existed and every subagent row stayed outside its Workflow
    /// row (c-wfdrill2 C).
    ///
    /// `None` only for a driver built outside a Node (tests,
    /// [`ShellPtyDriver::spawn`]), which mints one.
    pub instance_id: Option<InstanceId>,
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
            user_settings_home: None,
            hooks: None,
            emulator: emulator_enabled(),
            target: Target::Shell,
            pin_native_home: false,
            auto_trust_workspace: false,
            agent: None,
            instance_id: None,
        }
    }

    /// Instance identity every observation of this launch is scoped to.
    ///
    /// Public because the Node's own producers must derive under the same
    /// scope; see [`Self::instance_id`].
    #[must_use]
    pub fn instance_scope(&self) -> InstanceId {
        self.instance_id.clone().unwrap_or_default()
    }

    /// Launch `kind`'s CLI directly in the PTY instead of a login shell
    /// (D-028 §5.1).
    ///
    /// Promotion is switched on unconditionally: §1.0 rule 2 makes D-025 the
    /// only path by which any agent — including one Remuda started itself —
    /// becomes a promoted instance. Detecting our own launch through the same
    /// poller is what makes the two journals comparable.
    #[must_use]
    pub fn agent(cwd: PathBuf, kind: AgentKind, launch: AgentLaunch) -> Self {
        Self {
            promote: true,
            target: Target::Agent { kind, resume: None },
            agent: Some(Box::new(launch)),
            ..Self::login(cwd)
        }
    }

    /// Continue native session `session_id` (§5.6).
    ///
    /// Resume is not a different carrier or a different code path: it is this
    /// same launch with the harness's resume flag prefilled.
    #[must_use]
    pub fn resuming(mut self, session_id: impl Into<String>) -> Self {
        if let Target::Agent { kind, .. } = self.target {
            self.target = Target::Agent {
                kind,
                resume: Some(session_id.into()),
            };
        }
        self
    }
}

struct PtyState {
    /// Write half of the master, taken from it at spawn.
    ///
    /// `None` once the stop ladder has released the PTY. This is a *separate
    /// dup of the master fd*, so dropping the master box alone does not close
    /// the PTY — see [`PtyState::master`].
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    /// The PTY master. `None` once the stop ladder has dropped it.
    ///
    /// §5.3 step 2 pairs `SIGHUP` with closing the master, and that has to be
    /// a real close: while any fd to the master is open, the slave's other end
    /// stays open too, so a shell blocked on input never sees the hangup and
    /// the ladder escalates to `SIGKILL` for no reason.
    ///
    /// "The master" is three file descriptors, not one: `portable-pty` dups it
    /// for [`MasterPty::take_writer`] and again for
    /// [`MasterPty::try_clone_reader`]. Dropping only this box leaves the other
    /// two open, the slave never sees EOF, and on macOS the SIGKILLed leader
    /// parks in `E` (exiting) forever instead of becoming a reapable zombie —
    /// the `?Es` process the demo left behind on every delete. Closing the PTY
    /// therefore means releasing all three; [`ShellPtyDriver::stop_tree`] drops
    /// the two owned halves and the reader closes its own dup on EOF.
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    child: Mutex<Box<dyn portable_pty::Child + Send>>,
    /// Signals the child independently of whoever holds `child`.
    ///
    /// The reader thread and the exit waiter both hold `child` for long
    /// stretches, which is why the pre-D-028 `close` — `child.try_lock()` then
    /// skip on failure — usually sent nothing at all. The ladder in
    /// [`lifecycle`] signals the process *group* and never needs this, but it
    /// is kept as the last resort for a platform with no process groups.
    killer: Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>,
    /// Process group to signal. The child is a `setsid` session leader, so its
    /// pid is the group id, and everything it starts inherits the group.
    pgid: AtomicI32,
    output: broadcast::Sender<Vec<u8>>,
    ring: std::sync::Mutex<VecDeque<u8>>,
    interruption_output: std::sync::Mutex<promotion::InterruptionOutput>,
    /// Terminal emulator, when [`EMULATOR_ENV`] is on. Fed the same bytes as
    /// the ring, never instead of it: §4.6 keeps the ring as the fallback for
    /// any emulator failure, so the emulator can never cost a byte of output.
    emulator: Option<std::sync::Mutex<Emulator>>,
    cols: AtomicU16,
    rows: AtomicU16,
    closed: AtomicBool,
    /// Monotonic count of PTY reads, sampled to detect screen quiescence
    /// (§5.2). A counter rather than a timestamp because the reader thread is
    /// not async and this must stay lock-free on the hot path.
    reads: AtomicU64,
}

impl PtyState {
    fn screen_evidence(&self) -> (u64, ScreenGrid) {
        // The reader holds this same lock while updating both the rendered
        // grid and marker count. Sampling them separately can consume a new
        // marker against the preceding frame and lose its confirmation.
        let output = self.interruption_output.lock().ok();
        let count = output.as_ref().map_or(0, |output| output.count());
        (count, self.screen_grid())
    }

    /// Current screen as a grid: the emulator's when it is on, otherwise the
    /// ANSI-stripped ring tail, which is what the matchers read before D-028.
    pub(super) fn screen_grid(&self) -> ScreenGrid {
        if let Some(emulator) = &self.emulator {
            match emulator.lock() {
                Ok(emulator) => return emulator.grid(),
                Err(_) => tracing::warn!(
                    "pty emulator lock poisoned; falling back to the raw ring for signatures"
                ),
            }
        }
        ScreenGrid::from_raw(&remuda_screen::screen_tail(&self.ring_text()))
    }

    /// DECSET modes the emulator observed, or `None` when it is off.
    ///
    /// `None` is not "no modes are set" — it is "nobody was watching", which is
    /// why [`send::encode`] treats it as a reason *not* to bracket rather than
    /// as a negative observation.
    fn modes(&self) -> Option<ModeSet> {
        let emulator = self.emulator.as_ref()?;
        match emulator.lock() {
            Ok(emulator) => Some(emulator.modes()),
            Err(_) => {
                tracing::warn!("pty emulator lock poisoned; treating DECSET state as unobserved");
                None
            }
        }
    }

    /// Wait for the screen to stop changing (§5.2).
    ///
    /// Returns once no bytes have arrived for [`send::QUIESCENCE`], or after
    /// [`send::QUIESCENCE_CAP`] regardless — a TUI painting a spinner is never
    /// quiet, and a prompt delivered late beats one never delivered.
    async fn await_quiescence(&self) {
        let deadline = tokio::time::Instant::now() + send::QUIESCENCE_CAP;
        let mut last = self.reads.load(Ordering::SeqCst);
        let mut quiet_since = tokio::time::Instant::now();
        loop {
            tokio::time::sleep(send::QUIESCENCE_POLL).await;
            let now = tokio::time::Instant::now();
            let reads = self.reads.load(Ordering::SeqCst);
            if reads != last {
                last = reads;
                quiet_since = now;
            } else if now.duration_since(quiet_since) >= send::QUIESCENCE {
                return;
            }
            if now >= deadline {
                tracing::debug!(
                    "screen never went quiet within {:?}; submitting anyway",
                    send::QUIESCENCE_CAP
                );
                return;
            }
        }
    }

    /// Ring contents as lossy UTF-8.
    fn ring_text(&self) -> String {
        self.ring
            .lock()
            .ok()
            .map(|ring| {
                String::from_utf8_lossy(&ring.iter().copied().collect::<Vec<_>>()).into_owned()
            })
            .unwrap_or_default()
    }
}

/// How long the screen fallback waits for the dialog to clear before it
/// refuses to call the decision applied (§14 risk 1).
///
/// Generous enough for a TUI repaint on a loaded machine, short enough that a
/// caller is not left hanging: the answer "we could not confirm it" is useful
/// promptly, and the operator can look at the terminal.
const FALLBACK_CONFIRM_WINDOW: std::time::Duration = std::time::Duration::from_secs(3);

/// Gap between repaint checks while confirming.
const FALLBACK_CONFIRM_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// A PTY running a login shell or an agent CLI, with promotion (D-025).
pub struct ShellPtyDriver {
    options: ShellPtyOptions,
    inner: Mutex<Option<Arc<PtyState>>>,
    /// Set by the poller while a known agent CLI holds the foreground.
    promoted: Arc<std::sync::Mutex<Option<Detected>>>,
    /// Screen-derived readiness of that agent, when promoted.
    status: Arc<std::sync::Mutex<Option<ScreenStatus>>>,
    /// Deterministic transcript binding for the current promotion epoch.
    bindings: promotion::BindingHandle,
    /// Promotion poller, stopped on close.
    poller: Mutex<Option<JoinHandle<()>>>,
    /// Exit waiter (§5.5), stopped on close.
    waiter: Mutex<Option<JoinHandle<()>>>,
    /// Process table behind detection; a fixture in tests.
    table: Arc<dyn ProcessTable>,
    /// Live hook path, when the instance opted in. Dropped on close, which
    /// unbinds the socket.
    hooks: Mutex<Option<Arc<crate::launch::HookSession>>>,
    /// File-tail signal adapters (codex rollout, grok ACP files). Dropped on
    /// close, which stops their poll tasks (D-028 P6).
    adapters: Mutex<Option<crate::adapters::supervisor::AdapterHandle>>,
    /// Recipe from the last start, reused by [`Driver::close`] to journal what
    /// it stopped and by the Node to audit what was launched.
    recipe: std::sync::Mutex<Option<LaunchRecipe>>,
    /// Set once the exit waiter has concluded, so `close` on an already-dead
    /// process reports the real cause rather than "the ladder found nothing".
    exited: Arc<std::sync::Mutex<Option<ExitEvidence>>>,
    /// §9.1 switch coordination; recreated per run, adopted from the resumed
    /// driver on D-026 resume so an in-flight switch keeps its rendezvous.
    effort_bridge: Mutex<Arc<crate::effort::EffortBridge>>,
    effort_queue: Mutex<Arc<crate::effort::EffortQueue>>,
    effort_worker: Mutex<Option<JoinHandle<()>>>,
    /// §9.1 `/model` switch coordination, recreated per run like effort.
    model_bridge: Mutex<Arc<crate::model::ModelBridge>>,
    model_queue: Mutex<Arc<crate::model::ModelQueue>>,
    model_worker: Mutex<Option<JoinHandle<()>>>,
    /// In-session permission-mode switch coordination (shift+tab wheel).
    permission_bridge: Mutex<Arc<crate::permission::PermissionBridge>>,
    permission_queue: Mutex<Arc<crate::permission::PermissionQueue>>,
    permission_worker: Mutex<Option<JoinHandle<()>>>,
    /// Event sender for the current run, retained so an effort switch can
    /// journal `queued`/`degraded` without going through the worker.
    events_tx: Mutex<Option<mpsc::Sender<remuda_protocol::Observation>>>,
    /// Identity the current run's effort lifecycle observations are stamped
    /// with; `None` before `start`.
    promote_ctx: Mutex<Option<promotion::PromoteCtx>>,
    /// One screen-key fallback per interaction, for decisions the hook path
    /// proved it did not apply (§14 risk 1). Lives on the driver because the
    /// budget has to outlive the call that spends it.
    fallbacks: crate::hook_answer::FallbackLedger,
    /// Foreground Claude whose cancel key awaits native turn-end evidence.
    interrupt_pid: Arc<AtomicI32>,
    interrupt_screen_markers: Arc<std::sync::Mutex<promotion::InterruptBaseline>>,
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
            bindings: promotion::BindingHandle::empty(),
            poller: Mutex::new(None),
            waiter: Mutex::new(None),
            table,
            fallbacks: crate::hook_answer::FallbackLedger::new(),
            hooks: Mutex::new(None),
            adapters: Mutex::new(None),
            recipe: std::sync::Mutex::new(None),
            exited: Arc::new(std::sync::Mutex::new(None)),
            effort_bridge: Mutex::new(Arc::new(crate::effort::EffortBridge::new())),
            effort_queue: Mutex::new(Arc::new(crate::effort::EffortQueue::new())),
            effort_worker: Mutex::new(None),
            model_bridge: Mutex::new(Arc::new(crate::model::ModelBridge::new())),
            model_queue: Mutex::new(Arc::new(crate::model::ModelQueue::new())),
            model_worker: Mutex::new(None),
            permission_bridge: Mutex::new(Arc::new(crate::permission::PermissionBridge::new(
                false,
            ))),
            permission_queue: Mutex::new(Arc::new(crate::permission::PermissionQueue::new())),
            permission_worker: Mutex::new(None),
            events_tx: Mutex::new(None),
            promote_ctx: Mutex::new(None),
            interrupt_pid: Arc::new(AtomicI32::new(0)),
            interrupt_screen_markers: Arc::new(std::sync::Mutex::new(
                promotion::InterruptBaseline::default(),
            )),
            seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Hand a SessionStart hook report to the promotion supervisor.
    ///
    /// Channel A of deterministic transcript binding: the launch shim (D-028
    /// P1) relays `{hook_event_name, session_id, transcript_path, cwd, ppid}`;
    /// the report only binds an epoch when its `ppid` is the foreground pid.
    /// Safe to call when promotion is off or no agent is foreground — the
    /// report is matched at the next promotion tick.
    pub fn ingest_session_start(&self, report: crate::claude_transcript::SessionStartReport) {
        self.bindings.ingest_session_start(report);
    }

    /// Currently promoted agent kind, if the PTY's foreground is an agent CLI.
    #[must_use]
    pub fn promoted_kind(&self) -> Option<AgentKind> {
        self.promoted
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|found| found.kind))
    }

    /// Whether the hook path has produced a session binding for this PTY.
    ///
    /// Rung 1 of §5.2's ladder, and deliberately the *weaker* reading of it.
    /// The ladder's ideal is a `UserPromptSubmit` receipt proving the composer
    /// accepted the previous prompt; what the bus exposes today is the
    /// `SessionStart` binding, which proves the agent booted far enough to run
    /// hooks and to name its own session. That is strictly more than the screen
    /// knows and strictly less than a delivery receipt, so it is used to answer
    /// "has this TUI finished starting" and not "was the last prompt taken".
    /// The stronger form arrives with the hook adjudication path in P5.
    ///
    /// `false` whenever hooks are off, which drops the caller to rung 2.
    fn hook_receipt(&self) -> bool {
        let Some(pid) = self.session_pid() else {
            return false;
        };
        self.hooks
            .try_lock()
            .ok()
            .and_then(|slot| slot.as_ref().and_then(|session| session.binding()))
            .is_some_and(|binding| binding.pid == pid)
    }

    /// Screen-derived readiness of the promoted agent, when the poller has one.
    ///
    /// `None` means "no rule matched", which §10 insists never collapses into
    /// `idle` — so a caller guarding on idleness gets a conservative answer.
    #[must_use]
    pub fn screen_status(&self) -> Option<ScreenStatus> {
        self.status.lock().ok().and_then(|slot| *slot)
    }

    /// Answer an ignored hook decision on the agent's own dialog, once (§14 risk 1).
    ///
    /// Reached only when the hook path *proved* it did not take the decision.
    /// Everything here is deliberately conservative, because a stray keystroke
    /// in an agent TUI answers whatever question happens to be showing:
    ///
    /// - **One attempt ever**, claimed before the write. A lost ACK must not
    ///   become a replayed Enter (D-022).
    /// - **No guessing.** A screen that is truncated, ambiguous, or offers no
    ///   matching choice gets nothing pressed, and the caller is told so.
    /// - **No unconfirmed success.** After writing, the dialog has to be
    ///   observed to clear. If it does not, this reports `not-dispatched`
    ///   rather than a success the user would read as "approved".
    async fn fallback_to_screen(
        &self,
        id: &remuda_protocol::InteractionId,
        answer: &remuda_protocol::InteractionAnswer,
    ) -> DriverResult<DriverAck> {
        let Some(state) = self.inner.lock().await.clone() else {
            // No PTY at all: there is no dialog to answer, and nothing to
            // report but the truth.
            return Ok(DriverAck::not_dispatched());
        };
        let keys = match answer::keys_for(&state.screen_grid(), answer) {
            Ok(keys) => keys,
            Err(error) => {
                tracing::warn!(
                    interaction = %id.as_id().as_str(),
                    %error,
                    "the hook did not apply the decision and the screen cannot be answered safely"
                );
                return Ok(DriverAck::not_dispatched());
            }
        };
        // Claim before writing: when a write's outcome is unknown we must
        // assume it landed rather than send it twice (D-022).
        if !self.fallbacks.claim(id) {
            tracing::warn!(
                interaction = %id.as_id().as_str(),
                "the screen fallback for this interaction was already spent; not replaying"
            );
            return Ok(DriverAck::not_dispatched());
        }
        tracing::info!(
            interaction = %id.as_id().as_str(),
            ?keys,
            "hook decision was not applied; answering on screen once"
        );
        state.write_bytes(&logical_keys_to_bytes(&keys)).await?;
        if self
            .dialog_cleared_within(&state, FALLBACK_CONFIRM_WINDOW)
            .await
        {
            return Ok(DriverAck::transport_written());
        }
        // The keys went out and the prompt is still there. Saying "written"
        // here is what §14 risk 1 forbids: the user would read it as approved.
        tracing::warn!(
            interaction = %id.as_id().as_str(),
            "the approval dialog did not clear after the fallback; not reporting it as applied"
        );
        Ok(DriverAck::not_dispatched())
    }

    /// Poll until the approval dialog leaves the screen, or `budget` elapses.
    ///
    /// This is the "confirmed by screen change" half of the honesty rule: a
    /// keypress has no receipt, so the only evidence that it was taken is the
    /// prompt going away.
    async fn dialog_cleared_within(
        &self,
        state: &Arc<PtyState>,
        budget: std::time::Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            if answer::dialog_cleared(&state.screen_grid()) {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(FALLBACK_CONFIRM_POLL).await;
        }
    }

    /// Whether an on-screen approval left the screen after the hook accepted
    /// the decision.
    ///
    /// `false` when there is no PTY or no confirmation window, so a caller that
    /// cannot see the screen never *invents* a failure — but a live PTY that
    /// still shows the dialog after a hook `Answered` is the confined case, and
    /// that `false` is what arms the one fallback.
    async fn approval_cleared_on_screen(&self) -> bool {
        let Some(state) = self.inner.lock().await.clone() else {
            // No PTY: there is nothing to verify, and nothing to fall back to.
            return true;
        };
        self.dialog_cleared_within(&state, FALLBACK_CONFIRM_WINDOW)
            .await
    }

    /// The native session a hook reported, if `SessionStart` has fired.
    pub async fn hook_session(&self) -> Option<remuda_signal::SessionBinding> {
        self.hooks.lock().await.as_ref()?.binding()
    }

    /// The agent this session is actually running, however it got there.
    ///
    /// §1.0 rule 4: `launchedBy` records who typed the command and nothing
    /// else. A promoted hand-typed `claude` and a Remuda-launched one are the
    /// same session as far as capabilities go, so both answers come from the
    /// same place — the promotion state first (it is the live truth), the
    /// launch target as the answer before the first poll lands.
    fn session_kind(&self) -> Option<AgentKind> {
        self.promoted_kind()
            .or_else(|| self.options.target.agent_kind())
    }

    /// The live promoted process, or our direct agent child before detection.
    /// A login shell's PID is never evidence for a hand-typed agent.
    fn session_pid(&self) -> Option<i32> {
        if let Some(pid) = self
            .promoted
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|found| found.pid))
            .filter(|pid| *pid > 0)
        {
            return Some(pid);
        }
        self.options.target.agent_kind()?;
        self.inner
            .try_lock()
            .ok()?
            .as_ref()
            .map(|state| state.pgid.load(Ordering::SeqCst))
            .filter(|pid| *pid > 0)
    }

    /// Runtime capability overrides for `kind` (§4.3, §6).
    fn runtime_ref(&self, kind: AgentKind) -> remuda_protocol::NativeRef {
        let tier = if self.hook_receipt() {
            remuda_protocol::SignalTier::Hook
        } else if self.options.emulator {
            // The emulator retains OSC titles and `9;4` progress in VT state,
            // which is tier C. Without it there is only the stripped byte tail.
            remuda_protocol::SignalTier::Osc
        } else {
            remuda_protocol::SignalTier::Screen
        };
        let mut capabilities = Vec::new();
        if let Some(row) = keys::keys_for(kind) {
            let mut push = |name, provision: keys::Provision, reason: &str| {
                capabilities.push(remuda_protocol::RuntimeCapability {
                    name,
                    state: match provision {
                        // §6's honesty rule: `emulated` is still supported —
                        // it works, Remuda just implements it — while
                        // `unknown` must stay unknown rather than collapsing
                        // into a grey "no".
                        keys::Provision::Native | keys::Provision::Emulated => {
                            remuda_protocol::CapabilityState::Supported
                        }
                        keys::Provision::Unknown => remuda_protocol::CapabilityState::Unknown,
                    },
                    provision: provision.wire(),
                    tier,
                    reason_code: reason.to_owned(),
                });
            };
            push(
                remuda_protocol::CapabilityName::Steer,
                row.steer,
                if row.send_now_costs_turn {
                    // Grok: the only send-now transport cancels the running
                    // turn. The UI has to say so before the user presses it.
                    "send-now-cancels-turn"
                } else {
                    "measured"
                },
            );
            push(
                remuda_protocol::CapabilityName::Queue,
                row.queue,
                if row.queue_key.is_some() {
                    "harness-native-queue-key"
                } else {
                    "remuda-holds-the-queue"
                },
            );
            push(
                remuda_protocol::CapabilityName::Interrupt,
                row.interrupt.provision,
                "measured",
            );
        }
        remuda_protocol::NativeRef {
            host_id: HostId::new(),
            native_store_id: Id::new("obj").unwrap_or_else(|_| Id::new("obj").expect("id")),
            kind,
            session_id: remuda_protocol::Knowledge::Unknown {
                reason: "not-reported".into(),
                evidence_event_ids: Vec::new(),
            },
            transcript: remuda_protocol::Knowledge::Unknown {
                reason: "not-reported".into(),
                evidence_event_ids: Vec::new(),
            },
            signal_tier: Some(tier),
            capabilities,
            codex: None,
            acp: None,
            claude: None,
            claude_bg: None,
            agy: None,
            herdr: None,
        }
    }

    async fn state(&self) -> DriverResult<Arc<PtyState>> {
        self.inner
            .lock()
            .await
            .clone()
            .ok_or(DriverError::ControlUnavailable)
    }

    /// §9.1: queue `/effort <level>` for the composer, ready-ladder gated, and
    /// wait for transcript read-back while the composer is idle.
    async fn switch_effort(&self, level: &str) -> DriverResult<DriverAck> {
        let Some(request) = crate::effort::EffortRequest::from_level(level) else {
            return Err(DriverError::CapabilityUnsupported(format!(
                "claude /effort does not accept {level:?} in-session; \
                 valid: low, medium, high, xhigh, max, ultracode"
            )));
        };
        let state = self.state().await?;
        if state.closed.load(Ordering::SeqCst) {
            return Err(DriverError::ControlUnavailable);
        }
        let queue = self.effort_queue.lock().await.clone();
        let bridge = self.effort_bridge.lock().await.clone();
        let events = self
            .events_tx
            .lock()
            .await
            .clone()
            .ok_or(DriverError::ControlUnavailable)?;
        let ctx = self
            .promote_ctx
            .lock()
            .await
            .clone()
            .ok_or(DriverError::ControlUnavailable)?;
        let io: Arc<dyn crate::effort::EffortSwitchIo> = Arc::new(ShellEffortIo {
            state,
            events,
            seq: Arc::clone(&self.seq),
            ctx,
        });
        let ready = io.is_idle().await;
        if ready {
            let (done, rx_outcome) = tokio::sync::oneshot::channel();
            queue.enqueue(request, Some(done));
            let wait =
                std::time::Duration::from_millis(crate::effort::EFFORT_READBACK_TIMEOUT_MS + 5_000);
            if tokio::time::timeout(wait, rx_outcome).await.is_err() {
                // Bounded window elapsed without a terminal outcome; the worker
                // keeps running and journals applied/degraded. Never claim
                // applied here.
            }
        } else {
            // The agent is working (or the screen is not provably idle). Hold
            // for the next idle and let the worker journal the terminal state.
            io.journal(
                crate::effort::SwitchOutcome::Queued.journal_status(request.command_word(), ""),
                remuda_protocol::Severity::Info,
            )
            .await;
            queue.enqueue(request, None);
        }
        let _ = bridge;
        Ok(DriverAck::transport_written())
    }

    /// §9.1: queue `/model <id>` for the composer, ready-ladder gated, and wait
    /// for transcript read-back while the composer is idle.
    async fn switch_model(&self, model_id: &str) -> DriverResult<DriverAck> {
        let Some(request) = crate::model::ModelRequest::new(model_id) else {
            return Err(DriverError::CapabilityUnsupported(format!(
                "claude /model requires a non-empty id; got {model_id:?}"
            )));
        };
        let state = self.state().await?;
        if state.closed.load(Ordering::SeqCst) {
            return Err(DriverError::ControlUnavailable);
        }
        let queue = self.model_queue.lock().await.clone();
        let events = self
            .events_tx
            .lock()
            .await
            .clone()
            .ok_or(DriverError::ControlUnavailable)?;
        let ctx = self
            .promote_ctx
            .lock()
            .await
            .clone()
            .ok_or(DriverError::ControlUnavailable)?;
        let io: Arc<dyn crate::model::SwitchIo> = Arc::new(ShellEffortIo {
            state,
            events,
            seq: Arc::clone(&self.seq),
            ctx,
        });
        if io.is_idle().await {
            let (done, rx_outcome) = tokio::sync::oneshot::channel();
            queue.enqueue(request, Some(done));
            let wait =
                std::time::Duration::from_millis(crate::model::MODEL_READBACK_TIMEOUT_MS + 5_000);
            if tokio::time::timeout(wait, rx_outcome).await.is_err() {
                // Never claim applied without a terminal outcome.
            }
        } else {
            io.journal(
                crate::model::ModelSwitchOutcome::Queued.journal_status(&request.id, ""),
                remuda_protocol::Severity::Info,
            )
            .await;
            queue.enqueue(request, None);
        }
        Ok(DriverAck::transport_written())
    }

    /// Shift+tab the native permission wheel to `mode` and read it back from
    /// the status line / transcript.
    async fn switch_permission(&self, mode: &str) -> DriverResult<DriverAck> {
        let Some(request) = crate::permission::PermissionRequest::parse(mode) else {
            return Err(DriverError::CapabilityUnsupported(format!(
                "claude permission mode {mode:?} is not a valid mode"
            )));
        };
        let state = self.state().await?;
        if state.closed.load(Ordering::SeqCst) {
            return Err(DriverError::ControlUnavailable);
        }
        let bridge = self.permission_bridge.lock().await.clone();
        if !crate::permission::live_reachable(request.mode, bridge.bypass_allowed()) {
            return Err(DriverError::CapabilityUnsupported(format!(
                "claude permission mode {mode:?} is launch-only for this session"
            )));
        }
        let queue = self.permission_queue.lock().await.clone();
        let events = self
            .events_tx
            .lock()
            .await
            .clone()
            .ok_or(DriverError::ControlUnavailable)?;
        let ctx = self
            .promote_ctx
            .lock()
            .await
            .clone()
            .ok_or(DriverError::ControlUnavailable)?;
        let io: Arc<dyn crate::permission::PermissionSwitchIo> = Arc::new(ShellPermissionIo {
            state,
            events,
            seq: Arc::clone(&self.seq),
            ctx,
        });
        if io.is_idle().await {
            let (done, rx_outcome) = tokio::sync::oneshot::channel();
            queue.enqueue(request, Some(done));
            let wait = std::time::Duration::from_millis(
                crate::permission::PERMISSION_READBACK_TIMEOUT_MS + 5_000,
            );
            if tokio::time::timeout(wait, rx_outcome).await.is_err() {
                // The worker keeps running and journals applied/degraded; this
                // command settles as dispatched, never as applied.
            }
        } else {
            io.journal(
                crate::permission::SwitchOutcome::Queued.journal_status(request.word(), ""),
                remuda_protocol::Severity::Info,
            )
            .await;
            queue.enqueue(request, None);
        }
        Ok(DriverAck::transport_written())
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
        // §5.1: the recipe decides argv, and for an agent it is the real
        // materialized one — env allowlist, settings digest and provider all
        // filled in, not the stub the shell path used to emit for everything.
        // Materialization is blocking (`--version`, the ~207 MB binary hash,
        // overlay/shim/hook-socket writes); keep it off the async cores.
        let (recipe, hooks, provider_note) = {
            let options = self.options.clone();
            let cwd_owned = cwd.to_owned();
            let spec_owned = spec.cloned();
            let tx = tx.clone();
            let hook_ctx = hook_ctx.clone();
            let seq = Arc::clone(&self.seq);
            let interrupt_pid = Arc::clone(&self.interrupt_pid);
            let span = tracing::info_span!("materialize_launch");
            tokio::task::spawn_blocking(move || {
                let _guard = span.entered();
                prepare_launch_blocking(
                    &options,
                    &cwd_owned,
                    spec_owned.as_ref(),
                    &hook_ctx,
                    &tx,
                    &seq,
                    &interrupt_pid,
                )
            })
            .await
            .map_err(|error| {
                pty_err(io::Error::other(format!("materialize panicked: {error}")))
            })??
        };
        let cmd = build_command(&self.options, cwd, &recipe, hooks.as_ref())?;
        let child = pair.slave.spawn_command(cmd).map_err(pty_err)?;
        drop(pair.slave);
        // `portable-pty` calls `setsid()` before `exec`, so the child leads its
        // own process group and its pid is the pgid the stop ladder signals.
        let pgid = child.process_id().and_then(|pid| i32::try_from(pid).ok());
        let killer = child.clone_killer();
        let master = pair.master;
        let writer = master.take_writer().map_err(pty_err)?;
        let reader = master.try_clone_reader().map_err(pty_err)?;
        let (output, _) = broadcast::channel(64);
        let emulator = self.options.emulator.then(|| {
            tracing::info!(
                cols,
                rows,
                scrollback = remuda_screen::DEFAULT_SCROLLBACK_LINES,
                "terminal emulator on for this PTY ({EMULATOR_ENV})"
            );
            std::sync::Mutex::new(Emulator::new(cols, rows))
        });
        let state = Arc::new(PtyState {
            writer: Mutex::new(Some(writer)),
            master: Mutex::new(Some(master)),
            child: Mutex::new(child),
            killer: Mutex::new(killer),
            pgid: AtomicI32::new(pgid.unwrap_or(0)),
            output: output.clone(),
            ring: std::sync::Mutex::new(VecDeque::new()),
            interruption_output: std::sync::Mutex::new(promotion::InterruptionOutput::default()),
            emulator,
            cols: AtomicU16::new(cols),
            rows: AtomicU16::new(rows),
            closed: AtomicBool::new(false),
            reads: AtomicU64::new(0),
        });
        let pump = Arc::clone(&state);
        let (eof_tx, eof_rx) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("remuda-shell-pty".into())
            .spawn(move || {
                read_pty(pump, reader);
                // §5.5 second witness: EOF on the master means the slave is
                // closed at both ends. It frequently beats `wait()`.
                let _ = eof_tx.send(());
            })
            .map_err(DriverError::Io)?;
        *self.inner.lock().await = Some(Arc::clone(&state));
        *self.hooks.lock().await = hooks.clone();
        *self.events_tx.lock().await = Some(tx.clone());
        *self.promote_ctx.lock().await = Some(hook_ctx.clone());
        // §9.1: a fresh run starts with fresh switch coordination; D-026 resume
        // adopts the previous bridge afterwards when a switch was in flight.
        let effort_bridge = Arc::new(crate::effort::EffortBridge::new());
        if let Some(effort) = spec.and_then(|spec| spec.effort) {
            effort_bridge.note_launch_request(crate::effort::EffortRequest {
                name: effort.name,
                ultracode: effort.ultracode,
            });
        }
        let effort_io: Arc<dyn crate::effort::EffortSwitchIo> = Arc::new(ShellEffortIo {
            state: Arc::clone(&state),
            events: tx.clone(),
            seq: Arc::clone(&self.seq),
            ctx: hook_ctx.clone(),
        });
        // A fresh run needs a fresh queue: stop the previous worker and close
        // its queue before swapping.
        if let Some(worker) = self.effort_worker.lock().await.take() {
            worker.abort();
        }
        let effort_queue = {
            let mut slot = self.effort_queue.lock().await;
            slot.close();
            *slot = Arc::new(crate::effort::EffortQueue::new());
            Arc::clone(&slot)
        };
        *self.effort_worker.lock().await = Some(crate::effort::spawn_worker(
            Arc::clone(&effort_bridge),
            Arc::clone(&effort_queue),
            effort_io,
        ));
        *self.effort_bridge.lock().await = Arc::clone(&effort_bridge);
        // §9.1: /model switch coordination, same ready-ladder worker shape.
        let launch_model = spec.and_then(|spec| spec.model_id.clone());
        let model_bridge = Arc::new(crate::model::ModelBridge::new());
        if let Some(model) = launch_model.clone() {
            model_bridge.note_launch_request(model);
        }
        let host_config_dir =
            crate::claude_onboarding::HostClaudeConfig::from_env().and_then(|host| {
                host.user_settings
                    .parent()
                    .map(std::path::Path::to_path_buf)
            });
        let model_catalog = crate::model_discovery::resolve_catalog(
            Some(std::path::Path::new(&recipe.native_home)),
            host_config_dir.as_deref(),
            Some(&std::path::Path::new(&recipe.native_home).join("settings.json")),
            &[],
            launch_model.as_deref(),
        );
        let model_io: Arc<dyn crate::model::SwitchIo> = Arc::new(ShellEffortIo {
            state: Arc::clone(&state),
            events: tx.clone(),
            seq: Arc::clone(&self.seq),
            ctx: hook_ctx.clone(),
        });
        if let Some(worker) = self.model_worker.lock().await.take() {
            worker.abort();
        }
        let model_queue = {
            let mut slot = self.model_queue.lock().await;
            slot.close();
            *slot = Arc::new(crate::model::ModelQueue::new());
            Arc::clone(&slot)
        };
        *self.model_worker.lock().await = Some(crate::model::spawn_model_worker(
            Arc::clone(&model_bridge),
            Arc::clone(&model_queue),
            model_io,
        ));
        *self.model_bridge.lock().await = Arc::clone(&model_bridge);
        // Permission-mode coordination for this run. The wheel includes
        // bypassPermissions only for a bypass launch (argv flag) or a session
        // that started already in bypass.
        let launch_permission = spec.and_then(crate::claude_pty::launch_claude_permission);
        let bypass_allowed = recipe.argv.iter().any(|token| {
            token == "--dangerously-skip-permissions"
                || token == "--allow-dangerously-skip-permissions"
        }) || launch_permission
            == Some(remuda_protocol::ClaudePermissionMode::BypassPermissions);
        if let Some(worker) = self.permission_worker.lock().await.take() {
            worker.abort();
        }
        let permission_queue = {
            let mut slot = self.permission_queue.lock().await;
            slot.close();
            *slot = Arc::new(crate::permission::PermissionQueue::new());
            Arc::clone(&slot)
        };
        let permission_bridge = Arc::new(crate::permission::PermissionBridge::new(bypass_allowed));
        if let Some(mode) = launch_permission {
            permission_bridge.note_launch_mode(mode);
        }
        let permission_io: Arc<dyn crate::permission::PermissionSwitchIo> =
            Arc::new(ShellPermissionIo {
                state: Arc::clone(&state),
                events: tx.clone(),
                seq: Arc::clone(&self.seq),
                ctx: hook_ctx.clone(),
            });
        *self.permission_worker.lock().await = Some(crate::permission::spawn_worker(
            Arc::clone(&permission_bridge),
            Arc::clone(&permission_queue),
            permission_io,
        ));
        *self.permission_bridge.lock().await = Arc::clone(&permission_bridge);
        // D-028 P6: file-tail signal adapters for the codex/grok structured
        // channels. They read the shadow home the hook session materialized
        // (which is the same path the child receives via CODEX_HOME /
        // GROK_HOME), follow the child pid, and emit on the instance's one
        // ordered observation channel. Hook-confirmed session identity wins
        // over file discovery (Hook > File) via the adapter confirm path.
        if let Some(handle) = self.spawn_adapters(&hooks, &recipe, &hook_ctx, &tx, pgid, cwd)? {
            *self.adapters.lock().await = Some(handle);
        }
        if self.options.promote {
            *self.poller.lock().await = Some(promotion::spawn(
                Arc::clone(&state),
                hook_ctx.clone(),
                Arc::clone(&self.table),
                Arc::clone(&self.promoted),
                Arc::clone(&self.status),
                self.bindings.clone(),
                hooks,
                matches!(self.options.target, Target::Shell),
                self.options.auto_trust_workspace,
                Arc::clone(&self.interrupt_pid),
                Arc::clone(&self.interrupt_screen_markers),
                tx.clone(),
                Arc::clone(&self.seq),
                Some(Arc::clone(&effort_bridge)),
                spec.and_then(|spec| spec.effort),
                Some(promotion::ModelSync {
                    bridge: Arc::clone(&model_bridge),
                    launch: launch_model,
                    catalog: Some(model_catalog),
                }),
                Some(Arc::clone(&permission_bridge)),
                launch_permission,
                // c-wfdrill2 B: the pinned path this launch exec'd, so
                // detection does not depend on the executable's basename
                // being one the agent table has heard of. Only for an agent
                // target — a shell's recipe binary is the login shell, and
                // aliasing that would promote the shell itself.
                self.options.target.agent_kind().map(|kind| {
                    crate::promote::LaunchAlias::new(kind, recipe.binary.abs_path.clone())
                }),
                // The relay this launch exec'd and its per-instance hook socket,
                // so the poller can name why a quiet hook tier went silent.
                self.options
                    .hooks
                    .as_ref()
                    .map(|hooks| promotion::HookSilencePaths {
                        relay: hooks.relay_binary.clone(),
                        socket: hooks.instance_dir.join("hook.sock"),
                    }),
            ));
            // A login shell has no agent at spawn; once promotion identifies a
            // hand-typed codex/grok, start its file adapter against the native
            // (not a shadow) home. §1.0 rule 2: the promoted path gets the
            // same structured lifecycle channel as a launched one.
            if self.options.target.agent_kind().is_none() {
                *self.adapters.lock().await =
                    Some(self.spawn_promoted_adapter_watch(&hook_ctx, tx.clone()));
            }
        }
        // gateway-carryover-1: state which provider source actually applied,
        // before anything the session does can be misread as evidence of it.
        // The channel is already live and `rx` is handed to the caller below,
        // so this lands ahead of the first hook or screen observation.
        if let Some(payload) = provider_note
            && let Err(error) = promotion::emit_payload(
                &tx,
                &self.seq,
                &hook_ctx,
                remuda_protocol::SourceChannel::Runtime,
                remuda_protocol::Completeness::Structured,
                payload,
            )
            .await
        {
            tracing::debug!(%error, "provider source lifecycle not journaled");
        }
        // §5.5: a crashed agent used to stay `ready` forever, because EOF only
        // broke the read loop. Both witnesses now journal an exit.
        *self.waiter.lock().await = Some(spawn_exit_waiter(
            Arc::clone(&state),
            hook_ctx,
            tx,
            Arc::clone(&self.seq),
            Arc::clone(&self.exited),
            Arc::clone(&self.promoted),
            eof_rx,
        ));
        if let Ok(mut slot) = self.recipe.lock() {
            *slot = Some(recipe.clone());
        }
        Ok(RunHandle::new(recipe, DriverAck::transport_written(), rx))
    }

    /// Start the per-kind file-tail adapter for a Remuda-launched agent.
    ///
    /// Returns `None` for shells, promoted hand-typed sessions (their adapter
    /// starts when promotion identifies the kind), and kinds without a file
    /// channel. The adapter reads from the shadow home the hook session
    /// wrote, so it only exists when the hook path is live — without hooks
    /// there is no per-session shadow home, and reading the user's real
    /// `~/.codex` from a launched session would cross the §4.2 boundary.
    fn spawn_adapters(
        &self,
        hooks: &Option<Arc<crate::launch::HookSession>>,
        recipe: &LaunchRecipe,
        ctx: &promotion::PromoteCtx,
        events: &mpsc::Sender<remuda_protocol::Observation>,
        pid: Option<i32>,
        cwd: &str,
    ) -> DriverResult<Option<crate::adapters::supervisor::AdapterHandle>> {
        let Some(hooks) = hooks else {
            return Ok(None);
        };
        let Some(kind) = self.options.target.agent_kind() else {
            return Ok(None);
        };
        let Some(shadow) = &hooks.shadow else {
            return Ok(None);
        };
        let home = crate::adapters::AdapterHome {
            home: shadow.home.clone(),
            cwd: PathBuf::from(cwd),
            pid: pid.filter(|pid| *pid > 0).map(|pid| pid as u32),
        };
        let stamp = crate::adapters::supervisor::stamp_ctx(
            ctx.instance_id.clone(),
            ctx.host_id.clone(),
            ctx.journal_id.clone(),
            ctx.run_id.clone(),
            recipe
                .session_id
                .clone()
                .unwrap_or_else(|| format!("{kind:?}-pending")),
        );
        let adapter_ctx = crate::adapters::supervisor::AdapterCtx {
            stamp,
            seq: Arc::clone(&self.seq),
            events: events.clone(),
            home,
            fallback_model: Some(recipe.provider.model_requested.clone())
                .filter(|model| !model.is_empty()),
            hooks: Some(Arc::clone(hooks)),
            agent_pid: pid,
        };
        crate::adapters::supervisor::spawn_file_adapters(kind, adapter_ctx)
    }

    /// Watch the promoted agent; when it resolves to a codex/grok kind with a
    /// discoverable native home, spawn that kind's file adapter once.
    ///
    /// A promoted session reads the user's *real* `~/.codex`/`~/.grok` (the
    /// human started the binary themselves, outside the shadow), so the
    /// adapter home comes from the Node environment rather than from a
    /// materialized shadow. The poll cadence matches the promotion tick.
    fn spawn_promoted_adapter_watch(
        &self,
        ctx: &promotion::PromoteCtx,
        events: mpsc::Sender<remuda_protocol::Observation>,
    ) -> crate::adapters::supervisor::AdapterHandle {
        let promoted = Arc::clone(&self.promoted);
        let options_cwd = self.options.cwd.clone();
        let seq = Arc::clone(&self.seq);
        let stamp_ctx = crate::adapters::supervisor::stamp_ctx(
            ctx.instance_id.clone(),
            ctx.host_id.clone(),
            ctx.journal_id.clone(),
            ctx.run_id.clone(),
            "promoted-pending",
        );
        let task = tokio::spawn(async move {
            let mut current: Option<remuda_protocol::AgentKind> = None;
            let mut handle: Option<crate::adapters::supervisor::AdapterHandle> = None;
            let mut tick = tokio::time::interval(PROMOTE_POLL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if events.is_closed() {
                    break;
                }
                let kind = promoted
                    .lock()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(|found| found.kind));
                if kind == current {
                    continue;
                }
                // Kind changed (terminal → agent, or one agent → another):
                // drop the previous adapter before starting the new one.
                if let Some(old) = handle.take() {
                    old.abort();
                }
                current = kind;
                if let Some(kind @ (AgentKind::Codex | AgentKind::Grok)) = kind
                    && let Some(found) = promoted.lock().ok().and_then(|slot| slot.clone())
                    && let Some(home) = crate::adapters::supervisor::promoted_home(
                        kind,
                        options_cwd.clone(),
                        u32::try_from(found.pid).ok(),
                    )
                {
                    let adapter_ctx = crate::adapters::supervisor::AdapterCtx {
                        stamp: stamp_ctx.clone(),
                        seq: Arc::clone(&seq),
                        events: events.clone(),
                        home,
                        fallback_model: None,
                        hooks: None,
                        agent_pid: Some(found.pid),
                    };
                    match crate::adapters::supervisor::spawn_file_adapters(kind, adapter_ctx) {
                        Ok(Some(started)) => handle = Some(started),
                        Ok(None) => {}
                        Err(error) => {
                            tracing::debug!(%error, ?kind, "promoted file adapter failed to start")
                        }
                    }
                }
            }
        });
        crate::adapters::supervisor::AdapterHandle { tasks: vec![task] }
    }

    /// Run the stop ladder against this PTY's process group (§5.3).
    ///
    /// Falls back to the cloned killer only where there is no process group to
    /// signal — a platform without them, or a child that reported no pid. That
    /// fallback is the pre-D-028 behaviour and inherits its weakness (it
    /// reaches the direct child only), which is why it is last and logged.
    async fn stop_tree(&self, state: &Arc<PtyState>) -> DriverResult<StopOutcome> {
        let pgid = state.pgid.load(Ordering::SeqCst);
        if pgid <= 0 {
            tracing::warn!(
                "no process group for this PTY; falling back to killing the direct child, \
                 which cannot reach anything it started"
            );
            state.killer.lock().await.kill().map_err(DriverError::Io)?;
            return Ok(StopOutcome::already_gone());
        }
        // §5.3 step 2: the hangup only lands if the master is really closed.
        // An fd that is still open holds the slave's other end open, so the
        // shell inside never notices the SIGHUP and the ladder escalates to
        // SIGKILL for a process that would have hung up politely.
        //
        // *Every* dup counts. `portable-pty` hands out three fds on the master
        // — the box, the `take_writer` dup and the reader thread's
        // `try_clone_reader` dup — and the slave hangs up only when the last
        // one goes. Closing the box alone is why a SIGKILLed macOS leader sat
        // in `E` (exiting) indefinitely: unreapable, because the kernel will
        // not finish an exit while the terminal still has a live endpoint.
        //
        // Both owned halves are taken out here, before the ladder runs, and
        // moved into the closure, so the close is a plain `drop` of owned
        // values at exactly the rung that needs it, with no lock acquired from
        // inside a synchronous callback on a runtime thread. The reader's dup
        // closes itself: this drop is what makes its blocking read see EOF.
        let mut master = state.master.lock().await.take();
        let mut writer = state.writer.lock().await.take();
        state.closed.store(true, Ordering::SeqCst);
        // Hold the child across the ladder so this is the only task reaping it.
        // The leader is our direct child; a SIGKILLed child sits as a zombie
        // until `wait`, and a zombie still answers killpg — that produced the
        // false `stop-incomplete` against a process already in state E/Z.
        let mut child = state.child.lock().await;
        let mut reaped = false;
        let reaper = &mut || {
            if reaped {
                return true;
            }
            match child.try_wait() {
                Ok(Some(_status)) => {
                    reaped = true;
                    true
                }
                Ok(None) => false,
                Err(error) => {
                    tracing::debug!(%error, "pty child reap failed");
                    false
                }
            }
        };
        let outcome = lifecycle::stop_group(
            pgid,
            move || {
                drop(writer.take());
                drop(master.take());
            },
            reaper,
        )
        .await?;
        // The leader can be classified dead while the kernel has not made it
        // reapable yet: on macOS a SIGKILLed session leader sits in `E`
        // (exiting) before it becomes a zombie. `stop_group` correctly reports
        // the group gone, but leaving it unreaped is how the demo's `?Es`
        // process lingered as a child of the Node. With every master fd now
        // released the transition to a zombie takes milliseconds; the bound is
        // kept because a process wedged in uninterruptible exit is a kernel
        // problem no userspace wait can hurry.
        if !reaped {
            let deadline = tokio::time::Instant::now() + REAP_EXITING_GRACE;
            loop {
                match child.try_wait() {
                    Ok(Some(_status)) => break,
                    Ok(None) if tokio::time::Instant::now() < deadline => {
                        tokio::time::sleep(lifecycle::EXIT_REAP_POLL).await;
                    }
                    Ok(None) => {
                        // Not "left for init": init only adopts orphans once
                        // *this* process exits, and the Node keeps running, so
                        // an abandoned leader stays our unreaped child for the
                        // life of the Node. Reaching here means something still
                        // holds the PTY open (see `release_pty`) or the kernel
                        // is genuinely wedged — either way it is a defect to
                        // report, not a tidy handover.
                        tracing::error!(
                            pgid,
                            "reap-incomplete: the process group is dead but the leader has not \
                             become reapable within {:?}; it stays a child of this Node",
                            REAP_EXITING_GRACE
                        );
                        break;
                    }
                    Err(error) => {
                        tracing::debug!(%error, "final pty child reap failed");
                        break;
                    }
                }
            }
        }
        Ok(outcome)
    }

    /// Shared body of [`Driver::resume`] and [`Driver::start_resumed`].
    ///
    /// `spec` is `Some` for the cross-instance case (D-026), where the new
    /// instance brings its own spec, and `None` when this driver is resuming
    /// the session it was already configured for.
    async fn start_resumed_inner(
        &self,
        spec: Option<InstanceSpec>,
        session_id: String,
    ) -> DriverResult<RunHandle> {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Err(DriverError::NativeSessionNotFound);
        }
        let Target::Agent { kind, .. } = self.options.target else {
            return Err(DriverError::CapabilityUnsupported(
                "a login shell has no native session to resume".into(),
            ));
        };
        // A resumed driver is a fresh launch with the resume flag set. Cloning
        // options rather than mutating `self` keeps `resume` callable on a
        // driver whose original target is still meaningful for diagnostics.
        let mut resumed = Self::with_process_table(
            ShellPtyOptions {
                target: Target::Agent {
                    kind,
                    resume: Some(session_id.to_owned()),
                },
                ..self.options.clone()
            },
            Arc::clone(&self.table),
        );
        // The adopted poller/bus must update the same control handles that
        // callers retain on `self`, including native cancel confirmation.
        resumed.promoted = Arc::clone(&self.promoted);
        resumed.status = Arc::clone(&self.status);
        resumed.bindings = self.bindings.clone();
        resumed.interrupt_pid = Arc::clone(&self.interrupt_pid);
        resumed.interrupt_screen_markers = Arc::clone(&self.interrupt_screen_markers);
        resumed.exited = Arc::clone(&self.exited);
        resumed.seq = Arc::clone(&self.seq);
        let handle = match spec {
            Some(spec) => resumed.spawn_at(&spec.cwd.clone(), Some(&spec)).await?,
            None => resumed.spawn().await?,
        };
        // Adopt the new PTY: the caller holds *this* driver, so control
        // operations have to reach the process that was just started.
        *self.inner.lock().await = resumed.inner.lock().await.take();
        *self.hooks.lock().await = resumed.hooks.lock().await.take();
        *self.poller.lock().await = resumed.poller.lock().await.take();
        *self.waiter.lock().await = resumed.waiter.lock().await.take();
        // §9.1: adopt the resumed run's switch coordination. Its worker and
        // mapper share a bridge, and its event sender is the live one. When a
        // switch was mid-flight at resume, adopt the resumed bridge so the
        // read-back rendezvous survives; otherwise start the run with ours.
        self.effort_queue.lock().await.close();
        if let Some(worker) = self.effort_worker.lock().await.take() {
            worker.abort();
        }
        *self.effort_queue.lock().await = {
            let mut slot = resumed.effort_queue.lock().await;
            std::mem::replace(&mut *slot, Arc::new(crate::effort::EffortQueue::new()))
        };
        *self.effort_worker.lock().await = resumed.effort_worker.lock().await.take();
        *self.events_tx.lock().await = resumed.events_tx.lock().await.take();
        let adopt_bridge = resumed.effort_bridge.lock().await.has_pending();
        if adopt_bridge {
            let resumed_bridge = {
                let mut slot = resumed.effort_bridge.lock().await;
                std::mem::replace(&mut *slot, Arc::new(crate::effort::EffortBridge::new()))
            };
            *self.effort_bridge.lock().await = resumed_bridge;
        }
        // §9.1: adopt the resumed run's model worker/queue (its bridge starts
        // fresh; an in-flight model read-back need not survive a resume).
        self.model_queue.lock().await.close();
        if let Some(worker) = self.model_worker.lock().await.take() {
            worker.abort();
        }
        *self.model_queue.lock().await = {
            let mut slot = resumed.model_queue.lock().await;
            std::mem::replace(&mut *slot, Arc::new(crate::model::ModelQueue::new()))
        };
        *self.model_worker.lock().await = resumed.model_worker.lock().await.take();
        // Permission switch coordination: adopt queue/worker always and the
        // bridge when a wheel walk was mid-flight.
        self.permission_queue.lock().await.close();
        if let Some(worker) = self.permission_worker.lock().await.take() {
            worker.abort();
        }
        *self.permission_queue.lock().await = {
            let mut slot = resumed.permission_queue.lock().await;
            std::mem::replace(
                &mut *slot,
                Arc::new(crate::permission::PermissionQueue::new()),
            )
        };
        *self.permission_worker.lock().await = resumed.permission_worker.lock().await.take();
        if resumed.permission_bridge.lock().await.pending().is_some() {
            let resumed_bridge = {
                let mut slot = resumed.permission_bridge.lock().await;
                std::mem::replace(
                    &mut *slot,
                    Arc::new(crate::permission::PermissionBridge::new(false)),
                )
            };
            *self.permission_bridge.lock().await = resumed_bridge;
        }
        if let (Ok(mut ours), Ok(theirs)) = (self.recipe.lock(), resumed.recipe.lock()) {
            ours.clone_from(&theirs);
        }
        Ok(handle)
    }
}

/// Identity the promotion poller and the signal bus both stamp Observations
/// with.
///
/// One context for both so hook and screen evidence share a journal, a run and
/// a sequence counter. Node rebinds instance/journal/host on commit; the driver
/// only needs locally consistent ids (same contract as claude-pty).
/// Free-function form of [`ShellPtyDriver::prepare_launch`], so the whole
/// blocking materialization (recipe pin/hash, hook overlay, shims, socket
/// bind) can run inside `spawn_blocking` without borrowing a driver.
fn prepare_launch_blocking(
    options: &ShellPtyOptions,
    cwd: &str,
    spec: Option<&InstanceSpec>,
    ctx: &promotion::PromoteCtx,
    events: &mpsc::Sender<remuda_protocol::Observation>,
    seq: &Arc<AtomicU64>,
    interrupt_pid: &Arc<AtomicI32>,
) -> DriverResult<(
    LaunchRecipe,
    Option<Arc<crate::launch::HookSession>>,
    Option<remuda_protocol::ObservationPayload>,
)> {
    let recipe = recipe_for_blocking(options, cwd, spec, None)?;
    let native_claude = options.target.agent_kind() == Some(AgentKind::Claude);
    // Precedence for the merged overlay (native-config-1, 2026-09-16;
    // gateway-carryover-1, 2026-09-17): the launching user's effective settings
    // are the BASE (only when the carrier pinned a scoped native home), the
    // operator/request overlay goes on top, then Remuda's hooks and terminal
    // pins applied by the materializer below.
    //
    // For `gateway`/`direct` the operator overlay is *authoritative*, not just
    // higher: see `merge_provider_overlay_over_user`. Being higher was not
    // enough, because the host describes the provider through env variables
    // (`ANTHROPIC_BASE_URL`, `ANTHROPIC_MODEL`, `ANTHROPIC_DEFAULT_*_MODEL`,
    // `CLAUDE_CODE_SUBAGENT_MODEL`, …) that Claude honours *over* the overlay's
    // `model` key, so a plain merge left the session on the host's gateway.
    let user_settings = if native_claude {
        match &options.user_settings_home {
            Some(home) => crate::launch::load_effective_user_settings(home)?,
            None => None,
        }
    } else {
        None
    };
    // The operator-supplied overlay (if any) is read from the audited
    // materialized file so the bytes here are exactly the ones the recipe
    // recorded.
    let operator_settings = if native_claude {
        recipe
            .materialized_files
            .iter()
            .find(|file| file.role == crate::recipe::FileRole::Settings)
            .map(|file| Ok::<_, DriverError>(serde_json::from_slice(&std::fs::read(&file.path)?)?))
            .transpose()?
    } else {
        None
    };
    // Delegation decides who owns the provider. `none` is 跟随主机: the host's
    // own settings *are* the requested provider and carry over verbatim.
    let provider_authoritative = native_claude
        && matches!(
            options.agent.as_ref().map(|agent| agent.profile.delegation),
            Some(Delegation::Gateway | Delegation::Direct)
        );
    let mut base_settings = match (user_settings, operator_settings) {
        (Some(user), Some(operator)) if provider_authoritative => Some(
            crate::launch::merge_provider_overlay_over_user(&user, &operator),
        ),
        (Some(user), Some(operator)) => {
            Some(crate::launch::merge_settings_layers(&user, &operator))
        }
        // Delegation asked for a gateway/direct provider but no overlay reached
        // us. The host's settings must still not answer for it: strip its
        // endpoint/model variables anyway (an empty overlay merges to nothing)
        // so the session falls back to the harness's own login rather than
        // silently taking the host's gateway. Otherwise this failure mode is
        // indistinguishable from the bug being fixed.
        (Some(user), None) if provider_authoritative => Some(
            crate::launch::merge_provider_overlay_over_user(&user, &serde_json::json!({})),
        ),
        (user, operator) => user.or(operator),
    };
    // D-036 / model-pin-1, belt and braces: an explicit pin is authoritative on
    // its own, whatever the delegation. `none` still means the host owns the
    // endpoint and the credential — it must stop meaning the host owns the
    // model, which is how the 2026-09-18 demo ran every pinned worker on the
    // host's default. Applied after the merge above so it lands on whichever
    // base won, and only when a pin actually exists (rule 3: no pin, no
    // change). This is the second channel; the materializer's `--model` argv is
    // the first, and they carry the same id.
    let model_pin = spec.and_then(crate::materializer::pinned_model_for);
    if native_claude && let Some(pin) = model_pin.as_deref() {
        crate::launch::apply_model_pin(
            base_settings.get_or_insert_with(|| serde_json::json!({})),
            pin,
        );
    }
    let provider_note = provider_source_lifecycle(
        options,
        &recipe,
        provider_authoritative,
        base_settings.as_ref(),
    );
    if let Some(settings) = &base_settings {
        // Values are masked before they reach the log; the credential bytes
        // live only in the 0600 instance file.
        tracing::debug!(
            settings = ?crate::launch::redact_settings(settings),
            "user settings merged into the launch overlay"
        );
    }
    // A native launch that explicitly requested bypass permissions must not sit
    // on Claude's "WARNING: … Yes, I accept" disclaimer — that screen parks
    // before SessionStart exactly like the trust dialog does.
    //
    // The acceptance is written into the private **settings** overlay
    // (`skipDangerousModePermissionPrompt`), not the global config key: on a
    // migrated 2.1.x config (`migrationVersion` ≥ 14, verified on the installed
    // 2.1.221) the global `bypassPermissionsModeAccepted` is ignored at the
    // TUI gate while the settings key is honoured. This is also the key Claude
    // itself writes into `settings.json` when a human accepts the disclaimer,
    // and the herdr carrier's session overlay carries the same decision. The
    // operator's real settings are never touched.
    let bypass_requested = spec.is_some_and(spec_bypass_permissions);
    if native_claude && bypass_requested {
        let object = base_settings.get_or_insert_with(|| serde_json::json!({}));
        if let Some(object) = object.as_object_mut() {
            object.insert(
                "skipDangerousModePermissionPrompt".into(),
                serde_json::json!(true),
            );
        } else {
            return Err(DriverError::SettingsIsolationUnavailable(
                "settings overlay is not a JSON object".into(),
            ));
        }
    }
    let hooks = start_hooks_blocking(
        options,
        ctx,
        events,
        base_settings.clone(),
        seq,
        interrupt_pid,
    )?;
    if let Some(hooks) = &hooks {
        // The macOS retest failure showed only the emulator log; this line is
        // the proof that the hook path actually stood up (D-028 §4.2).
        tracing::info!(
            path = %hooks.overlay.path.display(),
            socket = %hooks.socket_path.display(),
            bin = %hooks.bin_dir().display(),
            "hooks overlay materialised"
        );
    }
    // Which overlay the recipe argv must carry: the hook session's merged file
    // when hooks are live, otherwise the merged user/bypass settings file we
    // write ourselves. Neither exists when hooks are off and no settings
    // needed seeding — then the first recipe already had the right argv (or no
    // --settings at all).
    let settings_overlay = match hooks.as_ref() {
        Some(hooks) => Some(hooks.overlay.path.clone()),
        None if native_claude && base_settings.is_some() => Some(write_settings_overlay(
            options,
            base_settings
                .as_ref()
                .expect("base settings presence was just matched"),
        )?),
        None => None,
    };
    let recipe = match settings_overlay {
        Some(path) => recipe_for_blocking(options, cwd, spec, Some(&path))?,
        None => recipe,
    };
    Ok((recipe, hooks, provider_note))
}

/// Native lifecycle name for the provider decision a launch actually made
/// (gateway-carryover-1).
pub const NATIVE_PROVIDER_SOURCE: &str = "provider_source_resolved";

/// Build the lifecycle payload that records **which provider source applied**.
///
/// The regression this exists for was invisible in the journal: the Hub record
/// said `gateway` + profile id, and the session ran on the host's own gateway,
/// with nothing in between to contradict either. One line stating the source
/// and the effective model makes the transcript able to prove what ran.
///
/// `status` is `overlay` or `host-native`. `related_ids` carries the profile id
/// and delegation, and `model` is the model that will actually answer: the
/// overlay's own `model` when the overlay applies, else the host's. Only names
/// and ids — never a token, never a base URL, since a journal is not the 0600
/// instance file.
fn provider_source_lifecycle(
    options: &ShellPtyOptions,
    recipe: &LaunchRecipe,
    provider_authoritative: bool,
    settings: Option<&serde_json::Value>,
) -> Option<remuda_protocol::ObservationPayload> {
    if options.target.agent_kind() != Some(AgentKind::Claude) {
        return None;
    }
    let mut related = std::collections::BTreeMap::new();
    related.insert(
        "delegation".to_owned(),
        match recipe.provider.delegation {
            Delegation::Gateway => "gateway",
            Delegation::Direct => "direct",
            Delegation::None => "none",
        }
        .to_owned(),
    );
    related.insert(
        "providerProfileId".to_owned(),
        recipe.provider.profile_id.to_string(),
    );
    // The model that will answer. The overlay's `model` is the requested one
    // when the overlay is authoritative; otherwise whatever the host layer
    // left in place is the honest answer, and "unset" is honest too.
    let model = settings
        .and_then(|settings| settings.get("model"))
        .and_then(serde_json::Value::as_str)
        .filter(|model| !model.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| {
            Some(recipe.provider.model_requested.clone()).filter(|model| !model.trim().is_empty())
        });
    related.insert(
        "effectiveModel".to_owned(),
        model.unwrap_or_else(|| "unset".to_owned()),
    );
    // Whether the host had provider config at all, so a `host-native` line can
    // be told apart from "nobody configured anything".
    related.insert(
        "hostSettingsSeeded".to_owned(),
        options.user_settings_home.is_some().to_string(),
    );
    Some(remuda_protocol::ObservationPayload::Lifecycle(Box::new(
        remuda_protocol::LifecyclePayload::Native(Box::new(remuda_protocol::NativeLifecycle {
            topic: remuda_protocol::LifecycleTopic::Configuration,
            native_name: NATIVE_PROVIDER_SOURCE.to_owned(),
            native_id: remuda_protocol::Knowledge::NotApplicable,
            status: remuda_protocol::Knowledge::Known {
                value: if provider_authoritative {
                    "overlay".to_owned()
                } else {
                    "host-native".to_owned()
                },
            },
            related_ids: related,
            data_ref: None,
            severity: remuda_protocol::Severity::Info,
            affects_completion: false,
        })),
    )))
}

/// Write the standalone settings overlay (hooks-off path) when no hook
/// session owns one. This is the user settings seeded into a scoped native
/// home, possibly with the bypass-acceptance key added.
fn write_settings_overlay(
    options: &ShellPtyOptions,
    settings: &serde_json::Value,
) -> DriverResult<PathBuf> {
    let launch = options
        .agent
        .as_ref()
        .map(|agent| agent.launch_dir.clone())
        .ok_or_else(|| {
            DriverError::InvalidLaunchSpec("bypass overlay needs an agent launch dir".into())
        })?;
    std::fs::create_dir_all(&launch)?;
    let path = launch.join("settings.json");
    let bytes = serde_json::to_vec_pretty(settings)?;
    std::fs::write(&path, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

/// Whether the launch spec explicitly asked for bypass permissions.
fn spec_bypass_permissions(spec: &InstanceSpec) -> bool {
    matches!(
        spec.permission_mode,
        remuda_protocol::PermissionMode::Claude(ref claude)
            if claude.mode == remuda_protocol::ClaudePermissionMode::BypassPermissions
    )
}

/// Free-function form of [`ShellPtyDriver::recipe_for`] usable off the async
/// runtime inside `spawn_blocking`.
fn recipe_for_blocking(
    options: &ShellPtyOptions,
    cwd: &str,
    spec: Option<&InstanceSpec>,
    settings_overlay: Option<&Path>,
) -> DriverResult<LaunchRecipe> {
    let Target::Agent { kind, resume } = &options.target else {
        return shell_recipe(options, cwd);
    };
    let Some(agent) = options.agent.as_ref() else {
        return Err(DriverError::InvalidLaunchSpec(
            "an agent target needs its launch inputs".into(),
        ));
    };
    let Some(spec) = spec else {
        return Err(DriverError::InvalidLaunchSpec(
            "an agent target needs an InstanceSpec to materialize".into(),
        ));
    };
    if spec.kind != *kind {
        return Err(DriverError::InvalidLaunchSpec(format!(
            "spec kind {:?} does not match the launch target {kind:?}",
            spec.kind
        )));
    }
    let mut agent = agent.as_ref().clone();
    if let Some(path) = settings_overlay {
        agent.settings_overlay = Some(path.to_path_buf());
    }
    launch::agent_recipe(spec, &agent, cwd, resume.as_deref())
}

/// Blocking half of hook session startup, callable off the async runtime.
#[allow(clippy::too_many_arguments)]
fn start_hooks_blocking(
    options: &ShellPtyOptions,
    ctx: &promotion::PromoteCtx,
    events: &mpsc::Sender<remuda_protocol::Observation>,
    base_settings: Option<serde_json::Value>,
    seq: &Arc<AtomicU64>,
    interrupt_pid: &Arc<AtomicI32>,
) -> DriverResult<Option<Arc<crate::launch::HookSession>>> {
    let Some(config) = options.hooks.clone() else {
        return Ok(None);
    };
    let bus = Arc::new(
        remuda_signal::SignalBus::new(bus_context(ctx), events.clone(), Arc::clone(seq))
            .with_interrupt_tracker(Arc::clone(interrupt_pid)),
    );
    Ok(Some(Arc::new(crate::launch::HookSession::start(
        &crate::launch::HookSessionOptions {
            instance_dir: config.instance_dir,
            relay_binary: config.relay_binary,
            tui: config.tui,
            base_settings,
            kind: options.target.agent_kind().unwrap_or(AgentKind::Claude),
        },
        bus,
    )?)))
}

/// The hook bus identity for a promotion context.
fn bus_context(ctx: &promotion::PromoteCtx) -> remuda_signal::BusContext {
    remuda_signal::BusContext {
        instance_id: ctx.instance_id.clone(),
        host_id: ctx.host_id.clone(),
        journal_id: ctx.journal_id.clone(),
        run_id: ctx.run_id.clone(),
        driver_kind: DriverKind::ShellPty,
        adapter_version: crate::capabilities::ADAPTER_VERSION.to_owned(),
    }
}

/// The [`remuda_signal::BusContext`] a shell-pty launch's hook socket folds
/// under, for `options` running in `cwd`.
///
/// Public so a caller that must derive ids for the same session — the Node's
/// workflow producer, and the integration test that pins the two against each
/// other — can go through the same `promote_ctx` production uses rather than
/// restating what it is expected to return.
///
/// # Errors
///
/// Propagates identity-minting failures from `promote_ctx`.
pub fn hook_bus_context(
    options: &ShellPtyOptions,
    cwd: &str,
    spec: Option<&InstanceSpec>,
) -> DriverResult<remuda_signal::BusContext> {
    Ok(bus_context(&promote_ctx(options, cwd, spec)?))
}

fn promote_ctx(
    options: &ShellPtyOptions,
    cwd: &str,
    spec: Option<&InstanceSpec>,
) -> DriverResult<promotion::PromoteCtx> {
    let instance_id = options.instance_scope();
    // The Node derives both from the same id, so a launch that disagrees with
    // its own instance directory is the c-wfdrill2 C defect coming back.
    debug_assert!(
        options.instance_id.is_none()
            || options.hooks.as_ref().is_none_or(|hooks| {
                hooks
                    .instance_dir
                    .file_name()
                    .is_none_or(|name| name.to_string_lossy() == instance_id.as_id().as_str())
            }),
        "hook instance dir must be the instance the driver is scoped to"
    );
    Ok(promotion::PromoteCtx {
        instance_id,
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
    fn alt_screen(&self) -> Option<bool> {
        self.emulator
            .as_ref()?
            .lock()
            .ok()
            .map(|emulator| emulator.alt_screen())
    }

    fn progress(&self) -> Option<remuda_screen::ProgressBar> {
        self.emulator
            .as_ref()?
            .lock()
            .ok()
            .and_then(|emulator| emulator.progress())
    }

    fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.output.subscribe()
    }

    fn snapshot(&self) -> Vec<u8> {
        self.screen_snapshot().bytes
    }

    /// §4.6: a synthesized repaint when the emulator is on, the raw ring when
    /// it is not — and an honest label either way, so Node can log which one
    /// the client actually received.
    fn screen_snapshot(&self) -> PtySnapshot {
        let ring = || {
            self.ring
                .lock()
                .map(|ring| ring.iter().copied().collect::<Vec<u8>>())
                .unwrap_or_default()
        };
        let Some(emulator) = &self.emulator else {
            return PtySnapshot::raw_ring(ring());
        };
        match emulator.lock() {
            Ok(emulator) => {
                let snapshot = remuda_screen::snapshot(Some(&emulator), ring());
                PtySnapshot {
                    bytes: snapshot.bytes,
                    source: snapshot.source,
                    alt_screen: snapshot.alt_screen,
                    progress: snapshot.progress,
                }
            }
            Err(_) => {
                tracing::warn!(
                    "pty emulator lock poisoned; serving the raw ring snapshot instead of a repaint"
                );
                PtySnapshot::raw_ring(ring())
            }
        }
    }

    async fn write_bytes(&self, bytes: &[u8]) -> DriverResult<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(DriverError::ControlUnavailable);
        }
        let mut writer = self.writer.lock().await;
        let Some(writer) = writer.as_mut() else {
            // The stop ladder released the PTY; there is nothing to write to.
            return Err(DriverError::ControlUnavailable);
        };
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
        let Some(master) = master.as_ref() else {
            // The stop ladder already released it; there is no PTY to resize.
            return Err(DriverError::ControlUnavailable);
        };
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
        if let Some(emulator) = &self.emulator {
            match emulator.lock() {
                Ok(mut emulator) => emulator.resize(cols, rows),
                // A stale emulator size skews the grid but must not fail the
                // resize: the PTY itself already took it.
                Err(_) => tracing::warn!("pty emulator lock poisoned; screen size not updated"),
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Driver for ShellPtyDriver {
    /// Capabilities for this session, not for the driver kind (§4.3, §6).
    ///
    /// The static matrix is keyed by `DriverKind`, which cannot express the
    /// thing that actually varies: a `shell-pty` carrying a promoted `claude`
    /// can steer and interrupt, and the same driver carrying a login shell
    /// cannot. The runtime override in [`crate::capabilities`] takes the kind
    /// this PTY is really running and reports steer / queue / interrupt with
    /// the provision measured for that harness — `native`, `emulated`, or, for
    /// agy, an honest `unknown`.
    async fn capabilities(&self) -> DriverResult<remuda_protocol::CapabilitySnapshot> {
        let binary = match &self.options.target {
            Target::Agent { .. } => self
                .recipe
                .lock()
                .ok()
                .and_then(|slot| slot.as_ref().map(|recipe| recipe.binary.clone())),
            Target::Shell => None,
        };
        let pin = match binary {
            Some(pin) => pin,
            None => pin_binary(&self.options.shell)?,
        };
        let mut snapshot = capability_snapshot(DriverKind::ShellPty, &pin, U64(1), U64(1))?;
        if let Some(kind) = self.session_kind() {
            snapshot.capabilities = crate::capabilities::capability_set_with_runtime(
                DriverKind::ShellPty,
                Some(&self.runtime_ref(kind)),
            );
        }
        Ok(snapshot)
    }

    async fn start(&self, spec: InstanceSpec) -> DriverResult<RunHandle> {
        if spec.driver != DriverKind::ShellPty {
            return Err(DriverError::InvalidLaunchSpec(
                "ShellPtyDriver requires driverKind shell-pty".into(),
            ));
        }
        self.spawn_at(&spec.cwd.clone(), Some(&spec)).await
    }

    /// Whether a prompt can be delivered right now (D-028 §5.2's ready ladder).
    ///
    /// A plain shell always takes bytes — it reads a line, and a line typed
    /// early just waits in the tty buffer. An agent TUI does not: a prompt
    /// arriving while it is still booting is painted over by the splash, and
    /// one arriving during a dialog answers the dialog. So an agent session is
    /// **not ready until something says it is**, and the ladder decides which
    /// something: a hook receipt if the hook path is live, else the emulator's
    /// mode set plus quiescence, else a screen signature.
    ///
    /// The `promoted_kind` check this replaces was the demo defect: a
    /// Remuda-launched `claude` has no promotion yet at create time, so it took
    /// the plain-shell branch and the first prompt was typed into the login
    /// shell — which ran it as a command. `session_kind` knows an agent from
    /// the launch target, so the gate applies from the first byte.
    ///
    /// Never an error that should tear down a PTY: `ControlUnavailable` means
    /// "hold this in the D-022 queue and ask again", which is exactly what the
    /// Node's 200 ms delivery tick does.
    async fn wait_control(&self) -> DriverResult<()> {
        let state = self.state().await?;
        let Some(_) = self.session_kind() else {
            return Ok(());
        };
        if self.screen_status() == Some(ScreenStatus::Blocked) {
            return Err(DriverError::ControlUnavailable);
        }
        let hooks = self.hooks.lock().await.clone();
        let activity = self
            .session_pid()
            .and_then(|pid| hooks.as_ref().and_then(|hooks| hooks.turn_active(pid)));
        match activity {
            Some(true) => return Err(DriverError::ControlUnavailable),
            Some(false) => return Ok(()),
            None => {}
        }
        let rung = send::ready_rung(
            true,
            self.hook_receipt(),
            state.modes(),
            self.screen_status() == Some(ScreenStatus::Idle),
        );
        match rung {
            // Rung 2 observed the composer's mode set but says nothing about
            // *this* instant, so it is paired with the quiescence wait `send`
            // also uses; the screen must additionally not be mid-turn.
            Some(send::ReadyEvidence::Quiescence) => {
                match self.screen_status() {
                    // `unknown` here is the booting TUI: no rule has matched
                    // yet. §10 forbids reading that as idle, and §5.2 says to
                    // queue rather than hard-send.
                    Some(ScreenStatus::Idle) => Ok(()),
                    _ => Err(DriverError::ControlUnavailable),
                }
            }
            Some(_) => Ok(()),
            None => Err(DriverError::ControlUnavailable),
        }
    }

    async fn attach(&self, _native_ref: remuda_protocol::NativeRef) -> DriverResult<DriverAck> {
        let _ = self.state().await?;
        Ok(DriverAck::transport_written())
    }

    async fn send(&self, input: DriverInput) -> DriverResult<DriverAck> {
        // §9.1: for an agent session, an effort switch is `/effort <level>`
        // typed through the composer and read back through the transcript.
        if self.session_kind() == Some(AgentKind::Claude)
            && let DriverInput::ModelSwitch(switch) = &input
            && let Some(level) = switch.effort.as_deref()
            && !level.is_empty()
            && switch.model_id.is_empty()
        {
            return self.switch_effort(level).await;
        }
        // §9.1: a model switch is `/model <id>` proven by the verdict.
        if self.session_kind() == Some(AgentKind::Claude)
            && let DriverInput::ModelSwitch(switch) = &input
            && !switch.model_id.is_empty()
        {
            return self.switch_model(&switch.model_id).await;
        }
        if self.session_kind() == Some(AgentKind::Claude)
            && let DriverInput::ModelSwitch(switch) = &input
            && let Some(mode) = switch.permission_mode.as_deref()
            && !mode.is_empty()
        {
            return self.switch_permission(mode).await;
        }
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
        // Same reasoning as `cancel`: a Remuda-launched agent is an agent from
        // the moment it is spawned, and a composer that got `text\r` in one
        // write during the pre-promotion window would take the bytes and never
        // submit them.
        let promoted = self.session_kind().is_some();
        let state = self.state().await?;
        let delivery = send::encode(&text, promoted, state.modes());
        state.write_bytes(&delivery.body).await?;
        let Some(submit) = delivery.submit else {
            return Ok(DriverAck::transport_written());
        };
        // §5.2: a promoted agent TUI reads its composer and its Enter as two
        // separate reads — a single write carrying both is accepted as bytes
        // and never submitted (terminal-promote-1, and [V] again in
        // claude-queue-steer-1). The gap is now "wait for the screen to go
        // quiet, capped", not a flat sleep, so a settled TUI is not charged
        // half a second per prompt.
        state.await_quiescence().await;
        state.write_bytes(&submit).await?;
        Ok(DriverAck::transport_written())
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

    /// The emulator grid this PTY is showing right now (D-028 §4.6).
    ///
    /// Reading is deliberately passive — no attach, no offset movement, no
    /// keystroke — because the case it exists for is an agent parked on a
    /// dialog *before* its first hook fires, where touching the session could
    /// change the very thing being diagnosed.
    async fn screen_read(&self) -> DriverResult<Option<crate::driver::ScreenRead>> {
        let Some(state) = self.inner.lock().await.clone() else {
            // Not started, or already closed: there is no screen. Say so
            // rather than returning an empty grid, which reads as a blank
            // terminal and would be indistinguishable from a cleared one.
            return Ok(None);
        };
        let grid = state.screen_grid();
        Ok(Some(crate::driver::ScreenRead {
            cols: state.cols.load(Ordering::SeqCst),
            rows: state.rows.load(Ordering::SeqCst),
            cursor: grid.cursor,
            alt_screen: grid.modes.alt_screen,
            emulated: grid.emulated,
            lines: grid.lines,
        }))
    }

    /// Interrupt the current turn — **not** stop the process (§5.3).
    ///
    /// For a plain shell `Ctrl+C` genuinely is the interrupt, and that is what
    /// it gets. For a promoted agent it is wrong in a way that used to lose
    /// people's sessions: in Claude, `Ctrl+C` is not "end this turn", and two
    /// of them quit. Each harness gets the key that was measured for it
    /// ([`keys`]), and a harness with no measured interrupt gets an honest
    /// `CAPABILITY_UNKNOWN` rather than a plausible keystroke and a false
    /// success.
    async fn cancel(&self) -> DriverResult<DriverAck> {
        // A direct launch knows its kind before the first promotion poll.
        // Keep P2's per-harness key table on both launch paths.
        let Some(kind) = self.session_kind() else {
            return self.write_tty(b"\x03").await;
        };
        let pid = self.session_pid();
        let hooks = self.hooks.lock().await.clone();
        let active = if kind == AgentKind::Claude {
            pid.and_then(|pid| hooks.as_ref().and_then(|hooks| hooks.turn_active(pid)))
        } else {
            None
        };
        // A hook turn boundary outranks a stale idle/working frame. Without
        // hook evidence, preserve P2's idle no-op and unknown-screen send.
        if active == Some(false)
            || (active.is_none() && self.screen_status() == Some(ScreenStatus::Idle))
        {
            return Ok(DriverAck::not_dispatched());
        }
        let Some(bytes) = keys::interrupt_bytes(kind) else {
            return Err(DriverError::CapabilityUnknown(format!(
                "the interrupt key for {kind:?} has not been verified; \
                 refusing to send a guess and report success"
            )));
        };
        let state = self.state().await?;
        // Writing the key proves only transport. Native hook/screen evidence
        // settles Claude's pending cancel, including before its first poll.
        if kind == AgentKind::Claude
            && let Some(pid) = pid
        {
            if let Ok(mut baseline) = self.interrupt_screen_markers.lock() {
                let (count, grid) = state.screen_evidence();
                baseline.arm(count, &grid);
            }
            self.interrupt_pid.store(pid, Ordering::SeqCst);
        }
        // Grok's two Ctrl+C keys need distinct reads; the first clears draft.
        if let Some(row) = keys::keys_for(kind)
            && row.interrupt.keys.len() > 1
        {
            for key in row.interrupt.keys {
                state
                    .write_bytes(&crate::tty::logical_keys_to_bytes(&[(*key).to_owned()]))
                    .await?;
                tokio::time::sleep(INTERRUPT_KEY_GAP).await;
            }
        } else {
            state.write_bytes(&bytes).await?;
        }
        Ok(DriverAck::transport_written())
    }

    /// Answer an interaction, by whichever carrier opened it (§4.4).
    ///
    /// Three carriers land here and they are answered differently:
    ///
    /// 1. **Hook** — a `PermissionRequest` / `Elicitation` whose agent is
    ///    parked on the socket. The decision goes to that process; no keys are
    ///    pressed. Only this path can be *confirmed*.
    /// 2. **Hook, ignored** — the decision was real but nothing received it
    ///    (§14 risk 1). One screen-key attempt follows, and only one.
    /// 3. **Transcript picker** — Remuda's own question, which binds the epoch.
    ///
    /// The return value never claims more than happened: a fallback that could
    /// not find a matching choice on screen reports `not-dispatched` rather
    /// than a success the user would read as "approved".
    async fn respond_interaction(
        &self,
        id: remuda_protocol::InteractionId,
        answer: remuda_protocol::InteractionAnswer,
    ) -> DriverResult<DriverAck> {
        // Route by what the answer *is*, not by whether a hook is still parked.
        // An abandoned hook is no longer in the table, and the whole point of
        // §14 risk 1 is that that case still needs answering — routing on
        // `is_parked` would send it to the transcript picker, which would
        // reject it as the wrong answer kind and lose the decision entirely.
        // Approvals and elicitations can only have come from a hook; a hook
        // question (AskUserQuestion) goes to the hook while its card is open,
        // otherwise to the transcript picker, the only other question source.
        let hook_carried = matches!(
            &answer,
            remuda_protocol::InteractionAnswer::Approval(_)
                | remuda_protocol::InteractionAnswer::Elicitation(_)
        ) || matches!(&answer, remuda_protocol::InteractionAnswer::Question(_)
            if self
                .hooks
                .lock()
                .await
                .as_ref()
                .is_some_and(|hooks| hooks.is_parked(&id)));
        if hook_carried {
            let outcome = match self.hooks.lock().await.clone() {
                Some(hooks) => hooks.resolve_answer(&id, &answer),
                // Hooks are off for this instance, so nothing was ever parked
                // and the agent's own dialog is the only place to answer.
                None => remuda_signal::Outcome::Abandoned,
            };
            if crate::hook_answer::needs_fallback(outcome) {
                // The hook stopped listening before the human decided, or the
                // wait expired. The dialog may still be on screen, so spend
                // the single fallback attempt on it (§14 risk 1).
                return self.fallback_to_screen(&id, &answer).await;
            }
            // Outcome::Answered means the reply reached the process — but a
            // confined (or otherwise restricted) session can accept the reply
            // and still keep its own dialog up. §14 risk 1: never report
            // applied unless confirmed, so for an approval verify the prompt
            // actually left the screen, and fall back exactly once if it did
            // not.
            if matches!(&answer, remuda_protocol::InteractionAnswer::Approval(_))
                && !self.approval_cleared_on_screen().await
            {
                tracing::warn!(
                    interaction = %id.as_id().as_str(),
                    "the hook accepted the decision but the approval dialog is still up; answering on screen once"
                );
                return self.fallback_to_screen(&id, &answer).await;
            }
            tracing::debug!(interaction = %id.as_id().as_str(), "hook decision delivered");
            return Ok(DriverAck::transport_written());
        }
        // The only structured question a shell-pty asks is the manual
        // transcript picker; its answer deterministically binds the epoch.
        self.bindings
            .answer(&id, &answer)
            .map_err(DriverError::InvalidLaunchSpec)?;
        Ok(DriverAck::transport_written())
    }

    /// Stop the managed process tree (§5.3).
    ///
    /// Two defects this replaces, both of which reported success while leaving
    /// processes running:
    ///
    /// * `child.try_lock()` then skip on failure. The reader thread and the
    ///   exit waiter hold that lock for long stretches, so the common outcome
    ///   was *no signal at all*, silently.
    /// * `child.kill()` — a `SIGKILL` to the login shell alone. The `claude`
    ///   started inside it is a different pid and was orphaned.
    ///
    /// The replacement signals the process **group** through
    /// [`lifecycle::stop_group`] and verifies the group is gone. When it is
    /// not, that is journaled as `stop-incomplete`; §5.3 step 4 forbids
    /// reporting `exited` over a process tree that is still there.
    async fn close(&self) -> DriverResult<DriverAck> {
        // §9.1: stop the effort switch before the PTY it types into goes away.
        self.effort_queue.lock().await.close();
        if let Some(worker) = self.effort_worker.lock().await.take() {
            worker.abort();
        }
        self.model_queue.lock().await.close();
        if let Some(worker) = self.model_worker.lock().await.take() {
            worker.abort();
        }
        // Same for the permission wheel worker.
        self.permission_queue.lock().await.close();
        if let Some(worker) = self.permission_worker.lock().await.take() {
            worker.abort();
        }
        self.events_tx.lock().await.take();
        if let Some(poller) = self.poller.lock().await.take() {
            poller.abort();
        }
        if let Ok(mut slot) = self.promoted.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = self.status.lock() {
            *slot = None;
        }
        // Release any hook parked on an approval before the socket goes away.
        // Otherwise each one holds its agent's turn open until its own
        // deadline, minutes after the session has stopped.
        if let Some(hooks) = self.hooks.lock().await.as_ref() {
            hooks.retire_parked();
        }
        // Unbinds the socket and removes the socket file. The overlay and shims
        // stay for `instance.purge` to remove with the rest of the instance
        // directory — they are launch audit evidence until then.
        self.hooks.lock().await.take();
        // Stop the file-tail adapters; their shadow files likewise survive for
        // purge as audit evidence.
        if let Some(handle) = self.adapters.lock().await.take() {
            handle.abort();
        }
        self.interrupt_pid.store(0, Ordering::SeqCst);
        // Drop any transcript claim so a respawn starts a fresh epoch.
        self.bindings.demobilize();
        let Some(state) = self.inner.lock().await.take() else {
            return Ok(DriverAck::not_dispatched());
        };
        // Set before the ladder runs so the exit waiter reads this exit as the
        // ladder working rather than as an unexplained crash.
        state.closed.store(true, Ordering::SeqCst);
        let outcome = self.stop_tree(&state).await;
        if let Some(waiter) = self.waiter.lock().await.take() {
            waiter.abort();
        }
        match outcome {
            Ok(outcome) if outcome.group_gone => {
                tracing::info!(
                    rung = outcome.rung.label(),
                    pgid = outcome.pgid,
                    "pty process group stopped"
                );
            }
            Ok(outcome) => {
                // §5.3 step 4: say so rather than claiming a clean stop.
                tracing::error!(
                    rung = outcome.rung.label(),
                    pgid = outcome.pgid,
                    survivors = ?outcome.survivors,
                    "stop-incomplete: the process group outlived SIGKILL"
                );
            }
            Err(error) => tracing::error!(%error, "pty stop ladder failed"),
        }
        Ok(DriverAck::not_dispatched())
    }

    /// Continue a native session in a fresh PTY (§5.6).
    ///
    /// Resume is not a separate mechanism: it is [`Self::start`] with the
    /// harness's own resume flag prefilled, which is why this hands straight
    /// back to the same spawn path. A shell has no session to continue and
    /// still says so.
    async fn resume(&self, native_ref: remuda_protocol::NativeRef) -> DriverResult<RunHandle> {
        let Target::Agent { .. } = self.options.target else {
            return Err(DriverError::CapabilityUnsupported(
                "a login shell has no native session to resume".into(),
            ));
        };
        let remuda_protocol::Knowledge::Known { value: session_id } = &native_ref.session_id else {
            // D-026: never `--continue`. Without an explicit id there is no
            // safe guess — "the most recent session" is a different
            // conversation as often as it is the right one.
            return Err(DriverError::NativeSessionNotFound);
        };
        self.start_resumed_inner(None, session_id.clone()).await
    }

    /// D-026's cross-instance resume: a *new* instance continuing an old
    /// session, so the spec comes from the caller and only the identity is
    /// inherited.
    async fn start_resumed(
        &self,
        spec: InstanceSpec,
        session_id: String,
    ) -> DriverResult<RunHandle> {
        if spec.driver != DriverKind::ShellPty {
            return Err(DriverError::InvalidLaunchSpec(
                "ShellPtyDriver requires driverKind shell-pty".into(),
            ));
        }
        self.start_resumed_inner(Some(spec), session_id).await
    }
}

fn pty_err(err: impl std::fmt::Display) -> DriverError {
    DriverError::Io(io::Error::other(err.to_string()))
}

fn build_command(
    options: &ShellPtyOptions,
    spec_cwd: &str,
    recipe: &LaunchRecipe,
    hooks: Option<&Arc<crate::launch::HookSession>>,
) -> DriverResult<CommandBuilder> {
    let cwd = if Path::new(spec_cwd).is_dir() {
        PathBuf::from(spec_cwd)
    } else {
        options.cwd.clone()
    };
    let mut cmd = match &options.target {
        // §5.1 step 3: argv[0] is the recipe's pinned binary. Reaching into
        // `options.args` here is what would let a caller name any executable.
        Target::Agent { kind, .. } => launch::agent_command(recipe, *kind, &cwd)?,
        Target::Shell if options.args.is_empty() => {
            let mut cmd = CommandBuilder::new(&options.shell);
            cmd.arg("-l");
            cmd
        }
        Target::Shell => {
            let mut cmd = CommandBuilder::new(&options.args[0]);
            for arg in options.args.iter().skip(1) {
                cmd.arg(arg);
            }
            cmd
        }
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
    // §5.1 step 1 / §9.1: an inherited `CLAUDE_CODE_EFFORT_LEVEL` outranks the
    // in-session `/effort` command, so a value leaking in from the operator's
    // shell would pin the effort for the whole session and make the UI's
    // "effective" readout a lie. `child_env` denies it, and this loop honours
    // that; the assertion is that nothing below re-adds it.
    for (key, value) in agent_env(&options.target, recipe, options.pin_native_home) {
        if crate::child_env::is_denied(&key) {
            tracing::warn!(%key, "recipe env entry is on the deny list; not forwarding");
            continue;
        }
        cmd.env(key, value);
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

/// Literal env the recipe asks for: `CLAUDE_CONFIG_DIR` / `CODEX_HOME` /
/// `GROK_HOME` shadow dirs and provider overlay values (§5.1).
///
/// Only [`EnvAllowlistSource::NativeHome`] and
/// [`EnvAllowlistSource::ProviderOverlay`] entries carry a value inline.
/// `Credential` entries name a secret the broker resolves and must never be
/// read from here, and `HostEnv` entries are inherited, not set.
///
/// Claude's preset has no `home_env` (its config dir is not part of argv
/// materialization), so without the explicit pin here a shell-pty agent child
/// inherits the Node's own `CLAUDE_CONFIG_DIR`/`$HOME` and writes its
/// transcript somewhere other than the registered home the promotion poller
/// scans — the native-carrier-3 `transcript_unbound` defect.
fn agent_env(
    target: &Target,
    recipe: &LaunchRecipe,
    pin_native_home: bool,
) -> Vec<(String, String)> {
    if target.agent_kind().is_none() {
        return Vec::new();
    }
    let mut entries = recipe
        .env_allowlist
        .iter()
        .filter(|entry| {
            matches!(
                entry.source,
                crate::recipe::EnvAllowlistSource::NativeHome
                    | crate::recipe::EnvAllowlistSource::ProviderOverlay
            )
        })
        .filter_map(|entry| match entry.name.as_str() {
            "CODEX_HOME" | "GROK_HOME" => Some((entry.name.clone(), recipe.native_home.clone())),
            "GROK_DISABLE_AUTOUPDATER" => Some((entry.name.clone(), "1".to_owned())),
            _ => None,
        })
        .collect::<Vec<_>>();
    if pin_native_home
        && target.agent_kind() == Some(AgentKind::Claude)
        && !recipe.native_home.is_empty()
        && !entries.iter().any(|(name, _)| name == "CLAUDE_CONFIG_DIR")
    {
        entries.push(("CLAUDE_CONFIG_DIR".into(), recipe.native_home.clone()));
        // Keep the *credentials* where the operator logged in, while the
        // config stays scoped.
        //
        // Claude 2.1 namespaces its OS credential store by config directory:
        // the macOS keychain service is `Claude Code…-credentials` plus, when
        // `CLAUDE_CONFIG_DIR` is set, `-<sha256(dir)[..8]>`. Pinning a
        // per-instance home therefore points the CLI at a namespace nobody has
        // ever logged into, and the session answers "Not logged in · Please
        // run /login" instead of the prompt — exactly what the macOS demo hit.
        // `CLAUDE_SECURESTORAGE_CONFIG_DIR=""` selects the default (unsuffixed)
        // namespace explicitly, so the launch reads the same credential a human
        // running `claude` in this account would.
        //
        // Remuda neither reads, copies nor writes the credential: this names a
        // lookup namespace, and the value stays in the OS keychain throughout.
        if !entries
            .iter()
            .any(|(name, _)| name == "CLAUDE_SECURESTORAGE_CONFIG_DIR")
        {
            entries.push(("CLAUDE_SECURESTORAGE_CONFIG_DIR".into(), String::new()));
        }
    }
    entries
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
                // Keep cancellation evidence on one output generation. All
                // snapshot/update paths acquire this before ring/emulator.
                let mut markers = state.interruption_output.lock().ok();
                if let Ok(mut ring) = state.ring.lock() {
                    ring.extend(chunk.iter().copied());
                    while ring.len() > TTY_SNAPSHOT_MAX {
                        ring.pop_front();
                    }
                }
                // §4.1: the emulator sees every byte the ring sees. It is fed
                // after the ring and before the fan-out so a failure here can
                // only degrade signatures, never the live stream.
                if let Some(emulator) = &state.emulator {
                    match emulator.lock() {
                        Ok(mut emulator) => emulator.feed(&chunk),
                        Err(_) => tracing::warn!(
                            "pty emulator lock poisoned; screen state will drift from the ring"
                        ),
                    }
                }
                // Quiescence is measured from this counter (§5.2).
                state.reads.fetch_add(1, Ordering::SeqCst);
                if let Some(markers) = markers.as_mut() {
                    markers.feed(&chunk);
                }
                drop(markers);
                let _ = state.output.send(chunk);
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/// Watch for the PTY's process ending and journal it (§5.5).
///
/// Two witnesses, because either can arrive first:
///
/// * `child.wait()` carries the exit code or signal, and is the evidence we
///   want — but on a PTY it can block past the point the session is visibly
///   over.
/// * Master EOF says the slave is closed at both ends. It is often first, and
///   it is the only witness when `wait()` is racing something else.
///
/// Whichever lands first decides, and the waiter then gives `wait()` a short
/// window to upgrade an EOF into a real status. §5.5's budget is 2 s from
/// process death to lifecycle event; the wait is blocking, so it runs on a
/// blocking thread rather than starving the runtime.
#[allow(clippy::too_many_arguments)]
fn spawn_exit_waiter(
    state: Arc<PtyState>,
    ctx: promotion::PromoteCtx,
    events: mpsc::Sender<remuda_protocol::Observation>,
    seq: Arc<AtomicU64>,
    exited: Arc<std::sync::Mutex<Option<ExitEvidence>>>,
    promoted: Arc<std::sync::Mutex<Option<Detected>>>,
    eof: tokio::sync::oneshot::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // `try_wait` on a timer rather than a blocking `wait()`. A blocking
        // wait parks a thread inside `child`'s lock until the process ends —
        // which is both the lock `close` would then contend for and a thread
        // the tokio runtime cannot join at shutdown, so dropping a runtime
        // while an agent is alive would hang until that agent happened to
        // exit. The poll interval is far inside §5.5's 2 s budget.
        let reaped = {
            let state = Arc::clone(&state);
            async move {
                loop {
                    {
                        let mut child = state.child.lock().await;
                        match child.try_wait() {
                            Ok(Some(status)) => {
                                return Some(lifecycle::evidence_from_status(&status));
                            }
                            Ok(None) => {}
                            Err(error) => {
                                tracing::debug!(%error, "pty child wait failed");
                                return None;
                            }
                        }
                    }
                    tokio::time::sleep(EXIT_POLL).await;
                }
            }
        };
        tokio::pin!(reaped);
        let evidence = tokio::select! {
            status = &mut reaped => match status {
                Some(evidence) => evidence,
                // The wait itself failed; EOF still tells us it is over.
                None => ExitEvidence::Eof,
            },
            _ = eof => {
                // EOF won. Give the reaper a moment to upgrade this to a real
                // status before settling for the weaker witness.
                match tokio::time::timeout(EXIT_STATUS_GRACE, reaped).await {
                    Ok(Some(evidence)) => evidence,
                    _ => ExitEvidence::Eof,
                }
            }
        };
        if state.closed.load(Ordering::SeqCst) {
            // `close` already ran the stop ladder and journaled the outcome;
            // this exit is that ladder working, not news.
            if let Ok(mut slot) = exited.lock() {
                slot.get_or_insert(evidence);
            }
            return;
        }
        // §5.5: a promoted instance distinguishes two deaths. The agent going
        // away is a *demote* — the shell it was typed into is still there and
        // the instance is very much alive. Only the PTY's own process ending
        // is an instance exit, and that is what this waiter observes: it waits
        // on the child the driver spawned, never on the promoted agent.
        let promoted_kind = promoted
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|found| promotion::kind_name(found.kind)));
        if let Ok(mut slot) = exited.lock() {
            *slot = Some(evidence.clone());
        }
        let mut related = std::collections::BTreeMap::new();
        related.insert("reason".to_owned(), evidence.reason());
        if let Some(code) = evidence.code() {
            related.insert("exitCode".to_owned(), code.to_string());
        }
        if let Some(signal) = evidence.signal() {
            related.insert("signal".to_owned(), signal.to_owned());
        }
        if let Some(kind) = promoted_kind {
            related.insert("promotedKind".to_owned(), kind.to_owned());
        }
        let severity = if evidence.lifecycle() == "exited" {
            remuda_protocol::Severity::Info
        } else {
            // The Node folds `Error` into instance `failed`; a crashed agent
            // must reach that fold rather than looking like a clean close.
            remuda_protocol::Severity::Error
        };
        let payload = remuda_protocol::ObservationPayload::Lifecycle(Box::new(
            remuda_protocol::LifecyclePayload::Native(Box::new(remuda_protocol::NativeLifecycle {
                topic: remuda_protocol::LifecycleTopic::Session,
                native_name: NATIVE_EXIT.to_owned(),
                native_id: remuda_protocol::Knowledge::NotApplicable,
                status: remuda_protocol::Knowledge::Known {
                    value: evidence.lifecycle().to_owned(),
                },
                related_ids: related,
                data_ref: None,
                severity,
                affects_completion: true,
            })),
        ));
        if let Err(error) = promotion::emit_payload(
            &events,
            &seq,
            &ctx,
            remuda_protocol::SourceChannel::Runtime,
            remuda_protocol::Completeness::Structured,
            payload,
        )
        .await
        {
            tracing::debug!(%error, "native exit lifecycle not journaled");
        }
    })
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
            // Login shell, not an agent CLI: `spec.binaryPath` is not read on
            // this path, so the executable is never a caller's choice.
            binary_override: false,
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

/// Single-quote a path for a POSIX shell, closing and reopening the quote
/// around any embedded single quote.
fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

/// §9.1 [`crate::effort::EffortSwitchIo`] backed by the local native PTY.
struct ShellEffortIo {
    state: Arc<PtyState>,
    events: mpsc::Sender<remuda_protocol::Observation>,
    seq: Arc<AtomicU64>,
    ctx: promotion::PromoteCtx,
}

#[async_trait]
impl crate::effort::EffortSwitchIo for ShellEffortIo {
    async fn is_idle(&self) -> bool {
        let rung = send::ready_rung(
            true,
            false,
            self.state.modes(),
            self_state_idle_helper(&self.state),
        );
        matches!(
            rung,
            Some(send::ReadyEvidence::Quiescence) | Some(send::ReadyEvidence::Glyph)
        )
    }

    async fn type_body(&self, body: &str) -> crate::DriverResult<()> {
        self.state.write_bytes(body.as_bytes()).await
    }

    async fn press_enter(&self) -> crate::DriverResult<()> {
        // Enter is its own write, exactly as `send::encode` requires for an
        // agent composer.
        self.state.write_bytes(b"\r").await
    }

    async fn screen_text(&self) -> crate::DriverResult<String> {
        Ok(self.state.screen_grid().text())
    }

    async fn journal(&self, status: String, severity: remuda_protocol::Severity) {
        let payload = promotion::effort_lifecycle(&status, severity);
        if promotion::emit_payload(
            &self.events,
            &self.seq,
            &self.ctx,
            SourceChannel::Runtime,
            Completeness::Structured,
            payload,
        )
        .await
        .is_err()
        {
            tracing::debug!("effort lifecycle dropped: event channel closed");
        }
    }
}

/// Read the emulator/screen idle signal without a `&ShellPtyDriver`.
fn self_state_idle_helper(state: &PtyState) -> bool {
    remuda_screen::screen_status(&state.screen_grid()) == Some(ScreenStatus::Idle)
}

/// [`crate::permission::PermissionSwitchIo`] for the local PTY carrier.
struct ShellPermissionIo {
    state: Arc<PtyState>,
    events: mpsc::Sender<remuda_protocol::Observation>,
    seq: Arc<AtomicU64>,
    ctx: promotion::PromoteCtx,
}

#[async_trait]
impl crate::permission::PermissionSwitchIo for ShellPermissionIo {
    async fn is_idle(&self) -> bool {
        let rung = send::ready_rung(
            true,
            false,
            self.state.modes(),
            self_state_idle_helper(&self.state),
        );
        matches!(
            rung,
            Some(send::ReadyEvidence::Quiescence) | Some(send::ReadyEvidence::Glyph)
        )
    }

    async fn send_cycle(&self) -> crate::DriverResult<()> {
        self.state.write_bytes(b"\x1b[Z").await
    }

    async fn press_down(&self) -> crate::DriverResult<()> {
        self.state.write_bytes(b"\x1b[B").await
    }

    async fn press_enter(&self) -> crate::DriverResult<()> {
        self.state.write_bytes(b"\r").await
    }

    async fn press_esc(&self) -> crate::DriverResult<()> {
        self.state.write_bytes(b"\x1b").await
    }

    async fn screen_text(&self) -> crate::DriverResult<String> {
        Ok(self.state.screen_grid().text())
    }

    async fn journal(&self, status: String, severity: remuda_protocol::Severity) {
        let payload = promotion::permission_lifecycle(&status, severity);
        if promotion::emit_payload(
            &self.events,
            &self.seq,
            &self.ctx,
            SourceChannel::Runtime,
            Completeness::Structured,
            payload,
        )
        .await
        .is_err()
        {
            tracing::debug!("permission lifecycle dropped: event channel closed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for RecordingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// c-wfdrill2 C. `promote_ctx` minted `InstanceId::new()`, so every id the
    /// driver derived — hook tool nodes through `remuda_signal`, the
    /// transcript replay's `TranscriptMapper` — was scoped to an id nothing
    /// else in the system knew. The Node's own workflow producer derives
    /// `workflow.run.toolCallId` from the same native tool id under the REAL
    /// instance, so the two could never name the same node and no subagent row
    /// could fold under its Workflow row.
    #[test]
    fn promote_ctx_is_scoped_to_the_instance_the_node_named() {
        let dir = tempfile::tempdir().unwrap();
        let instance = InstanceId::new();
        let mut options = ShellPtyOptions::login(dir.path().to_path_buf());
        options.instance_id = Some(instance.clone());
        let ctx = promote_ctx(&options, &dir.path().to_string_lossy(), None).expect("ctx");
        assert_eq!(
            ctx.instance_id, instance,
            "the driver must derive under the Node's instance, not a throwaway"
        );

        // Left unset (tests, `ShellPtyDriver::spawn`) it still mints one, and
        // two contexts never accidentally share it.
        let anonymous = ShellPtyOptions::login(dir.path().to_path_buf());
        let first = promote_ctx(&anonymous, &dir.path().to_string_lossy(), None).expect("ctx");
        let second = promote_ctx(&anonymous, &dir.path().to_string_lossy(), None).expect("ctx");
        assert_ne!(first.instance_id, second.instance_id);
        assert_ne!(first.instance_id, instance);
    }

    #[tokio::test]
    async fn a_direct_claude_recipe_preserves_settings_and_audits_the_hook_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let instance = dir.path().join("instance");
        let operator = dir.path().join("operator-settings.json");
        let settings = serde_json::json!({
            "env": {"ANTHROPIC_BASE_URL": "https://provider.example", "CUSTOM": "retained"},
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "operator-stop"}]}]}
        });
        std::fs::write(&operator, settings.to_string()).unwrap();
        let mut spec: InstanceSpec =
            serde_json::from_str(include_str!("../tests/fixtures/instance-spec.json")).unwrap();
        spec.driver = DriverKind::ShellPty;
        spec.cwd = dir.path().to_string_lossy().into_owned();
        let mut options = ShellPtyOptions::agent(
            dir.path().to_path_buf(),
            AgentKind::Claude,
            AgentLaunch {
                profile: Box::new(crate::profile::ProviderProfile {
                    id: spec.provider_profile.id.clone(),
                    kind: ProviderKind::Anthropic,
                    base_url: String::new(),
                    delegation: Delegation::None,
                    secret_ref: None,
                    models: vec!["sonnet".into()],
                    health: crate::profile::ProviderHealth::Healthy,
                }),
                launch_dir: instance.join("launch"),
                native_home: dir.path().to_path_buf(),
                binary: Some(PathBuf::from("/bin/sh")),
                origin: crate::materializer::LaunchOrigin::Human,
                settings_overlay: Some(operator.clone()),
            },
        );
        options.hooks = Some(HookConfig {
            instance_dir: instance.clone(),
            relay_binary: PathBuf::from("/nonexistent/remuda"),
            tui: crate::launch::TuiMode::Fullscreen,
        });
        let mut driver = ShellPtyDriver::new(options);
        let ctx = promote_ctx(&driver.options, &spec.cwd, Some(&spec)).unwrap();
        let (events, _rx) = mpsc::channel(8);
        let seq = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let interrupt_pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
        let (recipe, hooks, _) = prepare_launch_blocking(
            &driver.options,
            &spec.cwd,
            Some(&spec),
            &ctx,
            &events,
            &seq,
            &interrupt_pid,
        )
        .unwrap();
        let hooks = hooks.unwrap();
        assert!(recipe.argv.windows(2).any(|args| {
            args[0] == "--settings" && args[1] == hooks.overlay.path.to_string_lossy()
        }));
        assert_eq!(
            recipe.audit.settings_digest.as_ref(),
            Some(&hooks.overlay.digest)
        );
        let merged: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&hooks.overlay.path).unwrap()).unwrap();
        assert_eq!(merged["env"], settings["env"]);
        assert_eq!(merged["hooks"]["Stop"][0], settings["hooks"]["Stop"][0]);
        for event in crate::launch::overlay::HOOK_EVENTS {
            assert!(
                merged["hooks"][event].to_string().contains("hook emit"),
                "{event}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(&operator).unwrap(),
            settings.to_string()
        );
        drop(hooks);

        let invalid_instance = dir.path().join("invalid-instance");
        driver.options.hooks.as_mut().unwrap().instance_dir = invalid_instance.clone();
        driver.options.agent.as_mut().unwrap().binary = Some(dir.path().join("missing-binary"));
        assert!(
            prepare_launch_blocking(
                &driver.options,
                &spec.cwd,
                Some(&spec),
                &ctx,
                &events,
                &seq,
                &interrupt_pid,
            )
            .is_err()
        );
        assert!(
            !invalid_instance.exists(),
            "validate before creating hook artifacts"
        );
    }

    /// native-carrier-3: a bypass launch must carry the disclaimer acceptance
    /// in the merged overlay, the native home must be pinned for a Claude
    /// agent, and without hooks the bypass-only overlay is still written.
    #[tokio::test]
    async fn a_bypass_native_launch_pins_its_home_and_suppresses_the_disclaimer() {
        let dir = tempfile::tempdir().unwrap();
        let instance = dir.path().join("instance");
        let native_home = dir.path().join("native-home");
        let mut spec: InstanceSpec =
            serde_json::from_str(include_str!("../tests/fixtures/instance-spec.json")).unwrap();
        spec.driver = DriverKind::ShellPty;
        spec.cwd = dir.path().to_string_lossy().into_owned();
        spec.permission_mode =
            remuda_protocol::PermissionMode::Claude(Box::new(remuda_protocol::ClaudePermission {
                mode: remuda_protocol::ClaudePermissionMode::BypassPermissions,
                interaction: remuda_protocol::ClaudeInteractionMode::NativeTty,
            }));

        let build = |hooks_on: bool| {
            let mut options = ShellPtyOptions::agent(
                dir.path().to_path_buf(),
                AgentKind::Claude,
                AgentLaunch {
                    profile: Box::new(crate::profile::ProviderProfile {
                        id: spec.provider_profile.id.clone(),
                        kind: ProviderKind::Anthropic,
                        base_url: String::new(),
                        delegation: Delegation::None,
                        secret_ref: None,
                        models: vec!["sonnet".into()],
                        health: crate::profile::ProviderHealth::Healthy,
                    }),
                    launch_dir: instance.join("launch"),
                    native_home: native_home.clone(),
                    binary: Some(PathBuf::from("/bin/sh")),
                    origin: crate::materializer::LaunchOrigin::Human,
                    settings_overlay: None,
                },
            );
            options.pin_native_home = true;
            if hooks_on {
                options.hooks = Some(HookConfig {
                    instance_dir: instance.clone(),
                    relay_binary: PathBuf::from("/nonexistent/remuda"),
                    tui: crate::launch::TuiMode::Default,
                });
            }
            options
        };

        for hooks_on in [true, false] {
            let options = build(hooks_on);
            let ctx = promote_ctx(&options, &spec.cwd, Some(&spec)).unwrap();
            let (events, _rx) = mpsc::channel(8);
            let seq = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let interrupt_pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
            let (recipe, hooks, _) = prepare_launch_blocking(
                &options,
                &spec.cwd,
                Some(&spec),
                &ctx,
                &events,
                &seq,
                &interrupt_pid,
            )
            .unwrap();
            // The settings file the recipe argv carries holds the acceptance.
            let settings_path = recipe
                .argv
                .windows(2)
                .find_map(|pair| (pair[0] == "--settings").then(|| PathBuf::from(&pair[1])))
                .unwrap_or_else(|| {
                    panic!("bypass launch must carry --settings (hooks={hooks_on})")
                });
            let written: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
            assert_eq!(
                written["skipDangerousModePermissionPrompt"],
                serde_json::json!(true),
                "hooks={hooks_on}"
            );
            if hooks_on {
                let hooks = hooks.expect("hooks requested");
                assert_eq!(settings_path, hooks.overlay.path);
            } else {
                assert!(hooks.is_none(), "the bypass overlay exists without hooks");
            }
            // The native home pin travels through the recipe env applied to
            // the child command, not via inherited ambient env.
            let pinned = agent_env(&options.target, &recipe, options.pin_native_home);
            let pinned_home = native_home.to_string_lossy().into_owned();
            assert!(
                pinned
                    .iter()
                    .any(|(name, value)| name == "CLAUDE_CONFIG_DIR" && value == &pinned_home),
                "hooks={hooks_on}: CLAUDE_CONFIG_DIR must pin the registered home"
            );
            // native-carrier-4: pinning the config dir moves Claude's OS
            // credential namespace with it (the macOS keychain service is
            // suffixed with a hash of CLAUDE_CONFIG_DIR), so the pin alone
            // makes an authenticated operator look logged out. The empty
            // securestorage override selects the default namespace, keeping
            // the config scoped and the credential lookup where the human
            // logged in.
            assert!(
                pinned
                    .iter()
                    .any(|(name, value)| name == "CLAUDE_SECURESTORAGE_CONFIG_DIR"
                        && value.is_empty()),
                "hooks={hooks_on}: a pinned home must keep the default credential namespace"
            );
        }

        // Without the pin the env stays silent — an inherited home must not be
        // silently redirected at an empty directory.
        let unpinned_options = {
            let mut options = build(false);
            options.pin_native_home = false;
            options
        };
        let ctx = promote_ctx(&unpinned_options, &spec.cwd, Some(&spec)).unwrap();
        let (events, _rx) = mpsc::channel(8);
        let seq = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let interrupt_pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
        let (recipe, _, _) = prepare_launch_blocking(
            &unpinned_options,
            &spec.cwd,
            Some(&spec),
            &ctx,
            &events,
            &seq,
            &interrupt_pid,
        )
        .unwrap();
        let unpinned = agent_env(&unpinned_options.target, &recipe, false);
        assert!(
            !unpinned.iter().any(|(name, _)| name == "CLAUDE_CONFIG_DIR"),
            "an unpinned launch must not invent a config-dir env"
        );
        // An inherited home already resolves to the default credential
        // namespace, so overriding it there would be noise at best and, if the
        // operator had set the variable themselves, a silent contradiction.
        assert!(
            !unpinned
                .iter()
                .any(|(name, _)| name == "CLAUDE_SECURESTORAGE_CONFIG_DIR"),
            "an unpinned launch must leave the credential namespace alone"
        );
    }

    /// native-config-1: when the carrier pins a scoped native home the
    /// launching user's effective settings are copied into the per-instance
    /// overlay — gateway env (including the credential), model,
    /// modelSettings, statusLine and the user's own hooks all survive; the
    /// relay hooks are appended per event and the terminal pins are forced.
    /// Credential bytes live only in the 0600 instance file.
    #[tokio::test]
    async fn a_scoped_native_home_merges_the_users_effective_settings() {
        let dir = tempfile::tempdir().unwrap();
        let user_home = dir.path().join("real-claude-home");
        std::fs::create_dir_all(&user_home).unwrap();
        std::fs::write(
            user_home.join("settings.json"),
            serde_json::json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://gateway.example.invalid",
                    "ANTHROPIC_AUTH_TOKEN": "gateway-secret-token",
                    "ANTHROPIC_MODEL": "shared-gateway-model",
                    "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY": "1"
                },
                "model": "shared-gateway-model",
                "modelSettings": [{"name": "model_hub/es1_orange_o48"}],
                "statusLine": {"type": "command", "command": "echo ready"},
                "hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "user-session-start"}]}]},
                "theme": "dark"
            })
            .to_string(),
        )
        .unwrap();
        // settings.local.json wins scalars and merges env per variable.
        std::fs::write(
            user_home.join("settings.local.json"),
            serde_json::json!({
                "model": "local-gateway-model",
                "env": {"ANTHROPIC_MODEL": "local-gateway-model", "EXTRA": "kept"},
                "verbose": true
            })
            .to_string(),
        )
        .unwrap();

        let mut spec: InstanceSpec =
            serde_json::from_str(include_str!("../tests/fixtures/instance-spec.json")).unwrap();
        spec.driver = DriverKind::ShellPty;
        spec.cwd = dir.path().to_string_lossy().into_owned();

        let build = |hooks_on: bool| {
            // Fresh instance dir per variant, like production: a hooks-off
            // launch must never read a previous launch's settings.json.
            let inst = dir.path().join(if hooks_on {
                "instance-hooks"
            } else {
                "instance-plain"
            });
            let mut options = ShellPtyOptions::agent(
                dir.path().to_path_buf(),
                AgentKind::Claude,
                AgentLaunch {
                    profile: Box::new(crate::profile::ProviderProfile {
                        id: spec.provider_profile.id.clone(),
                        kind: ProviderKind::Anthropic,
                        base_url: String::new(),
                        delegation: Delegation::None,
                        secret_ref: None,
                        models: vec!["sonnet".into()],
                        health: crate::profile::ProviderHealth::Healthy,
                    }),
                    launch_dir: inst.join("launch"),
                    native_home: dir.path().join("scoped-native-home"),
                    binary: Some(PathBuf::from("/bin/sh")),
                    origin: crate::materializer::LaunchOrigin::Human,
                    settings_overlay: None,
                },
            );
            options.user_settings_home = Some(user_home.clone());
            if hooks_on {
                options.hooks = Some(HookConfig {
                    instance_dir: inst,
                    relay_binary: PathBuf::from("/nonexistent/remuda"),
                    tui: crate::launch::TuiMode::Fullscreen,
                });
            }
            options
        };

        for hooks_on in [true, false] {
            let options = build(hooks_on);
            let ctx = promote_ctx(&options, &spec.cwd, Some(&spec)).unwrap();
            let (events, _rx) = mpsc::channel(8);
            let seq = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let interrupt_pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
            let (recipe, hooks, _) = prepare_launch_blocking(
                &options,
                &spec.cwd,
                Some(&spec),
                &ctx,
                &events,
                &seq,
                &interrupt_pid,
            )
            .unwrap();
            let settings_path = recipe
                .argv
                .windows(2)
                .find_map(|pair| (pair[0] == "--settings").then(|| PathBuf::from(&pair[1])))
                .unwrap_or_else(|| panic!("seeded settings must be carried (hooks={hooks_on})"));
            let body = std::fs::read_to_string(&settings_path).unwrap();
            let merged: serde_json::Value = serde_json::from_str(&body).unwrap();
            // Gateway model config reaches the child exactly like a terminal.
            assert_eq!(
                merged["env"]["ANTHROPIC_BASE_URL"], "https://gateway.example.invalid",
                "hooks={hooks_on}"
            );
            assert_eq!(
                merged["env"]["ANTHROPIC_AUTH_TOKEN"], "gateway-secret-token",
                "hooks={hooks_on}: the credential is copied from the user's own settings only"
            );
            assert_eq!(
                merged["env"]["CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"], "1",
                "hooks={hooks_on}"
            );
            assert_eq!(merged["env"]["ANTHROPIC_MODEL"], "local-gateway-model");
            assert_eq!(merged["env"]["EXTRA"], "kept");
            assert_eq!(merged["model"], "local-gateway-model");
            assert_eq!(
                merged["modelSettings"][0]["name"],
                "model_hub/es1_orange_o48"
            );
            assert_eq!(merged["statusLine"]["command"], "echo ready");
            assert_eq!(merged["theme"], "dark");
            assert_eq!(merged["verbose"], true);
            if hooks_on {
                let hooks = hooks.expect("hooks requested");
                assert_eq!(settings_path, hooks.overlay.path);
                // User hook first, relay appended for every registered event.
                let session_start = merged["hooks"]["SessionStart"].as_array().unwrap();
                assert_eq!(
                    session_start[0]["hooks"][0]["command"], "user-session-start",
                    "the user's hook keeps its leading position"
                );
                assert!(
                    session_start
                        .iter()
                        .any(|matcher| matcher.to_string().contains("hook emit")),
                    "the relay registration is appended, not replaced"
                );
                assert_eq!(merged["tui"], "fullscreen");
                assert_eq!(merged["terminalProgressBarEnabled"], true);
            } else {
                assert!(hooks.is_none(), "the seeded overlay exists without hooks");
                // The user's own hooks are copied, but no relay registration
                // is invented on the hooks-off path.
                if let Some(events) = merged.get("hooks").and_then(|h| h.as_object()) {
                    assert!(
                        !events.values().any(|m| m.to_string().contains("hook emit")),
                        "hooks-off must not register the relay: {events:?}"
                    );
                }
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(&settings_path)
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600,
                    "credentials stay in a 0600 instance file"
                );
            }
            // The redaction path masks the token while keeping diagnostics.
            let redacted = crate::launch::redact_settings(&merged).to_string();
            assert!(!redacted.contains("gateway-secret-token"), "{redacted}");
            assert!(redacted.contains("local-gateway-model"), "{redacted}");
        }

        // The user's own settings files are read, never modified.
        assert!(
            std::fs::read_to_string(user_home.join("settings.json"))
                .unwrap()
                .contains("gateway-secret-token")
        );
    }

    /// gateway-carryover-1: the whole path, through the real materializer.
    ///
    /// The host user has their own gateway configured (base URL, token, model
    /// and the model env trio); the Hub delivered a different gateway. The
    /// merged file the child is handed must talk to the Hub's, run the Hub's
    /// model, keep the user's hooks and permissions, and carry no host provider
    /// variable at all. The journal line must say so.
    #[tokio::test]
    async fn a_gateway_delegation_overrides_the_hosts_provider_and_journals_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let user_home = dir.path().join("user-claude");
        std::fs::create_dir_all(&user_home).unwrap();
        // The host's own settings, as in the owner report.
        std::fs::write(
            user_home.join("settings.json"),
            serde_json::json!({
                "model": "ark/seed-evolving[1m]",
                "theme": "dark",
                "permissions": {"allow": ["Read"], "deny": ["Read(./.env)"]},
                "hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "user-session-start"}]}]},
                "statusLine": {"type": "command", "command": "echo host"},
                "env": {
                    "ANTHROPIC_BASE_URL": "https://host-native.example/api",
                    "ANTHROPIC_AUTH_TOKEN": "host-token-placeholder",
                    "ANTHROPIC_MODEL": "ark/seed-evolving[1m]",
                    "ANTHROPIC_DEFAULT_OPUS_MODEL": "ark/host-opus",
                    "ANTHROPIC_DEFAULT_SONNET_MODEL": "ark/host-sonnet",
                    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "ark/host-haiku",
                    "CLAUDE_CODE_SUBAGENT_MODEL": "ark/host-subagent",
                    "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "1000000",
                    "HOST_ONLY": "keep-me"
                }
            })
            .to_string(),
        )
        .unwrap();
        // What the Hub delivered for the chosen gateway profile.
        let provider_overlay = dir.path().join("provider-settings.json");
        std::fs::write(
            &provider_overlay,
            serde_json::json!({
                "model": "passthrough/ark/seed-evolving",
                "env": {
                    "ANTHROPIC_BASE_URL": "https://gateway.example/v1",
                    "ANTHROPIC_AUTH_TOKEN": "hub-token-placeholder",
                    "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY": "1"
                }
            })
            .to_string(),
        )
        .unwrap();

        let mut spec: InstanceSpec =
            serde_json::from_str(include_str!("../tests/fixtures/instance-spec.json")).unwrap();
        spec.driver = DriverKind::ShellPty;
        spec.cwd = dir.path().to_string_lossy().into_owned();
        let instance = dir.path().join("instance");
        let mut options = ShellPtyOptions::agent(
            dir.path().to_path_buf(),
            AgentKind::Claude,
            AgentLaunch {
                profile: Box::new(crate::profile::ProviderProfile {
                    id: spec.provider_profile.id.clone(),
                    kind: ProviderKind::Anthropic,
                    base_url: "https://gateway.example/v1".into(),
                    delegation: Delegation::Gateway,
                    secret_ref: None,
                    models: vec!["passthrough/ark/seed-evolving".into()],
                    health: crate::profile::ProviderHealth::Healthy,
                }),
                launch_dir: instance.join("launch"),
                native_home: dir.path().join("scoped-native-home"),
                binary: Some(PathBuf::from("/bin/sh")),
                origin: crate::materializer::LaunchOrigin::Human,
                settings_overlay: Some(provider_overlay.clone()),
            },
        );
        options.user_settings_home = Some(user_home.clone());
        options.pin_native_home = true;
        options.hooks = Some(HookConfig {
            instance_dir: instance.clone(),
            relay_binary: PathBuf::from("/nonexistent/remuda"),
            tui: crate::launch::TuiMode::Fullscreen,
        });

        let ctx = promote_ctx(&options, &spec.cwd, Some(&spec)).unwrap();
        let (events, _rx) = mpsc::channel(8);
        let seq = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let interrupt_pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
        let (recipe, hooks, note) = prepare_launch_blocking(
            &options,
            &spec.cwd,
            Some(&spec),
            &ctx,
            &events,
            &seq,
            &interrupt_pid,
        )
        .unwrap();
        let settings_path = recipe
            .argv
            .windows(2)
            .find_map(|pair| (pair[0] == "--settings").then(|| PathBuf::from(&pair[1])))
            .expect("a gateway launch must carry --settings");
        assert_eq!(settings_path, hooks.expect("hooks requested").overlay.path);
        let merged: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();

        // The Hub's provider is what the session runs on.
        assert_eq!(
            merged["env"]["ANTHROPIC_BASE_URL"], "https://gateway.example/v1",
            "the host's gateway must not win"
        );
        assert_eq!(
            merged["env"]["ANTHROPIC_AUTH_TOKEN"],
            "hub-token-placeholder"
        );
        assert_eq!(merged["model"], "passthrough/ark/seed-evolving");
        // No host endpoint or model variable survives anywhere.
        let rendered = merged.to_string();
        assert!(!rendered.contains("host-native.example"), "{rendered}");
        assert!(!rendered.contains("ark/seed-evolving[1m]"), "{rendered}");
        assert!(!rendered.contains("ark/host-"), "{rendered}");
        for name in [
            "ANTHROPIC_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "CLAUDE_CODE_SUBAGENT_MODEL",
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
        ] {
            assert!(
                merged["env"].get(name).is_none(),
                "{name} must be stripped from the merged overlay"
            );
        }
        // The user's own configuration still works.
        assert_eq!(
            merged["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "user-session-start"
        );
        assert_eq!(merged["permissions"]["deny"][0], "Read(./.env)");
        assert_eq!(merged["theme"], "dark");
        assert_eq!(merged["statusLine"]["command"], "echo host");
        assert_eq!(merged["env"]["HOST_ONLY"], "keep-me");

        // The journal line proves which source applied, with no secret in it.
        let payload = note.expect("a native claude launch journals its provider source");
        let remuda_protocol::ObservationPayload::Lifecycle(lifecycle) = payload else {
            panic!("provider source must be a lifecycle payload");
        };
        let remuda_protocol::LifecyclePayload::Native(native) = *lifecycle else {
            panic!("provider source must be a native lifecycle");
        };
        assert_eq!(native.native_name, NATIVE_PROVIDER_SOURCE);
        assert_eq!(native.topic, remuda_protocol::LifecycleTopic::Configuration);
        assert_eq!(
            native.status,
            remuda_protocol::Knowledge::Known {
                value: "overlay".into()
            },
            "the overlay applied, so the line must not say host-native"
        );
        assert_eq!(native.related_ids["delegation"], "gateway");
        assert_eq!(
            native.related_ids["effectiveModel"], "passthrough/ark/seed-evolving",
            "the line must name the model that actually answers"
        );
        assert_eq!(
            native.related_ids["providerProfileId"],
            recipe.provider.profile_id.to_string()
        );
        let journaled = format!("{:?}", native.related_ids);
        assert!(!journaled.contains("token"), "{journaled}");
        assert!(
            !journaled.contains("gateway.example"),
            "a journal is not the 0600 instance file: {journaled}"
        );

        // The user's own settings are read, never rewritten.
        assert!(
            std::fs::read_to_string(user_home.join("settings.json"))
                .unwrap()
                .contains("host-native.example")
        );
    }

    /// native-config-1: an inherited (or explicitly chosen) config dir is read
    /// natively by the CLI, so no copy is seeded and a plain launch with no
    /// other needs carries no `--settings` at all.
    #[tokio::test]
    async fn an_inherited_native_home_does_not_seed_or_copy_settings() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec: InstanceSpec =
            serde_json::from_str(include_str!("../tests/fixtures/instance-spec.json")).unwrap();
        spec.driver = DriverKind::ShellPty;
        spec.cwd = dir.path().to_string_lossy().into_owned();
        let options = ShellPtyOptions::agent(
            dir.path().to_path_buf(),
            AgentKind::Claude,
            AgentLaunch {
                profile: Box::new(crate::profile::ProviderProfile {
                    id: spec.provider_profile.id.clone(),
                    kind: ProviderKind::Anthropic,
                    base_url: String::new(),
                    delegation: Delegation::None,
                    secret_ref: None,
                    models: vec!["sonnet".into()],
                    health: crate::profile::ProviderHealth::Healthy,
                }),
                launch_dir: dir.path().join("instance").join("launch"),
                native_home: dir.path().join("native-home"),
                binary: Some(PathBuf::from("/bin/sh")),
                origin: crate::materializer::LaunchOrigin::Human,
                settings_overlay: None,
            },
        );
        let ctx = promote_ctx(&options, &spec.cwd, Some(&spec)).unwrap();
        let (events, _rx) = mpsc::channel(8);
        let seq = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let interrupt_pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
        let (recipe, hooks, _) = prepare_launch_blocking(
            &options,
            &spec.cwd,
            Some(&spec),
            &ctx,
            &events,
            &seq,
            &interrupt_pid,
        )
        .unwrap();
        assert!(hooks.is_none());
        assert!(
            !recipe.argv.iter().any(|arg| arg == "--settings"),
            "no overlay may shadow the inherited home's own settings: {:?}",
            recipe.argv
        );
    }

    /// it has to work on a live PTY and be honest when there is nothing to
    /// read. A driver that has not started must not answer with an empty grid,
    /// which reads as a blank terminal.
    #[tokio::test]
    async fn a_live_pty_reads_its_screen_and_an_unstarted_one_says_it_has_none() {
        use crate::driver::Driver as _;

        let dir = tempfile::tempdir().unwrap();
        let mut options = ShellPtyOptions::login(dir.path().to_path_buf());
        options.args = vec!["/bin/sh".into(), "-c".into(), "exec sleep 30".into()];
        options.emulator = true;
        let driver = ShellPtyDriver::new(options);
        assert!(
            driver.screen_read().await.unwrap().is_none(),
            "an unstarted driver has no screen, and must not pretend to a blank one"
        );

        let _events = driver.spawn().await.unwrap().into_events();
        let state = driver.state().await.unwrap();
        state
            .emulator
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .feed(b"Quick safety check: Is this a project you created?\r\n");

        let screen = driver
            .screen_read()
            .await
            .unwrap()
            .expect("a started PTY has a screen");
        assert!(
            screen.emulated,
            "the emulator is on, so the grid must not be a raw-ring fallback"
        );
        assert!(
            screen
                .lines
                .iter()
                .any(|line| line.contains("Quick safety check")),
            "the dialog parked on screen is exactly what this read exists to surface: {:?}",
            screen.lines
        );
        assert!(screen.cols > 0 && screen.rows > 0);

        driver.close().await.unwrap();
        assert!(
            driver.screen_read().await.unwrap().is_none(),
            "a closed driver has no screen to read"
        );
    }

    #[tokio::test]
    async fn interruption_snapshot_waits_for_the_complete_screen_update() {
        let dir = tempfile::tempdir().unwrap();
        let mut options = ShellPtyOptions::login(dir.path().to_path_buf());
        options.args = vec!["/bin/sh".into(), "-c".into(), "exec sleep 30".into()];
        options.emulator = true;
        let driver = ShellPtyDriver::new(options);
        let _events = driver.spawn().await.unwrap().into_events();
        let state = driver.state().await.unwrap();
        let marker = "Interrupted · What should Claude do instead?\r\n❯";
        {
            // Pause the reader at the old race: the emulator has the next frame,
            // but that frame's interruption count has not yet been committed.
            let mut output = state.interruption_output.lock().unwrap();
            state
                .emulator
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .feed(marker.as_bytes());
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (snapshot_tx, snapshot_rx) = std::sync::mpsc::channel();
            let sampled = Arc::clone(&state);
            let reader = std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                snapshot_tx.send(sampled.screen_evidence()).unwrap();
            });
            started_rx.recv().unwrap();
            assert!(
                snapshot_rx
                    .recv_timeout(std::time::Duration::from_millis(25))
                    .is_err(),
                "a snapshot cannot observe a partly updated output generation"
            );
            output.feed(marker.as_bytes());
            drop(output);
            let (count, grid) = snapshot_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            reader.join().unwrap();
            assert_eq!(count, 1);
            assert!(grid.flat().contains("Interrupted"));
        }
        Driver::close(&driver).await.unwrap();
    }

    #[tokio::test]
    async fn cancel_uses_escape_only_for_a_promoted_claude_turn() {
        let dir = tempfile::tempdir().unwrap();
        let mut options = ShellPtyOptions::login(dir.path().to_path_buf());
        options.args = vec!["/bin/sh".into(), "-c".into(), "exec sleep 30".into()];
        options.hooks = Some(HookConfig {
            instance_dir: dir.path().join("instance"),
            relay_binary: PathBuf::from("/nonexistent/remuda"),
            tui: crate::launch::TuiMode::Fullscreen,
        });
        let driver = ShellPtyDriver::new(options);
        let mut events = driver.spawn().await.unwrap().into_events();
        let written = Arc::new(std::sync::Mutex::new(Vec::new()));
        *driver.state().await.unwrap().writer.lock().await =
            Some(Box::new(RecordingWriter(Arc::clone(&written))));
        Driver::cancel(&driver).await.unwrap();
        assert_eq!(written.lock().unwrap().as_slice(), b"\x03");
        written.lock().unwrap().clear();
        *driver.promoted.lock().unwrap() = Some(Detected {
            kind: AgentKind::Claude,
            pid: 42,
            session_id: None,
            hydrates_transcript: true,
        });
        let hooks = driver.hooks.lock().await.clone().unwrap();
        assert!(
            !driver.hook_receipt(),
            "configuration alone is not a receipt"
        );
        for event in ["SessionStart", "UserPromptSubmit"] {
            remuda_signal::send_event(
                &hooks.socket_path,
                &remuda_signal::HookEnvelope {
                    credential: hooks.child_env("")["REMUDA_HOOK_CREDENTIAL"].clone(),
                    event: event.into(),
                    ppid: 42,
                    payload: serde_json::json!({"session_id": "cancel-fixture"}),
                },
                std::time::Duration::from_secs(1),
            )
            .await;
            tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap();
        }
        assert!(driver.hook_receipt());
        // The hook has already accepted a turn while the screen still shows
        // the old composer. That stale idle frame must not swallow cancel.
        *driver.status.lock().unwrap() = Some(ScreenStatus::Idle);
        assert!(
            Driver::wait_control(&driver).await.is_err(),
            "a SessionStart receipt cannot admit a prompt during a hook-confirmed turn"
        );
        Driver::cancel(&driver).await.unwrap();
        assert_eq!(written.lock().unwrap().as_slice(), b"\x1b");
        assert_eq!(driver.interrupt_pid.load(Ordering::SeqCst), 42);
        assert!(
            events.try_recv().is_err(),
            "writing a key alone must not manufacture an interrupted observation"
        );
        written.lock().unwrap().clear();
        hooks.confirm_screen_interrupt(42);
        *driver.status.lock().unwrap() = Some(ScreenStatus::Working);
        assert!(Driver::wait_control(&driver).await.is_ok());
        let ack = Driver::cancel(&driver).await.unwrap();
        assert_eq!(ack.dispatch, remuda_protocol::DispatchState::NotDispatched);
        assert!(
            written.lock().unwrap().is_empty(),
            "Esc on an idle Claude can quit the session"
        );
        *driver.status.lock().unwrap() = Some(ScreenStatus::Blocked);
        assert!(Driver::wait_control(&driver).await.is_err());
        Driver::close(&driver).await.unwrap();
    }

    #[test]
    fn a_plain_shell_prompt_is_one_write_of_text_plus_a_carriage_return() {
        let delivery = send::encode("ls -la", false, None);
        assert_eq!(delivery.body, b"ls -la\r");
        assert_eq!(delivery.submit, None);
        assert_eq!(send::encode("ls\n", false, None).body, b"ls\n");
    }

    #[test]
    fn a_promoted_prompt_sends_its_enter_as_a_separate_write() {
        let delivery = send::encode("hello\r\n", true, None);
        assert_eq!(delivery.body, b"hello");
        assert_eq!(delivery.submit, Some(b"\r".to_vec()));
    }

    #[test]
    fn a_promoted_multiline_prompt_is_bracketed_when_the_tui_requested_paste() {
        let delivery = send::encode(
            "first\nsecond",
            true,
            Some(ModeSet {
                bracketed_paste: true,
                ..ModeSet::default()
            }),
        );
        assert_eq!(delivery.body, b"\x1b[200~first\nsecond\x1b[201~");
        assert_eq!(delivery.submit, Some(b"\r".to_vec()));
    }

    #[tokio::test]
    async fn a_launched_agent_is_an_agent_before_the_first_promotion_poll() {
        // Measured live (native-pty-2 §5): `cancel` read only the promotion
        // poller, so in the ~1s before the first poll an `instance.cancel` on a
        // Remuda-launched Claude fell through to the shell branch and sent
        // `\x03` — which does not interrupt a Claude turn, it quits the
        // session. The launch target knows the kind immediately; the poller
        // only confirms it.
        let mut driver = agent_pty(None).await;
        driver.options.target = Target::Agent {
            kind: AgentKind::Claude,
            resume: None,
        };
        assert_eq!(
            driver.session_kind(),
            Some(AgentKind::Claude),
            "the kind is known from the launch, with no poll yet"
        );
        assert_eq!(
            driver.promoted_kind(),
            None,
            "and the poller has indeed not run"
        );
        let written = Arc::new(std::sync::Mutex::new(Vec::new()));
        let state = driver.state().await.unwrap();
        *state.writer.lock().await = Some(Box::new(RecordingWriter(Arc::clone(&written))));
        Driver::cancel(&driver).await.unwrap();
        assert_eq!(written.lock().unwrap().as_slice(), b"\x1b");
        assert_eq!(
            driver.interrupt_pid.load(Ordering::SeqCst),
            state.pgid.load(Ordering::SeqCst),
            "native evidence follows the direct child before promotion"
        );
        Driver::close(&driver).await.unwrap();
    }

    /// A driver whose PTY runs an inert process, carrying `kind` as its
    /// session.
    ///
    /// Promoted rather than launched: §1.0 makes the two the same session to
    /// everything below the spawn, and the promoted shape needs no materializer
    /// inputs. What is being tested is the gate, which reads `session_kind` —
    /// satisfied by either.
    async fn agent_pty(kind: Option<AgentKind>) -> ShellPtyDriver {
        let dir = tempfile::tempdir().unwrap();
        let mut options = ShellPtyOptions::login(dir.path().to_path_buf());
        options.args = vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()];
        let driver = ShellPtyDriver::new(options);
        driver.spawn().await.expect("spawns");
        if let Some(kind) = kind {
            *driver.promoted.lock().unwrap() = Some(crate::promote::Detected {
                kind,
                pid: 1,
                session_id: None,
                hydrates_transcript: true,
            });
        }
        driver
    }

    #[tokio::test]
    async fn a_booting_agent_is_not_ready_for_a_prompt() {
        // The demo defect: `wait_control` gated on `promoted_kind`, which is
        // None until the first poll, so a Remuda-launched claude took the
        // plain-shell branch and the create-time prompt was typed into the
        // login shell — which ran it as a command. An agent is an agent from
        // the launch target, and nothing has said it is ready yet.
        let driver = agent_pty(Some(AgentKind::Claude)).await;
        assert!(
            Driver::wait_control(&driver).await.is_err(),
            "no hook receipt, no emulator, no idle signature: queue it"
        );
        let _ = Driver::close(&driver).await;
    }

    #[tokio::test]
    async fn an_agent_whose_screen_says_idle_is_ready() {
        let driver = agent_pty(Some(AgentKind::Claude)).await;
        *driver.status.lock().unwrap() = Some(ScreenStatus::Idle);
        assert!(Driver::wait_control(&driver).await.is_ok());
        let _ = Driver::close(&driver).await;
    }

    #[tokio::test]
    async fn an_agent_mid_turn_holds_the_prompt() {
        // Typing into a running turn is how a prompt gets swallowed.
        let driver = agent_pty(Some(AgentKind::Claude)).await;
        for status in [ScreenStatus::Working, ScreenStatus::Blocked] {
            *driver.status.lock().unwrap() = Some(status);
            assert!(
                Driver::wait_control(&driver).await.is_err(),
                "{status:?} must not accept a prompt"
            );
        }
        let _ = Driver::close(&driver).await;
    }

    #[tokio::test]
    async fn a_plain_shell_is_always_ready() {
        // A shell reads a line; one typed early just waits in the tty buffer.
        // Gating it would break every `terminal` instance.
        let driver = agent_pty(None).await;
        assert!(Driver::wait_control(&driver).await.is_ok());
        let _ = Driver::close(&driver).await;
    }

    #[tokio::test]
    async fn cancelling_an_idle_composer_sends_nothing() {
        // Measured live against claude 2.1.221 (native-pty-2 §5): `Esc` on an
        // idle composer *quits*. The evidence that `Esc` interrupts is all from
        // a running turn. A cancel with no turn to cancel must therefore be a
        // no-op, not a keystroke that ends the session.
        let dir = tempfile::tempdir().unwrap();
        let mut options = ShellPtyOptions::login(dir.path().to_path_buf());
        options.args = vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()];
        let driver = ShellPtyDriver::new(options);
        driver.spawn().await.expect("spawns");
        // Promoted rather than launched, because the two are the same session
        // to every line of code below this point (§1.0) and the promoted shape
        // needs no materializer inputs.
        *driver.promoted.lock().unwrap() = Some(crate::promote::Detected {
            kind: AgentKind::Claude,
            pid: 1,
            session_id: None,
            hydrates_transcript: true,
        });
        *driver.status.lock().unwrap() = Some(ScreenStatus::Idle);

        let ack = Driver::cancel(&driver)
            .await
            .expect("cancel is not an error");
        assert_eq!(
            ack.dispatch,
            remuda_protocol::DispatchState::NotDispatched,
            "nothing was sent, and the ack says so rather than claiming a write"
        );
        let _ = Driver::close(&driver).await;
    }

    #[tokio::test]
    async fn a_plain_shell_still_has_no_session_kind() {
        let dir = tempfile::tempdir().unwrap();
        let driver = ShellPtyDriver::new(ShellPtyOptions::login(dir.path().to_path_buf()));
        assert_eq!(
            driver.session_kind(),
            None,
            "a login shell is not an agent, so cancel stays Ctrl+C"
        );
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
