//! Worker roster: the durable record of one dispatched T3 worker; design
//! coordinator-hierarchy.md §2.2 (field ④, the productised `relay-workers.tsv`).
//!
//! Every resource a worker occupies — name, branch, worktree, target dir, port
//! block — is assigned by the Hub (§2.4), never self-reported by the worker.
//! The same document carries the worker's lifecycle state so a coordinator
//! (human or agent) can rebuild dispatch→watch→retire from the Hub alone.

use crate::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Initial roster state: resources provisioned, agent not yet observed working.
pub const WORKER_STATE_DISPATCHED: &str = "dispatched";
/// Agent launched and the brief delivered.
pub const WORKER_STATE_WORKING: &str = "working";
/// Worker reported `DONE <sha>`; the sha is carried here (still a claim, not a
/// gate — design §7 I1).
pub const WORKER_STATE_DONE: &str = "done";
/// Worker reported `BLOCKED <reason>`.
pub const WORKER_STATE_BLOCKED: &str = "blocked";
/// Herdr tab closed, worktree and target dir reclaimed.
pub const WORKER_STATE_RETIRED: &str = "retired";

/// Lifecycle of a dispatched worker; design §1.1 goal 6.
///
/// Wire shape is adjacently tagged: `{"state":"done","sha":"…"}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum WorkerState {
    /// Resources provisioned, agent not yet confirmed working.
    #[default]
    Dispatched,
    /// Agent launched and working its brief.
    Working,
    /// Worker replied `DONE <sha>`; the sha is a claim until `land` (I1).
    Done {
        /// Claimed landed sha.
        sha: String,
    },
    /// Worker replied `BLOCKED <reason>`.
    Blocked {
        /// Machine-and-human-readable block reason.
        reason: String,
    },
    /// Tab closed and worktree/target dir reclaimed through the Node.
    Retired,
}

impl WorkerState {
    /// Canonical wire discriminant.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Dispatched => WORKER_STATE_DISPATCHED,
            Self::Working => WORKER_STATE_WORKING,
            Self::Done { .. } => WORKER_STATE_DONE,
            Self::Blocked { .. } => WORKER_STATE_BLOCKED,
            Self::Retired => WORKER_STATE_RETIRED,
        }
    }

    /// True when the worker is still consuming host resources. Retire refuses
    /// this without `--force`.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::Retired)
    }

    /// True for the `working` state.
    #[must_use]
    pub fn is_working(&self) -> bool {
        matches!(self, Self::Working)
    }

    /// Parse a state update from wire form. `done` requires `sha`, `blocked`
    /// requires `reason`; `dispatched` may not be restored by a state patch.
    pub fn from_update(
        kind: &str,
        sha: Option<&str>,
        reason: Option<&str>,
    ) -> Result<Self, String> {
        match kind {
            WORKER_STATE_WORKING => Ok(Self::Working),
            WORKER_STATE_DONE => {
                let sha = sha
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "done requires a sha".to_string())?;
                Ok(Self::Done {
                    sha: sha.to_string(),
                })
            }
            WORKER_STATE_BLOCKED => {
                let reason = reason
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "blocked requires a reason".to_string())?;
                Ok(Self::Blocked {
                    reason: reason.to_string(),
                })
            }
            other => Err(format!("invalid worker state {other}")),
        }
    }
}

// ── watch classification (M1 batch 5b) ─────────────────────────────────────

/// Worker is actively running its brief (no report line, no trouble).
pub const WATCH_WORKING: &str = "working";
/// A freshly-read `DONE <sha>` line, newer than the roster's known tip.
pub const WATCH_DONE: &str = "done";
/// A freshly-read `BLOCKED <reason>` line.
pub const WATCH_BLOCKED: &str = "blocked";
/// Agent idle while the screen shows an API/connection error — needs a nudge.
pub const WATCH_IDLE_API_ERROR: &str = "idle-api-error";
/// One turn has run with no screen/process activity past the stall threshold.
pub const WATCH_STALLED: &str = "stalled";
/// Host offline, instance closed, or no readable live carrier.
pub const WATCH_GONE: &str = "gone";
/// Instance lifecycle failed/exited after an errored turn, or the last turn
/// result itself errored (2026-09-17 watch-failed-1).
pub const WATCH_FAILED: &str = "failed";

/// Default quiet window before a busy, silent turn is called stalled. The
/// behaviour spec productised here ("a single API turn running for >30 min
/// with no process activity") uses 30 minutes; per-project policy can narrow
/// it (§3.2 `policy.configurable.stallThresholdMins`).
pub const DEFAULT_STALL_THRESHOLD_MINS: i64 = 30;

/// The point-in-time status `remuda watch` derives from a worker's screen.
///
/// Adjacently tagged on `status`, so the wire shape is
/// `{"status":"done","sha":"…"}` / `{"status":"blocked","reason":"…"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum WorkerWatchStatus {
    /// Working normally.
    Working,
    /// New `DONE <sha>` on screen (not an echo of a known tip).
    Done {
        /// Claimed landed sha.
        sha: String,
    },
    /// New `BLOCKED <reason>` on screen.
    Blocked {
        /// Block reason text.
        reason: String,
    },
    /// Idle with an API error / connection drop / retry loop on screen.
    IdleApiError,
    /// Busy but silent past the stall threshold.
    Stalled,
    /// Host offline, instance closed, or carrier gone.
    ///
    /// `reason` is the machine-readable why — the carrier code
    /// (`host-offline`, `instance-closed`, …) or the instance row's own
    /// `lastError` (`node-epoch-changed`, `host-lost`, …) when the row is
    /// authoritative. It is carried rather than folded into `detail` alone so
    /// `remuda watch`'s reason column can print it: a settled
    /// `node-epoch-changed` is the one fact that tells the owner the
    /// conversation can still be resumed (2026-09-18 demo).
    Gone {
        /// Machine-readable why.
        reason: String,
    },
    /// The instance lifecycle failed (or exited after an errored turn), or the
    /// last turn result errored. The worker cannot make progress as launched;
    /// unlike `gone` there is a concrete cause to report.
    Failed {
        /// First line of the last assistant error message, else the lifecycle
        /// reason code.
        reason: String,
    },
}

impl WorkerWatchStatus {
    /// Canonical wire discriminant.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Working => WATCH_WORKING,
            Self::Done { .. } => WATCH_DONE,
            Self::Blocked { .. } => WATCH_BLOCKED,
            Self::IdleApiError => WATCH_IDLE_API_ERROR,
            Self::Stalled => WATCH_STALLED,
            Self::Gone { .. } => WATCH_GONE,
            Self::Failed { .. } => WATCH_FAILED,
        }
    }

    /// True for the all-good terminal state `remuda watch --follow` waits for.
    #[must_use]
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Done { .. })
    }
}

/// One persisted `remuda watch` observation on a roster row.
///
/// Besides the current status it carries the echo-suppression baselines
/// (`lastDoneSha` / `lastBlocked` — a resumed session re-shows its old DONE)
/// and the activity bookkeeping the stall detector needs across polls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerWatch {
    /// The classified status (flattened, so its `status`/`sha`/`reason` ride
    /// this object directly).
    #[serde(flatten)]
    pub status: WorkerWatchStatus,
    /// When this observation was made (UTC RFC3339).
    pub observed_at: crate::Timestamp,
    /// The screen line or error that drove the classification, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Most recent DONE sha already treated as real; an equal later line is an
    /// echo, not a new report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_done_sha: Option<String>,
    /// Most recent BLOCKED reason already treated as real.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_blocked: Option<String>,
    /// Digest of the last screen that differed from the one before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_screen_digest: Option<String>,
    /// Last observed activity (screen change or journal event), as UTC
    /// RFC3339; the stall detector compares this to the observation time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<crate::Timestamp>,
}

/// Normalised inputs to the screen classifier. All time is Unix seconds so the
/// rule is testable without a clock.
#[derive(Debug, Clone, Copy)]
pub struct ScreenSignals<'a> {
    /// Visible screen rows, oldest first (the Node `tty.screen` `lines`).
    pub lines: &'a [String],
    /// Instance lifecycle (`ready` / `closed` / …).
    pub lifecycle: &'a str,
    /// Instance activity (`idle` / `blocked` / …).
    pub activity: &'a str,
    /// Whether the worker's host is currently connected.
    pub host_online: bool,
    /// Observation time (Unix seconds).
    pub now_unix: i64,
    /// Time of the last real activity (screen change / journal event), if any.
    pub last_activity_unix: Option<i64>,
    /// Quiet window that makes a busy turn stalled, in minutes.
    pub stall_mins: i64,
    /// The DONE sha already known to the roster (echo baseline).
    pub known_done_sha: Option<&'a str>,
    /// The BLOCKED reason already known to the roster (echo baseline).
    pub known_blocked: Option<&'a str>,
    /// Whether `lines` is a raw VT ring tail rather than an emulated row grid.
    /// A headless `tty.screen` (no live viewer driving the emulator) returns
    /// the ANSI-stripped ring tail: one string of cursor-positioned,
    /// width-padded repaint rows with no `\n` boundaries. In that mode the
    /// report token is located anywhere in the tail (the `coord-watch-all.sh`
    /// grep semantics) instead of requiring a start-of-line anchor.
    pub raw: bool,
    /// Whether a live screen was actually read for this observation. Print
    /// drivers (`claude-print`) and dead carriers have none; when false the
    /// text signals (`journal_lines`, `last_error_line`, `turn_error`) come
    /// from the mirrored instance journal instead, and the Hub marks the
    /// persisted detail `screen-unavailable`.
    pub screen_available: bool,
    /// Title of a first-run / native permission dialog detected on the live
    /// screen (folder trust, auto-mode outside reads, …), if one owns the
    /// keyboard. Classifies as [`ScreenClass::Blocked`] with the title as
    /// reason, regardless of reported activity: a dialog makes a worker look
    /// busy while it is actually parked (dispatch-onboarding-1). The rule is
    /// deliberately *not* echo-deduped — a modal still on screen stays blocked
    /// rather than collapsing back to working.
    pub dialog_title: Option<&'a str>,
    /// Assistant text blocks recovered from the instance journal (transcript
    /// order), used for report/error classification only when
    /// `screen_available` is false.
    pub journal_lines: &'a [String],
    /// The last turn result (`lifecycle/turn` `result`) was an error.
    pub turn_error: bool,
    /// The instance journal's last assistant text block grades as a hard
    /// error. Distinguishes a fatal exit after an error from a session that
    /// hit an error, recovered, reported DONE, and then exited cleanly.
    pub tail_error: bool,
    /// First line of the last assistant error message seen in the journal
    /// (e.g. `API Error: 400 requested model is not available`), if any.
    pub last_error_line: Option<&'a str>,
    /// Machine-readable reason code / driver error recorded on the Hub
    /// instance row, used as the failure reason when no error message exists.
    pub lifecycle_reason: Option<&'a str>,
}

/// The classified screen state plus the evidence line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenClass {
    /// Working normally.
    Working,
    /// A new `DONE <sha>` line.
    Done {
        /// Claimed sha.
        sha: String,
        /// Matched screen line.
        line: String,
    },
    /// A new `BLOCKED <reason>` line.
    Blocked {
        /// Reason text.
        reason: String,
        /// Matched screen line.
        line: String,
    },
    /// Idle with an API/connection error on screen.
    IdleApiError {
        /// Error fragment seen.
        fragment: String,
    },
    /// Busy and silent for at least the stall threshold.
    Stalled {
        /// Minutes without activity.
        quiet_mins: i64,
    },
    /// Host offline / instance closed / carrier gone.
    Gone {
        /// Machine-readable why: a carrier code (`host-offline`,
        /// `instance-closed`, …) or the instance row's own `lastError`
        /// (`node-epoch-changed`, `host-lost`, …) when the row is authoritative
        /// and already says how it died.
        reason: String,
    },
    /// The instance failed or exited after an errored turn, or the last turn
    /// result errored. Distinct from `Gone`: the worker is dead *with a known
    /// cause*, so a report can say what the owner must fix (watch-failed-1,
    /// 2026-09-17).
    Failed {
        /// First line of the last assistant error message, else the lifecycle
        /// reason code.
        reason: String,
        /// Evidence line (same text as `reason`, truncated).
        line: String,
    },
}

/// TUI list markers that may prefix a worker's report line; one is stripped at
/// match time, mirroring `instance wait --until 'line:(?m)^DONE'`.
const LINE_MARKERS: &[char] = &['•', '●', '◆', '▸', '▪', '-', '*', '>'];

/// Substrings that mark a line as the brief contract echoing back (a resumed
/// session re-renders its prompt) rather than a worker report.
const BRIEF_ECHO_HINTS: &[&str] = &[
    "<sha>",
    "<reason>",
    "last line",
    "reply on one line",
    "reply with",
    "reply done",
    "done tip",
    "further input",
    "for example",
];

/// Screen fragments that mean the harness is in an API error / retry loop.
const API_ERROR_FRAGMENTS: &[&str] = &["api error", "connection closed", "retrying"];

fn trim_report_line(raw: &str) -> &str {
    let mut line = raw.trim_start();
    if let Some(first) = line.chars().next()
        && LINE_MARKERS.contains(&first)
    {
        line = line[first.len_utf8()..].trim_start();
    }
    line.trim_end()
}

fn is_brief_echo(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    BRIEF_ECHO_HINTS.iter().any(|hint| lower.contains(hint))
}

/// Parse a `DONE <7-40 hex>` report at the start of a normalised line.
fn parse_done(line: &str) -> Option<String> {
    let rest = line.strip_prefix("DONE")?;
    if !rest.starts_with(|c: char| c.is_whitespace()) {
        return None;
    }
    let token: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    if (7..=40).contains(&token.len()) {
        Some(token)
    } else {
        None
    }
}

/// Parse a `BLOCKED <reason>` report at the start of a normalised line.
fn parse_blocked(line: &str) -> Option<String> {
    let rest = line.strip_prefix("BLOCKED")?;
    if !rest.starts_with(|c: char| c.is_whitespace()) {
        return None;
    }
    let reason = rest.trim().trim_start_matches([':', '-', '–']).trim();
    if reason.is_empty() {
        return None;
    }
    let truncated: String = reason.chars().take(120).collect();
    Some(truncated)
}

/// Lifecycles that mean the instance died *with an error* (watch-failed-1).
fn is_failed_lifecycle(lifecycle: &str) -> bool {
    lifecycle.eq_ignore_ascii_case("failed")
}

/// Lifecycles that mean the carrier/process is simply gone, without an error
/// diagnosis of their own.
fn is_closed_lifecycle(lifecycle: &str) -> bool {
    matches!(
        lifecycle.to_ascii_lowercase().as_str(),
        "closed" | "terminated"
    )
}

fn is_idle(lifecycle: &str, activity: &str) -> bool {
    let activity = activity.to_ascii_lowercase();
    let lifecycle = lifecycle.to_ascii_lowercase();
    if activity == "blocked" || activity == "waiting-interaction" {
        return false;
    }
    activity == "idle" || lifecycle == "ready" || lifecycle == "idle"
}

/// True when a turn is running (not idle, not blocked, not still starting).
fn is_busy(lifecycle: &str, activity: &str) -> bool {
    if is_idle(lifecycle, activity) {
        return false;
    }
    let activity = activity.to_ascii_lowercase();
    if activity == "blocked" || activity == "waiting-interaction" {
        return false;
    }
    !matches!(
        lifecycle.to_ascii_lowercase().as_str(),
        "requested" | "creating" | "starting" | ""
    )
}

/// The bottom-most DONE/BLOCKED report on a screen, classified as a new report
/// or an echo of a known tip. Returns `(keyword-index, class)` where the index
/// lets the caller compare DONE vs BLOCKED ordering against other signals.
fn bottom_report(signals: &ScreenSignals<'_>) -> Option<ScreenClass> {
    for (idx, raw) in signals.lines.iter().enumerate().rev() {
        let line = trim_report_line(raw);
        if is_brief_echo(line) {
            continue;
        }
        if let Some(sha) = parse_done(line) {
            if signals.known_done_sha == Some(sha.as_str()) {
                // Old tip re-shown by a resumed/rescrolled session: an echo,
                // not a new report. Fall through to trouble detection.
                return None;
            }
            return Some(ScreenClass::Done {
                sha,
                line: line.to_string(),
            });
        }
        if let Some(reason) = parse_blocked(line) {
            if signals.known_blocked == Some(reason.as_str()) {
                return None;
            }
            return Some(ScreenClass::Blocked {
                reason,
                line: line.to_string(),
            });
        }
        let _ = idx;
    }
    None
}

/// Locate a report token in a raw VT ring tail.
///
/// Unlike [`bottom_report`] this does not need a line boundary: the tail is a
/// run of cursor-positioned, width-padded repaint rows with no newlines. It
/// mirrors the `coord-watch-all.sh` greps (`DONE [0-9a-f]{7,40}`,
/// `BLOCKED …`) by searching the flattened text and keeping whichever keyword
/// occurs latest (bottom-most). The strict 7–40 hex suffix is a strong guard
/// against prose such as "reply DONE <sha>", and the brief echo is excluded by
/// the same known-tip / placeholder checks.
fn raw_tail_report(signals: &ScreenSignals<'_>) -> Option<ScreenClass> {
    let text = signals.lines.join(" ");
    let done = rfind_token(&text, "DONE").and_then(|(idx, _)| {
        parse_done_at(&text[idx..])
            .filter(|(sha, _)| signals.known_done_sha != Some(sha.as_str()))
            .map(|(sha, span)| ScreenClass::Done {
                sha,
                line: snippet(&text, idx, span + 5),
            })
    });
    let blocked = rfind_token(&text, "BLOCKED").and_then(|(idx, _)| {
        parse_blocked_at(&text[idx..])
            .filter(|(reason, _)| signals.known_blocked != Some(reason.as_str()))
            .filter(|(reason, _)| !is_brief_echo(reason))
            .map(|(reason, span)| ScreenClass::Blocked {
                reason,
                line: snippet(&text, idx, span + 7),
            })
    });
    // Keep the bottom-most (latest in the tail) of the two.
    let di = rfind_token(&text, "DONE")
        .map(|(i, _)| i)
        .unwrap_or(usize::MAX);
    let bi = rfind_token(&text, "BLOCKED")
        .map(|(i, _)| i)
        .unwrap_or(usize::MAX);
    match (done, blocked) {
        (Some(done), Some(blocked)) => {
            if di >= bi {
                Some(done)
            } else {
                Some(blocked)
            }
        }
        (Some(done), None) => Some(done),
        (None, Some(blocked)) => Some(blocked),
        _ => None,
    }
}

/// Byte index of the last case-sensitive occurrence of `token` that is a whole
/// word (preceded by a non-alphanumeric boundary).
fn rfind_token(text: &str, token: &str) -> Option<(usize, usize)> {
    let mut search = text;
    let mut base = 0usize;
    let mut found = None;
    while let Some(rel) = search.find(token) {
        let idx = base + rel;
        let boundary_before = text[..idx]
            .chars()
            .next_back()
            .is_none_or(|ch| !ch.is_ascii_alphanumeric());
        if boundary_before {
            found = Some((idx, token.len()));
        }
        // Advance past this occurrence.
        let next = rel + token.len();
        base += next;
        search = &search[next..];
    }
    found
}

/// Parse `DONE <7-40 hex>` starting at byte 0 of a substring; returns the sha
/// and the consumed length.
fn parse_done_at(s: &str) -> Option<(String, usize)> {
    let rest = s.strip_prefix("DONE")?;
    let after = rest.strip_prefix(|c: char| c.is_whitespace())?;
    let take = after.bytes().take_while(|b| b.is_ascii_hexdigit()).count();
    if (7..=40).contains(&take) {
        Some((after[..take].to_string(), take))
    } else {
        None
    }
}

/// Parse `BLOCKED <reason>` at byte 0; reason up to the first control/padding
/// boundary or 120 chars. Returns (reason, consumed length).
fn parse_blocked_at(s: &str) -> Option<(String, usize)> {
    let rest = s.strip_prefix("BLOCKED")?;
    let after = rest.strip_prefix(|c: char| c.is_whitespace())?;
    let end = after
        .char_indices()
        .find(|(_, ch)| ch.is_whitespace() && (*ch == '\n' || *ch == '\r'))
        .map(|(i, _)| i)
        .unwrap_or(after.len());
    // A raw row is space-padded to the terminal width; trim that padding.
    let mut end = end.min(120);
    while end > 0 && after.as_bytes()[end - 1] == b' ' {
        end -= 1;
    }
    let reason = after[..end].trim_start_matches([':', '-', '–']).trim();
    if reason.is_empty() || reason.starts_with('<') {
        return None;
    }
    Some((reason.to_string(), end))
}

/// A short evidence snippet around a matched token.
fn snippet(text: &str, start: usize, consumed: usize) -> String {
    let begin = start.saturating_sub(20);
    let end = (start + consumed + 20).min(text.len());
    text[begin..end].trim().to_string()
}

/// First non-empty line of an error message, trimmed and capped the same way
/// a BLOCKED reason is.
fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(text.trim());
    line.chars().take(120).collect()
}

/// Pick the failure reason per watch-failed-1: first line of the last
/// assistant error message, else the lifecycle reason code, else a static
/// fallback derived from what triggered the classification.
fn failure_reason(signals: &ScreenSignals<'_>, fallback: &'static str) -> (String, String) {
    if let Some(line) = signals
        .last_error_line
        .map(first_line)
        .filter(|s| !s.is_empty())
    {
        return (line.clone(), line);
    }
    if let Some(reason) = signals
        .lifecycle_reason
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .map(first_line)
    {
        return (reason.clone(), reason);
    }
    (fallback.to_string(), fallback.to_string())
}

/// The bottom-most DONE/BLOCKED report inside journaled assistant text.
///
/// Screenless (print) drivers expose no live grid; their final assistant
/// message is the same `DONE <sha>` / `BLOCKED <reason>` contract line the
/// screen classifier looks for, so the matching and echo rules are the same —
/// only the source differs. One journaled text block may span several lines.
fn journal_report(signals: &ScreenSignals<'_>) -> Option<ScreenClass> {
    for text in signals.journal_lines.iter().rev() {
        if is_brief_echo(text) {
            continue;
        }
        for raw in text.lines().rev() {
            let line = trim_report_line(raw);
            if is_brief_echo(line) {
                continue;
            }
            if let Some(sha) = parse_done(line) {
                // Old tip replayed from the transcript: an echo, not a new
                // report; stop looking, mirroring `bottom_report`.
                if signals.known_done_sha == Some(sha.as_str()) {
                    return None;
                }
                return Some(ScreenClass::Done {
                    sha,
                    line: line.to_string(),
                });
            }
            if let Some(reason) = parse_blocked(line) {
                if signals.known_blocked == Some(reason.as_str()) {
                    return None;
                }
                return Some(ScreenClass::Blocked {
                    reason,
                    line: line.to_string(),
                });
            }
        }
    }
    None
}

/// Classify a worker's visible screen per the coordinator watcher practice:
/// failed turn/instance → gone → new DONE/BLOCKED → idle-after-API-error →
/// stalled → working.
#[must_use]
pub fn classify_screen(signals: &ScreenSignals<'_>) -> ScreenClass {
    let lifecycle = signals.lifecycle.to_ascii_lowercase();
    // A failed instance, a turn that ended in error, or an exit whose *last*
    // assistant message was an error — regardless of screen availability. A
    // *clean* exit is different: a print worker that hit an error, recovered
    // and reported DONE exits 0, so the fresh journaled DONE below is what
    // classifies it; an exit with no report falls through to gone.
    let dead_after_error = lifecycle == "exited" && (signals.turn_error || signals.tail_error);
    if is_failed_lifecycle(&lifecycle) || dead_after_error {
        let (reason, line) = failure_reason(signals, "instance-failed");
        return ScreenClass::Failed { reason, line };
    }
    if signals.turn_error {
        let (reason, line) = failure_reason(signals, "turn-error");
        return ScreenClass::Failed { reason, line };
    }
    if !signals.host_online {
        return ScreenClass::Gone {
            reason: "host-offline".to_string(),
        };
    }
    // A closed/terminated carrier has nothing left to read.
    if is_closed_lifecycle(&lifecycle) {
        return ScreenClass::Gone {
            reason: "instance-closed".to_string(),
        };
    }
    // A fresh DONE/BLOCKED report outranks a clean exit: a print worker's
    // process exits 0 right after printing its report, and that report is the
    // classification. (An errored exit was already caught above.)
    if signals.screen_available {
        if let Some(report) = bottom_report(signals) {
            return report;
        }
        // Headless raw-ring tail: locate the report token anywhere in the
        // concatenated repaint (no start-of-line boundary available).
        if signals.raw
            && let Some(report) = raw_tail_report(signals)
        {
            return report;
        }
    } else if let Some(report) = journal_report(signals) {
        return report;
    }
    // Exited with no fresh report: the process is gone. The row's own
    // `lastError` is the honest why when it has one — a Hub-settled
    // `node-epoch-changed` is the reason the worker died, and a bare
    // `instance-closed` would throw that away and read as if the owner had
    // stopped it.
    if lifecycle == "exited" {
        return ScreenClass::Gone {
            reason: signals
                .lifecycle_reason
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
                .map(first_line)
                .unwrap_or_else(|| "instance-closed".to_string()),
        };
    }
    // A native first-run/permission dialog on the live screen owns the
    // keyboard: classify blocked with its title, no matter what activity the
    // carrier guessed mid-boot. This is intentionally below report detection
    // (a fresh DONE/BLOCKED report is the worker speaking, not a modal) and
    // above idle/stall heuristics, and it is not echo-deduped — a dialog that
    // never went away must keep reporting blocked (dispatch-onboarding-1).
    if signals.screen_available
        && let Some(title) = signals.dialog_title
        && !title.trim().is_empty()
    {
        let line = title.trim().to_string();
        return ScreenClass::Blocked {
            reason: line.clone(),
            line,
        };
    }
    // Idle after API error / connection drop / retry loop. The text source is
    // the live screen when present, otherwise the journaled assistant text.
    let trouble_lines: &[String] = if signals.screen_available {
        signals.lines
    } else {
        signals.journal_lines
    };
    if is_idle(&lifecycle, signals.activity) {
        for raw in trouble_lines {
            let lower = raw.to_ascii_lowercase();
            if let Some(fragment) = API_ERROR_FRAGMENTS
                .iter()
                .find(|needle| lower.contains(**needle))
            {
                return ScreenClass::IdleApiError {
                    fragment: (*fragment).to_string(),
                };
            }
        }
    }
    // A single turn busy with no activity past the stall threshold.
    if is_busy(&lifecycle, signals.activity)
        && let Some(last) = signals.last_activity_unix
        && signals.stall_mins > 0
    {
        let quiet_secs = signals.now_unix - last;
        let threshold = signals.stall_mins * 60;
        if quiet_secs >= threshold {
            return ScreenClass::Stalled {
                quiet_mins: quiet_secs / 60,
            };
        }
    }
    ScreenClass::Working
}

/// Logical key names accepted by `worker answer`, already encoded for
/// `tty.write` (names + base64 PTY bytes). Mirrors the CLI's key map so the
/// Hub can answer a dialog without the bytes ever round-tripping a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedKeys {
    /// Normalised key names actually sent.
    pub names: Vec<String>,
    /// Base64 of the raw PTY bytes.
    pub data_base64: String,
}

/// Encode the `enter|esc|1..9|<text>` answer vocabulary.
///
/// `enter`/`return` → CR, `esc`/`escape` → ESC, a single digit → that byte.
/// Any other token is typed verbatim (printable ASCII) and submitted with a
/// trailing CR, since free text answers must be submitted.
pub fn encode_answer_key(token: &str) -> Result<EncodedKeys, String> {
    let raw = token.trim();
    if raw.is_empty() {
        return Err("empty answer key".into());
    }
    let (name, bytes) = match raw.to_ascii_lowercase().as_str() {
        "enter" | "return" => ("enter".to_string(), vec![b'\r']),
        "esc" | "escape" => ("esc".to_string(), vec![0x1b]),
        "space" => ("space".to_string(), vec![b' ']),
        "tab" => ("tab".to_string(), vec![b'\t']),
        other if other.len() == 1 && other.as_bytes()[0].is_ascii_digit() => {
            (other.to_string(), vec![other.as_bytes()[0]])
        }
        _ => {
            if !raw.is_ascii()
                || raw
                    .chars()
                    .any(|c| !c.is_ascii_graphic() && !matches!(c, ' ' | '\t'))
            {
                return Err(
                    "answer text must be printable ASCII (enter, esc, a digit, or text)".into(),
                );
            }
            let mut bytes = raw.as_bytes().to_vec();
            bytes.push(b'\r');
            (raw.to_string(), bytes)
        }
    };
    Ok(EncodedKeys {
        names: vec![name],
        data_base64: base64_encode(&bytes),
    })
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i < input.len() {
        let remaining = input.len() - i;
        let b0 = input[i];
        let b1 = if remaining > 1 { input[i + 1] } else { 0 };
        let b2 = if remaining > 2 { input[i + 2] } else { 0 };
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if remaining > 1 {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if remaining > 2 {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        i += 3;
    }
    out
}

/// One row of the per-project worker roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRoster {
    /// `meta`; the id is `wkr_…`.
    #[serde(flatten)]
    pub meta: EntityMeta<WorkerRosterId>,
    /// Owning project.
    pub project_id: ProjectId,
    /// Human-facing worker name; unique among *active* rows of the project.
    /// Branch and worktree directory derive from it.
    pub name: String,
    /// Instance (`ins_…`) running the harness; absent if provisioning stopped
    /// before the agent launched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<InstanceId>,
    /// Host the worker was placed on.
    pub host_id: HostId,
    /// Registered workspace the worktree belongs to.
    pub workspace_id: WorkspaceId,
    /// Harness kind (`claude` / `codex` / `grok`).
    pub harness: String,
    /// Carrier driver the agent **actually** launched on, as reported back by
    /// the Node's create result (`shell-pty` / `claude-pty` / …).
    ///
    /// Recorded from the Node's answer rather than from the Hub's request: the
    /// two diverged silently once (a roster row said `claude-pty` while the Node
    /// ran `claude-print`), which made every screen read and nudge on that
    /// worker inexplicable. Absent on rows written before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// Model id the agent launched with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Provider profile used, when admission picked one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_profile_id: Option<String>,
    /// Branch `wt/<name>/<slug>`, product-assigned.
    pub branch: String,
    /// Absolute worktree path reported back by the Node (node-assigned under
    /// its managed worktree root).
    pub worktree_path: String,
    /// Allocated port block (e.g. `58600-58609`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_block: Option<String>,
    /// Per-worker cargo target dir reported by the Node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_dir: Option<String>,
    /// Brief delivered to the worker, as an objects-store attachment (`obj_…`).
    /// The brief always rides the file path, never inline prompt text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief_object_id: Option<String>,
    /// Optional bound task (`tsk_…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// Lifecycle state.
    pub state: WorkerState,
    /// Latest `remuda watch` screen classification (5b); absent until the
    /// worker is first observed. Point-in-time observation, distinct from the
    /// durable lifecycle `state`: idle-api-error/stalled/gone never move the
    /// lifecycle, only a freshly-read DONE/BLOCKED line does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<WorkerWatch>,
    /// When the coordinator last nudged this worker (nudge throttle, §3.2
    /// `policy.configurable.nudgeThrottleMins`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_nudge_at: Option<crate::Timestamp>,
    /// When `worker resume` respawns the agent, the previous instance id is
    /// kept here and `instance_id` is repointed at the replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<InstanceId>,
    /// How many times `worker replace` re-dispatched this worker name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_count: Option<crate::U64>,
    /// The admission/placement decision JSON (reasons[]/rejected[]), stored for
    /// the audit trail and the bot placement card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supply_decision: Option<serde_json::Value>,
    /// Bytes reclaimed from the target dir at retire (reported by the Node).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reclaimed_bytes: Option<crate::U64>,
}

// ── Hub→Node worker.provision / worker.remove ──────────────────────────────

/// `worker.provision` params: create the product-assigned worktree and the
/// per-worker cargo target directory on the Node.
///
/// The Node owns the filesystem layout: the request names the worker and the
/// branch, but never absolute paths (security-review-2 M4, same rule as
/// `worktree.create`). The Node creates the worktree under its managed
/// `<repo>/../remuda-wt/<name>` root and the target dir under
/// `<repo>/../remuda-target/<name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerProvisionParams {
    /// Worker name; one safe path segment, also the worktree directory.
    pub name: String,
    /// Full branch to create (`wt/<name>/<slug>`); must not exist yet.
    pub branch: String,
    /// Registered workspace whose repository root holds the checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// Start point to branch and check out from (default `origin/main`). The
    /// Node fetches it first, so the worktree always starts at remote main.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_point: Option<String>,
}

/// Result of `worker.provision`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerProvisionResult {
    /// Worker name.
    pub name: String,
    /// Branch created.
    pub branch: String,
    /// Start point actually used.
    pub start_point: String,
    /// Absolute worktree path (node-assigned).
    pub worktree_path: String,
    /// Absolute per-worker cargo target dir (node-assigned).
    pub target_dir: String,
}

/// `worker.remove` params: reclaim one worker's filesystem resources.
///
/// Paths are never taken from the wire: the Node recomputes both locations from
/// the managed roots and the `name`, then containment-checks before deleting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRemoveParams {
    /// Worker name whose worktree/target dir are removed.
    pub name: String,
    /// Registered workspace the worktree was created in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// Instance whose herdr carrier (tab/panes/workspace) is closed first.
    /// Absent for print-driver workers that own no tab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<InstanceId>,
}

/// Result of `worker.remove`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRemoveResult {
    /// Worker name.
    pub name: String,
    /// True when a worktree was found and removed.
    pub worktree_removed: bool,
    /// True when a target dir was found and removed.
    pub target_removed: bool,
    /// Bytes reclaimed from the target dir.
    pub reclaimed_bytes: crate::U64,
}

/// Validate a worker name: one safe path segment, same rule as worktree names.
pub fn validate_worker_name(name: &str) -> Result<(), String> {
    path_guard::safe_segment(name).map_err(|error| error.to_string())
}

/// Validate a worker branch of the form `wt/<name>/<slug>` where both trailing
/// segments are safe (the branch is product-assigned, so it is checked here
/// rather than passed to a shell).
pub fn validate_worker_branch(branch: &str) -> Result<(), String> {
    let mut parts = branch.split('/');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("wt"), Some(name), Some(slug), None) if !name.is_empty() && !slug.is_empty() => {
            path_guard::safe_segment(name).map_err(|error| error.to_string())?;
            validate_slug(slug)
        }
        _ => Err(format!(
            "worker branch must be wt/<name>/<slug> with safe segments: {branch}"
        )),
    }
}

/// Validate a branch slug: like a safe path segment but slightly looser —
/// digits and letters may start it, still single-segment with no traversal.
fn validate_slug(slug: &str) -> Result<(), String> {
    let valid = (1..=48).contains(&slug.len())
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(format!("bad branch slug {slug}"))
    }
}

/// Turn arbitrary text (task title, brief file stem) into a branch-safe slug.
#[must_use]
pub fn slugify(input: &str) -> String {
    let slug: String = input
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let mut slug = slug.trim_matches('-').to_string();
    // Collapse runs of dashes.
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    if slug.is_empty() {
        "work".to_string()
    } else {
        slug.truncate(48);
        slug.trim_end_matches('-').to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn state_wire_shape_is_adjacently_tagged() {
        assert_eq!(
            serde_json::to_value(WorkerState::Done {
                sha: "abc123".into()
            })
            .unwrap(),
            serde_json::json!({ "state": "done", "sha": "abc123" })
        );
        assert_eq!(
            serde_json::to_value(WorkerState::Blocked {
                reason: "need creds".into()
            })
            .unwrap(),
            serde_json::json!({ "state": "blocked", "reason": "need creds" })
        );
        assert_eq!(
            serde_json::to_value(WorkerState::Working).unwrap(),
            serde_json::json!({ "state": "working" })
        );
        let parsed: WorkerState = serde_json::from_value(json!({ "state": "retired" })).unwrap();
        assert_eq!(parsed, WorkerState::Retired);
        assert!(!WorkerState::Done { sha: "x".into() }.is_working());
        assert!(WorkerState::Working.is_working());
        assert!(WorkerState::Working.is_active());
        assert!(!WorkerState::Retired.is_active());
    }

    #[test]
    fn state_update_validation() {
        assert!(WorkerState::from_update("working", None, None).is_ok());
        assert!(WorkerState::from_update("done", Some("abc"), None).is_ok());
        assert!(WorkerState::from_update("done", None, None).is_err());
        assert!(WorkerState::from_update("blocked", None, Some("reason")).is_ok());
        assert!(WorkerState::from_update("blocked", None, None).is_err());
        assert!(WorkerState::from_update("dispatched", None, None).is_err());
    }

    #[test]
    fn names_and_branches_validate() {
        assert!(validate_worker_name("c-task1").is_ok());
        assert!(validate_worker_name("../x").is_err());
        assert!(validate_worker_branch("wt/c-task1/fix-bug").is_ok());
        assert!(validate_worker_branch("main").is_err());
        assert!(validate_worker_branch("wt/c/../x").is_err());
        assert_eq!(slugify("Fix the thing!"), "fix-the-thing");
        assert_eq!(slugify("..."), "work");
        assert_eq!(slugify("TRAIL---dash"), "trail-dash");
    }

    fn sig<'a>(lines: &'a [String], lifecycle: &'a str, activity: &'a str) -> ScreenSignals<'a> {
        ScreenSignals {
            lines,
            lifecycle,
            activity,
            host_online: true,
            now_unix: 10_000,
            last_activity_unix: None,
            stall_mins: DEFAULT_STALL_THRESHOLD_MINS,
            known_done_sha: None,
            known_blocked: None,
            raw: false,
            screen_available: true,
            dialog_title: None,
            journal_lines: &[],
            turn_error: false,
            tail_error: false,
            last_error_line: None,
            lifecycle_reason: None,
        }
    }

    fn rows(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    #[test]
    fn classifies_a_new_done_line() {
        let lines = rows("working on it…\nDONE 0123456789abcdef\n");
        assert_eq!(
            classify_screen(&sig(&lines, "ready", "idle")),
            ScreenClass::Done {
                sha: "0123456789abcdef".into(),
                line: "DONE 0123456789abcdef".into(),
            }
        );
    }

    #[test]
    fn a_done_equal_to_the_known_tip_is_an_echo() {
        let lines = rows("DONE 0123456789abcdef\n");
        let mut s = sig(&lines, "ready", "idle");
        s.known_done_sha = Some("0123456789abcdef");
        // Echoed old tip + idle, no API trouble → just working, not done again.
        assert_eq!(classify_screen(&s), ScreenClass::Working);
    }

    #[test]
    fn brief_contract_placeholder_is_not_a_done() {
        let lines = rows("When finished reply on one line: DONE <sha> or BLOCKED <reason>.\n");
        assert_eq!(
            classify_screen(&sig(&lines, "ready", "idle")),
            ScreenClass::Working
        );
    }

    #[test]
    fn blocked_reason_is_parsed_and_deduped() {
        let lines = rows("• BLOCKED need credentials for the registry\n");
        match classify_screen(&sig(&lines, "ready", "blocked")) {
            ScreenClass::Blocked { reason, .. } => {
                assert_eq!(reason, "need credentials for the registry")
            }
            other => panic!("{other:?}"),
        }
        let mut s = sig(&lines, "ready", "blocked");
        s.known_blocked = Some("need credentials for the registry");
        assert_eq!(classify_screen(&s), ScreenClass::Working);
    }

    #[test]
    fn idle_after_api_error_only_when_idle() {
        let lines = rows("API Error: bad gateway, Retrying…\n");
        assert_eq!(
            classify_screen(&sig(&lines, "ready", "idle")),
            ScreenClass::IdleApiError {
                fragment: "api error".into(),
            }
        );
        // A busy turn momentarily showing a retry is not "idle after API error".
        assert_eq!(
            classify_screen(&sig(&lines, "running", "busy")),
            ScreenClass::Working
        );
    }

    #[test]
    fn connection_closed_counts_as_api_trouble() {
        let lines = rows("Connection closed.\n");
        assert!(matches!(
            classify_screen(&sig(&lines, "ready", "idle")),
            ScreenClass::IdleApiError { .. }
        ));
    }

    #[test]
    fn stalled_needs_busy_and_a_quiet_window() {
        let lines = rows("thinking…\n");
        let mut s = sig(&lines, "running", "busy");
        s.last_activity_unix = Some(10_000 - 31 * 60);
        assert!(matches!(
            classify_screen(&s),
            ScreenClass::Stalled { quiet_mins } if quiet_mins >= 31
        ));
        // Recent activity is not a stall.
        s.last_activity_unix = Some(10_000 - 5 * 60);
        assert_eq!(classify_screen(&s), ScreenClass::Working);
    }

    #[test]
    fn gone_when_offline_or_closed() {
        let lines = rows("DONE 0123456789abcdef\n");
        let mut s = sig(&lines, "ready", "idle");
        s.host_online = false;
        assert_eq!(
            classify_screen(&s),
            ScreenClass::Gone {
                reason: "host-offline".to_string()
            }
        );
        let closed = sig(&lines, "closed", "idle");
        assert_eq!(
            classify_screen(&closed),
            ScreenClass::Gone {
                reason: "instance-closed".to_string()
            }
        );
    }

    /// A row the Hub already settled with a reason classifies gone *with that
    /// reason*: `node-epoch-changed` is why the worker died, and reporting a
    /// bare `instance-closed` would read as if its owner had stopped it.
    #[test]
    fn gone_survives_the_settled_rows_own_reason() {
        let mut s = sig(&[], "exited", "idle");
        s.screen_available = false;
        assert_eq!(
            classify_screen(&s),
            ScreenClass::Gone {
                reason: "instance-closed".to_string()
            },
            "a clean exit with no recorded reason keeps the carrier code"
        );
        s.lifecycle_reason = Some("node-epoch-changed");
        assert_eq!(
            classify_screen(&s),
            ScreenClass::Gone {
                reason: "node-epoch-changed".to_string()
            }
        );
    }

    #[test]
    fn failed_lifecycle_uses_last_assistant_error_line() {
        let mut s = sig(&[], "failed", "idle");
        s.last_error_line = Some("API Error: 400 requested model is not available\nretried 3x");
        assert_eq!(
            classify_screen(&s),
            ScreenClass::Failed {
                reason: "API Error: 400 requested model is not available".into(),
                line: "API Error: 400 requested model is not available".into(),
            }
        );
        // No error message: the lifecycle reason code is the reason.
        s.last_error_line = None;
        s.lifecycle_reason = Some("native-driver-start-failed");
        match classify_screen(&s) {
            ScreenClass::Failed { reason, .. } => assert_eq!(reason, "native-driver-start-failed"),
            other => panic!("{other:?}"),
        }
        // Nothing at all: the static fallback.
        s.lifecycle_reason = None;
        assert!(matches!(
            classify_screen(&s),
            ScreenClass::Failed { ref reason, .. } if reason == "instance-failed"
        ));
    }

    #[test]
    fn turn_error_fails_even_when_the_carrier_says_ready() {
        // The exact 2026-09-17 trap: stale "ready" screen lifecycle, errored
        // turn in the journal.
        let mut s = sig(&[], "ready", "idle");
        s.turn_error = true;
        s.last_error_line = Some("API Error: 400 requested model is not available");
        assert!(matches!(
            classify_screen(&s),
            ScreenClass::Failed { ref reason, .. }
                if reason == "API Error: 400 requested model is not available"
        ));
        // A failed turn wins even while the host is briefly unreachable.
        s.host_online = false;
        assert!(matches!(classify_screen(&s), ScreenClass::Failed { .. }));
    }

    #[test]
    fn exited_after_error_fails_but_clean_exit_is_gone() {
        let mut s = sig(&[], "exited", "idle");
        s.turn_error = true;
        assert!(matches!(classify_screen(&s), ScreenClass::Failed { .. }));
        s.turn_error = false;
        // The LAST assistant message was the error: fatal.
        s.tail_error = true;
        s.last_error_line = Some("API Error: 429 rate limited");
        assert!(matches!(classify_screen(&s), ScreenClass::Failed { .. }));
        // The error happened earlier, the session recovered and its final
        // message is a DONE report: an old error must not make that a failure.
        s.tail_error = false;
        let journal = rows("API Error: 429 rate limited\nDONE 0123456789abcdef");
        s.journal_lines = &journal;
        s.screen_available = false;
        assert!(matches!(
            classify_screen(&s),
            ScreenClass::Done { ref sha, .. } if sha == "0123456789abcdef"
        ));
        // A print worker exits 0 with no error tail: gone, not failed.
        s.journal_lines = &[];
        s.last_error_line = None;
        assert_eq!(
            classify_screen(&s),
            ScreenClass::Gone {
                reason: "instance-closed".to_string()
            }
        );
    }

    #[test]
    fn screenless_journal_classifies_done_blocked_and_echo() {
        let journal = rows("working on it\nDONE 0123456789abcdef");
        let mut s = sig(&[], "ready", "idle");
        s.screen_available = false;
        s.journal_lines = &journal;
        assert!(matches!(
            classify_screen(&s),
            ScreenClass::Done { ref sha, .. } if sha == "0123456789abcdef"
        ));
        // Same sha already known to the roster: echo, not a new DONE.
        s.known_done_sha = Some("0123456789abcdef");
        assert_eq!(classify_screen(&s), ScreenClass::Working);
        // BLOCKED journal text works too.
        s.known_done_sha = None;
        let blocked = rows("BLOCKED need credentials");
        s.journal_lines = &blocked;
        assert!(matches!(
            classify_screen(&s),
            ScreenClass::Blocked { ref reason, .. } if reason == "need credentials"
        ));
    }

    #[test]
    fn screenless_journal_idle_after_api_error() {
        let journal = rows("API Error: bad gateway, Retrying…");
        let mut s = sig(&[], "ready", "idle");
        s.screen_available = false;
        s.journal_lines = &journal;
        assert_eq!(
            classify_screen(&s),
            ScreenClass::IdleApiError {
                fragment: "api error".into(),
            }
        );
        // A busy turn is never "idle after error", screen or journal.
        s.activity = "busy";
        s.lifecycle = "running";
        assert_eq!(classify_screen(&s), ScreenClass::Working);
    }

    #[test]
    fn screenless_worker_mid_turn_with_no_news_is_working() {
        let mut s = sig(&[], "ready", "idle");
        s.screen_available = false;
        assert_eq!(classify_screen(&s), ScreenClass::Working);
    }

    #[test]
    fn raw_ring_tail_finds_done_without_line_boundary() {
        // A raw VT ring tail: one string of width-padded repaint rows.
        let row = |content: &str| format!("{content:<80}");
        let tail = vec![format!(
            "{}{}{}",
            row("Your coordinator brief is delivered as the attached file brief.md."),
            row("DONE 9f3807e59491"),
            row("/help for shortcuts")
        )];
        let mut s = sig(&tail, "ready", "idle");
        s.raw = true;
        assert!(matches!(
            classify_screen(&s),
            ScreenClass::Done { ref sha, .. } if sha == "9f3807e59491"
        ));
        // Emulated grids keep requiring the start-of-line anchor.
        s.raw = false;
        assert_eq!(classify_screen(&s), ScreenClass::Working);
        // The contract placeholder is never a DONE, even in a raw tail.
        let placeholder = vec![row("Reply on one line: DONE <sha> or BLOCKED <reason>.")];
        let mut p = sig(&placeholder, "ready", "idle");
        p.raw = true;
        assert_eq!(classify_screen(&p), ScreenClass::Working);
    }

    #[test]
    fn a_screen_dialog_blocks_with_its_title_even_while_the_carrier_says_busy() {
        // dispatch-onboarding-1: the incident shape — a parked first-run
        // dialog with lifecycle ready and activity the classifier would
        // otherwise treat as busy must report blocked, never working.
        for (lifecycle, activity) in [
            ("ready", "idle"),
            ("ready", "working"),
            ("running", "busy"),
            ("starting", ""),
        ] {
            let lines = rows(
                "Welcome to Claude Code!\nQuick safety check:\n\
                 Is this a project you created or one you trust?\n\
                 ❯ No, exit\n  Yes, I trust this folder",
            );
            let mut s = sig(&lines, lifecycle, activity);
            s.dialog_title = Some("Is this a project you created or one you trust?");
            match classify_screen(&s) {
                ScreenClass::Blocked { reason, line } => {
                    assert_eq!(reason, "Is this a project you created or one you trust?");
                    assert_eq!(line, reason);
                }
                other => panic!("{lifecycle}/{activity}: {other:?}"),
            }
        }
        let lines = rows(
            "Allow reads outside the working directories?\n\
             ❯ Yes, keep allowing reads outside the working directories\n\
               No, block reads outside the working directories from now on\n\
               No, ask again next time",
        );
        let mut s = sig(&lines, "running", "busy");
        s.dialog_title = Some("Allow reads outside the working directories?");
        assert!(matches!(
            classify_screen(&s),
            ScreenClass::Blocked { ref reason, .. }
                if reason == "Allow reads outside the working directories?"
        ));
    }

    #[test]
    fn a_screen_dialog_stays_blocked_across_echo_baselines() {
        // A modal still on screen after the first blocked observation must
        // not collapse to working like a deduped BLOCKED report does.
        let lines = rows("Quick safety check:\nIs this a project you created or one you trust?");
        let mut s = sig(&lines, "ready", "idle");
        s.dialog_title = Some("Is this a project you created or one you trust?");
        s.known_blocked = Some("Is this a project you created or one you trust?");
        assert!(matches!(classify_screen(&s), ScreenClass::Blocked { .. }));
    }

    #[test]
    fn a_dialog_title_is_ignored_without_a_live_screen() {
        // Screenless (print) workers classify from the journal only.
        let mut s = sig(&[], "ready", "idle");
        s.screen_available = false;
        s.dialog_title = Some("Is this a project you created or one you trust?");
        assert_eq!(classify_screen(&s), ScreenClass::Working);
    }

    #[test]
    fn answer_keys_encode() {
        assert_eq!(encode_answer_key("enter").unwrap().data_base64, "DQ==");
        assert_eq!(encode_answer_key("ESC").unwrap().data_base64, "Gw==");
        let digit = encode_answer_key("3").unwrap();
        assert_eq!(digit.names, ["3"]);
        assert_eq!(digit.data_base64, "Mw==");
        let text = encode_answer_key("yes").unwrap();
        assert_eq!(text.names, ["yes"]);
        // Free text is submitted with a trailing CR.
        assert_eq!(text.data_base64, "eWVzDQ==");
        assert!(encode_answer_key("").is_err());
    }
}
