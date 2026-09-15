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
use crate::claude_print::TranscriptMapper;
use crate::claude_pty::now_ts;
use crate::claude_transcript::{
    SessionStartReport, TranscriptBinding, TranscriptCandidate, TranscriptTail, bind_by_pid_file,
    bind_by_session_id, bind_manual, cwd_matches, list_candidates, recorded_cwd,
    transcript_belongs_to_cwd,
};
use crate::error::{DriverError, DriverResult};
use crate::promote::{Detected, ProcessTable, ScreenStatus, detect, foreground_pgid};
use remuda_protocol::{
    AgentKind, Completeness, DeadlineSource, DeliveryState, EntityLifecycle, EntityMeta, EventId,
    HostId, Id, InstanceId, Interaction, InteractionAnswer, InteractionCarrier, InteractionId,
    InteractionKind, InteractionRequest, InteractionRequestKey, InteractionRequestedPayload,
    InteractionState, Knowledge, LifecycleEntity, LifecyclePayload, LifecycleTopic,
    NativeLifecycle, NativeRequestKey, Observation, ObservationPayload, ObservationSource,
    QuestionField, QuestionInput, QuestionOption, QuestionRequest, RunId, RuntimeCursor,
    SchemaVersion, Severity, SourceChannel, SourceCursor, SourceDelivery, Timestamp, U64,
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
            if !duplicate {
                slot.hooks.push(report);
            }
        }
    }

    /// Current locked binding, if any.
    fn binding(&self) -> Option<TranscriptBinding> {
        self.inner.lock().ok().and_then(|slot| slot.binding.clone())
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
    interrupt_pid: Arc<AtomicI32>,
    interrupt_screen_markers: Arc<Mutex<InterruptBaseline>>,
    events: mpsc::Sender<Observation>,
    seq: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut promote = PromoteState::default();
        let mut hydrator: Option<Hydrator> = None;
        // Raise-only + blocked latch across the whole promoted session
        // (design §2.4 rule 6, §10 anchor ④). The stateless screen read feeds
        // it; its verdict is what we both journal and expose via the status
        // slot. Reset on every demotion below.
        let mut screen_latch = remuda_screen::ScreenLatch::new();
        // Sticky hook-tier health for this promoted epoch. A transient
        // `ps` miss (found == None) or a hook relay child taking the sampled
        // row must not read as "no hook tier" and release the raise: the
        // SessionStart/turn binding, once observed for this foreground pid,
        // stays live until that pid demotes or a hook confirms the turn end.
        let mut hook_ever_live = false;
        let mut last_status: Option<ScreenStatus> = None;
        // Last announced binding state, deduping lifecycle emission:
        // None = nothing announced yet this epoch.
        let mut announced: Option<String> = None;
        let mut bypass_announced = false;
        let mut tick = tokio::time::interval(PROMOTE_POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if state.closed.load(Ordering::SeqCst) || events.is_closed() {
                break;
            }
            let found = sample(&state, table.as_ref()).await;
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
                hydrator = None;
                announced = None;
            }
            if saw_promote {
                bindings.begin_epoch(&ctx.cwd, &ctx.claude_home);
                hydrator = None;
                announced = None;
                bypass_announced = false;
            }

            // The socket's SessionStart is also channel A for transcript
            // binding. Without this bridge only Claude's separate pid file
            // could hydrate, leaving a spurious manual picker over a hooked
            // session. begin_epoch must happen first so it cannot erase it.
            if let Some(binding) = hooks.as_ref().and_then(|hooks| hooks.binding())
                && found.as_ref().is_some_and(|found| found.pid == binding.pid)
                && let Some(path) = binding.transcript_path
            {
                bindings.ingest_session_start(SessionStartReport {
                    session_id: binding.session_id,
                    transcript_path: path.into(),
                    cwd: None,
                    ppid: Some(i64::from(binding.pid)),
                });
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
            let turn_state = match (found.as_ref(), hooks.as_ref()) {
                (Some(found), Some(hooks)) if found.pid > 0 => hooks.turn_active(found.pid),
                _ => None,
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
            let hook_health = match turn_state {
                Some(true) => remuda_screen::HookHealth::Healthy,
                Some(false) => remuda_screen::HookHealth::NeverMaterialised,
                None if hook_ever_live => remuda_screen::HookHealth::Healthy,
                None => remuda_screen::HookHealth::NeverMaterialised,
            };
            let status = screen_latch.update(&grid, hook_health);
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

            match (&promote.kind, &found) {
                // Claude is the only kind that hydrates a transcript in the MVP.
                (Some(AgentKind::Claude), Some(found)) if found.hydrates_transcript => {
                    maintain_binding(
                        &bindings,
                        &ctx,
                        found,
                        &mut hydrator,
                        &mut announced,
                        &events,
                        &seq,
                    )
                    .await;
                }
                _ => hydrator = None,
            }
            if events.is_closed() {
                return;
            }
        }
    })
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

/// One tick of the deterministic binding state machine for a promoted Claude.
async fn maintain_binding(
    bindings: &BindingHandle,
    ctx: &PromoteCtx,
    found: &Detected,
    hydrator: &mut Option<Hydrator>,
    announced: &mut Option<String>,
    events: &mpsc::Sender<Observation>,
    seq: &AtomicU64,
) {
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
        hydrator.take();
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
        *hydrator = Hydrator::open(ctx, &binding);
    }
    if let Some(active) = hydrator.as_mut() {
        match pump(active, events, seq, ctx).await {
            Ok(()) => {}
            Err(()) => {
                // The bound file vanished: degrade, never silently rebind.
                bindings.mark_degraded("bound transcript file vanished");
                hydrator.take();
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

/// One detection sample: foreground process group first, screen as fallback.
async fn sample(state: &PtyState, table: &dyn ProcessTable) -> Option<Detected> {
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
        return detect(&table.process_group(pgid));
    }
    remuda_screen::detect_from_screen(&state.screen_grid()).map(|kind| Detected {
        kind,
        pid: 0,
        session_id: None,
        hydrates_transcript: kind == AgentKind::Claude,
    })
}

/// A bound transcript plus the mapper replaying it.
struct Hydrator {
    tail: TranscriptTail,
    mapper: TranscriptMapper,
}

impl Hydrator {
    /// Start replay at byte 0 of the bound transcript.
    fn open(ctx: &PromoteCtx, binding: &TranscriptBinding) -> Option<Self> {
        tracing::info!(
            instance_id = %ctx.instance_id.as_id(),
            session = %binding.session_id,
            source = binding.source.as_wire(),
            "hydrating promoted terminal from its bound Claude transcript"
        );
        Some(Self {
            mapper: TranscriptMapper::new(
                remuda_protocol::DriverKind::ShellPty,
                ctx.instance_id.clone(),
                ctx.run_id.clone(),
                ctx.journal_id.clone(),
                ctx.host_id.clone(),
                binding.session_id.clone(),
                "promoted".to_owned(),
            ),
            tail: binding.tail(),
        })
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
    let lines = hydrator.tail.poll().map_err(|_| ())?;
    // The mapper buffers an assistant run until something supersedes it, so
    // the last message of a batch would otherwise sit unseen until the next
    // record arrives — which, at the end of a turn, may be minutes away.
    // Flushing at the end of each poll is what makes a finished turn appear.
    let mut batches: Vec<_> = lines
        .iter()
        .map(|line| hydrator.mapper.map_line(line))
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
    )?;
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
        native_at: Knowledge::Unknown {
            reason: "not-emitted".into(),
            evidence_event_ids: Vec::new(),
        },
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
}
