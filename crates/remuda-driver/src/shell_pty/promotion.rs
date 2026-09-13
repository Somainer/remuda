//! Promotion supervisor for [`super::ShellPtyDriver`] (D-025).
//!
//! One task per live PTY. It samples the foreground process group, journals
//! promote / demote transitions, and — for Claude — tails the native transcript
//! so the structured view is the real conversation.

use super::{PROMOTE_POLL, PtyState};
use crate::claude_print::TranscriptMapper;
use crate::claude_pty::now_ts;
use crate::claude_transcript::{TranscriptTail, locate_transcript};
use crate::error::DriverResult;
use crate::promote::{
    Detected, ProcessTable, ScreenStatus, detect, detect_from_screen, foreground_pgid,
    screen_status,
};
use remuda_protocol::{
    AgentKind, Completeness, EventId, HostId, Id, InstanceId, Knowledge, LifecyclePayload,
    LifecycleTopic, NativeLifecycle, Observation, ObservationPayload, ObservationSource, RunId,
    RuntimeCursor, SchemaVersion, Severity, SourceChannel, SourceCursor, SourceDelivery, Timestamp,
    U64,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
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

/// Current promotion state. Transitions are journaled; steady state is silent.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct PromoteState {
    /// Promoted kind, when promoted.
    pub(super) kind: Option<AgentKind>,
    /// Pid of the matched agent, used to notice a restart within one kind.
    pub(super) pid: Option<i32>,
}

/// Native diagnostic name for a detection transition.
pub(super) const DETECTED: &str = "agent_detected";
/// Native lifecycle name Node folds into `kind` / `mode`.
pub(super) const PROMOTED: &str = "agent_promoted";
/// Native lifecycle name that restores `kind: terminal`.
pub(super) const DEMOTED: &str = "agent_demoted";

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
                native_id: Knowledge::NotApplicable,
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
        &format!("agent detected: {}", kind_name(found.kind)),
        related,
        Severity::Info,
    )
}

fn detected_none() -> (Completeness, ObservationPayload) {
    native(
        LifecycleTopic::Diagnostic,
        DETECTED,
        "agent detected: none",
        BTreeMap::new(),
        Severity::Info,
    )
}

fn promoted(found: &Detected, at: &Timestamp) -> (Completeness, ObservationPayload) {
    let mut related = BTreeMap::new();
    related.insert("kind".into(), kind_name(found.kind).to_owned());
    related.insert("mode".into(), "promoted".to_owned());
    related.insert("promotedAt".into(), String::from(at.clone()));
    if let Some(session) = &found.session_id {
        related.insert("sessionId".into(), session.clone());
    }
    native(
        LifecycleTopic::Session,
        PROMOTED,
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
        "terminal",
        related,
        Severity::Info,
    )
}

/// Spawn the per-instance promotion poller.
pub(super) fn spawn(
    state: Arc<PtyState>,
    ctx: PromoteCtx,
    table: Arc<dyn ProcessTable>,
    current: Arc<std::sync::Mutex<Option<Detected>>>,
    status_slot: Arc<std::sync::Mutex<Option<ScreenStatus>>>,
    events: mpsc::Sender<Observation>,
    seq: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut promote = PromoteState::default();
        let mut hydrator: Option<Hydrator> = None;
        let mut last_status: Option<ScreenStatus> = None;
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
            for (completeness, payload) in transition(&mut promote, found.as_ref(), &now) {
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
            // takes from herdr. Only transitions are journaled.
            let status = promote
                .kind
                .and_then(|_| screen_status(&screen_text(&state)));
            if let Ok(mut slot) = status_slot.lock() {
                *slot = status;
            }
            if status != last_status {
                last_status = status;
                if let Some(status) = status
                    && emit(
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
                    if hydrator.is_none() {
                        hydrator = Hydrator::open(&ctx, found.session_id.as_deref());
                    }
                    if let Some(active) = hydrator.as_mut()
                        && pump(active, &events, &seq, &ctx).await.is_err()
                    {
                        return;
                    }
                }
                _ => hydrator = None,
            }
        }
    })
}

/// `agent_status` lifecycle Node folds into [`remuda_protocol::Activity`].
fn agent_status(status: ScreenStatus) -> ObservationPayload {
    let (_, payload) = native(
        LifecycleTopic::Turn,
        "agent_status",
        status.label(),
        BTreeMap::new(),
        Severity::Info,
    );
    payload
}

/// Current PTY ring as text, for the screen heuristics.
fn screen_text(state: &PtyState) -> String {
    state
        .ring
        .lock()
        .ok()
        .map(|ring| String::from_utf8_lossy(&ring.iter().copied().collect::<Vec<_>>()).into_owned())
        .unwrap_or_default()
}

/// One detection sample: foreground process group first, screen as fallback.
async fn sample(state: &PtyState, table: &dyn ProcessTable) -> Option<Detected> {
    let pgid = {
        let master = state.master.lock().await;
        foreground_pgid(master.as_ref())
    };
    if let Some(pgid) = pgid {
        return detect(&table.process_group(pgid));
    }
    detect_from_screen(&screen_text(state)).map(|kind| Detected {
        kind,
        pid: 0,
        session_id: None,
        hydrates_transcript: kind == AgentKind::Claude,
    })
}

/// A located transcript plus the mapper replaying it.
struct Hydrator {
    tail: TranscriptTail,
    mapper: TranscriptMapper,
}

impl Hydrator {
    fn open(ctx: &PromoteCtx, session_id: Option<&str>) -> Option<Self> {
        let path = locate_transcript(
            &ctx.claude_home,
            &ctx.cwd,
            session_id,
            SystemTime::now() - std::time::Duration::from_secs(60),
        )?;
        let session = session_id.map(ToOwned::to_owned).unwrap_or_else(|| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
        tracing::info!(
            instance_id = %ctx.instance_id.as_id(),
            %session,
            "hydrating promoted terminal from Claude transcript"
        );
        Some(Self {
            mapper: TranscriptMapper::new(
                remuda_protocol::DriverKind::ShellPty,
                ctx.instance_id.clone(),
                ctx.run_id.clone(),
                ctx.journal_id.clone(),
                ctx.host_id.clone(),
                session,
                "promoted".to_owned(),
            ),
            tail: TranscriptTail::new(path),
        })
    }
}

/// Drain whatever the transcript gained since the last poll.
async fn pump(
    hydrator: &mut Hydrator,
    events: &mpsc::Sender<Observation>,
    seq: &AtomicU64,
    ctx: &PromoteCtx,
) -> Result<(), ()> {
    let Ok(lines) = hydrator.tail.poll() else {
        return Ok(());
    };
    for line in lines {
        let mapped = match hydrator.mapper.map_line(&line) {
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
        .map_err(|_| crate::error::DriverError::ControlUnavailable)
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

    fn claude(pid: i32) -> Detected {
        Detected {
            kind: AgentKind::Claude,
            pid,
            session_id: Some("04b95a78-e876-4212-aa9c-a6482f30f583".into()),
            hydrates_transcript: true,
        }
    }

    fn at() -> Timestamp {
        super::now_ts().expect("timestamp")
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

    #[test]
    fn a_terminal_with_no_agent_journals_nothing() {
        let mut state = PromoteState::default();
        assert!(transition(&mut state, None, &at()).is_empty());
        assert_eq!(state, PromoteState::default());
    }

    #[test]
    fn first_detection_journals_a_diagnostic_then_the_promotion() {
        let mut state = PromoteState::default();
        let events = transition(&mut state, Some(&claude(42)), &at());
        assert_eq!(names(&events), vec![DETECTED, PROMOTED]);
        assert_eq!(state.kind, Some(AgentKind::Claude));
        assert_eq!(state.pid, Some(42));
    }

    #[test]
    fn promotion_is_idempotent_while_the_same_agent_holds_the_terminal() {
        let mut state = PromoteState::default();
        transition(&mut state, Some(&claude(42)), &at());
        assert!(transition(&mut state, Some(&claude(42)), &at()).is_empty());
        assert!(transition(&mut state, Some(&claude(42)), &at()).is_empty());
    }

    #[test]
    fn exiting_the_agent_demotes_once_and_then_stays_quiet() {
        let mut state = PromoteState::default();
        transition(&mut state, Some(&claude(42)), &at());
        let events = transition(&mut state, None, &at());
        assert_eq!(names(&events), vec![DEMOTED, DETECTED]);
        assert_eq!(state, PromoteState::default());
        assert!(transition(&mut state, None, &at()).is_empty());
    }

    #[test]
    fn swapping_agents_demotes_the_old_kind_before_promoting_the_new_one() {
        let mut state = PromoteState::default();
        transition(&mut state, Some(&claude(42)), &at());
        let codex = Detected {
            kind: AgentKind::Codex,
            pid: 43,
            session_id: None,
            hydrates_transcript: false,
        };
        let events = transition(&mut state, Some(&codex), &at());
        assert_eq!(names(&events), vec![DEMOTED, DETECTED, PROMOTED]);
        assert_eq!(state.kind, Some(AgentKind::Codex));
    }

    #[test]
    fn the_promotion_payload_carries_kind_mode_and_promoted_at() {
        let mut state = PromoteState::default();
        let stamp = at();
        let events = transition(&mut state, Some(&claude(42)), &stamp);
        let ObservationPayload::Lifecycle(lifecycle) = &events[1].1 else {
            panic!("expected lifecycle");
        };
        let LifecyclePayload::Native(native) = lifecycle.as_ref() else {
            panic!("expected native lifecycle");
        };
        assert_eq!(native.native_name, PROMOTED);
        assert_eq!(native.related_ids["kind"], "claude");
        assert_eq!(native.related_ids["mode"], "promoted");
        assert_eq!(native.related_ids["promotedAt"], String::from(stamp));
        assert_eq!(
            native.related_ids["sessionId"],
            "04b95a78-e876-4212-aa9c-a6482f30f583"
        );
    }
}
