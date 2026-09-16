//! Claude permission modes across the launch argv and the live session.
//!
//! Two layers, one module — the same split as [`crate::effort`]:
//!
//! * **Vocabulary** — Claude Code 2.1.273 exposes six `--permission-mode`
//!   values (`manual`, `acceptEdits`, `plan`, `auto`, `bypassPermissions`,
//!   `dontAsk`) but only four are in the live shift+tab wheel:
//!   `manual → acceptEdits → plan → auto → manual`. When the session was
//!   launched with bypass permitted, `bypassPermissions` joins the wheel
//!   between `plan` and `auto`. `dontAsk` never joins — one shift+tab exits
//!   it back to `manual` — and a session launched without the bypass flag
//!   cannot cycle into `bypassPermissions` at all. Both are launch-only, and
//!   the UI greys them for a live session. Measurements:
//!   `docs/design/evidence/permission-modes-1.md`.
//! * **Live two-way sync** — below the vocabulary section: Remuda → Claude
//!   sends shift+tab (`ESC [ Z`) and reads the mode back from the TUI status
//!   line (`⏸ manual mode on`, `⏵⏵ accept edits on`, `plan mode on`,
//!   `auto mode on`, `bypass permissions on`, `don't ask on`); Claude →
//!   Remuda feeds the transcript's `permission-mode` records through
//!   [`remuda_protocol::LivePermissionTracker`] in the protocol crate.
//!
//! Unlike `/effort`, shift+tab is a global key binding: it is honoured while
//! a turn is streaming (measured ~100 ms repaint). The driver still queues
//! switches until the composer is idle, same as effort, so the serialization
//! contract and the "nothing typed into a modal" guarantee are identical.

use crate::error::DriverResult;
use remuda_protocol::{ClaudePermissionMode, Severity};
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering as AtomicOrdering},
};
use tokio::sync::{Notify, oneshot};

// ───────────────────────────── Vocabulary ────────────────────────────────────

/// Protocol wire spelling of a mode. The TUI/transcript spell `Manual` as
/// `default`; [`from_native`] accepts both.
pub(crate) fn wire_word(mode: ClaudePermissionMode) -> &'static str {
    match mode {
        ClaudePermissionMode::Manual => "manual",
        ClaudePermissionMode::Auto => "auto",
        ClaudePermissionMode::AcceptEdits => "acceptEdits",
        ClaudePermissionMode::DontAsk => "dontAsk",
        ClaudePermissionMode::Plan => "plan",
        ClaudePermissionMode::BypassPermissions => "bypassPermissions",
    }
}

/// Parse a mode word off the wire, launch flag, transcript record, or TUI
/// status vocabulary. Protocol vocabulary (`default` folds to `manual`).
pub(crate) fn from_native(word: &str) -> Option<ClaudePermissionMode> {
    remuda_protocol::normalize_permission_word(word)
}

/// The exact settled status-line phrases, measured on claude 2.1.273.
///
/// For ~4 s after a cycle the same phrases carry a
/// `(shift+tab to cycle)` hint; both forms contain the phrase here.
pub(crate) const INDICATORS: &[(ClaudePermissionMode, &str)] = &[
    (ClaudePermissionMode::Manual, "manual mode on"),
    (ClaudePermissionMode::AcceptEdits, "accept edits on"),
    (ClaudePermissionMode::Plan, "plan mode on"),
    (ClaudePermissionMode::Auto, "auto mode on"),
    (
        ClaudePermissionMode::BypassPermissions,
        "bypass permissions on",
    ),
    (ClaudePermissionMode::DontAsk, "don't ask on"),
];

/// Read the current mode off a rendered screen.
///
/// Whitespace is collapsed first: the TUI repaints under streaming leave the
/// phrase split across cells (`accept edits(shift+tab …)`). When transient
/// toast history is stacked above the settled line, several phrases can be
/// present at once — the **last** one wins, the settled status line being
/// painted last.
pub(crate) fn parse_indicator(text: &str) -> Option<ClaudePermissionMode> {
    fn squash(s: &str) -> String {
        s.chars().filter(|ch| !ch.is_whitespace()).collect()
    }
    let screen = squash(text);
    INDICATORS
        .iter()
        .filter_map(|(mode, phrase)| screen.rfind(&squash(phrase)).map(|pos| (pos, *mode)))
        .max_by_key(|(pos, _)| *pos)
        .map(|(_, mode)| mode)
}

/// Whether `target` can be reached by live cycling from this session.
///
/// `dontAsk` is never reachable (one press exits it). `bypassPermissions` is
/// reachable only when the launch carried the bypass allowance.
pub(crate) fn live_reachable(
    target: ClaudePermissionMode,
    bypass_allowed: bool,
) -> bool {
    match target {
        ClaudePermissionMode::DontAsk => false,
        ClaudePermissionMode::BypassPermissions => bypass_allowed,
        _ => true,
    }
}

/// The mode the native wheel lands on after one shift+tab, given availability.
///
/// This is the measured 2.1.273 transition table (`EJe` in the TUI bundle):
/// `default→acceptEdits→plan→(bypass?)→(auto?)→default`, `dontAsk→default`.
#[cfg(test)]
fn next_in_wheel(
    current: ClaudePermissionMode,
    auto_available: bool,
    bypass_available: bool,
) -> ClaudePermissionMode {
    use ClaudePermissionMode::*;
    match current {
        Manual => AcceptEdits,
        AcceptEdits => Plan,
        // Measured (`EJe`): dontAsk leaves the wheel straight back to manual.
        DontAsk => Manual,
        Plan => {
            if bypass_available {
                BypassPermissions
            } else if auto_available {
                Auto
            } else {
                Manual
            }
        }
        BypassPermissions => {
            if auto_available {
                Auto
            } else {
                Manual
            }
        }
        Auto => Manual,
    }
}

/// Number of shift+tab presses to walk `current → target` along the wheel.
///
/// `None` when the target is not in this session's wheel. `Some(0)` means the
/// target is already in effect.
#[cfg(test)]
pub(crate) fn cycle_steps(
    current: ClaudePermissionMode,
    target: ClaudePermissionMode,
    auto_available: bool,
    bypass_allowed: bool,
) -> Option<usize> {
    if !live_reachable(target, bypass_allowed) {
        return None;
    }
    let mut mode = current;
    for presses in 0..=WHEEL_LEN_MAX {
        if mode == target {
            return Some(presses);
        }
        mode = next_in_wheel(mode, auto_available, bypass_allowed);
    }
    None
}

/// Upper bound on a wheel walk (including a `dontAsk` exit press).
#[cfg(test)]
const WHEEL_LEN_MAX: usize = 6;

// ───────────────────────────── Live read-back ────────────────────────────────

/// Bounded wait for the post-cycle status-line read-back.
///
/// The status line repaints ~80–110 ms after a shift+tab (measured); this
/// window only bounds the synchronous idle-path configure call against a
/// wedged TUI. A switch made while the agent is working is queued instead.
pub(crate) const PERMISSION_READBACK_TIMEOUT_MS: u64 = 8_000;
/// Poll cadence while waiting for the status-line repaint.
pub(crate) const PERMISSION_SCREEN_POLL_MS: u64 = 70;
/// Settle pause after a shift+tab write and after the gated confirm CR.
pub(crate) const PERMISSION_WRITE_SETTLE_MS: u64 = 120;
/// The indicator must survive two consecutive polls this far apart to count
/// as settled (streaming repaints transiently scramble the bottom rows).
pub(crate) const PERMISSION_STABLE_GAP_MS: u64 = 80;
/// How long one step waits for its mode to paint before failing the walk.
pub(crate) const PERMISSION_STEP_TIMEOUT_MS: u64 = 2_500;
/// At most this many shift+tab presses in one closed-loop walk. The wheel is
/// five modes long; six covers a `dontAsk` exit press plus a full lap.
pub(crate) const PERMISSION_MAX_PRESSES: usize = 6;

/// Collapsed-whitespace needles for the bypass disclaimer modal. Both must be
/// present; the modal's default selection is **"No, exit"**, so confirmation
/// is DOWN then Enter — never a bare Enter.
pub(crate) const BYPASS_DIALOG_TITLE: &str = "bypasspermissionsmode";
pub(crate) const BYPASS_DIALOG_ACCEPT: &str = "yes,iaccept";

/// What a switch was asked to change to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PermissionRequest {
    pub(crate) mode: ClaudePermissionMode,
}

impl PermissionRequest {
    pub(crate) fn parse(word: &str) -> Option<Self> {
        from_native(word).map(|mode| Self { mode })
    }

    pub(crate) fn word(&self) -> &'static str {
        wire_word(self.mode)
    }
}

/// Terminal state of a switch attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwitchOutcome {
    /// The status line now shows the requested mode.
    Applied,
    /// The agent was working; the change is held for the next idle moment.
    Queued,
    /// Keystrokes went out but the status line never confirmed the target;
    /// the effective mode stays wherever the wheel actually landed.
    Degraded,
    /// The requested mode is launch-only for this session.
    Unsupported,
    /// Control was not available (closed pane, booting TUI, modal, …).
    ControlUnavailable,
}

impl SwitchOutcome {
    /// Stable journal status string for the `instance.configure` lifecycle.
    pub(crate) fn journal_status(self, word: &str, detail: &str) -> String {
        match self {
            Self::Applied => format!("permission-applied:{word}"),
            Self::Queued => format!("permission-queued:{word}"),
            Self::Degraded => format!("permission-degraded:{word}:{detail}"),
            Self::Unsupported => format!("permission-unsupported-in-session:{word}"),
            Self::ControlUnavailable => format!("permission-control-unavailable:{detail}"),
        }
    }
}

/// Terminal verdict of a switch's read-back wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Readback {
    /// The status line shows the requested mode; this is what is in effect.
    Applied(ClaudePermissionMode),
    /// The walk was abandoned (dialog dismissed, indicator never settled).
    Rejected {
        /// Stable reason code (`bypass-dialog-kept`, `no-status-line`,
        /// `landed-other`).
        reason: String,
    },
}

/// Coordination between the driver's switch call, the closed-loop key worker,
/// and the transcript pump. Mirrors [`crate::effort::EffortBridge`].
pub(crate) struct PermissionBridge {
    state: Mutex<BridgeState>,
    notify: Notify,
}

#[derive(Default)]
struct BridgeState {
    /// A switch waiting for its status-line / transcript verdict.
    pending: Option<(u64, ClaudePermissionMode)>,
    /// Generation whose verdict has arrived.
    verdict_gen: u64,
    verdict: Option<Readback>,
    /// Sticky last Remuda-armed `(generation, target)`, used by the
    /// transcript pump to attribute a lazily-written `permission-mode` record
    /// after the status line already settled the switch.
    armed: Option<(u64, ClaudePermissionMode)>,
    /// Latest mode observed from either read-back channel.
    observed: Option<ClaudePermissionMode>,
    /// Whether this launch can cycle into bypass.
    bypass_allowed: bool,
    generation: u64,
}

impl PermissionBridge {
    pub(crate) fn new(bypass_allowed: bool) -> Self {
        Self {
            state: Mutex::new(BridgeState {
                bypass_allowed,
                ..BridgeState::default()
            }),
            notify: Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, BridgeState> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Arm a new Remuda switch; returns its generation token.
    pub(crate) fn arm(&self, mode: ClaudePermissionMode) -> u64 {
        let mut state = self.lock();
        state.generation += 1;
        let generation = state.generation;
        state.pending = Some((generation, mode));
        state.armed = Some((generation, mode));
        generation
    }

    pub(crate) fn pending(&self) -> Option<ClaudePermissionMode> {
        self.lock().pending.map(|(_, mode)| mode)
    }

    pub(crate) fn pending_with_gen(&self) -> Option<(u64, ClaudePermissionMode)> {
        self.lock().pending
    }

    /// Sticky `(generation, target)` of the last Remuda switch, for lazy
    /// transcript attribution.
    pub(crate) fn armed(&self) -> Option<(u64, ClaudePermissionMode)> {
        self.lock().armed
    }

    pub(crate) fn bypass_allowed(&self) -> bool {
        self.lock().bypass_allowed
    }

    /// Latest observed effective mode, from either read-back channel.
    pub(crate) fn observed(&self) -> Option<ClaudePermissionMode> {
        self.lock().observed
    }

    /// Record a mode observed on a read-back channel (status line or
    /// transcript). Returns true when this is a new edge.
    pub(crate) fn note_observed(&self, mode: ClaudePermissionMode) -> bool {
        self.lock().observed.replace(mode) != Some(mode)
    }

    /// Record the launch-time mode before any switch happens.
    pub(crate) fn note_launch_mode(&self, mode: ClaudePermissionMode) {
        let mut state = self.lock();
        if state.observed.is_none() {
            state.observed = Some(mode);
        }
    }

    /// Read-back side: the requested mode is now showing.
    pub(crate) fn resolve(&self, generation: u64, mode: ClaudePermissionMode) {
        {
            let mut state = self.lock();
            if let Some((pending_gen, _)) = state.pending
                && pending_gen == generation
            {
                state.pending = None;
            }
            state.observed = Some(mode);
            state.verdict = Some(Readback::Applied(mode));
            state.verdict_gen = generation;
        }
        self.notify.notify_waiters();
    }

    /// Read-back side: the walk failed (dialog dismissed, never settled).
    pub(crate) fn reject(&self, generation: u64, reason: impl Into<String>) {
        {
            let mut state = self.lock();
            if let Some((pending_gen, _)) = state.pending
                && pending_gen == generation
            {
                state.pending = None;
            }
            state.verdict = Some(Readback::Rejected { reason: reason.into() });
            state.verdict_gen = generation;
        }
        self.notify.notify_waiters();
    }

    /// Give up on a generation without a verdict (bounded timeout).
    pub(crate) fn fail(&self, generation: u64) {
        {
            let mut state = self.lock();
            if let Some((pending_gen, _)) = state.pending
                && pending_gen == generation
            {
                state.pending = None;
            }
        }
        self.notify.notify_waiters();
    }

    /// Wait until generation `generation` gets a verdict, bounded by timeout.
    /// Kept for parity with the effort rendezvous and unit tests; the wheel
    /// worker settles synchronously off the screen and consumes no verdict.
    #[allow(dead_code)]
    pub(crate) async fn wait(
        &self,
        generation: u64,
        timeout: std::time::Duration,
    ) -> Option<Readback> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            {
                let state = self.lock();
                if state.verdict_gen == generation {
                    return state.verdict.clone();
                }
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            tokio::select! {
                () = &mut notified => {}
                () = tokio::time::sleep(remaining) => return None,
            }
        }
    }
}

fn squash_ascii_lower(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// True while the bypass disclaimer modal is the **active** overlay.
///
/// The modal's text stays in scrollback after it is accepted, so matching the
/// words alone would fire forever. The active modal's last painted line is its
/// confirm hint (`Enter to confirm · Esc to cancel`); after acceptance the
/// settled status line paints below it.
pub(crate) fn bypass_dialog_visible(screen: &str) -> bool {
    let collapsed = squash_ascii_lower(screen);
    if !collapsed.contains(BYPASS_DIALOG_TITLE) || !collapsed.contains(BYPASS_DIALOG_ACCEPT) {
        return false;
    }
    let Some(confirm_pos) = collapsed.rfind("entertoconfirm") else {
        // No footer hint: cannot be the active modal (it is the only surface
        // that paints "Enter to confirm · Esc to cancel").
        return false;
    };
    // An indicator painted below the hint means the modal is already gone.
    let latest_indicator = INDICATORS
        .iter()
        .map(|(_, phrase)| squash_ascii_lower(phrase))
        .filter_map(|phrase| collapsed.rfind(&phrase))
        .max();
    match latest_indicator {
        Some(pos) => pos < confirm_pos,
        None => true,
    }
}

/// The driver-side I/O a permission switch needs. One adapter per PTY carrier
/// (Herdr pane, local PTY).
#[async_trait::async_trait]
pub(crate) trait PermissionSwitchIo: Send + Sync {
    /// True when the native composer can accept the cycling keys right now.
    async fn is_idle(&self) -> bool;

    /// Send one shift+tab (`ESC [ Z`) as a single write.
    async fn send_cycle(&self) -> DriverResult<()>;

    /// Send the DOWN arrow (bypass modal defaults to "No, exit").
    async fn press_down(&self) -> DriverResult<()>;

    /// Send Enter as its own write.
    async fn press_enter(&self) -> DriverResult<()>;

    /// Send Esc (cancel a modal without accepting it).
    async fn press_esc(&self) -> DriverResult<()>;

    /// Current visible screen text, ANSI stripped.
    async fn screen_text(&self) -> DriverResult<String>;

    /// Journal an `instance.configure` permission lifecycle from the worker.
    async fn journal(&self, status: String, severity: Severity);
}

/// Read the indicator twice, a small gap apart, so a streaming repaint cannot
/// be mistaken for a settled mode.
async fn stable_indicator(io: &dyn PermissionSwitchIo) -> Option<ClaudePermissionMode> {
    let first = parse_indicator(&io.screen_text().await.ok()?);
    tokio::time::sleep(std::time::Duration::from_millis(
        PERMISSION_STABLE_GAP_MS,
    ))
    .await;
    let second = parse_indicator(&io.screen_text().await.ok()?);
    first.filter(|mode| Some(*mode) == second)
}

/// Walk the wheel one closed-loop step at a time.
///
/// Each shift+tab is followed by a bounded wait for its resulting indicator,
/// so the walk does not depend on knowing the exact starting mode and adapts
/// to a wheel without `auto` or `bypass`. Entering bypass can open the
/// disclaimer modal, whose default is "No, exit": the accept is DOWN then
/// Enter, and only while the modal is provably on screen.
pub(crate) async fn perform_switch(
    request: PermissionRequest,
    bridge: &PermissionBridge,
    io: &dyn PermissionSwitchIo,
) -> SwitchOutcome {
    let word = request.word();

    // Launch-only targets never produce a keystroke.
    if !live_reachable(request.mode, bridge.bypass_allowed()) {
        io.journal(
            SwitchOutcome::Unsupported.journal_status(word, ""),
            Severity::Warning,
        )
        .await;
        return SwitchOutcome::Unsupported;
    }

    // Already showing the target (stable read) — nothing to press. Still arm
    // + resolve the rendezvous and journal applied: no keystroke means no new
    // transcript edge, so the caller's optimistic pending needs this verdict
    // to settle.
    if bridge.observed() == Some(request.mode)
        && stable_indicator(io).await == Some(request.mode)
    {
        let generation = bridge.arm(request.mode);
        bridge.resolve(generation, request.mode);
        io.journal(SwitchOutcome::Applied.journal_status(word, ""), Severity::Info)
            .await;
        return SwitchOutcome::Applied;
    }

    let generation = bridge.arm(request.mode);

    // One closed-loop walk. The disclaimer can appear at most once: after
    // DOWN+Enter it is gone for the session, so remember we confirmed.
    let mut dialog_confirmed = false;
    let mut dialog_ever_seen = false;
    for _press in 0..PERMISSION_MAX_PRESSES {
        if let Err(error) = io.send_cycle().await {
            tracing::warn!(%error, "permission switch: cycle write failed");
            bridge.fail(generation);
            io.journal(
                SwitchOutcome::ControlUnavailable.journal_status(word, &error.to_string()),
                Severity::Error,
            )
            .await;
            return SwitchOutcome::ControlUnavailable;
        }

        // Wait for the next settled indicator, gating a bypass modal if it
        // opens on this step.
        let step_deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(PERMISSION_STEP_TIMEOUT_MS);
        let mut landed = None;
        while std::time::Instant::now() < step_deadline {
            tokio::time::sleep(std::time::Duration::from_millis(
                PERMISSION_SCREEN_POLL_MS,
            ))
            .await;
            let Ok(screen) = io.screen_text().await else {
                continue;
            };
            if bypass_dialog_visible(&screen) {
                dialog_ever_seen = true;
                if !dialog_confirmed {
                    // CR-gated, two-key accept: the modal defaults to
                    // "No, exit", which exits Claude on a bare Enter.
                    tokio::time::sleep(std::time::Duration::from_millis(
                        PERMISSION_WRITE_SETTLE_MS,
                    ))
                    .await;
                    if io.press_down().await.is_ok() {
                        tokio::time::sleep(std::time::Duration::from_millis(
                            PERMISSION_WRITE_SETTLE_MS,
                        ))
                        .await;
                        if io.press_enter().await.is_ok() {
                            dialog_confirmed = true;
                        }
                    }
                }
                continue;
            }
            landed = stable_indicator(io).await;
            if landed.is_some() {
                break;
            }
        }

        if landed == Some(request.mode) {
            bridge.resolve(generation, request.mode);
            return SwitchOutcome::Applied;
        }
    }

    // Never claim applied without the status line. If the modal is still up,
    // cancel it (Esc is its documented cancel) so the session is left clean.
    let reason = match io.screen_text().await {
        Ok(screen) if bypass_dialog_visible(&screen) => {
            let _ = io.press_esc().await;
            "bypass-dialog-kept"
        }
        Ok(screen) => match parse_indicator(&screen) {
            Some(_) if dialog_ever_seen && !dialog_confirmed => "bypass-dialog-kept",
            Some(other) => {
                bridge.note_observed(other);
                "landed-other"
            }
            None => "no-status-line",
        },
        Err(_) => "screen-unreadable",
    };
    finish_degraded(bridge, io, generation, word, reason).await
}

async fn finish_degraded(
    bridge: &PermissionBridge,
    io: &dyn PermissionSwitchIo,
    generation: u64,
    word: &str,
    reason: &str,
) -> SwitchOutcome {
    bridge.reject(generation, reason);
    io.journal(
        SwitchOutcome::Degraded.journal_status(word, reason),
        Severity::Warning,
    )
    .await;
    SwitchOutcome::Degraded
}

/// A switch held for the next idle moment (same ready ladder as effort).
struct QueuedSwitch {
    request: PermissionRequest,
    done: Option<oneshot::Sender<SwitchOutcome>>,
}

/// Queue of at most one pending switch (a newer switch replaces an older one).
pub(crate) struct PermissionQueue {
    pending: Mutex<Option<QueuedSwitch>>,
    notify: Notify,
    closed: AtomicBool,
}

impl PermissionQueue {
    pub(crate) fn new() -> Self {
        Self {
            pending: Mutex::new(None),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
        }
    }

    pub(crate) fn enqueue(
        &self,
        request: PermissionRequest,
        done: Option<oneshot::Sender<SwitchOutcome>>,
    ) {
        if let Ok(mut pending) = self.pending.lock() {
            *pending = Some(QueuedSwitch { request, done });
        }
        self.notify.notify_one();
    }

    fn take(&self) -> Option<QueuedSwitch> {
        self.pending.lock().ok().and_then(|mut guard| guard.take())
    }

    fn has_replacements(&self) -> bool {
        self.pending
            .lock()
            .map(|p| p.is_some())
            .unwrap_or(false)
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, AtomicOrdering::SeqCst);
        self.notify.notify_waiters();
    }
}

/// Spawn the worker that applies queued switches once the composer is idle.
/// Identical contract to [`crate::effort::spawn_worker`].
pub(crate) fn spawn_worker(
    bridge: Arc<PermissionBridge>,
    queue: Arc<PermissionQueue>,
    io: Arc<dyn PermissionSwitchIo>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while !queue.closed.load(AtomicOrdering::SeqCst) {
            let notified = queue.notify.notified();
            let queued = queue.take();
            match queued {
                Some(QueuedSwitch { request, done }) => {
                    let word = request.word();
                    let mut replaced = false;
                    while !io.is_idle().await {
                        if queue.closed.load(AtomicOrdering::SeqCst) {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(
                            PERMISSION_SCREEN_POLL_MS,
                        ))
                        .await;
                        if queue.has_replacements() {
                            replaced = true;
                            break;
                        }
                    }
                    if replaced || queue.has_replacements() {
                        queue.notify.notify_one();
                        if let Some(done) = done {
                            let _ = done.send(SwitchOutcome::Queued);
                        }
                        continue;
                    }
                    let outcome = perform_switch(request, &bridge, io.as_ref()).await;
                    match done {
                        Some(done) => {
                            let _ = done.send(outcome);
                        }
                        None => {
                            let severity = match outcome {
                                SwitchOutcome::Applied | SwitchOutcome::Queued => {
                                    Severity::Info
                                }
                                SwitchOutcome::Unsupported => Severity::Warning,
                                SwitchOutcome::Degraded => Severity::Warning,
                                SwitchOutcome::ControlUnavailable => Severity::Error,
                            };
                            io.journal(outcome.journal_status(word, ""), severity).await;
                        }
                    }
                }
                None => {
                    notified.await;
                }
            }
        }
    })
}

#[cfg(test)]
mod vocabulary_tests {
    use super::*;

    #[test]
    fn native_words_round_trip_and_default_is_manual() {
        for mode in [
            ClaudePermissionMode::Manual,
            ClaudePermissionMode::Auto,
            ClaudePermissionMode::AcceptEdits,
            ClaudePermissionMode::DontAsk,
            ClaudePermissionMode::Plan,
            ClaudePermissionMode::BypassPermissions,
        ] {
            assert_eq!(from_native(wire_word(mode)), Some(mode));
        }
        assert_eq!(from_native("default"), Some(ClaudePermissionMode::Manual));
        assert_eq!(from_native("bypass"), Some(ClaudePermissionMode::BypassPermissions));
        assert_eq!(from_native("nonsense"), None);
    }

    #[test]
    fn reachability_matches_the_measured_wheel() {
        use ClaudePermissionMode::*;
        // dontAsk is never live-reachable.
        assert!(!live_reachable(DontAsk, true));
        // bypass needs the launch allowance.
        assert!(!live_reachable(BypassPermissions, false));
        assert!(live_reachable(BypassPermissions, true));
        for mode in [Manual, AcceptEdits, Plan, Auto] {
            assert!(live_reachable(mode, false));
        }
    }

    #[test]
    fn wheel_walks_match_measured_cycles() {
        use ClaudePermissionMode::*;
        // This host: auto available, no bypass — the four-mode wheel.
        let four = [Manual, AcceptEdits, Plan, Auto];
        for window in four.windows(2) {
            assert_eq!(cycle_steps(window[0], window[1], true, false), Some(1));
        }
        assert_eq!(cycle_steps(Auto, Manual, true, false), Some(1));
        assert_eq!(cycle_steps(Manual, Auto, true, false), Some(3));
        assert_eq!(cycle_steps(Plan, Manual, true, false), Some(2));
        // Bypass session: five-mode wheel, bypass between plan and auto.
        assert_eq!(cycle_steps(Plan, BypassPermissions, true, true), Some(1));
        assert_eq!(cycle_steps(BypassPermissions, Auto, true, true), Some(1));
        assert_eq!(cycle_steps(Manual, BypassPermissions, true, true), Some(3));
        assert_eq!(cycle_steps(Auto, BypassPermissions, true, true), Some(4));
        // dontAsk: one press exits to manual, then walk on.
        assert_eq!(cycle_steps(DontAsk, Manual, true, true), Some(1));
        assert_eq!(cycle_steps(DontAsk, Plan, true, true), Some(3));
        // Unreachable targets.
        assert_eq!(cycle_steps(Manual, DontAsk, true, true), None);
        assert_eq!(cycle_steps(Manual, BypassPermissions, true, false), None);
        // Already there costs no presses.
        assert_eq!(cycle_steps(Auto, Auto, true, false), Some(0));
    }

    #[test]
    fn indicators_parse_settled_and_hint_forms() {
        use ClaudePermissionMode::*;
        let cases = [
            ("  ⏸ manual mode on · ← for agents", Manual),
            ("⏵⏵ accept edits on(shift+tab to cycle) · ← for agents", AcceptEdits),
            ("  ⏸ plan mode on (shift+tab to cycle) · ← for agents", Plan),
            ("⏵⏵ auto mode on (shift+tab to cycle) · ← for agents", Auto),
            (
                "⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents",
                BypassPermissions,
            ),
            ("⏵⏵ don't ask on (shift+tab to cycle) · ← for agents", DontAsk),
            // Streaming-scrambled rendering: stray spaces inside the phrase
            // (measured: "plan mode on (shift+tab o cycle) · ←for agents").
            ("⏸ plan mode on (shift+tab o cycle)  · ←for agents", Plan),
        ];
        for (screen, expected) in cases {
            assert_eq!(parse_indicator(screen), Some(expected), "{screen}");
        }
        assert_eq!(parse_indicator("nothing here"), None);
    }

    #[test]
    fn last_indicator_wins_over_stacked_toasts() {
        // After a cycle the toast history lists previous modes above the
        // settled line; the settled line is painted last.
        let screen = "⏵⏵ accept edits on (shift+tab to cycle)\n\
                      ⏸ plan mode on (shift+tab to cycle)\n\
                      ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents";
        assert_eq!(parse_indicator(screen), Some(ClaudePermissionMode::Auto));
    }

    #[test]
    fn bypass_dialog_active_detection_ignores_scrollback() {
        let modal = "By proceeding, you accept all responsibility for actionstaken while \
                     running in Bypass Permissions mode.\n❯ No, exit\n Yes, I accept\n\
                     Enter to confirm · Esc to cancel";
        assert!(bypass_dialog_visible(modal));
        // After acceptance the modal text stays in scrollback, but the settled
        // status line paints BELOW the confirm hint — not the active modal.
        let settled = format!(
            "{modal}\n⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents"
        );
        assert!(!bypass_dialog_visible(&settled));
        // The non-blocking auto-mode entry warning must not be mistaken for
        // the bypass disclaimer (no confirm footer).
        let auto_warning = "Sessions are slightly more expensive. Claude can make mistakes \
                            that allow harmful commands to run … Shift+Tab to change mode.\n\
                            ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents";
        assert!(!bypass_dialog_visible(auto_warning));
    }

    #[tokio::test]
    async fn bridge_resolves_rejects_and_times_out() {
        let bridge = PermissionBridge::new(true);
        let generation = bridge.arm(ClaudePermissionMode::Auto);
        bridge.resolve(generation, ClaudePermissionMode::Auto);
        match bridge
            .wait(generation, std::time::Duration::from_secs(1))
            .await
            .expect("verdict")
        {
            Readback::Applied(mode) => assert_eq!(mode, ClaudePermissionMode::Auto),
            other => panic!("{other:?}"),
        }
        assert_eq!(bridge.pending(), None);

        let generation = bridge.arm(ClaudePermissionMode::Plan);
        bridge.reject(generation, "bypass-dialog-kept");
        match bridge
            .wait(generation, std::time::Duration::from_secs(1))
            .await
            .expect("verdict")
        {
            Readback::Rejected { reason } => assert_eq!(reason, "bypass-dialog-kept".to_string()),
            other => panic!("{other:?}"),
        }

        let generation = bridge.arm(ClaudePermissionMode::AcceptEdits);
        assert!(
            bridge
                .wait(generation, std::time::Duration::from_millis(20))
                .await
                .is_none()
        );
        bridge.fail(generation);
    }
}
#[cfg(test)]
mod switch_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex as StdMutex;

    /// Scripted PTY: each screen read returns the next fixture screen, and
    /// keystrokes are recorded. Modal screens are served verbatim.
    struct MockIo {
        screens: StdMutex<Vec<String>>,
        writes: Arc<StdMutex<Vec<String>>>,
        idle: bool,
        journals: Arc<StdMutex<Vec<(String, Severity)>>>,
        read_count: AtomicU32,
    }

    impl MockIo {
        fn new(idle: bool, screens: Vec<String>, writes: Arc<StdMutex<Vec<String>>>,
               journals: Arc<StdMutex<Vec<(String, Severity)>>>) -> Self {
            Self { screens: StdMutex::new(screens), writes, idle, journals,
                   read_count: AtomicU32::new(0) }
        }
        /// The visible screen follows the number of cycles performed: screen
        /// `n` is what paints after the nth shift+tab, and reads repeat the
        /// same screen until the next cycle (matching repaint semantics).
        fn current(&self) -> String {
            let cycles = self
                .writes
                .lock()
                .unwrap()
                .iter()
                .filter(|w| w.as_str() == "cycle")
                .count();
            let screens = self.screens.lock().unwrap();
            screens[cycles.min(screens.len() - 1)].clone()
        }
    }

    #[async_trait::async_trait]
    impl PermissionSwitchIo for MockIo {
        async fn is_idle(&self) -> bool { self.idle }
        async fn send_cycle(&self) -> DriverResult<()> {
            self.writes.lock().unwrap().push("cycle".into()); Ok(())
        }
        async fn press_down(&self) -> DriverResult<()> {
            self.writes.lock().unwrap().push("down".into()); Ok(())
        }
        async fn press_enter(&self) -> DriverResult<()> {
            self.writes.lock().unwrap().push("enter".into()); Ok(())
        }
        async fn press_esc(&self) -> DriverResult<()> {
            self.writes.lock().unwrap().push("esc".into()); Ok(())
        }
        async fn screen_text(&self) -> DriverResult<String> {
            self.read_count.fetch_add(1, Ordering::Relaxed);
            Ok(self.current())
        }
        async fn journal(&self, status: String, severity: Severity) {
            self.journals.lock().unwrap().push((status, severity));
        }
    }

    fn settled(mode: &str) -> String {
        let phrase = match mode {
            "manual" => "manual mode on",
            "acceptEdits" => "accept edits on",
            "plan" => "plan mode on",
            "auto" => "auto mode on",
            "bypassPermissions" => "bypass permissions on",
            "dontAsk" => "don't ask on",
            other => panic!("{other}"),
        };
        format!("  {phrase} · ← for agents")
    }

    fn modal() -> String {
        "WARNING: Claude Code running in Bypass Permissions mode\n\
         By proceeding, you accept all responsibility for actions taken \
         while running in Bypass Permissions mode.\n\
         ❯ No, exit\n Yes, I accept\n Enter to confirm · Esc to cancel".into()
    }

    #[tokio::test]
    async fn closed_loop_walk_presses_until_target_paints() {
        let writes = Arc::new(StdMutex::new(Vec::new()));
        let journals = Arc::new(StdMutex::new(Vec::new()));
        // start: acceptEdits, after press1: plan, press2: auto (target).
        let screens = vec![
            settled("acceptEdits"),
            settled("plan"),
            settled("auto"),
        ]; // index = cycles performed
        let io = Arc::new(MockIo::new(true, screens, writes.clone(), journals.clone()));
        let bridge = Arc::new(PermissionBridge::new(false));
        let request = PermissionRequest { mode: ClaudePermissionMode::Auto };
        let outcome = perform_switch(request, &bridge, io.as_ref()).await;
        assert_eq!(outcome, SwitchOutcome::Applied);
        let cycles = writes.lock().unwrap().iter().filter(|w| w.as_str() == "cycle").count();
        assert_eq!(cycles, 2, "two shift+tab presses acceptEdits→plan→auto");
        assert!(journals.lock().unwrap().is_empty(), "applied is silent");
    }

    #[tokio::test]
    async fn already_in_mode_is_a_noop() {
        let writes = Arc::new(StdMutex::new(Vec::new()));
        let journals = Arc::new(StdMutex::new(Vec::new()));
        let screens = vec![settled("plan")];
        let io = Arc::new(MockIo::new(true, screens, writes.clone(), journals.clone()));
        let bridge = Arc::new(PermissionBridge::new(true));
        bridge.note_launch_mode(ClaudePermissionMode::Plan);
        let outcome = perform_switch(
            PermissionRequest { mode: ClaudePermissionMode::Plan },
            &bridge, io.as_ref(),
        ).await;
        assert_eq!(outcome, SwitchOutcome::Applied);
        assert!(writes.lock().unwrap().is_empty());
        // The no-op still journals applied so an optimistic pending settles.
        assert!(journals.lock().unwrap().iter().any(|(s, _)| s == "permission-applied:plan"));
    }

    #[tokio::test]
    async fn dontask_is_refused_without_a_keystroke() {
        let writes = Arc::new(StdMutex::new(Vec::new()));
        let journals = Arc::new(StdMutex::new(Vec::new()));
        let screens = vec![settled("manual")];
        let io = Arc::new(MockIo::new(true, screens, writes.clone(), journals.clone()));
        let bridge = Arc::new(PermissionBridge::new(true));
        let outcome = perform_switch(
            PermissionRequest { mode: ClaudePermissionMode::DontAsk },
            &bridge, io.as_ref(),
        ).await;
        assert_eq!(outcome, SwitchOutcome::Unsupported);
        assert!(writes.lock().unwrap().is_empty(), "launch-only means no keys");
        let statuses = journals.lock().unwrap();
        assert!(statuses.iter().any(|(s, _)| s == "permission-unsupported-in-session:dontAsk"));
    }

    #[tokio::test]
    async fn bypass_without_allowance_is_refused() {
        let writes = Arc::new(StdMutex::new(Vec::new()));
        let journals = Arc::new(StdMutex::new(Vec::new()));
        let io = Arc::new(MockIo::new(true, vec![settled("plan")], writes.clone(), journals.clone()));
        let bridge = Arc::new(PermissionBridge::new(false));
        let outcome = perform_switch(
            PermissionRequest { mode: ClaudePermissionMode::BypassPermissions },
            &bridge, io.as_ref(),
        ).await;
        assert_eq!(outcome, SwitchOutcome::Unsupported);
        assert!(writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn bypass_modal_gets_gated_down_enter_then_applies() {
        let writes = Arc::new(StdMutex::new(Vec::new()));
        let journals = Arc::new(StdMutex::new(Vec::new()));
        // plan → press → modal (twice: across the gated accept) → bypass line.
        let screens = vec![
            settled("plan"),
            modal(),
            modal(),
            modal(),
            settled("bypassPermissions"),
        ];
        let io = Arc::new(MockIo::new(true, screens, writes.clone(), journals.clone()));
        let bridge = Arc::new(PermissionBridge::new(true));
        let outcome = perform_switch(
            PermissionRequest { mode: ClaudePermissionMode::BypassPermissions },
            &bridge, io.as_ref(),
        ).await;
        assert_eq!(outcome, SwitchOutcome::Applied);
        let all = writes.lock().unwrap().clone();
        assert!(all.contains(&"down".to_string()), "the modal defaults to No,exit");
        assert!(all.contains(&"enter".to_string()));
        // down/enter each happen at most once per modal.
        assert_eq!(all.iter().filter(|w| w.as_str() == "down").count(), 1);
        assert_eq!(all.iter().filter(|w| w.as_str() == "enter").count(), 1);
    }

    #[tokio::test]
    async fn never_reaching_target_degrades_with_reason() {
        let writes = Arc::new(StdMutex::new(Vec::new()));
        let journals = Arc::new(StdMutex::new(Vec::new()));
        // Wheel walks but never shows auto.
        let screens = vec![settled("manual"), settled("acceptEdits"), settled("plan")];
        let io = Arc::new(MockIo::new(true, screens, writes.clone(), journals.clone()));
        let bridge = Arc::new(PermissionBridge::new(false));
        let outcome = perform_switch(
            PermissionRequest { mode: ClaudePermissionMode::Auto },
            &bridge, io.as_ref(),
        ).await;
        assert_eq!(outcome, SwitchOutcome::Degraded);
        let cycles = writes.lock().unwrap().iter().filter(|w| w.as_str() == "cycle").count();
        assert_eq!(cycles, PERMISSION_MAX_PRESSES);
        let statuses = journals.lock().unwrap();
        assert!(statuses.iter().any(|(s, sev)| {
            s.starts_with("permission-degraded:auto:") && *sev == Severity::Warning
        }), "degraded journals a warning: {statuses:?}");
    }
}
