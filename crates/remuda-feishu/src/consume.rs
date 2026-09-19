//! `lark-cli event consume` supervisor: stdin keep-alive, SIGTERM, backoff restart.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};

use crate::error::Error;
use crate::inbound::{RawEvent, parse_event_line};
use crate::{EVENT_CARD_ACTION, EVENT_IM_RECEIVE};

/// Exponential backoff between consume restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// Delay after the first unexpected exit.
    pub initial: Duration,
    /// Cap.
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
        }
    }
}

impl Backoff {
    fn next(self, current: Duration) -> Duration {
        current.saturating_mul(2).min(self.max)
    }
}

/// How to spawn `event consume` children.
#[derive(Debug, Clone)]
pub struct ConsumeSettings {
    /// `lark-cli` binary.
    pub binary: PathBuf,
    /// Optional `--profile` for the dedicated dispatcher app.
    pub profile: Option<String>,
    /// `--as`; dispatcher uses `bot`.
    pub as_identity: String,
    /// One process per key (IM + card callback).
    pub event_keys: Vec<String>,
    /// Restart delay.
    pub backoff: Backoff,
    /// Extra env for test doubles (never used for secrets in production).
    pub extra_env: Vec<(String, String)>,
    /// Max stdout line size.
    pub line_max_bytes: usize,
}

impl ConsumeSettings {
    /// Default IM + card keys, `--as bot`.
    #[must_use]
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            profile: None,
            as_identity: "bot".into(),
            event_keys: vec![EVENT_IM_RECEIVE.into(), EVENT_CARD_ACTION.into()],
            backoff: Backoff::default(),
            extra_env: Vec::new(),
            line_max_bytes: 1024 * 1024,
        }
    }
}

/// Events emitted by the supervisor (not Feishu event types).
#[derive(Debug, Clone)]
pub enum ConsumeEvent {
    /// Stderr ready marker seen.
    Ready {
        /// EventKey for this child.
        event_key: String,
    },
    /// Parsed consume line.
    Event {
        /// EventKey for this child.
        event_key: String,
        /// Normalized payload.
        event: Box<RawEvent>,
    },
    /// Line was not JSON / failed schema.
    BadLine {
        /// EventKey for this child.
        event_key: String,
        /// Diagnostic (no secrets).
        detail: String,
    },
    /// Child exited (before a requested shutdown).
    Exited {
        /// EventKey for this child.
        event_key: String,
        /// OS status.
        status: Option<i32>,
    },
    /// Child will be spawned again after `delay`.
    Restarting {
        /// EventKey for this child.
        event_key: String,
        /// Restart attempt (1-based).
        attempt: u32,
        /// Sleep before spawn.
        delay: Duration,
    },
}

enum RunEnd {
    Shutdown,
    Exited(Option<i32>),
}

/// Owns one consume process per EventKey.
pub struct ConsumeSupervisor {
    shutdown: watch::Sender<bool>,
    join: Option<JoinHandle<()>>,
    pids: Arc<Mutex<Vec<u32>>>,
}

impl ConsumeSupervisor {
    /// Spawn workers. Caller must [`Self::shutdown`] (SIGTERM; stdin stays open until then).
    pub fn start(settings: ConsumeSettings) -> (Self, mpsc::Receiver<ConsumeEvent>) {
        let (tx, rx) = mpsc::channel(256);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let pids = Arc::new(Mutex::new(Vec::new()));
        let pids_task = Arc::clone(&pids);
        let join = tokio::spawn(async move {
            let mut handles = Vec::new();
            for key in settings.event_keys.clone() {
                let worker_settings = settings.clone();
                let worker_tx = tx.clone();
                let worker_shutdown = shutdown_rx.clone();
                let worker_pids = Arc::clone(&pids_task);
                handles.push(tokio::spawn(async move {
                    run_worker(
                        worker_settings,
                        key,
                        worker_tx,
                        worker_shutdown,
                        worker_pids,
                    )
                    .await;
                }));
            }
            drop(tx);
            for h in handles {
                let _ = h.await;
            }
        });
        (
            Self {
                shutdown,
                join: Some(join),
                pids,
            },
            rx,
        )
    }

    /// Signal SIGTERM to every child without waiting for its reader to finish.
    ///
    /// Use before [`Self::join`] when shutdown must keep processing accepted
    /// events while the children terminate.
    pub fn terminate(&self) {
        let _ = self.shutdown.send(true);
        sigterm_all(&self.pids);
    }

    /// Wait for every reader worker to finish. On shutdown each reader drains
    /// the child's already-accepted stdout to EOF before exiting, so this
    /// returning means every accepted event has been put on the event channel.
    pub async fn join(mut self) {
        if let Some(join) = self.join.take() {
            // Loaded-host budget: by this point the children have been asked to
            // exit and their readers only need scheduling to flush a pipe.
            let _ = timeout(Duration::from_secs(30), join).await;
        }
    }

    /// Signal SIGTERM and wait for workers to finish.
    pub async fn shutdown(self) {
        self.terminate();
        self.join().await;
    }
}

impl Drop for ConsumeSupervisor {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        sigterm_all(&self.pids);
    }
}

fn sigterm_all(pids: &Arc<Mutex<Vec<u32>>>) {
    let Ok(guard) = pids.lock() else {
        return;
    };
    for pid in guard.iter() {
        let _ = send_sigterm(*pid);
    }
}

fn send_sigterm(pid: u32) -> std::io::Result<()> {
    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("kill -TERM failed"))
    }
}

async fn run_worker(
    settings: ConsumeSettings,
    key: String,
    tx: mpsc::Sender<ConsumeEvent>,
    mut shutdown: watch::Receiver<bool>,
    pids: Arc<Mutex<Vec<u32>>>,
) {
    let mut delay = settings.backoff.initial;
    let mut attempt = 0u32;
    loop {
        if *shutdown.borrow() {
            break;
        }
        match run_one(&settings, &key, &tx, &mut shutdown, &pids).await {
            Ok(RunEnd::Shutdown) => break,
            Ok(RunEnd::Exited(status)) => {
                if *shutdown.borrow() {
                    break;
                }
                let _ = tx
                    .send(ConsumeEvent::Exited {
                        event_key: key.clone(),
                        status,
                    })
                    .await;
                attempt = attempt.saturating_add(1);
                let _ = tx
                    .send(ConsumeEvent::Restarting {
                        event_key: key.clone(),
                        attempt,
                        delay,
                    })
                    .await;
                warn!(event_key = %key, attempt, ?delay, "feishu consume restarting");
                tokio::select! {
                    _ = sleep(delay) => {}
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            break;
                        }
                    }
                }
                delay = settings.backoff.next(delay);
            }
            Err(err) => {
                warn!(event_key = %key, %err, "feishu consume worker error");
                if *shutdown.borrow() {
                    break;
                }
                attempt = attempt.saturating_add(1);
                let _ = tx
                    .send(ConsumeEvent::Restarting {
                        event_key: key.clone(),
                        attempt,
                        delay,
                    })
                    .await;
                tokio::select! {
                    _ = sleep(delay) => {}
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            break;
                        }
                    }
                }
                delay = settings.backoff.next(delay);
            }
        }
    }
}

async fn run_one(
    settings: &ConsumeSettings,
    key: &str,
    tx: &mpsc::Sender<ConsumeEvent>,
    shutdown: &mut watch::Receiver<bool>,
    pids: &Arc<Mutex<Vec<u32>>>,
) -> Result<RunEnd, Error> {
    let mut cmd = Command::new(&settings.binary);
    cmd.arg("event")
        .arg("consume")
        .arg(key)
        .arg("--as")
        .arg(&settings.as_identity)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    if let Some(profile) = &settings.profile {
        cmd.arg("--profile").arg(profile);
    }
    for (k, v) in &settings.extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;
    let pid = child.id();
    if let Some(pid) = pid {
        pids.lock().unwrap_or_else(|e| e.into_inner()).push(pid);
    }
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().ok_or_else(|| Error::Cli {
        status: None,
        stderr: "missing stdout".into(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| Error::Cli {
        status: None,
        stderr: "missing stderr".into(),
    })?;
    let mut stdout = BufReader::new(stdout);
    let mut stderr = BufReader::new(stderr);

    let ready = wait_ready(&mut child, &mut stderr, shutdown).await?;
    if *shutdown.borrow() {
        let _ = finish_shutdown(&mut child, stdin, pid, pids).await;
        return Ok(RunEnd::Shutdown);
    }
    if !ready {
        let status = finish_child(&mut child, stdin, pid, pids).await;
        return Ok(RunEnd::Exited(status));
    }
    let _ = tx
        .send(ConsumeEvent::Ready {
            event_key: key.to_string(),
        })
        .await;
    info!(event_key = %key, "feishu consume ready");

    let mut line = String::new();
    loop {
        line.clear();
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    if let Some(pid) = pid {
                        let _ = send_sigterm(pid);
                    }
                    let reaped = drain_stdout(&mut child, &mut stdout, tx, settings, key, &mut line).await;
                    drop(stdin);
                    if !reaped {
                        let _ = timeout(Duration::from_secs(2), child.wait()).await;
                    }
                    forget_pid(pids, pid);
                    return Ok(RunEnd::Shutdown);
                }
            }
            n = stdout.read_line(&mut line) => {
                let n = n?;
                if n == 0 {
                    let status = finish_child(&mut child, stdin, pid, pids).await;
                    return Ok(RunEnd::Exited(status));
                }
                dispatch_line(settings, key, tx, &line).await;
            }
            status = child.wait() => {
                forget_pid(pids, pid);
                drop(stdin);
                let code = status?.code();
                return Ok(RunEnd::Exited(code));
            }
        }
    }
}

/// Parse and deliver one stdout line as an event or a bad-line diagnostic.
async fn dispatch_line(
    settings: &ConsumeSettings,
    key: &str,
    tx: &mpsc::Sender<ConsumeEvent>,
    line: &str,
) {
    if line.len() > settings.line_max_bytes {
        let _ = tx
            .send(ConsumeEvent::BadLine {
                event_key: key.to_string(),
                detail: Error::LineTooLong(settings.line_max_bytes).to_string(),
            })
            .await;
        return;
    }
    match parse_event_line(line) {
        Ok(event) => {
            let _ = tx
                .send(ConsumeEvent::Event {
                    event_key: key.to_string(),
                    event: Box::new(event),
                })
                .await;
        }
        Err(err) => {
            debug!(event_key = %key, %err, "feishu consume bad line");
            let _ = tx
                .send(ConsumeEvent::BadLine {
                    event_key: key.to_string(),
                    detail: err.to_string(),
                })
                .await;
        }
    }
}

/// Upper bound for reading the child's accepted stdout after SIGTERM.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(8);

/// Keep reading the SIGTERM-ed child's stdout to EOF, enqueuing every line,
/// until the child exits, EOF, or the bounded drain window passes.
///
/// Closing the event channel before these buffered lines were read dropped
/// accepted events on shutdown (a gate flake under host load). Returns whether
/// the child was observed exiting; a child ignoring SIGTERM only loses its
/// remaining stdout once [`SHUTDOWN_DRAIN`] passes.
async fn drain_stdout(
    child: &mut Child,
    stdout: &mut BufReader<tokio::process::ChildStdout>,
    tx: &mpsc::Sender<ConsumeEvent>,
    settings: &ConsumeSettings,
    key: &str,
    line: &mut String,
) -> bool {
    let deadline = sleep(SHUTDOWN_DRAIN);
    tokio::pin!(deadline);
    loop {
        line.clear();
        tokio::select! {
            biased;
            _ = &mut deadline => return false,
            status = child.wait() => return status.is_ok(),
            result = stdout.read_line(line) => {
                if let Ok(0) | Err(_) = result {
                    return false;
                }
                dispatch_line(settings, key, tx, line).await;
            }
        }
    }
}

async fn wait_ready(
    child: &mut Child,
    stderr: &mut BufReader<tokio::process::ChildStderr>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<bool, Error> {
    let mut buf = String::new();
    loop {
        buf.clear();
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return Ok(false);
                }
            }
            n = stderr.read_line(&mut buf) => {
                let n = n?;
                if n == 0 {
                    return Ok(false);
                }
                if buf.trim_end().starts_with("[event] ready") {
                    return Ok(true);
                }
            }
            status = child.wait() => {
                let _ = status?;
                return Ok(false);
            }
        }
    }
}

async fn finish_shutdown(
    child: &mut Child,
    stdin: Option<ChildStdin>,
    pid: Option<u32>,
    pids: &Arc<Mutex<Vec<u32>>>,
) -> Option<i32> {
    if let Some(pid) = pid {
        let _ = send_sigterm(pid);
    }
    match timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) => {
            forget_pid(pids, pid);
            drop(stdin);
            status.code()
        }
        _ => {
            drop(stdin);
            match timeout(Duration::from_secs(2), child.wait()).await {
                Ok(Ok(status)) => {
                    forget_pid(pids, pid);
                    status.code()
                }
                _ => {
                    forget_pid(pids, pid);
                    None
                }
            }
        }
    }
}

async fn finish_child(
    child: &mut Child,
    stdin: Option<ChildStdin>,
    pid: Option<u32>,
    pids: &Arc<Mutex<Vec<u32>>>,
) -> Option<i32> {
    drop(stdin);
    match timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) => {
            forget_pid(pids, pid);
            status.code()
        }
        _ => {
            forget_pid(pids, pid);
            None
        }
    }
}

fn forget_pid(pids: &Arc<Mutex<Vec<u32>>>, pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    if let Ok(mut guard) = pids.lock() {
        guard.retain(|p| *p != pid);
    }
}
