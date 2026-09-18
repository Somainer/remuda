//! `remuda watch` observation + the per-worker intervention verbs (M1 batch
//! 5b; coordinator-hierarchy.md §1.1 goal 6, §5.3).
//!
//! The coordinator's `coord-watch-all.sh` practice, productised:
//! - `POST /v1/workers/observe` reads each live worker's screen through the
//!   Node `tty.screen` RPC, classifies it (working / new DONE sha / BLOCKED /
//!   idle-after-API-error / stalled / gone), persists the classification on
//!   the roster row, and moves lifecycle state only on a fresh DONE/BLOCKED
//!   (a resumed session re-showing its old tip is an echo, not a new report).
//! - nudge / answer / switch-model / resume / replace / stop are the
//!   interventions, all riding Hub → Node → `instance.send` / `tty.write`,
//!   never an ssh session from the CLI.
//!
//! Cross-layer state lives on the roster row, so any coordinator can rebuild
//! watch→act from the Hub alone (design goal 4).

use crate::AppState;
use crate::agent_scope::{caller, caller_project_scope, require_grant};
use crate::auth::require_origin;
use crate::error::HubError;
use crate::http::map_store;
use crate::store::{InstanceDelegation, Store, StoreError};
use crate::workers::{
    DispatchBody, dispatch_core, driver_for, resolve_worker, retire_core, stage_brief,
    worker_extra_env, worker_launch_spec,
};
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::post;
use remuda_protocol::{
    U64, WorkerRoster, WorkerState, WorkerWatch, WorkerWatchStatus, classify_screen,
    encode_answer_key,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use time::OffsetDateTime;

/// Screen-read budget per worker (the Node answers from an in-memory grid).
const SCREEN_TIMEOUT: Duration = Duration::from_secs(5);
/// How many journal events a watch observation scans for the last assistant
/// message / turn result. A single turn's tail is far smaller than this; the
/// cap bounds work on long-lived instances.
const JOURNAL_TAIL_EVENTS: i64 = 256;
/// Detail marker when a classification came from the journal rather than a
/// readable live screen.
const SCREEN_UNAVAILABLE: &str = "screen-unavailable";
/// Pacing of the `switch-model` confirmation dance.
const KEY_SETTLE: Duration = Duration::from_millis(150);
const MODEL_SETTLE: Duration = Duration::from_millis(700);
/// Default nudge used by `worker nudge` with no `--text`.
pub(crate) const DEFAULT_NUDGE: &str = "Continue where you left off. If you are blocked on \
something only the owner can provide (credentials, OS permission, a destructive action), reply \
on one line with BLOCKED <reason>; otherwise keep going and finish with DONE <sha>.";
/// State-loss preamble delivered after a same-worktree respawn.
const RESUME_HANDBACK: &str = "[coordinator] Your previous process was lost and this session was \
respawned in the same worktree. First run git status and git log --oneline -5 to compare with your \
last steps (uncommitted edits on disk survived; any running process did not — rebuild/re-run as \
needed). Then continue the task exactly where you left off, under the same rules and the same \
final report (one line: DONE <sha> or BLOCKED <reason>).";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/workers/observe", post(observe_workers))
        .route("/v1/workers/{id}/nudge", post(nudge_worker))
        .route("/v1/workers/{id}/answer", post(answer_worker))
        .route("/v1/workers/{id}/switch-model", post(switch_worker_model))
        .route("/v1/workers/{id}/resume", post(resume_worker))
        .route("/v1/workers/{id}/replace", post(replace_worker))
        .route("/v1/workers/{id}/stop", post(stop_worker))
}

// ── request bodies ─────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ObserveBody {
    /// Restrict to one project; absent = every project in the caller's scope.
    #[serde(default)]
    project_id: Option<String>,
    /// Override the stall quiet-window (minutes).
    #[serde(default)]
    stall_mins: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NudgeBody {
    /// Custom nudge text; absent = the default continue prompt.
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnswerBody {
    /// `enter` | `esc` | `1`..`9` | free text (submitted with enter).
    key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwitchModelBody {
    /// Model id to switch the live session to.
    model: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ResumeBody {
    /// Override the state-loss handback note.
    #[serde(default)]
    handback: Option<String>,
}

// ── observe ────────────────────────────────────────────────────────────────

async fn observe_workers(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ObserveBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = caller(&state, &headers).await?;
    require_grant(&state, &device, remuda_protocol::GrantVerb::Dispatch).await?;
    let scope = caller_project_scope(&state, &device).await?;
    let stall_mins = body
        .stall_mins
        .filter(|mins| *mins > 0)
        .unwrap_or(remuda_protocol::DEFAULT_STALL_THRESHOLD_MINS);
    let mut workers = state
        .store
        .list_workers(body.project_id.clone())
        .await
        .map_err(map_store)?;
    workers.retain(|worker| {
        worker.state.is_active() && scope.allows_project(worker.project_id.as_id().as_str())
    });
    let mut items = Vec::with_capacity(workers.len());
    for worker in workers {
        items.push(observe_one(&state, worker, stall_mins).await?);
    }
    Ok(Json(json!({ "items": items })))
}

/// Title of a first-run/native dialog detected on a live screen, if one owns
/// the keyboard.
///
/// Carrier-agnostic: shell-pty and claude-pty both return rows through
/// `tty.screen`, and the same rule runs over an emulated grid (`raw == false`)
/// and a raw VT ring tail (`raw == true`); the whitespace-flattened match
/// tolerates soft wraps in both shapes.
fn screen_dialog_title(lines: &[String], raw: bool) -> Option<String> {
    let grid = if raw {
        remuda_screen::ScreenGrid::from_raw(&lines.join("\n"))
    } else {
        remuda_screen::ScreenGrid::from_lines(lines.iter().cloned())
    };
    remuda_screen::first_run_dialog(&grid).map(|dialog| dialog.title.to_owned())
}

/// What the Hub could read of one worker's live screen.
enum ScreenRead {
    /// Screen rows plus the carrier-reported lifecycle. `raw` is true when the
    /// rows are a raw VT ring tail rather than an emulated grid.
    /// `screen_available` is false for a print driver / dead pty carrier that
    /// answered `supported=false`: classification then comes from the journal.
    Screen {
        lines: Vec<String>,
        lifecycle: String,
        raw: bool,
        screen_available: bool,
    },
    /// Carrier unreachable / gone.
    Gone(&'static str),
}

/// The reason a gone worker reports, preferring the settled row's own.
///
/// The carrier code says only *that* the Hub could not read a screen
/// (`host-offline`, `screen-unavailable`, …); the instance row's `lastError`
/// says how the worker actually died. A Hub-settled `node-epoch-changed` is
/// the honest answer and must reach the report, so it wins whenever the row
/// has one — a bare `host-offline` on a session whose process is known dead
/// tells the owner nothing they can act on.
fn gone_reason(carrier: &'static str, lifecycle_reason: Option<&str>) -> String {
    lifecycle_reason
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .map(first_line)
        .unwrap_or_else(|| carrier.to_string())
}

/// First line of a reason code, truncated the way failure reasons are.
fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(text.trim());
    line.chars().take(120).collect()
}

async fn read_worker_screen(state: &AppState, worker: &WorkerRoster) -> ScreenRead {
    let host_id = worker.host_id.as_id().as_str();
    let Some(instance_id) = worker.instance_id.as_ref() else {
        return ScreenRead::Gone("no-instance");
    };
    if state.nodes.kind_of(host_id).await.is_none() {
        return ScreenRead::Gone("host-offline");
    }
    match state
        .nodes
        .call(
            host_id,
            remuda_protocol::hubnode::METHOD_TTY_SCREEN,
            json!({ "instanceId": instance_id.as_id() }),
            SCREEN_TIMEOUT,
        )
        .await
    {
        Ok(Some(response)) if response.get("error").is_none() => {
            let result = response.get("result").unwrap_or(&response);
            let supported = result
                .get("supported")
                .and_then(Value::as_bool)
                .is_some_and(|value| value);
            let lifecycle = result
                .get("lifecycle")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if !supported {
                // A print driver legitimately has no screen; leave rows empty
                // (lifecycle alone then decides, with the journal supplying
                // text). A dead pty carrier reports supported=false with a
                // terminal lifecycle → gone below.
                return ScreenRead::Screen {
                    lines: Vec::new(),
                    lifecycle,
                    raw: false,
                    screen_available: false,
                };
            }
            let raw = result
                .get("source")
                .and_then(Value::as_str)
                .is_some_and(|source| source != "emulator");
            let lines = result
                .get("lines")
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .map(|row| row.as_str().unwrap_or_default().to_owned())
                        .collect()
                })
                .unwrap_or_default();
            ScreenRead::Screen {
                lines,
                lifecycle,
                raw,
                screen_available: true,
            }
        }
        Ok(Some(_)) => ScreenRead::Gone("screen-error"),
        Ok(None) => ScreenRead::Gone("host-offline"),
        Err(_) => ScreenRead::Gone("screen-unavailable"),
    }
}

async fn observe_one(
    state: &AppState,
    worker: WorkerRoster,
    stall_mins: i64,
) -> Result<Value, HubError> {
    let now = now_unix();
    // Hub-authoritative instance lifecycle/activity, if the instance exists.
    let mut activity = String::new();
    let mut instance_updated: Option<String> = None;
    let mut record_lifecycle = String::new();
    let mut lifecycle_reason: Option<String> = None;
    if let Some(instance_id) = worker.instance_id.as_ref()
        && let Some(record) = state
            .store
            .get_instance(instance_id.as_id().to_string())
            .await
            .map_err(map_store)?
    {
        activity = record.activity;
        instance_updated = Some(record.updated_at);
        record_lifecycle = record.lifecycle;
        lifecycle_reason = record.last_error;
    }

    let (lines, mut lifecycle, forced_gone, raw, screen_available) =
        match read_worker_screen(state, &worker).await {
            ScreenRead::Screen {
                lines,
                lifecycle,
                raw,
                screen_available,
            } => (lines, lifecycle, None, raw, screen_available),
            ScreenRead::Gone(reason) => (
                Vec::new(),
                String::new(),
                Some(gone_reason(reason, lifecycle_reason.as_deref())),
                false,
                false,
            ),
        };
    // The Hub row is authoritative once the instance is terminal: a stale or
    // absent "ready" from a disconnected screen carrier must not mask a
    // failed/exited instance (the 2026-09-17 watch-failed-1 trap). Otherwise
    // the live carrier lifecycle wins, falling back to the row when empty.
    if lifecycle.is_empty()
        || matches!(
            record_lifecycle.as_str(),
            "failed" | "exited" | "closed" | "terminated"
        )
    {
        lifecycle.clone_from(&record_lifecycle);
    }

    // Screenless carriers (claude-print, a dropped pty) classify from the
    // mirrored journal tail: last assistant texts and the last turn result.
    let hints = journal_hints(state, worker.instance_id.as_ref()).await?;

    let prior = worker.watch.clone();
    let digest =
        (!lines.is_empty()).then(|| crate::config::sha256_hex(lines.join("\n").as_bytes()));
    let last_activity_unix = last_activity_unix(
        digest.as_deref(),
        prior.as_ref(),
        instance_updated.as_deref(),
        now,
    );

    // Echo baselines seed from both prior watch and the durable lifecycle
    // state (a DONE recorded through the state API still suppresses an echo).
    let known_done_sha = prior
        .as_ref()
        .and_then(|watch| watch.last_done_sha.clone())
        .or_else(|| match &worker.state {
            WorkerState::Done { sha } => Some(sha.clone()),
            _ => None,
        });
    let known_blocked = prior
        .as_ref()
        .and_then(|watch| watch.last_blocked.clone())
        .or_else(|| match &worker.state {
            WorkerState::Blocked { reason } => Some(reason.clone()),
            _ => None,
        });

    // A hard failure known to the Hub (failed row / errored turn / exit whose
    // last assistant message was an error) classifies even when the carrier is
    // unreachable.
    let hard_failed = hints.turn_error
        || record_lifecycle == "failed"
        || (record_lifecycle == "exited" && hints.tail_error);
    let class = match forced_gone {
        Some(reason) if !hard_failed => remuda_protocol::ScreenClass::Gone { reason },
        _ => {
            // A first-run/native dialog detected on the live screen (folder
            // trust, auto-mode outside reads) classifies blocked with its
            // title, for both carriers and both screen sources — the dialog
            // is what the Node read, not what the carrier guessed it was
            // doing (dispatch-onboarding-1).
            let dialog_title = if screen_available {
                screen_dialog_title(&lines, raw)
            } else {
                None
            };
            let signals = remuda_protocol::ScreenSignals {
                lines: &lines,
                lifecycle: &lifecycle,
                activity: &activity,
                host_online: forced_gone.is_none(),
                now_unix: now,
                last_activity_unix,
                stall_mins,
                known_done_sha: known_done_sha.as_deref(),
                known_blocked: known_blocked.as_deref(),
                raw,
                screen_available,
                dialog_title: dialog_title.as_deref(),
                journal_lines: &hints.assistant_texts,
                turn_error: hints.turn_error,
                tail_error: hints.tail_error,
                last_error_line: hints.error_line.as_deref(),
                lifecycle_reason: lifecycle_reason.as_deref(),
            };
            classify_screen(&signals)
        }
    };

    let mut next_state: Option<WorkerState> = None;
    let mut detail: Option<String> = None;
    let (status, next_done, next_blocked) = match class {
        remuda_protocol::ScreenClass::Working => (WorkerWatchStatus::Working, None, None),
        remuda_protocol::ScreenClass::Done { sha, line } => {
            detail = Some(line);
            next_state = Some(WorkerState::Done { sha: sha.clone() });
            (
                WorkerWatchStatus::Done { sha: sha.clone() },
                Some(sha),
                None,
            )
        }
        remuda_protocol::ScreenClass::Blocked { reason, line } => {
            detail = Some(line);
            next_state = Some(WorkerState::Blocked {
                reason: reason.clone(),
            });
            (
                WorkerWatchStatus::Blocked {
                    reason: reason.clone(),
                },
                None,
                Some(reason),
            )
        }
        remuda_protocol::ScreenClass::IdleApiError { fragment } => {
            detail = Some(format!("idle; screen shows {fragment}"));
            (WorkerWatchStatus::IdleApiError, None, None)
        }
        remuda_protocol::ScreenClass::Stalled { quiet_mins } => {
            detail = Some(format!("no activity for ~{quiet_mins}m"));
            (WorkerWatchStatus::Stalled, None, None)
        }
        remuda_protocol::ScreenClass::Gone { reason } => {
            detail = Some(reason.clone());
            (WorkerWatchStatus::Gone { reason }, None, None)
        }
        remuda_protocol::ScreenClass::Failed { reason, line } => {
            detail = Some(line);
            (
                WorkerWatchStatus::Failed {
                    reason: reason.clone(),
                },
                None,
                None,
            )
        }
    };
    // No live screen: the classification came from the journal (or the
    // instance row), so the detail column must say where — a screen reader is
    // not possible for this worker (watch-failed-1). The marker leads so it
    // survives the CLI table's 48-char detail truncation.
    if !screen_available {
        detail = Some(match detail {
            Some(text) if text != SCREEN_UNAVAILABLE => {
                format!("{SCREEN_UNAVAILABLE}; {text}")
            }
            _ => SCREEN_UNAVAILABLE.to_string(),
        });
    }

    let watch = WorkerWatch {
        status,
        observed_at: remuda_protocol::Timestamp::try_from(crate::config::now_rfc3339())
            .map_err(|err| HubError::Internal(err.to_string()))?,
        detail,
        last_done_sha: next_done
            .or_else(|| prior.as_ref().and_then(|watch| watch.last_done_sha.clone())),
        last_blocked: next_blocked
            .or_else(|| prior.as_ref().and_then(|watch| watch.last_blocked.clone())),
        last_screen_digest: digest,
        last_activity_at: Some(
            remuda_protocol::Timestamp::try_from(fmt_rfc3339_millis(
                last_activity_unix.unwrap_or(now),
            ))
            .map_err(|err| HubError::Internal(err.to_string()))?,
        ),
    };

    let row = state
        .store
        .mutate_worker(worker.meta.id.as_id().to_string(), move |row| {
            if let Some(next) = next_state {
                row.state = next;
            }
            row.watch = Some(watch);
            Ok(())
        })
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    serde_json::to_value(row).map_err(|err| HubError::Internal(err.to_string()))
}

/// What a journal tail tells a screenless observation: the assistant text in
/// transcript order, whether the last turn result errored, and the first line
/// of the last error-grade assistant message.
#[derive(Default)]
struct JournalHints {
    assistant_texts: Vec<String>,
    turn_error: bool,
    /// The final assistant text block grades as a hard error. Used to tell a
    /// fatal exit-after-error apart from a session that errored, recovered and
    /// reported DONE before exiting.
    tail_error: bool,
    error_line: Option<String>,
}

/// Read the instance journal tail and extract the signals a screenless
/// classification needs. Mirrors what `remuda instance read --source screen`
/// falls back to for a print driver: assistant messages and turn results.
async fn journal_hints(
    state: &AppState,
    instance_id: Option<&remuda_protocol::InstanceId>,
) -> Result<JournalHints, HubError> {
    let Some(instance_id) = instance_id else {
        return Ok(JournalHints::default());
    };
    let events = state
        .store
        .read_journal_tail(instance_id.as_id().to_string(), JOURNAL_TAIL_EVENTS)
        .await
        .map_err(map_store)?;
    Ok(scan_journal(&events))
}

/// Pure fold over raw journal event JSON (the same payload shape the Node
/// mirrors with `journal.append`). Kept tolerant: an observation missing a
/// field is ignored, not fatal.
fn scan_journal(events: &[Value]) -> JournalHints {
    let mut hints = JournalHints::default();
    for event in events {
        if event.get("kind").and_then(Value::as_str) != Some("lifecycle")
            && event.get("kind").and_then(Value::as_str) != Some("message")
        {
            continue;
        }
        let Some(payload) = event.get("payload") else {
            continue;
        };
        match event.get("kind").and_then(Value::as_str) {
            Some("message") => {
                if payload.get("role").and_then(Value::as_str) != Some("assistant") {
                    continue;
                }
                let Some(blocks) = payload.get("blocks").and_then(Value::as_array) else {
                    continue;
                };
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("text")
                        && let Some(text) = block.get("text").and_then(Value::as_str)
                        && !text.trim().is_empty()
                    {
                        hints.assistant_texts.push(text.to_string());
                        if let Some(line) = error_line(text) {
                            hints.error_line = Some(line);
                        }
                    }
                }
            }
            Some("lifecycle") if payload.get("type").and_then(Value::as_str) == Some("native") => {
                // The print/pty drivers journal a turn result as native
                // lifecycle topic=turn, name=result, status error|turn_done.
                let is_turn_result = payload.get("topic").and_then(Value::as_str) == Some("turn")
                    && payload.get("nativeName").and_then(Value::as_str) == Some("result");
                if is_turn_result && let Some(status) = knowledge_string(payload.get("status")) {
                    hints.turn_error = status == "error";
                }
            }
            _ => {}
        }
    }
    // Only the final assistant text decides the "exited after an error"
    // heuristic; an older error that the session recovered from does not.
    hints.tail_error = hints
        .assistant_texts
        .last()
        .is_some_and(|text| error_line(text).is_some());
    hints
}

/// First line of an assistant text that grades as a hard error. Transient
/// retry noise (`Retrying…`) is intentionally excluded — that is the
/// idle-api-error class, not a failure.
fn error_line(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    const HARD_ERROR_FRAGMENTS: &[&str] = &["api error", "connection closed"];
    HARD_ERROR_FRAGMENTS
        .iter()
        .any(|needle| lower.contains(needle))
        .then(|| {
            text.lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or(text.trim())
                .chars()
                .take(120)
                .collect()
        })
}

/// Read a `Knowledge<String>`-shaped field that may serialize either as a bare
/// string or as `{"value": "…"}`.
fn knowledge_string(value: Option<&Value>) -> Option<String> {
    value.and_then(|value| {
        value.as_str().map(str::to_string).or_else(|| {
            value
                .get("value")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
    })
}

/// Decide the unix time of the worker's last real activity. The first
/// observation seeds from the instance record (so an already-stalled turn is
/// caught immediately); later observations advance the clock only when the
/// screen actually changed.
fn last_activity_unix(
    digest: Option<&str>,
    prior: Option<&WorkerWatch>,
    instance_updated: Option<&str>,
    now: i64,
) -> Option<i64> {
    match prior {
        None => instance_updated.and_then(parse_unix).or(Some(now)),
        Some(previous) => {
            let changed = match (previous.last_screen_digest.as_deref(), digest) {
                (Some(old), Some(new)) => old != new,
                (None, Some(_)) => true,
                _ => false,
            };
            if changed {
                Some(now)
            } else {
                previous
                    .last_activity_at
                    .as_ref()
                    .map(|stamp| String::from(stamp.clone()))
                    .and_then(|stamp| parse_unix(&stamp))
                    .or_else(|| instance_updated.and_then(parse_unix))
                    .or(Some(now))
            }
        }
    }
}

// ── nudge ──────────────────────────────────────────────────────────────────

async fn nudge_worker(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<NudgeBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = caller(&state, &headers).await?;
    require_grant(&state, &device, remuda_protocol::GrantVerb::Dispatch).await?;
    let scope = caller_project_scope(&state, &device).await?;
    let worker = resolve_worker(&state, &id, &scope).await?;
    if !worker.state.is_active() {
        return Err(HubError::Conflict("worker is retired".into()));
    }
    let text = body
        .text
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| DEFAULT_NUDGE.to_string());

    // Throttle per project policy (§3.2 nudgeThrottleMins).
    let throttle_mins = state
        .store
        .get_project(worker.project_id.as_id().to_string())
        .await
        .map_err(map_store)?
        .map(|project| project.policy.configurable.nudge_throttle_mins)
        .unwrap_or(15)
        .max(0);
    if let Some(last) = worker
        .last_nudge_at
        .as_ref()
        .map(|stamp| String::from(stamp.clone()))
        .and_then(|stamp| parse_unix(&stamp))
        && throttle_mins > 0
    {
        let elapsed = now_unix() - last;
        let wait = throttle_mins * 60 - elapsed;
        if wait > 0 {
            return Err(HubError::Conflict(format!(
                "nudge throttled for another {wait}s (project policy)"
            )));
        }
    }

    let command = deliver_note_file(&state, &worker, &device.id, &text, "nudge.md").await?;
    let now = crate::config::now_rfc3339();
    let updated = state
        .store
        .mutate_worker(worker.meta.id.as_id().to_string(), move |row| {
            row.last_nudge_at = Some(
                remuda_protocol::Timestamp::try_from(now)
                    .map_err(|err| StoreError::Id(err.to_string()))?,
            );
            Ok(())
        })
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    state
        .store
        .append_audit(
            device.id,
            "worker.nudge".into(),
            Some(updated.meta.id.as_id().to_string()),
            json!({ "throttled": false }),
        )
        .await
        .map_err(map_store)?;
    Ok(Json(json!({ "worker": updated, "command": command })))
}

/// Stage a short coordinator note as a file attachment and deliver it with
/// `instance.send` — the same file transport briefs use, never inline through
/// a shell.
async fn deliver_note_file(
    state: &AppState,
    worker: &WorkerRoster,
    device_id: &str,
    text: &str,
    name: &str,
) -> Result<Value, HubError> {
    let instance_id = worker
        .instance_id
        .as_ref()
        .ok_or_else(|| HubError::Conflict("worker never launched an instance".into()))?;
    let (object_id, payload) = stage_brief(
        state,
        instance_id.as_id().as_str(),
        worker.host_id.as_id().as_str(),
        device_id,
        text,
        name,
    )
    .await?;
    let (command, _) = state
        .store
        .queue_command(
            None,
            Some(instance_id.as_id().to_string()),
            worker.host_id.as_id().to_string(),
            "instance.send".into(),
            payload,
            None,
        )
        .await
        .map_err(map_store)?;
    let live = state
        .nodes
        .kind_of(worker.host_id.as_id().as_str())
        .await
        .is_some();
    let command = crate::http::forward_if_online(state, command, live).await?;
    let _ = object_id;
    Ok(json!(command))
}

// ── answer (keys) ──────────────────────────────────────────────────────────

async fn answer_worker(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<AnswerBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = caller(&state, &headers).await?;
    require_grant(&state, &device, remuda_protocol::GrantVerb::Dispatch).await?;
    let scope = caller_project_scope(&state, &device).await?;
    let worker = resolve_worker(&state, &id, &scope).await?;
    if !worker.state.is_active() {
        return Err(HubError::Conflict("worker is retired".into()));
    }
    let encoded = encode_answer_key(&body.key).map_err(HubError::BadRequest)?;
    let instance_id = worker
        .instance_id
        .as_ref()
        .ok_or_else(|| HubError::Conflict("worker never launched an instance".into()))?;
    let command = write_keys(
        &state,
        instance_id.as_id().as_str(),
        worker.host_id.as_id().as_str(),
        encoded.names.clone(),
        encoded.data_base64.clone(),
    )
    .await?;
    state
        .store
        .append_audit(
            device.id,
            "worker.answer".into(),
            Some(worker.meta.id.as_id().to_string()),
            json!({ "keys": encoded.names }),
        )
        .await
        .map_err(map_store)?;
    Ok(Json(json!({
        "worker": worker.meta.id.as_id(),
        "keys": encoded.names,
        "command": command,
    })))
}

async fn write_keys(
    state: &AppState,
    instance_id: &str,
    host_id: &str,
    names: Vec<String>,
    data_base64: String,
) -> Result<Value, HubError> {
    let (command, _) = state
        .store
        .queue_command(
            None,
            Some(instance_id.to_string()),
            host_id.to_string(),
            remuda_protocol::hubnode::METHOD_TTY_WRITE.into(),
            json!({
                "instanceId": instance_id,
                "keys": names,
                "dataBase64": data_base64,
                "source": "coordinator",
            }),
            None,
        )
        .await
        .map_err(map_store)?;
    let live = state.nodes.kind_of(host_id).await.is_some();
    Ok(json!(
        crate::http::forward_if_online(state, command, live).await?
    ))
}

// ── switch-model ─────────────────────────────────────────────────────────

/// Fragments that mark the bottom of the screen as a model-switch confirm
/// dialog. The confirming Enter is only sent when one is present.
fn shows_model_confirmation(lines: &[String]) -> bool {
    lines.iter().rev().take(8).any(|line| {
        let line = line.to_ascii_lowercase();
        (line.contains("switch") && line.contains("model"))
            || line.contains("are you sure")
            || line.contains("confirm")
    })
}

async fn switch_worker_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<SwitchModelBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = caller(&state, &headers).await?;
    require_grant(&state, &device, remuda_protocol::GrantVerb::Dispatch).await?;
    let scope = caller_project_scope(&state, &device).await?;
    let worker = resolve_worker(&state, &id, &scope).await?;
    if !worker.state.is_active() {
        return Err(HubError::Conflict("worker is retired".into()));
    }
    if worker.harness != "claude" {
        return Err(HubError::BadRequest(format!(
            "switch-model runs the Claude /model dance; this worker is {}",
            worker.harness
        )));
    }
    let instance_id = worker
        .instance_id
        .as_ref()
        .ok_or_else(|| HubError::Conflict("worker never launched an instance".into()))?
        .as_id()
        .to_string();
    let host_id = worker.host_id.as_id().to_string();
    if state.nodes.kind_of(&host_id).await.is_none() {
        return Err(HubError::HostOffline {
            host_id: host_id.clone(),
        });
    }
    let model = body.model.trim().to_string();
    if model.is_empty() {
        return Err(HubError::BadRequest("model id is required".into()));
    }

    // esc×2 interrupts a retry loop, then `/model <id>` + enter opens the
    // model switch UI.
    let esc = encode_answer_key("esc").map_err(HubError::BadRequest)?;
    write_keys(
        &state,
        &instance_id,
        &host_id,
        esc.names.clone(),
        esc.data_base64.clone(),
    )
    .await?;
    tokio::time::sleep(KEY_SETTLE).await;
    write_keys(
        &state,
        &instance_id,
        &host_id,
        esc.names.clone(),
        esc.data_base64.clone(),
    )
    .await?;
    tokio::time::sleep(KEY_SETTLE).await;
    let typed = encode_answer_key(&format!("/model {model}")).map_err(HubError::BadRequest)?;
    write_keys(
        &state,
        &instance_id,
        &host_id,
        typed.names,
        typed.data_base64,
    )
    .await?;
    tokio::time::sleep(MODEL_SETTLE).await;

    // Gate: confirm only when the screen actually shows the confirmation.
    let screen = bottom_screen_lines(&state, &host_id, &instance_id).await;
    let confirmed = screen.as_deref().is_some_and(shows_model_confirmation);
    if confirmed {
        let enter = encode_answer_key("enter").map_err(HubError::BadRequest)?;
        write_keys(
            &state,
            &instance_id,
            &host_id,
            enter.names,
            enter.data_base64,
        )
        .await?;
        tokio::time::sleep(KEY_SETTLE).await;
        let _ = deliver_note_file(
            &state,
            &worker,
            &device.id,
            "Continue where you left off.",
            "nudge.md",
        )
        .await;
    }

    let now = crate::config::now_rfc3339();
    let previous_model = worker.model.clone();
    let updated = state
        .store
        .mutate_worker(worker.meta.id.as_id().to_string(), move |row| {
            if confirmed {
                row.model = Some(model.clone());
                row.last_nudge_at = Some(
                    remuda_protocol::Timestamp::try_from(now)
                        .map_err(|err| StoreError::Id(err.to_string()))?,
                );
            }
            Ok(())
        })
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    state
        .store
        .append_audit(
            device.id,
            "worker.switch-model".into(),
            Some(updated.meta.id.as_id().to_string()),
            json!({ "from": previous_model, "to": body.model, "confirmed": confirmed }),
        )
        .await
        .map_err(map_store)?;
    Ok(Json(json!({
        "worker": updated,
        "confirmed": confirmed,
        "screen": screen.unwrap_or_default(),
        "hint": if confirmed {
            "model switched and confirmed"
        } else {
            "no confirmation dialog observed; not sending Enter — answer it with `remuda worker answer <name> enter`"
        },
    })))
}

/// Read the bottom of a worker's screen for a switch-model gate; `None` when
/// the carrier cannot be read.
async fn bottom_screen_lines(
    state: &AppState,
    host_id: &str,
    instance_id: &str,
) -> Option<Vec<String>> {
    let response = state
        .nodes
        .call(
            host_id,
            remuda_protocol::hubnode::METHOD_TTY_SCREEN,
            json!({ "instanceId": instance_id }),
            SCREEN_TIMEOUT,
        )
        .await
        .ok()??;
    let result = response.get("result").unwrap_or(&response);
    if !result
        .get("supported")
        .and_then(Value::as_bool)
        .is_some_and(|value| value)
    {
        return None;
    }
    Some(
        result
            .get("lines")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .map(|row| row.as_str().unwrap_or_default().to_owned())
                    .collect()
            })
            .unwrap_or_default(),
    )
}

// ── resume ─────────────────────────────────────────────────────────────────

async fn resume_worker(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ResumeBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = caller(&state, &headers).await?;
    require_grant(&state, &device, remuda_protocol::GrantVerb::Dispatch).await?;
    let scope = caller_project_scope(&state, &device).await?;
    let worker = resolve_worker(&state, &id, &scope).await?;
    if !worker.state.is_active() {
        return Err(HubError::Conflict("worker is retired".into()));
    }
    let old_instance_id = worker
        .instance_id
        .as_ref()
        .ok_or_else(|| HubError::Conflict("worker never launched an instance".into()))?
        .as_id()
        .to_string();

    let (new_instance_id, mode) =
        respawn_instance(&state, &headers, &worker, &old_instance_id).await?;

    // Recover the last brief bytes so it can be re-delivered.
    let mut content = body
        .handback
        .clone()
        .unwrap_or_else(|| RESUME_HANDBACK.into());
    if let Some(object_id) = &worker.brief_object_id
        && let Some(bytes) = state
            .store
            .read_object_bytes(object_id.clone())
            .await
            .map_err(map_store)?
        && let Ok(brief) = String::from_utf8(bytes)
    {
        content = format!("{content}\n\n--- your brief, re-delivered after respawn ---\n{brief}");
    }
    let (object_id, payload) = stage_brief(
        &state,
        &new_instance_id,
        worker.host_id.as_id().as_str(),
        &device.id,
        &content,
        "handback.md",
    )
    .await?;
    let (command, _) = state
        .store
        .queue_command(
            None,
            Some(new_instance_id.clone()),
            worker.host_id.as_id().to_string(),
            "instance.send".into(),
            payload,
            None,
        )
        .await
        .map_err(map_store)?;
    let live = state
        .nodes
        .kind_of(worker.host_id.as_id().as_str())
        .await
        .is_some();
    let command = crate::http::forward_if_online(&state, command, live).await?;

    let now = crate::config::now_rfc3339();
    let row = state
        .store
        .mutate_worker(worker.meta.id.as_id().to_string(), {
            let old_instance_id = old_instance_id.clone();
            let new_instance_id = new_instance_id.clone();
            let object_id = object_id.clone();
            move |row| {
                row.resumed_from = Some(old_instance_id.parse().map_err(
                    |err: remuda_protocol::WireValueError| StoreError::Id(err.to_string()),
                )?);
                row.instance_id = Some(new_instance_id.parse().map_err(
                    |err: remuda_protocol::WireValueError| StoreError::Id(err.to_string()),
                )?);
                row.brief_object_id = Some(object_id);
                row.state = WorkerState::Working;
                row.watch = None;
                row.last_nudge_at = Some(
                    remuda_protocol::Timestamp::try_from(now.clone())
                        .map_err(|err| StoreError::Id(err.to_string()))?,
                );
                Ok(())
            }
        })
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    state
        .store
        .append_audit(
            device.id,
            "worker.resume".into(),
            Some(row.meta.id.as_id().to_string()),
            json!({ "from": old_instance_id, "to": new_instance_id, "mode": mode }),
        )
        .await
        .map_err(map_store)?;
    Ok(Json(json!({
        "worker": row,
        "instanceId": new_instance_id,
        "mode": mode,
        "command": command,
    })))
}

/// Respawn a worker's agent in its same worktree. Prefer the D-026 native
/// resume (a known transcript id → `claude --resume <id>`); when the lost
/// process never reported one, launch a fresh agent in the same worktree (the
/// harness-equivalent of `claude --continue`).
async fn respawn_instance(
    state: &AppState,
    headers: &HeaderMap,
    worker: &WorkerRoster,
    old_instance_id: &str,
) -> Result<(String, &'static str), HubError> {
    let has_native_ref = state
        .store
        .get_instance(old_instance_id.to_string())
        .await
        .map_err(map_store)?
        .and_then(|record| record.native_session_id)
        .is_some();
    if has_native_ref {
        let resume_body: crate::http::ResumeBody = serde_json::from_value(json!({
            "mode": "terminal",
            "prompt": RESUME_HANDBACK,
        }))
        .map_err(|err| HubError::Internal(err.to_string()))?;
        match crate::http::resume_instance(
            State(state.clone()),
            headers.clone(),
            Path(old_instance_id.to_string()),
            Json(resume_body),
        )
        .await
        {
            Ok(Json(value)) => {
                let new_id = value
                    .get("instance")
                    .and_then(|instance| {
                        instance
                            .get("instanceId")
                            .or_else(|| instance.get("id"))
                            .and_then(Value::as_str)
                    })
                    .ok_or_else(|| HubError::Internal("resume returned no instance id".into()))?
                    .to_string();
                return Ok((new_id, "resume"));
            }
            // Only the missing-transcript case falls through to relaunch;
            // every other resume failure (offline, aged, operator-only) stands.
            Err(HubError::Conflict(message))
                if message.contains("never reported a native session id") => {}
            Err(err) => return Err(err),
        }
    }
    relaunch_instance(state, headers, worker)
        .await
        .map(|id| (id, "relaunch"))
}

/// Launch a fresh agent in the worker's existing (surviving) worktree.
async fn relaunch_instance(
    state: &AppState,
    headers: &HeaderMap,
    worker: &WorkerRoster,
) -> Result<String, HubError> {
    let device = caller(state, headers).await?;
    let host = state
        .store
        .get_host(worker.host_id.as_id().to_string())
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    if state.nodes.kind_of(&host.host_id).await.is_none() {
        return Err(HubError::HostOffline {
            host_id: host.host_id.clone(),
        });
    }
    let host = Store::with_live_link(host, true);
    let project = state
        .store
        .get_project(worker.project_id.as_id().to_string())
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;

    let delegation = match &worker.provider_profile_id {
        Some(profile_id) => {
            let profile = state
                .store
                .get_provider(profile_id.clone())
                .await
                .map_err(map_store)?
                .ok_or_else(|| HubError::Internal("launch named a vanished profile".into()))?;
            Some(match profile.kind.as_str() {
                "native" => "none".to_string(),
                "direct" => "direct".to_string(),
                _ => "gateway".to_string(),
            })
        }
        None => None,
    };
    // A respawn keeps the carrier the worker was actually running on, so
    // `worker resume` cannot silently move it to a different product. Rows
    // written before the roster recorded a driver fall back to the host default.
    let driver = match worker.driver.as_deref() {
        Some(recorded) => recorded.to_string(),
        None => driver_for(&worker.harness, &host)?,
    };
    let extra_env = worker_extra_env(worker.target_dir.as_deref(), worker.port_block.as_deref());
    let mut spec = worker_launch_spec(
        &worker.harness,
        &driver,
        &worker.model,
        &worker.provider_profile_id,
        &delegation,
        worker.workspace_id.as_id().as_str(),
        &host.host_id,
        &worker.worktree_path,
        &worker.name,
        project.meta.id.as_id().as_str(),
        worker.task_id.as_ref().map(|id| id.as_id().as_str()),
        extra_env,
    );
    crate::agent_scope::prepare_create(state, headers, &device, &host.host_id, &driver, &mut spec)
        .await?;
    if let Some(obj) = spec.as_object_mut()
        && let Some(path) = host
            .claude_binary_path
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        && obj.get("binaryPath").is_none_or(Value::is_null)
    {
        obj.insert("binaryPath".into(), json!(path));
    }
    crate::providers::resolve_and_attach_with_project(
        state,
        &host,
        &mut spec,
        Some(&project.provider),
    )
    .await?;

    let delegation_tree = InstanceDelegation {
        role: Some("worker".into()),
        scope: remuda_protocol::InstanceScope {
            project_ids: vec![project.meta.id.clone()],
            ..Default::default()
        },
        grants: Vec::new(),
        task_id: worker.task_id.as_ref().map(|id| id.as_id().to_string()),
        enforce_tree: true,
    };
    let (instance, _command) = crate::placement::spawn_on_host(
        state,
        &host,
        crate::placement::SpawnRequest {
            kind: worker.harness.clone(),
            driver,
            workspace_id: Some(worker.workspace_id.as_id().to_string()),
            title: Some(worker.name.clone()),
            prompt: None,
            spec,
            operation: "instance.create",
            idempotency_key: None,
            delegation: delegation_tree,
        },
    )
    .await?;
    Ok(instance.instance_id)
}

// ── replace ────────────────────────────────────────────────────────────────

async fn replace_worker(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = caller(&state, &headers).await?;
    require_grant(&state, &device, remuda_protocol::GrantVerb::Dispatch).await?;
    let scope = caller_project_scope(&state, &device).await?;
    let worker = resolve_worker(&state, &id, &scope).await?;

    // Recover the original brief so the replacement gets the same task.
    let brief = match &worker.brief_object_id {
        Some(object_id) => state
            .store
            .read_object_bytes(object_id.clone())
            .await
            .map_err(map_store)?
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .ok_or_else(|| {
                HubError::Conflict(
                    "original brief is no longer readable; re-dispatch with `remuda dispatch --brief`"
                        .into(),
                )
            })?,
        None => {
            return Err(HubError::Conflict(
                "worker has no stored brief; re-dispatch with `remuda dispatch --brief`".into(),
            ));
        }
    };

    let previous_count = worker.replace_count.map(|value| value.0).unwrap_or(0);
    let old_id = worker.meta.id.as_id().to_string();
    let name = worker.name.clone();
    let body = DispatchBody {
        project_id: worker.project_id.clone(),
        brief,
        brief_name: Some("brief.md".into()),
        task_id: worker.task_id.as_ref().map(|id| id.as_id().to_string()),
        harness: Some(worker.harness.clone()),
        model: worker.model.clone(),
        name: Some(name.clone()),
        host_id: Some(worker.host_id.as_id().to_string()),
        placement: None,
        driver: None,
        carrier: None,
    };

    // Retire (force) reclaims worktree/target, then re-dispatch the same brief
    // under the same name on a fresh branch.
    let retired = retire_core(State(state.clone()), headers.clone(), old_id.clone(), true).await?;
    let dispatched = dispatch_core(State(state.clone()), headers, body).await?;
    let new_id = dispatched
        .get("worker")
        .and_then(|row| row.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let updated = state
        .store
        .mutate_worker(new_id.clone(), move |row| {
            row.replace_count = Some(U64(previous_count + 1));
            Ok(())
        })
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    state
        .store
        .append_audit(
            device.id,
            "worker.replace".into(),
            Some(new_id),
            json!({ "replaced": old_id, "name": name }),
        )
        .await
        .map_err(map_store)?;
    let mut result = dispatched.0.clone();
    if let Some(object) = result.as_object_mut() {
        object.insert("worker".into(), json!(updated));
        object.insert("replaced".into(), json!(old_id));
        object.insert("retire".into(), retired);
    }
    Ok(Json(result))
}

// ── stop ───────────────────────────────────────────────────────────────────

async fn stop_worker(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = caller(&state, &headers).await?;
    require_grant(&state, &device, remuda_protocol::GrantVerb::Dispatch).await?;
    let scope = caller_project_scope(&state, &device).await?;
    let worker = resolve_worker(&state, &id, &scope).await?;
    let mut command = Value::Null;
    if let Some(instance_id) = &worker.instance_id {
        let (queued, _) = state
            .store
            .queue_command(
                None,
                Some(instance_id.as_id().to_string()),
                worker.host_id.as_id().to_string(),
                "instance.close".into(),
                json!({ "instanceId": instance_id.as_id() }),
                None,
            )
            .await
            .map_err(map_store)?;
        let live = state
            .nodes
            .kind_of(worker.host_id.as_id().as_str())
            .await
            .is_some();
        command = json!(crate::http::forward_if_online(&state, queued, live).await?);
    }
    state
        .store
        .append_audit(
            device.id,
            "worker.stop".into(),
            Some(worker.meta.id.as_id().to_string()),
            json!({}),
        )
        .await
        .map_err(map_store)?;
    Ok(Json(
        json!({ "worker": worker.meta.id.as_id(), "command": command }),
    ))
}

// ── small time helpers ─────────────────────────────────────────────────────

fn now_unix() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

fn parse_unix(value: &str) -> Option<i64> {
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .map(|dt| dt.unix_timestamp())
        .ok()
}

/// Format unix seconds as the millisecond RFC3339 shape `Timestamp` requires.
fn fmt_rfc3339_millis(secs: i64) -> String {
    let dt = OffsetDateTime::from_unix_timestamp(secs).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        dt.year(),
        u8::from(dt.month()),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.millisecond(),
    )
}
