//! Runtime for the file-tail adapters: one poll task per promoted agent.
//!
//! The adapters are pure state machines; this module owns their lifecycle:
//! it polls on an interval, forwards stamped observations onto the instance's
//! single observation channel (shared with the hook bus and the promotion
//! poller, so sequence numbers interleave correctly), and exits when that
//! channel closes or the instance is dropped.
//!
//! Session identity follows the hook channel when it is there (Hook > File):
//! before each poll the supervisor checks the hook binding for a session id
//! reported by this process and hands it to the adapter. With no hook, the
//! adapter discovers its session from native files — rollout header /
//! `active_sessions.json` — exactly as the §4.3 ranking says.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use remuda_protocol::{AgentKind, HostId, Id, InstanceId, RunId};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{
    AdapterHome, AdapterObservation, CodexAdapter, FileSignalAdapter, GrokAdapter, GrokLive,
    LiveIdentity, StampCtx, next_seq, stamp,
};
use crate::error::DriverResult;
use crate::launch::HookSession;

/// How often the file tails are polled.
///
/// Short enough that chunk-level streaming feels live against an 800 ms
/// promotion tick, long enough that an idle session does no meaningful IO:
/// each poll is one `seek`+`read_to_end` of only appended bytes.
pub const ADAPTER_POLL: Duration = Duration::from_millis(250);

/// Everything the supervisor needs to stamp and route observations.
#[derive(Clone)]
pub struct AdapterCtx {
    /// Instance identity shared with the hook bus.
    pub stamp: StampCtx,
    /// Shared monotonic sequence counter (hook bus + poller + adapters).
    pub seq: Arc<std::sync::atomic::AtomicU64>,
    /// Journal channel every producer for this instance writes to.
    pub events: mpsc::Sender<remuda_protocol::Observation>,
    /// Harness home and discovery inputs.
    pub home: AdapterHome,
    /// Optional model label used to price codex usage before a turn_context
    /// reports one.
    pub fallback_model: Option<String>,
    /// The live hook session, so a `SessionStart` reported on the socket
    /// confirms the adapter's session identity (Hook > File). `None` for a
    /// file-only (promoted, unshimmed) session.
    pub hooks: Option<Arc<HookSession>>,
    /// Agent pid the adapter follows, used to reject a `SessionStart` from a
    /// different process sharing this home (same rule as
    /// `remuda_node::signal::binds_instance`).
    pub agent_pid: Option<i32>,
}

/// Handle to a running adapter task. Dropping/aborting it stops the poll loop.
pub struct AdapterHandle {
    pub(crate) tasks: Vec<JoinHandle<()>>,
}

impl AdapterHandle {
    /// Stop the adapter tasks. The tails and state die with them;
    /// `instance.purge` removes the files.
    pub fn abort(&self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Drop for AdapterHandle {
    fn drop(&mut self) {
        self.abort();
    }
}

/// Build the per-instance stamp context.
#[must_use]
pub fn stamp_ctx(
    instance_id: InstanceId,
    host_id: HostId,
    journal_id: Id,
    run_id: RunId,
    session_id: impl Into<String>,
) -> StampCtx {
    StampCtx {
        instance_id,
        host_id,
        journal_id,
        run_id,
        session_id: session_id.into(),
    }
}

/// Start the file adapters for a Remuda-launched agent (or a promoted one).
///
/// Returns `None` for kinds that have no file adapter. Exactly one adapter
/// task runs today per kind; the vector keeps room for a grok `usage.json`
/// watcher without a second spawn site.
pub fn spawn_file_adapters(
    kind: AgentKind,
    ctx: AdapterCtx,
) -> DriverResult<Option<AdapterHandle>> {
    match kind {
        AgentKind::Codex => {
            let mut adapter = CodexAdapter::new(ctx.home.clone());
            if let Some(model) = &ctx.fallback_model {
                adapter = adapter.with_fallback_model(model.clone());
            }
            Ok(Some(run(adapter, ctx)))
        }
        AgentKind::Grok => {
            // The file-tier live fold (turn.live phases, question
            // interactions) wraps the plain frame translator.
            let adapter = GrokAdapter::new(ctx.home.clone());
            let identity = LiveIdentity {
                instance_id: ctx.stamp.instance_id.clone(),
                host_id: ctx.stamp.host_id.clone(),
                run_id: ctx.stamp.run_id.clone(),
            };
            Ok(Some(run(GrokLive::new(adapter, identity), ctx)))
        }
        _ => Ok(None),
    }
}

fn run<A: FileSignalAdapter + 'static>(mut adapter: A, ctx: AdapterCtx) -> AdapterHandle {
    let task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(ADAPTER_POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if ctx.events.is_closed() {
                break;
            }
            // Hook > File: a SessionStart bound to the agent pid confirms the
            // adapter's session before file discovery guesses.
            if let Some(hooks) = &ctx.hooks
                && let Some(binding) = hooks.binding()
                && let Some(session_id) =
                    confirmed_session(binding.pid, ctx.agent_pid, &binding.session_id)
            {
                adapter.confirm_session(&session_id);
            }
            let polled = adapter.poll();
            // Resolve every synchronous result before an `.await`: the adapter
            // is `Send` but not `Sync`, so a borrow of it may not cross an
            // await inside a `tokio::spawn`ed task.
            let (observations, session_id) = match polled {
                Ok(observations) => {
                    let session_id = adapter
                        .session_id()
                        .map_or(ctx.stamp.session_id.clone(), str::to_owned);
                    (observations, session_id)
                }
                Err(error) => {
                    // A tail error is retried next tick (the file may simply
                    // not exist yet). Persistent errors are debug-logged, never
                    // fatal: losing the file channel must degrade, not crash.
                    tracing::debug!(%error, kind = ?adapter.kind(), "file adapter poll failed");
                    continue;
                }
            };
            if observations.is_empty() {
                continue;
            }
            let stamp = StampCtx {
                session_id,
                ..ctx.stamp.clone()
            };
            if forward_all(observations, &stamp, &ctx.events, &ctx.seq)
                .await
                .is_err()
            {
                break;
            }
        }
    });
    AdapterHandle { tasks: vec![task] }
}

/// Stamp and send every adapter observation. Free of adapter borrows so the
/// future stays `Send`.
async fn forward_all(
    observations: Vec<AdapterObservation>,
    ctx: &StampCtx,
    events: &mpsc::Sender<remuda_protocol::Observation>,
    seq: &std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> Result<(), mpsc::error::SendError<remuda_protocol::Observation>> {
    for observed in observations {
        let number = next_seq(seq);
        if let Some(observation) = stamp(ctx, number, &observed) {
            events.send(observation).await?;
        }
    }
    Ok(())
}

/// Resolve the shadow/native home for a hand-typed (promoted) agent from the
/// Node process environment. This is the fallback path; Remuda-launched agents
/// get an explicit shadow home through [`AdapterCtx::home`].
#[must_use]
pub fn promoted_home(kind: AgentKind, cwd: PathBuf, pid: Option<u32>) -> Option<AdapterHome> {
    let (env, dot) = match kind {
        AgentKind::Codex => ("CODEX_HOME", ".codex"),
        AgentKind::Grok => ("GROK_HOME", ".grok"),
        _ => return None,
    };
    let home = std::env::var_os(env)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(dot)))?;
    Some(AdapterHome { home, cwd, pid })
}

/// The session id a file adapter should follow given a hook-confirmed binding.
///
/// Pure helper so the driver can pre-confirm without owning adapter state: it
/// filters to the agent pid the supervisor is following, which is what stops a
/// second terminal's hooks binding this instance (same rule as
/// `remuda_node::signal::binds_instance`).
#[must_use]
pub fn confirmed_session(pid: i32, agent_pid: Option<i32>, session_id: &str) -> Option<String> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return None;
    }
    match agent_pid {
        Some(agent_pid) if agent_pid == pid => Some(session_id.to_owned()),
        Some(_) => None,
        None => Some(session_id.to_owned()),
    }
}

/// Observation alias re-export for callers that build their own pump.
pub type FileObservation = AdapterObservation;
