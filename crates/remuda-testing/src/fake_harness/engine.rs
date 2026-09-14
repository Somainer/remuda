//! The fake-harness event loop: PTY input, scripted turns, hooks, artifacts.
//!
//! One [`run`] call owns the whole process lifetime: it puts stdin into raw
//! mode (when attached to a TTY), spawns the byte reader thread, and drives
//! the per-dialect turn machine. Timing is real — streamed chunks and tool
//! durations actually sleep so a PTY driver observes genuine working windows —
//! but every artifact timestamp comes from [`FakeClock`].
//!
//! A side-channel JSONL debug log (`--events-out`) records semantic events
//! (`submit`, `enqueue`, `boundary_deliver`, `interrupt`, …); PTY tests assert
//! on it instead of racing screen repaints.

use std::collections::{BTreeMap, VecDeque};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::fake_harness::artifacts::{
    ArtifactKind, ArtifactPaths, ArtifactSet, CodexCounters, CommandOutcome, GrokTurnIds,
    SessionMeta, claude_assistant_block, claude_queue_op, claude_queued_attachment,
    claude_tool_result, claude_user_record, codex_assistant_message, codex_command_item_completed,
    codex_function_call, codex_function_output, codex_session_index, codex_session_meta,
    codex_task_complete, codex_task_started, codex_token_usage, codex_turn_aborted,
    codex_user_item_completed, codex_user_message, grok_chunk_update, grok_tool_call,
    grok_tool_update_completed, grok_tool_update_failed, grok_turn_completed, grok_write_registry,
};
use crate::fake_harness::clock::FakeClock;
use crate::fake_harness::hooks::{
    HookContext, HookEvent, HookKind, HookOutcome, HookTable, decision_behavior,
};
use crate::fake_harness::input::Input;
use crate::fake_harness::screen::{
    ApprovalView, Dialect, DialectVersion, ScreenMode, View, WorkingPhase, default_choices, enter,
    osc_transitions, repaint, teardown,
};
use crate::fake_harness::script::{ApprovalMode, Scenario, TurnSpec, UsageSpec};

/// Failures surfaced by the binary.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// Filesystem / terminal I/O failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Bad options or scenario.
    #[error("{0}")]
    Args(String),
}

/// Command-line options (parsed by the binary).
#[derive(Clone, Debug)]
pub struct Options {
    /// Screen/artifact dialect.
    pub kind: Dialect,
    /// Scenario file; the built-in default scenario when `None`.
    pub script_path: Option<PathBuf>,
    /// Claude `--settings` overlay (hooks).
    pub settings: Option<PathBuf>,
    /// Harness home (CLAUDE_CONFIG_DIR / CODEX_HOME / GROK_HOME).
    pub home: Option<PathBuf>,
    /// Working directory (defaults to the real cwd).
    pub cwd: Option<PathBuf>,
    /// Explicit session id.
    pub session_id: Option<String>,
    /// Resume an existing artifact set.
    pub resume: Option<String>,
    /// Model label.
    pub model: Option<String>,
    /// Force the first-run trust-directory dialog.
    pub trust_dialog: bool,
    /// Skip alt-screen entry (mirrors `--no-alt-screen`).
    pub no_alt_screen: bool,
    /// Fallback width when the terminal reports no size.
    pub cols: u16,
    /// Fallback height when the terminal reports no size.
    pub rows: u16,
    /// Pin the deterministic clock epoch (tests).
    pub epoch_ms: Option<i64>,
    /// Append semantic debug events to this JSONL file.
    pub events_path: Option<PathBuf>,
    /// Screen dialect version (`modern` = claude 2.1.270, no `esc to
    /// interrupt`, live OSC edges with an empty percent).
    pub dialect_version: DialectVersion,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            kind: Dialect::Claude,
            script_path: None,
            settings: None,
            home: None,
            cwd: None,
            session_id: None,
            resume: None,
            model: None,
            trust_dialog: false,
            no_alt_screen: false,
            cols: 80,
            rows: 24,
            epoch_ms: None,
            events_path: None,
            dialect_version: DialectVersion::Legacy,
        }
    }
}

/// Default session id when `--session-id` is not supplied.
pub const DEFAULT_SESSION_ID: &str = "00000000-0000-4000-8000-000000000001";

const TICK: Duration = Duration::from_millis(10);
const LONG_WAIT: Duration = Duration::from_secs(3600);

// ---------------------------------------------------------------------------
// Turn machine state
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq)]
enum Step {
    /// About to emit turn-start records.
    Start,
    /// Streaming thinking, `left` chunks remain; next chunk at `ready`.
    Think { left: usize, ready: Instant },
    /// Emit the tool call and resolve approval.
    ToolCall { idx: usize },
    /// Tool is "running" until `ends`; may be interrupted.
    ToolRun { idx: usize, ends: Instant },
    /// Emit tool result and the boundary logic.
    ToolFinish { idx: usize, denied: Option<String> },
    /// Streaming final text, `left` chunks remain.
    Text {
        left: usize,
        total: usize,
        ready: Instant,
    },
    /// Close the turn.
    Finish,
}

#[derive(Debug)]
struct TurnRun {
    prompt: String,
    spec: TurnSpec,
    started: Instant,
    step: Step,
    codex_turn_id: Option<String>,
    grok_ids: GrokTurnIds,
    claude_seq: u64,
    tool_ids: Vec<String>,
    streamed_text: String,
}

/// One prompt sitting in claude's native queue.
#[derive(Clone, Debug)]
struct QueuedPrompt {
    text: String,
    enqueued_at: String,
}

struct Engine {
    dialect: Dialect,
    scenario: Scenario,
    clock: FakeClock,
    hooks: HookTable,
    artifacts: ArtifactSet,
    paths: ArtifactPaths,
    home: PathBuf,
    meta: SessionMeta,
    view: View,
    codex_counters: CodexCounters,
    draft: String,
    claude_queue: Vec<QueuedPrompt>,
    codex_queue: VecDeque<String>,
    grok_queue: VecDeque<String>,
    consumed: BTreeMap<usize, ()>,
    turn: Option<TurnRun>,
    turn_count: u32,
    grok_turn_number: u64,
    pending_redirect: Option<String>,
    trust_selected: bool,
    approval_tool_idx: Option<usize>,
    pre_tool_decision: Option<String>,
    last_ctrl_c: Option<Instant>,
    ctrl_q_count: u32,
    last_ctrl_q: Option<Instant>,
    stopping: Arc<AtomicBool>,
    events_file: Option<std::fs::File>,
    should_exit: bool,
    dirty: bool,
    last_second: u64,
    /// Last OSC 0 title emitted, so live edges only resend changes.
    osc_title: String,
    /// Last raw OSC 9;4 payload emitted (`"3;"` / `"0;"` in modern).
    osc_progress: String,
}

impl Engine {
    /// Write one full repaint, preceded by any OSC edges the modern dialect
    /// owes the observer since the last paint. The legacy dialect writes OSC
    /// only at [`enter`], so this prepends nothing for it.
    fn paint(&mut self, cols: u16, rows: u16) {
        if self.dialect == Dialect::Claude && self.view.dialect_version.is_modern() {
            let (edges, title, progress) =
                osc_transitions(&self.view, &self.osc_title, &self.osc_progress);
            if let Some(title) = title {
                self.osc_title = title;
            }
            if let Some(progress) = progress {
                self.osc_progress = progress;
            }
            if !edges.is_empty() {
                let _ = write_stdout(&edges);
            }
        }
        let _ = write_stdout(&repaint(&self.view, cols, rows));
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the fake harness until exit. All terminal setup happens in here.
pub fn run(opts: Options) -> Result<i32, RunError> {
    let stopping = Arc::new(AtomicBool::new(false));
    install_signal_handler(stopping.clone());
    // Die with the spawner (aborted gate / test timeout / ctrl-c of the
    // driver). The flag gives this loop a chance to tear down the terminal
    // gracefully; the watcher hard-exits if that ever wedges.
    let _parent_watch = crate::parent_watch::install_with_flag(Some(stopping.clone()));

    let dialect = opts.kind;
    if opts.dialect_version.is_modern() && dialect != Dialect::Claude {
        return Err(RunError::Args(
            "--dialect-version modern is only defined for --kind claude".into(),
        ));
    }
    let scenario = match &opts.script_path {
        Some(path) => Scenario::load(path).map_err(RunError::Args)?,
        None => default_scenario(),
    };
    let cwd = opts
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let home = opts
        .home
        .clone()
        .or_else(|| default_home(dialect))
        .ok_or_else(|| RunError::Args("could not resolve a harness home".into()))?;
    std::fs::create_dir_all(&home)?;
    let session_id = opts
        .session_id
        .clone()
        .or_else(|| opts.resume.clone())
        .unwrap_or_else(|| DEFAULT_SESSION_ID.to_owned());
    let model = opts
        .model
        .clone()
        .unwrap_or_else(|| default_model(dialect).to_owned());
    let meta = SessionMeta {
        session_id: session_id.clone(),
        cwd: cwd.clone(),
        model: model.clone(),
        version: dialect.version().to_owned(),
    };
    let clock = FakeClock::new(
        opts.epoch_ms
            .unwrap_or(crate::fake_harness::clock::DEFAULT_EPOCH_MS),
    );

    let hook_kind = match dialect {
        Dialect::Claude => HookKind::Claude,
        Dialect::Codex => HookKind::Codex,
        Dialect::Grok => HookKind::Grok,
    };
    let hooks = HookTable::load(hook_kind, opts.settings.as_deref(), Some(&home))
        .map_err(RunError::Args)?;

    let artifact_kind = match dialect {
        Dialect::Claude => ArtifactKind::Claude,
        Dialect::Codex => ArtifactKind::Codex,
        Dialect::Grok => ArtifactKind::Grok,
    };
    let resuming = opts.resume.is_some();
    let (artifacts, paths) = match &opts.resume {
        Some(resume_id) => open_resume(artifact_kind, &home, resume_id, meta.clone())?,
        None => ArtifactSet::create(artifact_kind, &home, meta.clone(), &clock)?,
    };

    let events_file = match &opts.events_path {
        Some(path) => Some(OpenOptions::new().create(true).append(true).open(path)?),
        None => None,
    };

    let dir_name = cwd
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "work".into());
    let mut view = View::new(dialect, model.clone(), dir_name);
    view.alt_screen = !opts.no_alt_screen;
    view.dialect_version = opts.dialect_version;
    if dialect == Dialect::Codex && !resuming {
        view.transcript
            .push(format!(">_ OpenAI Codex (v{})", dialect.version()));
        view.transcript.push(format!("model: {model} low"));
    }
    // `enter()` emits these before the first paint; seed the last-sent values
    // so the first live edge compares against them.
    let initial_title = view.title();
    let initial_progress = view.osc_progress_payload();

    let mut engine = Engine {
        dialect,
        scenario,
        clock,
        hooks,
        artifacts,
        paths,
        home,
        meta,
        view,
        codex_counters: CodexCounters::new(),
        draft: String::new(),
        claude_queue: Vec::new(),
        codex_queue: VecDeque::new(),
        grok_queue: VecDeque::new(),
        consumed: BTreeMap::new(),
        turn: None,
        turn_count: 0,
        grok_turn_number: 0,
        pending_redirect: None,
        trust_selected: false,
        approval_tool_idx: None,
        pre_tool_decision: None,
        last_ctrl_c: None,
        ctrl_q_count: 0,
        last_ctrl_q: None,
        stopping,
        events_file,
        should_exit: false,
        dirty: true,
        last_second: 0,
        osc_title: initial_title,
        osc_progress: initial_progress,
    };

    if resuming {
        engine.grok_turn_number = engine.count_grok_turns();
        if dialect == Dialect::Grok {
            grok_write_registry(
                &engine.home,
                &engine.meta,
                std::process::id(),
                &engine.clock,
            )?;
        }
    } else {
        engine.write_session_header()?;
        if dialect == Dialect::Grok {
            grok_write_registry(
                &engine.home,
                &engine.meta,
                std::process::id(),
                &engine.clock,
            )?;
        }
    }
    let mut session_start = json!({ "source": if resuming { "resume" } else { "new" } });
    if dialect == Dialect::Claude {
        session_start["transcript_path"] = json!(engine.paths.main.to_string_lossy());
    }
    engine.fire_hook(HookEvent::SessionStart, session_start);
    if opts.trust_dialog {
        engine.view.mode = ScreenMode::Trust;
    }

    let (cols, rows) = terminal_size().unwrap_or((opts.cols, opts.rows));
    let _ = write_stdout(&enter(&engine.view));
    let restore = set_raw_mode();
    let rx = spawn_reader(engine.stopping.clone());

    let result = engine.event_loop(&rx, cols, rows);

    drop(restore);
    engine.shutdown_artifacts()?;
    engine.fire_hook(HookEvent::SessionEnd, json!({ "reason": "shutdown" }));
    let _ = write_stdout(&teardown(&engine.view));
    result
}

fn default_home(dialect: Dialect) -> Option<PathBuf> {
    let (env, fallback) = match dialect {
        Dialect::Claude => ("CLAUDE_CONFIG_DIR", ".claude"),
        Dialect::Codex => ("CODEX_HOME", ".codex"),
        Dialect::Grok => ("GROK_HOME", ".grok"),
    };
    if let Ok(path) = std::env::var(env)
        && !path.is_empty()
    {
        return Some(PathBuf::from(path));
    }
    Some(PathBuf::from(std::env::var_os("HOME")?).join(fallback))
}

#[must_use]
fn default_model(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::Claude => "claude-opus-5",
        Dialect::Codex => "gpt-5.4",
        Dialect::Grok => "Local spike",
    }
}

fn open_resume(
    kind: ArtifactKind,
    home: &Path,
    resume_id: &str,
    meta: SessionMeta,
) -> Result<(ArtifactSet, ArtifactPaths), RunError> {
    match kind {
        ArtifactKind::Claude => {
            let dir =
                home.join("projects")
                    .join(remuda_driver::claude_transcript::encode_project_dir(
                        &meta.cwd,
                    ));
            let path = dir.join(format!("{resume_id}.jsonl"));
            if !path.is_file() {
                return Err(RunError::Args(format!(
                    "resume: transcript not found: {}",
                    path.display()
                )));
            }
            let file = std::fs::OpenOptions::new().append(true).open(&path)?;
            Ok((
                ArtifactSet::from_existing(kind, file, None, meta, None)?,
                ArtifactPaths {
                    main: path,
                    events: None,
                    grok_dir: None,
                    session_index: None,
                },
            ))
        }
        ArtifactKind::Codex => {
            let path = remuda_driver::codex_rollout::locate_rollout_in(home, resume_id)
                .ok_or_else(|| {
                    RunError::Args(format!("resume: no codex rollout for {resume_id}"))
                })?;
            let file = std::fs::OpenOptions::new().append(true).open(&path)?;
            Ok((
                ArtifactSet::from_existing(kind, file, None, meta, None)?,
                ArtifactPaths {
                    main: path,
                    events: None,
                    grok_dir: None,
                    session_index: Some(home.join("session_index.jsonl")),
                },
            ))
        }
        ArtifactKind::Grok => {
            let dir = home
                .join("sessions")
                .join(remuda_driver::grok_session::encode_session_cwd(&meta.cwd))
                .join(resume_id);
            let updates = dir.join("updates.jsonl");
            let events = dir.join("events.jsonl");
            if !updates.is_file() {
                return Err(RunError::Args(format!(
                    "resume: grok session not found: {}",
                    dir.display()
                )));
            }
            Ok((
                ArtifactSet::from_existing(
                    kind,
                    std::fs::OpenOptions::new().append(true).open(&updates)?,
                    Some(std::fs::OpenOptions::new().append(true).open(&events)?),
                    meta,
                    Some(dir.clone()),
                )?,
                ArtifactPaths {
                    main: updates,
                    events: Some(events),
                    grok_dir: Some(dir),
                    session_index: None,
                },
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Terminal plumbing
// ---------------------------------------------------------------------------

#[must_use]
fn terminal_size() -> Option<(u16, u16)> {
    terminal_size::terminal_size().map(|(w, h)| (w.0, h.0))
}

#[cfg(unix)]
struct RawGuard {
    original: nix::sys::termios::Termios,
}

#[cfg(unix)]
impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = nix::sys::termios::tcsetattr(
            std::io::stdin(),
            nix::sys::termios::SetArg::TCSANOW,
            &self.original,
        );
    }
}

#[cfg(unix)]
fn set_raw_mode() -> Option<RawGuard> {
    use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
    let stdin = std::io::stdin();
    let Ok(original) = tcgetattr(&stdin) else {
        return None;
    };
    let mut raw = original.clone();
    raw.local_flags.remove(LocalFlags::ICANON);
    raw.local_flags.remove(LocalFlags::ECHO);
    raw.local_flags.remove(LocalFlags::ISIG);
    tcsetattr(&stdin, SetArg::TCSANOW, &raw).ok()?;
    Some(RawGuard { original })
}

#[cfg(not(unix))]
fn set_raw_mode() -> Option<()> {
    None
}

fn install_signal_handler(stopping: Arc<AtomicBool>) {
    #[cfg(unix)]
    {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
        let _ = signal_hook::flag::register(SIGTERM, stopping.clone());
        let _ = signal_hook::flag::register(SIGHUP, stopping.clone());
        let _ = signal_hook::flag::register(SIGINT, stopping);
    }
    #[cfg(not(unix))]
    let _ = stopping;
}

#[cfg(unix)]
fn spawn_reader(stopping: Arc<AtomicBool>) -> Receiver<Vec<Input>> {
    use std::os::fd::{AsRawFd, RawFd};
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        use nix::unistd::read;
        let fd: RawFd = std::io::stdin().as_raw_fd();
        let mut parser = crate::fake_harness::input::Parser::new();
        let mut buf = [0u8; 256];
        loop {
            if stopping.load(Ordering::Relaxed) {
                break;
            }
            match read(fd, &mut buf) {
                Ok(0) => {
                    let _ = tx.send(Vec::new());
                    break;
                }
                Ok(n) => {
                    let inputs = parser.push(&buf[..n]);
                    if !inputs.is_empty() && tx.send(inputs).is_err() {
                        break;
                    }
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(_) => {
                    let _ = tx.send(Vec::new());
                    break;
                }
            }
        }
    });
    rx
}

#[cfg(not(unix))]
fn spawn_reader(stopping: Arc<AtomicBool>) -> Receiver<Vec<Input>> {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        use std::io::Read;
        let mut parser = crate::fake_harness::input::Parser::new();
        let mut buf = [0u8; 256];
        while let Ok(n) = std::io::stdin().read(&mut buf) {
            if n == 0 || stopping.load(Ordering::Relaxed) {
                let _ = tx.send(Vec::new());
                break;
            }
            let inputs = parser.push(&buf[..n]);
            if !inputs.is_empty() && tx.send(inputs).is_err() {
                break;
            }
        }
    });
    rx
}

fn write_stdout(bytes: &str) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(bytes.as_bytes())?;
    out.flush()
}

// ---------------------------------------------------------------------------
// Event log
// ---------------------------------------------------------------------------

impl Engine {
    fn event(&mut self, name: &str, extra: Value) {
        if let Some(file) = self.events_file.as_mut() {
            let mut record = json!({ "t_ms": self.clock.ms(), "event": name });
            if let (Some(obj), Some(extra)) = (record.as_object_mut(), extra.as_object()) {
                for (key, value) in extra {
                    obj.insert(key.clone(), value.clone());
                }
            }
            let _ = writeln!(file, "{}", record);
            let _ = file.flush();
        }
    }
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

impl Engine {
    fn event_loop(
        &mut self,
        rx: &Receiver<Vec<Input>>,
        cols: u16,
        rows: u16,
    ) -> Result<i32, RunError> {
        loop {
            let deadline = self.pump()?;
            // Elapsed-second repaint while working.
            if let Some(turn) = &self.turn {
                let secs = turn.started.elapsed().as_secs();
                if secs != self.last_second {
                    self.last_second = secs;
                    self.view.elapsed_secs = secs;
                    self.dirty = true;
                }
            }
            if self.dirty {
                let (w, h) = terminal_size().unwrap_or((cols, rows));
                self.paint(w, h);
                self.dirty = false;
            }
            if self.should_exit || self.stopping.load(Ordering::Relaxed) {
                break;
            }
            let timeout = deadline
                .saturating_duration_since(Instant::now())
                .min(TICK * 20)
                .max(Duration::from_millis(2));
            match rx.recv_timeout(timeout) {
                Ok(inputs) if inputs.is_empty() => break, // EOF
                Ok(inputs) => {
                    for input in inputs {
                        self.handle_input(input);
                        if self.should_exit {
                            break;
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        let (w, h) = terminal_size().unwrap_or((cols, rows));
        self.paint(w, h);
        Ok(0)
    }

    /// Run all due machine transitions. Returns the [`Instant`] at which the
    /// machine next needs attention.
    fn pump(&mut self) -> Result<Instant, RunError> {
        if self.should_exit || matches!(self.view.mode, ScreenMode::Trust | ScreenMode::Approval) {
            return Ok(Instant::now() + LONG_WAIT);
        }
        if self.turn.is_none() {
            return Ok(Instant::now() + LONG_WAIT);
        }
        let step = self.turn.as_ref().unwrap().step.clone();
        match step {
            Step::Start => {
                self.begin_turn_records()?;
                let spec = self.turn.as_ref().unwrap().spec.clone();
                let next = if spec.thinking.is_some() {
                    Step::Think {
                        left: spec.think_chunk_count(),
                        ready: Instant::now(),
                    }
                } else if !spec.tools.is_empty() {
                    Step::ToolCall { idx: 0 }
                } else {
                    let total = spec.text_chunk_count();
                    Step::Text {
                        left: total,
                        total,
                        ready: Instant::now(),
                    }
                };
                self.turn.as_mut().unwrap().step = next;
            }
            Step::Think { left, ready } => {
                if Instant::now() < ready {
                    return Ok(ready);
                }
                self.emit_thinking_chunk()?;
                let delay = self.turn.as_ref().unwrap().spec.delay_ms();
                self.turn.as_mut().unwrap().step = if left <= 1 {
                    if !self.turn.as_ref().unwrap().spec.tools.is_empty() {
                        Step::ToolCall { idx: 0 }
                    } else {
                        let total = self.turn.as_ref().unwrap().spec.text_chunk_count();
                        Step::Text {
                            left: total,
                            total,
                            ready: Instant::now(),
                        }
                    }
                } else {
                    Step::Think {
                        left: left - 1,
                        ready: Instant::now() + Duration::from_millis(delay),
                    }
                };
            }
            Step::ToolCall { idx } => {
                self.emit_tool_call(idx)?;
                let mode = self.turn.as_ref().unwrap().spec.tools[idx].approval_mode();
                match self.resolve_tool_hook(idx, mode)? {
                    ToolDecision::Allow => self.begin_tool_run(idx)?,
                    ToolDecision::Deny(reason) => {
                        self.deny_tool(idx, &reason)?;
                        self.turn.as_mut().unwrap().step = Step::ToolFinish {
                            idx,
                            denied: Some(reason),
                        };
                    }
                    ToolDecision::Ask => self.open_approval_dialog(idx),
                }
            }
            Step::ToolRun { idx, ends } => {
                if Instant::now() >= ends {
                    self.turn.as_mut().unwrap().step = Step::ToolFinish { idx, denied: None };
                } else {
                    return Ok(ends);
                }
            }
            Step::ToolFinish { idx, denied } => {
                self.finish_tool(idx, denied.as_deref())?;
                let next = idx + 1;
                self.turn.as_mut().unwrap().step =
                    if next < self.turn.as_ref().unwrap().spec.tools.len() {
                        Step::ToolCall { idx: next }
                    } else {
                        let total = self.turn.as_ref().unwrap().spec.text_chunk_count();
                        Step::Text {
                            left: total,
                            total,
                            ready: Instant::now(),
                        }
                    };
                self.dirty = true;
            }
            Step::Text { left, total, ready } => {
                if Instant::now() < ready {
                    return Ok(ready);
                }
                // Codex persists completed items only; nothing is streamed.
                if self.dialect == Dialect::Codex {
                    self.turn.as_mut().unwrap().step = Step::Finish;
                } else {
                    self.emit_text_chunk(left, total)?;
                    let delay = self.turn.as_ref().unwrap().spec.delay_ms();
                    self.turn.as_mut().unwrap().step = if left <= 1 {
                        Step::Finish
                    } else {
                        Step::Text {
                            left: left - 1,
                            total,
                            ready: Instant::now() + Duration::from_millis(delay),
                        }
                    };
                }
            }
            Step::Finish => {
                self.finish_turn_records("end_turn")?;
                self.end_turn_and_maybe_continue(None)?;
            }
        }
        self.dirty = true;
        Ok(Instant::now())
    }
}

enum ToolDecision {
    Allow,
    Deny(String),
    Ask,
}

// ---------------------------------------------------------------------------
// Scenario selection
// ---------------------------------------------------------------------------

impl Engine {
    fn select_spec(&mut self, prompt: &str) -> TurnSpec {
        if let Some(idx) = self.scenario.select_turn(prompt, &self.consumed) {
            let spec = self.scenario.turns[idx].clone();
            if spec.is_catch_all() {
                self.consumed.insert(idx, ());
            }
            return spec;
        }
        echo_turn(prompt)
    }
}

fn echo_turn(prompt: &str) -> TurnSpec {
    TurnSpec {
        text: Some(format!("SPIKE_COMPLETE {prompt}")),
        ..TurnSpec::default()
    }
}

// ---------------------------------------------------------------------------
// Input handling
// ---------------------------------------------------------------------------

impl Engine {
    fn handle_input(&mut self, input: Input) {
        match self.view.mode {
            ScreenMode::Trust => self.handle_trust(input),
            ScreenMode::Approval => self.handle_approval(input),
            ScreenMode::Idle => self.handle_idle(input),
            ScreenMode::Working => self.handle_working(input),
        }
        self.dirty = true;
    }

    fn handle_trust(&mut self, input: Input) {
        let approved = match (self.dialect, &input) {
            (Dialect::Claude, Input::Up) => {
                self.trust_selected = false;
                return;
            }
            (Dialect::Claude, Input::Down) => {
                self.trust_selected = true;
                return;
            }
            (Dialect::Claude, Input::Digit(2)) => true,
            (Dialect::Claude, Input::Digit(1)) => false,
            (Dialect::Claude, Input::Submit) => self.trust_selected,
            (Dialect::Codex | Dialect::Grok, Input::Submit | Input::Digit(1)) => true,
            (Dialect::Codex | Dialect::Grok, Input::Digit(2)) => false,
            _ => return,
        };
        self.view.approval = None;
        if approved {
            self.view.mode = ScreenMode::Idle;
            self.trust_selected = false;
            self.event("trust_accepted", json!({}));
        } else {
            self.should_exit = true;
        }
    }

    fn handle_idle(&mut self, input: Input) {
        match input {
            Input::Text(text) | Input::Paste(text) => self.draft.push_str(&text),
            Input::Backspace => {
                self.draft.pop();
            }
            // Codex Tab submits when idle (evidence: "Tab submits when idle").
            Input::Tab if self.dialect == Dialect::Codex && !self.draft.is_empty() => {
                self.submit_prompt()
            }
            Input::Submit if !self.draft.is_empty() => self.submit_prompt(),
            Input::CtrlQ => self.grok_quit_chord(),
            _ => {}
        }
        self.view.draft = self.draft.clone();
    }

    fn handle_working(&mut self, input: Input) {
        match input {
            Input::Text(text) | Input::Paste(text) => self.draft.push_str(&text),
            Input::Backspace => {
                self.draft.pop();
            }
            Input::CtrlC if self.dialect == Dialect::Grok => self.grok_ctrl_c(),
            Input::Escape if self.dialect == Dialect::Grok => {
                // Esc is NOT an interrupt: turn and draft are preserved.
                self.view.notice = Some("Press Ctrl+c to cancel the turn".into());
                self.event(
                    "esc_notice",
                    json!({ "draft_preserved": !self.draft.is_empty() }),
                );
            }
            Input::Escape if self.dialect == Dialect::Claude => {
                let _ = self.interrupt_claude();
            }
            Input::Escape if self.dialect == Dialect::Codex => {
                let _ = self.interrupt_codex();
            }
            Input::Submit => self.working_submit(),
            Input::Tab if self.dialect == Dialect::Codex => self.codex_tab_queue(),
            Input::CtrlQ => self.grok_quit_chord(),
            _ => {}
        }
        self.view.draft = self.draft.clone();
        self.sync_queue_view();
    }

    fn working_submit(&mut self) {
        if self.draft.is_empty() {
            // Grok: empty Enter while a queued item waits cancels the turn and
            // immediately sends the queued item (the only send-now transport).
            if self.dialect == Dialect::Grok && !self.grok_queue.is_empty() {
                let _ = self.interrupt_grok("send_now");
            } else {
                self.event("empty_submit", json!({}));
            }
            return;
        }
        let prompt = std::mem::take(&mut self.draft);
        match self.dialect {
            Dialect::Claude => {
                let enqueued_at = self.clock.rfc3339();
                self.artifacts
                    .append(claude_queue_op(
                        &self.meta,
                        &self.clock,
                        "enqueue",
                        Some(&prompt),
                        None,
                    ))
                    .expect("artifact");
                self.fire_prompt_hook(&prompt);
                self.event("enqueue", json!({ "content": prompt }));
                self.claude_queue.push(QueuedPrompt {
                    text: prompt,
                    enqueued_at,
                });
            }
            Dialect::Codex => {
                if let Some(turn_id) = self.turn.as_ref().and_then(|t| t.codex_turn_id.clone()) {
                    self.write_codex_user_message(&turn_id, &prompt);
                }
                self.fire_prompt_hook(&prompt);
                self.event("steer", json!({ "content": prompt }));
                self.view.steering.push(prompt);
            }
            Dialect::Grok => {
                self.event("queue", json!({ "content": prompt }));
                self.grok_queue.push_back(prompt);
            }
        }
    }

    fn codex_tab_queue(&mut self) {
        if self.draft.is_empty() {
            return;
        }
        let prompt = std::mem::take(&mut self.draft);
        self.event("queue", json!({ "content": prompt, "key": "tab" }));
        self.codex_queue.push_back(prompt.clone());
        self.view.queued.push(prompt);
    }

    fn grok_ctrl_c(&mut self) {
        if !self.draft.is_empty() {
            // First Ctrl+C only clears the draft; the turn keeps running.
            self.draft.clear();
            self.view.draft.clear();
            self.last_ctrl_c = Some(Instant::now());
            self.event("ctrl_c_clear_draft", json!({}));
            return;
        }
        let now = Instant::now();
        let double = self
            .last_ctrl_c
            .is_some_and(|at| now.duration_since(at) < Duration::from_millis(1500));
        self.last_ctrl_c = Some(now);
        if double {
            let _ = self.interrupt_grok("ctrl_c");
        } else if self.turn.is_some() {
            self.view.notice = Some("Press Ctrl+c to cancel the turn".into());
        }
    }

    fn grok_quit_chord(&mut self) {
        let now = Instant::now();
        let double = self
            .last_ctrl_q
            .is_some_and(|at| now.duration_since(at) < Duration::from_secs(1));
        self.last_ctrl_q = Some(now);
        self.ctrl_q_count += 1;
        if double || self.ctrl_q_count >= 2 {
            self.event("quit", json!({ "key": "ctrl+q twice" }));
            self.should_exit = true;
        }
    }

    fn sync_queue_view(&mut self) {
        self.view.queued = match self.dialect {
            Dialect::Claude => self.claude_queue.iter().map(|q| q.text.clone()).collect(),
            Dialect::Codex => self.codex_queue.iter().cloned().collect(),
            Dialect::Grok => self.grok_queue.iter().cloned().collect(),
        };
    }
}

// ---------------------------------------------------------------------------
// Approval dialog
// ---------------------------------------------------------------------------

impl Engine {
    fn open_approval_dialog(&mut self, idx: usize) {
        let tool = self.turn.as_ref().unwrap().spec.tools[idx].clone();
        let command = tool
            .input_object()
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or(&tool.name)
            .to_owned();
        let reason = tool
            .input_object()
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let choices = default_choices(self.dialect);
        self.view.running_tool = Some(tool.name.clone());
        self.view.approval = Some(ApprovalView {
            tool: tool.name.clone(),
            command,
            reason,
            selected: 0,
            choices,
        });
        self.view.mode = ScreenMode::Approval;
        self.approval_tool_idx = Some(idx);
        self.event(
            "approval_prompt",
            json!({ "tool": tool.name, "index": idx }),
        );
        if self.dialect == Dialect::Grok {
            let _ = self.artifacts.grok_event(
                &self.clock,
                json!({ "type": "permission_requested", "tool_name": "run_terminal_command" }),
            );
        }
    }

    fn handle_approval(&mut self, input: Input) {
        let Some(idx) = self.approval_tool_idx else {
            self.view.mode = ScreenMode::Working;
            return;
        };
        let choice_count = self
            .view
            .approval
            .as_ref()
            .map(|a| a.choices.len())
            .unwrap_or(0);
        let decision: Option<&str> = match self.dialect {
            Dialect::Claude => match input {
                Input::Up => {
                    if let Some(view) = self.view.approval.as_mut() {
                        view.selected = view.selected.saturating_sub(1);
                    }
                    return;
                }
                Input::Down => {
                    if let Some(view) = self.view.approval.as_mut()
                        && view.selected + 1 < choice_count
                    {
                        view.selected += 1;
                    }
                    return;
                }
                Input::Digit(n) => Some(if n >= 3 { "deny" } else { "allow" }),
                Input::Submit => {
                    let selected = self.view.approval.as_ref().map(|a| a.selected).unwrap_or(0);
                    Some(if selected >= 2 { "deny" } else { "allow" })
                }
                Input::Escape => Some("deny"),
                _ => return,
            },
            Dialect::Codex => match input {
                Input::Submit | Input::Digit(1) => Some("allow"),
                Input::Digit(3) | Input::Escape => Some("deny"),
                _ => return,
            },
            Dialect::Grok => match input {
                Input::Digit(1) | Input::Digit(2) => Some("allow"),
                Input::Digit(3) | Input::Digit(4) | Input::Escape | Input::CtrlC => Some("deny"),
                _ => return,
            },
        };
        let Some(decision) = decision else {
            return;
        };
        self.view.mode = ScreenMode::Working;
        self.view.approval = None;
        self.approval_tool_idx = None;
        self.record_permission_resolved(idx, decision);
        self.event(
            "approval_decision",
            json!({ "decision": decision, "source": "keys", "tool_index": idx }),
        );
        if decision == "allow" {
            let _ = self.begin_tool_run(idx);
        } else {
            let reason = "user denied the tool call".to_owned();
            let _ = self.deny_tool(idx, &reason);
            if let Some(turn) = self.turn.as_mut() {
                turn.step = Step::ToolFinish {
                    idx,
                    denied: Some("user denied".to_owned()),
                };
            }
        }
    }

    fn record_permission_resolved(&mut self, idx: usize, decision: &str) {
        if self.dialect != Dialect::Grok {
            return;
        }
        let _ = self.artifacts.grok_event(
            &self.clock,
            json!({
                "type": "permission_resolved",
                "tool_name": "run_terminal_command",
                "decision": decision,
                "wait_ms": 1
            }),
        );
        let _ = idx;
    }
}

// ---------------------------------------------------------------------------
// Prompt submission and turn start
// ---------------------------------------------------------------------------

impl Engine {
    fn submit_prompt(&mut self) {
        let prompt = std::mem::take(&mut self.draft);
        if prompt.is_empty() {
            return;
        }
        let exit = match self.dialect {
            Dialect::Claude => prompt.trim() == "/exit",
            Dialect::Codex => prompt.trim() == "/quit",
            Dialect::Grok => prompt.trim() == "/exit",
        };
        if exit {
            self.event("exit_command", json!({ "command": prompt }));
            self.should_exit = true;
            return;
        }
        self.fire_prompt_hook(&prompt);
        self.event("submit", json!({ "content": prompt }));
        self.start_turn(prompt, None);
    }

    fn fire_prompt_hook(&mut self, prompt: &str) {
        let extra = json!({
            "prompt": prompt,
            "promptId": uuid_like(),
            "transcript_path": self.paths.main.to_string_lossy(),
            "transcriptPath": self.paths.main.to_string_lossy()
        });
        self.fire_hook(HookEvent::UserPromptSubmit, extra);
    }

    fn start_turn(&mut self, prompt: String, redirect: Option<&str>) {
        let spec = self.select_spec(&prompt);
        self.turn_count += 1;
        self.last_second = u64::MAX;
        let codex_turn_id = (self.dialect == Dialect::Codex).then(uuid_like);
        let grok_ids = GrokTurnIds {
            prompt_id: format!("prompt-{:03}", self.turn_count),
        };
        self.view.transcript.push(prompt.clone());
        self.view.mode = ScreenMode::Working;
        self.view.phase = Some(WorkingPhase::Thinking);
        self.view.elapsed_secs = 0;
        self.view.queued.clear();
        self.view.steering.clear();
        self.view.notice = None;
        self.view.running_tool = None;
        self.view.streaming_line = None;
        self.turn = Some(TurnRun {
            prompt,
            spec,
            started: Instant::now(),
            step: Step::Start,
            codex_turn_id,
            grok_ids,
            claude_seq: 0,
            tool_ids: Vec::new(),
            streamed_text: String::new(),
        });
        self.pending_redirect = redirect.map(str::to_owned);
        self.event("turn_start", json!({ "redirect": redirect }));
    }

    fn begin_turn_records(&mut self) -> Result<(), RunError> {
        let prompt = self.turn.as_ref().unwrap().prompt.clone();
        match self.dialect {
            Dialect::Claude => {
                self.artifacts.append(claude_user_record(
                    &self.meta,
                    &self.clock,
                    &prompt,
                    false,
                ))?;
            }
            Dialect::Codex => {
                let turn_id = self
                    .turn
                    .as_ref()
                    .and_then(|t| t.codex_turn_id.clone())
                    .unwrap_or_else(uuid_like);
                if self
                    .turn
                    .as_ref()
                    .is_some_and(|t| t.codex_turn_id.is_none())
                {
                    self.turn.as_mut().unwrap().codex_turn_id = Some(turn_id.clone());
                }
                self.write_codex_turn_header(&turn_id)?;
                self.write_codex_user_message(&turn_id, &prompt);
            }
            Dialect::Grok => {
                let turn_number = self.grok_turn_number;
                self.grok_turn_number += 1;
                let redirect = self.pending_redirect.clone();
                self.artifacts.grok_event(
                    &self.clock,
                    json!({
                        "type": "turn_started",
                        "turn_number": turn_number,
                        "model_id": self.meta.model,
                        "yolo_mode": false,
                        "conversation_message_count": 3,
                        "session_relationship": "primary",
                        "schema_version": "1.0",
                        "redirect_kind": redirect
                    }),
                )?;
                self.artifacts.grok_event(
                    &self.clock,
                    json!({ "type": "loop_started", "loop_index": 0 }),
                )?;
                self.artifacts.grok_event(
                    &self.clock,
                    json!({ "type": "phase_changed", "phase": "waiting_for_model" }),
                )?;
                self.artifacts
                    .grok_event(&self.clock, json!({ "type": "first_token" }))?;
                let ids = self.turn.as_ref().unwrap().grok_ids.clone();
                self.artifacts.grok_update(
                    &self.clock,
                    &ids,
                    grok_chunk_update("user_message_chunk", &prompt, &ids, &self.meta.model),
                    false,
                )?;
            }
        }
        Ok(())
    }

    fn write_codex_turn_header(&mut self, turn_id: &str) -> Result<(), RunError> {
        let payload = codex_task_started(&self.meta, &self.clock, turn_id);
        self.artifacts
            .codex_record(payload, &self.clock, &mut self.codex_counters)?;
        let context = json!({
            "type": "turn_context",
            "turn_id": turn_id,
            "root_turn_id": turn_id,
            "cwd": self.meta.cwd.to_string_lossy(),
            "workspace_roots": [self.meta.cwd.to_string_lossy()],
            "current_date": "2026-09-14",
            "timezone": "UTC",
            "approval_policy": "on-request",
            "approvals_reviewer": "user",
            "model": self.meta.model,
            "effort": "low"
        });
        self.artifacts
            .codex_record(context, &self.clock, &mut self.codex_counters)?;
        Ok(())
    }

    fn write_codex_user_message(&mut self, turn_id: &str, prompt: &str) {
        let message = codex_user_message(turn_id, prompt);
        self.artifacts
            .codex_record(message, &self.clock, &mut self.codex_counters)
            .expect("artifact");
        let item = codex_user_item_completed(&self.meta, &self.clock, turn_id, prompt);
        self.artifacts
            .codex_record(item, &self.clock, &mut self.codex_counters)
            .expect("artifact");
    }
}

// ---------------------------------------------------------------------------
// Thinking / text streaming
// ---------------------------------------------------------------------------

impl Engine {
    fn emit_thinking_chunk(&mut self) -> Result<(), RunError> {
        let full = self
            .turn
            .as_ref()
            .unwrap()
            .spec
            .thinking
            .clone()
            .unwrap_or_default();
        let count = self.turn.as_ref().unwrap().spec.think_chunk_count();
        let left = match self.turn.as_ref().unwrap().step {
            Step::Think { left, .. } => left,
            _ => count,
        };
        let chunks = split_chunks(&full, count);
        let chunk = chunks.get(count - left).cloned().unwrap_or_default();
        self.view.phase = Some(WorkingPhase::Thinking);
        match self.dialect {
            Dialect::Grok => {
                let ids = self.turn.as_ref().unwrap().grok_ids.clone();
                self.artifacts.grok_update(
                    &self.clock,
                    &ids,
                    grok_chunk_update("agent_thought_chunk", &chunk, &ids, &self.meta.model),
                    false,
                )?;
                self.artifacts.grok_event(
                    &self.clock,
                    json!({ "type": "phase_changed", "phase": "reasoning" }),
                )?;
            }
            Dialect::Codex if count - left == 0 => {
                // One completed reasoning item, no deltas (evidence A3).
                let turn_id = self
                    .turn
                    .as_ref()
                    .and_then(|t| t.codex_turn_id.clone())
                    .unwrap_or_default();
                let reasoning = json!({
                    "type": "reasoning",
                    "id": format!("rs_{}", uuid_like()),
                    "summary": [{ "type": "summary_text", "text": full }],
                    "content": null,
                    "encrypted_content": null,
                    "internal_chat_message_metadata_passthrough": { "turn_id": turn_id }
                });
                self.artifacts
                    .codex_record(reasoning, &self.clock, &mut self.codex_counters)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn emit_text_chunk(&mut self, left: usize, total: usize) -> Result<(), RunError> {
        let full = self
            .turn
            .as_ref()
            .unwrap()
            .spec
            .text
            .clone()
            .unwrap_or_default();
        let chunks = split_chunks(&full, total);
        let index = total - left;
        let chunk = chunks.get(index).cloned().unwrap_or_default();
        self.view.phase = Some(WorkingPhase::Responding);
        match self.dialect {
            Dialect::Claude => {
                self.turn.as_mut().unwrap().streamed_text.push_str(&chunk);
                let streamed = self.turn.as_ref().unwrap().streamed_text.clone();
                self.show_assistant_text(&streamed);
                let message_id = self.claude_message_id();
                self.fire_hook(
                    HookEvent::MessageDisplay,
                    json!({
                        "transcript_path": self.paths.main.to_string_lossy(),
                        "turn_id": format!("turn-{:03}", self.turn_count),
                        "message_id": message_id,
                        "index": index,
                        "final": left == 1,
                        "delta": chunk
                    }),
                );
            }
            Dialect::Grok => {
                let ids = self.turn.as_ref().unwrap().grok_ids.clone();
                self.artifacts.grok_update(
                    &self.clock,
                    &ids,
                    grok_chunk_update("agent_message_chunk", &chunk, &ids, &self.meta.model),
                    false,
                )?;
                self.turn.as_mut().unwrap().streamed_text.push_str(&chunk);
                let streamed = self.turn.as_ref().unwrap().streamed_text.clone();
                self.show_assistant_text(&streamed);
            }
            Dialect::Codex => unreachable!("codex skips the streaming phase"),
        }
        Ok(())
    }

    /// Show (and keep replacing) the in-flight assistant text line.
    fn show_assistant_text(&mut self, text: &str) {
        self.view.streaming_line = Some(text.to_owned());
    }

    /// Fold the in-flight streaming line into the finished transcript. Codex
    /// has no stream, so its completed text appears here all at once.
    fn finalize_assistant_text(&mut self) {
        let text = self
            .turn
            .as_ref()
            .and_then(|t| {
                if t.streamed_text.is_empty() {
                    t.spec.text.clone()
                } else {
                    Some(t.streamed_text.clone())
                }
            })
            .unwrap_or_default();
        self.view.streaming_line = None;
        if !text.is_empty() {
            self.view.transcript.push(text);
        }
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

impl Engine {
    fn emit_tool_call(&mut self, idx: usize) -> Result<(), RunError> {
        let tool = self.turn.as_ref().unwrap().spec.tools[idx].clone();
        let tool_id = format!("toolu-fake-{:03}-{idx}", self.turn_count);
        self.turn.as_mut().unwrap().tool_ids.push(tool_id.clone());
        self.view.phase = Some(WorkingPhase::Running);
        self.view.running_tool = Some(tool.name.clone());
        let command = tool
            .input_object()
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        match self.dialect {
            Dialect::Claude => {
                let message_id = self.claude_message_id();
                let block = json!({
                    "type": "tool_use",
                    "id": tool_id,
                    "name": tool.name,
                    "input": tool.input_object()
                });
                let seq = self.turn.as_ref().unwrap().claude_seq;
                let record = claude_assistant_block(
                    &self.meta,
                    &self.clock,
                    &message_id,
                    seq,
                    block,
                    "tool_use",
                    &self.usage_spec(),
                );
                self.artifacts.append(record)?;
                self.turn.as_mut().unwrap().claude_seq += 1;
                self.view
                    .transcript
                    .push(format!("⏺ {}({})", tool.name, command));
            }
            Dialect::Codex => {
                let turn_id = self
                    .turn
                    .as_ref()
                    .and_then(|t| t.codex_turn_id.clone())
                    .unwrap_or_default();
                // The model calls exec_command; the permission name normalizes
                // to Bash exactly like codex-cli 0.154 (evidence A1).
                let record =
                    codex_function_call(&tool_id, "exec_command", &tool.input_object(), &turn_id);
                self.artifacts
                    .codex_record(record, &self.clock, &mut self.codex_counters)?;
                self.view.transcript.push(format!("$ {command}"));
            }
            Dialect::Grok => {
                let ids = self.turn.as_ref().unwrap().grok_ids.clone();
                self.artifacts.grok_update(
                    &self.clock,
                    &ids,
                    grok_tool_call(&tool_id, "run_terminal_command", &tool.input_object()),
                    false,
                )?;
                self.artifacts.grok_event(
                    &self.clock,
                    json!({ "type": "tool_started", "tool_name": "run_terminal_command" }),
                )?;
                let label = tool
                    .input_object()
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or(&tool.name)
                    .to_owned();
                self.view.transcript.push(format!("◆ {label}"));
            }
        }
        let extra = json!({
            "toolName": tool.name,
            "tool_name": tool.name,
            "toolUseId": tool_id,
            "tool_use_id": tool_id,
            "toolInput": tool.input_object(),
            "tool_input": tool.input_object(),
            "toolInputTruncated": false,
            "transcript_path": self.paths.main.to_string_lossy(),
            "transcriptPath": self.paths.main.to_string_lossy()
        });
        let outcome = self.fire_hook(HookEvent::PreToolUse, extra);
        self.pre_tool_decision = outcome
            .decision
            .as_ref()
            .and_then(decision_behavior)
            .map(str::to_owned);
        Ok(())
    }

    /// Resolve approval for tool `idx` using hooks first, then the screen.
    fn resolve_tool_hook(
        &mut self,
        idx: usize,
        mode: ApprovalMode,
    ) -> Result<ToolDecision, RunError> {
        let tool = self.turn.as_ref().unwrap().spec.tools[idx].clone();
        // Grok has no PermissionRequest: PreToolUse deny/ask is the channel,
        // and ask forces the visible dialog.
        if self.dialect == Dialect::Grok {
            return Ok(match self.pre_tool_decision.take().as_deref() {
                Some("deny") => ToolDecision::Deny("Hook denied: pre_tool_use".into()),
                Some("ask") => ToolDecision::Ask,
                Some(other) => {
                    ToolDecision::Deny(format!("unsupported pre_tool_use decision {other}"))
                }
                None => match mode {
                    ApprovalMode::Auto => ToolDecision::Allow,
                    _ => ToolDecision::Ask,
                },
            });
        }
        // A PreToolUse deny wins everywhere.
        if let Some("deny") = self.pre_tool_decision.take().as_deref() {
            return Ok(ToolDecision::Deny("Hook denied: pre_tool_use".into()));
        }
        match mode {
            ApprovalMode::Auto => Ok(ToolDecision::Allow),
            ApprovalMode::Ask => Ok(ToolDecision::Ask),
            ApprovalMode::HookThenAsk | ApprovalMode::HookRequired => {
                let tool_id = self.turn.as_ref().unwrap().tool_ids[idx].clone();
                let normalized = if self.dialect == Dialect::Codex {
                    "Bash"
                } else {
                    tool.name.as_str()
                };
                let mut extra = json!({
                    "tool_name": normalized,
                    "toolName": normalized,
                    "tool_input": tool.input_object(),
                    "toolInput": tool.input_object(),
                    "cwd": self.meta.cwd.to_string_lossy(),
                    "model": self.meta.model,
                    "permission_mode": "default",
                    "transcript_path": self.paths.main.to_string_lossy()
                });
                // Codex stdin deliberately has no tool_use_id (evidence A1).
                if self.dialect == Dialect::Claude {
                    extra["tool_use_id"] = json!(tool_id);
                    extra["toolUseId"] = json!(tool_id);
                }
                if self.dialect == Dialect::Codex
                    && let Some(turn_id) = self.turn.as_ref().and_then(|t| t.codex_turn_id.clone())
                {
                    extra["session_id"] = json!(self.meta.session_id);
                    extra["turn_id"] = json!(turn_id);
                }
                let outcome = self.fire_hook(HookEvent::PermissionRequest, extra);
                match outcome.decision.as_ref().and_then(decision_behavior) {
                    Some("allow") => Ok(ToolDecision::Allow),
                    Some("deny") => Ok(ToolDecision::Deny("PermissionRequest hook denied".into())),
                    Some(other) => Ok(ToolDecision::Deny(format!(
                        "unsupported PermissionRequest decision {other}"
                    ))),
                    None => {
                        if mode == ApprovalMode::HookRequired {
                            Ok(ToolDecision::Deny(
                                "no PermissionRequest hook decision (hook_required)".into(),
                            ))
                        } else {
                            Ok(ToolDecision::Ask)
                        }
                    }
                }
            }
        }
    }

    fn begin_tool_run(&mut self, idx: usize) -> Result<(), RunError> {
        let duration = self.turn.as_ref().unwrap().spec.tools[idx].duration();
        self.turn.as_mut().unwrap().step = Step::ToolRun {
            idx,
            ends: Instant::now() + Duration::from_millis(duration),
        };
        Ok(())
    }

    fn deny_tool(&mut self, idx: usize, reason: &str) -> Result<(), RunError> {
        let tool = self.turn.as_ref().unwrap().spec.tools[idx].clone();
        let tool_id = self.turn.as_ref().unwrap().tool_ids[idx].clone();
        match self.dialect {
            Dialect::Grok => {
                let ids = self.turn.as_ref().unwrap().grok_ids.clone();
                self.artifacts.grok_update(
                    &self.clock,
                    &ids,
                    grok_tool_update_failed(&tool_id, reason),
                    false,
                )?;
                self.artifacts.grok_event(
                    &self.clock,
                    json!({
                        "type": "permission_resolved",
                        "tool_name": "run_terminal_command",
                        "decision": "deny",
                        "wait_ms": 0
                    }),
                )?;
                self.view.transcript.push(format!("✗ {}", reason));
            }
            Dialect::Codex => {
                let turn_id = self
                    .turn
                    .as_ref()
                    .and_then(|t| t.codex_turn_id.clone())
                    .unwrap_or_default();
                let output = codex_function_output(&tool_id, "", Some(reason), &turn_id);
                self.artifacts
                    .codex_record(output, &self.clock, &mut self.codex_counters)?;
                let command = tool
                    .input_object()
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let item = codex_command_item_completed(
                    &self.meta,
                    &self.clock,
                    &turn_id,
                    &tool_id,
                    &command,
                    CommandOutcome {
                        output: "",
                        exit_code: 1,
                        rejected: Some(reason),
                    },
                );
                self.artifacts
                    .codex_record(item, &self.clock, &mut self.codex_counters)?;
            }
            Dialect::Claude => {
                let source = uuid_like();
                let record = claude_tool_result(
                    &self.meta,
                    &self.clock,
                    &tool_id,
                    &source,
                    &Value::String(format!("Permission denied: {reason}")),
                    true,
                );
                self.artifacts.append(record)?;
            }
        }
        Ok(())
    }

    fn finish_tool(&mut self, idx: usize, denied: Option<&str>) -> Result<(), RunError> {
        let tool = self.turn.as_ref().unwrap().spec.tools[idx].clone();
        let tool_id = self.turn.as_ref().unwrap().tool_ids[idx].clone();
        let result = tool.result_value();
        let output = result.as_str().unwrap_or("").to_owned();
        let exit_code = tool.exit_code();

        if self.dialect == Dialect::Claude {
            // Physical order in the captured session: queue `remove` records,
            // then the tool result, then the queued-command attachments.
            self.write_claude_boundary_removes()?;
        }

        if denied.is_none() {
            match self.dialect {
                Dialect::Claude => {
                    let source = uuid_like();
                    let record = claude_tool_result(
                        &self.meta,
                        &self.clock,
                        &tool_id,
                        &source,
                        &result,
                        tool.is_error(),
                    );
                    self.artifacts.append(record)?;
                    self.fire_hook(
                        HookEvent::PostToolUse,
                        json!({
                            "tool_name": tool.name,
                            "toolName": tool.name,
                            "tool_use_id": tool_id,
                            "toolUseId": tool_id,
                            "tool_input": tool.input_object(),
                            "tool_response": { "stdout": output, "exit_code": exit_code },
                            "toolResult": { "stdout": output, "exit_code": exit_code },
                            "duration_ms": tool.duration(),
                            "durationMs": tool.duration(),
                            "isBackgrounded": false,
                            "transcript_path": self.paths.main.to_string_lossy()
                        }),
                    );
                }
                Dialect::Codex => {
                    let turn_id = self
                        .turn
                        .as_ref()
                        .and_then(|t| t.codex_turn_id.clone())
                        .unwrap_or_default();
                    let output_record = codex_function_output(&tool_id, &output, None, &turn_id);
                    self.artifacts.codex_record(
                        output_record,
                        &self.clock,
                        &mut self.codex_counters,
                    )?;
                    let command = tool
                        .input_object()
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    let item = codex_command_item_completed(
                        &self.meta,
                        &self.clock,
                        &turn_id,
                        &tool_id,
                        &command,
                        CommandOutcome {
                            output: &output,
                            exit_code,
                            rejected: None,
                        },
                    );
                    self.artifacts
                        .codex_record(item, &self.clock, &mut self.codex_counters)?;
                }
                Dialect::Grok => {
                    let ids = self.turn.as_ref().unwrap().grok_ids.clone();
                    let command = tool
                        .input_object()
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    self.artifacts.grok_update(
                        &self.clock,
                        &ids,
                        grok_tool_update_completed(
                            &tool_id,
                            &command,
                            &output,
                            exit_code,
                            &self.meta.cwd,
                            tool.is_error(),
                        ),
                        false,
                    )?;
                    self.artifacts.grok_event(
                        &self.clock,
                        json!({
                            "type": "tool_completed",
                            "tool_name": "run_terminal_command",
                            "duration_ms": tool.duration(),
                            "outcome": "success",
                            "tool_call_id": tool_id
                        }),
                    )?;
                }
            }
        }

        if self.dialect == Dialect::Claude {
            self.write_claude_boundary_attachments()?;
        }
        self.view.running_tool = None;
        Ok(())
    }

    /// Claude native boundary: queued prompts are absorbed after the tool
    /// result and before the next tool call. Emits the `remove` records only;
    /// attachments follow the result in [`Self::finish_tool`].
    fn write_claude_boundary_removes(&mut self) -> Result<(), RunError> {
        for queued in &self.claude_queue {
            self.artifacts.append(claude_queue_op(
                &self.meta,
                &self.clock,
                "remove",
                Some(&queued.text),
                Some("absorbed_mid_turn"),
            ))?;
        }
        Ok(())
    }

    fn write_claude_boundary_attachments(&mut self) -> Result<(), RunError> {
        if self.claude_queue.is_empty() {
            return Ok(());
        }
        let absorbed: Vec<QueuedPrompt> = self.claude_queue.drain(..).collect();
        for queued in &absorbed {
            // The attachment keeps its enqueue timestamp even though it is
            // physically written after the tool result.
            self.artifacts.append(claude_queued_attachment(
                &self.meta,
                &queued.enqueued_at,
                &queued.text,
            ))?;
        }
        self.view.queued.clear();
        self.event(
            "boundary_deliver",
            json!({ "count": absorbed.len(), "reason": "absorbed_mid_turn" }),
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Finish / interrupts / continuation
// ---------------------------------------------------------------------------

impl Engine {
    fn finish_turn_records(&mut self, stop_reason: &str) -> Result<(), RunError> {
        let text = self
            .turn
            .as_ref()
            .and_then(|t| {
                if t.streamed_text.is_empty() {
                    t.spec.text.clone()
                } else {
                    Some(t.streamed_text.clone())
                }
            })
            .unwrap_or_default();
        self.finalize_assistant_text();
        let usage = self.usage_spec();
        match self.dialect {
            Dialect::Claude => {
                let message_id = self.claude_message_id();
                let seq = self.turn.as_ref().unwrap().claude_seq;
                let record = claude_assistant_block(
                    &self.meta,
                    &self.clock,
                    &message_id,
                    seq,
                    json!({ "type": "text", "text": text }),
                    stop_reason,
                    &usage,
                );
                self.artifacts.append(record)?;
            }
            Dialect::Codex => {
                let turn_id = self
                    .turn
                    .as_ref()
                    .and_then(|t| t.codex_turn_id.clone())
                    .unwrap_or_default();
                let message = codex_assistant_message(&turn_id, &text);
                self.artifacts
                    .codex_record(message, &self.clock, &mut self.codex_counters)?;
                let response_id = format!("resp-{:03}", self.turn_count);
                let usage_record = codex_token_usage(&self.meta, &turn_id, &response_id, &usage);
                self.artifacts.codex_top_record(
                    usage_record,
                    &self.clock,
                    &mut self.codex_counters,
                )?;
                let complete = codex_task_complete(&turn_id, &text, &self.clock);
                self.artifacts
                    .codex_record(complete, &self.clock, &mut self.codex_counters)?;
            }
            Dialect::Grok => {
                let ids = self.turn.as_ref().unwrap().grok_ids.clone();
                let elapsed = self.turn.as_ref().unwrap().started.elapsed().as_millis() as u64;
                self.artifacts.grok_update(
                    &self.clock,
                    &ids,
                    grok_turn_completed(&ids, stop_reason, elapsed),
                    true,
                )?;
                self.artifacts.grok_event(
                    &self.clock,
                    json!({
                        "type": "turn_ended",
                        "outcome": if stop_reason == "end_turn" { "completed" } else { "cancelled" }
                    }),
                )?;
            }
        }
        self.fire_hook(
            HookEvent::Stop,
            json!({
                "reason": if stop_reason == "end_turn" { "end_turn" } else { "cancelled" },
                "stopHookActive": false,
                "lastAssistantMessage": text,
                "backgroundTasks": [],
                "sessionCrons": [],
                "promptId": format!("prompt-{:03}", self.turn_count),
                "transcript_path": self.paths.main.to_string_lossy()
            }),
        );
        Ok(())
    }

    fn end_turn_and_maybe_continue(&mut self, outcome: Option<&str>) -> Result<(), RunError> {
        self.event(
            "turn_end",
            json!({ "outcome": outcome.unwrap_or("completed") }),
        );
        self.view.mode = ScreenMode::Idle;
        self.view.phase = None;
        self.view.running_tool = None;
        self.view.notice = None;
        self.view.steering.clear();
        self.turn = None;

        // Claude: items still queued after the final tool boundary dequeue
        // after Stop and run as a new turn; the queue otherwise survives only
        // across an interrupt.
        if self.dialect == Dialect::Claude && !self.claude_queue.is_empty() {
            let next = self.claude_queue.remove(0);
            self.artifacts.append(claude_queue_op(
                &self.meta,
                &self.clock,
                "dequeue",
                None,
                None,
            ))?;
            self.artifacts.append(claude_user_record(
                &self.meta,
                &self.clock,
                &next.text,
                true,
            ))?;
            self.view.queued.clear();
            self.start_turn(next.text, Some("dequeue_after_stop"));
            return Ok(());
        }
        if let Some(next) = self.codex_queue.pop_front() {
            self.view.queued = self.codex_queue.iter().cloned().collect();
            self.start_turn(next, Some("queued_next_turn"));
            return Ok(());
        }
        if let Some(next) = self.grok_queue.pop_front() {
            self.view.queued = self.grok_queue.iter().cloned().collect();
            let redirect = self.pending_redirect.take();
            self.start_turn(next, redirect.as_deref());
            return Ok(());
        }
        if self
            .scenario
            .quit_after_turns
            .is_some_and(|n| self.turn_count >= n)
        {
            self.should_exit = true;
        }
        Ok(())
    }

    fn interrupt_claude(&mut self) -> Result<(), RunError> {
        let Some(turn) = self.turn.take() else {
            return Ok(());
        };
        let running_tool = matches!(turn.step, Step::ToolRun { .. });
        if let Step::ToolRun { idx, .. } = turn.step
            && let Some(tool_id) = turn.tool_ids.get(idx).cloned()
        {
            let record = claude_tool_result(
                &self.meta,
                &self.clock,
                &tool_id,
                &uuid_like(),
                &Value::String("User rejected tool use".to_owned()),
                true,
            );
            self.artifacts.append(record)?;
            self.view
                .transcript
                .push("[Request interrupted by user for tool use]".into());
        } else {
            let mut record = claude_user_record(
                &self.meta,
                &self.clock,
                "[Request interrupted by user]",
                false,
            );
            record["message"]["content"] = json!("[Request interrupted by user]");
            self.artifacts.append(record)?;
            self.view
                .transcript
                .push("[Request interrupted by user]".into());
        }
        // The queue survives Esc: one dequeue, then queued text runs on.
        if !self.claude_queue.is_empty() {
            self.artifacts.append(claude_queue_op(
                &self.meta,
                &self.clock,
                "dequeue",
                None,
                None,
            ))?;
        }
        let queued: Vec<QueuedPrompt> = self.claude_queue.drain(..).collect();
        for queued in &queued {
            self.artifacts.append(claude_user_record(
                &self.meta,
                &self.clock,
                &queued.text,
                true,
            ))?;
        }
        self.event(
            "interrupt",
            json!({
                "by": "esc",
                "tool_running": running_tool,
                "queue_survived": queued.len()
            }),
        );
        self.view.mode = ScreenMode::Idle;
        self.view.phase = None;
        self.view.running_tool = None;
        self.view.queued.clear();
        if let Some(first) = queued.into_iter().next() {
            self.claude_queue.clear();
            self.start_turn(first.text, Some("dequeue_after_interrupt"));
        }
        Ok(())
    }

    fn interrupt_codex(&mut self) -> Result<(), RunError> {
        let Some(turn) = self.turn.take() else {
            return Ok(());
        };
        let turn_id = turn.codex_turn_id.clone().unwrap_or_else(uuid_like);
        // An already-started tool can still complete under the old turn id
        // after Esc (evidence A5: ordinal 86 landed after turn_aborted).
        if let Step::ToolRun { idx, .. } = turn.step
            && let Some(tool) = turn.spec.tools.get(idx).cloned()
            && let Some(tool_id) = turn.tool_ids.get(idx).cloned()
        {
            let output = tool.result_value().as_str().unwrap_or("").to_owned();
            let command = tool
                .input_object()
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let out = codex_function_output(&tool_id, &output, None, &turn_id);
            self.artifacts
                .codex_record(out, &self.clock, &mut self.codex_counters)?;
            let item = codex_command_item_completed(
                &self.meta,
                &self.clock,
                &turn_id,
                &tool_id,
                &command,
                CommandOutcome {
                    output: &output,
                    exit_code: tool.exit_code(),
                    rejected: None,
                },
            );
            self.artifacts
                .codex_record(item, &self.clock, &mut self.codex_counters)?;
        }
        let aborted = codex_turn_aborted(&turn_id, &self.clock);
        self.artifacts
            .codex_record(aborted, &self.clock, &mut self.codex_counters)?;
        self.event("interrupt", json!({ "by": "esc", "turn_id": turn_id }));
        self.view.notice =
            Some("Conversation interrupted - tell the model what to do differently.".into());
        self.view.mode = ScreenMode::Idle;
        self.view.phase = None;
        self.view.running_tool = None;
        self.view.queued = self.codex_queue.iter().cloned().collect();
        Ok(())
    }

    fn interrupt_grok(&mut self, trigger: &str) -> Result<(), RunError> {
        let Some(turn) = self.turn.as_mut() else {
            return Ok(());
        };
        let ids = turn.grok_ids.clone();
        let elapsed = turn.started.elapsed().as_millis() as u64;
        self.artifacts.grok_update(
            &self.clock,
            &ids,
            grok_turn_completed(&ids, "cancelled", elapsed),
            true,
        )?;
        self.artifacts.grok_event(
            &self.clock,
            json!({
                "type": "turn_ended",
                "outcome": "cancelled",
                "cancellation_category": "mid_turn_abort",
                "cancellation_context": { "trigger": trigger }
            }),
        )?;
        self.event("interrupt", json!({ "by": trigger }));
        self.view.phase = None;
        self.view.running_tool = None;
        self.view.notice = None;
        self.turn = None;
        if trigger == "send_now"
            && let Some(next) = self.grok_queue.pop_front()
        {
            self.view.queued = self.grok_queue.iter().cloned().collect();
            self.start_turn(next, Some("queued_after_cancel"));
        } else {
            self.view.mode = ScreenMode::Idle;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Session header / shutdown / hooks / helpers
// ---------------------------------------------------------------------------

impl Engine {
    fn write_session_header(&mut self) -> Result<(), RunError> {
        if self.dialect == Dialect::Codex {
            let meta = codex_session_meta(&self.meta, &self.clock);
            self.artifacts.append(json!({
                "timestamp": self.clock.rfc3339(),
                "ordinal": 0,
                "type": "session_meta",
                "payload": meta
            }))?;
            codex_session_index(
                &self.home.join("session_index.jsonl"),
                &self.meta,
                "fake-harness",
                &self.clock,
            )?;
        }
        Ok(())
    }

    fn shutdown_artifacts(&mut self) -> Result<(), RunError> {
        if self.dialect == Dialect::Grok {
            // The registry entry is removed during shutdown, before the
            // process exits (evidence A2): discovery only, never liveness.
            std::fs::write(self.home.join("active_sessions.json"), "[]\n")?;
            if let Some(dir) = &self.paths.grok_dir {
                std::fs::write(dir.join("usage.json"), "{}\n")?;
            }
        }
        self.event("exit", json!({}));
        Ok(())
    }

    fn count_grok_turns(&self) -> u64 {
        let Some(events) = &self.paths.events else {
            return 0;
        };
        std::fs::read_to_string(events)
            .ok()
            .and_then(|text| {
                text.lines()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .filter(|row| row.get("type").and_then(Value::as_str) == Some("turn_started"))
                    .filter_map(|row| row.get("turn_number").and_then(Value::as_u64))
                    .max()
            })
            .map_or(0, |n| n + 1)
    }

    fn usage_spec(&self) -> UsageSpec {
        self.turn
            .as_ref()
            .and_then(|t| t.spec.usage.clone())
            .unwrap_or_default()
    }

    fn claude_message_id(&self) -> String {
        let seq = self.turn.as_ref().map(|t| t.claude_seq).unwrap_or(0);
        format!("msg-fake-{:03}-{seq}", self.turn_count)
    }

    fn fire_hook(&mut self, event: HookEvent, extra: Value) -> HookOutcome {
        let payload = crate::fake_harness::hooks::base_payload(
            event,
            &self.meta.session_id,
            &self.meta.cwd,
            &self.clock.rfc3339(),
            extra,
        );
        let kind = match self.dialect {
            Dialect::Claude => HookKind::Claude,
            Dialect::Codex => HookKind::Codex,
            Dialect::Grok => HookKind::Grok,
        };
        let ctx = HookContext {
            kind,
            session_id: &self.meta.session_id,
            cwd: &self.meta.cwd,
        };
        let outcome = self.hooks.fire(event, &payload, &ctx);
        // Grok reflects hook execution into updates.jsonl.
        if self.dialect == Dialect::Grok {
            for handler in self.hooks.handlers_for(event) {
                let _ = self.artifacts.grok_hook_execution(
                    &self.clock,
                    event.snake(),
                    &handler.command,
                    "success",
                    1,
                );
            }
        }
        outcome
    }
}

// ---------------------------------------------------------------------------
// Scenario + text helpers
// ---------------------------------------------------------------------------

fn default_scenario() -> Scenario {
    Scenario::parse(
        r#"{
            "turns": [
                {
                    "match_prefix": "SLOW",
                    "thinking": "Working through the slow request.",
                    "think_chunks": 2,
                    "text": "SPIKE_COMPLETE slow",
                    "tools": [
                        {"name": "Bash", "input": "sleep 20", "duration_ms": 400,
                         "approval": "auto"}
                    ]
                },
                {
                    "text": "SPIKE_COMPLETE",
                    "thinking": "Checking the probe.",
                    "tools": [
                        {"name": "Bash",
                         "input": {"command": "printf SPIKE_TOOL_OK", "description": "Write the fixed probe marker."},
                         "approval": "ask", "duration_ms": 20}
                    ]
                }
            ]
        }"#,
        "json",
    )
    .expect("default scenario")
}

fn split_chunks(text: &str, count: usize) -> Vec<String> {
    let count = count.max(1);
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![String::new(); count];
    }
    let mut out = Vec::with_capacity(count);
    let base = chars.len() / count;
    let extra = chars.len() % count;
    let mut start = 0;
    for i in 0..count {
        let len = base + usize::from(i < extra);
        let end = (start + len).min(chars.len());
        out.push(chars[start..end].iter().collect());
        start = end;
    }
    out
}

fn uuid_like() -> String {
    uuid::Uuid::new_v4().to_string()
}
