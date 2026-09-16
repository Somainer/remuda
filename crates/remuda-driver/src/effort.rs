//! Effort across the launch argv and the live session (D-028 §9.1).
//!
//! Two layers, one module:
//!
//! * **Launch vocabulary** — [`effort_argv`] maps one normalized
//!   [`remuda_protocol::EffortSelection`] onto each CLI's native channel.
//!   claude/agy take `--effort <v>`, codex takes the
//!   `-c model_reasoning_effort="<v>"` overlay (codex parses no top-level
//!   `--effort`), grok takes `--reasoning-effort <v>`. The closed
//!   vocabularies and the legacy-name migration
//!   (`minimal`/`quick`/`standard`/`max`…) live in remuda-protocol
//!   (`normalize_legacy_effort`); this layer only validates a normalized
//!   selection is launchable. Vocabulary evidence in
//!   `docs/design/evidence/composer-slider-5.md`.
//! * **Live two-way sync** — below the launch section: Remuda → Claude writes
//!   `/effort <level>` and waits for transcript read-back; Claude → Remuda
//!   feeds assistant records through `remuda_protocol::EffortTracker`.
//!   Measured on claude 2.1.221 (`docs/design/evidence/effort-sync-1.md`).
//!
//! One rule for the live direction: the transcript is the only authority on
//! what is actually in effect.

use crate::error::{DriverError, DriverResult};
use remuda_protocol::{AgentKind, EffortName, EffortSelection, Severity};
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering as AtomicOrdering},
};
use tokio::sync::{Notify, oneshot};

// ───────────────────────────── Launch vocabulary ─────────────────────────────

/// The six tiers in the codex-cli 0.154.0 picker on the owner's Mac.
/// `minimal` remains an input alias for `low`, but is no longer offered.
/// Evidence: `docs/design/evidence/effort-codex-tiers-1.md`.
pub const CODEX_REASONING_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];

/// Grok's built-in effort menu (`EFFORT_LEVELS` / user guide `/effort`):
/// xhigh · high · medium · low. The wire enum also parses `none`/`minimal`/
/// `max` as power-user spellings, but no current model menu advertises them.
pub const GROK_REASONING_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];

/// Values `claude --effort` accepts: the five levels. `ultracode` rides as the
/// separate flag value and is handled via [`EffortSelection::flag_value`].
pub const CLAUDE_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Build the argv tokens that carry one effort selection for a native-PTY
/// launch. `None` emits nothing: an unpinned effort means the CLI uses its own
/// default rather than inheriting a level nobody chose.
pub fn effort_argv(kind: AgentKind, effort: Option<EffortSelection>) -> DriverResult<Vec<String>> {
    let Some(effort) = effort else {
        return Ok(Vec::new());
    };
    match kind {
        AgentKind::Claude | AgentKind::Agy => {
            // agy shares the Claude Code level flag shape, but ultracode is
            // a Claude-only workflow flag.
            if kind == AgentKind::Agy && effort.ultracode {
                return Err(DriverError::InvalidLaunchSpec(
                    "ultracode is a Claude-only workflow flag; agy has no such effort".into(),
                ));
            }
            if matches!(effort.name, EffortName::Minimal | EffortName::Ultra) {
                return Err(DriverError::InvalidLaunchSpec(format!(
                    "--effort {} is not a Claude/agy level (one of {})",
                    effort.level_name(),
                    CLAUDE_EFFORTS.join(" / ")
                )));
            }
            Ok(vec!["--effort".into(), effort.flag_value().into()])
        }
        AgentKind::Codex => {
            if effort.ultracode {
                return Err(DriverError::InvalidLaunchSpec(
                    "ultracode is a Claude-only workflow flag; codex has no such effort".into(),
                ));
            }
            let value = codex_reasoning_value(effort.name);
            Ok(vec![
                "-c".into(),
                format!("model_reasoning_effort=\"{value}\""),
            ])
        }
        AgentKind::Grok => {
            if effort.ultracode {
                return Err(DriverError::InvalidLaunchSpec(
                    "ultracode is a Claude-only workflow flag; grok has no such effort".into(),
                ));
            }
            let value = grok_reasoning_value(effort.name)?;
            // Canonical long name; `--effort` is only a visible alias.
            Ok(vec!["--reasoning-effort".into(), value.into()])
        }
        // Terminal launches a login shell; generic is rejected before this
        // path by the preset lookup, but never invent effort flags either way.
        AgentKind::Terminal | AgentKind::Generic => Ok(Vec::new()),
    }
}

/// Map a protocol level onto the Codex `model_reasoning_effort` vocabulary.
fn codex_reasoning_value(name: EffortName) -> &'static str {
    match name {
        EffortName::Minimal | EffortName::Low => "low",
        EffortName::Medium => "medium",
        EffortName::High => "high",
        EffortName::Xhigh => "xhigh",
        EffortName::Max => "max",
        EffortName::Ultra => "ultra",
    }
}

/// Map a protocol level onto the grok `--reasoning-effort` menu vocabulary.
fn grok_reasoning_value(name: EffortName) -> DriverResult<&'static str> {
    match name {
        EffortName::Low => Ok("low"),
        EffortName::Medium => Ok("medium"),
        EffortName::High => Ok("high"),
        EffortName::Xhigh => Ok("xhigh"),
        EffortName::Minimal | EffortName::Max | EffortName::Ultra => {
            Err(DriverError::InvalidLaunchSpec(format!(
                "grok --reasoning-effort takes one of {} (the built-in /effort menu)",
                GROK_REASONING_EFFORTS.join(" / ")
            )))
        }
    }
}

/// Reject effort flags a caller smuggled through `spec.args`: the materializer
/// owns this axis per-kind, and a hand-written flag would either repeat the
/// emitted token or use the wrong vocabulary (codex parses no `--effort` at
/// all). The materializer-emitted tokens never pass through this check.
pub fn ensure_no_effort_in_extras(kind: AgentKind, extras: &[String]) -> DriverResult<()> {
    let banned: &[&str] = match kind {
        AgentKind::Claude | AgentKind::Agy => &["--effort"],
        AgentKind::Codex => &["--effort", "--reasoning-effort"],
        AgentKind::Grok => &["--effort", "--reasoning-effort"],
        AgentKind::Terminal | AgentKind::Generic => &[],
    };
    for token in extras {
        let head = token.split('=').next().unwrap_or(token);
        if banned.contains(&head) {
            return Err(DriverError::InvalidLaunchSpec(format!(
                "flag {head} is reserved for the per-kind effort mapping"
            )));
        }
    }
    Ok(())
}

// ───────────────────────── Live two-way effort sync ──────────────────────────
/// Bounded wait for the post-switch read-back.
///
/// The acceptance channel is the `/effort` command's
/// `<local-command-stdout>` verdict, which lands a few hundred ms after the
/// dialog is confirmed (measured ~125–270 ms on claude 2.1.272, see
/// `docs/design/evidence/effort-sync-2.md`). This window only bounds the
/// synchronous idle-path configure call against a wedged TUI; a switch made
/// while the agent is working is queued instead.
pub(crate) const EFFORT_READBACK_TIMEOUT_MS: u64 = 10_000;
/// Poll cadence while waiting for read-back (also the rescue-read cadence).
pub(crate) const EFFORT_READBACK_POLL_MS: u64 = 250;
/// Settle pause between writes (composer body, the submitting CR, the confirm
/// dialog CR). The ready ladder already gates the first write; these small
/// gaps keep keystrokes out of one TTY read.
pub(crate) const EFFORT_WRITE_SETTLE_MS: u64 = 120;
/// How long the confirming dialog is waited for before the read-back wait
/// begins. If the dialog is missed inside this window the rescue pass during
/// the read-back wait confirms it once it is seen.
pub(crate) const EFFORT_DIALOG_DEADLINE_MS: u64 = 1_500;
/// Poll cadence of the dialog phase.
pub(crate) const EFFORT_DIALOG_POLL_MS: u64 = 40;
/// A rescue Enter is only sent this long after the previous one, so a dialog
/// repainting itself closed never earns a double-confirm.
pub(crate) const EFFORT_RESCUE_CR_GAP_MS: u64 = 700;
/// Maximum rescue Enters per switch (the original confirm plus two).
pub(crate) const EFFORT_MAX_RESCUE_CRS: u32 = 2;

/// Lowercased screen markers for the `/effort` confirmation dialog and the
/// invalid-argument error. Compared against ANSI-stripped, lowercased text.
pub(crate) const DIALOG_MARKER: &str = "change effort level";
pub(crate) const INVALID_MARKER: &str = "invalid argument";

/// The exact `/effort` vocabulary measured in-session on claude 2.1.221.
#[allow(dead_code)] // documentation constant; also used by the fake harness
pub(crate) const IN_SESSION_LEVELS: &[&str] =
    &["low", "medium", "high", "xhigh", "max", "ultracode", "auto"];

/// What the switch was asked to change to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EffortRequest {
    /// Level to type (`ultracode` spelled out so `/effort ultracode` works).
    pub(crate) name: EffortName,
    pub(crate) ultracode: bool,
}

impl EffortRequest {
    pub(crate) fn from_level(level: &str) -> Option<Self> {
        match level.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Self {
                name: EffortName::Low,
                ultracode: false,
            }),
            "medium" => Some(Self {
                name: EffortName::Medium,
                ultracode: false,
            }),
            "high" => Some(Self {
                name: EffortName::High,
                ultracode: false,
            }),
            "xhigh" => Some(Self {
                name: EffortName::Xhigh,
                ultracode: false,
            }),
            "max" => Some(Self {
                name: EffortName::Max,
                ultracode: false,
            }),
            "ultracode" => Some(Self {
                name: EffortName::Xhigh,
                ultracode: true,
            }),
            _ => None,
        }
    }

    /// The native spelling to type after `/effort ` — `ultracode` is its own
    /// word; the five levels spell themselves.
    pub(crate) fn command_word(&self) -> &'static str {
        if self.ultracode {
            "ultracode"
        } else {
            match self.name {
                EffortName::Low => "low",
                EffortName::Medium => "medium",
                EffortName::High => "high",
                EffortName::Xhigh => "xhigh",
                EffortName::Max => "max",
                // Neither the legacy `minimal` nor Codex `ultra` is a Claude
                // `/effort` level; EffortRequest::from_level produces neither.
                EffortName::Minimal => "minimal",
                EffortName::Ultra => "ultra",
            }
        }
    }

    /// The whole slash command body, without the submitting CR.
    pub(crate) fn command_body(&self) -> String {
        format!("/effort {}", self.command_word())
    }

    /// Word the confirmation dialog uses to name the tier this switch targets.
    ///
    /// The dialog (measured on 2.1.272/2.1.273) renders
    /// "1. Yes, switch to <tier>" / "Switching to <tier> means …". An ultracode
    /// switch is an xhigh switch plus the workflow flag, so the dialog names
    /// `xhigh`, not `ultracode`. This is what gates the confirming CR: a stale
    /// dialog left on screen by the previous switch names a different tier and
    /// must never eat the next switch's Enter (c-effort3).
    pub(crate) fn dialog_target_word(&self) -> &'static str {
        match self.name {
            EffortName::Low => "low",
            EffortName::Medium => "medium",
            EffortName::High => "high",
            EffortName::Xhigh => "xhigh",
            EffortName::Max => "max",
            // EffortRequest never carries either name for Claude.
            EffortName::Minimal => "minimal",
            EffortName::Ultra => "ultra",
        }
    }

    /// Level the transcript must report for this switch to count as observed.
    ///
    /// `ultracode` reads back as `xhigh` — Claude does not repeat the workflow
    /// flag on assistant records.
    pub(crate) fn observed_name(&self) -> EffortName {
        self.name
    }
}

/// Terminal state of a switch attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwitchOutcome {
    /// `/effort …` was typed and an assistant record observed the new level.
    Applied,
    /// The agent was working; the change is held for the next idle moment.
    Queued,
    /// The command was typed but read-back never matched; effective stays put.
    Degraded,
    /// The requested word is not in the in-session vocabulary.
    #[allow(dead_code)] // produced by the documented reject path
    Unsupported,
    /// Control was not available (closed pane, booting TUI, …).
    ControlUnavailable,
}

impl SwitchOutcome {
    /// Stable journal status string for the `instance.configure` lifecycle.
    pub(crate) fn journal_status(self, word: &str, detail: &str) -> String {
        match self {
            Self::Applied => format!("effort-applied:{word}"),
            Self::Queued => format!("effort-queued:{word}"),
            Self::Degraded => format!("effort-degraded:{word}:{detail}"),
            Self::Unsupported => format!("effort-unsupported-in-session:{word}"),
            Self::ControlUnavailable => format!("effort-control-unavailable:{detail}"),
        }
    }
}

/// Terminal verdict of a switch's read-back wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readback {
    /// The stdout verdict accepted the level; this is what is now in effect.
    Applied(remuda_protocol::ObservedEffort),
    /// Claude refused the switch (dialog dismissed with Esc, invalid argument)
    /// and the journaled reason names why.
    Rejected {
        /// Stable reason code (`dialog-kept`, `invalid-argument`).
        reason: String,
    },
}

/// Coordination between the driver's switch call and the transcript pump.
///
/// The driver arming a switch is synchronous with typing `/effort`; the
/// read-back now arrives when the transcript pump maps the command's
/// `<local-command-stdout>` verdict (measured ~300 ms), rather than waiting
/// for the next turn's assistant record. The bridge is that rendezvous. A
/// much later natural change to the same level is not mis-attributed: the
/// generation only resolves on the verdict of its own command.
pub(crate) struct EffortBridge {
    state: Mutex<BridgeState>,
    notify: Notify,
}

#[derive(Default)]
struct BridgeState {
    /// A switch waiting for its command verdict.
    pending: Option<(u64, EffortRequest)>,
    /// Generation whose verdict has arrived.
    verdict_gen: u64,
    verdict: Option<Readback>,
    /// The latest thing any side asked for (payload `requested` field).
    requested: Option<EffortRequest>,
    generation: u64,
}

impl EffortBridge {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(BridgeState::default()),
            notify: Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, BridgeState> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Arm a new Remuda switch; returns its generation token.
    pub(crate) fn arm(&self, request: EffortRequest) -> u64 {
        let mut state = self.lock();
        state.generation += 1;
        let generation = state.generation;
        state.pending = Some((generation, request));
        state.requested = Some(request);
        generation
    }

    /// Record the launch-time requested selection (payload provenance only).
    pub(crate) fn note_launch_request(&self, request: EffortRequest) {
        self.lock().requested = Some(request);
    }

    /// The pending switch the mapper should arm its tracker with, if any.
    pub(crate) fn pending(&self) -> Option<EffortRequest> {
        self.lock().pending.map(|(_, request)| request)
    }

    /// Pending switch together with its generation, for the mapper to arm once.
    pub(crate) fn pending_with_gen(&self) -> Option<(u64, EffortRequest)> {
        self.lock().pending
    }

    /// Whether a switch is currently awaiting read-back.
    pub(crate) fn has_pending(&self) -> bool {
        self.lock().pending.is_some()
    }

    /// Latest requested selection, for the emitted payload.
    pub(crate) fn requested(&self) -> Option<EffortRequest> {
        self.lock().requested
    }

    /// Mapper side: the command verdict `generation` was waiting for arrived.
    pub(crate) fn resolve(&self, generation: u64, observed: remuda_protocol::ObservedEffort) {
        {
            let mut state = self.lock();
            if let Some((pending_gen, _)) = state.pending
                && pending_gen == generation
            {
                state.pending = None;
            }
            state.verdict = Some(Readback::Applied(observed));
            state.verdict_gen = generation;
        }
        self.notify.notify_waiters();
    }

    /// Mapper side: Claude refused the command (Esc on the confirmation dialog
    /// → "Kept effort level", or an invalid argument).
    pub(crate) fn reject(&self, generation: u64, reason: impl Into<String>) {
        {
            let mut state = self.lock();
            if let Some((pending_gen, _)) = state.pending
                && pending_gen == generation
            {
                state.pending = None;
            }
            state.verdict = Some(Readback::Rejected {
                reason: reason.into(),
            });
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

    /// Wait until generation `generation` gets a verdict, bounded by `timeout`.
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

/// The driver-side I/O a switch needs: readiness, two-segment writes, and a
/// screen read for the post-submit confirmation dialog. One adapter per PTY
/// carrier (Herdr pane, local PTY).
#[async_trait::async_trait]
pub(crate) trait EffortSwitchIo: Send + Sync {
    /// True when the native composer can accept the slash command right now.
    async fn is_idle(&self) -> bool;

    /// Type the slash-command body (no submitting CR).
    async fn type_body(&self, body: &str) -> crate::DriverResult<()>;

    /// Send Enter as its own write.
    async fn press_enter(&self) -> crate::DriverResult<()>;

    /// Current visible screen text, ANSI stripped.
    async fn screen_text(&self) -> crate::DriverResult<String>;

    /// Journal an `instance.configure` effort lifecycle from the worker.
    async fn journal(&self, status: String, severity: Severity);
}

/// Type the slash command, clear the confirmation dialog Claude opens for a
/// cached conversation, then wait for the command's stdout verdict.
///
/// The body and both Enter presses are separate writes with a settle gap,
/// matching [`crate::shell_pty::send`]'s measured requirement.
///
/// ## Dialog gating (c-effort3)
///
/// The confirming Enter used to be sent as soon as the screen contained
/// "Change effort level". On the second switch of a session a **rendered**
/// dialog from the previous switch can still be inside the visible region
/// (a short Herdr grid, a slow repaint): the Enter then raced the new dialog's
/// paint, and when it landed first the real "Yes, switch" modal was left
/// open — no stdout verdict ever came, and the switch was falsely reported
/// `no-readback-within-window` even though Claude had applied the command.
///
/// Two layers prevent that now:
///
/// 1. **Target gating.** The screen is snapshotted before typing. A dialog
///    already present is treated as stale; the phase-1 Enter waits for a dialog
///    naming THIS switch's tier ("switch to \<tier\>"). A stale dialog naming
///    the previous tier can never take it.
/// 2. **Rescue pass.** While the read-back is waited on, a dialog still open
///    and attributable to this command gets a bounded number of spaced
///    Enters. This both heals a phase-1 miss (late paint) and an Enter that
///    raced the paint; an extra Enter after the dialog has closed lands in an
///    empty composer, which the TUI ignores. The transcript verdict remains
///    the only authority.
pub(crate) async fn perform_switch(
    request: EffortRequest,
    bridge: &EffortBridge,
    io: &dyn EffortSwitchIo,
) -> SwitchOutcome {
    let word = request.command_word();
    let target = request.dialog_target_word();
    // Arm before typing so the mapper correlates the slash record the command
    // produces with this generation.
    let generation = bridge.arm(request);

    // Snapshot before typing: a dialog the previous switch left rendered is
    // stale and must not receive this switch's Enter.
    let pre_screen = normalize_screen(&io.screen_text().await.unwrap_or_default());
    let mut stale_dialog = pre_screen.contains(DIALOG_MARKER);

    if let Err(error) = io.type_body(&request.command_body()).await {
        tracing::warn!(%error, "effort switch: body write failed");
        bridge.fail(generation);
        io.journal(
            SwitchOutcome::ControlUnavailable.journal_status(word, &error.to_string()),
            Severity::Error,
        )
        .await;
        return SwitchOutcome::ControlUnavailable;
    }
    tokio::time::sleep(std::time::Duration::from_millis(EFFORT_WRITE_SETTLE_MS)).await;
    if let Err(error) = io.press_enter().await {
        tracing::warn!(%error, "effort switch: submit write failed");
        bridge.fail(generation);
        io.journal(
            SwitchOutcome::ControlUnavailable.journal_status(word, &error.to_string()),
            Severity::Error,
        )
        .await;
        return SwitchOutcome::ControlUnavailable;
    }

    // Phase 1: confirm the dialog THIS command opened. An invalid argument
    // renders an error instead and must never get a confirming Enter; `Esc`
    // cancels, so every Enter here is gated. A verdict can arrive inside this
    // window too (a dialog-free build applies on the submit CR), in which case
    // the wait ends immediately.
    let dialog_deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(EFFORT_DIALOG_DEADLINE_MS);
    let mut invalid = false;
    let mut confirmed = false;
    let mut outcome = None;
    while std::time::Instant::now() < dialog_deadline {
        // Non-blocking verdict check: a dialog-free switch applies within
        // hundreds of ms of the submit CR and must not wait out the window.
        if let Some(verdict) = bridge.wait(generation, std::time::Duration::ZERO).await {
            outcome = Some(verdict);
            break;
        }
        let text = match io.screen_text().await {
            Ok(text) => normalize_screen(&text),
            Err(error) => {
                // A failed screen read cannot stop the switch — the command
                // may already be applied, and the verdict is the authority.
                tracing::debug!(%error, "effort switch: dialog screen read failed; continuing");
                break;
            }
        };
        if text.contains(INVALID_MARKER) {
            invalid = true;
            break;
        }
        if text.contains(DIALOG_MARKER) {
            let names_target = screen_names_target(&text, target);
            // A dialog naming our target is ours even when a same-word dialog
            // was rendered pre-submit (a modal cannot stay open while the
            // composer accepted the new command). When no dialog predated the
            // submit, a marker-only dialog (unknown future copy) is accepted
            // too; a stale marker naming the previous tier is simply waited out.
            if names_target || !stale_dialog {
                tokio::time::sleep(std::time::Duration::from_millis(EFFORT_WRITE_SETTLE_MS)).await;
                if io.press_enter().await.is_ok() {
                    confirmed = true;
                }
                break;
            }
            // Marker present but naming the previous tier: keep waiting for
            // the stale render to clear and the new dialog to paint.
        } else {
            // The visible region is dialog-free; whatever paints next is new.
            stale_dialog = false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(EFFORT_DIALOG_POLL_MS)).await;
    }

    // Phase 2: wait for the command verdict, rescuing a still-open dialog.
    let readback_deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(EFFORT_READBACK_TIMEOUT_MS);
    let mut last_enter: Option<std::time::Instant> = confirmed.then(std::time::Instant::now);
    let mut rescue_enters = 0_u32;
    // The screen was observed dialog-free at least once after the submit
    // (immediately so when no stale dialog existed); a later marker-only dialog
    // is then attributable even without a tier word in its text.
    let mut seen_dialog_free = !pre_screen.contains(DIALOG_MARKER);
    while outcome.is_none() && std::time::Instant::now() < readback_deadline {
        if let Some(verdict) = bridge
            .wait(
                generation,
                std::time::Duration::from_millis(EFFORT_READBACK_POLL_MS),
            )
            .await
        {
            outcome = Some(verdict);
            break;
        }
        if std::time::Instant::now() >= readback_deadline {
            break;
        }
        if invalid {
            continue;
        }
        let Ok(raw_text) = io.screen_text().await else {
            continue;
        };
        let text = normalize_screen(&raw_text);
        if text.contains(INVALID_MARKER) {
            invalid = true;
            continue;
        }
        if !text.contains(DIALOG_MARKER) {
            seen_dialog_free = true;
            continue;
        }
        let names_target = screen_names_target(&text, target);
        let attributable = names_target || seen_dialog_free;
        let gap_elapsed = last_enter
            .map(|when| when.elapsed() >= std::time::Duration::from_millis(EFFORT_RESCUE_CR_GAP_MS))
            .unwrap_or(true);
        if attributable
            && gap_elapsed
            && rescue_enters < EFFORT_MAX_RESCUE_CRS
            && io.press_enter().await.is_ok()
        {
            rescue_enters += 1;
            last_enter = Some(std::time::Instant::now());
        }
    }

    match outcome {
        Some(Readback::Applied(_)) => SwitchOutcome::Applied,
        Some(Readback::Rejected { reason }) => {
            // The dialog was dismissed (Esc) or the argument rejected. The
            // effective level never changed; the UI reverts on this lifecycle.
            bridge.fail(generation);
            io.journal(
                SwitchOutcome::Degraded.journal_status(word, &reason),
                Severity::Warning,
            )
            .await;
            SwitchOutcome::Degraded
        }
        None => {
            // Never claim applied without a verdict. A switch the user does
            // nothing after simply has no command record to read.
            bridge.fail(generation);
            io.journal(
                SwitchOutcome::Degraded.journal_status(word, "no-readback-within-window"),
                Severity::Warning,
            )
            .await;
            SwitchOutcome::Degraded
        }
    }
}

/// Lowercase a screen read and collapse every whitespace run to one space,
/// matching `remuda_screen::Grid::flat`'s convention: the TUI soft-wraps the
/// dialog copy across rows, so a multi-word phrase must match with the newlines
/// and the dialog's own odd spacing stripped (the measured 2.1.273 modal renders
/// "Switchingtoxhigh"-adjacent fragments with ragged spacing on narrow grids).
fn normalize_screen(raw: &str) -> String {
    raw.to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a screen names `tier` inside the `/effort` confirmation dialog.
fn screen_names_target(screen_lowercased: &str, tier: &str) -> bool {
    screen_lowercased.contains(&format!("switch to {tier}"))
        || screen_lowercased.contains(&format!("switching to {tier}"))
}

/// A switch held for the next idle moment (D-028 §9.1 ready ladder).
struct QueuedSwitch {
    request: EffortRequest,
    /// When present the caller is awaiting the outcome (idle fast path); when
    /// absent the worker journals the terminal status itself (working path).
    done: Option<oneshot::Sender<SwitchOutcome>>,
}

/// Queue of at most one pending switch (a newer switch replaces an older one).
pub(crate) struct EffortQueue {
    pending: Mutex<Option<QueuedSwitch>>,
    notify: Notify,
    closed: AtomicBool,
}

impl EffortQueue {
    pub(crate) fn new() -> Self {
        Self {
            pending: Mutex::new(None),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
        }
    }

    /// Queue a switch. `done` is handed back when it has been applied.
    pub(crate) fn enqueue(
        &self,
        request: EffortRequest,
        done: Option<oneshot::Sender<SwitchOutcome>>,
    ) {
        if let Ok(mut pending) = self.pending.lock() {
            *pending = Some(QueuedSwitch { request, done });
        }
        self.notify.notify_one();
    }

    /// Take the queued switch, if any.
    fn take(&self) -> Option<QueuedSwitch> {
        self.pending.lock().ok().and_then(|mut guard| guard.take())
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, AtomicOrdering::SeqCst);
        self.notify.notify_waiters();
    }
}

/// Spawn the worker that applies queued switches once the composer is idle.
///
/// The fast path (configure while idle) still goes through here so there is a
/// single serialization point; its `oneshot` carries the outcome straight
/// back, so no final lifecycle is journaled. The working path journaled
/// `queued` at enqueue time and gets its terminal `applied` / `degraded`
/// lifecycle from this worker.
pub(crate) fn spawn_worker(
    bridge: Arc<EffortBridge>,
    queue: Arc<EffortQueue>,
    io: Arc<dyn EffortSwitchIo>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while !queue.closed.load(AtomicOrdering::SeqCst) {
            // Arm the waiter before checking, so an enqueue cannot be missed.
            let notified = queue.notify.notified();
            let queued = queue.take();
            match queued {
                Some(QueuedSwitch { request, done }) => {
                    let word = request.command_word();
                    // Wait out a running turn on the ready ladder. A newer
                    // switch arriving meanwhile replaces this one, which never
                    // goes out and is reported back as still `queued`.
                    let mut replaced = false;
                    while !io.is_idle().await {
                        if queue.closed.load(AtomicOrdering::SeqCst) {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(
                            EFFORT_READBACK_POLL_MS,
                        ))
                        .await;
                        if queue.pending.lock().map(|p| p.is_some()).unwrap_or(false) {
                            replaced = true;
                            break;
                        }
                    }
                    if replaced || queue.pending.lock().map(|p| p.is_some()).unwrap_or(false) {
                        // The enqueue's notify_one was consumed by us; wake the
                        // outer iteration for the replacement explicitly.
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
                                SwitchOutcome::Applied | SwitchOutcome::Queued => Severity::Info,
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
mod argv_tests {
    use super::*;

    fn sel(name: EffortName, ultracode: bool) -> EffortSelection {
        EffortSelection { name, ultracode }
    }

    #[test]
    fn absent_effort_emits_nothing_for_every_kind() {
        for kind in [
            AgentKind::Claude,
            AgentKind::Codex,
            AgentKind::Grok,
            AgentKind::Agy,
        ] {
            assert_eq!(effort_argv(kind, None).unwrap(), Vec::<String>::new());
        }
    }

    #[test]
    fn claude_and_agy_use_the_effort_flag() {
        for kind in [AgentKind::Claude, AgentKind::Agy] {
            assert_eq!(
                effort_argv(kind, Some(sel(EffortName::High, false))).unwrap(),
                vec!["--effort".to_string(), "high".to_string()]
            );
            assert!(effort_argv(kind, Some(sel(EffortName::Minimal, false))).is_err());
        }
        assert_eq!(
            effort_argv(AgentKind::Claude, Some(sel(EffortName::Xhigh, true))).unwrap(),
            vec!["--effort".to_string(), "ultracode".to_string()]
        );
        assert!(matches!(
            effort_argv(AgentKind::Agy, Some(sel(EffortName::Xhigh, true))),
            Err(DriverError::InvalidLaunchSpec(message))
                if message == "ultracode is a Claude-only workflow flag; agy has no such effort"
        ));
    }

    #[test]
    fn ultra_is_rejected_by_other_harnesses_without_changing_the_error_shape() {
        for kind in [AgentKind::Claude, AgentKind::Agy, AgentKind::Grok] {
            assert!(matches!(
                effort_argv(kind, Some(sel(EffortName::Ultra, false))),
                Err(DriverError::InvalidLaunchSpec(_))
            ));
        }
    }

    #[test]
    fn codex_maps_one_to_one_onto_the_config_overlay() {
        let tiers = [
            (EffortName::Low, "low"),
            (EffortName::Medium, "medium"),
            (EffortName::High, "high"),
            (EffortName::Xhigh, "xhigh"),
            (EffortName::Max, "max"),
            (EffortName::Ultra, "ultra"),
        ];
        assert_eq!(
            CODEX_REASONING_EFFORTS,
            tiers.map(|(_, value)| value).as_slice()
        );
        for (name, value) in tiers {
            assert_eq!(
                effort_argv(AgentKind::Codex, Some(sel(name, false))).unwrap(),
                vec![
                    "-c".to_string(),
                    format!("model_reasoning_effort=\"{value}\"")
                ]
            );
        }
        // Old stored `minimal` selections remain launchable as `low`.
        assert_eq!(
            effort_argv(AgentKind::Codex, Some(sel(EffortName::Minimal, false))).unwrap(),
            vec![
                "-c".to_string(),
                "model_reasoning_effort=\"low\"".to_string()
            ]
        );
        assert!(effort_argv(AgentKind::Codex, Some(sel(EffortName::Xhigh, true))).is_err());
        assert!(effort_argv(AgentKind::Codex, Some(sel(EffortName::Ultra, true))).is_err());
    }

    #[test]
    fn grok_uses_the_reasoning_effort_flag_with_the_menu_vocabulary() {
        for (name, value) in [
            (EffortName::Low, "low"),
            (EffortName::Medium, "medium"),
            (EffortName::High, "high"),
            (EffortName::Xhigh, "xhigh"),
        ] {
            assert_eq!(
                effort_argv(AgentKind::Grok, Some(sel(name, false))).unwrap(),
                vec!["--reasoning-effort".to_string(), value.to_string()]
            );
        }
        assert!(effort_argv(AgentKind::Grok, Some(sel(EffortName::Minimal, false))).is_err());
        assert!(effort_argv(AgentKind::Grok, Some(sel(EffortName::Max, false))).is_err());
        assert!(effort_argv(AgentKind::Grok, Some(sel(EffortName::Xhigh, true))).is_err());
    }

    #[test]
    fn extras_cannot_smuggle_effort_flags() {
        for kind in [AgentKind::Claude, AgentKind::Codex, AgentKind::Grok] {
            assert!(ensure_no_effort_in_extras(kind, &[]).is_ok());
            assert!(ensure_no_effort_in_extras(kind, &["--effort=xhigh".into()]).is_err());
        }
        assert!(
            ensure_no_effort_in_extras(AgentKind::Codex, &["--reasoning-effort=high".into()])
                .is_err()
        );
        assert!(
            ensure_no_effort_in_extras(
                AgentKind::Grok,
                &["--reasoning-effort".into(), "high".into()]
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod sync_tests {
    use super::*;

    #[test]
    fn vocabulary_round_trips_through_the_command_word() {
        for (level, word, observed) in [
            ("low", "/effort low", EffortName::Low),
            ("high", "/effort high", EffortName::High),
            ("xhigh", "/effort xhigh", EffortName::Xhigh),
            ("max", "/effort max", EffortName::Max),
            ("ultracode", "/effort ultracode", EffortName::Xhigh),
        ] {
            let request = EffortRequest::from_level(level).expect(level);
            assert_eq!(request.command_body(), word);
            assert_eq!(request.observed_name(), observed);
        }
        assert!(EffortRequest::from_level("bogus").is_none());
        assert!(EffortRequest::from_level("auto").is_none());
        assert!(EffortRequest::from_level("ultra").is_none());
    }

    #[tokio::test]
    async fn bridge_resolves_when_the_mapper_reports_the_level() {
        let bridge = EffortBridge::new();
        let request = EffortRequest::from_level("xhigh").unwrap();
        let generation = bridge.arm(request);
        let observed = remuda_protocol::ObservedEffort {
            name: EffortName::Xhigh,
            ultracode: None,
        };
        bridge.resolve(generation, observed);
        let got = bridge
            .wait(generation, std::time::Duration::from_secs(2))
            .await
            .expect("read-back resolves");
        match got {
            Readback::Applied(observed) => {
                assert_eq!(observed.name, EffortName::Xhigh);
            }
            other => panic!("{other:?}"),
        }
        assert!(bridge.pending().is_none());
    }

    #[tokio::test]
    async fn bridge_rejects_carry_the_reason() {
        let bridge = EffortBridge::new();
        let generation = bridge.arm(EffortRequest::from_level("xhigh").unwrap());
        bridge.reject(generation, "dialog-kept");
        match bridge
            .wait(generation, std::time::Duration::from_secs(1))
            .await
            .expect("verdict")
        {
            Readback::Rejected { reason } => assert_eq!(reason, "dialog-kept"),
            other => panic!("{other:?}"),
        }
        assert!(bridge.pending().is_none());
    }

    #[tokio::test]
    async fn bridge_times_out_when_nothing_is_observed() {
        let bridge = EffortBridge::new();
        let request = EffortRequest::from_level("max").unwrap();
        let generation = bridge.arm(request);
        assert!(
            bridge
                .wait(generation, std::time::Duration::from_millis(20))
                .await
                .is_none()
        );
        bridge.fail(generation);
        assert!(bridge.pending().is_none());
    }
}

/// Mock switch I/O recording the exact write order for unit tests.
#[cfg(test)]
mod switch_tests {
    use super::*;
    use crate::DriverResult;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct MockIo {
        writes: Mutex<Vec<String>>,
        journals: Mutex<Vec<(String, Severity)>>,
        idle: AtomicBool,
        /// Current screen; each scripted screen read overwrites this slot.
        screen: Mutex<String>,
        /// Screens served in order on successive `screen_text` calls; when the
        /// script is exhausted the last served screen persists.
        screen_script: Mutex<VecDeque<String>>,
        /// Screen text observed at each Enter write.
        cr_screens: Mutex<Vec<String>>,
        bridge: Mutex<Option<Arc<EffortBridge>>>,
        /// Resolve the read-back once this many writes have happened.
        resolve_after: usize,
        /// Reject the pending generation (with `invalid-argument`) the first
        /// time a screen containing the invalid marker is observed.
        reject_on_invalid: bool,
        invalid_rejected: AtomicBool,
        journal_count: AtomicUsize,
    }

    impl MockIo {
        fn set_idle(&self, idle: bool) {
            self.idle.store(idle, Ordering::SeqCst);
        }
        fn writes(&self) -> Vec<String> {
            self.writes.lock().unwrap().clone()
        }
        fn journals(&self) -> Vec<(String, Severity)> {
            self.journals.lock().unwrap().clone()
        }
        fn cr_screens(&self) -> Vec<String> {
            self.cr_screens.lock().unwrap().clone()
        }
        fn script(&self, screens: &[&str]) {
            *self.screen_script.lock().unwrap() = screens.iter().map(|s| (*s).to_owned()).collect();
        }
        fn maybe_resolve(&self) {
            let n = self.writes.lock().unwrap().len();
            if self.resolve_after != 0
                && n >= self.resolve_after
                && let Some(bridge) = self.bridge.lock().unwrap().clone()
                && let Some((generation, request)) = bridge.pending_with_gen()
            {
                bridge.resolve(
                    generation,
                    remuda_protocol::ObservedEffort {
                        name: request.observed_name(),
                        ultracode: None,
                    },
                );
            }
        }
        fn maybe_reject_invalid(&self, screen: &str) {
            if self.reject_on_invalid
                && screen.contains(INVALID_MARKER)
                && !self.invalid_rejected.swap(true, Ordering::SeqCst)
                && let Some(bridge) = self.bridge.lock().unwrap().clone()
                && let Some((generation, _)) = bridge.pending_with_gen()
            {
                bridge.reject(generation, "invalid-argument");
            }
        }
    }

    #[async_trait::async_trait]
    impl EffortSwitchIo for MockIo {
        async fn is_idle(&self) -> bool {
            self.idle.load(Ordering::SeqCst)
        }
        async fn type_body(&self, body: &str) -> DriverResult<()> {
            self.writes.lock().unwrap().push(format!("body:{body}"));
            self.maybe_resolve();
            Ok(())
        }
        async fn press_enter(&self) -> DriverResult<()> {
            self.cr_screens
                .lock()
                .unwrap()
                .push(self.screen.lock().unwrap().clone());
            self.writes.lock().unwrap().push("cr".into());
            self.journal_count.fetch_add(1, Ordering::SeqCst);
            self.maybe_resolve();
            Ok(())
        }
        async fn screen_text(&self) -> DriverResult<String> {
            if let Some(next) = self.screen_script.lock().unwrap().pop_front() {
                *self.screen.lock().unwrap() = next;
            }
            let screen = self.screen.lock().unwrap().clone();
            self.maybe_reject_invalid(&screen.to_ascii_lowercase());
            Ok(screen)
        }
        async fn journal(&self, status: String, severity: Severity) {
            self.journals.lock().unwrap().push((status, severity));
        }
    }

    const XHIGH_DIALOG: &str =
        "Change effort level? This conversation is cached. 1. Yes, switch to xhigh 2. No, go back";
    const HIGH_DIALOG: &str =
        "Change effort level? This conversation is cached. 1. Yes, switch to high 2. No, go back";
    const MAX_DIALOG: &str =
        "Change effort level? This conversation is cached. 1. Yes, switch to max 2. No, go back";
    const INVALID_SCREEN: &str = "Invalid argument: bogus. Valid options are: low, medium, high, xhigh, max, ultracode, auto";

    #[tokio::test]
    async fn an_idle_switch_types_body_then_two_crs_and_reports_applied() {
        let bridge = Arc::new(EffortBridge::new());
        let io = Arc::new(MockIo {
            // With a dialog the verdict lands on the confirm CR (write 3).
            resolve_after: 3,
            ..Default::default()
        });
        io.set_idle(true);
        io.screen.lock().unwrap().push_str(XHIGH_DIALOG);
        *io.bridge.lock().unwrap() = Some(Arc::clone(&bridge));
        let request = EffortRequest::from_level("xhigh").unwrap();

        let outcome = perform_switch(request, &bridge, io.as_ref()).await;
        assert_eq!(outcome, SwitchOutcome::Applied);
        // Body, submit CR, then the confirmation dialog CR — three writes.
        assert_eq!(
            io.writes(),
            vec![
                "body:/effort xhigh".to_string(),
                "cr".to_string(),
                "cr".to_string(),
            ]
        );
        assert!(
            io.journals().is_empty(),
            "applied emits no terminal journal"
        );
    }

    #[tokio::test]
    async fn no_confirmation_dialog_means_one_cr() {
        let bridge = Arc::new(EffortBridge::new());
        let io = Arc::new(MockIo {
            resolve_after: 2, // body + the single submit CR
            ..Default::default()
        });
        io.set_idle(true);
        *io.bridge.lock().unwrap() = Some(Arc::clone(&bridge));
        let request = EffortRequest::from_level("low").unwrap();
        let outcome = perform_switch(request, &bridge, io.as_ref()).await;
        assert_eq!(outcome, SwitchOutcome::Applied);
        assert_eq!(
            io.writes(),
            vec!["body:/effort low".to_string(), "cr".to_string()]
        );
    }

    #[tokio::test]
    async fn missing_read_back_degrades_and_never_claims_applied() {
        // The contract is that a silent transcript (no slash/stdout verdict)
        // cannot be claimed as applied.
        let bridge = Arc::new(EffortBridge::new());
        let io = Arc::new(MockIo::default());
        io.set_idle(true);
        let start = std::time::Instant::now();
        let outcome = perform_switch(
            EffortRequest::from_level("max").unwrap(),
            &bridge,
            io.as_ref(),
        )
        .await;
        assert_eq!(outcome, SwitchOutcome::Degraded);
        assert!(
            start.elapsed().as_millis() >= 9_000,
            "waits the bounded window: {:?}",
            start.elapsed()
        );
        // No dialog ever appears, so no rescue Enter is sent.
        assert_eq!(io.writes().len(), 2);
        assert!(io.journals().iter().any(|(status, severity)| {
            status.starts_with("effort-degraded:max") && *severity == Severity::Warning
        }));
    }

    /// c-effort3: a dialog the PREVIOUS switch left rendered must never take
    /// the next switch's confirming Enter. The CR waits for a dialog naming
    /// the NEW tier.
    #[tokio::test]
    async fn a_stale_dialog_naming_the_old_tier_never_gets_the_confirm_cr() {
        let bridge = Arc::new(EffortBridge::new());
        let io = Arc::new(MockIo {
            resolve_after: 3, // body + submit CR + the one correct confirm CR
            ..Default::default()
        });
        io.set_idle(true);
        // Pre-snapshot read and the first post-submit reads still show the
        // xhigh dialog the previous (ultracode/xhigh) switch left rendered;
        // then the screen clears and THIS switch's high dialog paints.
        io.screen.lock().unwrap().push_str(XHIGH_DIALOG);
        io.script(&[XHIGH_DIALOG, XHIGH_DIALOG, "", HIGH_DIALOG, HIGH_DIALOG]);
        *io.bridge.lock().unwrap() = Some(Arc::clone(&bridge));

        let outcome = perform_switch(
            EffortRequest::from_level("high").unwrap(),
            &bridge,
            io.as_ref(),
        )
        .await;
        assert_eq!(outcome, SwitchOutcome::Applied);
        assert_eq!(
            io.writes(),
            vec![
                "body:/effort high".to_string(),
                "cr".to_string(),
                "cr".to_string(),
            ],
            "exactly one confirm Enter, after the high dialog paints"
        );
        // The Enter that confirmed did so against the high dialog, never the
        // stale xhigh one.
        let cr_screens = io.cr_screens();
        assert_eq!(cr_screens.len(), 2, "submit CR and confirm CR");
        assert!(
            cr_screens[1].contains("switch to high"),
            "confirm CR screen: {}",
            cr_screens[1]
        );
        assert!(
            !cr_screens[1].contains("switch to xhigh"),
            "stale xhigh dialog must be gone before the confirm: {}",
            cr_screens[1]
        );
    }

    /// c-effort3: when the dialog paints only after phase 1's window (a slow
    /// repaint, an Enter that raced the paint), the rescue pass confirms it
    /// during the read-back wait and the switch still reads back Applied —
    /// never a false `no-readback-within-window`.
    #[tokio::test]
    async fn a_dialog_painting_after_phase1_is_confirmed_by_the_rescue_pass() {
        let bridge = Arc::new(EffortBridge::new());
        let io = Arc::new(MockIo {
            resolve_after: 3, // body + submit CR + rescue confirm CR
            ..Default::default()
        });
        io.set_idle(true);
        // Dialog-free through all of phase 1 (~37 reads over 1.5 s) and the
        // first rescue reads; the max dialog appears only later.
        let mut screens = vec![""];
        screens.extend(std::iter::repeat_n("", 44));
        screens.push(MAX_DIALOG);
        io.script(&screens);
        *io.bridge.lock().unwrap() = Some(Arc::clone(&bridge));

        let start = std::time::Instant::now();
        let outcome = perform_switch(
            EffortRequest::from_level("max").unwrap(),
            &bridge,
            io.as_ref(),
        )
        .await;
        assert_eq!(outcome, SwitchOutcome::Applied);
        assert_eq!(
            io.writes(),
            vec![
                "body:/effort max".to_string(),
                "cr".to_string(),
                "cr".to_string(),
            ],
            "the rescue pass sends the missing confirm Enter"
        );
        assert!(
            start.elapsed().as_millis() >= 1_400,
            "phase 1 genuinely elapsed before rescue: {:?}",
            start.elapsed()
        );
        assert!(
            start.elapsed().as_millis() < 9_000,
            "rescue lands well inside the bounded window: {:?}",
            start.elapsed()
        );
    }

    /// An invalid argument renders an error, never a dialog: no confirming
    /// Enter is sent and the bridge rejects with `invalid-argument`.
    #[tokio::test]
    async fn an_invalid_argument_gets_no_confirm_cr_and_degrades_with_the_reason() {
        let bridge = Arc::new(EffortBridge::new());
        let io = Arc::new(MockIo {
            reject_on_invalid: true,
            ..Default::default()
        });
        io.set_idle(true);
        io.script(&["", INVALID_SCREEN, INVALID_SCREEN]);
        *io.bridge.lock().unwrap() = Some(Arc::clone(&bridge));

        let outcome = perform_switch(
            EffortRequest::from_level("max").unwrap(),
            &bridge,
            io.as_ref(),
        )
        .await;
        assert_eq!(outcome, SwitchOutcome::Degraded);
        assert_eq!(
            io.writes(),
            vec!["body:/effort max".to_string(), "cr".to_string()],
            "the invalid-argument error must never receive an Enter"
        );
        assert!(
            io.journals()
                .iter()
                .any(|(status, _)| status == "effort-degraded:max:invalid-argument"),
            "{:?}",
            io.journals()
        );
    }

    #[tokio::test]
    async fn a_switch_while_working_is_queued_until_idle_then_applied() {
        let bridge = Arc::new(EffortBridge::new());
        let io = Arc::new(MockIo {
            resolve_after: 3,
            ..Default::default()
        });
        io.set_idle(false);
        // No dialog until the queued command is actually typed once idle;
        // then the real dialog paints and gets its confirm CR.
        io.script(&["", "", "", "", XHIGH_DIALOG]);
        *io.bridge.lock().unwrap() = Some(Arc::clone(&bridge));
        let queue = Arc::new(EffortQueue::new());
        let _worker = spawn_worker(
            Arc::clone(&bridge),
            Arc::clone(&queue),
            Arc::clone(&io) as Arc<dyn EffortSwitchIo>,
        );
        let request = EffortRequest::from_level("xhigh").unwrap();
        io.journal(
            SwitchOutcome::Queued.journal_status("xhigh", ""),
            Severity::Info,
        )
        .await;
        queue.enqueue(request, None);
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(io.writes().is_empty(), "nothing goes out while working");
        io.set_idle(true);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(3_000);
        while io.writes().len() < 3 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(
            io.writes(),
            vec![
                "body:/effort xhigh".to_string(),
                "cr".to_string(),
                "cr".to_string(),
            ]
        );
    }
}
