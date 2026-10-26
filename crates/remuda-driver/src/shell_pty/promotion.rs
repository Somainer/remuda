//! Promotion supervisor for [`super::ShellPtyDriver`] (D-025).
//!
//! One task per live PTY. It samples the foreground process group, journals
//! promote / demote transitions, and — for Claude — tails the native transcript
//! of the session the foreground process actually owns, so the structured view
//! is that session's real conversation rather than the busiest file in the
//! slug directory.
//!
//! ## Transcript binding (deterministic, never guessed)
//!
//! Binding is owned by [`BindingHandle`], shared between this task and the
//! driver's `respond_interaction`. Within one promotion epoch it claims a file
//! only through exact identity, in this precedence:
//!
//! 1. argv `--session-id` / `--resume <uuid>` ([`detect`] already parses it);
//! 2. the SessionStart hook whose `ppid` is the foreground pid;
//! 3. `~/.claude/sessions/<pid>.json`, the pid-keyed registry Claude writes;
//! 4. nothing — the epoch stays **unbound**, hydrates no transcript, and the
//!    human is offered an explicit picker ([`offer_picker`]); nothing is
//!    auto-picked.
//!
//! The claim is locked for the epoch: a later poll never switches files, even
//! when another session in the same cwd becomes newer. Demotion or a different
//! foreground pid starts a new epoch and resets the tail and hydrated state.

use super::{PROMOTE_POLL, PtyState};

/// Release build: `spawn` returns just the task handle.
#[cfg(not(test))]
pub(crate) type PollerTask = tokio::task::JoinHandle<()>;
use crate::claude_print::TranscriptMapper;
use crate::claude_pty::now_ts;
use crate::claude_transcript::{
    ResumeBoundary, ResumeMode, SessionStartReport, TailAnchor, TailProvenance, TranscriptBinding,
    TranscriptCandidate, TranscriptTail, bind_by_pid_file, bind_by_session_id, bind_manual,
    cwd_matches, list_candidates, recorded_cwd, transcript_belongs_to_cwd,
};
use crate::error::{DriverError, DriverResult};
use crate::promote::{
    Detected, LaunchAlias, ProcessRow, ProcessTable, ScreenStatus, detect, foreground_pgid,
};
use crate::screenlive::{live_status_inactive, live_status_payload, now_utc};
use crate::tty::{LocalPty as _, logical_keys_to_bytes};
use remuda_protocol::{
    AgentKind, Completeness, DeadlineSource, DeliveryState, EffortSelection, EntityLifecycle,
    EntityMeta, EventId, HostId, Id, InstanceId, Interaction, InteractionAnswer,
    InteractionCarrier, InteractionId, InteractionKind, InteractionRequest, InteractionRequestKey,
    InteractionRequestedPayload, InteractionState, Knowledge, LifecycleEntity, LifecyclePayload,
    LifecycleTopic, NativeLifecycle, NativeRequestKey, Observation, ObservationPayload,
    ObservationSource, QuestionField, QuestionInput, QuestionOption, QuestionRequest, RunId,
    RuntimeCursor, SchemaVersion, Severity, SourceChannel, SourceCursor, SourceDelivery, Timestamp,
    U64,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Everything the poller needs to stamp Observations for one instance.
#[derive(Clone)]
pub(super) struct PromoteCtx {
    pub(super) instance_id: InstanceId,
    pub(super) host_id: HostId,
    pub(super) journal_id: Id,
    pub(super) run_id: RunId,
    /// Absolute cwd; the Claude transcript directory is derived from it.
    pub(super) cwd: PathBuf,
    /// `$HOME/.claude` (or `CLAUDE_CONFIG_DIR`) for transcript lookup.
    pub(super) claude_home: PathBuf,
}

/// Native diagnostic name for a detection transition.
pub(super) const DETECTED: &str = "agent_detected";
/// Native lifecycle name Node folds into `kind` / `mode`.
pub(super) const PROMOTED: &str = "agent_promoted";
/// Native lifecycle name that restores `kind: terminal`.
pub(super) const DEMOTED: &str = "agent_demoted";

/// Lifecycle announcing the deterministic transcript binding for an epoch.
pub(super) const TRANSCRIPT_BOUND: &str = "transcript_bound";
/// Lifecycle announcing an epoch with no deterministic binding (picker shown).
pub(super) const TRANSCRIPT_UNBOUND: &str = "transcript_unbound";
/// Lifecycle announcing a bound file that vanished or failed its cwd check.
pub(super) const TRANSCRIPT_DEGRADED: &str = "transcript_degraded";

const SHIM_BYPASSED: &str = "claude_shim_bypassed";

/// Wire name for an agent kind, matching the `AgentKind` wire values.
pub(super) fn kind_name(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Claude => "claude",
        AgentKind::Codex => "codex",
        AgentKind::Grok => "grok",
        AgentKind::Agy => "agy",
        AgentKind::Generic => "generic",
        AgentKind::Terminal => "terminal",
    }
}

/// Question field id the picker answer arrives under.
const PICKER_FIELD: &str = "transcript";
/// How often the unbound epoch rescans the slug dir for new candidates.
const CANDIDATE_RESCAN: Duration = Duration::from_secs(3);
/// A hook tier that has delivered no record for the foreground pid for this
/// many polls no longer vouches for a live turn, so the screen's idle edge may
/// lower busy (rule 6 stays in force while hooks are fresh). A small multiple
/// of the 800 ms poll cadence — ≈ the web's 3 × 2 s hook-stall budget — rides
/// a normal event gap (a long tool is quiet for tens of seconds) but releases
/// a channel that has genuinely stopped: a deleted pinned relay, a stalled
/// journal pump, or a session that finished without emitting a Stop hook.
const HOOK_STALL_POLLS: u32 = 8;
/// Polls between hook-silence probe runs (8 × 800 ms ≈ 6.4 s) — sparse, since a
/// probe makes a bounded connect to the instance hook socket.
const HOOK_SILENCE_PROBE_TICKS: u32 = 8;
/// Consecutive idle screen polls required to lower busy once the hook tier is
/// stale (3 × 800 ms ≈ 2.4 s). A finished turn shows a stable idle prompt on
/// every poll; a single missed spinner repaint never closes a live tool.
const SCREEN_IDLE_CONFIRM_POLLS: u32 = 3;

/// Current promotion state. Transitions are journaled; steady state is silent.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct PromoteState {
    /// Promoted kind, when promoted.
    pub(super) kind: Option<AgentKind>,
    /// Pid of the matched agent, used to notice a restart within one kind.
    pub(super) pid: Option<i32>,
}

/// Shared binding state: the poller mutates it each tick, the driver answers
/// the manual picker through it, and (once D-028's launch shim lands) SessionStart
/// hook reports are ingested into it by the driver.
#[derive(Clone)]
pub(super) struct BindingHandle {
    inner: Arc<Mutex<Slot>>,
}

/// Picker offered once per unbound epoch.
struct Picker {
    /// Stable interaction identity; Node dedups on it if a tick re-emits.
    interaction_id: InteractionId,
    /// Full entity, retained so a later demote/bind can retire it.
    interaction: Interaction,
    /// Option ids (session ids) the human is allowed to answer with.
    option_ids: Vec<String>,
    /// Whether the request was put on the event channel yet.
    emitted: bool,
}

struct Slot {
    cwd: PathBuf,
    claude_home: PathBuf,
    /// Bumped on every promotion; claims do not survive across epochs.
    #[allow(dead_code)]
    epoch: u64,
    /// The one locked claim for this epoch, once established.
    binding: Option<TranscriptBinding>,
    /// The claim proved unusable (file vanished, content cwd mismatch). It is
    /// kept so a later poll can never silently swap files underneath the user.
    degraded: bool,
    /// Why it degraded, surfaced on the lifecycle / header chip.
    degraded_reason: String,
    /// Content-record cwd has already been cross-checked against the terminal.
    cwd_checked: bool,
    /// Manual-picker state, while the epoch is unbound (or just-bound, until
    /// the poller retires the question).
    picker: Option<Picker>,
    /// Last time the slug dir was scanned for picker candidates.
    candidate_scan_at: Option<Instant>,
    /// SessionStart hook reports not yet matched to a foreground pid.
    hooks: Vec<SessionStartReport>,
    /// c-effortread r8 item 2: SessionStart hook `source` keyed by
    /// `(ppid, session_id)` as reported at delivery. Survives `hooks.clear()`
    /// on a rebind so the sticky mode for the rebound session (recorded on a
    /// later tick) can still classify clear/startup vs resume without sizing
    /// a transcript that already holds the queued prompt.
    start_sources: std::collections::HashMap<(i64, String), String>,
}

impl BindingHandle {
    /// An unconfigured handle; [`Self::begin_epoch`] supplies the paths when a
    /// PTY spawns and again each time a new agent takes the foreground.
    pub(super) fn empty() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Slot {
                cwd: PathBuf::from("."),
                claude_home: PathBuf::from("."),
                epoch: 0,
                binding: None,
                degraded: false,
                degraded_reason: String::new(),
                cwd_checked: false,
                picker: None,
                candidate_scan_at: None,
                hooks: Vec::new(),
                start_sources: std::collections::HashMap::new(),
            })),
        }
    }

    /// Start a fresh promotion epoch: drop every claim, the degraded flag, the
    /// picker, and unmatched hook reports, and record the identity paths.
    fn begin_epoch(&self, cwd: &Path, claude_home: &Path) {
        if let Ok(mut slot) = self.inner.lock() {
            slot.epoch = slot.epoch.saturating_add(1);
            slot.cwd = cwd.to_path_buf();
            slot.claude_home = claude_home.to_path_buf();
            slot.binding = None;
            slot.degraded = false;
            slot.degraded_reason.clear();
            slot.cwd_checked = false;
            slot.picker = None;
            slot.candidate_scan_at = None;
            slot.hooks.clear();
            slot.start_sources.clear();
        }
    }

    /// Drop epoch state without bumping it (used when the agent leaves the
    /// foreground). The caller first retires any outstanding picker.
    pub(super) fn demobilize(&self) {
        if let Ok(mut slot) = self.inner.lock() {
            slot.binding = None;
            slot.degraded = false;
            slot.degraded_reason.clear();
            slot.cwd_checked = false;
            slot.picker = None;
            slot.candidate_scan_at = None;
            slot.hooks.clear();
            // start_sources deliberately survive demobilize: a demote followed
            // by re-promotion of the same pid (r7 item 4c) must keep the
            // /clear-vs-/resume classification; begin_epoch (a genuinely new
            // foreground epoch) clears them.
        }
    }

    /// Consume a SessionStart hook report (channel A).
    ///
    /// It only binds later if its `ppid` equals a detected foreground pid;
    /// reports from other sessions in the same cwd are retained but ignored.
    /// Duplicate reports (same ppid + session) are dropped.
    pub(super) fn ingest_session_start(&self, report: SessionStartReport) {
        if let Ok(mut slot) = self.inner.lock() {
            let duplicate = slot.hooks.iter().any(|existing| {
                existing.ppid == report.ppid && existing.session_id == report.session_id
            });
            if let (Some(ppid), Some(source)) = (report.ppid, report.start_source.as_ref()) {
                // Last source wins per (pid, session); survives hooks.clear()
                // (c-effortread r8 item 2).
                slot.start_sources
                    .insert((ppid, report.session_id.clone()), source.clone());
            }
            if !duplicate {
                slot.hooks.push(report);
            }
        }
    }

    /// An authenticated fresh SessionStart can describe an exec-style renderer
    /// relaunch: the PID is unchanged but its native session/transcript changed.
    /// This is explicit identity evidence, never an mtime-based rebind.
    fn rebind_authenticated_start(&self, found: &Detected, report: &SessionStartReport) -> bool {
        let Ok(mut slot) = self.inner.lock() else {
            return false;
        };
        let Some(candidate) = report.bind(found.pid) else {
            return false;
        };
        let Some(previous) = &slot.binding else {
            return false;
        };
        if previous.session_id == candidate.session_id
            || !transcript_belongs_to_cwd(&slot.claude_home, &slot.cwd, &candidate.path)
        {
            return false;
        }
        slot.epoch = slot.epoch.saturating_add(1);
        slot.binding = Some(candidate);
        slot.degraded = false;
        slot.degraded_reason.clear();
        slot.cwd_checked = false;
        slot.hooks.clear();
        true
    }

    /// Bind an authenticated SessionStart report on an epoch's FIRST claim
    /// when no binding exists yet.
    ///
    /// The argv session id is stale the moment the user runs `/resume` or
    /// `/clear` in the TUI (the process argv never changes), and with the
    /// transcript created lazily the hook report can arrive before any other
    /// channel binds: without this, `resolve`'s argv arm would lock the OLD
    /// launch session while the live session is the one the authenticated
    /// report names. The same cwd/pid gates as a rebind apply; the epoch is
    /// NOT bumped (this is its first claim).
    fn bind_authenticated_report(&self, found: &Detected, report: &SessionStartReport) -> bool {
        let Ok(mut slot) = self.inner.lock() else {
            return false;
        };
        if slot.binding.is_some() {
            return false;
        }
        let Some(candidate) = report.bind(found.pid) else {
            return false;
        };
        if !transcript_belongs_to_cwd(&slot.claude_home, &slot.cwd, &candidate.path) {
            return false;
        }
        slot.binding = Some(candidate);
        slot.degraded = false;
        slot.degraded_reason.clear();
        slot.cwd_checked = false;
        true
    }

    /// Current locked binding, if any.
    ///
    /// c-effortread r15 item 4: visible to the parent `shell_pty` module so
    /// the effort I/O can stat the bound transcript's EOF for a switch's
    /// submission floor.
    pub(super) fn binding(&self) -> Option<TranscriptBinding> {
        self.inner.lock().ok().and_then(|slot| slot.binding.clone())
    }

    /// c-effortread r8 item 2: the SessionStart hook `source` for
    /// `(pid, session_id)` captured at delivery (survives rebind clears).
    fn start_source_for(&self, pid: i32, session_id: &str) -> Option<String> {
        self.inner.lock().ok().and_then(|slot| {
            slot.start_sources
                .get(&(i64::from(pid), session_id.to_owned()))
                .cloned()
        })
    }

    /// Whether the locked binding has been marked degraded.
    fn degraded(&self) -> bool {
        self.inner.lock().is_ok_and(|slot| slot.degraded)
    }

    /// Mark the locked binding degraded; never rebound within the epoch.
    fn mark_degraded(&self, reason: &str) {
        if let Ok(mut slot) = self.inner.lock()
            && slot.binding.is_some()
        {
            slot.degraded = true;
            slot.degraded_reason = reason.to_owned();
        }
    }

    /// Run the deterministic channel precedence once. Returns a claim only on
    /// the first success of an unbound, non-degraded epoch; later calls keep
    /// returning the existing claim, so a newer other session can never win.
    fn resolve(&self, found: &Detected) -> Option<TranscriptBinding> {
        let mut slot = self.inner.lock().ok()?;
        if slot.degraded {
            return slot.binding.clone();
        }
        if let Some(existing) = &slot.binding {
            return Some(existing.clone());
        }
        let cwd = slot.cwd.clone();
        let home = slot.claude_home.clone();

        // 1. Explicit argv session id (`--session-id` / `--resume <uuid>`).
        if let Some(argv_id) = found.session_id.as_deref()
            && let Some(binding) = bind_by_session_id(&home, &cwd, argv_id)
        {
            slot.binding = Some(binding.clone());
            return Some(binding);
        }
        // 2. SessionStart hook whose parent pid is the foreground pid and whose
        //    transcript lives in this cwd's slug dir.
        let hook_binding = slot.hooks.iter().find_map(|report| {
            let candidate = report.bind(found.pid)?;
            transcript_belongs_to_cwd(&home, &cwd, &candidate.path).then_some(candidate)
        });
        if let Some(binding) = hook_binding {
            slot.binding = Some(binding.clone());
            return Some(binding);
        }
        // 3. The pid-keyed `~/.claude/sessions/<pid>.json` registry.
        if let Some(binding) = bind_by_pid_file(&home, found.pid, &cwd) {
            slot.binding = Some(binding.clone());
            return Some(binding);
        }
        None
    }

    /// Cross-check the bound transcript's own `cwd` record against the terminal
    /// cwd. `Ok(true)` = matched or not yet decidable; `Ok(false)` = mismatch.
    fn content_cwd_check(&self) -> bool {
        let Ok(mut slot) = self.inner.lock() else {
            return true;
        };
        let Some(binding) = slot.binding.clone() else {
            return true;
        };
        if slot.cwd_checked {
            return true;
        }
        // No cwd-bearing record yet: the identity channels already proved the
        // file, so this stays indeterminate rather than rejecting it.
        if recorded_cwd(&binding.path).is_none() {
            return true;
        }
        slot.cwd_checked = true;
        cwd_matches(&binding.path, &slot.cwd)
    }

    /// Build (once per rescan) the manual picker question for an unbound epoch.
    ///
    /// Returns the interaction to emit; `None` when candidates are unavailable
    /// or the epoch became bound/degraded. Nothing here auto-selects.
    fn offer_picker(&self, ctx: &PromoteCtx) -> Option<Interaction> {
        let mut slot = self.inner.lock().ok()?;
        if slot.binding.is_some() || slot.degraded {
            return None;
        }
        let due = slot
            .candidate_scan_at
            .is_none_or(|at| at.elapsed() >= CANDIDATE_RESCAN);
        if !due {
            return None;
        }
        slot.candidate_scan_at = Some(Instant::now());
        let candidates: Vec<TranscriptCandidate> = list_candidates(&slot.claude_home, &slot.cwd);
        if candidates.is_empty() {
            return None;
        }
        // First scan builds the question; later scans leave it stable.
        if slot.picker.is_none() {
            let options: Vec<QuestionOption> = candidates
                .iter()
                .map(|candidate| QuestionOption {
                    id: candidate.session_id.clone(),
                    label: candidate.label(),
                    description: None,
                })
                .collect();
            let interaction = picker_interaction(ctx, options.clone()).ok()?;
            slot.picker = Some(Picker {
                interaction_id: interaction.meta.id.clone(),
                interaction: interaction.clone(),
                option_ids: options.iter().map(|option| option.id.clone()).collect(),
                emitted: false,
            });
        }
        let picker = slot.picker.as_mut()?;
        if picker.emitted {
            return None;
        }
        picker.emitted = true;
        Some(picker.interaction.clone())
    }

    /// If the epoch just gained a binding (auto or manual) while the picker was
    /// pending, take the entity lifecycle that retires the question.
    fn take_picker_retirement(&self, state: &str, reason: &str) -> Option<ObservationPayload> {
        let mut slot = self.inner.lock().ok()?;
        let picker = slot.picker.take()?;
        retire_payload(picker.interaction, state, reason)
    }

    /// Retire the pending picker with no binding (demotion / epoch reset).
    fn invalidate_picker(&self) -> Option<ObservationPayload> {
        self.take_picker_retirement("invalidated", "agent-demoted")
    }

    /// Apply the human's picker answer: validate it against the offered options
    /// and bind that exact session. First-answer-wins.
    pub(super) fn answer(
        &self,
        id: &InteractionId,
        answer: &InteractionAnswer,
    ) -> Result<(), String> {
        let InteractionAnswer::Question(question) = answer else {
            return Err("transcript picker expects a question answer".into());
        };
        let field = question
            .answers
            .get(PICKER_FIELD)
            .ok_or_else(|| "answer does not address the transcript picker".to_string())?;
        let chosen = field
            .option_ids
            .first()
            .ok_or_else(|| "choose one transcript from the list".to_string())?;
        let mut slot = self
            .inner
            .lock()
            .map_err(|_| "binding state unavailable".to_string())?;
        let picker = slot
            .picker
            .as_ref()
            .filter(|picker| &picker.interaction_id == id)
            .ok_or_else(|| "no transcript picker is waiting for that interaction".to_string())?;
        if !picker.option_ids.iter().any(|option| option == chosen) {
            return Err("the chosen transcript is not one of the offered sessions".to_string());
        }
        if slot.binding.is_some() {
            return Err("a transcript was already bound for this session".to_string());
        }
        let binding = bind_manual(&slot.claude_home, &slot.cwd, chosen)
            .ok_or_else(|| "the chosen transcript no longer exists".to_string())?;
        slot.binding = Some(binding);
        slot.degraded = false;
        Ok(())
    }
}

/// Fold one detection sample into the state machine.
///
/// Returns the lifecycle payloads to journal. A steady state — same kind, same
/// pid, or still nothing — returns empty, which is what makes both promotion
/// and demotion idempotent.
pub(super) fn transition(
    state: &mut PromoteState,
    found: Option<&Detected>,
    at: &Timestamp,
) -> Vec<(Completeness, ObservationPayload)> {
    let next = found.map(|found| (found.kind, found.pid));
    let current = state.kind.map(|kind| (kind, state.pid.unwrap_or_default()));
    if current == next {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some(previous) = state.kind {
        out.push(demoted(previous));
    }
    match found {
        Some(found) => {
            out.push(detected(found));
            out.push(promoted(found, at));
            state.kind = Some(found.kind);
            state.pid = Some(found.pid);
        }
        None => {
            out.push(detected_none());
            state.kind = None;
            state.pid = None;
        }
    }
    out
}

fn native(
    topic: LifecycleTopic,
    name: &str,
    native_id: Knowledge<String>,
    status: &str,
    related: BTreeMap<String, String>,
    severity: Severity,
) -> (Completeness, ObservationPayload) {
    (
        Completeness::Structured,
        ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
            NativeLifecycle {
                topic,
                native_name: name.to_owned(),
                native_id,
                status: Knowledge::Known {
                    value: status.to_owned(),
                },
                related_ids: related,
                data_ref: None,
                severity,
                affects_completion: false,
            },
        )))),
    )
}

fn detected(found: &Detected) -> (Completeness, ObservationPayload) {
    let mut related = BTreeMap::new();
    related.insert("kind".into(), kind_name(found.kind).to_owned());
    related.insert("pid".into(), found.pid.to_string());
    if let Some(session) = &found.session_id {
        related.insert("sessionId".into(), session.clone());
    }
    native(
        LifecycleTopic::Diagnostic,
        DETECTED,
        Knowledge::NotApplicable,
        &format!("agent detected: {}", kind_name(found.kind)),
        related,
        Severity::Info,
    )
}

fn detected_none() -> (Completeness, ObservationPayload) {
    native(
        LifecycleTopic::Diagnostic,
        DETECTED,
        Knowledge::NotApplicable,
        "agent detected: none",
        BTreeMap::new(),
        Severity::Info,
    )
}

fn promoted(found: &Detected, at: &Timestamp) -> (Completeness, ObservationPayload) {
    let mut related = BTreeMap::new();
    related.insert("kind".into(), kind_name(found.kind).to_owned());
    related.insert("pid".into(), found.pid.to_string());
    related.insert("mode".into(), "promoted".to_owned());
    related.insert("promotedAt".into(), String::from(at.clone()));
    if let Some(session) = &found.session_id {
        related.insert("sessionId".into(), session.clone());
    }
    native(
        LifecycleTopic::Session,
        PROMOTED,
        Knowledge::NotApplicable,
        kind_name(found.kind),
        related,
        Severity::Info,
    )
}

fn demoted(previous: AgentKind) -> (Completeness, ObservationPayload) {
    let mut related = BTreeMap::new();
    related.insert("kind".into(), "terminal".to_owned());
    related.insert("mode".into(), "native".to_owned());
    related.insert("previousKind".into(), kind_name(previous).to_owned());
    native(
        LifecycleTopic::Session,
        DEMOTED,
        Knowledge::NotApplicable,
        "terminal",
        related,
        Severity::Info,
    )
}

/// `transcript_bound` lifecycle: which exact session is hydrated and by which
/// deterministic channel, so the header chip can say so.
fn bound_lifecycle(binding: &TranscriptBinding) -> ObservationPayload {
    let mut related = BTreeMap::new();
    related.insert("sessionId".into(), binding.session_id.clone());
    related.insert(
        "transcriptPath".into(),
        binding.path.to_string_lossy().into_owned(),
    );
    related.insert("source".into(), binding.source.as_wire().to_owned());
    native(
        LifecycleTopic::Session,
        TRANSCRIPT_BOUND,
        Knowledge::Known {
            value: binding.session_id.clone(),
        },
        binding.source.as_wire(),
        related,
        Severity::Info,
    )
    .1
}

fn diagnostic_lifecycle(
    name: &str,
    status: &str,
    related: BTreeMap<String, String>,
) -> ObservationPayload {
    native(
        LifecycleTopic::Diagnostic,
        name,
        Knowledge::NotApplicable,
        status,
        related,
        Severity::Info,
    )
    .1
}

/// Build the manual transcript-picker question entity.
fn picker_interaction(ctx: &PromoteCtx, options: Vec<QuestionOption>) -> DriverResult<Interaction> {
    let ts = now_ts()?;
    Ok(Interaction {
        meta: EntityMeta {
            id: InteractionId::new(),
            revision: U64(1),
            created_at: ts.clone(),
            updated_at: ts,
        },
        instance_id: ctx.instance_id.clone(),
        host_id: ctx.host_id.clone(),
        run_id: Some(ctx.run_id.clone()),
        kind: InteractionKind::Question,
        request_key: InteractionRequestKey {
            // This question is Remuda's own; there is no native request key.
            native: NativeRequestKey::None,
            process_generation: U64(1),
            run_generation: Some(U64(1)),
            connection_epoch: Id::new("epoch")?,
        },
        request_version: U64(1),
        state: InteractionState::Pending,
        // Informational: it must not block normal use of the live terminal.
        blocking: false,
        answerable: true,
        carrier: InteractionCarrier::NativeTty,
        request: InteractionRequest::Question(Box::new(QuestionRequest {
            title: "绑定 Claude transcript".into(),
            fields: vec![QuestionField {
                id: PICKER_FIELD.into(),
                title: "这个终端对应哪个会话？".into(),
                description: Some(
                    "无法通过 hook、进程 pid 或 argv 确定唯一会话；请显式选择，Remuda 不会自动猜测。"
                        .into(),
                ),
                input: QuestionInput::SingleSelect,
                required: true,
                options,
                allow_free_text: false,
                sensitive: false,
            }],
        })),
        deadline: Knowledge::Unknown {
            reason: "manual-bind".into(),
            evidence_event_ids: Vec::new(),
        },
        deadline_source: DeadlineSource::None,
        answer: Knowledge::Unknown {
            reason: "pending".into(),
            evidence_event_ids: Vec::new(),
        },
        delivery: DeliveryState::NotSent,
        resolution: Knowledge::Unknown {
            reason: "pending".into(),
            evidence_event_ids: Vec::new(),
        },
    })
}

/// Entity lifecycle that retires the picker question (answer, auto-bind, demote).
fn retire_payload(
    mut interaction: Interaction,
    state: &str,
    reason: &str,
) -> Option<ObservationPayload> {
    let resolved = state == "resolved";
    interaction.state = if resolved {
        InteractionState::Resolved
    } else {
        InteractionState::Invalidated
    };
    interaction.blocking = false;
    interaction.answerable = false;
    interaction.meta.revision.0 += 1;
    interaction.meta.updated_at = now_ts().ok()?;
    Some(ObservationPayload::Lifecycle(Box::new(
        LifecyclePayload::Entity(Box::new(EntityLifecycle {
            entity_id: interaction.meta.id.as_id().clone(),
            revision: interaction.meta.revision,
            previous_state: Some("pending".into()),
            state: state.to_owned(),
            reason_code: reason.to_owned(),
            evidence_event_ids: Vec::new(),
            entity_value: LifecycleEntity::Interaction(Box::new(interaction)),
        })),
    )))
}

/// §9.1 model-sync inputs threaded into the promotion hydrator, mirroring the
/// effort bridge. `launch`/`catalog` seed the launch-time model snapshot the
/// picker reads its real list from.
pub(super) struct ModelSync {
    pub(super) bridge: Arc<crate::model::ModelBridge>,
    pub(super) launch: Option<String>,
    pub(super) catalog: Option<remuda_protocol::ModelCatalogInfo>,
}

/// Decide the hook tier's veto strength over the screen's idle edge (rule 6,
/// design §2.4/§2.6).
///
/// `turn_active` is the HookSession's boundary decision (`Some(true)` a live
/// hook turn, `Some(false)` a hook-confirmed end, `None` no bound turn this
/// poll); `ever_live` is the sticky "this epoch materialised a hook tier" bit;
/// `fresh` is whether a hook record from the foreground pid arrived inside the
/// freshness bound. Only a fresh, still-claimed turn holds the screen at
/// `working`: a once-live channel that has gone quiet downgrades to
/// [`HookHealth::Stalled`], letting the screen lower busy when it reads idle —
/// the missing-Stop hole, without weakening hook precedence while hooks speak.
fn hook_health_for(
    turn_active: Option<bool>,
    ever_live: bool,
    fresh: bool,
) -> remuda_screen::HookHealth {
    use remuda_screen::HookHealth;
    match turn_active {
        Some(true) if fresh => HookHealth::Healthy,
        Some(true) => HookHealth::Stalled,
        Some(false) => HookHealth::NeverMaterialised,
        None if ever_live && fresh => HookHealth::Healthy,
        None if ever_live => HookHealth::Stalled,
        None => HookHealth::NeverMaterialised,
    }
}

/// Files the hook-silence diagnostic probes: the pinned relay the hook execs
/// and the per-instance hook socket it connects to.
#[derive(Clone, Debug)]
pub(super) struct HookSilencePaths {
    pub relay: std::path::PathBuf,
    pub socket: std::path::PathBuf,
}

/// Name the check that explains a quiet hook tier at a screen-idle end, or
/// `None` when there is nothing honest to say.
///
/// A *fresh* tier returns `None` unconditionally: a clean `Stop` that just
/// landed is a normal end and must never write a `hook.silence` record. Of the
/// probes this screen poller can actually run, the missing pinned relay wins
/// over an unbound socket; when both are healthy there is `None` — a finished
/// session emits no hooks, and judging the journal pump needs the Node
/// transport's flush cursor, which this layer does not hold (so this never
/// reports `link-stalled`).
fn silence_reason_for(
    hook_fresh: bool,
    relay_executable: bool,
    socket_listening: bool,
) -> Option<remuda_signal::hook_silence::HookSilenceReason> {
    use remuda_signal::hook_silence::HookSilenceReason;
    if hook_fresh {
        return None;
    }
    if !relay_executable {
        return Some(HookSilenceReason::RelayMissing);
    }
    if !socket_listening {
        return Some(HookSilenceReason::SocketRefused);
    }
    None
}

/// Spawn the per-instance promotion poller.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn(
    state: Arc<PtyState>,
    ctx: PromoteCtx,
    table: Arc<dyn ProcessTable>,
    current: Arc<std::sync::Mutex<Option<Detected>>>,
    status_slot: Arc<std::sync::Mutex<Option<ScreenStatus>>>,
    bindings: BindingHandle,
    hooks: Option<Arc<crate::launch::HookSession>>,
    expect_shim: bool,
    // Answer Claude's exact folder-trust dialog with its documented key
    // sequence, once per promoted epoch. Granted by the Node only when the cwd
    // is inside a registered workspace root (D-022).
    auto_trust_workspace: bool,
    interrupt_pid: Arc<AtomicI32>,
    interrupt_screen_markers: Arc<Mutex<InterruptBaseline>>,
    // c-usagefu r12 item 1 (Ruling A2'): driver-owned slot collecting the
    // promoted-codex lifetime OWNER guards this poller registers. The poller
    // only ever pushes a fresh guard when a codex pid is detected; it never
    // closes one. The driver drains and closes them after the stop ladder
    // confirms termination, so the lifetime is owned by the session rather
    // than by the poller or foreground detection.
    promoted_lifetimes: Arc<std::sync::Mutex<Vec<crate::adapters::codex_discovery::LifetimeGuard>>>,
    // Census epoch of the Node that owns promoted intervals this poller
    // registers (c-usagefu r18 item 3).
    node_epoch: Option<u64>,
    events: mpsc::Sender<Observation>,
    seq: Arc<AtomicU64>,
    effort_bridge: Option<Arc<crate::effort::EffortBridge>>,
    launch_effort: Option<remuda_protocol::EffortSelection>,
    model: Option<ModelSync>,
    permission_bridge: Option<Arc<crate::permission::PermissionBridge>>,
    launch_permission: Option<remuda_protocol::ClaudePermissionMode>,
    // Pre-spawn resume mode for a Remuda-launched `--resume` agent, captured
    // before the child spawned (Fresh for a new session, Boundary for a proven
    // resume, Unverified for a known-but-unprovable resume). `None` for a
    // login shell, where the mode is derived per detected foreground process.
    pre_resume_mode: Option<ResumeMode>,
    // The exact executable this launch exec'd, when Remuda launched one
    // (c-wfdrill2 B). `None` for a login shell.
    alias: Option<LaunchAlias>,
    // Per-instance hook relay/socket paths for the silence diagnostic; absent
    // on a launch with no hook tier.
    silence_paths: Option<HookSilencePaths>,
    // c-ctxusage r5 item 6: cooperative close that wakes the loop even between
    // poll ticks; the loop finalises its hydrator and signals `finalised` once
    // that is done, so close never aborts a first-bind pump mid-`finish()`.
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    finalised: tokio::sync::oneshot::Sender<()>,
) -> PollerTask {
    #[cfg(not(any(test, feature = "test-stub")))]
    type PollerTask = tokio::task::JoinHandle<()>;
    // Abort-path safety net: even if the task is aborted while a first-bind
    // pump is still mapping a large transcript, the guard's Drop runs the
    // mapper's synchronous `finish()` and best-effort delivers the
    // observations via try_send before its sender drops. The variable is read
    // only by its Drop impl.

    // c-usagefu r18 item 5 + r20 items 1/2: the guard OWNS the live hydrator
    // for the whole poll loop and the mapped-but-unpublished finalize queue;
    // the task below only borrows the hydrator (`hydrator_slot`). When the
    // task is aborted — mid-loop on a wedged pump, or INSIDE finalization on
    // close's deadline — the guard's Drop delivers whatever remains.
    //
    // CANCELLATION-SAFETY RULE (c-usagefu r20): nothing that must be
    // published may live only in a future's LOCAL state across an await:
    // (a) the hydrator is borrowed until production has COMPLETED and leaves
    //     the guard slot only after its final read is mapped and
    //     mapper.finish() has run;
    // (b) the final tail read and mapper.finish() land in guard-owned pending
    //     storage BEFORE any awaited publication;
    // (c) publication awaits channel CAPACITY (reserve) while the observation
    //     is still in the guard, then pops and permit.send()s synchronously —
    //     no cancellation point between removal and hand-off;
    // (d) demote/rebind finalization produces and publishes through the same
    //     guard storage.
    //
    // The pending queue is a task-owned Arc shared with the guard, so the loop
    // can use it while it holds the `&mut guard.hydrator` reborrow.
    let finalise_pending: Arc<std::sync::Mutex<Vec<Observation>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    // c-usagefu r21 item 3: a PER-SPAWN cancellation-test gate (permit +
    // stage + pending-permit acknowledgement). Unlike a process-global gate,
    // another poller's finalization cannot satisfy this driver's wait.
    #[cfg(test)]
    let finalise_gate: Arc<FinalisePublishGate> = Arc::new(FinalisePublishGate::new());
    let mut finalise_guard = FinaliseGuard::new(
        events.clone(),
        Arc::clone(&seq),
        ctx.clone(),
        Arc::clone(&finalise_pending),
    );
    #[cfg(test)]
    let task_finalise_gate: Arc<FinalisePublishGate> = Arc::clone(&finalise_gate);
    let handle = tokio::spawn({
        #[cfg(test)]
        let finalise_gate = Arc::clone(&task_finalise_gate);
        async move {
            let mut promote = PromoteState::default();
            // Borrowed ownership of the live hydrator — created, replaced and
            // finalised through this reborrow but stored in `finalise_guard`.
            let hydrator: &mut Option<Hydrator> = &mut finalise_guard.hydrator;
            // Raise-only + blocked latch across the whole promoted session
            // (design §2.4 rule 6, §10 anchor ④). The stateless screen read feeds
            // it; its verdict is what we both journal and expose via the status
            // slot. Reset on every demotion below.
            let mut screen_latch = remuda_screen::ScreenLatch::new();
            // Spinner-line projection (verb/tokens/phrase/interruptible), one
            // observation per distinct reading; reset with the raise latch below.
            let mut live_latch = remuda_screen::ScreenLiveLatch::new();
            // Sticky hook-tier health for this promoted epoch. A transient
            // `ps` miss (found == None) or a hook relay child taking the sampled
            // row must not read as "no hook tier" and release the raise: the
            // SessionStart/turn binding, once observed for this foreground pid,
            // stays live until that pid demotes or a hook confirms the turn end.
            let mut hook_ever_live = false;
            // c-usagefu r12 item 1 (Ruling A2'): the promoted-codex census
            // intervals are registered HERE, BEFORE the detection is published to
            // the `current` slot the file-adapter watch reads, but OWNED by the
            // driver: every detected codex pid gets a fresh interval whose guard is
            // handed to the driver in `promoted_lifetimes`. The poller never closes
            // one — a demotion only forgets the pid so the next codex foreground
            // registers another interval. r11 owned the guard in a poller local
            // and closed it on demotion, so a codex that left the foreground (or a
            // poller that was shut down) lost its census before the session's
            // process terminated: a rollout created during close could be
            // attributed to a competing same-cwd discovery.
            let mut census_pid: Option<i32> = None;
            let mut last_status: Option<ScreenStatus> = None;
            // Once the hook tier is merely *stale*, the screen decides the idle
            // edge — but one sample is not proof: the emulator can miss the TUI's
            // 1 s spinner repaint and read a single idle frame during a genuinely
            // running, hook-quiet tool. Count consecutive idle polls and only lower
            // busy after a stable idle; a finished turn reads idle every poll and
            // confirms quickly, a flicker resets and never lowers.
            let mut screen_idle_confirms: u32 = 0;
            // Last announced binding state, deduping lifecycle emission:
            // None = nothing announced yet this epoch.
            let mut announced: Option<String> = None;
            let mut bypass_announced = false;
            let mut tui_released = false;
            // One trust-dialog answer per promoted epoch (D-022). Reset on
            // promotion: a relaunched agent that hits the dialog again must not be
            // blocked by an answer spent on the previous foreground pid.
            let mut trust_attempted = false;
            // Hook-silence diagnostic: probe at a slower cadence than the 800 ms
            // screen poll, and journal only when the named reason changes, so the
            // badge copy is an edge, never a per-tick record. `None` = not probed
            // yet; `Some(None)` = healthy; `Some(Some(r))` = a failed check.
            let mut silence_ticks: u32 = 0;
            let mut last_silence: Option<Option<remuda_signal::hook_silence::HookSilenceReason>> =
                None;
            // Sticky tri-state (item 4) for the Remuda-launched pre_resume_mode:
            // the boundary proves one (pid, session) only. A plain Option could
            // not say "applied, then invalidated by a rebind", so the rebound arm
            // used to clear the slot and the next tick immediately re-applied the
            // stale launch mode to the new session.
            let mut launch_mode = LaunchModeBinding::Unbound;
            // r6 items 3/4: one boundary per (pid, session) measured at its
            // SessionStart report. The first session reported per pid is the
            // launched process's own; every later key is an in-TUI rebind.
            let mut session_modes = SessionModeTable::default();
            // r7 item 4(c): the last read offset per (pid, session), so a
            // demote/re-promotion continues the same transcript where it left
            // off instead of re-emitting it from the sticky anchor.
            let mut read_anchors = std::collections::HashMap::new();
            // c-effortread r10 item 6: (pid, session) pairs whose bound transcript
            // was EXPLICITLY withdrawn while no hydrator could prove it stayed in
            // place (the session was renamed/resumed away, or the bound file
            // vanished/changed identity). Persisted independently of anchor
            // capture — a demote with no successful final pump leaves only a
            // stale verified anchor in `read_anchors`; this set forces an
            // unverified EOF continuation (and boundary fallback) even after the
            // session is restored and even when the restored file keeps the same
            // inode. Sticky for the promotion loop's lifetime (fail closed).
            let mut withdrawn_sessions: std::collections::HashSet<(i32, String)> =
                std::collections::HashSet::new();
            // The (pid, session) the LIVE hydrator is currently reading, so a
            // rebind/demote arm knows which session an explicit withdrawal marks.
            let mut active_session: Option<(i32, String)> = None;
            // c-effortread r8 item 5: EffortTracker latch per (pid, session),
            // captured from every pump and re-seeded into the replacement mapper
            // on demote/re-promotion.
            let mut effort_seeds: std::collections::HashMap<(i32, String), EffortTrackerSeed> =
                std::collections::HashMap::new();
            let mut tick = tokio::time::interval(PROMOTE_POLL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut closing = false;
            loop {
                tokio::select! {
                    // Wakes close between ticks; a tick still drives normal polls.
                    _ = shutdown.changed() => {
                        closing = *shutdown.borrow_and_update();
                    }
                    _ = tick.tick() => {}
                }
                if closing || state.closed.load(Ordering::SeqCst) || events.is_closed() {
                    break;
                }
                let Sample { rows, mut found } =
                    sample(&state, table.as_ref(), alias.as_ref()).await;
                // Detection can still be late: `ps` has not caught the exec yet,
                // or the agent runs under a name neither the table nor the alias
                // covers. An authenticated SessionStart whose pid is in this PTY's
                // foreground group says what the process table has not said yet,
                // and binding it is what gives the session its nativeId and
                // transcript (without it `claude --resume` has nothing to resume
                // and the drill-in read has no session directory).
                if found.is_none()
                    && let Some(hooks) = hooks.as_ref()
                    && let Some(binding) = hooks.binding()
                {
                    found = promote_from_hook_binding(
                        &rows,
                        &binding,
                        alias.as_ref().map_or(AgentKind::Claude, |alias| alias.kind),
                    );
                }
                // Census transition judged from the SAME sample that is about to
                // be published. A codex entering the foreground registers BEFORE
                // the slot write, so a same-cwd generic launch discovering during
                // the promotion window already sees this unknown-boundary
                // instance and refuses prompt-only attribution. A codex leaving
                // (demotion or a different foreground process) does NOT close the
                // interval here: ownership is the driver's, and the interval
                // closes only once the stop ladder confirms the process terminated
                // (Ruling A2', r12 item 1). A codex -> codex foreground change
                // hands the driver a second live interval — the poller cannot
                // prove the earlier pid is dead, so both stay in the census.
                let next_codex_pid = found
                    .as_ref()
                    .and_then(|f| (f.kind == AgentKind::Codex).then_some(f.pid));
                if next_codex_pid != census_pid {
                    if let Some(pid) = next_codex_pid {
                        let guard = crate::adapters::codex_discovery::begin_lifetime_for_epoch(
                            &ctx.cwd,
                            &format!("promoted-codex-{pid}"),
                            None,
                            node_epoch,
                        );
                        promoted_lifetimes
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(guard);
                    }
                    census_pid = next_codex_pid;
                }
                // `send` reads this to decide bracketed-paste delivery, so it must
                // track the live foreground even between journaled transitions.
                if let Ok(mut slot) = current.lock() {
                    slot.clone_from(&found);
                }
                let Ok(now) = now_ts() else { continue };
                let mut saw_demote = false;
                let mut saw_promote = false;
                for (completeness, payload) in transition(&mut promote, found.as_ref(), &now) {
                    if let ObservationPayload::Lifecycle(lifecycle) = &payload
                        && let LifecyclePayload::Native(native) = lifecycle.as_ref()
                    {
                        saw_demote |= native.native_name == DEMOTED;
                        saw_promote |= native.native_name == PROMOTED;
                    }
                    if emit(
                        &events,
                        &seq,
                        &ctx,
                        SourceChannel::Runtime,
                        completeness,
                        payload,
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                }
                // Epoch boundaries reset tail, hydrated state, the picker, and the
                // screen raise/blocked latch — a new foreground starts unraised.
                if saw_demote {
                    screen_latch = remuda_screen::ScreenLatch::new();
                    live_latch = remuda_screen::ScreenLiveLatch::new();
                    hook_ever_live = false;
                    interrupt_pid.store(0, Ordering::SeqCst);
                    if let Some(payload) = bindings.invalidate_picker()
                        && emit(
                            &events,
                            &seq,
                            &ctx,
                            SourceChannel::Runtime,
                            Completeness::Structured,
                            payload,
                        )
                        .await
                        .is_err()
                    {
                        return;
                    }
                    bindings.demobilize();
                    if let Some(mut old) = hydrator.take() {
                        produce_replaced(&mut old, &finalise_pending);
                        #[cfg(test)]
                        publish_pending_gated(
                            &finalise_pending,
                            &events,
                            &seq,
                            &ctx,
                            &finalise_gate,
                        )
                        .await;
                        #[cfg(not(test))]
                        publish_pending(&finalise_pending, &events, &seq, &ctx).await;
                    }
                    announced = None;
                }
                if saw_promote {
                    bindings.begin_epoch(&ctx.cwd, &ctx.claude_home);
                    if let Some(mut old) = hydrator.take() {
                        produce_replaced(&mut old, &finalise_pending);
                        #[cfg(test)]
                        publish_pending_gated(
                            &finalise_pending,
                            &events,
                            &seq,
                            &ctx,
                            &finalise_gate,
                        )
                        .await;
                        #[cfg(not(test))]
                        publish_pending(&finalise_pending, &events, &seq, &ctx).await;
                    }
                    announced = None;
                    bypass_announced = false;
                    trust_attempted = false;
                }

                // The socket's SessionStart is also channel A for transcript
                // binding. Without this bridge only Claude's separate pid file
                // could hydrate, leaving a spurious manual picker over a hooked
                // session. begin_epoch must happen first so it cannot erase it.
                //
                // r6 items 3/4: the report's (pid, session) gets a boundary
                // measured at THIS instant — absent/empty transcript → Fresh
                // (`/clear` or a brand-new session whose file is created lazily),
                // otherwise an unverified anchor at current EOF (`/resume` of an
                // older session). It is stored once and applied on every bind of
                // the session, including the first one, so a fresh launch with no
                // transcript whose user /resume'd before typing is never tailed
                // byte 0 as current.
                if let Some(hooks) = hooks.as_ref()
                    && let Some(binding) = hooks.binding()
                    && found.as_ref().is_some_and(|found| found.pid == binding.pid)
                {
                    // Only an authenticated SessionStart for the observed
                    // foreground agent can release the deterministic start pin.
                    // A /tui relaunch keeps the same --settings argv, so its file
                    // must stop overriding the user's new renderer preference.
                    if !tui_released {
                        match hooks.release_tui_pin() {
                            Ok(()) => tui_released = true,
                            Err(error) => {
                                tracing::warn!(%error, "could not release launch renderer pin")
                            }
                        }
                    }
                    // One production seam (c-effortread r11 item 7): classify the
                    // bus-derived report, run the authenticated rebind/first-claim
                    // arm and the explicit-withdrawal marking, then ingest. The
                    // same function is what tests drive — never a hand-built
                    // record/rebind sequence.
                    if apply_native_session_binding(
                        &bindings,
                        found.as_ref().expect("matched foreground"),
                        &binding,
                        pre_resume_mode,
                        &mut session_modes,
                        &mut read_anchors,
                        &mut withdrawn_sessions,
                        &mut active_session,
                    ) {
                        // c-usagefu r17 item 2: the authenticated report rebound
                        // this foreground to a DIFFERENT native session, so publish
                        // the old hydrator's buffered group — its open turn's usage
                        // is only emitted by mapper.finish() — BEFORE discarding it.
                        // Dropping it outright (`hydrator = None`) lost every token
                        // of the in-flight turn on /clear and /resume rebinds. The
                        // binding seam above and the withdrawal bookkeeping inside
                        // `apply_native_session_binding` are unchanged.
                        if let Some(mut old) = hydrator.take() {
                            produce_replaced(&mut old, &finalise_pending);
                            #[cfg(test)]
                            publish_pending_gated(
                                &finalise_pending,
                                &events,
                                &seq,
                                &ctx,
                                &finalise_gate,
                            )
                            .await;
                            #[cfg(not(test))]
                            publish_pending(&finalise_pending, &events, &seq, &ctx).await;
                        }
                        announced = None;
                    }
                }

                if !bypass_announced
                    && let (Some(found), Some(hooks)) = (found.as_ref(), hooks.as_ref())
                    && shim_bypassed(found, hooks, expect_shim)
                {
                    bypass_announced = true;
                    let (completeness, payload) = native(
                        LifecycleTopic::Diagnostic,
                        SHIM_BYPASSED,
                        Knowledge::NotApplicable,
                        "claude 未经 shim 启动，hook 不可用",
                        BTreeMap::from([
                            ("kind".into(), "claude".into()),
                            ("pid".into(), found.pid.to_string()),
                            ("signalTier".into(), "screen".into()),
                        ]),
                        Severity::Warning,
                    );
                    if emit(
                        &events,
                        &seq,
                        &ctx,
                        SourceChannel::Runtime,
                        completeness,
                        payload,
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                }

                // Screen-derived readiness, on the same evidence class claude-pty
                // takes from herdr. Only latch transitions are journaled.
                //
                // The grid is the emulator's when `REMUDA_PTY_EMULATOR=1`, and the
                // ANSI-stripped ring tail otherwise — the same input the matchers
                // read before D-028, so the default path is unchanged (§13 P0).
                let (interruption_count, grid) = state.screen_evidence();
                // Pre-launch config seeding is the first line of defence against
                // the folder-trust dialog, but an inherited operator home or an
                // ancestor project entry with `hasTrustDialogAccepted: false` can
                // still put it on screen. When the Node granted auto-trust for this
                // registered workspace, answer the EXACT dialog once with its own
                // key sequence (D-022, same gate and same narrow matcher the herdr
                // carrier uses) — never by touching the operator's real config.
                if auto_trust_workspace
                    && !trust_attempted
                    && found
                        .as_ref()
                        .is_some_and(|found| found.kind == AgentKind::Claude)
                    && let Some(keys) = remuda_screen::trust_dialog_keys(&grid)
                {
                    trust_attempted = true;
                    let result = state.write_bytes(&logical_keys_to_bytes(&keys)).await;
                    let success = result.is_ok();
                    tracing::info!(
                        ?keys,
                        success,
                        "folder-trust dialog observed on the native carrier; answering once (registered workspace)"
                    );
                    let (_, payload) = native(
                        LifecycleTopic::Diagnostic,
                        "trust-dialog",
                        Knowledge::NotApplicable,
                        if success {
                            "trust-dialog auto-accepted (registered workspace)"
                        } else {
                            "trust-dialog automatic answer delivery failed; not replayed"
                        },
                        BTreeMap::new(),
                        if success {
                            Severity::Info
                        } else {
                            Severity::Warning
                        },
                    );
                    if emit(
                        &events,
                        &seq,
                        &ctx,
                        SourceChannel::Runtime,
                        Completeness::Structured,
                        payload,
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                }
                // Esc can end a Claude turn without a Stop hook. A newly rendered
                // native interruption marker settles the pending request, while an
                // old marker already present when cancel was sent cannot do so.
                if let Some(found) = found.as_ref()
                    && found.kind == AgentKind::Claude
                    && found.pid > 0
                    && interrupt_pid.load(Ordering::SeqCst) == found.pid
                    && interrupt_screen_markers
                        .lock()
                        .is_ok_and(|mut baseline| baseline.confirmed(interruption_count, &grid))
                    && interrupt_pid
                        .compare_exchange(found.pid, 0, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                {
                    if let Some(hooks) = hooks.as_ref() {
                        hooks.confirm_screen_interrupt(found.pid);
                    }
                    let (_, payload) = native(
                        LifecycleTopic::Turn,
                        "interrupted",
                        Knowledge::NotApplicable,
                        "idle",
                        BTreeMap::from([
                            ("kind".into(), "claude".into()),
                            ("pid".into(), found.pid.to_string()),
                            ("requestedBy".into(), "instance.cancel".into()),
                        ]),
                        Severity::Info,
                    );
                    if emit(
                        &events,
                        &seq,
                        &ctx,
                        SourceChannel::Pty,
                        Completeness::ScreenDerived,
                        payload,
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                }
                // Rule 6 (design §2.4): the screen/OSC tier may raise busy and
                // latch blocked, but it never lowers busy while a hook tier is
                // live for this foreground process. The HookSession's
                // `turn_active(pid)` is the §2.6 decision: `Some(true)` holds
                // busy, `Some(false)`/`None` differ only by whether this run ever
                // materialised a hook tier. That "ever materialised" bit is
                // sticky per epoch so a transient process-table miss cannot lower
                // the guard.
                let (turn_state, hook_last_seen) = match (found.as_ref(), hooks.as_ref()) {
                    (Some(found), Some(hooks)) if found.pid > 0 => (
                        hooks.turn_active(found.pid),
                        hooks.hook_last_seen(found.pid),
                    ),
                    _ => (None, None),
                };
                match turn_state {
                    Some(true) => hook_ever_live = true,
                    Some(false) if hook_ever_live => {
                        // A hook-confirmed turn end: the screen idle edge is now
                        // allowed. Clear the epoch stickiness for the next turn —
                        // the next UserPromptSubmit sets it again.
                        hook_ever_live = false;
                    }
                    _ => {}
                }
                // Freshness bound: a hook turn that has sent no record within a
                // small multiple of the poll cadence downgrades Healthy→Stalled so
                // the screen can lower busy once the turn is actually over. A hook
                // `Stop` (Some(false)) and a tier that never materialised are
                // unchanged; only a stuck-but-once-live channel loses its veto.
                let hook_fresh = hook_last_seen.is_some_and(|at| {
                    at.elapsed() <= PROMOTE_POLL.saturating_mul(HOOK_STALL_POLLS)
                });
                let hook_health = hook_health_for(turn_state, hook_ever_live, hook_fresh);
                let raw_status = screen_latch.update(&grid, hook_health);
                // Once the hook tier is merely stale the OSC busy bit alone cannot
                // end the turn: a long, hook-quiet tool reads that bit as idle
                // while the TUI is still showing its spinner. Require BOTH that no
                // spinner line is on screen this poll AND a run of consecutive such
                // polls (a torn repaint never lowers; a finished turn — idle prompt
                // on every poll — confirms within a couple of seconds). While hooks
                // are fresh Healthy already holds Working, and a tier that never
                // materialised lowers immediately exactly as before.
                let spinner_present = remuda_screen::screen_live(&grid).is_some();
                let status = if raw_status == Some(ScreenStatus::Idle)
                    && hook_health == remuda_screen::HookHealth::Stalled
                {
                    if spinner_present {
                        screen_idle_confirms = 0;
                        Some(ScreenStatus::Working)
                    } else {
                        screen_idle_confirms = screen_idle_confirms.saturating_add(1);
                        if screen_idle_confirms >= SCREEN_IDLE_CONFIRM_POLLS {
                            Some(ScreenStatus::Idle)
                        } else {
                            Some(ScreenStatus::Working)
                        }
                    }
                } else {
                    if raw_status != Some(ScreenStatus::Idle) {
                        screen_idle_confirms = 0;
                    }
                    raw_status
                };
                if let Ok(mut slot) = status_slot.lock() {
                    *slot = status;
                }
                // Journal only on an actual latch transition.
                if let Some(status) = status
                    && last_status != Some(status)
                {
                    last_status = Some(status);
                    if emit(
                        &events,
                        &seq,
                        &ctx,
                        SourceChannel::Pty,
                        // A screen read is not proof; say so in the envelope.
                        Completeness::ScreenDerived,
                        agent_status(status),
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                }
                // Why is the hook tier quiet? Only worth naming once the screen
                // has actually decided the turn is idle: a running-but-hook-quiet
                // tool still shows its spinner, and there "no events for a few
                // seconds" is normal, not a broken channel (the plain amber note is
                // enough). At the screen-idle end, probe — at a slow cadence — the
                // pinned relay and the instance hook socket, and journal the first
                // failed check only when it changes.
                //
                // We deliberately do NOT claim `link-stalled` here: this screen
                // poller cannot see the Node transport's journal-pump cursor, and a
                // finished session simply emits no hooks — so an old hook timestamp
                // with a healthy relay+socket is an ordinary quiet end, not a stalled
                // pump (the `link-stalled` check belongs to a caller that holds the
                // real flush cursor). A fresh tier (a clean Stop that just landed)
                // is skipped entirely, so a normal turn never writes a record.
                if status == Some(ScreenStatus::Idle)
                    && !hook_fresh
                    && silence_paths.is_some()
                    && found.as_ref().is_some_and(|f| f.pid > 0)
                {
                    silence_ticks = silence_ticks.saturating_add(1);
                    if silence_ticks >= HOOK_SILENCE_PROBE_TICKS
                        && let Some(paths) = silence_paths.as_ref()
                    {
                        silence_ticks = 0;
                        let relay_ok = remuda_signal::hook_silence::relay_executable(Some(
                            paths.relay.as_path(),
                        ));
                        let socket_ok =
                            remuda_signal::hook_silence::socket_listening(paths.socket.as_path())
                                .await;
                        let reason = silence_reason_for(hook_fresh, relay_ok, socket_ok);
                        if last_silence != Some(reason) {
                            last_silence = Some(reason);
                            if let Some(reason) = reason {
                                let (_, payload) = native(
                                    LifecycleTopic::Hook,
                                    "hook.silence",
                                    Knowledge::NotApplicable,
                                    "observed",
                                    BTreeMap::from([("reason".into(), reason.as_str().into())]),
                                    Severity::Info,
                                );
                                if emit(
                                    &events,
                                    &seq,
                                    &ctx,
                                    SourceChannel::Pty,
                                    Completeness::ScreenDerived,
                                    payload,
                                )
                                .await
                                .is_err()
                                {
                                    return;
                                }
                            }
                        }
                    }
                } else if status != Some(ScreenStatus::Idle) {
                    silence_ticks = 0;
                }
                // The spinner status line is richer status than the busy bit:
                // verb, streamed token estimate, phrase and the interrupt hint.
                // Status only (design §2.4); the latch itself enforces one
                // observation per distinct reading and a single clear on leave.
                // The latched verdict already folds rule 6 (OSC idle while the
                // hook tier is alive stays Working) and the blocked latch; feed
                // it as the authoritative working hint so a screen clear can
                // only land when this carrier itself stopped claiming busy.
                let known_working = match status {
                    Some(ScreenStatus::Working) => Some(true),
                    Some(ScreenStatus::Idle) | Some(ScreenStatus::Blocked) => Some(false),
                    None => None,
                };
                if let Some(change) = live_latch.observe_with(&grid, known_working) {
                    let payload = match change {
                        remuda_screen::ScreenLiveChange::Active(live) => {
                            live_status_payload(&live, now_utc())
                        }
                        remuda_screen::ScreenLiveChange::Inactive => live_status_inactive(),
                    };
                    if emit(
                        &events,
                        &seq,
                        &ctx,
                        SourceChannel::Pty,
                        Completeness::ScreenDerived,
                        payload,
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                }

                // c-usagefu r20 item 2: demote/rebind/degrade finalization routes
                // the replaced hydrator through the FINALISE GUARD (synchronous
                // production into guard storage + cancellation-safe publication),
                // never through a local queue.
                //
                // CANCELLATION-SAFETY (coordinator note, c-usagefu r21): the
                // hydrator is MOVED into this callback rather than borrowed,
                // but it remains safe — `produce_replaced` synchronously maps
                // the final read and mapper.finish() into the shared
                // `finalise_pending` storage BEFORE the awaited publication,
                // so an abort at any await drops only the now-empty hydrator;
                // all produced observations live in the guard queue (rule
                // (a)/(b)/(d)).
                let events_ref: &mpsc::Sender<Observation> = &events;
                let seq_ref: &AtomicU64 = &seq;
                let ctx_ref: &PromoteCtx = &ctx;
                // Borrows the task's pending Arc and channel/ctx refs only while
                // building the future; the returned future owns its own clones.
                let mut finalize_replaced = |mut old: Hydrator| {
                    let pending = Arc::clone(&finalise_pending);
                    #[cfg(test)]
                    let gate = Arc::clone(&finalise_gate);
                    async move {
                        produce_replaced(&mut old, &pending);
                        #[cfg(test)]
                        publish_pending_gated(&pending, events_ref, seq_ref, ctx_ref, &gate).await;
                        #[cfg(not(test))]
                        publish_pending(&pending, events_ref, seq_ref, ctx_ref).await;
                    }
                };

                match (&promote.kind, &found) {
                    // Claude is the only kind that hydrates a transcript in the MVP.
                    (Some(AgentKind::Claude), Some(found)) if found.hydrates_transcript => {
                        maintain_binding_inner(
                            &bindings,
                            &ctx,
                            found,
                            hydrator,
                            &mut announced,
                            &events,
                            &seq,
                            effort_bridge.as_ref(),
                            launch_effort,
                            model.as_ref(),
                            permission_bridge.as_ref(),
                            launch_permission,
                            &mut session_modes,
                            &mut launch_mode,
                            pre_resume_mode,
                            &mut read_anchors,
                            &mut effort_seeds,
                            &mut withdrawn_sessions,
                            &mut active_session,
                            &mut finalize_replaced,
                        )
                        .await;
                    }
                    _ => {
                        if let Some(old) = hydrator.take() {
                            finalize_replaced(old).await;
                        }
                    }
                }
                // c-ctxusage r5 item 6: close can land while this iteration's pump
                // is still mapping. Check before waiting another tick so the final
                // finalise runs now (close awaits it via `finalised`), never after
                // a 2 s abort.
                if state.closed.load(Ordering::SeqCst) || *shutdown.borrow() {
                    break;
                }
                if events.is_closed() {
                    // Channel already gone: nothing can receive the finalise.
                    break;
                }
            }
            // c-ctxusage r4 item 2 / r5 item 6: cooperative shutdown finalises the
            // last assistant run (usage with no stop_reason) instead of dropping
            // the hydrator on close/process exit. This awaits the current pump's
            // completion first and must finish before close aborts the task.
            //
            // c-usagefu r18 items 1/5 test gate: when a test holds the park permit,
            // the poller stops HERE — after the cooperative break, immediately
            // before the final finalise — so a close waiting on the poller handle
            // is deterministically parked and its abort-after-deadline path (which
            // must run the guard's finish_sync on the live mapper) is drivable.
            test_park_before_final_finalise().await;
            // c-usagefu r20 items 1/2: synchronously PRODUCE the final tail read
            // and mapper.finish() into the guard's pending storage FIRST (no await,
            // nothing cancellable); the hydrator leaves the slot only after that.
            // Publication below awaits channel CAPACITY while every observation is
            // still guard-owned, so an abort can never strand a mapped group.
            produce_slot_shutdown(&mut finalise_guard.hydrator, &finalise_pending);
            #[cfg(test)]
            publish_pending_gated(&finalise_pending, &events, &seq, &ctx, &finalise_gate).await;
            #[cfg(not(test))]
            publish_pending(&finalise_pending, &events, &seq, &ctx).await;
            // Finalization completed and the queue drained; mark done so Drop does
            // nothing.
            finalise_guard.mark_done();
            let _ = finalised.send(());
        }
    });
    #[cfg(test)]
    {
        PollerTask {
            handle,
            finalise_gate,
        }
    }
    #[cfg(not(test))]
    {
        handle
    }
}

/// Guard-owned finalization storage shared between the poller task and the
/// [`FinaliseGuard`] it owns.
type FinalisePending = Arc<std::sync::Mutex<Vec<Observation>>>;

/// Best-effort abort-path safety net for [`spawn`].
///
/// The guard owns the live hydrator for the whole poll loop and shares the
/// queue of mapped-but-unpublished observations. Its Drop is the ONLY place an
/// aborted finalization can recover state from, so all production state it
/// protects lives here — never in a future's locals (cancellation-safety
/// contract on [`spawn`]).
struct FinaliseGuard {
    /// The currently live hydrator. `Some` while the loop is pumping (or after
    /// a demote/rebind reopened a replacement); `None` only between producing
    /// a finalized hydrator and the loop reopening one — and for good after
    /// shutdown production.
    hydrator: Option<Hydrator>,
    /// Shared mapped-but-unpublished finalize observations.
    pending: FinalisePending,
    events: mpsc::Sender<Observation>,
    /// Owned stamping context so Drop can stamp recovered observations without
    /// the task's borrows.
    seq: Arc<AtomicU64>,
    ctx: PromoteCtx,
    /// Normal-cooperative-completion suppression.
    done: bool,
}

impl FinaliseGuard {
    fn new(
        events: mpsc::Sender<Observation>,
        seq: Arc<AtomicU64>,
        ctx: PromoteCtx,
        pending: FinalisePending,
    ) -> Self {
        Self {
            hydrator: None,
            pending,
            events,
            seq,
            ctx,
            done: false,
        }
    }

    /// Normal completion: production and publication both finished, nothing
    /// left in the queue.
    fn mark_done(&mut self) {
        debug_assert!(
            self.pending.lock().expect("finalise pending").is_empty(),
            "mark_done with unpublished finalize observations"
        );
        self.done = true;
    }
}

/// Shutdown production half (rules (a)/(b)): synchronously map the final tail
/// read and run mapper.finish() on the LIVE slot hydrator into shared pending
/// storage, then empty the slot. No awaits — uncancellable.
fn produce_slot_shutdown(slot: &mut Option<Hydrator>, pending: &FinalisePending) {
    if let Some(hydrator) = slot.as_mut() {
        let mut queue = pending.lock().expect("finalise pending");
        produce_hydrator(hydrator, &mut queue);
    }
    *slot = None;
}

/// Demote/rebind/degrade production half (rule (d)): the loop has taken the
/// replaced hydrator out of its slot; produce its final state synchronously
/// into the shared pending queue.
fn produce_replaced(old: &mut Hydrator, pending: &FinalisePending) {
    let mut queue = pending.lock().expect("finalise pending");
    produce_hydrator(old, &mut queue);
}

/// Cancellation-safe publication (rule (c)): await channel CAPACITY while the
/// observation stays in the shared queue; pop, stamp and hand it to the permit
/// SYNCHRONOUSLY after the await completes — no cancellation point between
/// removal and the send.
#[cfg(not(test))]
async fn publish_pending(
    pending: &FinalisePending,
    events: &mpsc::Sender<Observation>,
    seq: &AtomicU64,
    ctx: &PromoteCtx,
) {
    publish_pending_inner(pending, events, seq, ctx).await;
}

/// Test build: same publication with the per-spawn cancellation gate.
#[cfg(test)]
async fn publish_pending_gated(
    pending: &FinalisePending,
    events: &mpsc::Sender<Observation>,
    seq: &AtomicU64,
    ctx: &PromoteCtx,
    gate: &Arc<FinalisePublishGate>,
) {
    gate.hold().await;
    publish_pending_inner(pending, events, seq, ctx, Some(gate)).await;
}

async fn publish_pending_inner(
    pending: &FinalisePending,
    events: &mpsc::Sender<Observation>,
    seq: &AtomicU64,
    ctx: &PromoteCtx,
    #[cfg(test)] ack: Option<&FinalisePublishGate>,
) {
    loop {
        if pending.lock().expect("finalise pending").is_empty() {
            return;
        }
        // The only await in the loop, and the observation is still queued.
        //
        // c-usagefu r22 item 1 test seam: the ACTUAL reservation future is
        // box-pinned inside [`AckPendingReservation`] (tokio's reserve future
        // is !Unpin) and polled in place; the gate is signalled only when one
        // of ITS polls returns Poll::Pending. The same future keeps being
        // polled afterwards — it is never re-created — so the harness
        // acknowledgement proves this invocation reached a genuinely pending
        // reserve() before it aborts.
        #[cfg(test)]
        let reserve = AckPendingReservation::new(ack, events.reserve());
        #[cfg(not(test))]
        let reserve = events.reserve();
        let Ok(permit) = reserve.await else {
            // Channel closed: nothing more can be delivered.
            return;
        };
        // Synchronous pop + stamp + hand-off; an abort pending in the runtime
        // cannot run between these statements.
        let mapped = pending.lock().expect("finalise pending").remove(0);
        if let Some(observation) = stamp_mapped(ctx, seq, mapped) {
            permit.send(observation);
        }
    }
}

impl Drop for FinaliseGuard {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        // 1. Produced-but-unpublished observations (cancellation landed on a
        // pending capacity reservation or the channel was full/closed).
        let pending = std::mem::take(&mut *self.pending.lock().expect("finalise pending"));
        for observation in pending {
            let Some(stamped) = stamp_mapped(&self.ctx, &self.seq, observation) else {
                break;
            };
            if self.events.try_send(stamped).is_err() {
                break;
            }
        }
        // 2. The task was cancelled BEFORE production consumed the live
        // hydrator (a wedged pump, or a demote/rebind before its reopen): run
        // its synchronous finish group and deliver that too. When the slot is
        // None production already called mapper.finish(), so this never
        // double-drains.
        if let Some(hydrator) = self.hydrator.as_mut()
            && let Ok(observations) = hydrator.finish_sync()
        {
            for observation in observations {
                let Some(stamped) = stamp_mapped(&self.ctx, &self.seq, observation) else {
                    break;
                };
                if self.events.try_send(stamped).is_err() {
                    break;
                }
            }
        }
    }
}

/// c-usagefu r21 item 3: the poller task + its per-spawn cancellation gate.
/// Tests acquire the gate and observe THIS poller's finalization only.
#[cfg(test)]
pub(crate) struct PollerTask {
    handle: tokio::task::JoinHandle<()>,
    #[cfg(test)]
    finalise_gate: Arc<FinalisePublishGate>,
}

#[cfg(test)]
impl PollerTask {
    /// This poller's per-spawn cancellation gate.
    pub(crate) fn gate(&self) -> Arc<FinalisePublishGate> {
        Arc::clone(&self.finalise_gate)
    }

    /// Consume into the raw task handle (used by close and resume transfer).
    pub(crate) fn into_handle(self) -> tokio::task::JoinHandle<()> {
        self.handle
    }
}

/// Per-spawn gate around the first finalization capacity reservation.
///
/// - A test acquires [`Self::permit`] (an owned token keyed to this gate).
/// - The poller, when finalization reaches publication, parks on
///   [`Self::parked`] until it reaches the gate and sets `entered`.
/// - Once the channel is filled and the caller releases the 2-party barrier,
///   the poller enters the pending reserve() on the full channel.
/// - The caller then awaits `reserve_pending`: an acknowledgement emitted from
///   polling the ACTUAL reservation future once a poll returns Pending. The
///   barrier alone is insufficient on a current-thread runtime (its last
///   participant returns before the released task resumes), so only this ack
///   proves the poller reached a genuinely pending reservation before abort.
#[cfg(test)]
pub(crate) struct FinalisePublishGate {
    /// One parked slot per poller; None unless a test holds the gate open.
    slot: tokio::sync::Mutex<Option<ParkSlot>>,
}

#[cfg(test)]
struct ParkSlot {
    /// Set when the poller arrives at the gate.
    entered: tokio::sync::watch::Sender<bool>,
    /// Set when the poller's ACTUAL reservation future has been polled and
    /// that poll returned Poll::Pending.
    reserve_pending: tokio::sync::watch::Sender<bool>,
    /// 2-party barrier: the poller waits here until the test releases it into
    /// the full-channel reservation.
    release: Arc<tokio::sync::Barrier>,
}

#[cfg(test)]
impl FinalisePublishGate {
    fn new() -> Self {
        Self {
            slot: tokio::sync::Mutex::new(None),
        }
    }

    /// Acquire this poller's finalization gate. Returns a permit the caller
    /// uses to release the poller into the reserve() park.
    pub(crate) async fn permit(self: &Arc<Self>) -> PublishPermit {
        let mut slot = self.slot.lock().await;
        // A prior gate on this same poller must be cleared first.
        *slot = None;
        let (entered_tx, entered_rx) = tokio::sync::watch::channel(false);
        let (reserve_tx, reserve_rx) = tokio::sync::watch::channel(false);
        let release = Arc::new(tokio::sync::Barrier::new(2));
        *slot = Some(ParkSlot {
            entered: entered_tx,
            reserve_pending: reserve_tx,
            release: Arc::clone(&release),
        });
        PublishPermit {
            entered: entered_rx,
            reserve_pending: reserve_rx,
            release,
        }
    }

    /// Poller-side: signal arrival and wait on the release barrier until the
    /// test has filled the channel and releases the poller into reserve().
    async fn hold(&self) {
        let release = {
            let mut guard = self.slot.lock().await;
            let Some(slot) = guard.as_mut() else {
                return;
            };
            slot.entered.send(true).ok();
            Arc::clone(&slot.release)
        };
        release.wait().await;
    }

    /// Test seam (c-usagefu r22 item 1): acknowledge that the poller's ACTUAL
    /// reservation future was polled and returned Pending. `try_lock` keeps
    /// polling synchronous; the slot is uncontended here because `permit()`
    /// finished before the poller was released into reserve().
    fn signal_reserve_pending(&self) {
        if let Ok(mut guard) = self.slot.try_lock()
            && let Some(slot) = guard.as_mut()
        {
            slot.reserve_pending.send(true).ok();
        }
    }
}

#[cfg(test)]
pub(crate) struct PublishPermit {
    entered: tokio::sync::watch::Receiver<bool>,
    reserve_pending: tokio::sync::watch::Receiver<bool>,
    release: Arc<tokio::sync::Barrier>,
}

#[cfg(test)]
impl PublishPermit {
    pub(crate) async fn wait_entered(&mut self) {
        if *self.entered.borrow() {
            return;
        }
        let _ = self.entered.changed().await;
    }

    /// Release the parked poller toward reserve() on the FULL channel. The
    /// barrier returning does NOT guarantee the poller has resumed (on a
    /// current-thread runtime the last participant returns immediately); the
    /// caller must follow with [`Self::wait_reserve_pending`] before aborting.
    pub(crate) async fn release_to_pending_reserve(&mut self) {
        self.release.wait().await;
    }

    /// Wait for the acknowledgement emitted from polling the ACTUAL
    /// reservation future once a poll returned Poll::Pending. Per-invocation:
    /// only this gate's poller can set this permit's watch.
    pub(crate) async fn wait_reserve_pending(&mut self) {
        if *self.reserve_pending.borrow() {
            return;
        }
        let _ = self.reserve_pending.changed().await;
    }
}

/// c-usagefu r22 item 1 test seam: wraps the ACTUAL channel reservation
/// future. The inner future is box-pinned with the wrapper and the SAME future
/// is re-polled on every wake — it is never re-created — while `gate` is
/// signalled after every poll that returns Pending.
#[cfg(test)]
struct AckPendingReservation<'a, F> {
    gate: Option<&'a FinalisePublishGate>,
    inner: std::pin::Pin<Box<F>>,
}

#[cfg(test)]
impl<'a, F: std::future::Future> AckPendingReservation<'a, F> {
    fn new(gate: Option<&'a FinalisePublishGate>, inner: F) -> Self {
        Self {
            gate,
            inner: Box::pin(inner),
        }
    }
}

#[cfg(test)]
impl<F: std::future::Future> std::future::Future for AckPendingReservation<'_, F> {
    type Output = F::Output;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<F::Output> {
        // Pin<Box<F>> is Unpin for any F, so the wrapper is Unpin and the
        // pinned reservation future can be re-polled in place through the
        // reborrow — the same future instance every time, never re-created.
        let this = self.get_mut();
        let gate = this.gate;
        match this.inner.as_mut().poll(cx) {
            std::task::Poll::Pending => {
                // Signal only AFTER the actual reservation poll returned
                // Pending; the pinned inner future stays and is polled again.
                if let Some(gate) = gate {
                    gate.signal_reserve_pending();
                }
                std::task::Poll::Pending
            }
            std::task::Poll::Ready(output) => std::task::Poll::Ready(output),
        }
    }
}

/// Synchronously produce a hydrator's final state: one final tail read mapped
/// line-by-line, then mapper.finish(). No awaits — called with the hydrator
/// still guard-owned (cancellation-safety rules (a)/(b)).
fn produce_hydrator(hydrator: &mut Hydrator, pending: &mut Vec<Observation>) {
    if let Ok(read) = hydrator.tail.poll() {
        for (line, position) in read.lines.iter().zip(read.positions.iter().copied()) {
            if let Ok(mapped) = hydrator.mapper.map_line_at(line, Some(position)) {
                pending.extend(mapped);
            }
        }
    }
    if let Ok(finished) = hydrator.mapper.finish() {
        pending.extend(finished);
    }
}

/// Synchronously stamp a mapper observation for publication (no channel
/// access). `None` on the only fallible step (clock read).
fn stamp_mapped(ctx: &PromoteCtx, seq: &AtomicU64, mapped: Observation) -> Option<Observation> {
    let event_id = mapped.event_id;
    let native_at = mapped.native_at.clone();
    let evidence = mapped.evidence_event_ids.clone();
    let mut observation = build(
        ctx,
        seq.fetch_add(1, Ordering::SeqCst) + 1,
        SourceChannel::Transcript,
        mapped.completeness,
        mapped.body,
        native_at,
    )
    .ok()?;
    observation.event_id = event_id;
    observation.evidence_event_ids = evidence;
    Some(observation)
}

/// c-usagefu r20 item 5 test helper: a fully stamped throwaway observation a
/// test can use to fill the bounded run channel.
#[cfg(any(test, feature = "test-stub"))]
pub(super) fn test_filler_observation(ctx: &PromoteCtx) -> Observation {
    build(
        ctx,
        1,
        SourceChannel::Runtime,
        Completeness::Structured,
        ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
            NativeLifecycle {
                topic: LifecycleTopic::Diagnostic,
                native_name: "r20-channel-fill".to_owned(),
                native_id: Knowledge::NotApplicable,
                status: Knowledge::Known {
                    value: "filler".to_owned(),
                },
                related_ids: BTreeMap::new(),
                data_ref: None,
                severity: Severity::Info,
                affects_completion: false,
            },
        )))),
        Knowledge::NotApplicable,
    )
    .expect("filler observation")
}

/// c-usagefu r18 items 1/5 deterministic test gate. A test ACQUIRES the permit
/// after its cooperative shutdown break and immediately before its final
/// finalise, parks until the permit is dropped. With nobody holding it the lock
/// is free and production-shaped tests pass straight through.
#[cfg(test)]
static TEST_FINAL_FINALISE_PARK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

#[cfg(test)]
async fn test_park_before_final_finalise() {
    let _parked = TEST_FINAL_FINALISE_PARK.lock().await;
}

/// Non-test builds pass straight through the gate (there is no permit holder).
#[cfg(not(test))]
async fn test_park_before_final_finalise() {}

/// Test seam: park every poller at its final-finalise gate until the guard is
/// dropped.
#[cfg(test)]
pub(crate) async fn test_acquire_final_finalise_park() -> tokio::sync::MutexGuard<'static, ()> {
    TEST_FINAL_FINALISE_PARK.lock().await
}

/// Test seam: whether a poller is currently parked at the final-finalise gate
/// (i.e. it has broken out of its loop after the shutdown signal).
#[cfg(test)]
pub(crate) fn test_final_finalise_park_held() -> bool {
    TEST_FINAL_FINALISE_PARK.try_lock().is_err()
}

/// A shim records its exec PID before Claude can enter the foreground. Missing
/// marker plus no SessionStart for that process is bypass evidence; elapsed
/// time without events and unreadable marker directories are not.
fn shim_bypassed(found: &Detected, hooks: &crate::launch::HookSession, expect_shim: bool) -> bool {
    let Some(launch) = hooks.bin_dir().parent() else {
        return false;
    };
    shim_bypass_evidence(
        found,
        hooks.binding().map(|binding| binding.pid),
        &launch.join("shim-pids"),
        expect_shim,
    )
}

fn shim_bypass_evidence(
    found: &Detected,
    hook_pid: Option<i32>,
    markers: &Path,
    expect_shim: bool,
) -> bool {
    // P2 intentionally execs a pinned agent directly with its overlay. That
    // launch did not bypass a shell PATH shim: no shell was involved.
    if !expect_shim
        || found.kind != AgentKind::Claude
        || found.pid <= 0
        || hook_pid == Some(found.pid)
    {
        return false;
    }
    markers.is_dir() && matches!(markers.join(found.pid.to_string()).try_exists(), Ok(false))
}

/// A bounded stream recognizer, independent of scrolling and viewport reuse.
/// Escape state survives reads, so OSC titles cannot forge rendered evidence.
#[derive(Default)]
pub(super) struct InterruptionOutput {
    text: Vec<u8>,
    escape: u8,
    count: u64,
}

impl InterruptionOutput {
    pub(super) fn count(&self) -> u64 {
        self.count
    }

    pub(super) fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match self.escape {
                1 => {
                    self.escape = match byte {
                        b'[' => 2,
                        b']' => 3,
                        b'P' | b'_' | b'^' => 5,
                        b'(' | b')' | b'#' => 6,
                        _ => 0,
                    }
                }
                2 => {
                    if (0x40..=0x7e).contains(&byte) {
                        self.escape = 0;
                        if byte != b'm' {
                            self.text.clear();
                        }
                    }
                }
                3 => match byte {
                    7 => self.escape = 0,
                    27 => self.escape = 4,
                    _ => {}
                },
                4 => self.escape = if byte == b'\\' { 0 } else { 3 },
                5 => {
                    if byte == 27 {
                        self.escape = 7;
                    }
                }
                6 => self.escape = 0,
                7 => self.escape = if byte == b'\\' { 0 } else { 5 },
                _ => match byte {
                    27 => self.escape = 1,
                    0..=31 | 127 => self.text.clear(),
                    _ => {
                        self.text.push(byte.to_ascii_lowercase());
                        if self.text.len() > 64 {
                            self.text.remove(0);
                        }
                        if self
                            .text
                            .ends_with("interrupted · what should claude do instead?".as_bytes())
                            || self.text.ends_with(b"[request interrupted by user")
                        {
                            self.count = self.count.saturating_add(1);
                        }
                    }
                },
            }
        }
    }
}

#[derive(Default)]
pub(super) struct InterruptBaseline {
    count: u64,
    screen: String,
}

impl InterruptBaseline {
    pub(super) fn arm(&mut self, count: u64, grid: &remuda_screen::ScreenGrid) {
        self.count = count;
        self.screen = grid.flat();
    }

    fn confirmed(&mut self, count: u64, grid: &remuda_screen::ScreenGrid) -> bool {
        if count <= self.count {
            return false;
        }
        let Some(current_turn) = current_turn_interrupted(grid) else {
            // The marker can precede its composer in another PTY read. Keep
            // that evidence pending until the native frame is complete.
            return false;
        };
        self.count = count;
        // A known unchanged repaint is not a new interruption, even though
        // its old marker was printed again. Idle without fresh text is inert.
        self.screen != grid.flat() && current_turn
    }
}

/// A repaint may repeat an older turn's interruption text while the current
/// spinner changes. Require an empty native composer and a marker in the current
/// transcript turn, below its most recent submitted prompt. The live Claude
/// spinner can omit `esc to interrupt`, so status alone is insufficient.
fn current_turn_interrupted(grid: &remuda_screen::ScreenGrid) -> Option<bool> {
    let composer = grid
        .lines
        .iter()
        .rposition(|line| line.trim_start().starts_with('❯'))?;
    if grid.lines[composer].trim() != "❯" {
        // A submitted prompt or draft is not proof that input was restored.
        return None;
    }
    let transcript = grid.lines[..composer]
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    let current_turn = transcript
        .rsplit_once('❯')
        .map_or(transcript.as_str(), |(_, turn)| turn);
    let has_marker = |text: &str| {
        text.contains("interrupted · what should claude do instead?")
            || text.contains("[request interrupted by user")
    };
    // The last prompt may still be the submitted user row: a freshly
    // printed marker below it awaits the final composer in a later read.
    // Check this before looking above it, where an older turn may also have
    // an interruption marker.
    let below = grid.lines[composer + 1..]
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    if has_marker(&below) {
        return None;
    }
    if !has_marker(current_turn) {
        return Some(false);
    }
    (remuda_screen::screen_status(grid) == Some(ScreenStatus::Idle)).then_some(true)
}

/// Item 4 sticky tri-state for the Remuda-launched pre-spawn [`ResumeMode`].
///
/// The boundary captured before spawn proves records only for ONE
/// `(pid, session)`. A plain `Option<(pid, session)>` cannot distinguish
/// "not applied yet" from "applied and then invalidated", so the rebound arm
/// used to clear the slot and the very next tick re-applied the stale launch
/// mode to the rebound session. [`LaunchModeBinding::Dead`] is sticky.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LaunchModeBinding {
    /// No foreground session has used the pre-spawn mode yet.
    Unbound,
    /// The pre-spawn mode belongs to this `(pid, session)`. An empty session
    /// means detection had not learned the id on that sample.
    Bound(i32, String),
    /// The foreground left the launched (pid, session): the pre-spawn mode is
    /// dead for the rest of the promoted epoch. The carried mode is the
    /// REBOUND session's own boundary (item 2: an authenticated rebind is
    /// bounded from its own transcript snapshot, not the launch argv); for a
    /// pid/argv-only change it is the detected provenance.
    Dead(ResumeMode),
}

/// Boundary measured at the INSTANT an authenticated SessionStart report for
/// `(pid, session_id)` arrived (r6 items 3/4).
///
/// The mode is snapshotted once per session and applied on EVERY bind of that
/// session within the spawn — including the very first one (a fresh launch
/// with no transcript yet whose user runs `/resume` before typing) and after a
/// demote/re-promotion — so it never falls back to the launch argv or byte 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StickySessionMode {
    /// ResumeMode measured for the session at its first SessionStart report.
    mode: ResumeMode,
    /// True for every session after the launched process's FIRST reported
    /// session (an in-TUI `/resume` / `/clear`): the pre-spawn launch mode is
    /// dead for it.
    rebound: bool,
}

/// Per-spawn table of the boundary measured at each authenticated
/// SessionStart report (r6 items 3/4). The first session reported per pid is
/// the launched process's own (it keeps the pre-spawn mode when one exists);
/// every later session for that pid is an in-TUI rebind bounded from its own
/// transcript snapshot. Entries are recorded once and survive demotions,
/// re-promotions and late binds (the lazy-create case where the user
/// `/resume`s before the fresh transcript exists).
#[derive(Debug, Default)]
struct SessionModeTable {
    modes: std::collections::HashMap<(i32, String), StickySessionMode>,
    first_pid: std::collections::HashSet<i32>,
}

impl SessionModeTable {
    /// Record the SessionStart for `(pid, session_id)` once and return its
    /// sticky mode. `transcript_path` is the path the hook named at that
    /// instant (may not exist yet — a lazily created fresh session).
    ///
    /// `start_source` is the authenticated SessionStart hook's `source`
    /// (c-effortread r8 item 2), captured AT HOOK DELIVERY — before the prompt
    /// reaches the rebound transcript: a `/clear`/startup/compact source is a
    /// brand-new empty session (Fresh) even if the file is already non-empty
    /// by the time this runs (queued/pasted prompt before the next poll tick).
    fn record(
        &mut self,
        pid: i32,
        session_id: &str,
        transcript_path: Option<&std::path::Path>,
        pre_resume_mode: Option<ResumeMode>,
        start_source: Option<&str>,
        found: &Detected,
    ) -> StickySessionMode {
        let key = (pid, session_id.to_owned());
        if let Some(entry) = self.modes.get(&key) {
            return *entry;
        }
        // c-effortread r8 item 4(a): "first for pid" is about WHICH session the
        // pid launched with, not the transcript handed to this first record.
        // If argv's session id names a DIFFERENT session than the one being
        // recorded, the argv session is stale (an in-TUI switch already
        // happened, e.g. a hook delivery racing the first poll) — record this
        // as a rebound, never as the launch's own first session.
        // c-effortread r9 item 4: only CLAIM the pid's launch slot here; do
        // not `insert` a record that loses on argv (a hook-raced switch). The
        // old insert-then-discard consumed the slot, so the real launch-id the
        // argv named landed later as a rebound and lost pre_resume_mode.
        let slot_unclaimed = !self.first_pid.contains(&pid);
        // c-effortread r11 item 6: the hook fallback populates
        // `found.session_id` from the authenticated SessionStart, not argv.
        // That identity is NOT the launch's own established session, so a
        // hand-typed `claude` (no argv session/resume) whose user /resume'd X
        // before the first poll cannot win Fresh launch provenance here.
        let argv_names_other_session = found
            .session_id
            .as_deref()
            .is_some_and(|argv_id| !found.session_id_from_hook && argv_id != session_id);
        let first_for_pid = slot_unclaimed && !argv_names_other_session;
        if first_for_pid {
            self.first_pid.insert(pid);
        }
        let (mode, rebound) = if first_for_pid {
            // The launched process's own FIRST reported session. A pre-spawn
            // capture (a Remuda-launched resume) wins. Without one the mode
            // comes from ARGV PROVENANCE, never from the file's size at this
            // instant: an instance.create with an initial prompt writes the
            // transcript before the poller ingests SessionStart, and sizing
            // it here made every such launch a permanent Unverified.
            //
            // c-effortread r10 item 7: but when argv established NO launch
            // session (a hand-typed `claude` — no --resume, no session id on
            // the detected process) an AUTHENTICATED hook source can still
            // prove what this first record is: a `resume` source means the
            // named session carries a prior session's history, so it needs an
            // unverified EOF boundary (or Fresh when the file is empty),
            // exactly like a rebound. Ignoring it returned Fresh and replayed
            // the resumed history as current evidence.
            //
            // c-effortread r11 item 6: a session id learned from the HOOK
            // (found.session_id_from_hook) establishes launch provenance no
            // more than no id at all — only a pre-spawn capture or an argv
            // session/resume does.
            let launch_session_established = pre_resume_mode.is_some()
                || (found.session_id.is_some() && !found.session_id_from_hook);
            let hook_provenance = if !launch_session_established {
                match start_source {
                    Some("resume") => Some(ResumeMode::for_explicit_resume(transcript_path)),
                    Some("clear" | "startup" | "compact") => Some(ResumeMode::Fresh),
                    _ => None,
                }
            } else {
                None
            };
            (
                pre_resume_mode
                    .or(hook_provenance)
                    .unwrap_or_else(|| first_session_mode(found, transcript_path)),
                false,
            )
        } else {
            // A later in-TUI session (/resume, /clear). The SessionStart hook
            // source captured at delivery decides:
            // - clear/startup/compact → a NEW empty session → Fresh, even if
            //   the transcript already holds the queued/pasted prompt now;
            // - resume → c-effortread r13 item 5: explicit resume provenance
            //   stays unverified even while its transcript is absent (an
            //   unverified EOF boundary when the file is available, else bare
            //   Unverified) — a rename-and-restore must never reopen as Fresh;
            // - unknown → the file carries (or may carry) another session's
            //   history → unverified EOF anchor (or Fresh if empty).
            let mode = match start_source {
                Some("clear" | "startup" | "compact") => ResumeMode::Fresh,
                Some("resume") => ResumeMode::for_explicit_resume(transcript_path),
                _ => transcript_path
                    .map(ResumeMode::rebound_mode)
                    .unwrap_or(ResumeMode::Fresh),
            };
            (mode, true)
        };
        let entry = StickySessionMode { mode, rebound };
        self.modes.insert(key, entry);
        entry
    }

    /// The recorded mode for `(pid, session_id)`, if a SessionStart for it has
    /// been ingested this spawn.
    fn mode_for(&self, pid: i32, session_id: &str) -> Option<StickySessionMode> {
        self.modes.get(&(pid, session_id.to_owned())).copied()
    }
}
/// Decide one tick's epoch [`ResumeMode`].
///
/// `sticky` is the boundary measured at this (pid, session)'s authenticated
/// SessionStart report (r6 items 3/4): it exists for every hook-bound
/// session, INCLUDING the first bind, so a fresh launch whose user
/// `/resume`s before typing is bounded from the older session's own snapshot
/// instead of byte 0. A sticky `rebound` session kills the pre-spawn launch
/// mode; the launched process's own first session keeps the pre-spawn mode.
/// Without a sticky report (pid-file/argv-only detection, no hook yet) the
/// mode derives from the pre-spawn capture or the detected argv provenance.
fn epoch_mode(
    binding: &mut LaunchModeBinding,
    pre_resume_mode: Option<ResumeMode>,
    sticky: Option<StickySessionMode>,
    found: &Detected,
) -> ResumeMode {
    let (pid, session) = (found.pid, found.session_id.clone().unwrap_or_default());
    if let Some(sticky) = sticky {
        if sticky.rebound {
            // A later in-TUI session: the launch boundary can never prove it.
            *binding = LaunchModeBinding::Dead(sticky.mode);
            return sticky.mode;
        }
        // c-effortread r8 item 6 (Opus): a non-rebound sticky entry IS the
        // launch session. A stored Dead mode was produced by a PRIOR rebound
        // session and must not govern this one (returning another session's
        // Dead(Fresh) made /resume X fall back to TranscriptTail::new and
        // replay X's history as Current). Use the launch sticky's own measured
        // mode and re-bind.
        // The launched process's own first reported session.
        if let Some(pre) = pre_resume_mode {
            return match *binding {
                LaunchModeBinding::Bound(bound_pid, ref bound_session)
                    if bound_pid == pid
                        && (bound_session.is_empty()
                            || session.is_empty()
                            || bound_session == &session) =>
                {
                    if bound_session.is_empty() && !session.is_empty() {
                        *binding = LaunchModeBinding::Bound(pid, session);
                    }
                    pre
                }
                // A Dead mode from a rebound session must not govern the
                // launch session: use the launch sticky's own mode and re-bind.
                LaunchModeBinding::Dead(_) => {
                    *binding = LaunchModeBinding::Bound(pid, session);
                    sticky.mode
                }
                _ => {
                    *binding = LaunchModeBinding::Bound(pid, session);
                    pre
                }
            };
        }
        // New launch / login shell: use the mode measured at SessionStart.
        return match *binding {
            // Same item-6 fix for the no-pre-resume path.
            LaunchModeBinding::Dead(_) => {
                *binding = LaunchModeBinding::Bound(pid, session);
                sticky.mode
            }
            _ => {
                *binding = LaunchModeBinding::Bound(pid, session);
                sticky.mode
            }
        };
    }
    // No authenticated session report yet (pid-file/argv-only detection).
    let Some(mode) = pre_resume_mode else {
        return shell_resume_mode(found);
    };
    match *binding {
        LaunchModeBinding::Unbound => {
            *binding = LaunchModeBinding::Bound(pid, session);
            mode
        }
        // Same process and a compatible session (still unknown on one side, or
        // unchanged): the pre-spawn boundary still proves this foreground.
        // Refine a remembered empty id once the hook reports it.
        LaunchModeBinding::Bound(bound_pid, ref bound_session)
            if bound_pid == pid
                && (bound_session.is_empty()
                    || session.is_empty()
                    || bound_session == &session) =>
        {
            if bound_session.is_empty() && !session.is_empty() {
                *binding = LaunchModeBinding::Bound(pid, session);
            }
            mode
        }
        // A previously stored Dead mode stays in force on later ticks.
        LaunchModeBinding::Dead(dead_mode) => dead_mode,
        // A pid change or a known session id flipping without an authenticated
        // rebind: fall back to detected provenance and hold it.
        LaunchModeBinding::Bound(..) => {
            let detected = shell_resume_mode(found);
            *binding = LaunchModeBinding::Dead(detected);
            detected
        }
    }
}

/// Resume mode a re-opened hydrator uses when continuing a captured anchor.
///
/// c-effortread r9 item 3: this keys on the anchor's ORIGIN, not on whether
/// its gate was open. A LIVE (Fresh) anchor reopens as [`ResumeMode::Fresh`];
/// a RESUME anchor — even a *verified* one currently reading Current — reopens
/// in the session's OWN `epoch` mode. Otherwise a verified-resume X whose
/// inode changed while inactive would continue-fail into `Hydrator::open` and
/// fall back to Fresh, replaying X's history as current evidence.
fn continuation_resume_mode(anchor: Option<&TailAnchor>, epoch: ResumeMode) -> ResumeMode {
    match anchor {
        Some(anchor) if anchor.is_live_origin() => ResumeMode::Fresh,
        _ => epoch,
    }
}

/// Boundary for a pid's FIRST session that has no pre-spawn capture.
///
/// The decision is argv provenance, not the transcript size when the
/// SessionStart report is ingested (an initial prompt races the 800 ms poll
/// and may already have written records):
/// - a non-resume argv (`claude`, `claude --session-id <id>`) is
///   [`ResumeMode::Fresh`] however many bytes the file already holds;
/// - a resume argv (`--resume`, `--continue`, `-c`) is a known resume
///   without a proven boundary: an unverified EOF anchor when the file
///   exists, bare [`ResumeMode::Unverified`] (retry next tick, never byte 0)
///   when it is still absent.
fn first_session_mode(found: &Detected, transcript_path: Option<&Path>) -> ResumeMode {
    if !found.resume {
        return ResumeMode::Fresh;
    }
    transcript_path
        .and_then(ResumeBoundary::unverified_eof)
        .map_or(ResumeMode::Unverified, ResumeMode::Boundary)
}

/// Epoch resume mode for an agent the terminal did NOT launch (a hand-typed
/// `claude --resume` in a login shell).
///
/// r4 item 2: even with a uuid and a readable process start time this is
/// [`ResumeMode::Unverified`], never time-verified — `exec claude --resume`
/// keeps the shell's PID/start time and a backward clock step defeats
/// timestamp reasoning. Only a Remuda-launched agent with a pre-exec EOF
/// snapshot (the caller's `pre_resume_boundary`) is proven. A non-resume
/// foreground agent is [`ResumeMode::Fresh`].
fn shell_resume_mode(found: &Detected) -> ResumeMode {
    if found.resume {
        ResumeMode::Unverified
    } else {
        ResumeMode::Fresh
    }
}

/// c-effortread r10 item 6: record an explicit withdrawal of one
/// `(pid, session)` transcript. The cached anchor (if any) is downgraded so
/// `TranscriptTail::continued` cannot restore a verified gate, and the key is
/// persisted in `withdrawn` even when no anchor was ever captured (a demote
/// with the file already absent) — the boundary fallback then opens the
/// restored session at an unverified EOF.
fn mark_session_withdrawn(
    key: (i32, String),
    read_anchors: &mut std::collections::HashMap<(i32, String), TailAnchor>,
    withdrawn: &mut std::collections::HashSet<(i32, String)>,
) {
    tracing::warn!(
        pid = key.0,
        session = %key.1,
        "promoted transcript explicitly withdrawn; later continuation stays unverified"
    );
    if let Some(anchor) = read_anchors.get_mut(&key) {
        anchor.mark_withdrawn();
    }
    withdrawn.insert(key);
}

/// Apply one NATIVE SessionStart binding for the current foreground — the
/// single production seam the poll loop runs once per tick and the one tests
/// drive (c-effortread r11 item 7: no hand-built `table.record` / rebind /
/// `mark_session_withdrawn` sequences).
///
/// 1. classifies the bus-derived report's sticky boundary BEFORE it is
///    ingested into `start_sources` (a /clear whose transcript already holds
///    the queued prompt is sized Fresh from the authenticated source, not from
///    file size);
/// 2. runs the authenticated rebind (or first-claim) against the observed
///    foreground, dropping/marking the previous session;
/// 3. explicitly withdraws the live session when the report names a different
///    one;
/// 4. ingests the report so later ticks (and pid-file binds) see its source.
///
/// Returns true when the rebind/first-claim dropped the live hydrator.
#[allow(clippy::too_many_arguments)]
fn apply_native_session_binding(
    bindings: &BindingHandle,
    found: &Detected,
    binding: &remuda_signal::SessionBinding,
    pre_resume_mode: Option<ResumeMode>,
    session_modes: &mut SessionModeTable,
    read_anchors: &mut std::collections::HashMap<(i32, String), TailAnchor>,
    withdrawn_sessions: &mut std::collections::HashSet<(i32, String)>,
    active_session: &mut Option<(i32, String)>,
) -> bool {
    // One report derived from the NATIVE hook binding. Its source was folded
    // by the bus at hook delivery (r9 item 1): reading it here sizes the first
    // tick correctly even before the report is ingested into `start_sources`.
    let hook_report = SessionStartReport::from_bus_binding(binding);
    if session_modes
        .mode_for(binding.pid, &binding.session_id)
        .is_none()
    {
        session_modes.record(
            binding.pid,
            &binding.session_id,
            binding.transcript_path.as_deref().map(std::path::Path::new),
            pre_resume_mode,
            hook_report
                .as_ref()
                .and_then(|report| report.start_source.as_deref()),
            found,
        );
    }
    let Some(report) = hook_report else {
        return false;
    };
    // r7 item 1: the authenticated report names the session ACTUALLY live in
    // this pid, both for a rebind (/resume, /clear after the launch session
    // was bound) and for the lazy-create first claim (argv still names the
    // launch session while the hook already reports the new one).
    let rebound = bindings.rebind_authenticated_start(found, &report);
    let first_claim = !rebound
        && bindings.binding().is_none()
        && found.session_id.as_deref() != Some(report.session_id.as_str())
        && bindings.bind_authenticated_report(found, &report);
    if rebound || first_claim {
        // c-effortread r10 item 6: switching sessions EXPLICITLY withdraws the
        // transcript the live hydrator was reading — it will not be polled
        // while absent and its file cannot be proven unchanged, so a later
        // restore of the old (pid, session) reopens unverified even with the
        // same inode or a cached verified anchor.
        if let Some(previous) = active_session.take()
            && previous.1 != report.session_id
        {
            mark_session_withdrawn(previous, read_anchors, withdrawn_sessions);
        }
    }
    bindings.ingest_session_start(report);
    rebound || first_claim
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
async fn maintain_binding(
    bindings: &BindingHandle,
    ctx: &PromoteCtx,
    found: &Detected,
    hydrator: &mut Option<Hydrator>,
    announced: &mut Option<String>,
    events: &mpsc::Sender<Observation>,
    seq: &AtomicU64,
    effort_bridge: Option<&Arc<crate::effort::EffortBridge>>,
    launch_effort: Option<EffortSelection>,
    model: Option<&ModelSync>,
    permission_bridge: Option<&Arc<crate::permission::PermissionBridge>>,
    launch_permission: Option<remuda_protocol::ClaudePermissionMode>,
    session_modes: &mut SessionModeTable,
    launch_mode: &mut LaunchModeBinding,
    pre_resume_mode: Option<ResumeMode>,
    read_anchors: &mut std::collections::HashMap<(i32, String), TailAnchor>,
    effort_seeds: &mut std::collections::HashMap<(i32, String), EffortTrackerSeed>,
    withdrawn_sessions: &mut std::collections::HashSet<(i32, String)>,
    active_session: &mut Option<(i32, String)>,
) {
    // Unit-test entry point: a degraded hydrator is simply dropped (these
    // tests drive maintain_binding in isolation without the poll loop's
    // FinaliseGuard).
    let mut finalize_replaced = |_old: Hydrator| std::future::ready(());
    maintain_binding_inner(
        bindings,
        ctx,
        found,
        hydrator,
        announced,
        events,
        seq,
        effort_bridge,
        launch_effort,
        model,
        permission_bridge,
        launch_permission,
        session_modes,
        launch_mode,
        pre_resume_mode,
        read_anchors,
        effort_seeds,
        withdrawn_sessions,
        active_session,
        &mut finalize_replaced,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn maintain_binding_inner<FinalizeFut>(
    bindings: &BindingHandle,
    ctx: &PromoteCtx,
    found: &Detected,
    hydrator: &mut Option<Hydrator>,
    announced: &mut Option<String>,
    events: &mpsc::Sender<Observation>,
    seq: &AtomicU64,
    effort_bridge: Option<&Arc<crate::effort::EffortBridge>>,
    launch_effort: Option<EffortSelection>,
    model: Option<&ModelSync>,
    permission_bridge: Option<&Arc<crate::permission::PermissionBridge>>,
    launch_permission: Option<remuda_protocol::ClaudePermissionMode>,
    session_modes: &mut SessionModeTable,
    launch_mode: &mut LaunchModeBinding,
    pre_resume_mode: Option<ResumeMode>,
    read_anchors: &mut std::collections::HashMap<(i32, String), TailAnchor>,
    // c-effortread r8 item 5: tracker latch carried across re-promotion.
    effort_seeds: &mut std::collections::HashMap<(i32, String), EffortTrackerSeed>,
    // c-effortread r10 item 6: explicitly withdrawn sessions and the live key.
    withdrawn_sessions: &mut std::collections::HashSet<(i32, String)>,
    active_session: &mut Option<(i32, String)>,
    // c-usagefu r20 item 2: how to cancellation-safely finalize the hydrator
    // when the bound transcript degrades (the poll loop passes its
    // FinaliseGuard produce+publish path).
    finalize_replaced: &mut (dyn FnMut(Hydrator) -> FinalizeFut + Send),
) where
    FinalizeFut: std::future::Future<Output = ()> + Send,
{
    // Deterministic channels get first crack at an unbound, healthy epoch.
    if bindings.binding().is_none() && !bindings.degraded() {
        bindings.resolve(found);
    }

    // A binding that arrived (deterministically or by a manual answer) retires
    // the picker question before anything else is journaled.
    if bindings.binding().is_some()
        && !bindings.degraded()
        && let Some(payload) = bindings.take_picker_retirement("resolved", "transcript-bound")
        && emit(
            events,
            seq,
            ctx,
            SourceChannel::Runtime,
            Completeness::Structured,
            payload,
        )
        .await
        .is_err()
    {
        return;
    }

    if bindings.degraded() {
        if let Some(old) = hydrator.take() {
            finalize_replaced(old).await;
        }
        let related = bindings
            .binding()
            .map(|binding| {
                BTreeMap::from([
                    ("sessionId".to_owned(), binding.session_id),
                    ("source".to_owned(), binding.source.as_wire().to_owned()),
                    ("reason".to_owned(), "unavailable".to_owned()),
                ])
            })
            .unwrap_or_default();
        announce(
            announced,
            TRANSCRIPT_DEGRADED,
            diagnostic_lifecycle(
                TRANSCRIPT_DEGRADED,
                "bound transcript unavailable; staying on this claim",
                related,
            ),
            events,
            seq,
            ctx,
        )
        .await;
        return;
    }

    let Some(binding) = bindings.binding() else {
        // Unbound: offer the explicit picker, never an auto-pick.
        if let Some(interaction) = bindings.offer_picker(ctx) {
            let payload =
                ObservationPayload::InteractionRequested(Box::new(InteractionRequestedPayload {
                    interaction,
                }));
            if emit(
                events,
                seq,
                ctx,
                SourceChannel::Runtime,
                Completeness::Structured,
                payload,
            )
            .await
            .is_err()
            {
                return;
            }
        }
        announce(
            announced,
            TRANSCRIPT_UNBOUND,
            diagnostic_lifecycle(
                TRANSCRIPT_UNBOUND,
                "no deterministic transcript binding; manual pick offered",
                BTreeMap::new(),
            ),
            events,
            seq,
            ctx,
        )
        .await;
        return;
    };

    // Content cwd is the last, deliberately conservative sanity check. The
    // pid/hook/argv channels already proved identity; this only catches a slug
    // collision once records carrying a cwd have actually been written.
    if !bindings.content_cwd_check() {
        bindings.mark_degraded("transcript cwd does not match the promoted terminal");
        // c-ctxusage r5 item 1: DISCARD, do not finalise. A final tail read here
        // would journal exactly the mismatched-cwd records — messages, tool
        // calls and usage of a transcript the driver just declared foreign — so
        // the chip would show another session's context. Demote/promote/
        // rebind/vanish/close keep their reading finalise; only the
        // cwd-mismatch degrade drops the hydrator unread.
        hydrator.take();
        let mut related = BTreeMap::from([
            ("sessionId".to_owned(), binding.session_id.clone()),
            ("source".to_owned(), binding.source.as_wire().to_owned()),
            ("reason".to_owned(), "cwd-mismatch".to_owned()),
        ]);
        let _ = &mut related;
        announce(
            announced,
            TRANSCRIPT_DEGRADED,
            diagnostic_lifecycle(TRANSCRIPT_DEGRADED, "transcript cwd mismatch", related),
            events,
            seq,
            ctx,
        )
        .await;
        return;
    }

    if hydrator.is_none() {
        // r7 item 1: the sticky boundary is keyed by the session ACTUALLY
        // BOUND this tick — not by the argv session id, which stays the launch
        // session after an in-TUI /resume or /clear and is absent altogether
        // for a hand-typed claude. A pid-file/argv bind with no hook report
        // records its first mode here, from ARGV provenance (r7 item 2): a
        // non-empty transcript at bind time must not make a fresh launch
        // Unverified.
        if session_modes
            .mode_for(found.pid, &binding.session_id)
            .is_none()
        {
            session_modes.record(
                found.pid,
                &binding.session_id,
                Some(&binding.path),
                pre_resume_mode,
                bindings
                    .start_source_for(found.pid, &binding.session_id)
                    .as_deref(),
                found,
            );
        }
        let sticky = session_modes.mode_for(found.pid, &binding.session_id);
        let mut bound_found = found.clone();
        bound_found.session_id = Some(binding.session_id.clone());
        let mode = epoch_mode(launch_mode, pre_resume_mode, sticky, &bound_found);
        // r7 item 4(c): continue a re-promoted same-session tail at its last
        // read offset instead of replaying from the sticky anchor.
        // c-effortread r8 item 4(c): a CURRENT (Fresh, gate-open) anchor must
        // reopen as a live tail — never forced into a resume tail, which would
        // turn subsequent Fresh edges into Unverified.
        let session_key = (found.pid, binding.session_id.clone());
        // c-effortread r10 item 6: an explicitly withdrawn session (renamed
        // away while absent, or a vanished/displaced tracked file) reopens at
        // an UNVERIFIED EOF boundary — never from the cached verified anchor,
        // whose file could not be proven to have stayed put.
        let session_withdrawn = withdrawn_sessions.contains(&session_key);
        let continue_anchor = if session_withdrawn {
            None
        } else {
            read_anchors.get(&session_key).cloned()
        };
        // c-effortread r9 item 3: key on the anchor's ORIGIN, not its
        // gate-open flag — a verified resume currently reading Current must
        // reopen in this session's own mode so a failed continuation (its
        // inode changed while inactive) never falls back to Fresh and replays
        // the resumed history as current.
        let continue_resume_mode = if session_withdrawn {
            ResumeMode::Unverified
        } else {
            continuation_resume_mode(continue_anchor.as_ref(), mode)
        };
        *active_session = Some(session_key.clone());
        *hydrator = Hydrator::open(
            ctx,
            &binding,
            effort_bridge,
            launch_effort,
            model,
            permission_bridge,
            launch_permission,
            continue_resume_mode,
            continue_anchor,
            // c-effortread r8 item 5: re-latch the tracker from the last
            // hydrator for this (pid, session).
            effort_seeds
                .get(&(found.pid, binding.session_id.clone()))
                .cloned(),
        );
    }
    if let Some(active) = hydrator.as_mut() {
        // c-effortread r10 item 6: whether the tail just reported its tracked
        // file displaced (renamed/replaced/shrink below the cursor) on THIS
        // pump — an explicit withdrawal that must survive a later demote even
        // if no further pump captures the downgraded anchor.
        let displaced_after_pump = active.tail.displaced();
        match pump(active, events, seq, ctx).await {
            Ok(()) => {
                // Remember where this (pid, session) was read to, so a
                // demote/re-promotion continues here instead of re-emitting
                // the whole conversation.
                if let Some(anchor) = active.tail.anchor() {
                    read_anchors.insert((found.pid, binding.session_id.clone()), anchor);
                }
                if displaced_after_pump || active.tail.displaced() {
                    mark_session_withdrawn(
                        (found.pid, binding.session_id.clone()),
                        read_anchors,
                        withdrawn_sessions,
                    );
                }
                // c-effortread r8 item 5: remember the effort latch so the
                // replacement mapper after a re-promotion keeps level/flag.
                if let Some(seed) = EffortTrackerSeed::from_mapper(&active.mapper) {
                    effort_seeds.insert((found.pid, binding.session_id.clone()), seed);
                }
            }
            Err(()) => {
                // The bound file vanished: degrade, never silently rebind.
                bindings.mark_degraded("bound transcript file vanished");
                hydrator.take();
                // r10 item 6: the tracked file is gone with no further pump
                // to downgrade the cached anchor — persist the withdrawal now
                // so a restored same-(pid, session) rebind can never reopen
                // the stale verified anchor.
                let vanished_key = active_session
                    .take()
                    .unwrap_or_else(|| (found.pid, binding.session_id.clone()));
                mark_session_withdrawn(vanished_key, read_anchors, withdrawn_sessions);
                let related = BTreeMap::from([
                    ("sessionId".to_owned(), binding.session_id.clone()),
                    ("source".to_owned(), binding.source.as_wire().to_owned()),
                    ("reason".to_owned(), "vanished".to_owned()),
                ]);
                announce(
                    announced,
                    TRANSCRIPT_DEGRADED,
                    diagnostic_lifecycle(TRANSCRIPT_DEGRADED, "bound transcript vanished", related),
                    events,
                    seq,
                    ctx,
                )
                .await;
                return;
            }
        }
    }
    announce(
        announced,
        TRANSCRIPT_BOUND,
        bound_lifecycle(&binding),
        events,
        seq,
        ctx,
    )
    .await;
}

/// Emit `payload` only when this epoch has not announced `name` yet.
async fn announce(
    announced: &mut Option<String>,
    name: &str,
    payload: ObservationPayload,
    events: &mpsc::Sender<Observation>,
    seq: &AtomicU64,
    ctx: &PromoteCtx,
) {
    if announced.as_deref() == Some(name) {
        return;
    }
    if emit(
        events,
        seq,
        ctx,
        SourceChannel::Runtime,
        Completeness::Structured,
        payload,
    )
    .await
    .is_ok()
    {
        *announced = Some(name.to_owned());
    }
}

/// `agent_status` lifecycle Node folds into [`remuda_protocol::Activity`].
fn agent_status(status: ScreenStatus) -> ObservationPayload {
    native(
        LifecycleTopic::Turn,
        "agent_status",
        Knowledge::NotApplicable,
        status.label(),
        BTreeMap::new(),
        Severity::Info,
    )
    .1
}

/// §9.1 `instance.configure` effort lifecycle (`effort-queued`,
/// `effort-applied`, `effort-degraded`, …) emitted by the switch worker.
///
/// `related_ids` carries the configure's per-request id under
/// [`remuda_protocol::EFFORT_REQUEST_ID_KEY`] when the client minted one
/// (c-effortread r10).
pub(super) fn effort_lifecycle(
    status: &str,
    severity: Severity,
    related_ids: BTreeMap<String, String>,
) -> ObservationPayload {
    native(
        LifecycleTopic::Configuration,
        "instance.configure",
        Knowledge::NotApplicable,
        status,
        related_ids,
        severity,
    )
    .1
}

/// `instance.configure` permission lifecycle (`permission-queued`,
/// `permission-applied`, `permission-degraded`, …) from the wheel worker.
pub(super) fn permission_lifecycle(status: &str, severity: Severity) -> ObservationPayload {
    native(
        LifecycleTopic::Configuration,
        "instance.configure",
        Knowledge::NotApplicable,
        status,
        BTreeMap::new(),
        severity,
    )
    .1
}

/// One detection sample: the foreground process group and what it matched.
///
/// `rows` is kept beside the verdict because a `None` verdict is not the same
/// as an empty group: a hook report can only be trusted when its pid is
/// really running in *this* PTY's foreground group, and that is a question
/// about the rows, not about the match.
struct Sample {
    /// Rows of the PTY's foreground process group; empty on the screen path.
    rows: Vec<ProcessRow>,
    /// What [`detect`] made of them.
    found: Option<Detected>,
}

/// One detection sample: foreground process group first, screen as fallback.
async fn sample(state: &PtyState, table: &dyn ProcessTable, alias: Option<&LaunchAlias>) -> Sample {
    let pgid = {
        let master = state.master.lock().await;
        // `None` once the stop ladder has released the master (§5.3 step 2).
        // Falling through to the screen is right: there is no foreground group
        // to read, and the poller is about to be aborted anyway.
        master
            .as_ref()
            .and_then(|master| foreground_pgid(master.as_ref()))
    };
    if let Some(pgid) = pgid {
        let rows = table.process_group(pgid);
        let found = detect(&rows, alias);
        return Sample { rows, found };
    }
    Sample {
        rows: Vec::new(),
        found: remuda_screen::detect_from_screen(&state.screen_grid()).map(|kind| Detected {
            kind,
            pid: 0,
            session_id: None,
            hydrates_transcript: kind == AgentKind::Claude,
            resume: false,
            session_id_from_hook: false,
        }),
    }
}

/// Promote from an authenticated `SessionStart` when detection came up empty.
///
/// The gate is the PTY's own foreground process group: the hook's pid must be
/// one of the rows we just read. That makes this identity evidence rather than
/// a guess — the process that reported the session is the process this
/// terminal is running — and it keeps a nested or unrelated claude sharing the
/// socket from claiming the instance.
///
/// `kind` is what the launch started; a login shell that grew an agent has
/// only claude's overlay on its hook socket, so claude is the honest default.
fn promote_from_hook_binding(
    rows: &[ProcessRow],
    binding: &remuda_signal::SessionBinding,
    kind: AgentKind,
) -> Option<Detected> {
    if binding.pid <= 0 || !rows.iter().any(|row| row.pid == binding.pid) {
        return None;
    }
    // The hook payload names the session; whether it was a `--resume` comes
    // from the foreground process's own argv (the SessionStart payload does
    // not carry it), so a hand-typed resume in a login shell is gated too.
    let resume = rows
        .iter()
        .find(|row| row.pid == binding.pid)
        .map(|row| crate::promote::resume_provenance(&row.args).1)
        .unwrap_or(false);
    Some(Detected {
        kind,
        pid: binding.pid,
        session_id: Some(binding.session_id.clone()),
        hydrates_transcript: crate::promote::AGENT_TABLE
            .iter()
            .any(|entry| entry.kind == kind && entry.hydrates_transcript),
        resume,
        // r11 item 6: this identity came from the authenticated hook, not from
        // argv/pre-spawn provenance; the caller must not treat it as the
        // launch's own established session.
        session_id_from_hook: true,
    })
}

/// A bound transcript plus the mapper replaying it.
struct Hydrator {
    tail: TranscriptTail,
    mapper: TranscriptMapper,
    /// Launch-time observations (model snapshot) emitted on the first pump.
    pending: Vec<Observation>,
    /// Consecutive polls whose bound file could not be opened (live tail
    /// NotFound). A resume tail survives a one-poll ENOENT internally; a LIVE
    /// tail's file never legitimately vanishes, so after this many missing
    /// polls the binding degrades instead of waiting forever.
    missing_polls: u32,
}

/// c-effortread r8 item 5: per-(pid, session) EffortTracker state that must
/// survive a demote/re-promotion of the same pid/session. A new Hydrator builds
/// a fresh TranscriptMapper whose tracker forgets the last level/flag/
/// semantics; on a decoupled build with ultracode on at high, a later
/// assistant record without a re-sent ultra_effort attachment then observed
/// `{high, ultracode: None}` and the chip fell back to "high". Seeding the new
/// tracker from the last PUBLISHED observation restores the latch.
#[derive(Debug, Clone, Default)]
struct EffortTrackerSeed {
    /// The last published effort latch (level + tri-state flag + semantics +
    /// the channel that published the level, c-effortread r22 ruling S).
    last: Option<(
        remuda_protocol::EffortName,
        Option<bool>,
        remuda_protocol::EffortSemantics,
        remuda_protocol::LevelSource,
    )>,
}

impl EffortTrackerSeed {
    /// Capture the current tracker latch from a live mapper (c-effortread r8
    /// item 5). `None` when it has never observed a level.
    fn from_mapper(mapper: &TranscriptMapper) -> Option<Self> {
        mapper
            .effort_seed()
            .map(|(name, flag, semantics, level_source)| Self {
                last: Some((name, flag, semantics, level_source)),
            })
    }
}

/// Consecutive live-tail NotFound polls that mark the binding degraded.
const LIVE_MISSING_POLL_DEGRADE: u32 = 3;

impl Hydrator {
    /// Open the bound transcript for one detected foreground process /
    /// promotion epoch.
    ///
    /// `boundary` is the verified resume boundary for THIS epoch:
    /// - [`ResumeMode::Fresh`] → live tail from byte 0.
    /// - [`ResumeMode::Boundary`] → a verified resume: tail from the pre-exec /
    ///   process-start boundary.
    /// - [`ResumeMode::Unverified`] → a known but unprovable resume: anchor at
    ///   current EOF, hydrate conversation, never open the effort gate.
    #[allow(clippy::too_many_arguments)]
    fn open(
        ctx: &PromoteCtx,
        binding: &TranscriptBinding,
        effort_bridge: Option<&Arc<crate::effort::EffortBridge>>,
        launch_effort: Option<EffortSelection>,
        model: Option<&ModelSync>,
        permission_bridge: Option<&Arc<crate::permission::PermissionBridge>>,
        launch_permission: Option<remuda_protocol::ClaudePermissionMode>,
        mode: ResumeMode,
        continue_anchor: Option<TailAnchor>,
        // c-effortread r8 item 5: last published effort state from the prior
        // hydrator for the same (pid, session), restored across re-promotion.
        effort_seed: Option<EffortTrackerSeed>,
    ) -> Option<Self> {
        tracing::info!(
            instance_id = %ctx.instance_id.as_id(),
            session = %binding.session_id,
            source = binding.source.as_wire(),
            resumed = matches!(mode, ResumeMode::Boundary(_) | ResumeMode::Unverified),
            "hydrating promoted terminal from its bound Claude transcript"
        );
        let mut mapper = TranscriptMapper::new(
            remuda_protocol::DriverKind::ShellPty,
            ctx.instance_id.clone(),
            ctx.run_id.clone(),
            ctx.journal_id.clone(),
            ctx.host_id.clone(),
            binding.session_id.clone(),
            "promoted".to_owned(),
        );
        if let Some(bridge) = effort_bridge {
            mapper = mapper.with_effort_bridge(Arc::clone(bridge), launch_effort);
        }
        let mut pending = Vec::new();
        if let Some(model) = model {
            mapper = mapper.with_model_bridge(
                Arc::clone(&model.bridge),
                model.launch.clone(),
                model.catalog.clone(),
            );
            if let Some(id) = model.launch.as_deref().filter(|id| !id.is_empty())
                && let Ok(observations) = mapper.take_launch_model_snapshot(Some(id))
            {
                pending = observations;
            }
        }
        if let Some(bridge) = permission_bridge {
            mapper = mapper.with_permission_bridge(Arc::clone(bridge), launch_permission);
        }
        // c-effortread r8 item 5: re-latch effort level/flag/semantics from the
        // previous hydrator's last published observation on re-promotion.
        if let Some(last) = effort_seed.and_then(|seed| seed.last) {
            mapper = mapper.with_effort_seed(last);
        }
        // r7 item 4(c): a re-promotion of the same (pid, session) continues at
        // the last offset the previous hydrator read instead of replaying the
        // sticky anchor. A changed identity/shrink invalidates the captured
        // anchor, so fall back to the mode's own boundary/EOF open.
        let tail = continue_anchor
            .and_then(|anchor| TranscriptTail::continued(binding.path.clone(), anchor))
            .or_else(|| mode.open_tail(&binding.path))?;
        Some(Self {
            mapper,
            tail,
            pending,
            missing_polls: 0,
        })
    }

    /// Synchronous finalise for the task-abort safety net (no async emit is
    /// possible during Drop): one last tail read plus `mapper.finish()`,
    /// returning the MAPPED observations for the guard to stamp and
    /// best-effort `try_send`.
    fn finish_sync(&mut self) -> DriverResult<Vec<Observation>> {
        let mut observations = Vec::new();
        if let Ok(read) = self.tail.poll() {
            for (line, position) in read.lines.iter().zip(read.positions.iter().copied()) {
                observations.extend(self.mapper.map_line_at(line, Some(position))?);
            }
        }
        observations.extend(self.mapper.finish()?);
        Ok(observations)
    }
}

/// Final drain before the hydrator is dropped (demote / promote / rebind /
/// degrade / close — c-ctxusage r4 item 2): read whatever the tail gained,
/// then `finish()` the mapper so an assistant record with usage and no
/// stop_reason still publishes its counters.
///
/// Test-only entry point (the poll loop uses FinaliseGuard's cancellation-safe
/// produce/publish instead): synchronously produces into a local queue via the
/// shared produce_hydrator helper and publishes with ordinary awaited sends.
#[cfg(test)]
impl Hydrator {
    #[allow(dead_code)]
    async fn finalize(
        mut self,
        events: &mpsc::Sender<Observation>,
        seq: &AtomicU64,
        ctx: &PromoteCtx,
    ) {
        let mut produced: Vec<Observation> = Vec::new();
        produce_hydrator(&mut self, &mut produced);
        for mapped in produced {
            if let Some(observation) = stamp_mapped(ctx, seq, mapped)
                && events.send(observation).await.is_err()
            {
                return;
            }
        }
    }
}

/// Drain whatever the transcript gained since the last poll.
///
/// `Err(())` means the bound file can no longer be opened (it vanished); the
/// caller degrades rather than rebinding. Mapper failures on individual lines
/// are non-fatal bookkeeping noise.
async fn pump(
    hydrator: &mut Hydrator,
    events: &mpsc::Sender<Observation>,
    seq: &AtomicU64,
    ctx: &PromoteCtx,
) -> Result<(), ()> {
    // Launch snapshot (model list/current model) once, ahead of replayed lines.
    if !hydrator.pending.is_empty() {
        for observation in std::mem::take(&mut hydrator.pending) {
            if emit(
                events,
                seq,
                ctx,
                SourceChannel::Transcript,
                observation.completeness,
                observation.body,
            )
            .await
            .is_err()
            {
                return Err(());
            }
        }
    }
    // A resume tail survives a one-poll ENOENT internally (it returns an
    // empty Unverified read); a LIVE tail's file never legitimately vanishes.
    // Tolerate a few consecutive missing polls (a transient rename/save),
    // then degrade the binding rather than swallowing NotFound forever.
    let read = match hydrator.tail.poll() {
        Ok(read) => {
            hydrator.missing_polls = 0;
            read
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            hydrator.missing_polls = hydrator.missing_polls.saturating_add(1);
            if hydrator.missing_polls >= LIVE_MISSING_POLL_DEGRADE {
                return Err(());
            }
            return Ok(());
        }
        Err(_) => return Err(()),
    };
    // Apply provenance on EVERY poll, even with no lines: an unverified resume
    // must close the effort gate before its first record maps, and a
    // verified→unverified transition emits an explicit read-back-unavailable
    // edge clearing the projected effective state (item 5).
    let gate_edges = hydrator
        .mapper
        .apply_effort_provenance(read.provenance == TailProvenance::Current)
        .map_err(|_| ())?;
    // The mapper buffers an assistant run until something supersedes it, so
    // the last message of a batch would otherwise sit unseen until the next
    // record arrives — which, at the end of a turn, may be minutes away.
    // Flushing at the end of each poll is what makes a finished turn appear.
    // c-effortread r15 item 4: each record rides with its verified transcript
    // position so an armed slash attribution excludes pre-submission records.
    let mut batches: Vec<_> = gate_edges
        .into_iter()
        .map(|obs| Ok(vec![obs]))
        .chain(
            read.lines
                .iter()
                .zip(read.positions.iter().copied())
                .map(|(line, position)| hydrator.mapper.map_line_at(line, Some(position))),
        )
        .collect();
    batches.push(hydrator.mapper.flush());
    for batch in batches {
        let mapped = match batch {
            Ok(mapped) => mapped,
            Err(error) => {
                tracing::debug!(%error, "transcript line did not map");
                continue;
            }
        };
        for observation in mapped {
            // c-ctxusage r3 item 5: preserve the mapper-provided native time
            // (historical usage timestamps). §9.1 live/journal parity: preserve
            // the deterministic event id and the mapper's replay evidence too.
            if emit_mapped(events, seq, ctx, SourceChannel::Transcript, observation)
                .await
                .is_err()
            {
                return Err(());
            }
        }
    }
    Ok(())
}

/// Stamp and send one Observation on the driver's event channel.
///
/// Re-exported to the parent module so the exit waiter (§5.5) stamps its
/// lifecycle with the same identity and the same sequence counter as every
/// promotion event. Two emitters with two counters would interleave
/// unpredictably, and §5.5's exit would sort against promotion events by
/// accident rather than by order of observation.
pub(super) async fn emit_payload(
    events: &mpsc::Sender<Observation>,
    seq: &AtomicU64,
    ctx: &PromoteCtx,
    channel: SourceChannel,
    completeness: Completeness,
    body: ObservationPayload,
) -> DriverResult<()> {
    emit(events, seq, ctx, channel, completeness, body).await
}

async fn emit(
    events: &mpsc::Sender<Observation>,
    seq: &AtomicU64,
    ctx: &PromoteCtx,
    channel: SourceChannel,
    completeness: Completeness,
    body: ObservationPayload,
) -> DriverResult<()> {
    let observation = build(
        ctx,
        seq.fetch_add(1, Ordering::SeqCst) + 1,
        channel,
        completeness,
        body,
        Knowledge::Unknown {
            reason: "not-emitted".into(),
            evidence_event_ids: Vec::new(),
        },
    )?;
    events
        .send(observation)
        .await
        .map_err(|_| DriverError::ControlUnavailable)
}

/// Stamp and send a mapper-produced observation, PRESERVING its envelope:
/// the deterministic event id (§9.1 live/journal parity: an effort edge and
/// the journal tailer derive the same id — rebuilding it with a fresh random id
/// would publish a different one), the historical `native_at` (c-ctxusage r3
/// item 5), and the replay completeness/evidence the mapper computed.
async fn emit_mapped(
    events: &mpsc::Sender<Observation>,
    seq: &AtomicU64,
    ctx: &PromoteCtx,
    channel: SourceChannel,
    mapped: Observation,
) -> DriverResult<()> {
    let event_id = mapped.event_id;
    let native_at = mapped.native_at.clone();
    let evidence = mapped.evidence_event_ids.clone();
    let mut observation = build(
        ctx,
        seq.fetch_add(1, Ordering::SeqCst) + 1,
        channel,
        mapped.completeness,
        mapped.body,
        native_at,
    )?;
    observation.event_id = event_id;
    observation.evidence_event_ids = evidence;
    events
        .send(observation)
        .await
        .map_err(|_| DriverError::ControlUnavailable)
}

fn build(
    ctx: &PromoteCtx,
    seq: u64,
    channel: SourceChannel,
    completeness: Completeness,
    body: ObservationPayload,
    native_at: Knowledge<Timestamp>,
) -> DriverResult<Observation> {
    Ok(Observation {
        schema_version: SchemaVersion,
        event_id: EventId::new(),
        journal_id: ctx.journal_id.clone(),
        instance_id: ctx.instance_id.clone(),
        run_id: Some(ctx.run_id.clone()),
        host_id: ctx.host_id.clone(),
        process_generation: U64(1),
        run_generation: Some(U64(1)),
        seq: U64(seq),
        observed_at: now_ts()?,
        native_at,
        source: ObservationSource {
            driver_kind: remuda_protocol::DriverKind::ShellPty,
            driver_version: "shell-pty".into(),
            adapter_version: crate::capabilities::ADAPTER_VERSION.into(),
            channel,
            delivery: SourceDelivery::Live,
            native_session_id: Knowledge::Unknown {
                reason: "not-emitted".into(),
                evidence_event_ids: Vec::new(),
            },
            native_turn_id: Knowledge::NotApplicable,
            native_agent_id: Knowledge::NotApplicable,
            native_item_id: Knowledge::NotApplicable,
            native_event_id: Knowledge::NotApplicable,
            native_request_id: remuda_protocol::NativeRequestKey::None,
            source_cursor: SourceCursor::Runtime(Box::new(RuntimeCursor {
                ledger_revision: U64(seq),
            })),
        },
        completeness,
        raw_ref: None,
        evidence_event_ids: Vec::new(),
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_protocol::{QuestionAnswer, QuestionFieldAnswer};
    use std::io::Write;

    fn ctx_in(dir: &Path) -> PromoteCtx {
        PromoteCtx {
            instance_id: InstanceId::new(),
            host_id: HostId::new(),
            journal_id: Id::new("obj").expect("journal id"),
            run_id: RunId::new(),
            cwd: dir.to_path_buf(),
            claude_home: dir.to_path_buf(),
        }
    }

    #[test]
    fn a_quiet_hook_tier_loses_its_screen_idle_veto_after_the_bound() {
        use remuda_screen::HookHealth;
        // A live hook turn with a fresh record keeps rule 6: the screen's idle
        // edge is suppressed (the long-tool gap is real but hooks still speak).
        assert_eq!(hook_health_for(Some(true), true, true), HookHealth::Healthy);
        // The same live turn once no record has arrived within the bound drops
        // to Stalled: the screen latch is allowed to lower busy from its own
        // idle read, so agent_status idles even with no Stop hook.
        assert_eq!(
            hook_health_for(Some(true), true, false),
            HookHealth::Stalled
        );
        // The sticky "ever materialised" guard downgrades the same way on a
        // poll that cannot read the process table but knows hooks went quiet.
        assert_eq!(hook_health_for(None, true, false), HookHealth::Stalled);
        // A fresh sticky miss still holds, a hook-confirmed end never holds,
        // and a tier that never materialised stays exactly that.
        assert_eq!(hook_health_for(None, true, true), HookHealth::Healthy);
        assert_eq!(
            hook_health_for(Some(false), true, true),
            HookHealth::NeverMaterialised
        );
        assert_eq!(
            hook_health_for(None, false, false),
            HookHealth::NeverMaterialised
        );
    }

    #[test]
    fn a_clean_stop_emits_no_hook_silence_record_and_a_quiet_end_naming_only_real_failures() {
        use remuda_signal::hook_silence::HookSilenceReason;
        // A fresh tier (a Stop just landed) is a normal end: no reason, so the
        // badge gains no false「journal 推送停滞」and no hook.silence row is
        // journaled.
        assert_eq!(silence_reason_for(true, true, true), None);
        assert_eq!(silence_reason_for(true, false, false), None);
        // Relay + socket both healthy but stale: an ordinary quiet end, NOT a
        // link-stalled claim (the screen poller does not hold the pump cursor).
        assert_eq!(silence_reason_for(false, true, true), None);
        // A real, named failure surfaces in dependency order.
        assert_eq!(
            silence_reason_for(false, false, false),
            Some(HookSilenceReason::RelayMissing)
        );
        assert_eq!(
            silence_reason_for(false, true, false),
            Some(HookSilenceReason::SocketRefused)
        );
    }

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        let mut file = std::fs::File::create(path).expect("create");
        file.write_all(body.as_bytes()).expect("write");
    }

    fn detected_claude(pid: i32, session_id: Option<&str>) -> Detected {
        Detected {
            kind: AgentKind::Claude,
            pid,
            session_id: session_id.map(str::to_owned),
            hydrates_transcript: true,
            resume: false,
            session_id_from_hook: false,
        }
    }

    fn slug_session(home: &Path, cwd: &Path, session: &str, body: &str) -> PathBuf {
        let path =
            crate::claude_transcript::project_dir(home, cwd).join(format!("{session}.jsonl"));
        write(&path, body);
        path
    }

    const CORRECT: &str = "aaaaaaaa-2222-4333-8444-aaaaaaaaaaaa";
    const BUSY_OTHER: &str = "bbbbbbbb-8888-4777-8666-bbbbbbbbbbbb";
    const LATE_STARTER: &str = "cccccccc-9999-4777-8666-cccccccccccc";

    #[test]
    fn bypass_needs_a_real_foreground_pid_missing_its_exec_marker() {
        let dir = tempfile::tempdir().unwrap();
        let markers = dir.path().join("shim-pids");
        let found = detected_claude(42, None);
        assert!(!shim_bypass_evidence(&found, None, &markers, true));
        std::fs::create_dir(&markers).unwrap();
        assert!(shim_bypass_evidence(&found, None, &markers, true));
        assert!(
            !shim_bypass_evidence(&found, None, &markers, false),
            "P2's direct pinned launch intentionally does not traverse the shell shim"
        );
        assert!(
            !shim_bypass_evidence(&found, Some(42), &markers, true),
            "matching hooks outrank a missing marker"
        );
        assert!(
            !shim_bypass_evidence(&detected_claude(0, None), None, &markers, true),
            "screen-only detection has no PID proof"
        );
        std::fs::write(markers.join("42"), "").unwrap();
        assert!(!shim_bypass_evidence(&found, None, &markers, true));
    }

    #[test]
    fn interruption_output_recognizes_split_ansi_and_excludes_osc_titles() {
        let mut output = InterruptionOutput::default();
        let bytes = concat!(
            "\x1b]0;[Request interrupted by user\x1b\\",
            "❯ What should Claude do instead?\r\n",
            "\x1b[31mInterrup\x1b[0mted · What should Claude do instead?\r\n",
            "[Request interrupted by user for tool use]"
        )
        .as_bytes();
        for byte in bytes {
            output.feed(std::slice::from_ref(byte));
        }
        assert_eq!(output.count(), 2);
    }

    #[test]
    fn fresh_replacement_marker_confirms_but_unchanged_redraw_and_idle_do_not() {
        use remuda_screen::{Emulator, ScreenGrid};
        let mut emulator = Emulator::with_scrollback(100, 4, 0);
        let mut output = InterruptionOutput::default();
        let old = "Interrupted · What should Claude do instead?\r\nfirst prompt working";
        output.feed(old.as_bytes());
        emulator.feed(old.as_bytes());
        let mut baseline = InterruptBaseline::default();
        baseline.arm(output.count(), &emulator.grid());
        assert!(
            !baseline.confirmed(output.count(), &ScreenGrid::from_raw("❯")),
            "idle alone proves no interruption"
        );

        let replacement =
            "\x1b[2J\x1b[Hsecond prompt\r\nInterrupted · What should Claude do instead?\r\n❯";
        // The old marker disappears: the final viewport still contains one.
        for chunk in replacement.as_bytes().chunks(3) {
            output.feed(chunk);
            emulator.feed(chunk);
        }
        assert_eq!(output.count(), 2);
        assert_eq!(emulator.grid().flat().matches("Interrupted").count(), 1);
        assert!(baseline.confirmed(output.count(), &emulator.grid()));

        baseline.arm(output.count(), &emulator.grid());
        output.feed(replacement.as_bytes());
        emulator.feed(replacement.as_bytes());
        assert_eq!(output.count(), 3);
        assert!(
            !baseline.confirmed(output.count(), &emulator.grid()),
            "an unchanged historical repaint is not new turn evidence"
        );
        assert!(
            !baseline.confirmed(output.count(), &ScreenGrid::from_raw("❯")),
            "the ignored repaint cannot settle a later screen change"
        );
    }

    #[test]
    fn a_historical_marker_with_a_changing_native_spinner_cannot_settle_cancel() {
        use remuda_screen::ScreenGrid;
        let working = |seconds| {
            // The saved Claude 2.1.270 tool screen has this elapsed spinner,
            // not `esc to interrupt`; the old general matcher calls it idle.
            ScreenGrid::from_lines([
                "❯ earlier turn".into(),
                "⎿ Interrupted · What should Claude do instead?".into(),
                "❯ Use Bash to run sleep 30, then reply FINISHED.".into(),
                "Running 1 shell command…".into(),
                format!("·Befuddling… ({seconds}s · ↓ 200 tokens · thought for 4s)"),
                "────────────────".into(),
                "❯".into(),
            ])
        };
        let mut output = InterruptionOutput::default();
        let mut baseline = InterruptBaseline::default();
        let before = working(39);
        output.feed(before.text().as_bytes());
        baseline.arm(output.count(), &before);
        let repaint = working(40);
        output.feed(repaint.text().as_bytes());
        assert_eq!(output.count(), 2, "the historical marker was repainted");
        assert_eq!(
            remuda_screen::screen_status(&repaint),
            Some(ScreenStatus::Idle)
        );
        let partial_repaint = ScreenGrid::from_lines(repaint.lines[..5].iter().cloned());
        assert!(
            !baseline.confirmed(output.count(), &partial_repaint),
            "a submitted user row is not the missing final composer"
        );
        assert!(!baseline.confirmed(output.count(), &repaint));

        let after = ScreenGrid::from_lines([
            "❯ earlier turn",
            "⎿ Interrupted · What should Claude do instead?",
            "❯ Use Bash to run sleep 30, then reply FINISHED.",
            "Ran 1 shell command",
            "⎿ Interrupted · What should Claude do instead?",
            "⎿ Interrupted · What should Claude do instead?",
            "────────────────",
            "❯",
        ]);
        assert!(
            !baseline.confirmed(output.count(), &after),
            "an ignored historical repaint cannot settle a later frame"
        );
        let partial = ScreenGrid::from_lines(after.lines[..after.lines.len() - 1].iter().cloned());
        output.feed(partial.text().as_bytes());
        assert!(
            !baseline.confirmed(output.count(), &partial),
            "the new marker can arrive before its composer in a separate read"
        );
        let marker_count = output.count();
        output.feed("\n❯".as_bytes());
        assert_eq!(output.count(), marker_count);
        assert!(
            baseline.confirmed(output.count(), &after),
            "the later composer completes that same fresh marker evidence"
        );
    }

    #[test]
    fn a_terminal_with_no_agent_journals_nothing() {
        let mut state = PromoteState::default();
        let now = now_ts().expect("timestamp");
        assert!(transition(&mut state, None, &now).is_empty());
        assert_eq!(state, PromoteState::default());
    }

    #[test]
    fn first_detection_journals_a_diagnostic_then_the_promotion() {
        let mut state = PromoteState::default();
        let found = detected_claude(42, Some("04b95a78-e876-4212-aa9c-a6482f30f583"));
        let now = now_ts().expect("timestamp");
        let events = transition(&mut state, Some(&found), &now);
        assert_eq!(names(&events), vec![DETECTED, PROMOTED]);
        assert_eq!(state.kind, Some(AgentKind::Claude));
        assert_eq!(state.pid, Some(42));
    }

    #[test]
    fn promotion_is_idempotent_while_the_same_agent_holds_the_terminal() {
        let mut state = PromoteState::default();
        let now = now_ts().expect("timestamp");
        let found = detected_claude(42, None);
        transition(&mut state, Some(&found), &now);
        assert!(transition(&mut state, Some(&found), &now).is_empty());
    }

    #[test]
    fn exiting_the_agent_demotes_once_and_then_stays_quiet() {
        let mut state = PromoteState::default();
        let now = now_ts().expect("timestamp");
        transition(&mut state, Some(&detected_claude(42, None)), &now);
        let events = transition(&mut state, None, &now);
        assert_eq!(names(&events), vec![DEMOTED, DETECTED]);
        assert_eq!(state, PromoteState::default());
        assert!(transition(&mut state, None, &now).is_empty());
    }

    #[test]
    fn swapping_agents_demotes_the_old_kind_before_promoting_the_new_one() {
        let mut state = PromoteState::default();
        let now = now_ts().expect("timestamp");
        transition(&mut state, Some(&detected_claude(42, None)), &now);
        let codex = Detected {
            kind: AgentKind::Codex,
            pid: 43,
            session_id: None,
            hydrates_transcript: false,
            resume: false,
            session_id_from_hook: false,
        };
        let events = transition(&mut state, Some(&codex), &now);
        assert_eq!(names(&events), vec![DEMOTED, DETECTED, PROMOTED]);
        assert_eq!(state.kind, Some(AgentKind::Codex));
    }

    fn names(payloads: &[(Completeness, ObservationPayload)]) -> Vec<String> {
        payloads
            .iter()
            .filter_map(|(_, payload)| match payload {
                ObservationPayload::Lifecycle(lifecycle) => match lifecycle.as_ref() {
                    LifecyclePayload::Native(native) => Some(native.native_name.clone()),
                    LifecyclePayload::Entity(_) => None,
                },
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_cwd_mismatch_degrade_discards_the_foreign_transcript_unread() {
        // c-ctxusage r5 item 1: after a bind is established, the transcript
        // grows a record whose top-level cwd is a DIFFERENT directory. The
        // degrade must DISCARD the hydrator without a final read — the
        // foreign record's Message/Usage must never reach this instance.
        let dir = tempfile::tempdir().expect("tmp");
        let home = dir.path().join("home");
        let cwd = dir.path().join("repo");
        let foreign = dir.path().join("elsewhere");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let session = "ffffffff-1111-4222-8333-ffffffffffff";

        // Phase 1 record: assistant text+usage, NO stop_reason, no top-level cwd
        // — the content check is still indeterminate, so the bind hydrates.
        let first = serde_json::json!({
            "type": "assistant",
            "uuid": "msg-foreign-rec",
            "timestamp": "2020-01-01T00:00:00.000Z",
            "isSidechain": false,
            "message": {
                "id": "msg-own",
                "role": "assistant",
                "type": "message",
                "model": "m",
                "stop_reason": null,
                "content": [{"type": "text", "text": "OWN"}],
                "usage": {
                    "input_tokens": 1,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "output_tokens": 1
                }
            }
        });
        let path = slug_session(&home, &cwd, session, &format!("{}\n", first));

        let bindings = BindingHandle::empty();
        bindings.begin_epoch(&cwd, &home);
        // Force the deterministic claim directly (the transcript slug is under
        // the real cwd, so identity channels accept it; only its later content
        // disagrees).
        {
            let mut slot = bindings
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot.binding = Some(crate::claude_transcript::TranscriptBinding {
                session_id: session.into(),
                path: path.clone(),
                cwd: cwd.clone(),
                source: crate::claude_transcript::BindingSource::Hook,
            });
        }

        let ctx = ctx_in(&cwd);
        let found = detected_claude(42, Some(session));
        let (tx, mut rx) = mpsc::channel::<Observation>(128);
        let mut hydrator: Option<Hydrator> = None;
        let mut announced: Option<String> = None;
        let mut session_modes = SessionModeTable::default();
        let mut launch_mode = LaunchModeBinding::Unbound;
        let mut read_anchors = std::collections::HashMap::new();
        let mut effort_seeds = std::collections::HashMap::new();
        let mut withdrawn_sessions = std::collections::HashSet::new();
        let mut active_session: Option<(i32, String)> = None;
        maintain_binding(
            &bindings,
            &ctx,
            &found,
            &mut hydrator,
            &mut announced,
            &tx,
            &std::sync::atomic::AtomicU64::new(0),
            None,
            None,
            None,
            None,
            None,
            &mut session_modes,
            &mut launch_mode,
            None,
            &mut read_anchors,
            &mut effort_seeds,
            &mut withdrawn_sessions,
            &mut active_session,
        )
        .await;
        assert!(
            !bindings.degraded(),
            "still indeterminate: no cwd record yet"
        );
        assert!(hydrator.is_some(), "the hydrator opened against the claim");
        // Phase 1 drains only content (no stop_reason -> no usage).
        while let Ok(_obs) = rx.try_recv() {}

        // Phase 2: a foreign-cwd user record, then a finalised assistant turn.
        // Under r4 the reading finalise polled these exact records and
        // journaled them; the fix must not.
        use std::io::Write;
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open transcript");
            writeln!(
                file,
                "{}",
                serde_json::json!({
                    "type": "user",
                    "uuid": "user-foreign",
                    "timestamp": "2020-01-01T00:00:01.000Z",
                    "cwd": foreign.to_string_lossy(),
                    "message": {"role": "user", "content": "go"}
                })
            )
            .unwrap();
            writeln!(
                file,
                "{}",
                serde_json::json!({
                    "type": "assistant",
                    "uuid": "msg-evil-rec",
                    "timestamp": "2020-01-01T00:00:02.000Z",
                    "isSidechain": false,
                    "message": {
                        "id": "msg-evil",
                        "role": "assistant",
                        "type": "message",
                        "model": "m",
                        "stop_reason": "end_turn",
                        "content": [{"type": "text", "text": "EVIL_FOREIGN_CONTENT"}],
                        "usage": {
                            "input_tokens": 999,
                            "cache_creation_input_tokens": 0,
                            "cache_read_input_tokens": 0,
                            "output_tokens": 999
                        }
                    }
                })
            )
            .unwrap();
        }

        maintain_binding(
            &bindings,
            &ctx,
            &found,
            &mut hydrator,
            &mut announced,
            &tx,
            &std::sync::atomic::AtomicU64::new(0),
            None,
            None,
            None,
            None,
            None,
            &mut session_modes,
            &mut launch_mode,
            None,
            &mut read_anchors,
            &mut effort_seeds,
            &mut withdrawn_sessions,
            &mut active_session,
        )
        .await;
        assert!(bindings.degraded(), "the foreign cwd degrades the claim");
        assert!(hydrator.is_none(), "the hydrator is discarded");

        let mut saw_usage = false;
        let mut saw_foreign_message = false;
        while let Ok(obs) = rx.try_recv() {
            match &obs.body {
                ObservationPayload::Usage(_) => saw_usage = true,
                ObservationPayload::Message(message) => {
                    let rendered = serde_json::to_string(message).unwrap_or_default();
                    if rendered.contains("EVIL_FOREIGN_CONTENT") || rendered.contains("msg-evil") {
                        saw_foreign_message = true;
                    }
                }
                _ => {}
            }
        }
        assert!(
            !saw_foreign_message,
            "foreign content must never be journaled"
        );
        assert!(!saw_usage, "foreign usage must never reach usage_events");
    }

    // ----- deterministic binding, per epoch --------------------------------

    #[test]
    fn the_pid_file_binds_the_foreground_session_and_a_second_poll_does_not_switch() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let bindings = BindingHandle::empty();
        bindings.begin_epoch(&cwd, tmp.path());

        // The correct, older session belongs to the foreground pid; a busier
        // session from another pid keeps getting written and is newest-mtime.
        slug_session(tmp.path(), &cwd, CORRECT, "{}\n");
        slug_session(tmp.path(), &cwd, BUSY_OTHER, "{}\n");
        write(
            &tmp.path().join("sessions").join("4242.json"),
            &format!(
                r#"{{"pid":4242,"sessionId":"{CORRECT}","cwd":{}}}"#,
                serde_json::json!(cwd.to_string_lossy())
            ),
        );
        std::thread::sleep(Duration::from_millis(20));
        let mut busy = std::fs::OpenOptions::new()
            .append(true)
            .open(
                crate::claude_transcript::project_dir(tmp.path(), &cwd)
                    .join(format!("{BUSY_OTHER}.jsonl")),
            )
            .expect("open");
        writeln!(busy, "{{}}").expect("write");

        let found = detected_claude(4242, None);
        let first = bindings.resolve(&found).expect("first poll binds");
        assert_eq!(first.session_id, CORRECT);
        assert_eq!(
            first.source,
            crate::claude_transcript::BindingSource::PidFile
        );

        // Another session is created and becomes the newest file afterwards;
        // the epoch must stay on its locked claim.
        std::thread::sleep(Duration::from_millis(20));
        slug_session(tmp.path(), &cwd, LATE_STARTER, "{}\n");
        let second = bindings.resolve(&found).expect("second poll still bound");
        assert_eq!(second.session_id, CORRECT, "must never switch files");
    }

    #[test]
    fn authenticated_renderer_relaunch_can_replace_session_at_the_same_pid() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let bindings = BindingHandle::empty();
        bindings.begin_epoch(&cwd, tmp.path());
        slug_session(tmp.path(), &cwd, CORRECT, "{}\n");
        slug_session(tmp.path(), &cwd, LATE_STARTER, "{}\n");
        let found = detected_claude(4242, Some(CORRECT));
        assert_eq!(bindings.resolve(&found).unwrap().session_id, CORRECT);
        let mut report = SessionStartReport {
            start_source: None,
            session_id: LATE_STARTER.into(),
            transcript_path: crate::claude_transcript::project_dir(tmp.path(), &cwd)
                .join(format!("{LATE_STARTER}.jsonl")),
            cwd: None,
            ppid: Some(7777),
        };
        assert!(!bindings.rebind_authenticated_start(&found, &report));
        assert_eq!(bindings.binding().unwrap().session_id, CORRECT);
        report.ppid = Some(4242);
        assert!(bindings.rebind_authenticated_start(&found, &report));
        assert_eq!(bindings.resolve(&found).unwrap().session_id, LATE_STARTER);
        assert!(!bindings.rebind_authenticated_start(&found, &report));
        report.session_id = BUSY_OTHER.into();
        report.transcript_path = tmp.path().join("other.jsonl");
        std::fs::write(&report.transcript_path, "{}\n").unwrap();
        assert!(!bindings.rebind_authenticated_start(&found, &report));
        assert_eq!(bindings.binding().unwrap().session_id, LATE_STARTER);
    }

    #[test]
    fn an_authenticated_report_binds_the_first_claim_over_a_stale_argv_session() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let bindings = BindingHandle::empty();
        bindings.begin_epoch(&cwd, tmp.path());
        // The launch argv still names S1 and S1's file now exists, but the
        // authenticated report says the live session is LATE_STARTER.
        slug_session(tmp.path(), &cwd, CORRECT, "{}\n");
        slug_session(tmp.path(), &cwd, LATE_STARTER, "{}\n");
        let found = detected_claude(7, Some(CORRECT));
        let report = SessionStartReport {
            start_source: None,
            session_id: LATE_STARTER.into(),
            transcript_path: crate::claude_transcript::project_dir(tmp.path(), &cwd)
                .join(format!("{LATE_STARTER}.jsonl")),
            cwd: None,
            ppid: Some(7),
        };
        // No binding yet: the report takes the first claim.
        assert!(bindings.bind_authenticated_report(&found, &report));
        assert_eq!(bindings.binding().unwrap().session_id, LATE_STARTER);
        // Once bound it never overrides an existing claim.
        assert!(!bindings.bind_authenticated_report(&found, &report));
        // A wrong-pid report does not bind an empty epoch.
        bindings.demobilize();
        let foreign = detected_claude(8, Some(CORRECT));
        assert!(!bindings.bind_authenticated_report(&foreign, &report));
        assert!(bindings.binding().is_none());
        // A transcript outside the promoted cwd is refused.
        let other_cwd = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&other_cwd).unwrap();
        let stray = other_cwd.join("stray.jsonl");
        std::fs::write(&stray, "{}\n").unwrap();
        let stray_report = SessionStartReport {
            start_source: None,
            session_id: "stray-session".into(),
            transcript_path: stray,
            cwd: None,
            ppid: Some(7),
        };
        assert!(!bindings.bind_authenticated_report(&found, &stray_report));
        assert!(bindings.binding().is_none());
    }

    #[test]
    fn an_explicit_resume_id_wins_over_every_other_channel() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let bindings = BindingHandle::empty();
        bindings.begin_epoch(&cwd, tmp.path());
        slug_session(tmp.path(), &cwd, CORRECT, "{}\n");
        // Even with a pid file naming a different session, argv wins.
        slug_session(tmp.path(), &cwd, BUSY_OTHER, "{}\n");
        write(
            &tmp.path().join("sessions").join("7.json"),
            &format!(
                r#"{{"pid":7,"sessionId":"{BUSY_OTHER}","cwd":{}}}"#,
                serde_json::json!(cwd.to_string_lossy())
            ),
        );
        let found = detected_claude(7, Some(CORRECT));
        assert_eq!(bindings.resolve(&found).expect("bound").session_id, CORRECT);
    }

    #[test]
    fn a_missing_argv_file_falls_through_to_the_pid_channel() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let bindings = BindingHandle::empty();
        bindings.begin_epoch(&cwd, tmp.path());
        slug_session(tmp.path(), &cwd, CORRECT, "{}\n");
        write(
            &tmp.path().join("sessions").join("9.json"),
            &format!(
                r#"{{"pid":9,"sessionId":"{CORRECT}","cwd":{}}}"#,
                serde_json::json!(cwd.to_string_lossy())
            ),
        );
        // --resume names a session whose file does not exist yet; pid file
        // still proves the foreground session instead of going unbound.
        let found = detected_claude(9, Some("00000000-0000-4000-8000-000000000000"));
        assert_eq!(bindings.resolve(&found).expect("bound").session_id, CORRECT);
    }

    #[test]
    fn a_hook_binds_only_its_own_foreground_pid_and_beats_the_pid_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let hook_path = slug_session(tmp.path(), &cwd, CORRECT, "{}\n");
        // A pid file for the same pid names a stale id; the fresh hook wins.
        write(
            &tmp.path().join("sessions").join("5150.json"),
            &format!(
                r#"{{"pid":5150,"sessionId":"{BUSY_OTHER}","cwd":{}}}"#,
                serde_json::json!(cwd.to_string_lossy())
            ),
        );
        let bindings = BindingHandle::empty();
        bindings.begin_epoch(&cwd, tmp.path());
        let report = SessionStartReport::from_stdin(&format!(
            r#"{{"hook_event_name":"SessionStart","session_id":"{CORRECT}","transcript_path":{},"cwd":{},"ppid":5150}}"#,
            serde_json::json!(hook_path.to_string_lossy()),
            serde_json::json!(cwd.to_string_lossy()),
        ))
        .expect("parse hook");
        bindings.ingest_session_start(report);
        // A hook from a different session in the same cwd is ignored.
        bindings.ingest_session_start(
            SessionStartReport::from_stdin(&format!(
                r#"{{"hook_event_name":"SessionStart","session_id":"{BUSY_OTHER}","transcript_path":"/tmp/x.jsonl","ppid":9999}}"#
            ))
            .expect("parse"),
        );
        let found = detected_claude(5150, None);
        let binding = bindings.resolve(&found).expect("hook binding");
        assert_eq!(binding.session_id, CORRECT);
        assert_eq!(
            binding.source,
            crate::claude_transcript::BindingSource::Hook
        );
    }

    #[test]
    fn a_late_appearing_transcript_binds_on_a_later_poll() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let bindings = BindingHandle::empty();
        bindings.begin_epoch(&cwd, tmp.path());
        write(
            &tmp.path().join("sessions").join("11.json"),
            &format!(
                r#"{{"pid":11,"sessionId":"{CORRECT}","cwd":{}}}"#,
                serde_json::json!(cwd.to_string_lossy())
            ),
        );
        let found = detected_claude(11, None);
        // The transcript jsonl has not appeared yet: stay unbound, no guess.
        assert!(bindings.resolve(&found).is_none());
        assert!(!bindings.degraded());
        // The session's file lands shortly after startup; next poll binds.
        slug_session(tmp.path(), &cwd, CORRECT, "{}\n");
        assert_eq!(
            bindings.resolve(&found).expect("binds late").session_id,
            CORRECT
        );
    }

    /// c-effortread r8 item 2: the native hook payload's `source` is parsed
    /// and exposed; a `/clear` report drives a Fresh classification even when
    /// the transcript is non-empty at binding time.
    #[test]
    fn r8_item2_session_start_report_parses_source_and_survives_rebind_clear() {
        let parsed = SessionStartReport::from_stdin(
            r#"{"hook_event_name":"SessionStart","session_id":"clear-z","transcript_path":"/tmp/z.jsonl","cwd":"/repo","ppid":4242,"source":"clear"}"#,
        )
        .expect("parses");
        assert_eq!(parsed.start_source.as_deref(), Some("clear"));
        assert!(parsed.source_is_fresh_start());
        assert!(!parsed.source_is_resume());

        let resume = SessionStartReport::from_stdin(
            r#"{"hook_event_name":"SessionStart","session_id":"old","transcript_path":"/tmp/o.jsonl","ppid":4242,"source":"resume"}"#,
        )
        .expect("parses");
        assert!(!resume.source_is_fresh_start());
        assert!(resume.source_is_resume());

        // The source registry survives a rebind's hooks.clear(): bind an
        // existing "old" session, then drive an authenticated /clear rebind to
        // a non-empty Z transcript (rebind clears the hooks vector); the
        // /clear source must still be available, and the sticky-mode table
        // records Fresh so the gate opens for Z.
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let old_path = slug_session(tmp.path(), &cwd, "old", "{}\n");
        // Z.jsonl ALREADY holds the queued prompt before the poll tick.
        let z: std::path::PathBuf = slug_session(
            tmp.path(),
            &cwd,
            "clear-z",
            "{\"type\":\"user\",\"message\":{\"content\":\"queued prompt\"}}\n",
        );
        let bindings = BindingHandle::empty();
        bindings.begin_epoch(&cwd, tmp.path());
        let mut table = SessionModeTable::default();
        let found = detected_claude(4242, Some("old"));
        let old_report = SessionStartReport {
            session_id: "old".into(),
            transcript_path: old_path,
            cwd: Some(cwd.clone()),
            ppid: Some(4242),
            start_source: Some("resume".into()),
        };
        assert!(bindings.bind_authenticated_report(&found, &old_report));
        let clear_report = SessionStartReport {
            session_id: "clear-z".into(),
            transcript_path: z,
            cwd: Some(cwd.clone()),
            ppid: Some(4242),
            start_source: Some("clear".into()),
        };
        // The /clear rebind flips the slot and clears the hooks vector.
        assert!(bindings.rebind_authenticated_start(&found, &clear_report));
        bindings.ingest_session_start(clear_report);
        assert_eq!(
            bindings.start_source_for(4242, "clear-z").as_deref(),
            Some("clear"),
            "the /clear source survives the rebind hooks.clear()"
        );
        let m = table.record(
            4242,
            "clear-z",
            bindings.binding().as_ref().map(|b| b.path.as_path()),
            None,
            bindings.start_source_for(4242, "clear-z").as_deref(),
            &found,
        );
        assert_eq!(
            m.mode,
            ResumeMode::Fresh,
            "queued prompt in Z.jsonl -> Fresh, gate opens"
        );
    }

    #[test]
    fn with_no_channel_the_epoch_stays_unbound_and_offers_a_manual_picker() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        slug_session(tmp.path(), &cwd, CORRECT, "{}\n");
        slug_session(tmp.path(), &cwd, BUSY_OTHER, "{}\n");
        let bindings = BindingHandle::empty();
        let ctx = ctx_in(&cwd);
        bindings.begin_epoch(&cwd, tmp.path());
        let found = detected_claude(7777, None);
        assert!(
            bindings.resolve(&found).is_none(),
            "no pid file, no argv, no hook"
        );

        let interaction = bindings
            .offer_picker(&ctx)
            .expect("a picker question is offered");
        let InteractionRequest::Question(question) = &interaction.request else {
            panic!("expected a question");
        };
        let field = &question.fields[0];
        assert_eq!(field.input, QuestionInput::SingleSelect);
        assert_eq!(field.options.len(), 2, "both sessions are offered");
        assert!(!field.allow_free_text, "free text would be a guess");
        // It is emitted once; asking again before a rescan yields nothing.
        assert!(bindings.offer_picker(&ctx).is_none());
    }

    #[test]
    fn a_manual_answer_binds_exactly_and_an_unknown_option_is_rejected() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        slug_session(tmp.path(), &cwd, CORRECT, "{}\n");
        slug_session(tmp.path(), &cwd, BUSY_OTHER, "{}\n");
        let bindings = BindingHandle::empty();
        let ctx = ctx_in(&cwd);
        bindings.begin_epoch(&cwd, tmp.path());
        let found = detected_claude(7777, None);
        assert!(bindings.resolve(&found).is_none());
        let interaction = bindings.offer_picker(&ctx).expect("picker");
        let id = interaction.meta.id.clone();

        let bogus = QuestionAnswer {
            answers: BTreeMap::from([(
                PICKER_FIELD.into(),
                QuestionFieldAnswer {
                    option_ids: vec!["deadbeef-0000-4000-8000-000000000000".into()],
                    text: None,
                },
            )]),
        };
        assert!(
            bindings
                .answer(&id, &InteractionAnswer::Question(Box::new(bogus)))
                .is_err()
        );
        assert!(
            bindings.binding().is_none(),
            "a rejected answer binds nothing"
        );

        let answer = QuestionAnswer {
            answers: BTreeMap::from([(
                PICKER_FIELD.into(),
                QuestionFieldAnswer {
                    option_ids: vec![BUSY_OTHER.into()],
                    text: None,
                },
            )]),
        };
        bindings
            .answer(&id, &InteractionAnswer::Question(Box::new(answer)))
            .expect("the human's explicit choice binds");
        let binding = bindings.binding().expect("bound");
        assert_eq!(
            binding.session_id, BUSY_OTHER,
            "even the older file is honored"
        );
        assert_eq!(
            binding.source,
            crate::claude_transcript::BindingSource::Manual
        );
        // First-answer-wins: a second answer is refused.
        let again = QuestionAnswer {
            answers: BTreeMap::from([(
                PICKER_FIELD.into(),
                QuestionFieldAnswer {
                    option_ids: vec![CORRECT.into()],
                    text: None,
                },
            )]),
        };
        assert!(
            bindings
                .answer(&id, &InteractionAnswer::Question(Box::new(again)))
                .is_err()
        );
    }

    #[test]
    fn a_vanished_file_degrades_and_is_never_rebound_within_the_epoch() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let path = slug_session(tmp.path(), &cwd, CORRECT, "{}\n");
        let bindings = BindingHandle::empty();
        bindings.begin_epoch(&cwd, tmp.path());
        write(
            &tmp.path().join("sessions").join("5.json"),
            &format!(
                r#"{{"pid":5,"sessionId":"{CORRECT}","cwd":{}}}"#,
                serde_json::json!(cwd.to_string_lossy())
            ),
        );
        let found = detected_claude(5, None);
        assert_eq!(bindings.resolve(&found).expect("bound").session_id, CORRECT);
        std::fs::remove_file(&path).expect("vanish");
        bindings.mark_degraded("bound transcript file vanished");
        assert!(bindings.degraded());
        // resolve keeps reporting the locked claim rather than hunting a new one.
        assert_eq!(
            bindings.resolve(&found).expect("claim kept").session_id,
            CORRECT
        );
    }

    #[test]
    fn a_new_epoch_clears_the_locked_claim_and_resets_the_picker() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        slug_session(tmp.path(), &cwd, CORRECT, "{}\n");
        let bindings = BindingHandle::empty();
        let ctx = ctx_in(&cwd);
        bindings.begin_epoch(&cwd, tmp.path());
        let found = detected_claude(1, Some(CORRECT));
        assert!(bindings.resolve(&found).is_some());
        bindings.offer_picker(&ctx); // would be suppressed while bound
        bindings.begin_epoch(&cwd, tmp.path());
        assert!(bindings.binding().is_none());
        assert!(!bindings.degraded());
    }
    /// c-ctxusage r4 item 2, carrier level: the real Hydrator (bound tail +
    /// mapper + emit path) must keep an assistant run with usage but NO
    /// stop_reason unpublished across ordinary poll flushes, then publish it
    /// on `finalize` — the exact path exit / close / demote now take. A plain
    /// `flush()` used to be the only call, so the last turn's usage never
    /// reached the Hub and the chip stayed "—".
    #[tokio::test]
    async fn hydrator_finalise_publishes_usage_without_stop_reason_on_close() {
        use crate::claude_transcript::{BindingSource, TranscriptBinding};
        use remuda_protocol::Knowledge;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cwd = dir.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let session = "dddddddd-aaaa-4bbb-8ccc-dddddddddddd";

        // One assistant record: real counters, stop_reason deliberately null
        // (a process killed right after the final block, before Claude writes
        // the terminal record).
        let body = serde_json::json!({
            "type": "assistant",
            "uuid": "uuid-final",
            "timestamp": "2026-10-08T00:00:00.000Z",
            "isSidechain": false,
            "message": {
                "type": "message",
                "role": "assistant",
                "id": "msg_last_turn",
                "model": "claude-opus-5-5",
                "content": [{"type": "text", "text": "DONE"}],
                "stop_reason": null,
                "usage": {
                    "input_tokens": 3,
                    "cache_read_input_tokens": 4000,
                    "cache_creation_input_tokens": 7,
                    "output_tokens": 11,
                    "cache_creation": {
                        "ephemeral_5m_input_tokens": 7,
                        "ephemeral_1h_input_tokens": 0
                    }
                }
            }
        });
        let path = slug_session(&home, &cwd, session, &format!("{}\n", body));

        let binding = TranscriptBinding {
            session_id: session.into(),
            path: path.clone(),
            cwd: cwd.clone(),
            source: BindingSource::Hook,
        };
        let ctx = ctx_in(&cwd);
        let mut hydrator = Hydrator::open(
            &ctx,
            &binding,
            None,
            None,
            None,
            None,
            None,
            crate::claude_transcript::ResumeMode::Fresh,
            None,
            None,
        )
        .expect("hydrator binds the seeded transcript");

        let (tx, mut rx) = mpsc::channel::<Observation>(64);

        // Ordinary poll cycle: content is journaled, but usage with no
        // stop_reason stays with the run.
        pump(
            &mut hydrator,
            &tx,
            &std::sync::atomic::AtomicU64::new(0),
            &ctx,
        )
        .await
        .expect("pump");
        let mut saw_usage_after_poll = false;
        while let Ok(observation) = rx.try_recv() {
            if matches!(observation.body, ObservationPayload::Usage(_)) {
                saw_usage_after_poll = true;
            }
        }
        assert!(
            !saw_usage_after_poll,
            "a poll flush must not publish usage for a run without stop_reason"
        );

        // Close/exit/demotion all route through Hydrator::finalize now.
        hydrator
            .finalize(&tx, &std::sync::atomic::AtomicU64::new(0), &ctx)
            .await;
        drop(tx);

        let mut finalised = Vec::new();
        while let Some(observation) = rx.recv().await {
            if let ObservationPayload::Usage(payload) = observation.body {
                finalised.push(payload);
            }
        }
        assert_eq!(finalised.len(), 1, "the last turn finalises exactly once");
        let payload = &finalised[0];
        let known = |value: &Knowledge<remuda_protocol::U64>| match value {
            Knowledge::Known { value } => value.0,
            other => panic!("expected Known counter, got {other:?}"),
        };
        assert_eq!(known(&payload.input_tokens), 3);
        assert_eq!(known(&payload.cache_read_tokens), 4_000);
        assert_eq!(known(&payload.cache_write_tokens), 7);
        assert_eq!(known(&payload.output_tokens), 11);
    }

    /// c-usagefu r17 item 3: a FAILED final transcript read must not suppress
    /// the buffered usage. The open turn's counters live in the mapper, not on
    /// disk: when the bound file vanishes between the last poll and the close
    /// finalise (a Fresh tail returns ENOENT as an error), the successfully
    /// read lines are mapped (none here) and mapper.finish() still runs and
    /// emits the held usage. The old `if let Ok(read)` skipped finish() too.
    #[tokio::test]
    async fn hydrator_finalise_emits_buffered_usage_when_the_final_read_fails() {
        use crate::claude_transcript::{BindingSource, TranscriptBinding};
        use remuda_protocol::Knowledge;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cwd = dir.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let session = "eeeeeeee-aaaa-4bbb-8ccc-eeeeeeeeeeee";

        // Same open-turn shape: real counters, stop_reason null.
        let body = serde_json::json!({
            "type": "assistant",
            "uuid": "uuid-final",
            "timestamp": "2026-10-08T00:00:00.000Z",
            "isSidechain": false,
            "message": {
                "type": "message",
                "role": "assistant",
                "id": "msg_vanished_turn",
                "model": "claude-opus-5-5",
                "content": [{"type": "text", "text": "DONE"}],
                "stop_reason": null,
                "usage": {
                    "input_tokens": 5,
                    "cache_read_input_tokens": 6000,
                    "cache_creation_input_tokens": 9,
                    "output_tokens": 13,
                    "cache_creation": {
                        "ephemeral_5m_input_tokens": 9,
                        "ephemeral_1h_input_tokens": 0
                    }
                }
            }
        });
        let path = slug_session(&home, &cwd, session, &format!("{}\n", body));

        let binding = TranscriptBinding {
            session_id: session.into(),
            path: path.clone(),
            cwd: cwd.clone(),
            source: BindingSource::Hook,
        };
        let ctx = ctx_in(&cwd);
        let mut hydrator = Hydrator::open(
            &ctx,
            &binding,
            None,
            None,
            None,
            None,
            None,
            crate::claude_transcript::ResumeMode::Fresh,
            None,
            None,
        )
        .expect("hydrator binds the seeded transcript");

        let (tx, mut rx) = mpsc::channel::<Observation>(64);
        // One ordinary pump maps the record; the open usage stays buffered.
        pump(
            &mut hydrator,
            &tx,
            &std::sync::atomic::AtomicU64::new(0),
            &ctx,
        )
        .await
        .expect("pump");
        while rx.try_recv().is_ok() {}

        // The transcript VANISHES before the close finalise (rotation/cleanup):
        // a Fresh tail's final poll returns the ENOENT error.
        std::fs::remove_file(&path).expect("the bound transcript vanishes");
        assert!(
            hydrator.tail.poll().is_err(),
            "the test genuinely drives the failed-final-read path"
        );

        hydrator
            .finalize(&tx, &std::sync::atomic::AtomicU64::new(0), &ctx)
            .await;
        drop(tx);

        let mut finalised = Vec::new();
        while let Some(observation) = rx.recv().await {
            if let ObservationPayload::Usage(payload) = observation.body {
                finalised.push(payload);
            }
        }
        assert_eq!(
            finalised.len(),
            1,
            "the buffered usage still finalises exactly once after a failed final read"
        );
        let known = |value: &Knowledge<remuda_protocol::U64>| match value {
            Knowledge::Known { value } => value.0,
            other => panic!("expected Known counter, got {other:?}"),
        };
        assert_eq!(known(&finalised[0].input_tokens), 5);
        assert_eq!(known(&finalised[0].cache_read_tokens), 6_000);
        assert_eq!(known(&finalised[0].cache_write_tokens), 9);
        assert_eq!(known(&finalised[0].output_tokens), 13);
    }

    // =======================================================================
    // c-effortread r4
    //

    // ----- item 4: sticky tri-state launch mode ----------------------------

    fn boundary_mode() -> ResumeMode {
        // A distinct value is all the helper needs; the boundary itself is not
        // consulted by epoch_mode.
        ResumeMode::Unverified
    }

    /// A sticky entry for the launched process's OWN first session.
    fn own_session(mode: ResumeMode) -> StickySessionMode {
        StickySessionMode {
            mode,
            rebound: false,
        }
    }

    /// A sticky entry for a later in-TUI session (`/resume` / `/clear`).
    fn rebound_session(mode: ResumeMode) -> StickySessionMode {
        StickySessionMode {
            mode,
            rebound: true,
        }
    }

    #[test]
    fn launch_mode_applies_once_and_refines_a_later_known_session() {
        let mut binding = LaunchModeBinding::Unbound;
        // First sighting with the launched session's own SessionStart mode.
        let mode = epoch_mode(
            &mut binding,
            Some(boundary_mode()),
            Some(own_session(boundary_mode())),
            &detected_claude(7, None),
        );
        assert_eq!(mode, boundary_mode());
        assert_eq!(binding, LaunchModeBinding::Bound(7, String::new()));
        // The authenticated SessionStart reports the id for the SAME pid:
        // refine the slot, keep the pre-spawn mode.
        let mode = epoch_mode(
            &mut binding,
            Some(boundary_mode()),
            Some(own_session(boundary_mode())),
            &detected_claude(7, Some(CORRECT)),
        );
        assert_eq!(mode, boundary_mode());
        assert_eq!(binding, LaunchModeBinding::Bound(7, CORRECT.to_owned()));
        // Steady state keeps applying it.
        let mode = epoch_mode(
            &mut binding,
            Some(boundary_mode()),
            Some(own_session(boundary_mode())),
            &detected_claude(7, Some(CORRECT)),
        );
        assert_eq!(mode, boundary_mode());
    }

    #[test]
    fn an_authenticated_rebind_binds_the_new_session_to_its_own_mode_stickily() {
        // r5/r6 items 2/3: a later sticky session is bounded by the mode
        // measured at ITS OWN SessionStart, never by the launch argv. A fresh
        // `/clear` rebound is Fresh; a `/resume` rebound is its own
        // unverified-EOF mode — and it sticks on later ticks (as Dead).
        let mut binding = LaunchModeBinding::Bound(7, CORRECT.to_owned());
        let mode = epoch_mode(
            &mut binding,
            Some(boundary_mode()),
            Some(rebound_session(ResumeMode::Fresh)),
            &detected_claude(7, Some(LATE_STARTER)),
        );
        assert_eq!(mode, ResumeMode::Fresh);
        assert_eq!(binding, LaunchModeBinding::Dead(ResumeMode::Fresh));
        // Sticks on later ticks: the same rebound entry plus an argv that says
        // resume cannot change it.
        let mut found = detected_claude(7, Some(LATE_STARTER));
        found.resume = true;
        let mode = epoch_mode(
            &mut binding,
            Some(boundary_mode()),
            Some(rebound_session(ResumeMode::Fresh)),
            &found,
        );
        assert_eq!(mode, ResumeMode::Fresh, "the rebound mode is sticky");
        // A second rebind replaces it with that session's own mode.
        let mode = epoch_mode(
            &mut binding,
            Some(boundary_mode()),
            Some(rebound_session(ResumeMode::Unverified)),
            &found,
        );
        assert_eq!(mode, ResumeMode::Unverified);
        assert_eq!(binding, LaunchModeBinding::Dead(ResumeMode::Unverified));
    }

    #[test]
    fn a_rebind_before_any_binding_never_falls_back_to_the_launch_argv() {
        // r6 item 3 (the lazy-create case): a fresh launch (resume None) has
        // no transcript and no binding when the user /resume's an older
        // session; the older session's sticky rebound mode (Unverified at
        // EOF) is applied on that FIRST bind instead of byte-0 Fresh.
        let mut binding = LaunchModeBinding::Unbound;
        let mode = epoch_mode(
            &mut binding,
            None,
            Some(rebound_session(ResumeMode::Unverified)),
            &detected_claude(7, Some(LATE_STARTER)),
        );
        assert_eq!(mode, ResumeMode::Unverified);
        assert_eq!(binding, LaunchModeBinding::Dead(ResumeMode::Unverified));
    }

    #[test]
    fn a_pid_change_or_a_session_id_flip_uses_detected_provenance_and_holds_it() {
        let mut binding = LaunchModeBinding::Bound(7, CORRECT.to_owned());
        let mode = epoch_mode(
            &mut binding,
            Some(boundary_mode()),
            None,
            &detected_claude(8, Some(CORRECT)),
        );
        assert_eq!(mode, ResumeMode::Fresh, "a new pid is a new process");
        assert_eq!(binding, LaunchModeBinding::Dead(ResumeMode::Fresh));
        // A known session id silently changing to another known id at the same
        // pid (without an authenticated rebind) is also not the launch.
        let mut binding = LaunchModeBinding::Bound(7, CORRECT.to_owned());
        let mode = epoch_mode(
            &mut binding,
            Some(boundary_mode()),
            None,
            &detected_claude(7, Some(BUSY_OTHER)),
        );
        assert_eq!(mode, ResumeMode::Fresh);
        assert_eq!(binding, LaunchModeBinding::Dead(ResumeMode::Fresh));
    }

    #[test]
    fn a_login_shell_uses_detected_provenance_or_the_sticky_mode_without_binding() {
        let mut binding = LaunchModeBinding::Unbound;
        let mut found = detected_claude(7, Some(CORRECT));
        found.resume = true;
        // No hook report yet: argv provenance.
        assert_eq!(
            epoch_mode(&mut binding, None, None, &found),
            ResumeMode::Unverified
        );
        assert_eq!(binding, LaunchModeBinding::Unbound);
        found.resume = false;
        assert_eq!(
            epoch_mode(&mut binding, None, None, &found),
            ResumeMode::Fresh
        );
        // The launched session's own first report binds without Dead.
        let mut binding = LaunchModeBinding::Unbound;
        assert_eq!(
            epoch_mode(
                &mut binding,
                None,
                Some(own_session(ResumeMode::Fresh)),
                &found
            ),
            ResumeMode::Fresh
        );
        assert_eq!(binding, LaunchModeBinding::Bound(7, CORRECT.to_owned()));
        // A later login-shell rebind is bounded from the rebound transcript.
        assert_eq!(
            epoch_mode(
                &mut binding,
                None,
                Some(rebound_session(ResumeMode::Unverified)),
                &found
            ),
            ResumeMode::Unverified
        );
        assert_eq!(binding, LaunchModeBinding::Dead(ResumeMode::Unverified));
    }
    // ----- items 1/2/5/6/7: the production promotion pump ------------------

    const PUMP_SESSION: &str = "dddddddd-2222-4333-8444-eeeeeeeeeeee";
    const DECOUPLED: &str = "2.1.289";

    struct PumpFixture {
        ctx: PromoteCtx,
        binding: TranscriptBinding,
        seq: Arc<AtomicU64>,
        tx: mpsc::Sender<Observation>,
        rx: mpsc::Receiver<Observation>,
    }

    fn pump_fixture(tmp: &Path, initial: &str) -> PumpFixture {
        let cwd = tmp.join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let path = slug_session(tmp, &cwd, PUMP_SESSION, initial);
        let binding = TranscriptBinding {
            session_id: PUMP_SESSION.to_owned(),
            path,
            cwd,
            source: crate::claude_transcript::BindingSource::PidFile,
        };
        let (tx, rx) = mpsc::channel(256);
        PumpFixture {
            ctx: ctx_in(tmp),
            binding,
            seq: Arc::new(AtomicU64::new(0)),
            tx,
            rx,
        }
    }

    fn open_hydrator(
        fx: &PumpFixture,
        mode: ResumeMode,
        bridge: Option<&Arc<crate::effort::EffortBridge>>,
    ) -> Hydrator {
        Hydrator::open(
            &fx.ctx,
            &fx.binding,
            bridge,
            None,
            None,
            None,
            None,
            mode,
            None,
            None,
        )
        .expect("hydrator opens against the bound transcript")
    }

    async fn pump_once(hydrator: &mut Hydrator, fx: &PumpFixture) {
        pump(hydrator, &fx.tx, &fx.seq, &fx.ctx)
            .await
            .expect("the production promotion pump");
    }

    /// c-effortread r9 item 9d: the per-tick state the production promotion
    /// loop owns, so tests can run maintain_binding (the real bind/open/pump
    /// entry point) across demote/re-promotion instead of calling
    /// epoch_mode/open_hydrator directly.
    struct MaintainState {
        bindings: BindingHandle,
        hydrator: Option<Hydrator>,
        announced: Option<String>,
        session_modes: SessionModeTable,
        launch_mode: LaunchModeBinding,
        read_anchors: std::collections::HashMap<(i32, String), TailAnchor>,
        effort_seeds: std::collections::HashMap<(i32, String), EffortTrackerSeed>,
        withdrawn_sessions: std::collections::HashSet<(i32, String)>,
        active_session: Option<(i32, String)>,
        found: Detected,
    }

    /// Build MaintainState bound to the fixture's (pid 7, PUMP_SESSION) with
    /// the hook source and argv the production loop would see. `resume` picks
    /// the `--resume` argv (and a "resume" source) vs a fresh startup.
    fn maintain_state(fx: &PumpFixture, resume: bool) -> MaintainState {
        let args = if resume {
            format!("claude --resume {PUMP_SESSION}")
        } else {
            format!("claude --session-id {PUMP_SESSION}")
        };
        let found =
            crate::promote::detect(std::slice::from_ref(&ProcessRow { pid: 7, args }), None)
                .expect("detects as claude");
        let bindings = BindingHandle::empty();
        bindings.begin_epoch(&fx.ctx.cwd, &fx.ctx.claude_home);
        bindings.ingest_session_start(SessionStartReport {
            session_id: PUMP_SESSION.to_owned(),
            transcript_path: fx.binding.path.clone(),
            cwd: Some(fx.binding.cwd.clone()),
            ppid: Some(7),
            start_source: Some(if resume {
                "resume".into()
            } else {
                "startup".into()
            }),
        });
        MaintainState {
            bindings,
            hydrator: None,
            announced: None,
            session_modes: SessionModeTable::default(),
            launch_mode: LaunchModeBinding::Unbound,
            read_anchors: std::collections::HashMap::new(),
            effort_seeds: std::collections::HashMap::new(),
            withdrawn_sessions: std::collections::HashSet::new(),
            active_session: None,
            found,
        }
    }

    /// Run one production maintain_binding tick against `state`.
    async fn maintain_tick(
        state: &mut MaintainState,
        fx: &PumpFixture,
        pre_resume_mode: Option<ResumeMode>,
    ) {
        maintain_binding(
            &state.bindings,
            &fx.ctx,
            &state.found,
            &mut state.hydrator,
            &mut state.announced,
            &fx.tx,
            &fx.seq,
            None,
            None,
            None,
            None,
            None,
            &mut state.session_modes,
            &mut state.launch_mode,
            pre_resume_mode,
            &mut state.read_anchors,
            &mut state.effort_seeds,
            &mut state.withdrawn_sessions,
            &mut state.active_session,
        )
        .await;
    }

    fn drain(fx: &mut PumpFixture) -> Vec<Observation> {
        let mut out = Vec::new();
        while let Ok(observation) = fx.rx.try_recv() {
            out.push(observation);
        }
        out
    }

    /// `(level, readback_available)` for every emitted effort observation.
    /// Level is None on the read-back-unavailable edge.
    fn effort_rows(
        observations: &[Observation],
    ) -> Vec<(Option<remuda_protocol::EffortName>, Option<bool>)> {
        observations
            .iter()
            .filter_map(|observation| match &observation.body {
                ObservationPayload::Effort(payload) => {
                    let row: (Option<remuda_protocol::EffortName>, Option<bool>) =
                        (payload.effective.name, payload.effective.ultracode);
                    Some(row)
                }
                _ => None,
            })
            .collect()
    }

    fn conversation_hydrated(observations: &[Observation], needle: &str) -> bool {
        observations
            .iter()
            .filter(|observation| {
                matches!(
                    observation.body,
                    ObservationPayload::Message(_)
                        | ObservationPayload::Thought(_)
                        | ObservationPayload::ToolCall(_)
                        | ObservationPayload::ToolResult(_)
                )
            })
            .any(|observation| {
                serde_json::to_string(&observation.body).is_ok_and(|body| body.contains(needle))
            })
    }

    fn append_line(path: &Path, line: &str) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open append");
        file.write_all(line.as_bytes()).expect("append");
    }

    fn assistant_line(effort: Option<&str>, n: u64) -> String {
        let mut record = serde_json::json!({
            "type": "assistant",
            "uuid": format!("msg-{n}"),
            "sessionId": PUMP_SESSION,
            "version": DECOUPLED,
            "timestamp": "2026-10-07T10:00:00.000Z",
            "message": {
                "id": format!("msg-{n}"),
                "role": "assistant",
                "type": "message",
                "content": [{"type": "text", "text": format!("reply {n}")}],
                "stop_reason": "end_turn",
            },
        });
        if let Some(level) = effort {
            record
                .as_object_mut()
                .expect("object")
                .insert("perTurnEffort".into(), serde_json::json!(level));
        }
        let mut line = record.to_string();
        line.push('\n');
        line
    }

    fn user_line(n: u64, text: &str) -> String {
        let mut line = serde_json::json!({
            "type": "user",
            "uuid": format!("prompt-{n}"),
            "sessionId": PUMP_SESSION,
            "version": DECOUPLED,
            "message": {"role": "user", "content": text},
        })
        .to_string();
        line.push('\n');
        line
    }

    #[allow(dead_code)] // used by the item-5 test added later
    fn slash_line(args: &str, n: u64) -> String {
        let mut line = serde_json::json!({
            "type": "user",
            "uuid": format!("cmd-{n}"),
            "sessionId": PUMP_SESSION,
            "version": DECOUPLED,
            "message": {
                "role": "user",
                "content": format!(
                    "<command-name>/effort</command-name>\n<command-message>effort</command-message>\n\
                     <command-args>{args}</command-args>"
                ),
            },
        })
        .to_string();
        line.push('\n');
        line
    }

    #[allow(dead_code)] // used by the item-5 test added later
    fn stdout_verdict(stdout: &str, n: u64) -> String {
        user_line(
            n,
            &format!("<local-command-stdout>{stdout}</local-command-stdout>"),
        )
    }

    /// c-effortread r11 item 7: fold one SessionStart through the REAL native
    /// bus and return the folded binding — exactly the payload the promotion
    /// loop feeds to `apply_native_session_binding`. Tests never hand-build a
    /// SessionStartReport for a hook-driven rebind anymore.
    async fn native_binding(
        session: &str,
        path: &std::path::Path,
        cwd: &std::path::Path,
        source: Option<&str>,
    ) -> remuda_signal::SessionBinding {
        let (tx, _rx) = mpsc::channel(256);
        let bus = remuda_signal::SignalBus::new(
            remuda_signal::BusContext {
                instance_id: InstanceId::new(),
                host_id: HostId::new(),
                journal_id: Id::new("obj").expect("journal id"),
                run_id: RunId::new(),
                driver_kind: remuda_protocol::DriverKind::ShellPty,
                adapter_version: "test".into(),
            },
            tx,
            Arc::new(AtomicU64::new(0)),
        );
        let mut payload = serde_json::json!({
            "session_id": session,
            "transcript_path": path.to_string_lossy(),
            "cwd": cwd.to_string_lossy(),
        });
        if let Some(source) = source {
            payload["source"] = serde_json::json!(source);
        }
        bus.handle(remuda_signal::HookEnvelope {
            credential: "cred".into(),
            event: "SessionStart".into(),
            ppid: 7,
            payload,
        })
        .await;
        bus.binding().expect("the native bus folded a binding")
    }

    /// Core unverifiable-resume assertion shared by the item 1/2 cases: the
    /// pump anchors at current EOF, conversation appended afterwards still
    /// hydrates, and the effort gate never opens.
    async fn assert_unverified_pump_gates_history(history: &str, appended: &str, needle: &str) {
        let tmp = tempfile::tempdir().expect("tmp");
        let mut fx = pump_fixture(tmp.path(), history);
        let mut hydrator = open_hydrator(&fx, ResumeMode::Unverified, None);
        pump_once(&mut hydrator, &fx).await;
        assert!(effort_rows(&drain(&mut fx)).is_empty(), "no edge at EOF");
        append_line(&fx.binding.path, appended);
        pump_once(&mut hydrator, &fx).await;
        let observations = drain(&mut fx);
        assert!(
            effort_rows(&observations).is_empty(),
            "unverified records never open the effort gate: {observations:?}"
        );
        assert!(
            conversation_hydrated(&observations, needle),
            "conversation still hydrates: {observations:?}"
        );
    }
    #[tokio::test]
    async fn item1_a_resume_without_a_uuid_is_unverified_through_the_pump() {
        // `--resume` with no id and `--resume latest` keep the resume bit while
        // yielding no session id; `latest` is not uuid-shaped.
        let cases = [
            "claude --resume",
            "claude --resume latest",
            "claude -r",
            "node /opt/claude/cli.js --resume",
        ];
        for argv in cases {
            let (session_id, is_resume) = crate::promote::resume_provenance(argv);
            assert_eq!((session_id, is_resume), (None, true), "argv {argv:?}");
            let found = Detected {
                resume: is_resume,
                ..detected_claude(7, None)
            };
            assert_eq!(shell_resume_mode(&found), ResumeMode::Unverified);
        }
        assert_unverified_pump_gates_history(
            &assistant_line(Some("high"), 1),
            &user_line(2, "question after resume"),
            "question after resume",
        )
        .await;
    }

    #[tokio::test]
    async fn item1_no_start_time_evidence_a_resume_is_still_unverified() {
        // r5 item 6: process time is never a verifier. The mode derives only
        // from the production argv detector via shell_resume_mode: any resume
        // flag with no pre-exec snapshot is Unverified, regardless of pid.
        let argv = "claude --resume";
        let (session_id, is_resume) = crate::promote::resume_provenance(argv);
        assert_eq!(session_id, None);
        assert!(is_resume);
        let found = Detected {
            resume: is_resume,
            ..detected_claude(7, session_id.as_deref())
        };
        assert_eq!(shell_resume_mode(&found), ResumeMode::Unverified);
        assert_unverified_pump_gates_history(
            &assistant_line(Some("high"), 1),
            &assistant_line(Some("max"), 2),
            "reply 2",
        )
        .await;
    }

    #[tokio::test]
    async fn item1_a_partial_trailing_line_does_not_error_the_unverified_pump() {
        // The resumed process is mid-write on a record (no newline). The
        // unverified EOF anchor tolerates the half-written record: completing
        // it with a newline neither errors nor reopens the gate; only a
        // genuinely new record hydrates (conversation, not effort).
        let tmp = tempfile::tempdir().expect("tmp");
        let history_line = "{\"body\":\"history\"}\n";
        let partial = "{\"body\":\"partial\"}";
        let mut fx = pump_fixture(tmp.path(), &format!("{history_line}{partial}"));
        let mut hydrator = open_hydrator(&fx, ResumeMode::Unverified, None);
        pump_once(&mut hydrator, &fx).await;
        assert!(drain(&mut fx).is_empty());
        append_line(&fx.binding.path, "\n");
        pump_once(&mut hydrator, &fx).await;
        assert!(
            drain(&mut fx).is_empty(),
            "completing the partial emits nothing"
        );
        append_line(&fx.binding.path, &user_line(1, "real next question"));
        pump_once(&mut hydrator, &fx).await;
        let observations = drain(&mut fx);
        assert!(effort_rows(&observations).is_empty());
        assert!(conversation_hydrated(&observations, "real next question"));
    }

    #[tokio::test]
    async fn item2_an_exec_resume_keeping_the_shell_pid_is_never_time_verified() {
        // r7 item 3 (R6-7), c-effortread r9 item 9d: derive the mode the way
        // the poller does by running maintain_binding with the real argv
        // detector (no direct epoch_mode/open_hydrator). `exec claude --resume`
        // keeps the shell's pid/start time, so the uuid resume is Unverified
        // end to end: history never replays as current and post-anchor records
        // hydrate conversation only, never opening the effort gate.
        let history = assistant_line(Some("high"), 1);
        let tmp = tempfile::tempdir().expect("tmp");
        let mut fx = pump_fixture(tmp.path(), &history);
        fx.ctx.cwd = tmp.path().join("repo");
        let mut state = maintain_state(&fx, true);
        assert!(state.found.resume, "--resume is resume provenance");

        maintain_tick(&mut state, &fx, None).await;
        let first9d = drain(&mut fx);
        assert!(
            first9d
                .iter()
                .all(|observation| matches!(observation.body, ObservationPayload::Lifecycle(_))),
            "the resume bind may announce a lifecycle, but history never replays: {first9d:?}"
        );
        assert!(effort_rows(&first9d).is_empty());

        append_line(&fx.binding.path, &assistant_line(Some("max"), 2));
        maintain_tick(&mut state, &fx, None).await;
        let observations = drain(&mut fx);
        assert!(
            effort_rows(&observations).is_empty(),
            "post-anchor records never open the gate: {observations:?}"
        );
        assert!(conversation_hydrated(&observations, "reply 2"));
    }

    #[test]
    fn item7_no_timestamp_or_pid_path_can_ever_produce_a_verified_resume() {
        // r6 item 7: with at_process_start/process_started_at deleted, the
        // ONLY way to a verified Boundary is a pre-exec EOF snapshot. Every
        // argv-derived resume (uuid, latest, bare flag, --continue/-c) is
        // Unverified through the production detector — a clock step has no
        // code path it could influence.
        for argv in [
            "claude --resume dddddddd-2222-4333-8444-eeeeeeeeeeee",
            "claude --resume latest",
            "claude --resume",
            "claude -r",
            "claude --continue",
            "claude -c",
        ] {
            let (_, is_resume) = crate::promote::resume_provenance(argv);
            assert!(is_resume, "{argv} parses as a resume");
            let rows = crate::promote::ProcessRow {
                pid: 7,
                args: argv.to_owned(),
            };
            let detected = crate::promote::detect(std::slice::from_ref(&rows), None)
                .unwrap_or_else(|| panic!("{argv} detects as claude"));
            assert_eq!(
                shell_resume_mode(&detected),
                ResumeMode::Unverified,
                "{argv} must never be time-verified"
            );
        }
        // The exec-via-node form only resolves with its LaunchAlias; its
        // argv parser still sees the value-less resume.
        let (_, is_resume) =
            crate::promote::resume_provenance("node /opt/claude --continue fix the build");
        assert!(
            is_resume,
            "exec continue parses without consuming the prompt"
        );
        // A non-resume argv stays Fresh (the only Fresh production path).
        let (_, is_resume) = crate::promote::resume_provenance("claude");
        assert!(!is_resume);
    }

    #[tokio::test]
    async fn item7_the_unverified_anchor_is_built_once_per_epoch_not_per_poll() {
        let tmp = tempfile::tempdir().expect("tmp");
        // History already carries an effort record at the anchor instant.
        let mut fx = pump_fixture(tmp.path(), &assistant_line(Some("high"), 1));
        let mut hydrator = open_hydrator(&fx, ResumeMode::Unverified, None);
        pump_once(&mut hydrator, &fx).await;
        assert!(effort_rows(&drain(&mut fx)).is_empty());
        // A record appended between polls would be skipped by an anchor
        // rebuilt at EOF every poll; a once-per-epoch anchor still reads it
        // (as Unverified, hydrating conversation only).
        append_line(&fx.binding.path, &user_line(2, "between polls"));
        pump_once(&mut hydrator, &fx).await;
        let observations = drain(&mut fx);
        assert!(effort_rows(&observations).is_empty());
        assert!(conversation_hydrated(&observations, "between polls"));
        append_line(&fx.binding.path, &assistant_line(Some("max"), 3));
        pump_once(&mut hydrator, &fx).await;
        let observations = drain(&mut fx);
        assert!(
            effort_rows(&observations).is_empty(),
            "the gate never opens for the whole epoch: {observations:?}"
        );
        assert!(conversation_hydrated(&observations, "reply 3"));
    }
    #[tokio::test]
    async fn item6_a_one_poll_enoent_keeps_the_hydrator_and_resumes_unverified() {
        let tmp = tempfile::tempdir().expect("tmp");
        let mut fx = pump_fixture(tmp.path(), "{}\n");
        let mut hydrator = open_hydrator(&fx, ResumeMode::Unverified, None);
        pump_once(&mut hydrator, &fx).await;
        drain(&mut fx);
        // The bound file vanishes for one poll (rotation/recreate): the pump
        // survives, the hydrator is NOT torn down.
        std::fs::remove_file(&fx.binding.path).expect("vanish");
        pump_once(&mut hydrator, &fx).await;
        assert!(drain(&mut fx).is_empty(), "no observations while absent");
        // It returns with DIFFERENT content (a recreate): that poll anchors
        // at the restored EOF and replays nothing.
        append_line(&fx.binding.path, &user_line(1, "after restore"));
        append_line(&fx.binding.path, &assistant_line(Some("high"), 2));
        pump_once(&mut hydrator, &fx).await;
        assert!(drain(&mut fx).is_empty(), "restored bytes never replay");
        // Only bytes appended AFTER the anchor hydrate, still Unverified, so
        // an effort record on them cannot open the gate.
        append_line(&fx.binding.path, &user_line(3, "later question"));
        pump_once(&mut hydrator, &fx).await;
        let observations = drain(&mut fx);
        assert!(
            effort_rows(&observations).is_empty(),
            "post-restore bytes stay gated: {observations:?}"
        );
        assert!(conversation_hydrated(&observations, "later question"));
        // An identical-content recreate continues at the stored offset: the
        // later append hydrates and nothing already seen is re-emitted.
        std::fs::remove_file(&fx.binding.path).expect("vanish again");
        pump_once(&mut hydrator, &fx).await;
        let _ = drain(&mut fx);
        let restored = format!(
            "{}{}{}",
            user_line(1, "after restore"),
            assistant_line(Some("high"), 2),
            user_line(3, "later question"),
        );
        std::fs::write(&fx.binding.path, restored).expect("identical restore");
        pump_once(&mut hydrator, &fx).await;
        assert!(
            drain(&mut fx).is_empty(),
            "identical restore replays nothing"
        );
        append_line(&fx.binding.path, &user_line(4, "after identical restore"));
        pump_once(&mut hydrator, &fx).await;
        assert!(conversation_hydrated(
            &drain(&mut fx),
            "after identical restore"
        ));
    }

    #[tokio::test]
    async fn item5_verified_to_unverified_clears_effective_state_without_settling() {
        let tmp = tempfile::tempdir().expect("tmp");
        // Pre-spawn history, then the proven pre-spawn EOF snapshot.
        let mut fx = pump_fixture(tmp.path(), "{\"before\":true}\n");
        let boundary = crate::claude_transcript::ResumeBoundary::snapshot(&fx.binding.path)
            .expect("pre-spawn snapshot");
        let bridge = Arc::new(crate::effort::EffortBridge::new());
        let mut hydrator = open_hydrator(&fx, ResumeMode::Boundary(boundary), Some(&bridge));
        // A current-process record publishes an effective high.
        append_line(&fx.binding.path, &assistant_line(Some("high"), 1));
        pump_once(&mut hydrator, &fx).await;
        let edges = effort_rows(&drain(&mut fx));
        assert_eq!(
            edges,
            vec![(Some(remuda_protocol::EffortName::High), None)],
            "the verified boundary opens the gate"
        );
        // Remuda arms a switch to max; its slash record is a current record.
        let generation = bridge.arm(crate::effort::EffortRequest::from_level("max").expect("max"));
        assert!(bridge.has_pending());
        append_line(&fx.binding.path, &slash_line("max", 2));
        pump_once(&mut hydrator, &fx).await;
        drain(&mut fx);
        assert!(bridge.has_pending());

        // The bound file is replaced (shrink): the boundary is lost.
        // c-effortread r8 item 4(e): file shrink/replacement IS still a
        // withdrawal trigger (only session-local exec stopped being one); the
        // edge carries readbackAvailable=false, which effort_rows exposes
        // separately (the level field is null on the withdrawal edge).
        std::fs::write(&fx.binding.path, "{\"rotated\":true}\n").expect("rotate");
        pump_once(&mut hydrator, &fx).await;
        let withdrawn: Vec<bool> = drain(&mut fx)
            .iter()
            .filter_map(|observation| match &observation.body {
                ObservationPayload::Effort(payload) => {
                    Some(payload.effective.readback_available == Some(false))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            withdrawn,
            vec![true],
            "one explicit read-back-unavailable edge withdraws the projected state"
        );
        assert!(
            bridge.has_pending(),
            "the pending switch is not resolved or rejected"
        );

        // The max verdict then lands in the recreated file: it must not settle
        // the switch Applied, and it emits no effort edge.
        append_line(
            &fx.binding.path,
            &stdout_verdict("Set effort level to max", 3),
        );
        pump_once(&mut hydrator, &fx).await;
        let observations = drain(&mut fx);
        assert!(
            effort_rows(&observations).is_empty(),
            "an unverified verdict emits nothing: {observations:?}"
        );
        assert!(
            bridge.has_pending(),
            "read-back loss never resolves a pending switch"
        );
        let settled = bridge
            .wait(generation, std::time::Duration::from_millis(150))
            .await;
        assert!(settled.is_none(), "the switch keeps waiting, never Applied");
    }

    /// c-effortread r9 item 3: a previously-CURRENT RESUME anchor X whose
    /// inode changed while the session was inactive must NOT fall back to
    /// Fresh on `/resume X` (that replayed X's history as current, so an old
    /// verdict could resolve a newly armed switch). Continuation failing must
    /// reopen against X's OWN boundary/unverified mode. The origin (live vs
    /// resume) is distinct from the gate-open flag — a verified resume reads
    /// Current but is still a resume origin.
    #[tokio::test]
    async fn r9_item3_resume_x_with_a_changed_inode_reopens_at_its_own_mode_not_fresh() {
        let tmp = tempfile::tempdir().expect("tmp");
        let mut fx = pump_fixture(tmp.path(), &assistant_line(Some("high"), 1));
        // A proven pre-spawn EOF boundary anchors X; post-anchor content is
        // verified Current, so the captured anchor is a CURRENT resume anchor
        // (current=true) with a RESUME origin.
        let boundary = crate::claude_transcript::ResumeBoundary::snapshot(&fx.binding.path)
            .expect("verified pre-spawn snapshot");
        let bridge = Arc::new(crate::effort::EffortBridge::new());
        let mut hydrator = open_hydrator(&fx, ResumeMode::Boundary(boundary), Some(&bridge));
        append_line(&fx.binding.path, &assistant_line(Some("xhigh"), 2));
        pump_once(&mut hydrator, &fx).await;
        assert!(
            effort_rows(&drain(&mut fx))
                .iter()
                .any(|(name, _)| *name == Some(remuda_protocol::EffortName::Xhigh)),
            "the post-boundary xhigh is verified Current"
        );
        let anchor = hydrator
            .tail
            .anchor()
            .expect("the current resume tail yields an anchor");
        assert!(
            !anchor.is_live_origin(),
            "a verified resume still has a RESUME origin even while current"
        );

        // Switch away to Z, then return: argv --resume X derives X's OWN mode
        // (never time-verified) exactly as the poller does.
        let row = ProcessRow {
            pid: 7,
            args: format!("claude --resume {PUMP_SESSION}"),
        };
        let found = crate::promote::detect(std::slice::from_ref(&row), None)
            .expect("the exec argv detects as claude");
        let mut launch = LaunchModeBinding::Unbound;
        let x_mode = epoch_mode(&mut launch, None, None, &found);
        assert!(
            matches!(x_mode, ResumeMode::Unverified | ResumeMode::Boundary(_)),
            "X's own returning mode is an unverified/boundary resume, not Fresh"
        );

        // While inactive X.jsonl is replaced (new inode) carrying a STALE
        // verdict for max — history that must never become current evidence.
        let stale = assistant_line(Some("max"), 3);
        std::fs::write(&fx.binding.path, &stale).expect("X.jsonl rewritten to a new inode");

        // The production continuation decision: even though the stored anchor
        // was current, its RESUME origin means a failed continuation reopens in
        // X's own mode, never Fresh.
        let reopen_mode = continuation_resume_mode(Some(&anchor), x_mode);
        assert!(
            !matches!(reopen_mode, ResumeMode::Fresh),
            "a resume anchor must not substitute Fresh on a failed continuation"
        );

        // Full reopen through the production Hydrator: continued() rejects the
        // stale inode, so it opens X's own unverified EOF — the stale max
        // history publishes nothing.
        let mut returned = Hydrator::open(
            &fx.ctx,
            &fx.binding,
            Some(&bridge),
            None,
            None,
            None,
            None,
            reopen_mode,
            Some(anchor),
            None,
        )
        .expect("hydrator reopens at X's own boundary");
        pump_once(&mut returned, &fx).await;
        let rows = effort_rows(&drain(&mut fx));
        assert!(
            rows.is_empty(),
            "X's replaced history must not publish as current: {rows:?}"
        );
    }

    /// c-effortread r9 item 2: a Fresh (live) tail that is demoted and then
    /// re-promoted must keep its LIVE origin across the continuation. It used
    /// to reopen as a resume tail, so a single ENOENT (a rotate/recreate while
    /// the terminal is briefly demoted) marked it displaced and withdrew
    /// read-back for the rest of the run instead of taking the live tail's
    /// bounded retry. After the file is restored, a later effort record must
    /// still verify.
    #[tokio::test]
    async fn r9_item2_repromoted_fresh_tail_survives_one_enoent_and_keeps_verifying() {
        let tmp = tempfile::tempdir().expect("tmp");
        let first_turn = assistant_line(Some("high"), 1);
        let mut fx = pump_fixture(tmp.path(), &first_turn);
        let bridge = Arc::new(crate::effort::EffortBridge::new());
        let mut hydrator = open_hydrator(&fx, ResumeMode::Fresh, Some(&bridge));
        pump_once(&mut hydrator, &fx).await;
        assert!(
            effort_rows(&drain(&mut fx))
                .iter()
                .any(|(name, _)| *name == Some(remuda_protocol::EffortName::High)),
            "the Fresh first turn verifies high"
        );

        // Demote: capture the live read anchor, then re-promote THROUGH the
        // production open path continuing that anchor.
        let anchor = hydrator.tail.anchor().expect("a live anchor is captured");
        let mut again = Hydrator::open(
            &fx.ctx,
            &fx.binding,
            Some(&bridge),
            None,
            None,
            None,
            None,
            ResumeMode::Fresh,
            Some(anchor),
            None,
        )
        .expect("re-promotion reopens the Fresh tail");

        // The bound file vanishes for exactly one poll (rotate/recreate).
        std::fs::remove_file(&fx.binding.path).expect("vanish");
        pump(&mut again, &fx.tx, &fx.seq, &fx.ctx)
            .await
            .expect("a live tail tolerates one missing poll");
        assert!(
            drain(&mut fx).is_empty(),
            "the ENOENT poll emits nothing and does not withdraw read-back"
        );

        // Restored under a NEW inode with identical content, then a later
        // effort verdict appends. On the live tail it verifies; the displaced
        // resume tail the old code reopened as would report it Unverified.
        write(&fx.binding.path, &first_turn);
        append_line(&fx.binding.path, &assistant_line(Some("low"), 2));
        pump_once(&mut again, &fx).await;
        let rows = effort_rows(&drain(&mut fx));
        assert!(
            rows.iter()
                .any(|(name, _)| *name == Some(remuda_protocol::EffortName::Low)),
            "the later low verdict still verifies on the restored live tail: {rows:?}"
        );
    }

    /// r5 item 3: `claude -c` / `--continue` parses as a value-less resume,
    /// so the continued transcript is tailed from current EOF unverified, not
    /// byte 0 as current. The mode derives through the production argv parser.
    /// r5 item 5: a LIVE tail whose bound file vanishes is tolerated only for a
    /// few consecutive polls; after that the pump errors so the binding is
    /// marked degraded (TRANSCRIPT_DEGRADED) instead of waiting forever.
    #[tokio::test]
    async fn item5_a_persistently_vanished_live_tail_degrades_after_the_threshold() {
        let tmp = tempfile::tempdir().expect("tmp");
        let fx = pump_fixture(tmp.path(), "{}\n");
        let mut hydrator = open_hydrator(&fx, ResumeMode::Fresh, None);
        // The file is genuinely gone (not a resume-tail one-poll ENOENT).
        std::fs::remove_file(&fx.binding.path).expect("vanish");
        for expected in 1..LIVE_MISSING_POLL_DEGRADE {
            assert_eq!(hydrator.missing_polls, expected - 1);
            pump(&mut hydrator, &fx.tx, &fx.seq, &fx.ctx)
                .await
                .expect("transient missing polls are tolerated");
        }
        pump(&mut hydrator, &fx.tx, &fx.seq, &fx.ctx)
            .await
            .expect_err("the threshold poll degrades the binding");
        // The file returning resets the counter: the next vanish again gets
        // the full tolerance window instead of degrading immediately.
        let mut live = open_hydrator(&fx, ResumeMode::Fresh, None);
        std::fs::write(&fx.binding.path, "{}\n").expect("restored");
        pump_once(&mut live, &fx).await;
        assert_eq!(
            live.missing_polls, 0,
            "a successful poll resets the counter"
        );
        std::fs::remove_file(&fx.binding.path).expect("vanish again");
        for _ in 1..LIVE_MISSING_POLL_DEGRADE {
            pump(&mut live, &fx.tx, &fx.seq, &fx.ctx)
                .await
                .expect("the new window tolerates transient misses again");
        }
        pump(&mut live, &fx.tx, &fx.seq, &fx.ctx)
            .await
            .expect_err("the repeated vanish degrades again");
    }

    #[tokio::test]
    async fn item3_continue_flag_tails_history_from_eof_unverified() {
        let history = assistant_line(Some("high"), 1);
        let dir = tempfile::tempdir().expect("tmp");
        let mut fx = pump_fixture(dir.path(), &history);
        // The real detector on `claude -c "fix the build"` sees a resume with
        // no session id and keeps the prompt token.
        let (session_id, is_resume) = crate::promote::resume_provenance("claude -c fix the build");
        assert!(is_resume, "-c is a resume flag");
        assert_eq!(session_id, None, "the prompt is not consumed as an id");
        let mut found = detected_claude(7, session_id.as_deref());
        found.resume = is_resume;
        let mode = shell_resume_mode(&found);
        assert_eq!(mode, ResumeMode::Unverified);
        let mut hydrator = open_hydrator(&fx, mode, None);
        pump_once(&mut hydrator, &fx).await;
        assert!(
            drain(&mut fx).is_empty(),
            "history must not replay as current"
        );
        append_line(&fx.binding.path, &user_line(2, "continued session work"));
        pump_once(&mut hydrator, &fx).await;
        assert!(conversation_hydrated(
            &drain(&mut fx),
            "continued session work"
        ));
        // A high record appended after the anchor still does NOT open the gate
        // (value-less continue is unverifiable).
        append_line(&fx.binding.path, &assistant_line(Some("max"), 3));
        pump_once(&mut hydrator, &fx).await;
        assert!(effort_rows(&drain(&mut fx)).is_empty());
    }

    #[test]
    fn rebound_mode_is_fresh_for_absent_or_empty_and_unverified_eof_otherwise() {
        use crate::claude_transcript::ResumeMode;
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("resumed.jsonl");
        // Absent: Fresh (the new session's file does not exist yet).
        assert_eq!(ResumeMode::rebound_mode(&path), ResumeMode::Fresh);
        // Empty: Fresh (`/clear` created a zero-length file).
        write(&path, "");
        assert_eq!(ResumeMode::rebound_mode(&path), ResumeMode::Fresh);
        // Non-empty: an unverified boundary anchored at the CURRENT EOF.
        write(&path, "{\"history\":true}\n");
        match ResumeMode::rebound_mode(&path) {
            ResumeMode::Boundary(boundary) => {
                assert!(!boundary.verified, "a rebound is never time-verified");
                assert_eq!(boundary.start as usize, "{\"history\":true}\n".len());
            }
            other => panic!("expected an unverified boundary, got {other:?}"),
        }
    }

    /// Unit: the SessionModeTable records one mode per (pid, session) at its
    /// first SessionStart — launch session keeps the pre-spawn mode, later
    /// sessions are rebounds, and the transcript presence at that instant
    /// decides Fresh vs Unverified.
    #[test]
    fn session_mode_table_records_own_then_rebound_modes_once() {
        let dir = tempfile::tempdir().expect("tmp");
        let cwd = dir.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let older = slug_session(dir.path(), &cwd, LATE_STARTER, "{\"old\":true}\n");
        let newer = slug_session(dir.path(), &cwd, BUSY_OTHER, "");
        let mut table = SessionModeTable::default();
        let pre = Some(ResumeMode::Unverified);
        let own_found = detected_claude(7, Some(CORRECT));
        // First report for the pid = the launched session: keeps the
        // pre-spawn mode, rebound=false.
        let first = table.record(7, CORRECT, None, pre, None, &own_found);
        assert!(!first.rebound);
        assert_eq!(first.mode, ResumeMode::Unverified);
        // Idempotent: a repeat report cannot flip it.
        assert_eq!(
            table.record(7, CORRECT, Some(&older), pre, None, &own_found),
            first
        );
        // A later /resume session with an existing transcript: Unverified
        // EOF, rebound=true.
        let rebound = table.record(7, LATE_STARTER, Some(&older), pre, None, &own_found);
        assert!(rebound.rebound);
        assert!(
            matches!(rebound.mode, ResumeMode::Boundary(b) if !b.verified),
            "existing transcript -> unverified anchor: {:?}",
            rebound.mode
        );
        // A later /clear session whose file is EMPTY at the report: Fresh.
        let clear = table.record(7, BUSY_OTHER, Some(&newer), pre, None, &own_found);
        assert!(clear.rebound);
        assert_eq!(clear.mode, ResumeMode::Fresh);
        // Lookup.
        assert!(table.mode_for(7, LATE_STARTER).is_some());
        assert!(table.mode_for(8, LATE_STARTER).is_none());
    }

    /// c-effortread r9 item 4: a hook-raced FIRST record for a switched-id
    /// (argv still names the real launch session) must not CONSUME the pid's
    /// launch slot. When the real launch-id is recorded afterward it is the
    /// launch session: non-rebound and keeping `pre_resume_mode`. The old code
    /// ran `first_pid.insert(pid)` even for the losing record, so the launch-id
    /// later landed as a rebound and lost its pre-spawn resume mode.
    #[test]
    fn r9_item4_a_raced_switch_record_does_not_consume_the_launch_slot() {
        let dir = tempfile::tempdir().expect("tmp");
        let cwd = dir.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let launched = slug_session(dir.path(), &cwd, "launch-id", "{}\n");
        let switched = slug_session(dir.path(), &cwd, "switched-id", "{}\n");
        let mut table = SessionModeTable::default();
        // argv proves the launch session is launch-id throughout.
        let argv_found = detected_claude(9, Some("launch-id"));

        // The hook for switched-id wins the race to the first record.
        let raced = table.record(9, "switched-id", Some(&switched), None, None, &argv_found);
        assert!(raced.rebound, "argv names a different session -> rebound");

        // The real launch-id is recorded next on the SAME table with a
        // pre-spawn resume capture. It must still own the launch slot.
        let launch = table.record(
            9,
            "launch-id",
            Some(&launched),
            Some(ResumeMode::Unverified),
            None,
            &detected_claude(9, Some("launch-id")),
        );
        assert!(
            !launch.rebound,
            "the launch-id keeps the launch slot after a raced switch record"
        );
        assert!(
            matches!(
                launch.mode,
                ResumeMode::Unverified | ResumeMode::Boundary(_)
            ),
            "the launch-id keeps its pre-spawn resume mode, not rebound sizing: {:?}",
            launch.mode
        );
    }

    /// c-effortread r8 item 4(a): the pid's first RECORDED session is assumed
    /// to be the launch session only when argv agrees. If argv names a
    /// different session (a hook/poll raced an in-TUI switch), the recorded
    /// session is a rebound.
    #[test]
    fn r8_item4a_first_recorded_session_that_disagrees_with_argv_is_a_rebound() {
        let dir = tempfile::tempdir().expect("tmp");
        let cwd = dir.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let launched = slug_session(dir.path(), &cwd, "launch-id", "{}\n");
        let switched = slug_session(dir.path(), &cwd, "switched-id", "{}\n");
        let mut table = SessionModeTable::default();
        // argv says the launch session is "launch-id" but the first RECORDED
        // session is "switched-id". Without a hook source the rebound file is
        // non-empty → unverified anchor; the key assertion is rebound=true.
        let argv_found = detected_claude(9, Some("launch-id"));
        let m = table.record(9, "switched-id", Some(&switched), None, None, &argv_found);
        assert!(
            m.rebound,
            "argv disagreement => the recorded session is a rebound"
        );
        assert!(
            matches!(m.mode, ResumeMode::Boundary(_)),
            "non-empty file without a clear source sizes to an unverified anchor"
        );

        // With a /clear source the same argv disagreement is a rebound Fresh.
        let mut table_clear = SessionModeTable::default();
        let cleared = table_clear.record(
            9,
            "switched-id",
            Some(&switched),
            None,
            Some("clear"),
            &argv_found,
        );
        assert!(cleared.rebound);
        assert_eq!(cleared.mode, ResumeMode::Fresh);

        // Agreement stays the launch session.
        let mut table2 = SessionModeTable::default();
        let agreed = table2.record(
            9,
            "launch-id",
            Some(&launched),
            None,
            None,
            &detected_claude(9, Some("launch-id")),
        );
        assert!(
            !agreed.rebound,
            "argv agreement => the first session is the launch session"
        );
        // No argv session id at all: first recorded wins (legacy behaviour).
        let mut table3 = SessionModeTable::default();
        let hand_typed = table3.record(
            9,
            "switched-id",
            Some(&switched),
            None,
            None,
            &detected_claude(9, None),
        );
        assert!(
            !hand_typed.rebound,
            "no argv id => first recorded is the launch session"
        );
    }

    /// c-effortread r8 item 6 (Opus) / c-effortread r10 item 9: returning to
    /// the LAUNCH session after a `/clear` must inherit the launch sticky's
    /// own (Unverified) mode, never the rebound /clear session's Dead(Fresh).
    /// Driven through the PRODUCTION orchestration (maintain_binding ticks +
    /// rebind_authenticated_start), not table.record/epoch_mode directly.
    /// The X return uses a DISTINCT inode (remove + recreate) so a continued
    /// anchor cannot accidentally validate; the resumed history must never
    /// publish Current evidence.
    #[tokio::test]
    async fn r8_item6_returning_to_launch_after_clear_reopens_unverified_via_maintain_binding() {
        use crate::effort::{EffortBridge, EffortRequest};
        use remuda_protocol::EffortAxis::Tier;
        use remuda_protocol::EffortSemantics::Decoupled;

        let dir = tempfile::tempdir().expect("tmp");
        let cwd = dir.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let launch_path = slug_session(dir.path(), &cwd, "launch-x", "{}\n");
        let clear_path = slug_session(dir.path(), &cwd, "clear-z", "{}\n");
        let mut fx = pump_fixture(dir.path(), "{}\n");
        fx.ctx.cwd = cwd.clone();
        fx.binding.session_id = "launch-x".to_owned();
        fx.binding.path = launch_path.clone();

        // A REAL bridge with a request armed across the whole sequence. The
        // replaced X's historical slash/verdict must never settle it.
        let bridge = Arc::new(EffortBridge::new());
        let armed = EffortRequest::for_configure(
            "max",
            Some(false),
            Some(Tier),
            Decoupled,
            None,
            None,
            Some("req-returning-x"),
        )
        .expect("max tier request");
        bridge.arm(armed);

        let bindings = BindingHandle::empty();
        bindings.begin_epoch(&cwd, dir.path());
        let mut hydrator: Option<Hydrator> = None;
        let mut announced: Option<String> = None;
        let mut session_modes = SessionModeTable::default();
        let mut launch_mode = LaunchModeBinding::Unbound;
        let mut read_anchors = std::collections::HashMap::new();
        let mut effort_seeds = std::collections::HashMap::new();
        let mut withdrawn_sessions = std::collections::HashSet::new();
        let mut active_session: Option<(i32, String)> = None;
        let found_x = detected_claude(7, Some("launch-x"));

        // Tick 1: the launch SessionStart rides the NATIVE bus through the
        // production hook seam, then maintain_binding binds and pumps.
        let launch_binding = native_binding("launch-x", &launch_path, &cwd, None).await;
        // The launch report names the same session argv already does: no
        // rebind, the first maintain_binding tick resolves the argv channel.
        assert!(!apply_native_session_binding(
            &bindings,
            &found_x,
            &launch_binding,
            Some(ResumeMode::Unverified),
            &mut session_modes,
            &mut read_anchors,
            &mut withdrawn_sessions,
            &mut active_session,
        ));
        maintain_binding(
            &bindings,
            &fx.ctx,
            &found_x,
            &mut hydrator,
            &mut announced,
            &fx.tx,
            &fx.seq,
            Some(&bridge),
            None,
            None,
            None,
            None,
            &mut session_modes,
            &mut launch_mode,
            Some(ResumeMode::Unverified),
            &mut read_anchors,
            &mut effort_seeds,
            &mut withdrawn_sessions,
            &mut active_session,
        )
        .await;
        assert!(hydrator.is_some(), "launch X hydrates");
        let _ = drain(&mut fx);

        // Tick 2: an in-TUI /clear to Z (Fresh rebound) over the native bus.
        // The production seam performs the rebind, the withdrawal marking and
        // the report ingestion; it returns true so the caller drops hydrator.
        let clear_binding = native_binding("clear-z", &clear_path, &cwd, Some("clear")).await;
        let found_z = detected_claude(7, Some("clear-z"));
        assert!(apply_native_session_binding(
            &bindings,
            &found_z,
            &clear_binding,
            None,
            &mut session_modes,
            &mut read_anchors,
            &mut withdrawn_sessions,
            &mut active_session,
        ));
        // Production loop rebind arm.
        hydrator = None;
        announced = None;
        fx.binding.session_id = "clear-z".to_owned();
        fx.binding.path = clear_path.clone();
        maintain_binding(
            &bindings,
            &fx.ctx,
            &found_z,
            &mut hydrator,
            &mut announced,
            &fx.tx,
            &fx.seq,
            Some(&bridge),
            None,
            None,
            None,
            None,
            &mut session_modes,
            &mut launch_mode,
            None,
            &mut read_anchors,
            &mut effort_seeds,
            &mut withdrawn_sessions,
            &mut active_session,
        )
        .await;
        assert!(hydrator.is_some(), "the /clear rebound hydrates Fresh");
        let _ = drain(&mut fx);

        // Tick 3: /resume back to X. X.jsonl was REPLACED with a DISTINCT
        // inode carrying a prior session's high record AND a historical
        // /effort max slash+verdict — the exact pair that settles the armed
        // request when the boundary is wrongly Fresh. Rename the old file
        // aside so the recreated session file gets a NEW inode.
        std::fs::rename(&launch_path, dir.path().join("launch-x.old"))
            .expect("move the old X inode aside");
        let x_slash_max = |n: u64| {
            serde_json::json!({
                "type": "user",
                "uuid": format!("cmd-{n}"),
                "sessionId": "launch-x",
                "version": DECOUPLED,
                "message": {
                    "role": "user",
                    "content": "<command-name>/effort</command-name>\n<command-message>effort</command-message>\n<command-args>max</command-args>",
                },
            })
            .to_string()
                + "\n"
        };
        let x_stdout = |n: u64, body: &str| {
            serde_json::json!({
                "type": "user",
                "uuid": format!("out-{n}"),
                "sessionId": "launch-x",
                "version": DECOUPLED,
                "message": {"role": "user", "content": format!("<local-command-stdout>{body}</local-command-stdout>")},
            })
            .to_string()
                + "\n"
        };
        // The replacement X file already carries, BEFORE re-promotion, the
        // resumed session's high read-back AND the historical /effort max slash
        // with its ACCEPTED verdict — the exact pair that settles the armed
        // request when the returning boundary is wrongly opened Fresh.
        let restored_history = format!(
            "{}{}{}",
            serde_json::json!({
                "type": "assistant",
                "uuid": "msg-5",
                "sessionId": "launch-x",
                "version": DECOUPLED,
                "message": {
                    "id": "msg-5",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "high reply"}],
                    "effort": "high"
                },
            })
            .to_string()
                + "\n",
            x_slash_max(6),
            x_stdout(
                7,
                "Set effort level to max (this session only): Maximum capability",
            ),
        );
        let restored = slug_session(dir.path(), &cwd, "launch-x", &restored_history);
        let resume_binding = native_binding("launch-x", &restored, &cwd, Some("resume")).await;
        assert!(apply_native_session_binding(
            &bindings,
            &found_x,
            &resume_binding,
            None,
            &mut session_modes,
            &mut read_anchors,
            &mut withdrawn_sessions,
            &mut active_session,
        ));

        // c-effortread r12 item 8: assert the PRODUCTION MODE-SELECTION result
        // BEFORE the explicit-withdrawal override can mask it. The X→Z rebind
        // marked X withdrawn, so maintain_binding's session_withdrawn arm would
        // force an Unverified reopen regardless of the selected mode; the old
        // test only asserted after that override. epoch_mode is the exact
        // selection maintain_binding runs — the returning launch sticky must
        // keep its OWN Unverified mode, never the rebound Z's carried
        // Dead(Fresh).
        assert!(
            withdrawn_sessions.contains(&(7, "launch-x".to_string())),
            "X is the withdrawn session whose reopen override would mask the selection"
        );
        let sticky_x = session_modes
            .mode_for(7, "launch-x")
            .expect("the launch sticky survives the Z rebind");
        let mut probe_launch_mode = launch_mode.clone();
        let mut probe_found = found_x.clone();
        probe_found.session_id = Some("launch-x".to_owned());
        let selected_mode = epoch_mode(
            &mut probe_launch_mode,
            Some(ResumeMode::Unverified),
            Some(sticky_x),
            &probe_found,
        );
        let selected_unverified = match selected_mode {
            ResumeMode::Unverified => true,
            ResumeMode::Boundary(boundary) => !boundary.verified,
            ResumeMode::Fresh => false,
        };
        assert!(
            selected_unverified,
            "the production mode selection is itself unverified, independent of the withdrawal \
             override: {selected_mode:?}"
        );

        // Production loop rebind arm.
        hydrator = None;
        announced = None;
        fx.binding.session_id = "launch-x".to_owned();
        fx.binding.path = restored.clone();
        maintain_binding(
            &bindings,
            &fx.ctx,
            &found_x,
            &mut hydrator,
            &mut announced,
            &fx.tx,
            &fx.seq,
            Some(&bridge),
            None,
            None,
            None,
            None,
            &mut session_modes,
            &mut launch_mode,
            Some(ResumeMode::Unverified),
            &mut read_anchors,
            &mut effort_seeds,
            &mut withdrawn_sessions,
            &mut active_session,
        )
        .await;
        let reopened = hydrator.as_ref().expect("X reopens after the rebind");
        assert!(
            !reopened.tail.verified(),
            "returning to X reuses an Unverified boundary, not Z's Fresh: {:?}",
            session_modes.mode_for(7, "launch-x").map(|m| m.mode)
        );

        // The slash + accepted verdict are replayed as UNVERIFIED HISTORY at
        // re-promotion (they were already in the replaced file): no Current
        // effort edge, and the armed request survives that replay.
        let replayed = drain(&mut fx);
        assert!(
            !effort_rows(&replayed)
                .iter()
                .any(|(name, _)| *name == Some(remuda_protocol::EffortName::High)
                    || *name == Some(remuda_protocol::EffortName::Max)),
            "the replaced X history (slash+verdict included) never publishes Current effort"
        );
        let pending = bridge
            .pending()
            .expect("the pre-repromotion slash/verdict replay cannot settle the armed request");
        assert_eq!(pending.request_id.as_deref(), Some("req-returning-x"));
        assert_eq!(pending.name, remuda_protocol::EffortName::Max);

        // Defense in depth: a slash + verdict appended AFTER re-promotion, while
        // the epoch is still unverified, likewise hydrate conversation but can
        // neither publish an edge nor settle the armed bridge.
        append_line(&fx.binding.path, &x_slash_max(8));
        append_line(
            &fx.binding.path,
            &x_stdout(
                9,
                "Set effort level to max (this session only): Maximum capability",
            ),
        );
        maintain_binding(
            &bindings,
            &fx.ctx,
            &found_x,
            &mut hydrator,
            &mut announced,
            &fx.tx,
            &fx.seq,
            Some(&bridge),
            None,
            None,
            None,
            None,
            &mut session_modes,
            &mut launch_mode,
            Some(ResumeMode::Unverified),
            &mut read_anchors,
            &mut effort_seeds,
            &mut withdrawn_sessions,
            &mut active_session,
        )
        .await;
        let observations = drain(&mut fx);
        assert!(
            !effort_rows(&observations)
                .iter()
                .any(|(name, _)| *name == Some(remuda_protocol::EffortName::High)
                    || *name == Some(remuda_protocol::EffortName::Max)),
            "unverified slash/verdict appended after re-promotion never publishes Current effort: {:?}",
            effort_rows(&observations)
        );
        let pending = bridge
            .pending()
            .expect("the resumed X history never settles the armed request");
        assert_eq!(pending.request_id.as_deref(), Some("req-returning-x"));
        assert_eq!(pending.name, remuda_protocol::EffortName::Max);
    }

    /// c-effortread r8 item 2: a `/clear` hook with source "clear" is Fresh
    /// even when the rebound transcript ALREADY HOLDS THE PROMPT at the time
    /// the sticky mode is recorded (queued/pasted prompt beat the 800 ms
    /// poll tick). Sizing at the tick used to make it an unverified EOF anchor,
    /// which skipped the prompt for hydration and kept the effort gate shut.
    #[test]
    fn r8_item2_clear_source_is_fresh_when_transcript_already_nonempty() {
        let dir = tempfile::tempdir().expect("tmp");
        let cwd = dir.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        // Simulate the r7 ordering: the rebound session Z's transcript already
        // has content (the queued prompt) BEFORE the poll block records mode.
        let z: std::path::PathBuf = slug_session(
            dir.path(),
            &cwd,
            "clear-session",
            "{\"type\":\"user\",\"message\":{\"content\":\"do the thing\"}}\n",
        );
        let mut table = SessionModeTable::default();
        let own_found = detected_claude(7, Some(CORRECT));
        // The launched process's first session (so the /clear one is a rebound).
        let launch = slug_session(dir.path(), &cwd, CORRECT, "{}\n");
        table.record(7, CORRECT, Some(&launch), None, None, &own_found);

        // A /clear report (source captured at hook delivery): Fresh despite
        // the non-empty file.
        let cleared = table.record(
            7,
            "clear-session",
            Some(&z),
            None,
            Some("clear"),
            &own_found,
        );
        assert!(cleared.rebound);
        assert_eq!(cleared.mode, ResumeMode::Fresh);

        // startup and compact are likewise fresh-new sessions; resume stays an
        // unverified anchor; unknown source falls back to file sizing. Each
        // gets a distinct rebound session id on the SAME (already-launched)
        // pid.
        for (source, expect_fresh, label) in [
            ("startup", true, "startup"),
            ("compact", true, "compact"),
            ("resume", false, "resume"),
        ] {
            let m = table.record(
                7,
                &format!("rebound-{label}"),
                Some(&z),
                None,
                Some(source),
                &own_found,
            );
            assert!(m.rebound, "{label} is a rebound session");
            if expect_fresh {
                assert_eq!(m.mode, ResumeMode::Fresh, "{label} source is a fresh start");
            } else {
                assert!(
                    matches!(m.mode, ResumeMode::Boundary(b) if !b.verified),
                    "{label} with history -> unverified anchor: {:?}",
                    m.mode
                );
            }
        }

        // Unknown source (older hook payload) falls back to sizing: non-empty
        // file -> unverified.
        let unknown = table.record(7, "rebound-unknown", Some(&z), None, None, &own_found);
        assert!(
            matches!(unknown.mode, ResumeMode::Boundary(_)),
            "unknown source sizes the file (non-empty -> unverified): {:?}",
            unknown.mode
        );
    }

    /// c-effortread r9 item 1: the source must ride the NATIVE hook bus
    /// binding all the way to the first-tick classification. A `/clear` whose
    /// Z.jsonl ALREADY holds the queued prompt when the pump ingests the
    /// SessionStart is Fresh (gate opens, queued prompt is current session
    /// content) — never an unverified EOF anchor that skips the prompt and
    /// keeps the effort read-back gate shut. Unlike the r8 test this goes bus
    /// -> binding -> [`SessionStartReport::from_bus_binding`] -> record -> the
    /// production Hydrator pump, with no hand-passed `Some("clear")`.
    /// c-effortread r9 item 1 / r11 item 7: the source rides the NATIVE hook
    /// bus all the way to the first-tick classification, through the PRODUCTION
    /// `apply_native_session_binding` seam (classify → rebind/first-claim arm
    /// → ingest) — no manual ingest, no direct rebind call, no table.record
    /// hand-off. A `/clear` whose Z.jsonl ALREADY holds the queued prompt is
    /// Fresh: the gate opens and the queued prompt is current-session content.
    #[tokio::test]
    async fn r9_item1_native_bus_clear_source_sizes_z_fresh_at_the_first_tick() {
        let dir = tempfile::tempdir().expect("tmp");
        let cwd = dir.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let launch = slug_session(dir.path(), &cwd, CORRECT, "{}\n");
        // Z.jsonl already holds the queued prompt BEFORE the first poll tick.
        let z: std::path::PathBuf = slug_session(
            dir.path(),
            &cwd,
            "clear-z",
            &user_line(1, "queued before first tick"),
        );

        let mut fx = pump_fixture(dir.path(), "{}");
        fx.ctx.cwd = cwd.clone();
        fx.binding.session_id = CORRECT.to_owned();
        fx.binding.path = launch.clone();
        let bindings = BindingHandle::empty();
        bindings.begin_epoch(&cwd, dir.path());
        let mut hydrator: Option<Hydrator> = None;
        let mut announced: Option<String> = None;
        let mut session_modes = SessionModeTable::default();
        let mut launch_mode = LaunchModeBinding::Unbound;
        let mut read_anchors = std::collections::HashMap::new();
        let mut effort_seeds = std::collections::HashMap::new();
        let mut withdrawn_sessions = std::collections::HashSet::new();
        let mut active_session: Option<(i32, String)> = None;
        let launch_found = detected_claude(7, Some(CORRECT));

        // The launch SessionStart rides the native bus through the production
        // seam; argv already names that session, so no rebind happens and the
        // first maintain_binding tick resolves + pumps it.
        let launch_binding = native_binding(CORRECT, &launch, &cwd, Some("startup")).await;
        assert!(!apply_native_session_binding(
            &bindings,
            &launch_found,
            &launch_binding,
            None,
            &mut session_modes,
            &mut read_anchors,
            &mut withdrawn_sessions,
            &mut active_session,
        ));
        // c-effortread r12 item 7: assert the classified mode IMMEDIATELY at
        // the seam. Without this, deleting the record() inside
        // apply_native_session_binding was masked — maintain_binding
        // re-classifies before the later hydrator assertion runs. The startup
        // source is the launch session's own FIRST record → Fresh, not rebound.
        let launch_mode_at_seam = session_modes
            .mode_for(7, CORRECT)
            .expect("the launch seam records a mode before maintain_binding");
        assert!(
            matches!(
                launch_mode_at_seam.mode,
                crate::claude_transcript::ResumeMode::Fresh
            ) && !launch_mode_at_seam.rebound,
            "startup source classifies the launch session Fresh at the seam: {launch_mode_at_seam:?}"
        );
        maintain_binding(
            &bindings,
            &fx.ctx,
            &launch_found,
            &mut hydrator,
            &mut announced,
            &fx.tx,
            &fx.seq,
            None,
            None,
            None,
            None,
            None,
            &mut session_modes,
            &mut launch_mode,
            None,
            &mut read_anchors,
            &mut effort_seeds,
            &mut withdrawn_sessions,
            &mut active_session,
        )
        .await;
        assert!(hydrator.is_some(), "the launch session hydrates first");
        let _ = drain(&mut fx);

        // The /clear SessionStart (source folded on the native bus) goes
        // through the SAME seam: it classifies Z Fresh from the source despite
        // the non-empty file, performs the authenticated rebind (returning
        // true so the loop drops the previous hydrator), and ingests. The
        // argv the poller reads is IMMUTABLE — it still names the launch
        // session; only the authenticated hook report knows about Z.
        let z_found = launch_found.clone();
        let clear_binding = native_binding("clear-z", &z, &cwd, Some("clear")).await;
        assert_eq!(clear_binding.source.as_deref(), Some("clear"));
        assert!(apply_native_session_binding(
            &bindings,
            &z_found,
            &clear_binding,
            None,
            &mut session_modes,
            &mut read_anchors,
            &mut withdrawn_sessions,
            &mut active_session,
        ));
        // r12 item 7: the /clear seam records Z as a Fresh REBOUND at the seam
        // itself — a later record for the same pid — before maintain_binding
        // opens anything. Deleting the seam record must fail here even though
        // the final hydration below happens to open Fresh anyway.
        let clear_mode_at_seam = session_modes
            .mode_for(7, "clear-z")
            .expect("the /clear seam records Z's mode before maintain_binding");
        assert!(
            matches!(
                clear_mode_at_seam.mode,
                crate::claude_transcript::ResumeMode::Fresh
            ) && clear_mode_at_seam.rebound,
            "clear source classifies Z as a Fresh rebound at the seam: {clear_mode_at_seam:?}"
        );
        // The production loop arm on a rebind/first-claim: drop the previous
        // session's hydrator and its one-shot announce.
        hydrator = None;
        announced = None;
        fx.binding.session_id = "clear-z".to_owned();
        fx.binding.path = z.clone();
        maintain_binding(
            &bindings,
            &fx.ctx,
            &z_found,
            &mut hydrator,
            &mut announced,
            &fx.tx,
            &fx.seq,
            None,
            None,
            None,
            None,
            None,
            &mut session_modes,
            &mut launch_mode,
            None,
            &mut read_anchors,
            &mut effort_seeds,
            &mut withdrawn_sessions,
            &mut active_session,
        )
        .await;
        let reopened = hydrator.as_ref().expect("the /clear rebound hydrates");
        assert!(
            reopened.tail.verified(),
            "the bus-carried /clear source makes Z Fresh despite the non-empty file"
        );
        assert!(
            conversation_hydrated(&drain(&mut fx), "queued before first tick"),
            "the Fresh gate hydrates Z's queued prompt (read-back gate open)"
        );
    }

    /// c-effortread r9 item 9b: the EffortTracker latch survives a
    /// demote/re-promotion driven through the PRODUCTION maintain_binding
    /// path (not a hand-built mapper). After an ultracode-on high epoch, a
    /// demote drops the hydrator while keeping the per-(pid,session) anchor and
    /// effort seed; the re-promote's Hydrator::open re-latches from that seed,
    /// so an attachment-less assistant at a NEW level publishes
    /// {Max, ultracode:true}. Goes red if the seed stops reaching
    /// Hydrator::open (the edge comes out {Max, None}).
    #[tokio::test]
    async fn r9_item9b_tracker_latch_rides_a_maintain_binding_repromotion() {
        // Native 2.1.289 shape: an ultra_effort_enter attachment, then the
        // assistant high record rides the latched flag.
        let first_turn = format!(
            "{}{}",
            serde_json::json!({
                "type":"attachment",
                "attachment":{"type":"ultra_effort_enter","reminderType":"full"}
            })
            .to_string()
                + "\n",
            assistant_line(Some("high"), 2),
        );
        let dir = tempfile::tempdir().expect("tmp");
        let mut fx = pump_fixture(dir.path(), &first_turn);
        // pump_fixture's default ctx cwd is the tmp root; the binding slug is
        // under tmp/repo, so give the production ctx the same cwd the hydrator
        // content-membership check enforces.
        fx.ctx.cwd = dir.path().join("repo");
        let bridge = Arc::new(crate::effort::EffortBridge::new());
        let found = detected_claude(7, Some(PUMP_SESSION));

        // The exact per-tick state the promotion loop owns.
        let bindings = BindingHandle::empty();
        bindings.begin_epoch(&fx.ctx.cwd, &fx.ctx.claude_home);
        bindings.ingest_session_start(SessionStartReport {
            session_id: PUMP_SESSION.to_owned(),
            transcript_path: fx.binding.path.clone(),
            cwd: Some(fx.binding.cwd.clone()),
            ppid: Some(7),
            start_source: Some("startup".into()),
        });
        let mut hydrator: Option<Hydrator> = None;
        let mut announced: Option<String> = None;
        let mut session_modes = SessionModeTable::default();
        let mut launch_mode = LaunchModeBinding::Unbound;
        let mut read_anchors = std::collections::HashMap::new();
        let mut effort_seeds = std::collections::HashMap::new();
        let mut withdrawn_sessions = std::collections::HashSet::new();
        let mut active_session: Option<(i32, String)> = None;

        // First promotion tick: maintain_binding opens the Fresh hydrator with
        // the bridge and pumps the ultra-on high turn, capturing the read
        // anchor and effort seed exactly as the production loop does.
        maintain_binding(
            &bindings,
            &fx.ctx,
            &found,
            &mut hydrator,
            &mut announced,
            &fx.tx,
            &fx.seq,
            Some(&bridge),
            None,
            None,
            None,
            None,
            &mut session_modes,
            &mut launch_mode,
            Some(ResumeMode::Fresh),
            &mut read_anchors,
            &mut effort_seeds,
            &mut withdrawn_sessions,
            &mut active_session,
        )
        .await;
        let first = drain(&mut fx);
        assert!(
            effort_rows(&first).iter().any(|(name, flag)| *name
                == Some(remuda_protocol::EffortName::High)
                && *flag == Some(true)),
            "the first epoch publishes high + ultracode on: {:?}",
            effort_rows(&first)
        );
        assert!(
            effort_seeds.contains_key(&(7, PUMP_SESSION.to_owned())),
            "the production pump stored an effort seed for (pid, session)"
        );

        // Demote: the hydrator is dropped; the loop keeps the anchor + seed.
        hydrator = None;

        // After re-promotion the agent appends an attachment-less assistant at
        // a NEW level. The reopened hydrator must carry the latched flag.
        append_line(&fx.binding.path, &assistant_line(Some("max"), 10));

        // Re-promote through the SAME production entry point.
        maintain_binding(
            &bindings,
            &fx.ctx,
            &found,
            &mut hydrator,
            &mut announced,
            &fx.tx,
            &fx.seq,
            Some(&bridge),
            None,
            None,
            None,
            None,
            &mut session_modes,
            &mut launch_mode,
            Some(ResumeMode::Fresh),
            &mut read_anchors,
            &mut effort_seeds,
            &mut withdrawn_sessions,
            &mut active_session,
        )
        .await;
        assert!(hydrator.is_some(), "the re-promote opened a hydrator");
        let second = drain(&mut fx);
        assert!(
            effort_rows(&second).iter().any(|(name, flag)| *name
                == Some(remuda_protocol::EffortName::Max)
                && *flag == Some(true)),
            "the attachment-less max record carries the seeded ultracode flag: {:?}",
            effort_rows(&second)
        );
    }

    /// c-effortread r10 item 6 / r11 item 7: the EXACT verified-anchor
    /// withdrawal sequence, driven through production lines only (no direct
    /// `mark_session_withdrawn` / rebind calls):
    /// rename-away → a maintain_binding pump whose anchor capture FAILS
    /// (the bound file vanished) marks the withdrawal from the pump's Err arm
    /// → the loop demotes while absent → the ORIGINAL file is restored (same
    /// inode, renamed back) → re-promotion reopens UNVERIFIED, so neither the
    /// restored history nor a later current record publishes effort evidence.
    #[tokio::test]
    #[allow(unused_assignments)] // the loop-arm `hydrator/announced = None` resets mirror production
    async fn r10_item6_withdrawn_session_restored_reopens_unverified_never_cached_verified() {
        let other_session = "zzzzzzzz-2222-4333-8444-zzzzzzzzzzzz";
        let x_history = assistant_line(Some("high"), 1);
        let dir = tempfile::tempdir().expect("tmp");
        let mut fx = pump_fixture(dir.path(), &x_history);
        fx.ctx.cwd = dir.path().join("repo");
        let found = detected_claude(7, Some(PUMP_SESSION));

        let bindings = BindingHandle::empty();
        bindings.begin_epoch(&fx.ctx.cwd, &fx.ctx.claude_home);
        let mut hydrator: Option<Hydrator> = None;
        let mut announced: Option<String> = None;
        let mut session_modes = SessionModeTable::default();
        let mut launch_mode = LaunchModeBinding::Unbound;
        let mut read_anchors = std::collections::HashMap::new();
        let mut effort_seeds = std::collections::HashMap::new();
        let mut withdrawn_sessions = std::collections::HashSet::new();
        let mut active_session: Option<(i32, String)> = None;

        // Tick 1: X hydrates FRESH through the native-bus seam; the first
        // pump captures a gate-open (live) anchor and publishes high.
        let x_binding = native_binding(
            PUMP_SESSION,
            &fx.binding.path,
            &fx.binding.cwd,
            Some("startup"),
        )
        .await;
        assert!(!apply_native_session_binding(
            &bindings,
            &found,
            &x_binding,
            Some(ResumeMode::Fresh),
            &mut session_modes,
            &mut read_anchors,
            &mut withdrawn_sessions,
            &mut active_session,
        ));
        maintain_binding(
            &bindings,
            &fx.ctx,
            &found,
            &mut hydrator,
            &mut announced,
            &fx.tx,
            &fx.seq,
            None,
            None,
            None,
            None,
            None,
            &mut session_modes,
            &mut launch_mode,
            Some(ResumeMode::Fresh),
            &mut read_anchors,
            &mut effort_seeds,
            &mut withdrawn_sessions,
            &mut active_session,
        )
        .await;
        assert!(hydrator.is_some(), "X hydrates on the first tick");
        assert!(
            read_anchors
                .get(&(7, PUMP_SESSION.to_owned()))
                .is_some_and(|anchor| anchor.current),
            "the first pump captures X's gate-open live anchor"
        );
        assert!(
            effort_rows(&drain(&mut fx))
                .iter()
                .any(|(name, _)| *name == Some(remuda_protocol::EffortName::High))
        );

        // RENAME AWAY: the bound file vanishes while the hydrator is live (no
        // anchor can be captured this pump).
        let moved_aside = fx.binding.path.with_extension("jsonl.old");
        std::fs::rename(&fx.binding.path, &moved_aside).expect("rename X away");

        // The next production pumps fail to open the file. A LIVE tail
        // tolerates LIVE_MISSING_POLL_DEGRADE transient missing polls; the
        // final one degrades and its ERR arm marks the withdrawal, downgrades
        // the cached anchor and drops the hydrator.
        for missing_tick in 0..LIVE_MISSING_POLL_DEGRADE {
            maintain_binding(
                &bindings,
                &fx.ctx,
                &found,
                &mut hydrator,
                &mut announced,
                &fx.tx,
                &fx.seq,
                None,
                None,
                None,
                None,
                None,
                &mut session_modes,
                &mut launch_mode,
                Some(ResumeMode::Fresh),
                &mut read_anchors,
                &mut effort_seeds,
                &mut withdrawn_sessions,
                &mut active_session,
            )
            .await;
            if missing_tick + 1 < LIVE_MISSING_POLL_DEGRADE {
                assert!(hydrator.is_some(), "a transient miss keeps the hydrator");
                assert!(!bindings.degraded(), "one miss does not degrade yet");
            }
        }
        assert!(
            hydrator.is_none(),
            "the vanished-file arm drops the hydrator"
        );
        assert!(bindings.degraded(), "the binding is degraded while absent");
        assert!(
            withdrawn_sessions.contains(&(7, PUMP_SESSION.to_owned())),
            "the pump's failed anchor capture marks X withdrawn"
        );
        assert!(
            read_anchors
                .get(&(7, PUMP_SESSION.to_owned()))
                .is_some_and(|anchor| !anchor.current),
            "the cached anchor is downgraded by the production arm"
        );
        let _ = drain(&mut fx);

        // DEMOTE WHILE ABSENT: the exact promotion-loop demote arms.
        let now = now_ts().expect("timestamp");
        let mut promote = PromoteState::default();
        for (completeness, payload) in transition(&mut promote, Some(&found), &now) {
            emit(
                &fx.tx,
                &fx.seq,
                &fx.ctx,
                SourceChannel::Runtime,
                completeness,
                payload,
            )
            .await
            .expect("emit promote");
        }
        for (completeness, payload) in transition(&mut promote, None, &now) {
            emit(
                &fx.tx,
                &fx.seq,
                &fx.ctx,
                SourceChannel::Runtime,
                completeness,
                payload,
            )
            .await
            .expect("emit demote");
        }
        bindings.demobilize();
        hydrator = None;
        announced = None;
        let _ = drain(&mut fx);

        // A different session Z takes the foreground while X is away. The
        // re-promotion begins a fresh epoch and the authenticated Z report
        // first-claims the binding through the production seam.
        let z_path = slug_session(
            dir.path(),
            &fx.binding.cwd,
            other_session,
            &assistant_line(Some("medium"), 1),
        );
        bindings.begin_epoch(&fx.ctx.cwd, &fx.ctx.claude_home);
        let z_binding =
            native_binding(other_session, &z_path, &fx.binding.cwd, Some("resume")).await;
        assert!(apply_native_session_binding(
            &bindings,
            &found,
            &z_binding,
            None,
            &mut session_modes,
            &mut read_anchors,
            &mut withdrawn_sessions,
            &mut active_session,
        ));
        hydrator = None;
        announced = None;
        maintain_binding(
            &bindings,
            &fx.ctx,
            &found,
            &mut hydrator,
            &mut announced,
            &fx.tx,
            &fx.seq,
            None,
            None,
            None,
            None,
            None,
            &mut session_modes,
            &mut launch_mode,
            None,
            &mut read_anchors,
            &mut effort_seeds,
            &mut withdrawn_sessions,
            &mut active_session,
        )
        .await;
        assert!(hydrator.is_some(), "Z hydrates while X stays away");
        let _ = drain(&mut fx);

        // RESTORE THE ORIGINAL FILE (rename back — SAME inode and head) and
        // come back to X via another authenticated report + production seam.
        std::fs::rename(&moved_aside, &fx.binding.path).expect("restore the original X file");
        let x_resume = native_binding(
            PUMP_SESSION,
            &fx.binding.path,
            &fx.binding.cwd,
            Some("resume"),
        )
        .await;
        assert!(apply_native_session_binding(
            &bindings,
            &found,
            &x_resume,
            None,
            &mut session_modes,
            &mut read_anchors,
            &mut withdrawn_sessions,
            &mut active_session,
        ));
        hydrator = None;
        announced = None;
        maintain_binding(
            &bindings,
            &fx.ctx,
            &found,
            &mut hydrator,
            &mut announced,
            &fx.tx,
            &fx.seq,
            None,
            None,
            None,
            None,
            None,
            &mut session_modes,
            &mut launch_mode,
            None,
            &mut read_anchors,
            &mut effort_seeds,
            &mut withdrawn_sessions,
            &mut active_session,
        )
        .await;
        let reopened = hydrator.as_ref().expect("restored X reopens a hydrator");
        assert!(
            !reopened.tail.verified(),
            "the SAME-INODE restore still reopens unverified — the withdrawal is sticky"
        );

        // The restored ORIGINAL history can never publish Current evidence.
        assert!(
            !effort_rows(&drain(&mut fx))
                .iter()
                .any(|(name, _)| *name == Some(remuda_protocol::EffortName::High)),
            "restored original history replays as current"
        );

        // Even a genuinely NEW current-process record stays gated.
        append_line(&fx.binding.path, &assistant_line(Some("max"), 10));
        maintain_binding(
            &bindings,
            &fx.ctx,
            &found,
            &mut hydrator,
            &mut announced,
            &fx.tx,
            &fx.seq,
            None,
            None,
            None,
            None,
            None,
            &mut session_modes,
            &mut launch_mode,
            None,
            &mut read_anchors,
            &mut effort_seeds,
            &mut withdrawn_sessions,
            &mut active_session,
        )
        .await;
        assert!(
            !effort_rows(&drain(&mut fx))
                .iter()
                .any(|(name, _)| *name == Some(remuda_protocol::EffortName::Max)),
            "an unverified boundary never opens the effort gate"
        );
    }

    /// c-effortread r10 item 7: a HAND-TYPED `claude` (no argv session id, no
    /// Remuda pre-spawn capture) that `/resume`s into X before the first poll
    /// must honour the authenticated hook's `resume` source: X opens at an
    /// unverified EOF boundary and its prior history never publishes Current
    /// effort evidence. Driven through the production SessionStart ingestion
    /// + maintain_binding tick (not a hand-built mapper).
    #[tokio::test]
    async fn r10_item7_hand_typed_claude_resume_source_on_first_record_is_unverified_eof() {
        // X already carries a prior session's history.
        let history = assistant_line(Some("high"), 1);
        let dir = tempfile::tempdir().expect("tmp");
        let mut fx = pump_fixture(dir.path(), &history);
        fx.ctx.cwd = dir.path().join("repo");
        // Hand-typed: the process table knows NO argv session and NO resume
        // argv — this is exactly what made the old branch call the
        // argv-provenance Fresh path.
        let found = detected_claude(7, None);

        let bindings = BindingHandle::empty();
        bindings.begin_epoch(&fx.ctx.cwd, &fx.ctx.claude_home);
        // The authenticated hook report, delivered before the first poll.
        bindings.ingest_session_start(SessionStartReport {
            session_id: PUMP_SESSION.to_owned(),
            transcript_path: fx.binding.path.clone(),
            cwd: Some(fx.binding.cwd.clone()),
            ppid: Some(7),
            start_source: Some("resume".into()),
        });
        let mut hydrator: Option<Hydrator> = None;
        let mut announced: Option<String> = None;
        let mut session_modes = SessionModeTable::default();
        let mut launch_mode = LaunchModeBinding::Unbound;
        let mut read_anchors = std::collections::HashMap::new();
        let mut effort_seeds = std::collections::HashMap::new();
        let mut withdrawn_sessions = std::collections::HashSet::new();
        let mut active_session: Option<(i32, String)> = None;

        maintain_binding(
            &bindings,
            &fx.ctx,
            &found,
            &mut hydrator,
            &mut announced,
            &fx.tx,
            &fx.seq,
            None,
            None,
            None,
            None,
            None,
            &mut session_modes,
            &mut launch_mode,
            None,
            &mut read_anchors,
            &mut effort_seeds,
            &mut withdrawn_sessions,
            &mut active_session,
        )
        .await;
        let hydrator = hydrator
            .as_ref()
            .expect("the resume binds and opens a hydrator");
        assert!(
            !hydrator.tail.verified(),
            "a resume source with no launch-session provenance is unverified"
        );

        // The prior history hydrates/lands but never as CURRENT evidence.
        let events = {
            let mut out = Vec::new();
            while let Ok(observation) = fx.rx.try_recv() {
                out.push(observation);
            }
            out
        };
        assert!(
            !effort_rows(&events)
                .iter()
                .any(|(name, _)| *name == Some(remuda_protocol::EffortName::High)),
            "resumed history published as current effort: {:?}",
            effort_rows(&events)
        );

        // A genuinely new record stays gated too.
        let mut again: Option<Hydrator> = None;
        let mut again_announced: Option<String> = None;
        append_line(&fx.binding.path, &assistant_line(Some("max"), 2));
        maintain_binding(
            &bindings,
            &fx.ctx,
            &found,
            &mut again,
            &mut again_announced,
            &fx.tx,
            &fx.seq,
            None,
            None,
            None,
            None,
            None,
            &mut session_modes,
            &mut launch_mode,
            None,
            &mut read_anchors,
            &mut effort_seeds,
            &mut withdrawn_sessions,
            &mut active_session,
        )
        .await;
        let later = {
            let mut out = Vec::new();
            while let Ok(observation) = fx.rx.try_recv() {
                out.push(observation);
            }
            out
        };
        assert!(
            !effort_rows(&later)
                .iter()
                .any(|(name, _)| *name == Some(remuda_protocol::EffortName::Max)),
            "the unverified boundary never opens the gate: {:?}",
            effort_rows(&later)
        );
    }

    /// c-effortread r10 item 7 control: a hand-typed `claude` with a
    /// `startup` source stays Fresh and DOES publish the first record.
    #[tokio::test]
    async fn r10_item7_hand_typed_claude_startup_source_stays_fresh() {
        let first_turn = assistant_line(Some("high"), 1);
        let dir = tempfile::tempdir().expect("tmp");
        let mut fx = pump_fixture(dir.path(), &first_turn);
        fx.ctx.cwd = dir.path().join("repo");
        let found = detected_claude(7, None);
        let bindings = BindingHandle::empty();
        bindings.begin_epoch(&fx.ctx.cwd, &fx.ctx.claude_home);
        bindings.ingest_session_start(SessionStartReport {
            session_id: PUMP_SESSION.to_owned(),
            transcript_path: fx.binding.path.clone(),
            cwd: Some(fx.binding.cwd.clone()),
            ppid: Some(7),
            start_source: Some("startup".into()),
        });
        let mut hydrator: Option<Hydrator> = None;
        let mut announced: Option<String> = None;
        let mut session_modes = SessionModeTable::default();
        let mut launch_mode = LaunchModeBinding::Unbound;
        let mut read_anchors = std::collections::HashMap::new();
        let mut effort_seeds = std::collections::HashMap::new();
        let mut withdrawn_sessions = std::collections::HashSet::new();
        let mut active_session: Option<(i32, String)> = None;
        maintain_binding(
            &bindings,
            &fx.ctx,
            &found,
            &mut hydrator,
            &mut announced,
            &fx.tx,
            &fx.seq,
            None,
            None,
            None,
            None,
            None,
            &mut session_modes,
            &mut launch_mode,
            None,
            &mut read_anchors,
            &mut effort_seeds,
            &mut withdrawn_sessions,
            &mut active_session,
        )
        .await;
        let events = {
            let mut out = Vec::new();
            while let Ok(observation) = fx.rx.try_recv() {
                out.push(observation);
            }
            out
        };
        assert!(
            effort_rows(&events)
                .iter()
                .any(|(name, _)| *name == Some(remuda_protocol::EffortName::High)),
            "a startup source is the fresh current process and its record publishes: {:?}",
            effort_rows(&events)
        );
    }

    /// c-effortread r12 item 6 (was r11 item 6): a hook-derived session id is
    /// SESSION IDENTITY, not launch provenance. A hand-typed agent whose
    /// process table row normal `detect` CANNOT RECOGNIZE (a node wrapper:
    /// `node /opt/claude/cli.js` — argv0 `node` is no agent basename) is
    /// promoted ONLY through the authenticated SessionStart hook fallback. Its
    /// user `/resume`d X before the first poll (source resume), so it must open
    /// X at an unverified EOF boundary: X's historical effort/verdict records
    /// neither publish as current nor settle an armed switch.
    ///
    /// This drives the SAME orchestration the poll loop runs: detect() comes
    /// up empty → promote_from_hook_binding → apply_native_session_binding
    /// (classify → authenticated first-claim/rebind arm → ingest) →
    /// maintain_binding/promotion. The old test hand-fed argv `claude` (which
    /// detect recognises, so the production fallback guard never fires) and
    /// ingested the SessionStartReport by hand, bypassing the seam.
    #[tokio::test]
    async fn r12_item6_hook_only_identity_through_the_production_fallback_and_seam() {
        use crate::effort::EffortRequest;
        use remuda_protocol::EffortAxis::Tier;
        use remuda_protocol::EffortSemantics::Decoupled;

        // X's prior history: a high read-back, then a /effort max slash and
        // its accepted verdict — the exact pair that settles an armed max
        // request when the boundary is wrongly Fresh.
        let history = format!(
            "{}{}{}",
            assistant_line(Some("high"), 1),
            slash_line("max", 2),
            stdout_verdict(
                "Set effort level to max (this session only): Maximum capability",
                3,
            ),
        );
        let dir = tempfile::tempdir().expect("tmp");
        let mut fx = pump_fixture(dir.path(), &history);
        fx.ctx.cwd = dir.path().join("repo");

        // Native hook bus: exactly the relay payload, source folded to resume.
        let (tx, _rx) = mpsc::channel(256);
        let bus = remuda_signal::SignalBus::new(
            remuda_signal::BusContext {
                instance_id: InstanceId::new(),
                host_id: HostId::new(),
                journal_id: Id::new("obj").expect("journal id"),
                run_id: RunId::new(),
                driver_kind: remuda_protocol::DriverKind::ShellPty,
                adapter_version: "test".into(),
            },
            tx,
            Arc::new(AtomicU64::new(0)),
        );
        bus.handle(remuda_signal::HookEnvelope {
            credential: "cred".into(),
            event: "SessionStart".into(),
            ppid: 7,
            payload: serde_json::json!({
                "session_id": PUMP_SESSION,
                "transcript_path": fx.binding.path.to_string_lossy(),
                "cwd": fx.binding.cwd.to_string_lossy(),
                "source": "resume",
            }),
        })
        .await;
        let binding = bus.binding().expect("the native bus folded a binding");
        assert_eq!(binding.source.as_deref(), Some("resume"));

        // The foreground group the poller just sampled: a node-launched agent
        // whose argv0 is `node`, which detect() cannot match — so the hook
        // fallback is genuinely REQUIRED (the production guard at the
        // found.is_none() branch), never a row detect would already claim.
        let rows = vec![ProcessRow {
            pid: 7,
            args: "node /opt/claude/cli.js".into(),
        }];
        assert!(
            crate::promote::detect(&rows, None).is_none(),
            "a node-wrapper argv must not be claimed by normal detection"
        );
        let found = promote_from_hook_binding(&rows, &binding, AgentKind::Claude)
            .expect("the authenticated hook pid in the foreground group promotes");
        assert_eq!(
            found.pid, binding.pid,
            "the production found==binding guard"
        );
        assert_eq!(found.session_id.as_deref(), Some(PUMP_SESSION));
        assert!(!found.resume, "the wrapper argv carries no resume flag");
        assert!(
            found.session_id_from_hook,
            "the session identity is learned from the hook, not argv"
        );
        assert!(found.hydrates_transcript);

        // A live configure request is already armed when the mapper replays X.
        let bridge = Arc::new(crate::effort::EffortBridge::new());
        let armed = EffortRequest::for_configure(
            "max",
            Some(false),
            Some(Tier),
            Decoupled,
            None,
            None,
            Some("req-armed"),
        )
        .expect("max tier request");
        bridge.arm(armed);
        assert!(bridge.has_pending());

        let bindings = BindingHandle::empty();
        bindings.begin_epoch(&fx.ctx.cwd, &fx.ctx.claude_home);
        let mut hydrator: Option<Hydrator> = None;
        let mut announced: Option<String> = None;
        let mut session_modes = SessionModeTable::default();
        let mut launch_mode = LaunchModeBinding::Unbound;
        let mut read_anchors = std::collections::HashMap::new();
        let mut effort_seeds = std::collections::HashMap::new();
        let mut withdrawn_sessions = std::collections::HashSet::new();
        let mut active_session: Option<(i32, String)> = None;

        // The production seam (poll loop ~1057): classify the bus-derived
        // report, arm the authenticated first-claim/rebind, and ingest. No
        // hand-built report and no manual ingest_session_start. The first
        // bind of a hook-only identity is an ingest (false), exactly as the
        // loop sees it.
        let rebound = apply_native_session_binding(
            &bindings,
            &found,
            &binding,
            None,
            &mut session_modes,
            &mut read_anchors,
            &mut withdrawn_sessions,
            &mut active_session,
        );
        assert!(
            !rebound,
            "the first hook-only bind ingests, it does not rebind"
        );

        // The classified boundary is Unverified IMMEDIATELY after the seam:
        // a hook-only resume identity never wins Fresh launch provenance. A
        // non-empty transcript classifies as an unverified EOF Boundary
        // (verified=false), an empty/missing one as bare Unverified — both
        // anchor at current EOF with the effort gate closed.
        let classified = session_modes
            .mode_for(7, PUMP_SESSION)
            .expect("the seam recorded one mode for the hook (pid, session)");
        let unverified_boundary = match &classified.mode {
            crate::claude_transcript::ResumeMode::Unverified => true,
            crate::claude_transcript::ResumeMode::Boundary(boundary) => !boundary.verified,
            crate::claude_transcript::ResumeMode::Fresh => false,
        };
        assert!(
            !classified.rebound && unverified_boundary,
            "the resume-source hook-only identity is classified unverified: {classified:?}"
        );

        // The loop only resets the hydrator on a real rebind/first-claim.
        if rebound {
            hydrator = None;
            announced = None;
        }
        maintain_binding(
            &bindings,
            &fx.ctx,
            &found,
            &mut hydrator,
            &mut announced,
            &fx.tx,
            &fx.seq,
            Some(&bridge),
            None,
            None,
            None,
            None,
            &mut session_modes,
            &mut launch_mode,
            None,
            &mut read_anchors,
            &mut effort_seeds,
            &mut withdrawn_sessions,
            &mut active_session,
        )
        .await;
        let hydrator = hydrator.expect("the hook identity binds and opens a hydrator");
        assert!(
            !hydrator.tail.verified(),
            "a hook-only resume identity opens an unverified EOF boundary, not Fresh"
        );

        let events = drain(&mut fx);
        assert!(
            effort_rows(&events).is_empty(),
            "X's historical high/max records never publish current effort: {:?}",
            effort_rows(&events)
        );
        let pending = bridge
            .pending()
            .expect("the historical slash/verdict cannot settle the armed request");
        assert_eq!(pending.request_id.as_deref(), Some("req-armed"));
        assert_eq!(pending.name, remuda_protocol::EffortName::Max);
    }

    /// c-effortread r13 item 5: a hook-only detected Claude process reports
    /// historical session X with explicit source "resume" while X's transcript
    /// is temporarily ABSENT (rename-and-restore). The sticky mode captured at
    /// that instant must be bare Unverified — never Fresh — and it must STAY
    /// unverified after X is restored, so X's historical effort cannot publish
    /// as current or settle the armed request.
    ///
    /// Same production orchestration as the r12 item 6 case: native hook bus
    /// → promote_from_hook_binding → apply_native_session_binding (the seam
    /// classifies the sticky mode) → file restored → maintain_binding opens
    /// the hydrator and pumps.
    #[tokio::test]
    async fn r13_item5_resume_while_transcript_absent_stays_unverified_after_restore() {
        use crate::effort::EffortRequest;
        use remuda_protocol::EffortAxis::Tier;
        use remuda_protocol::EffortSemantics::Decoupled;

        // X's prior history: a high read-back, then a /effort max slash and
        // its accepted verdict — the pair that settles an armed max request
        // once a Fresh tail replays it.
        let history = format!(
            "{}{}{}",
            assistant_line(Some("high"), 1),
            slash_line("max", 2),
            stdout_verdict(
                "Set effort level to max (this session only): Maximum capability",
                3,
            ),
        );
        let dir = tempfile::tempdir().expect("tmp");
        let mut fx = pump_fixture(dir.path(), &history);
        fx.ctx.cwd = dir.path().join("repo");

        // The transcript is renamed AWAY between the last poll and the
        // SessionStart delivery (the rename-and-restore window).
        std::fs::remove_file(&fx.binding.path).expect("X is absent at the hook instant");

        // Native hook bus: the real relay payload, source resume, naming X.
        let (tx, _rx) = mpsc::channel(256);
        let bus = remuda_signal::SignalBus::new(
            remuda_signal::BusContext {
                instance_id: InstanceId::new(),
                host_id: HostId::new(),
                journal_id: Id::new("obj").expect("journal id"),
                run_id: RunId::new(),
                driver_kind: remuda_protocol::DriverKind::ShellPty,
                adapter_version: "test".into(),
            },
            tx,
            Arc::new(AtomicU64::new(0)),
        );
        bus.handle(remuda_signal::HookEnvelope {
            credential: "cred".into(),
            event: "SessionStart".into(),
            ppid: 7,
            payload: serde_json::json!({
                "session_id": PUMP_SESSION,
                "transcript_path": fx.binding.path.to_string_lossy(),
                "cwd": fx.binding.cwd.to_string_lossy(),
                "source": "resume",
            }),
        })
        .await;
        let binding = bus.binding().expect("the native bus folded a binding");
        assert_eq!(binding.source.as_deref(), Some("resume"));

        // The node-wrapper foreground row forces the hook fallback (the guard
        // normal detect cannot claim it), exactly as in production.
        let rows = vec![ProcessRow {
            pid: 7,
            args: "node /opt/claude/cli.js".into(),
        }];
        assert!(crate::promote::detect(&rows, None).is_none());
        let found = promote_from_hook_binding(&rows, &binding, AgentKind::Claude)
            .expect("the authenticated hook pid promotes");
        assert_eq!(found.session_id.as_deref(), Some(PUMP_SESSION));
        assert!(found.session_id_from_hook);

        let bindings = BindingHandle::empty();
        bindings.begin_epoch(&fx.ctx.cwd, &fx.ctx.claude_home);
        let mut hydrator: Option<Hydrator> = None;
        let mut announced: Option<String> = None;
        let mut session_modes = SessionModeTable::default();
        let mut launch_mode = LaunchModeBinding::Unbound;
        let mut read_anchors = std::collections::HashMap::new();
        let mut effort_seeds = std::collections::HashMap::new();
        let mut withdrawn_sessions = std::collections::HashSet::new();
        let mut active_session: Option<(i32, String)> = None;

        // The production seam classifies the mode WHILE X IS ABSENT.
        let rebound = apply_native_session_binding(
            &bindings,
            &found,
            &binding,
            None,
            &mut session_modes,
            &mut read_anchors,
            &mut withdrawn_sessions,
            &mut active_session,
        );
        assert!(
            !rebound,
            "the first hook-only bind ingests, it does not rebind"
        );
        let classified = session_modes
            .mode_for(7, PUMP_SESSION)
            .expect("the seam recorded one mode for the hook (pid, session)");
        assert!(
            matches!(
                classified.mode,
                crate::claude_transcript::ResumeMode::Unverified
            ),
            "explicit resume provenance with the transcript absent is bare Unverified, never Fresh: {:?}",
            classified.mode
        );

        // X is restored (rename back) WITH its prior history before the next
        // poll tick opens the hydrator.
        std::fs::write(&fx.binding.path, history.as_bytes()).expect("X restored");

        // A live configure request is armed before the restored file replays.
        let bridge = Arc::new(crate::effort::EffortBridge::new());
        let armed = EffortRequest::for_configure(
            "max",
            Some(false),
            Some(Tier),
            Decoupled,
            None,
            None,
            Some("req-armed"),
        )
        .expect("max tier request");
        bridge.arm(armed);
        assert!(bridge.has_pending());

        maintain_binding(
            &bindings,
            &fx.ctx,
            &found,
            &mut hydrator,
            &mut announced,
            &fx.tx,
            &fx.seq,
            Some(&bridge),
            None,
            None,
            None,
            None,
            &mut session_modes,
            &mut launch_mode,
            None,
            &mut read_anchors,
            &mut effort_seeds,
            &mut withdrawn_sessions,
            &mut active_session,
        )
        .await;
        let hydrator = hydrator.expect("the restored X binds and opens a hydrator");
        assert!(
            !hydrator.tail.verified(),
            "the sticky Unverified mode survives the restore: an unverified EOF anchor, not Fresh"
        );

        let events = drain(&mut fx);
        assert!(
            effort_rows(&events).is_empty(),
            "X's restored historical high/max records never publish current effort: {:?}",
            effort_rows(&events)
        );
        let pending = bridge
            .pending()
            .expect("the restored historical slash/verdict cannot settle the armed request");
        assert_eq!(pending.request_id.as_deref(), Some("req-armed"));
        assert_eq!(pending.name, remuda_protocol::EffortName::Max);
    }

    /// r6 item 3 (the lazy-create case): a fresh launch has no transcript and
    /// no binding yet; the user /resume's an older session before typing. The
    /// session recorded SECOND for the pid is an unverified rebound: the older
    /// history never replays as current and its old slash/verdict cannot
    /// settle an armed switch.
    #[tokio::test]
    async fn item3_rebind_before_any_binding_is_unverified_via_the_hook_table() {
        let history = format!(
            "{}{}{}",
            assistant_line(Some("high"), 1),
            slash_line("max", 2),
            stdout_verdict("Set effort level to max", 3),
        );
        let dir = tempfile::tempdir().expect("tmp");
        let mut fx = pump_fixture(dir.path(), &history);
        // The poller's hook block: first SessionStart is the launched (new,
        // transcript-less) session; the SECOND is the in-TUI /resume target.
        let mut table = SessionModeTable::default();
        let launched_found = detected_claude(7, Some(BUSY_OTHER));
        let _launched = table.record(7, BUSY_OTHER, None, None, None, &launched_found);
        let rebound = table.record(
            7,
            PUMP_SESSION,
            Some(&fx.binding.path),
            None,
            None,
            &launched_found,
        );
        assert!(rebound.rebound, "the bound session is a rebound");
        let bridge = std::sync::Arc::new(crate::effort::EffortBridge::new());
        let mut hydrator = open_hydrator(&fx, rebound.mode, Some(&bridge));
        pump_once(&mut hydrator, &fx).await;
        assert!(drain(&mut fx).is_empty(), "no history replays as current");
        let generation = bridge.arm(crate::effort::EffortRequest::from_level("max").expect("max"));
        append_line(
            &fx.binding.path,
            &format!(
                "{}{}",
                slash_line("max", 4),
                stdout_verdict("Set effort level to max", 5),
            ),
        );
        pump_once(&mut hydrator, &fx).await;
        assert!(effort_rows(&drain(&mut fx)).is_empty(), "gate stays closed");
        assert!(bridge.has_pending());
        assert!(
            bridge
                .wait(generation, std::time::Duration::from_millis(150))
                .await
                .is_none(),
            "replayed verdict must not settle Applied"
        );
        append_line(&fx.binding.path, &user_line(6, "after rebind"));
        pump_once(&mut hydrator, &fx).await;
        assert!(conversation_hydrated(&drain(&mut fx), "after rebind"));
    }

    /// r6 item 4: `/clear` reports a SessionStart while the new transcript is
    /// ABSENT (lazy creation); the mode is Fresh. The file only APPEARS later,
    /// already carrying the new session's first records — they are read from
    /// byte 0 as Current and settle switches (the empty-file hand fixture in
    /// r5 never exercised this production path).
    #[tokio::test]
    async fn item4_clear_session_report_before_its_file_exists_is_fresh() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut fx = pump_fixture(dir.path(), "");
        std::fs::remove_file(&fx.binding.path).expect("file does not exist at SessionStart");
        let mut table = SessionModeTable::default();
        let launched_found = detected_claude(7, Some(BUSY_OTHER));
        // First report is the launched session; the second (same pid, new
        // session id) is /clear, recorded while its file is absent.
        let _launched = table.record(7, BUSY_OTHER, None, None, None, &launched_found);
        let clear = table.record(
            7,
            PUMP_SESSION,
            Some(&fx.binding.path),
            None,
            None,
            &launched_found,
        );
        assert_eq!(clear.mode, ResumeMode::Fresh);
        // The file then appears WITH content (Claude created it lazily).
        append_line(
            &fx.binding.path,
            &format!(
                "{}{}{}",
                slash_line("max", 1),
                stdout_verdict("Set effort level to max", 2),
                assistant_line(Some("max"), 3),
            ),
        );
        let bridge = std::sync::Arc::new(crate::effort::EffortBridge::new());
        let mut hydrator = open_hydrator(&fx, clear.mode, Some(&bridge));
        let generation = bridge.arm(crate::effort::EffortRequest::from_level("max").expect("max"));
        pump_once(&mut hydrator, &fx).await;
        let observations = drain(&mut fx);
        assert_eq!(
            effort_rows(&observations),
            vec![(Some(remuda_protocol::EffortName::Max), None)],
            "records created after the absent-file report are current: {observations:?}"
        );
        assert!(
            bridge
                .wait(generation, std::time::Duration::from_millis(150))
                .await
                .is_some(),
            "the new session settles its own switch"
        );
    }

    /// r7 item 2: argv provenance, not poll-time file size, decides a pid's
    /// FIRST session. A fresh launch whose transcript already holds the
    /// initial prompt turn when its first SessionStart is ingested stays
    /// Fresh: the gate opens and the turn's effort read-back publishes.
    #[tokio::test]
    async fn item2_nonempty_first_session_with_a_fresh_argv_stays_fresh() {
        // instance.create with an initial prompt: the pty queue delivers it
        // before the first 800 ms tick, so the transcript already carries the
        // whole first turn (slash + verdict + assistant read-back) at ingest.
        let first_turn = format!(
            "{}{}{}",
            slash_line("max", 1),
            stdout_verdict("Set effort level to max", 2),
            assistant_line(Some("max"), 3),
        );
        let dir = tempfile::tempdir().expect("tmp");
        let mut fx = pump_fixture(dir.path(), &first_turn);
        let found = detected_claude(7, Some(PUMP_SESSION));
        let mut table = SessionModeTable::default();
        let first = table.record(7, PUMP_SESSION, Some(&fx.binding.path), None, None, &found);
        assert!(!first.rebound);
        assert_eq!(first.mode, ResumeMode::Fresh, "a fresh argv is Fresh");
        let mut launch = LaunchModeBinding::Unbound;
        let mode = epoch_mode(&mut launch, None, Some(first), &found);
        assert_eq!(mode, ResumeMode::Fresh);
        let bridge = std::sync::Arc::new(crate::effort::EffortBridge::new());
        let generation = bridge.arm(crate::effort::EffortRequest::from_level("max").expect("max"));
        let mut hydrator = open_hydrator(&fx, mode, Some(&bridge));
        pump_once(&mut hydrator, &fx).await;
        let observations = drain(&mut fx);
        assert_eq!(
            effort_rows(&observations),
            vec![(Some(remuda_protocol::EffortName::Max), None)],
            "the pre-tick turn is current: the gate opens and read-back publishes: {observations:?}"
        );
        assert!(
            bridge
                .wait(generation, std::time::Duration::from_millis(150))
                .await
                .is_some(),
            "the initial prompt's own verdict settles Applied"
        );
    }

    /// r7 item 2 unit: resume argv → unverified however the file looks;
    /// fresh argv → Fresh even with a non-empty file; size never decides.
    #[test]
    fn item2_first_session_mode_follows_argv_provenance_not_file_size() {
        let tmp = tempfile::tempdir().expect("tmp");
        let non_empty = tmp.path().join("started.jsonl");
        write(&non_empty, "{\"already\":\"here\"}\n");
        let absent = tmp.path().join("never.jsonl");
        let resume = Detected {
            resume: true,
            ..detected_claude(7, Some(PUMP_SESSION))
        };
        let fresh = Detected {
            resume: false,
            ..detected_claude(7, Some(PUMP_SESSION))
        };
        // Fresh argv: size is irrelevant.
        assert_eq!(
            first_session_mode(&fresh, Some(&non_empty)),
            ResumeMode::Fresh
        );
        assert_eq!(first_session_mode(&fresh, Some(&absent)), ResumeMode::Fresh);
        assert_eq!(first_session_mode(&fresh, None), ResumeMode::Fresh);
        // Resume argv, file present: an unverified EOF anchor (never Fresh).
        match first_session_mode(&resume, Some(&non_empty)) {
            ResumeMode::Boundary(boundary) => assert!(!boundary.verified),
            other => panic!("expected unverified boundary, got {other:?}"),
        }
        // Resume argv, file still absent: bare Unverified so the pump retries
        // rather than opening a byte-0 live tail when the file appears.
        assert_eq!(
            first_session_mode(&resume, Some(&absent)),
            ResumeMode::Unverified
        );
        assert_eq!(first_session_mode(&resume, None), ResumeMode::Unverified);
    }

    /// r7 item 4(c): a demote/re-promotion of the SAME (pid, session) reopens
    /// its tail at the last read offset, so the conversation already
    /// published before the demote is not re-emitted (no duplicated effort
    /// edge) and only bytes appended after the demote hydrate.
    #[tokio::test]
    async fn item4c_repromotion_continues_at_the_last_read_offset() {
        // r7 item 4(c), c-effortread r9 item 9d: the demote/re-promotion now
        // runs through maintain_binding (production bind/open/pump), not a
        // direct Hydrator::open. A Fresh session publishes one turn (incl. its
        // high read-back); after the demote only bytes appended following the
        // re-promote hydrate, and the pre-demote effort/conversation is not
        // re-emitted.
        let first_turn = format!(
            "{}{}",
            user_line(1, "first question"),
            assistant_line(Some("high"), 2),
        );
        let dir = tempfile::tempdir().expect("tmp");
        let mut fx = pump_fixture(dir.path(), &first_turn);
        fx.ctx.cwd = dir.path().join("repo");
        let mut state = maintain_state(&fx, false);

        // First promotion tick reads the turn as current.
        maintain_tick(&mut state, &fx, Some(ResumeMode::Fresh)).await;
        let first = drain(&mut fx);
        assert_eq!(
            effort_rows(&first),
            vec![(Some(remuda_protocol::EffortName::High), None)],
            "first epoch reads the turn as current"
        );
        assert!(conversation_hydrated(&first, "first question"));
        assert!(
            state
                .read_anchors
                .contains_key(&(7, PUMP_SESSION.to_owned())),
            "the poll loop captured a read anchor for the re-promotion"
        );

        // The agent demotes: the loop drops the hydrator but keeps the anchor.
        state.hydrator = None;
        append_line(&fx.binding.path, &user_line(3, "after re-promotion"));

        // Re-promotion reopens through the SAME production entry point.
        maintain_tick(&mut state, &fx, Some(ResumeMode::Fresh)).await;
        let second = drain(&mut fx);
        assert!(
            effort_rows(&second).is_empty(),
            "the pre-demote high read-back must not be re-emitted: {second:?}"
        );
        assert!(
            !conversation_hydrated(&second, "first question"),
            "pre-demote conversation must not re-emit: {second:?}"
        );
        assert!(
            conversation_hydrated(&second, "after re-promotion"),
            "only the post-demote record hydrates: {second:?}"
        );

        // An anchor against a DIFFERENT identity (the file was replaced) does
        // not continue; the caller falls back to the mode's own open.
        let anchor = state
            .read_anchors
            .get(&(7, PUMP_SESSION.to_owned()))
            .cloned()
            .expect("anchor held across the demote");
        std::fs::remove_file(&fx.binding.path).unwrap();
        std::fs::write(
            &fx.binding.path,
            format!(
                "{}{}",
                user_line(4, "recreated"),
                assistant_line(Some("max"), 5)
            ),
        )
        .unwrap();
        assert!(
            TranscriptTail::continued(fx.binding.path.clone(), anchor).is_none(),
            "a replaced file invalidates the captured anchor"
        );
    }

    /// r6 item 5: an unverified anchor captured at the SessionStart instant is
    /// used AS-IS — records appended between that instant and the first pump
    /// are read from the anchor and hydrated (Unverified), not skipped by a
    /// second EOF snapshot.
    #[tokio::test]
    async fn item5_records_between_report_and_first_poll_hydrate_unverified() {
        let history = "{\"before\":true}\n";
        let dir = tempfile::tempdir().expect("tmp");
        let mut fx = pump_fixture(dir.path(), history);
        // Capture the unverified boundary at THIS instant (EOF after history).
        let anchor = crate::claude_transcript::ResumeBoundary::unverified_eof(&fx.binding.path)
            .expect("anchor");
        let anchored = crate::claude_transcript::ResumeMode::Boundary(anchor);
        // A record arrives BEFORE the hydrator first polls (the promotion loop
        // awaits between detection and maintain_binding).
        append_line(
            &fx.binding.path,
            &user_line(1, "between report and first poll"),
        );
        let mut hydrator = open_hydrator(&fx, anchored, None);
        pump_once(&mut hydrator, &fx).await;
        let observations = drain(&mut fx);
        assert!(
            effort_rows(&observations).is_empty(),
            "unverified records never open the gate"
        );
        assert!(
            conversation_hydrated(&observations, "between report and first poll"),
            "the between-instant record hydrates instead of being skipped: {observations:?}"
        );
    }
}
