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
    /// Recipe inputs for [`Target::Agent`]. Required for that target and
    /// ignored for a shell.
    ///
    /// Boxed: it holds a `ProviderProfile`, and `ShellPtyOptions` is cloned
    /// into every driver.
    pub agent: Option<Box<AgentLaunch>>,
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
            emulator: emulator_enabled(),
            target: Target::Shell,
            agent: None,
        }
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
    writer: Mutex<Box<dyn Write + Send>>,
    /// The PTY master. `None` once the stop ladder has dropped it.
    ///
    /// §5.3 step 2 pairs `SIGHUP` with closing the master, and that has to be
    /// a real close: while any fd to the master is open, the slave's other end
    /// stays open too, so a shell blocked on input never sees the hangup and
    /// the ladder escalates to `SIGKILL` for no reason. Dropping the box is the
    /// close — `portable-pty` releases the fd in `Drop`.
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
    /// Event sender for the current run, retained so an effort switch can
    /// journal `queued`/`degraded` without going through the worker.
    events_tx: Mutex<Option<mpsc::Sender<remuda_protocol::Observation>>>,
    /// Identity the current run's effort lifecycle observations are stamped
    /// with; `None` before `start`.
    promote_ctx: Mutex<Option<promotion::PromoteCtx>>,
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
            hooks: Mutex::new(None),
            adapters: Mutex::new(None),
            recipe: std::sync::Mutex::new(None),
            exited: Arc::new(std::sync::Mutex::new(None)),
            effort_bridge: Mutex::new(Arc::new(crate::effort::EffortBridge::new())),
            effort_queue: Mutex::new(Arc::new(crate::effort::EffortQueue::new())),
            effort_worker: Mutex::new(None),
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
        base_settings: Option<serde_json::Value>,
    ) -> DriverResult<Option<Arc<crate::launch::HookSession>>> {
        let Some(config) = self.options.hooks.clone() else {
            return Ok(None);
        };
        let bus = Arc::new(
            remuda_signal::SignalBus::new(
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
            )
            .with_interrupt_tracker(Arc::clone(&self.interrupt_pid)),
        );
        Ok(Some(Arc::new(crate::launch::HookSession::start(
            &crate::launch::HookSessionOptions {
                instance_dir: config.instance_dir,
                relay_binary: config.relay_binary,
                tui: config.tui,
                base_settings,
                // The launch target is authoritative before the first promote
                // tick; a promoted hand-typed session defaults to Claude for
                // the overlay (its shim is a pass-through until promotion
                // rewires it), matching §1.0's one-path rule.
                kind: self
                    .options
                    .target
                    .agent_kind()
                    .unwrap_or(AgentKind::Claude),
            },
            bus,
        )?)))
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
        let (recipe, hooks) = self.prepare_launch(cwd, spec, &hook_ctx, &tx)?;
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
            writer: Mutex::new(writer),
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
                Arc::clone(&self.interrupt_pid),
                Arc::clone(&self.interrupt_screen_markers),
                tx.clone(),
                Arc::clone(&self.seq),
                Some(Arc::clone(&effort_bridge)),
                spec.and_then(|spec| spec.effort),
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

    /// The launch recipe for this PTY's target.    ///
    /// A shell keeps the lightweight recipe it always had; an agent goes
    /// through the materializer so §5.1 step 4's audit fields are real.
    fn recipe_for(
        &self,
        cwd: &str,
        spec: Option<&InstanceSpec>,
        settings_overlay: Option<&Path>,
    ) -> DriverResult<LaunchRecipe> {
        let Target::Agent { kind, resume } = &self.options.target else {
            return shell_recipe(&self.options, cwd);
        };
        let Some(agent) = self.options.agent.as_ref() else {
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

    /// Validate the caller's native recipe first, then layer the hook overlay
    /// into that same materializer so its argv and audit describe one file.
    fn prepare_launch(
        &self,
        cwd: &str,
        spec: Option<&InstanceSpec>,
        ctx: &promotion::PromoteCtx,
        events: &mpsc::Sender<remuda_protocol::Observation>,
    ) -> DriverResult<(LaunchRecipe, Option<Arc<crate::launch::HookSession>>)> {
        let mut recipe = self.recipe_for(cwd, spec, None)?;
        let native_claude = self.options.target.agent_kind() == Some(AgentKind::Claude);
        let base_settings = if native_claude && self.options.hooks.is_some() {
            recipe
                .materialized_files
                .iter()
                .find(|file| file.role == crate::recipe::FileRole::Settings)
                .map(|file| {
                    Ok::<_, DriverError>(serde_json::from_slice(&std::fs::read(&file.path)?)?)
                })
                .transpose()?
        } else {
            None
        };
        let hooks = self.start_hooks(ctx, events, base_settings)?;
        if native_claude && let Some(hooks) = &hooks {
            recipe = self.recipe_for(cwd, spec, Some(&hooks.overlay.path))?;
        }
        Ok((recipe, hooks))
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
        // The box is taken out here, before the ladder runs, and moved into the
        // closure — so the close is a plain `drop` of an owned value at exactly
        // the rung that needs it, with no lock acquired from inside a
        // synchronous callback on a runtime thread.
        let mut master = state.master.lock().await.take();
        state.closed.store(true, Ordering::SeqCst);
        lifecycle::stop_group(pgid, move || {
            drop(master.take());
        })
        .await
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
        {
            return self.switch_effort(level).await;
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

    async fn respond_interaction(
        &self,
        id: remuda_protocol::InteractionId,
        answer: remuda_protocol::InteractionAnswer,
    ) -> DriverResult<DriverAck> {
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
    for (key, value) in agent_env(&options.target, recipe) {
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

/// Literal env the recipe asks for: `CODEX_HOME` / `GROK_HOME` shadow dirs and
/// provider overlay values (§5.1).
///
/// Only [`EnvAllowlistSource::NativeHome`] and
/// [`EnvAllowlistSource::ProviderOverlay`] entries carry a value inline.
/// `Credential` entries name a secret the broker resolves and must never be
/// read from here, and `HostEnv` entries are inherited, not set.
fn agent_env(target: &Target, recipe: &LaunchRecipe) -> Vec<(String, String)> {
    if target.agent_kind().is_none() {
        return Vec::new();
    }
    recipe
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
        .collect()
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
        let (recipe, hooks) = driver
            .prepare_launch(&spec.cwd, Some(&spec), &ctx, &events)
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
            driver
                .prepare_launch(&spec.cwd, Some(&spec), &ctx, &events)
                .is_err()
        );
        assert!(
            !invalid_instance.exists(),
            "validate before creating hook artifacts"
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
            Box::new(RecordingWriter(Arc::clone(&written)));
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
        *state.writer.lock().await = Box::new(RecordingWriter(Arc::clone(&written)));
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
