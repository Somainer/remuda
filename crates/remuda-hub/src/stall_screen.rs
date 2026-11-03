//! The screen stall fallback (main-agent.md §10, §10.5; `cb-stall-tooluse`).
//!
//! A screen carrier's journal can be quiet while its screen shows progress the
//! journal never carries: a TUI streaming a command's output, a harness whose
//! tools reach no channel the Node reads. Journal quiet alone is therefore not
//! a stall there. The inbox tick (`inbox::due_stalls`) names each busy child on
//! a screen carrier whose journal has been quiet for the project's
//! `stallThresholdMins` as a [`ScreenCandidate`]; this module reads those
//! screens, per host in its own task with the existing screen timeout, and
//! records at most one stall fact per quiet period through the inbox's record
//! path (same key, same cap).
//!
//! * A tool call the journal shows in flight is the tracker's: the tick never
//!   names such a child, and the verdict re-checks it.
//! * The screen's last observed change restarts the quiet period. The first
//!   read in a quiet period is the baseline the screen's quiet counts from; a
//!   later read that finds it unchanged lets `classify_screen` (the classifier
//!   `observe_one` uses) decide, measured from the start of the quiet period.
//! * A Node that has no screen for the instance (`supported: false`) makes it
//!   a journal carrier for that evaluation: the journal quiet rule records.
//! * Any other unreadable screen (host offline, timeout) yields nothing: the
//!   host and exit facts cover a dead carrier, and a guess would be a stall
//!   the Hub cannot show.

use crate::AppState;
use crate::inbox::{self, ScreenCandidate};
use crate::store::{Store, StoreError};
use remuda_protocol::{ScreenClass, ScreenSignals, classify_screen};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use time::OffsetDateTime;

/// Whether `driver` is a screen carrier: a driver whose Node answers
/// `tty.screen` with a grid ([`remuda_driver::reads_screen`]; today only
/// `shell-pty`). Its activity can come from the screen, and its journal can
/// be quiet while the screen moves. Every other carrier is journal-structured
/// (§10.5).
pub(crate) fn is_screen_carrier(driver: &str) -> bool {
    serde_json::from_value::<remuda_protocol::DriverKind>(serde_json::Value::from(driver))
        .is_ok_and(remuda_driver::reads_screen)
}

/// The last screen read per screen carrier, so a quiet period survives a Hub
/// restart and yields one fact (`hub.sqlite`, carried by the backup whole).
pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS inbox_screen_reads (
            instance_id TEXT PRIMARY KEY,
            digest TEXT NOT NULL,
            read_at TEXT NOT NULL,
            changed_at TEXT
         );",
    )
}

/// Drop `instance_id`'s screen read (the instance was deleted).
pub(crate) fn forget(conn: &Connection, instance_id: &str) -> Result<(), StoreError> {
    conn.execute(
        "DELETE FROM inbox_screen_reads WHERE instance_id = ?1",
        params![instance_id],
    )?;
    Ok(())
}

/// When the screen last showed progress, if a read ever saw it change.
pub(crate) fn screen_changed_at(
    conn: &Connection,
    instance_id: &str,
) -> Result<Option<OffsetDateTime>, StoreError> {
    let changed: Option<Option<String>> = conn
        .query_row(
            "SELECT changed_at FROM inbox_screen_reads WHERE instance_id = ?1",
            params![instance_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(changed.flatten().as_deref().and_then(inbox::parse_at))
}

/// A digest of what the screen shows, without the parts that repaint on
/// their own. A spinner status line counts by its signature (verb, token
/// estimate, phrase, interrupt hint), never its cycling glyph or printed
/// elapsed, so a spinner that only ticks is not progress.
pub(crate) fn progress_digest(lines: &[String], raw: bool) -> String {
    let grid = if raw {
        remuda_screen::ScreenGrid::from_raw(&lines.join("\n"))
    } else {
        remuda_screen::ScreenGrid::from_lines(lines.iter().cloned())
    };
    let mut hasher = Sha256::new();
    for row in grid.text().lines() {
        let row = row.trim_end();
        let one = remuda_screen::ScreenGrid::from_lines([row.to_owned()]);
        match remuda_screen::screen_live(&one) {
            Some(live) => hasher.update(format!("spinner:{}", live.signature()).as_bytes()),
            None => hasher.update(row.as_bytes()),
        }
        hasher.update(b"\n");
    }
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// What the sweep read of one candidate's screen.
#[derive(Debug, Clone)]
pub(crate) enum ScreenSnapshot {
    /// Rows of a live screen and the carrier-reported lifecycle.
    Lines {
        lines: Vec<String>,
        raw: bool,
        lifecycle: String,
    },
    /// No screen could be read; the category says why.
    Unreadable(&'static str),
}

/// The category of a read the Node answered with `supported: false`: this
/// instance has no screen to read, so it is not a screen carrier for this
/// evaluation and the journal quiet rule decides.
pub(crate) const SCREEN_UNSUPPORTED: &str = "screen-unsupported";

fn is_ended(lifecycle: &str) -> bool {
    matches!(lifecycle, "exited" | "failed" | "closed" | "terminated")
}

/// Decide one candidate from its screen read, in one writer transaction.
/// Returns how many facts it recorded (0 or 1). An unsupported screen falls
/// back to the journal quiet rule; any other unreadable screen (host
/// offline, a timeout) records nothing.
pub(crate) fn verdict(
    conn: &mut Connection,
    candidate: &ScreenCandidate,
    snapshot: &ScreenSnapshot,
    now: OffsetDateTime,
) -> Result<usize, StoreError> {
    let tx = crate::store::immediate_tx(conn)?;
    let written = match snapshot {
        ScreenSnapshot::Lines {
            lines,
            raw,
            lifecycle,
        } => verdict_in(&tx, candidate, lines, *raw, lifecycle, now)?,
        ScreenSnapshot::Unreadable(SCREEN_UNSUPPORTED) => journal_quiet_in(&tx, candidate, now)?,
        ScreenSnapshot::Unreadable(_) => 0,
    };
    tx.commit()?;
    Ok(written)
}

/// The candidate as the tick named it, re-read inside the verdict's
/// transaction: still a busy, live child on the same host, with no journaled
/// call in flight and a subscriber.
struct Candidate {
    activity: String,
    run: i64,
    subscriber: String,
    project: Option<String>,
}

fn still_candidate(
    conn: &Connection,
    candidate: &ScreenCandidate,
) -> Result<Option<Candidate>, StoreError> {
    let instance_id = candidate.instance_id.as_str();
    // A row that moved on since the tick is not this quiet period's.
    let row: Option<(String, String, String)> = conn
        .query_row(
            "SELECT lifecycle, activity, host_id FROM instances
              WHERE id = ?1 AND ended_at IS NULL AND fenced_at IS NULL
                AND json_extract(spec_json, '$.parentInstanceId') IS NOT NULL",
            params![instance_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((lifecycle, activity, host_id)) = row else {
        return Ok(None);
    };
    if !matches!(activity.as_str(), "working" | "draining")
        || !matches!(lifecycle.as_str(), "running" | "ready")
        || host_id != candidate.host_id
    {
        return Ok(None);
    }
    let (run, tool_in_flight) = inbox::run_and_tool_in_flight(conn, instance_id)?;
    if tool_in_flight {
        // The journal's tool rule owns a call in flight.
        return Ok(None);
    }
    let Some(subscriber) = inbox::subscriber_of(conn, instance_id)? else {
        return Ok(None);
    };
    Ok(Some(Candidate {
        activity,
        run,
        subscriber,
        project: inbox::project_of(conn, instance_id)?,
    }))
}

/// The Node has no screen for this instance: the journal quiet rule the
/// tick applies to every other carrier decides.
fn journal_quiet_in(
    conn: &Connection,
    candidate: &ScreenCandidate,
    now: OffsetDateTime,
) -> Result<usize, StoreError> {
    let Some(live) = still_candidate(conn, candidate)? else {
        return Ok(0);
    };
    let instance_id = candidate.instance_id.as_str();
    let Some(due) = inbox::due_quiet(conn, instance_id, live.project.as_deref(), true, now)? else {
        return Ok(0);
    };
    Ok(usize::from(inbox::record_quiet_stall(
        conn,
        &live.subscriber,
        instance_id,
        live.run,
        &due,
        None,
    )?))
}

fn verdict_in(
    conn: &Connection,
    candidate: &ScreenCandidate,
    lines: &[String],
    raw: bool,
    screen_lifecycle: &str,
    now: OffsetDateTime,
) -> Result<usize, StoreError> {
    let Some(live) = still_candidate(conn, candidate)? else {
        return Ok(0);
    };
    let instance_id = candidate.instance_id.as_str();
    let threshold = inbox::quiet_threshold(conn, live.project.as_deref())?;
    let Some(journal_quiet) = inbox::journal_quiet_since(conn, instance_id)? else {
        return Ok(0);
    };

    let digest = progress_digest(lines, raw);
    let read_at = crate::config::rfc3339(now);
    let prior: Option<(String, String, Option<String>)> = conn
        .query_row(
            "SELECT digest, read_at, changed_at FROM inbox_screen_reads WHERE instance_id = ?1",
            params![instance_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    // A read from before the journal's last row belongs to an earlier quiet
    // period and says nothing about this one.
    let comparable =
        prior.filter(|(_, at, _)| inbox::parse_at(at).is_some_and(|at| at >= journal_quiet));
    let changed_at = match comparable {
        None => {
            // The first read of this quiet period is the baseline, as
            // `observe_one` takes its first observation: what the screen did
            // before it is unknown, so its quiet is counted from this read.
            store_read(conn, instance_id, &digest, &read_at, Some(&read_at))?;
            return Ok(0);
        }
        Some((previous, _, _)) if previous != digest => {
            // Progress the journal did not carry: the quiet period restarts.
            store_read(conn, instance_id, &digest, &read_at, Some(&read_at))?;
            return Ok(0);
        }
        Some((_, _, changed_at)) => {
            store_read(conn, instance_id, &digest, &read_at, changed_at.as_deref())?;
            changed_at.as_deref().and_then(inbox::parse_at)
        }
    };
    let quiet_since = changed_at.map_or(journal_quiet, |changed| changed.max(journal_quiet));
    let Some(stalled_at) =
        inbox::window_minutes(threshold).and_then(|window| quiet_since.checked_add(window))
    else {
        inbox::report_once("stall-window-out-of-range");
        return Ok(0);
    };
    if stalled_at > now {
        return Ok(0);
    }

    // The classifier `observe_one` uses, over the live screen. A live row is
    // `running` to it (it reads `ready` as idle, the Hub row says busy); a
    // carrier that reports its process ended is gone, not stalled.
    let class_lifecycle = if is_ended(screen_lifecycle) {
        screen_lifecycle
    } else {
        "running"
    };
    let dialog_title = {
        let grid = if raw {
            remuda_screen::ScreenGrid::from_raw(&lines.join("\n"))
        } else {
            remuda_screen::ScreenGrid::from_lines(lines.iter().cloned())
        };
        remuda_screen::first_run_dialog(&grid).map(|dialog| dialog.title.to_owned())
    };
    let signals = ScreenSignals {
        lines,
        lifecycle: class_lifecycle,
        activity: &live.activity,
        host_online: true,
        now_unix: now.unix_timestamp(),
        last_activity_unix: Some(quiet_since.unix_timestamp()),
        stall_mins: threshold,
        known_done_sha: None,
        known_blocked: None,
        raw,
        screen_available: true,
        dialog_title: dialog_title.as_deref(),
        journal_lines: &[],
        turn_error: false,
        tail_error: false,
        last_error_line: None,
        lifecycle_reason: None,
    };
    let class = classify_screen(&signals);
    if !matches!(class, ScreenClass::Stalled { .. }) {
        tracing::debug!(
            class = class_label(&class),
            "inbox screen read: not a stall"
        );
        return Ok(0);
    }
    // Keyed by when the quiet period started, like the journal quiet stall:
    // every read of the same period is the same fact.
    let due = inbox::DueQuiet {
        quiet_since,
        threshold,
        stalled_at,
    };
    Ok(usize::from(inbox::record_quiet_stall(
        conn,
        &live.subscriber,
        instance_id,
        live.run,
        &due,
        Some("screen"),
    )?))
}

fn store_read(
    conn: &Connection,
    instance_id: &str,
    digest: &str,
    read_at: &str,
    changed_at: Option<&str>,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO inbox_screen_reads (instance_id, digest, read_at, changed_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(instance_id) DO UPDATE SET
            digest = excluded.digest, read_at = excluded.read_at,
            changed_at = excluded.changed_at",
        params![instance_id, digest, read_at, changed_at],
    )?;
    Ok(())
}

/// A log-safe label for a classification: never its reason or screen line.
fn class_label(class: &ScreenClass) -> &'static str {
    match class {
        ScreenClass::Working => "working",
        ScreenClass::Done { .. } => "done",
        ScreenClass::Blocked { .. } => "blocked",
        ScreenClass::IdleApiError { .. } => "idle-api-error",
        ScreenClass::Stalled { .. } => "stalled",
        ScreenClass::Gone { .. } => "gone",
        ScreenClass::Failed { .. } => "failed",
    }
}

impl Store {
    /// [`verdict`] on the writer thread.
    pub(crate) async fn inbox_screen_verdict(
        &self,
        candidate: ScreenCandidate,
        snapshot: ScreenSnapshot,
        now: OffsetDateTime,
    ) -> Result<usize, StoreError> {
        self.run_named("inbox_screen_verdict", move |conn| {
            verdict(conn, &candidate, &snapshot, now)
        })
        .await
    }
}

/// Hosts whose screen sweep is still running. A host is swept by at most one
/// task at a time; a sweep still waiting on an unreachable host makes the
/// next tick skip that host only.
pub(crate) type SweepsInFlight = Arc<Mutex<HashSet<String>>>;

/// The claim on one host's sweep, released when its task ends (or is
/// aborted).
struct HostClaim {
    sweeps: SweepsInFlight,
    host_id: String,
}

impl HostClaim {
    fn take(sweeps: &SweepsInFlight, host_id: &str) -> Option<Self> {
        let mut held = sweeps.lock().unwrap_or_else(|poison| poison.into_inner());
        held.insert(host_id.to_owned()).then(|| Self {
            sweeps: Arc::clone(sweeps),
            host_id: host_id.to_owned(),
        })
    }
}

impl Drop for HostClaim {
    fn drop(&mut self) {
        self.sweeps
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&self.host_id);
    }
}

/// Read every candidate's screen, one task per host, each read bounded by
/// `timeout`, and decide each. Returns the spawned tasks (the facts each
/// recorded) by host; the caller need not await them.
pub(crate) fn sweep(
    state: &AppState,
    candidates: Vec<ScreenCandidate>,
    now: OffsetDateTime,
    timeout: Duration,
) -> Vec<(String, tokio::task::JoinHandle<usize>)> {
    let mut by_host: BTreeMap<String, Vec<ScreenCandidate>> = BTreeMap::new();
    for candidate in candidates {
        by_host
            .entry(candidate.host_id.clone())
            .or_default()
            .push(candidate);
    }
    let mut tasks = Vec::with_capacity(by_host.len());
    for (host_id, candidates) in by_host {
        let Some(claim) = HostClaim::take(&state.screen_sweeps, &host_id) else {
            tracing::debug!(
                candidates = candidates.len(),
                "inbox screen sweep still running for a host; skipped this tick"
            );
            continue;
        };
        let state = state.clone();
        let task = tokio::spawn(async move {
            let _claim = claim;
            sweep_host(&state, candidates, now, timeout).await
        });
        tasks.push((host_id, task));
    }
    tasks
}

async fn sweep_host(
    state: &AppState,
    candidates: Vec<ScreenCandidate>,
    now: OffsetDateTime,
    timeout: Duration,
) -> usize {
    let mut written = 0;
    for candidate in candidates {
        let read = crate::worker_watch::read_screen(
            state,
            &candidate.host_id,
            &candidate.instance_id,
            timeout,
        )
        .await;
        let snapshot = match read {
            crate::worker_watch::ScreenRead::Screen {
                lines,
                lifecycle,
                raw,
                screen_available: true,
            } => ScreenSnapshot::Lines {
                lines,
                raw,
                lifecycle,
            },
            crate::worker_watch::ScreenRead::Screen { .. } => {
                ScreenSnapshot::Unreadable(SCREEN_UNSUPPORTED)
            }
            crate::worker_watch::ScreenRead::Gone(code) => ScreenSnapshot::Unreadable(code),
        };
        if let ScreenSnapshot::Unreadable(code) = snapshot
            && code != SCREEN_UNSUPPORTED
        {
            // §10.1 item 9: the category only.
            tracing::debug!(code, "inbox screen read: unreadable, no stall fact");
            if matches!(code, "host-offline" | "screen-unavailable") {
                // The host did not answer in time: its other screens would
                // wait out the same timeout. The next tick tries again.
                break;
            }
            continue;
        }
        match state
            .store
            .inbox_screen_verdict(candidate, snapshot, now)
            .await
        {
            Ok(recorded) => written += recorded,
            Err(err) => tracing::error!(
                code = inbox::error_code(&err),
                "inbox screen verdict failed"
            ),
        }
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn a_ticking_spinner_is_not_progress() {
        let before = rows(&[
            "● Bash(sleep 400)",
            "✻ Razzmatazzing… (1m 02s · ↓ 1.2k tokens · esc to interrupt)",
        ]);
        let after = rows(&[
            "● Bash(sleep 400)",
            "✢ Razzmatazzing… (4m 51s · ↓ 1.2k tokens · esc to interrupt)",
        ]);
        assert_eq!(
            progress_digest(&before, false),
            progress_digest(&after, false)
        );
    }

    #[test]
    fn new_rows_and_streamed_tokens_are_progress() {
        let base = rows(&["$ cargo test", "running 10 tests"]);
        let more = rows(&["$ cargo test", "running 10 tests", "test a ... ok"]);
        assert_ne!(progress_digest(&base, false), progress_digest(&more, false));
        let one = rows(&["✻ Razzmatazzing… (1m 02s · ↓ 1.2k tokens)"]);
        let two = rows(&["✻ Razzmatazzing… (1m 03s · ↓ 1.9k tokens)"]);
        assert_ne!(progress_digest(&one, false), progress_digest(&two, false));
    }

    #[test]
    fn screen_carriers_are_the_drivers_that_read_a_screen() {
        assert!(is_screen_carrier("shell-pty"));
        // r2: claude-pty and generic-pty keep `Driver::screen_read`'s default
        // `None`, so the Node answers `supported: false` for them.
        for driver in [
            "claude-pty",
            "generic-pty",
            "claude-sdk",
            "claude-print",
            "claude-bg",
            "unknown-driver",
            "codex-appserver",
            "grok-acp",
            "agy-print",
        ] {
            assert!(!is_screen_carrier(driver), "{driver}");
        }
    }
}
