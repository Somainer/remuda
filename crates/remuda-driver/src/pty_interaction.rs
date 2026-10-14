//! Explicit human replies to a fenced, screen-derived Herdr blocked prompt.
//! A key write proves transport delivery only; clearing the prompt settles it.

use crate::binary::hash_bytes;
use crate::claude_pty::{ObsCtx, emit_on, map_herdr, now_ts};
use crate::{DriverAck, DriverError, DriverResult};
use remuda_herdr::{
    AgentReadParams, AgentStatus, Client, PersistentClient, ReadFormat, ReadSource,
};
use remuda_protocol::*;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

#[cfg(test)]
use remuda_screen::SCREEN_BYTES;
use remuda_screen::{SCREEN_LINES, ScreenGrid};

const ANSWER_BYTES: usize = 1024;

/// `agent.start` can report not-ready after it has created a blocked pane.
/// Reconcile that exact pane once; never replay the launch or auto-dismiss it.
pub(crate) async fn start_agent(
    client: &Client,
    params: remuda_herdr::AgentStartParams,
) -> DriverResult<remuda_herdr::AgentStarted> {
    match crate::pty_launch::start_agent(client, params.clone()).await {
        Ok(started) => Ok(started),
        Err(error @ remuda_herdr::Error::Api { .. }) => {
            if let remuda_herdr::Error::Api { code, .. } = &error
                && matches!(code.as_str(), "agent_not_ready" | "agent_blocked")
                && let Ok(info) = client.agent_get(&params.name).await
                && info.agent.pane_id == params.pane_id
                && info.agent.agent_status == AgentStatus::Blocked
            {
                return Ok(remuda_herdr::AgentStarted {
                    kind: "agent_started".into(),
                    agent: info.agent,
                    argv: params.args,
                });
            }
            Err(map_herdr(error))
        }
        Err(error) => Err(map_herdr(error)),
    }
}

struct Pending {
    interaction: Interaction,
    screen: String,
    screen_truncated: bool,
    state_change_seq: u64,
    keys: BTreeMap<String, Vec<String>>,
    attempted: bool,
}

/// Startup-watch budget: 250 ms per poll, so ~3 minutes. This only bounds a
/// carrier whose driver never reports a successful start; the normal path
/// disarms the watch as soon as the native session is real.
const STARTUP_POLL_BUDGET: u32 = 720;

struct State {
    pending: Option<Pending>,
    status: Option<AgentStatus>,
    closed: bool,
    trust_attempted: bool,
    /// One automatic answer per recognised first-run screen, per carrier.
    onboarding_attempted: BTreeSet<&'static str>,
    /// Screens already journalled, so a wizard step that lingers across polls
    /// produces one diagnostic rather than four per second.
    onboarding_reported: BTreeSet<&'static str>,
    /// Polls left in the startup watch. The watch reads the viewport while the
    /// pane is *idle*, which is the only way a first-run wizard is visible at
    /// all; the driver disarms it once the carrier has really started.
    startup_polls_left: u32,
    /// Spinner-line projection (verb/tokens/phrase/interruptible).
    live: remuda_screen::ScreenLiveLatch,
    /// A live spinner reading is currently journaled, so the viewport must
    /// keep being read until the clear is observed.
    live_active: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            pending: None,
            status: None,
            closed: false,
            trust_attempted: false,
            onboarding_attempted: BTreeSet::new(),
            onboarding_reported: BTreeSet::new(),
            startup_polls_left: STARTUP_POLL_BUDGET,
            live: remuda_screen::ScreenLiveLatch::new(),
            live_active: false,
        }
    }
}

pub(crate) struct PtyInteractions {
    client: Client,
    // Always an exact pane ID, never a mutable herdr agent alias.
    target: String,
    ctx: ObsCtx,
    tx: mpsc::Sender<Observation>,
    seq: Arc<AtomicU64>,
    state: Mutex<State>,
    /// Per-run revocation latch (c-ctxusage r26 item 1). A control that passed
    /// the driver entry gate can still be QUEUED on `state` behind another
    /// request holding the mutex across a stalled Herdr RPC. `close` sets this
    /// synchronously — before any awaited shutdown work — so the queued control
    /// is refused after acquiring the mutex even when the bounded `close`
    /// cleanup never reaches `state.closed`. The pane can then be reused by a
    /// restarted carrier without receiving a queued keystroke.
    closing: Arc<AtomicBool>,
    auto_trust_registered_workspace: bool,
    /// c-ctxusage r27/r28 item 1 test seam: when armed, an interaction
    /// dispatch signals `arrived` AFTER passing the final (post-mutex)
    /// revocation check and then parks on `release` BEFORE the pinned
    /// transport write — the exact window in which a close can revoke and
    /// the carrier restart/reuse the pane id. The acknowledgement makes the
    /// park point deterministic: no sleep-based race.
    #[cfg(any(test, feature = "test-stub"))]
    pre_dispatch_gate: std::sync::Mutex<Option<DispatchGate>>,
}

/// See [`PtyInteractions::arm_pre_dispatch_gate`].
#[cfg(any(test, feature = "test-stub"))]
pub(crate) struct DispatchGate {
    /// Fired once, strictly after the final check passes.
    arrived: tokio::sync::oneshot::Sender<()>,
    /// The parked dispatch proceeds only once this receiver resolves. Using a
    /// oneshot (rather than `Notify::notify_waiters`) RETAINS the release for
    /// a waiter that has not parked yet, so a release can never be lost to
    /// scheduling.
    release: tokio::sync::oneshot::Receiver<()>,
}

impl PtyInteractions {
    pub(crate) async fn observe(&self) -> DriverResult<()> {
        // One pinned connection for the whole refresh, including any
        // auto-answer dispatch it reaches (r27 item 1).
        let mut conn = self.pinned().await?;
        let mut state = self.state.lock().await;
        if state.closed || self.revoked() {
            return Ok(());
        }
        self.refresh(&mut state, &mut conn).await
    }
    pub(crate) fn new(
        client: Client,
        target: String,
        ctx: ObsCtx,
        tx: mpsc::Sender<Observation>,
        seq: Arc<AtomicU64>,
        auto_trust_registered_workspace: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            client,
            target,
            ctx,
            tx,
            seq,
            state: Mutex::new(State::default()),
            closing: Arc::new(AtomicBool::new(false)),
            auto_trust_registered_workspace,
            #[cfg(any(test, feature = "test-stub"))]
            pre_dispatch_gate: std::sync::Mutex::new(None),
        })
    }

    /// Arm the post-check/pre-write park used by the r27/r28 regressions.
    #[allow(dead_code)]
    #[cfg(any(test, feature = "test-stub"))]
    pub(crate) fn arm_pre_dispatch_gate(&self, gate: DispatchGate) {
        *self.pre_dispatch_gate.lock().expect("gate lock") = Some(gate);
    }

    /// Open the held-open connection a control/refresh cycle runs on. It is
    /// established BEFORE the revocation check: a carrier restart that lands
    /// after the check breaks this connection rather than letting the dispatch
    /// dial a fresh socket onto the replacement boot that reused the pane id
    /// (c-ctxusage r27 item 1, F1' held-connection pattern).
    async fn pinned(&self) -> DriverResult<PersistentClient> {
        self.client.persistent().await.map_err(map_herdr)
    }

    /// Revoke every queued/future control SYNCHRONOUSLY. Called the moment a
    /// run's close begins, before any awaited shutdown: unlike `state.closed`,
    /// setting this never waits on the interaction mutex, so a request stalled
    /// mid-RPC while holding that mutex cannot shield a queued control past a
    /// carrier restart and pane reuse.
    pub(crate) fn revoke(&self) {
        self.closing.store(true, Ordering::SeqCst);
    }

    /// True after [`Self::revoke`]. Callers MUST be holding `state`; the check
    /// sits between mutex acquisition and the dispatch so a control queued
    /// behind a stalled request is refused once it reaches the front.
    fn revoked(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }

    pub(crate) fn spawn(self: &Arc<Self>) -> JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(250));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                // Pin the carrier boot for this tick's refresh and any
                // auto-answer dispatch it reaches.
                let mut conn = match this.pinned().await {
                    Ok(conn) => conn,
                    Err(error) => {
                        tracing::debug!(%error, "PTY interaction connection unavailable");
                        continue;
                    }
                };
                let mut state = this.state.lock().await;
                if state.closed || this.revoked() || this.tx.is_closed() {
                    break;
                }
                if let Err(error) = this.refresh(&mut state, &mut conn).await {
                    tracing::debug!(%error, "PTY interaction observation unavailable");
                }
            }
        })
    }

    async fn emit(&self, payload: ObservationPayload) -> DriverResult<()> {
        emit_on(
            &self.tx,
            &self.seq,
            &self.ctx,
            SourceChannel::Herdr,
            Completeness::ScreenDerived,
            payload,
        )
        .await
    }

    async fn screen(&self, conn: &mut PersistentClient) -> DriverResult<(String, bool)> {
        let read = conn
            .agent_read(AgentReadParams {
                target: self.target.clone(),
                source: ReadSource::Visible,
                lines: Some(SCREEN_LINES as u32),
                format: ReadFormat::Text,
                strip_ansi: true,
            })
            .await
            .map_err(map_herdr)?;
        let truncated = read.truncated.unwrap_or(false)
            || read.read.as_ref().is_some_and(|body| body.truncated);
        Ok((read.text().to_owned(), truncated))
    }

    /// Project the spinner status line while the pane is working (or a live
    /// reading has yet to clear). Herdr's own `agent_status` is the working
    /// authority here — the pane text alone lost the hint phrase in 2.1.270
    /// (D-2) — so it is handed to the latch as the known-working override.
    async fn observe_live(
        &self,
        state: &mut State,
        status: AgentStatus,
        conn: &mut PersistentClient,
    ) -> DriverResult<()> {
        if status != AgentStatus::Working && !state.live_active {
            return Ok(());
        }
        let known_working = match status {
            AgentStatus::Working => Some(true),
            AgentStatus::Idle | AgentStatus::Done | AgentStatus::Blocked => Some(false),
            AgentStatus::Unknown => None,
        };
        let (screen, _) = self.screen(conn).await?;
        let grid = ScreenGrid::from_raw(&screen);
        let Some(change) = state.live.observe_with(&grid, known_working) else {
            return Ok(());
        };
        let active = matches!(change, remuda_screen::ScreenLiveChange::Active(_));
        let payload = match change {
            remuda_screen::ScreenLiveChange::Active(mut live) => {
                // Herdr's working verdict, not the retired hint text.
                if status == AgentStatus::Working {
                    live.interruptible = true;
                }
                crate::screenlive::live_status_payload(&live, crate::screenlive::now_utc())
            }
            remuda_screen::ScreenLiveChange::Inactive => crate::screenlive::live_status_inactive(),
        };
        self.emit(payload).await?;
        state.live_active = active;
        Ok(())
    }

    async fn refresh(&self, state: &mut State, conn: &mut PersistentClient) -> DriverResult<()> {
        let info = conn.agent_get(&self.target).await.map_err(map_herdr)?.agent;
        let status = info.agent_status;
        if state.status != Some(status) {
            let label = match status {
                AgentStatus::Blocked => "blocked",
                AgentStatus::Working => "working",
                AgentStatus::Idle | AgentStatus::Done => "idle",
                AgentStatus::Unknown => "unknown",
            };
            self.emit(ObservationPayload::Lifecycle(Box::new(
                LifecyclePayload::Native(Box::new(NativeLifecycle {
                    topic: LifecycleTopic::Turn,
                    native_name: "agent_status".into(),
                    native_id: Knowledge::Known {
                        value: self.target.clone(),
                    },
                    status: Knowledge::Known {
                        value: label.into(),
                    },
                    related_ids: BTreeMap::new(),
                    data_ref: None,
                    severity: Severity::Info,
                    affects_completion: false,
                })),
            )))
            .await?;
            state.status = Some(status);
        }
        self.observe_live(state, status, conn).await?;
        if status != AgentStatus::Blocked {
            if status != AgentStatus::Unknown {
                self.clear(state, InteractionState::Resolved, "native-cleared")
                    .await?;
            }
            // Herdr calls a first-run wizard idle: it is a TUI at a menu, not a
            // blocked agent. Only a viewport read tells the two apart, and
            // without this the D-022 queue would type a prompt into the wizard.
            self.watch_startup(state, status, conn).await?;
            return Ok(());
        }
        let (screen, screen_truncated) = self.screen(conn).await?;
        // Check again after reading: never associate an old viewport with a new status epoch.
        let after = conn.agent_get(&self.target).await.map_err(map_herdr)?.agent;
        if after.agent_status != status || after.state_change_seq != info.state_change_seq {
            return Ok(());
        }
        if state.pending.as_ref().is_some_and(|p| {
            p.screen == screen
                && p.screen_truncated == screen_truncated
                && p.state_change_seq == info.state_change_seq
        }) {
            return Ok(());
        }
        self.clear(state, InteractionState::Resolved, "native-cleared")
            .await?;
        let (mut interaction, keys) = screen_request(&self.ctx, &screen)?;
        interaction.answerable &= !screen_truncated;
        self.emit(ObservationPayload::InteractionRequested(Box::new(
            InteractionRequestedPayload {
                interaction: interaction.clone(),
            },
        )))
        .await?;
        state.pending = Some(Pending {
            interaction,
            keys,
            screen,
            screen_truncated,
            state_change_seq: info.state_change_seq,
            attempted: false,
        });
        if self.auto_trust_registered_workspace
            && self.ctx.driver == DriverKind::ClaudePty
            && !state.trust_attempted
            && !screen_truncated
            && let Some(pending) = state.pending.as_mut()
            && pending.interaction.answerable
            && let Some(keys) = trust_dialog_keys(&pending.screen)
        {
            // One automatic attempt per carrier, even if an ACK is lost and the
            // screen changes. Only observed native clearing resolves the dialog.
            state.trust_attempted = true;
            let result = self.commit_answer(conn, pending, keys).await;
            self.emit(ObservationPayload::Lifecycle(Box::new(
                LifecyclePayload::Native(Box::new(NativeLifecycle {
                    topic: LifecycleTopic::Diagnostic,
                    native_name: "trust-dialog".into(),
                    native_id: Knowledge::Known {
                        value: self.target.clone(),
                    },
                    status: Knowledge::Known {
                        value: if result.is_ok() {
                            "trust-dialog auto-accepted (registered workspace)"
                        } else {
                            "trust-dialog automatic answer delivery unknown; not replayed"
                        }
                        .into(),
                    },
                    related_ids: BTreeMap::new(),
                    data_ref: None,
                    severity: if result.is_ok() {
                        Severity::Info
                    } else {
                        Severity::Warning
                    },
                    affects_completion: false,
                })),
            )))
            .await?;
            result?;
        }
        Ok(())
    }

    /// True while a recognised first-run screen is on the viewport.
    ///
    /// [`prompt_ready`] consults this so the D-022 queue keeps waiting instead
    /// of typing a prompt into a wizard that Herdr reports as idle.
    pub(crate) async fn startup_dialog_pending(&self) -> bool {
        let state = self.state.lock().await;
        state.startup_polls_left > 0 && !state.onboarding_reported.is_empty()
    }

    /// Stop reading the viewport for first-run screens.
    ///
    /// Called once the carrier has really started (Claude's own SessionStart
    /// hook fired), so ordinary model output can never be mistaken for a
    /// wizard later in the session.
    pub(crate) async fn disarm_startup_watch(&self) {
        self.state.lock().await.startup_polls_left = 0;
    }

    /// Look for a first-run screen on an otherwise-idle pane.
    ///
    /// Answers the steps whose default is safe and journals every recognised
    /// screen exactly once. Screens that carry a real decision (login, the
    /// bypass disclaimer) are only reported: Remuda never accepts those for a
    /// human.
    async fn watch_startup(
        &self,
        state: &mut State,
        status: AgentStatus,
        conn: &mut PersistentClient,
    ) -> DriverResult<()> {
        if self.ctx.driver != DriverKind::ClaudePty || state.startup_polls_left == 0 {
            return Ok(());
        }
        state.startup_polls_left -= 1;
        // An `unknown` pane has no readable viewport yet; spend the budget
        // rather than an RPC round-trip per tick.
        if status == AgentStatus::Unknown {
            return Ok(());
        }
        let (screen, truncated) = self.screen(conn).await?;
        let Some(dialog) = crate::claude_onboarding::startup_dialog(&screen) else {
            return Ok(());
        };
        let first_sighting = state.onboarding_reported.insert(dialog.name);
        // Only the exact, whole screen is answerable: a truncated viewport may
        // hide a further option or a different cursor position. One attempt per
        // screen per carrier, even if an ACK is lost.
        let answerable = !truncated && !state.onboarding_attempted.contains(dialog.name);
        if let Some(keys) = dialog.keys.filter(|_| answerable) {
            // Close began while this refresh waited for the screen: never
            // auto-answer onto a pane the carrier may be about to reuse.
            if self.revoked() {
                return Ok(());
            }
            // Mark before I/O: a lost ACK must never replay Enter.
            state.onboarding_attempted.insert(dialog.name);
            let (status, severity) = match self.write(conn, keys).await {
                Ok(_) => (
                    format!("{} auto-answered (default)", dialog.name),
                    Severity::Info,
                ),
                Err(error) => (
                    format!("{} automatic answer delivery unknown: {error}", dialog.name),
                    Severity::Warning,
                ),
            };
            return self.report_startup_dialog(&status, severity).await;
        }
        if !first_sighting {
            return Ok(());
        }
        let status = if truncated {
            format!("{} detected; screen truncated, not answered", dialog.name)
        } else {
            format!("{} needs a human; not answered", dialog.name)
        };
        self.report_startup_dialog(&status, Severity::Warning).await
    }

    async fn report_startup_dialog(&self, status: &str, severity: Severity) -> DriverResult<()> {
        self.emit(ObservationPayload::Lifecycle(Box::new(
            LifecyclePayload::Native(Box::new(NativeLifecycle {
                topic: LifecycleTopic::Diagnostic,
                native_name: "claude-onboarding".into(),
                native_id: Knowledge::Known {
                    value: self.target.clone(),
                },
                status: Knowledge::Known {
                    value: status.to_owned(),
                },
                related_ids: BTreeMap::new(),
                data_ref: None,
                severity,
                affects_completion: false,
            })),
        )))
        .await
    }

    async fn clear(
        &self,
        state: &mut State,
        next: InteractionState,
        reason: &str,
    ) -> DriverResult<()> {
        let Some(pending) = state.pending.as_mut() else {
            return Ok(());
        };
        let mut interaction = pending.interaction.clone();
        interaction.state = next;
        interaction.blocking = false;
        interaction.answerable = false;
        interaction.meta.revision.0 += 1;
        interaction.meta.updated_at = now_ts()?;
        interaction.delivery = if pending.interaction.delivery == DeliveryState::Unknown {
            DeliveryState::Unknown
        } else if pending.attempted {
            DeliveryState::Written
        } else {
            DeliveryState::NotSent
        };
        interaction.resolution = Knowledge::Known {
            value: InteractionResolution {
                reason: if next == InteractionState::Resolved {
                    InteractionResolutionReason::NativeCleared
                } else {
                    InteractionResolutionReason::GenerationEnded
                },
                event_ids: vec![],
            },
        };
        self.emit(ObservationPayload::Lifecycle(Box::new(
            LifecyclePayload::Entity(Box::new(EntityLifecycle {
                entity_id: interaction.meta.id.as_id().clone(),
                revision: interaction.meta.revision,
                previous_state: Some(
                    if pending.attempted {
                        "answer-committed"
                    } else {
                        "pending"
                    }
                    .into(),
                ),
                state: if next == InteractionState::Resolved {
                    "resolved"
                } else {
                    "invalidated"
                }
                .into(),
                reason_code: reason.into(),
                evidence_event_ids: vec![],
                entity_value: LifecycleEntity::Interaction(Box::new(interaction)),
            })),
        )))
        .await?;
        state.pending = None;
        Ok(())
    }

    /// Both `tty.write` and interaction replies use this exact key transport.
    ///
    /// c-ctxusage r27 item 1: the carrier connection is established FIRST and
    /// only then is revocation re-checked under the state mutex. The dispatch
    /// runs on that SAME held connection with no reconnect, so a close that
    /// revokes and a carrier restart that reuses this pane id while the request
    /// is parked between check and write can only make the pinned write fail
    /// (broken connection) — the keystroke is never dialled fresh onto the
    /// replacement foreign carrier.
    pub(crate) async fn send_keys(&self, keys: Vec<String>) -> DriverResult<DriverAck> {
        let mut conn = self.pinned().await?;
        let state = self.state.lock().await;
        if state.closed || self.revoked() {
            return Err(DriverError::ControlUnavailable);
        }
        // The guard stays held across the dispatch and `conn` was pinned
        // before this check: a concurrent close can only break the write,
        // never make it reach a new carrier.
        self.write(&mut conn, keys).await
    }

    /// Send keys on a connection the caller pinned before its revocation
    /// check. Never dials a fresh socket: a dropped pinned connection is a
    /// failed dispatch, not a transparent retry.
    async fn write(
        &self,
        conn: &mut PersistentClient,
        keys: Vec<String>,
    ) -> DriverResult<DriverAck> {
        #[cfg(any(test, feature = "test-stub"))]
        {
            // Take the gate (one park per arm), dropping the std MutexGuard
            // BEFORE any await (it is not Send).
            let gate = self.pre_dispatch_gate.lock().expect("gate lock").take();
            if let Some(gate) = gate {
                // Acknowledgement strictly AFTER the final check passed: the
                // test waits on this before revoking, so it can never mistake
                // an early ControlUnavailable for the post-check window.
                let _ = gate.arrived.send(());
                // Retained wait: a value sent on oneshot is held for a waiter
                // that has not parked yet (unlike Notify::notify_waiters).
                let _ = gate.release.await;
            }
        }
        conn.agent_send_keys(&self.target, keys)
            .await
            .map_err(map_herdr)?;
        Ok(DriverAck::transport_written())
    }

    pub(crate) async fn respond(
        &self,
        id: InteractionId,
        answer: InteractionAnswer,
    ) -> DriverResult<DriverAck> {
        // Pin the carrier boot before the mutex/check; the whole refresh and
        // the final answer dispatch ride this one connection.
        let mut conn = self.pinned().await?;
        let mut state = self.state.lock().await;
        if state.closed || self.revoked() {
            return Err(DriverError::ControlUnavailable);
        }
        self.refresh(&mut state, &mut conn).await?;
        let pending = state
            .pending
            .as_mut()
            .filter(|p| p.interaction.meta.id == id)
            .ok_or_else(|| invalid("PTY prompt cleared or changed"))?;
        if pending.attempted {
            return Err(invalid("PTY answer already attempted; do not replay"));
        }
        if !pending.interaction.answerable {
            return Err(invalid("PTY screen unavailable or truncated"));
        }
        let keys = answer_keys(pending, &answer)?;
        self.commit_answer(&mut conn, pending, keys).await
    }

    async fn commit_answer(
        &self,
        conn: &mut PersistentClient,
        pending: &mut Pending,
        keys: Vec<String>,
    ) -> DriverResult<DriverAck> {
        // Reachable only through `respond`, which pinned `conn` and checked
        // under the same lock acquisition before its refresh; re-check
        // immediately before the dispatch so a refresh-await in that critical
        // section cannot be the last gate. A restart in that window breaks the
        // pinned conn; the write then fails instead of reaching a reused pane.
        if self.revoked() {
            return Err(DriverError::ControlUnavailable);
        }
        // Mark before I/O: even a lost RPC ACK must never replay Enter.
        pending.attempted = true;
        let result = self.write(conn, keys).await;
        pending.interaction.delivery = if result.is_ok() {
            DeliveryState::Written
        } else {
            DeliveryState::Unknown
        };
        pending.interaction.state = InteractionState::AnswerCommitted;
        pending.interaction.answerable = false;
        pending.interaction.meta.revision.0 += 1;
        pending.interaction.meta.updated_at = now_ts()?;
        self.emit(ObservationPayload::Lifecycle(Box::new(
            LifecyclePayload::Entity(Box::new(EntityLifecycle {
                entity_id: pending.interaction.meta.id.as_id().clone(),
                revision: pending.interaction.meta.revision,
                previous_state: Some("pending".into()),
                state: "answer-committed".into(),
                reason_code: if result.is_ok() {
                    "tty-written"
                } else {
                    "tty-write-unknown"
                }
                .into(),
                evidence_event_ids: vec![],
                entity_value: LifecycleEntity::Interaction(Box::new(pending.interaction.clone())),
            })),
        )))
        .await?;
        result
    }

    pub(crate) async fn close(&self) -> DriverResult<()> {
        // Revoke controls FIRST, synchronously: a queued request must be
        // refused even when the state mutex is held by a stalled RPC and the
        // bounded cleanup below times out.
        self.revoke();
        let closing = async {
            let mut state = self.state.lock().await;
            state.closed = true;
            self.clear(
                &mut state,
                InteractionState::Invalidated,
                "generation-ended",
            )
            .await
        };
        // Match carrier shutdown: a detached/stalled optional observer cannot
        // prevent reclamation of the owned Herdr resources. The timeout only
        // bounds the OPTIONAL invalidation emission now — control revocation
        // already happened above and never waits on this lock.
        match tokio::time::timeout(Duration::from_millis(250), closing).await {
            Ok(result) if !self.tx.is_closed() => result,
            _ => Ok(()),
        }
    }
}

fn invalid(message: &str) -> DriverError {
    DriverError::InvalidLaunchSpec(message.into())
}

/// Prompt readiness is a pre-dispatch query. A busy/blocked pane is safe to queue.
pub(crate) async fn prompt_ready(client: &Client, target: &str) -> DriverResult<()> {
    let agent = client.agent_get(target).await.map_err(map_herdr)?.agent;
    if !matches!(agent.agent_status, AgentStatus::Idle | AgentStatus::Done)
        || !agent.interactive_ready
    {
        return Err(DriverError::ControlUnavailable);
    }
    Ok(())
}

/// The same query, plus the first-run screens Herdr reports as idle.
///
/// A wizard step is `idle` and `interactive_ready` to Herdr — it is a TUI at a
/// menu — so `prompt_ready` alone would let the D-022 queue type a prompt into
/// the theme picker.
pub(crate) async fn prompt_ready_for(
    client: &Client,
    target: &str,
    interactions: &PtyInteractions,
) -> DriverResult<()> {
    prompt_ready(client, target).await?;
    if interactions.startup_dialog_pending().await {
        return Err(DriverError::ControlUnavailable);
    }
    Ok(())
}

/// Only Claude's exact folder question with both known choices and one cursor.
///
/// Adapter over [`remuda_screen::trust_dialog_keys`]; the herdr screen arrives
/// already ANSI-stripped, so it becomes a raw grid unchanged.
fn trust_dialog_keys(screen: &str) -> Option<Vec<String>> {
    remuda_screen::trust_dialog_keys(&ScreenGrid::from_raw(screen))
}

fn answer_keys(pending: &Pending, answer: &InteractionAnswer) -> DriverResult<Vec<String>> {
    let selected = match (&pending.interaction.request, answer) {
        (InteractionRequest::Approval(request), InteractionAnswer::Approval(answer)) => {
            if request.input_digest != answer.input_digest {
                return Err(invalid("PTY input digest mismatch"));
            }
            answer.option_id.as_str()
        }
        (InteractionRequest::Question(request), InteractionAnswer::Question(answer)) => {
            let field = &request.fields[0];
            if answer.answers.len() != 1 {
                return Err(invalid("PTY reply requires exactly one field"));
            }
            let value = answer
                .answers
                .get(&field.id)
                .ok_or_else(|| invalid("unknown PTY question field"))?;
            if field.input == QuestionInput::Text {
                let text = value
                    .text
                    .as_deref()
                    .ok_or_else(|| invalid("PTY reply requires text"))?;
                if !value.option_ids.is_empty()
                    || text.is_empty()
                    || text.len() > ANSWER_BYTES
                    || text.chars().any(char::is_control)
                {
                    return Err(invalid(
                        "PTY reply must be a nonempty single line of at most 1024 bytes",
                    ));
                }
                let mut keys: Vec<_> = text.chars().map(|c| c.to_string()).collect();
                keys.push("enter".into());
                return Ok(keys);
            }
            if value.option_ids.len() != 1 || value.text.is_some() {
                return Err(invalid("PTY reply requires one option and no text"));
            }
            &value.option_ids[0]
        }
        _ => return Err(invalid("PTY answer kind mismatch")),
    };
    pending
        .keys
        .get(selected)
        .cloned()
        .ok_or_else(|| invalid("unknown PTY answer option"))
}

pub(crate) fn screen_request(
    ctx: &ObsCtx,
    screen: &str,
) -> DriverResult<(Interaction, BTreeMap<String, Vec<String>>)> {
    let parsed = remuda_screen::screen_request(&ScreenGrid::from_raw(screen));
    let excerpt = parsed.excerpt.clone();
    let keys: BTreeMap<String, Vec<String>> = parsed
        .choices
        .iter()
        .map(|choice| (choice.id.clone(), choice.keys.clone()))
        .collect();
    let request = if parsed.approval {
        InteractionRequest::Approval(Box::new(ApprovalRequest {
            title: "Terminal approval".into(),
            description: excerpt.clone(),
            tool_call_id: None,
            action_ref: Id::new("obj")?,
            requested_permissions_ref: None,
            input_digest: hash_bytes(screen.as_bytes())?,
            options: parsed
                .choices
                .iter()
                .map(|choice| {
                    Ok(DecisionOption {
                        id: choice.id.clone(),
                        label: choice.label.clone(),
                        effect: DecisionEffect::NativeSpecific,
                        native_value_ref: Id::new("obj")?,
                    })
                })
                .collect::<DriverResult<_>>()?,
        }))
    } else {
        InteractionRequest::Question(Box::new(QuestionRequest {
            title: "Terminal question".into(),
            fields: vec![QuestionField {
                id: "screen".into(),
                title: "Reply to the terminal prompt".into(),
                description: Some(excerpt.clone()),
                input: if parsed.free_text() {
                    QuestionInput::Text
                } else {
                    QuestionInput::SingleSelect
                },
                required: true,
                options: parsed
                    .choices
                    .iter()
                    .map(|choice| QuestionOption {
                        id: choice.id.clone(),
                        label: choice.label.clone(),
                        description: None,
                    })
                    .collect(),
                allow_free_text: parsed.free_text(),
                sensitive: false,
            }],
        }))
    };
    let ts = now_ts()?;
    Ok((
        Interaction {
            meta: EntityMeta {
                id: InteractionId::new(),
                revision: U64(1),
                created_at: ts.clone(),
                updated_at: ts,
            },
            instance_id: ctx.instance_id.clone(),
            host_id: ctx.host_id.clone(),
            run_id: Some(ctx.run_id.clone()),
            kind: if parsed.approval {
                InteractionKind::Approval
            } else {
                InteractionKind::Question
            },
            request_key: InteractionRequestKey {
                native: NativeRequestKey::None,
                process_generation: U64(1),
                run_generation: Some(U64(1)),
                connection_epoch: Id::new("epoch")?,
            },
            request_version: U64(1),
            state: InteractionState::Pending,
            blocking: true,
            // D-022: a truncated or ambiguous screen is never answerable — a
            // reply would be aimed at a prompt the human could not fully see.
            answerable: !parsed.truncated && !parsed.ambiguous && !excerpt.is_empty(),
            carrier: InteractionCarrier::NativeTty,
            request,
            deadline: Knowledge::Unknown {
                reason: "native-tty".into(),
                evidence_event_ids: vec![],
            },
            deadline_source: DeadlineSource::None,
            answer: Knowledge::Unknown {
                reason: "pending".into(),
                evidence_event_ids: vec![],
            },
            delivery: DeliveryState::NotSent,
            resolution: Knowledge::Unknown {
                reason: "pending".into(),
                evidence_event_ids: vec![],
            },
        },
        keys,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ctx() -> ObsCtx {
        crate::claude_pty::obs_ctx(
            DriverKind::GenericPty,
            InstanceId::new(),
            HostId::new(),
            Id::new("obj").unwrap(),
            RunId::new(),
            "fixture".into(),
            "fixture".into(),
        )
    }
    #[tokio::test]
    async fn closing_blocked_prompt_does_not_wait_for_optional_observer() {
        for detached in [false, true] {
            let (tx, rx) = mpsc::channel(1);
            let interactions = PtyInteractions::new(
                Client::connect("/tmp/unused-pty-interaction-test.sock"),
                "pane-fixture".into(),
                ctx(),
                tx,
                Arc::new(AtomicU64::new(0)),
                false,
            );
            let screen = "Continue? [y/n]";
            let (interaction, keys) = screen_request(&interactions.ctx, screen).unwrap();
            interactions
                .emit(ObservationPayload::InteractionRequested(Box::new(
                    InteractionRequestedPayload {
                        interaction: interaction.clone(),
                    },
                )))
                .await
                .unwrap();
            interactions.state.lock().await.pending = Some(Pending {
                interaction,
                screen: screen.into(),
                screen_truncated: false,
                state_change_seq: 1,
                keys,
                attempted: false,
            });
            let _observer = if detached {
                drop(rx);
                None
            } else {
                Some(rx)
            };
            tokio::time::timeout(Duration::from_secs(1), interactions.close())
                .await
                .expect("observer cannot block carrier reclamation")
                .unwrap();
            assert!(interactions.state.lock().await.closed);
            assert!(interactions.send_keys(vec!["enter".into()]).await.is_err());
        }
    }
    #[test]
    fn exact_trust_dialog_requires_unambiguous_selected_menu() {
        let title = "Quick safety check: Is this a project you created or one you trust?";
        assert_eq!(
            trust_dialog_keys(&format!("{title}\n❯ No, exit\nYes, I trust this folder")),
            Some(vec!["down".into(), "enter".into()])
        );
        assert_eq!(
            trust_dialog_keys(&format!("{title}\n❯ Yes, I trust this folder\nNo, exit")),
            Some(vec!["enter".into()])
        );
        for screen in [
            "Do you trust this command?\n❯ No, exit\nYes, I trust this folder".to_owned(),
            format!("{title}\nNo, exit\nYes, I trust this folder"),
            format!("{title}\n❯ No, exit\n❯ Yes, I trust this folder"),
            format!("{title}\n❯ No, exit\nYes, I trust this folder\nNo, exit"),
        ] {
            assert!(trust_dialog_keys(&screen).is_none());
        }
    }

    #[test]
    fn bounded_excerpt_is_utf8_safe_and_disables_truncated_reply() {
        assert!(
            !screen_request(&ctx(), "Choose:\n1. First\n1. Duplicate")
                .unwrap()
                .0
                .answerable
        );
        let screen = "界".repeat(3000);
        let (interaction, _) = screen_request(&ctx(), &screen).unwrap();
        assert!(!interaction.answerable);
        let InteractionRequest::Question(request) = interaction.request else {
            panic!()
        };
        assert!(request.fields[0].description.as_ref().unwrap().len() <= SCREEN_BYTES);
    }
    #[test]
    fn exact_candidates_and_control_input_validation() {
        let (interaction, keys) =
            screen_request(&ctx(), "Select environment:\n❯ 1. Development\n2. Staging").unwrap();
        assert_eq!(keys["1"], ["enter"]);
        assert_eq!(keys["2"], ["down", "enter"]);
        assert_eq!(interaction.kind, InteractionKind::Question);
        let (interaction, keys) = screen_request(&ctx(), "Type a project name:").unwrap();
        let pending = Pending {
            interaction,
            screen: String::new(),
            screen_truncated: false,
            state_change_seq: 1,
            keys,
            attempted: false,
        };
        let answer = InteractionAnswer::Question(Box::new(QuestionAnswer {
            answers: BTreeMap::from([(
                "screen".into(),
                QuestionFieldAnswer {
                    option_ids: vec![],
                    text: Some("bad\ncommand".into()),
                },
            )]),
        }));
        assert!(answer_keys(&pending, &answer).is_err());
    }

    /// c-ctxusage r26 item 1: a control that passed the driver entry gate can
    /// still be QUEUED on the interaction state mutex behind request A, which
    /// holds that mutex across a stalled Herdr RPC. Close begins (carrier
    /// restart + pane reuse follow); its bounded cleanup cannot acquire the
    /// mutex within 250 ms. The queued control must nevertheless be revoked
    /// synchronously and reach ZERO dispatch — never dial the Herdr the
    /// restarted carrier reuses the pane under.
    #[tokio::test]
    async fn close_revokes_a_queued_key_control_behind_a_stalled_rpc() {
        use tokio::io::AsyncReadExt;
        use tokio::net::UnixListener;

        // Witness "Herdr": accepts connections, counts every byte received,
        // never replies — a permanently stalled RPC, exactly like the carrier
        // that is restarting.
        let dir = tempfile::Builder::new()
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .expect("in-crate temp dir");
        let socket = dir.path().join("parked-herdr.sock");
        let listener = UnixListener::bind(&socket).expect("bind parked herdr");
        let observed_bytes = Arc::new(AtomicU64::new(0));
        let server = {
            let observed_bytes = observed_bytes.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut conn, _)) = listener.accept().await else {
                        break;
                    };
                    let observed_bytes = observed_bytes.clone();
                    tokio::spawn(async move {
                        let mut buf = [0u8; 4096];
                        loop {
                            match conn.read(&mut buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    observed_bytes.fetch_add(n as u64, Ordering::SeqCst);
                                }
                            }
                        }
                    });
                }
            })
        };

        let (tx, _rx) = mpsc::channel(4);
        let interactions: Arc<PtyInteractions> = PtyInteractions::new(
            // 2 s RPC timeout keeps the RED run bounded when the queued
            // control wrongly dials this listener.
            Client::connect(&socket).with_timeout(Duration::from_secs(2)),
            "pane-reused-after-restart".into(),
            ctx(),
            tx,
            Arc::new(AtomicU64::new(0)),
            false,
        );

        // Request A: holds the interaction state lock across its stalled RPC.
        let release_a = Arc::new(tokio::sync::Notify::new());
        let a_release = release_a.clone();
        let a_interactions = interactions.clone();
        let a = tokio::spawn(async move {
            let _guard = a_interactions.state.lock().await;
            a_release.notified().await;
        });

        // Request B: already past the driver entry gate, now queued on the
        // same mutex behind A.
        let b_interactions = interactions.clone();
        let b = tokio::spawn(async move { b_interactions.send_keys(vec!["enter".into()]).await });
        // Let B park on the mutex before close begins.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Close starts WHILE A still holds the lock: revocation must not wait.
        tokio::time::timeout(Duration::from_secs(5), interactions.close())
            .await
            .expect("close itself stays bounded")
            .expect("close reports ok");
        assert!(
            interactions.closing.load(Ordering::SeqCst),
            "the synchronous per-run closing latch is set even with the mutex held"
        );

        // A's RPC fails (carrier restart); release the mutex so B reaches the
        // front of the queue.
        release_a.notify_waiters();
        a.await.expect("stalled request task");
        let result = tokio::time::timeout(Duration::from_secs(10), b)
            .await
            .expect("queued control resolves")
            .expect("queued control task");
        assert!(
            matches!(result, Err(DriverError::ControlUnavailable)),
            "the queued control is refused, got {result:?}"
        );
        // Give a buggy build a beat to have dialled after acquiring the mutex.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            observed_bytes.load(Ordering::SeqCst),
            0,
            "zero agent.send_keys dispatch reaches the reused pane"
        );
        server.abort();
    }

    /// c-ctxusage r27 item 1: the r26 window the mutex test cannot reach. A
    /// request passes the post-mutex revocation check and is parked BEFORE the
    /// transport write while holding the state mutex. Close revokes, the
    /// carrier RESTARTS on the same socket path and reuses the pane id, and
    /// the request resumes. The dispatch is pinned to the ORIGINAL carrier's
    /// connection (established before the check), so the write must fail on
    /// the broken connection — zero bytes may reach the replacement carrier.
    #[tokio::test]
    async fn a_revoked_request_parked_after_check_cannot_reach_a_reused_carrier() {
        use tokio::io::AsyncReadExt;
        use tokio::net::UnixListener;

        let dir = tempfile::Builder::new()
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .expect("in-crate temp dir");
        let socket = dir.path().join("restarted-herdr.sock");

        // Original carrier: accept exactly the pinned connection and HOLD it
        // until the test orders the shutdown (the restart moment).
        let old_listener = UnixListener::bind(&socket).expect("bind original carrier");
        let old_shutdown = Arc::new(tokio::sync::Notify::new());
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let old_shutdown_task = {
            let old_shutdown = old_shutdown.clone();
            tokio::spawn(async move {
                let (mut stream, _) = old_listener.accept().await.expect("pinned connect");
                accepted_tx.send(()).expect("witness waits");
                // Drain nothing; just keep the pinned connection alive until
                // the simulated restart, then drop it (and the listener).
                old_shutdown.notified().await;
                let mut buf = [0u8; 4096];
                let _ =
                    tokio::time::timeout(Duration::from_millis(100), stream.read(&mut buf)).await;
            })
        };

        let (tx, _rx) = mpsc::channel(4);
        let interactions: Arc<PtyInteractions> = PtyInteractions::new(
            Client::connect(&socket).with_timeout(Duration::from_secs(2)),
            "pane-id-reused-after-restart".into(),
            ctx(),
            tx,
            Arc::new(AtomicU64::new(0)),
            false,
        );
        // Park point: after check, before the pinned write. The arrival
        // acknowledgement is fired INSIDE the seam strictly after the final
        // check, so waiting on it deterministically proves the request reached
        // the race window (no sleep-based guess).
        // Sender kept until the release moment; the seam holds the receiver.
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel();
        interactions.arm_pre_dispatch_gate(DispatchGate {
            arrived: arrived_tx,
            release: release_rx,
        });

        // The request: connects to the ORIGINAL carrier, passes the check,
        // then parks holding the state mutex.
        let request_interactions = interactions.clone();
        let request =
            tokio::spawn(async move { request_interactions.send_keys(vec!["enter".into()]).await });
        // Wait until the pinned connection was established AND the request has
        // passed its final revocation check and is parked at the seam.
        tokio::time::timeout(Duration::from_secs(5), accepted_rx)
            .await
            .expect("pinned connection never established")
            .expect("accept oneshot");
        tokio::time::timeout(Duration::from_secs(5), arrived_rx)
            .await
            .expect("dispatch never reached the post-check park point")
            .expect("arrival oneshot");

        // Close revokes WHILE the request is parked.
        interactions.revoke();

        // Carrier restart: kill the original boot (drops listener + pinned
        // connection), then serve the SAME socket path as the replacement,
        // counting every byte it receives. The pane id it reuses is the one
        // in the request target.
        old_shutdown.notify_waiters();
        old_shutdown_task
            .await
            .expect("original carrier shutdown task");
        // A dead Unix listener leaves its socket path behind; the replacement
        // boot (like a fresh herdr) removes the stale path before rebinding.
        let _ = std::fs::remove_file(&socket);
        let replacement = UnixListener::bind(&socket).expect("replacement carrier rebinds");
        let replacement_bytes = Arc::new(AtomicU64::new(0));
        let replacement_task = {
            let replacement_bytes = replacement_bytes.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut conn, _)) = replacement.accept().await else {
                        break;
                    };
                    let replacement_bytes = replacement_bytes.clone();
                    tokio::spawn(async move {
                        let mut buf = [0u8; 4096];
                        loop {
                            match conn.read(&mut buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    replacement_bytes.fetch_add(n as u64, Ordering::SeqCst);
                                }
                            }
                        }
                    });
                }
            })
        };

        // Release the parked request: its pinned connection is the dead
        // original boot's, so the write must fail with a TRANSPORT error —
        // never the local ControlUnavailable, which would mean it was revoked
        // before the check and never exercised this window. The retained
        // oneshot release is not lost if the request has not parked yet (a
        // non-retaining notify_waiters would race the release away).
        release_tx.send(()).expect("release receiver is alive");
        let result = tokio::time::timeout(Duration::from_secs(10), request)
            .await
            .expect("dispatch resolves")
            .expect("dispatch task");
        let err = match result {
            Err(err) => err,
            Ok(ack) => panic!("the broken pinned write must fail, got Ok({ack:?})"),
        };
        assert!(
            !matches!(err, DriverError::ControlUnavailable),
            "an early ControlUnavailable means the request never reached the \
             post-check window, got {err:?}"
        );
        // Positive assertion: the mapped failure is a BROKEN TRANSPORT on the
        // pinned connection (write to the half-closed socket), proving the
        // write was attempted against the dead original carrier rather than
        // refused before dispatch or retried onto the replacement boot.
        let broken_transport = matches!(
            &err,
            DriverError::Io(io) if matches!(
                io.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionAborted
            )
        );
        assert!(
            broken_transport,
            "expected a broken-transport error from the pinned connection, \
             got {err:?}"
        );

        // Give a buggy (reconnecting) build a beat to dial the replacement.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            replacement_bytes.load(Ordering::SeqCst),
            0,
            "zero bytes reach the replacement carrier's reused pane"
        );
        replacement_task.abort();
    }
}
