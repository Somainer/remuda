//! Claude permission-mode vocabulary, read-back tracker, and stable event
//! ids (the `permission` counterpart of [`crate::effort`]).
//!
//! Claude 2.1.273 writes the effective permission mode to its transcript as a
//! top-level `{"type":"permission-mode","permissionMode":"plan"}` record
//! whenever the mode changes (a shift+tab cycle, a `/plan` command, or a
//! launch flag). The TUI status line paints the same mode as
//! `manual mode on` / `accept edits on` / `plan mode on` / `auto mode on` /
//! `bypass permissions on` / `don't ask on`. Measurements:
//! `docs/design/evidence/permission-modes-1.md`.

use crate::{ClaudePermissionMode, EventId, Id, PermissionSource};

/// Parse a mode word off the wire, a launch flag, a transcript record, or the
/// TUI status vocabulary.
///
/// `default` is the TUI/transcript spelling of the protocol's `manual`; both
/// map to [`ClaudePermissionMode::Manual`].
pub fn normalize_permission_word(word: &str) -> Option<ClaudePermissionMode> {
    match word.trim() {
        "manual" | "default" => Some(ClaudePermissionMode::Manual),
        "auto" => Some(ClaudePermissionMode::Auto),
        "acceptEdits" | "acceptedEdits" => Some(ClaudePermissionMode::AcceptEdits),
        "dontAsk" | "dontask" => Some(ClaudePermissionMode::DontAsk),
        "plan" => Some(ClaudePermissionMode::Plan),
        "bypassPermissions" | "bypass" => Some(ClaudePermissionMode::BypassPermissions),
        _ => None,
    }
}

/// Protocol wire spelling of a Claude permission mode.
pub fn permission_mode_wire(mode: ClaudePermissionMode) -> &'static str {
    match mode {
        ClaudePermissionMode::Manual => "manual",
        ClaudePermissionMode::Auto => "auto",
        ClaudePermissionMode::AcceptEdits => "acceptEdits",
        ClaudePermissionMode::DontAsk => "dontAsk",
        ClaudePermissionMode::Plan => "plan",
        ClaudePermissionMode::BypassPermissions => "bypassPermissions",
    }
}

/// Edge-dedup + source attribution for permission-mode read-back.
///
/// Shared by the live `TranscriptMapper` (driver) and the durable journal file
/// tailer, exactly like [`crate::EffortTracker`]. The driver additionally
/// correlates a pending Remuda switch; the tailer always observes with
/// `remuda_pending = None`.
#[derive(Debug, Default, Clone)]
pub struct LivePermissionTracker {
    /// Last mode emitted as an observation.
    last: Option<ClaudePermissionMode>,
    /// Launch-time mode; the first matching edge is attributed `launch`.
    launch: Option<ClaudePermissionMode>,
    launch_consumed: bool,
    /// A `/plan` (or future native mode command) record was just seen.
    slash_armed: bool,
}

impl LivePermissionTracker {
    /// Empty tracker with no launch mode or prior edge.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the launch-time mode. The first observed record matching it
    /// still emits an edge attributed `launch` (the edge proves the flag took
    /// effect); only repeated records after that dedupe.
    pub fn mark_launch(&mut self, mode: ClaudePermissionMode) {
        self.launch = Some(mode);
    }

    /// A `/plan` user record (or a future native mode command) was just seen;
    /// the next permission-mode edge is attributed `slash`.
    pub fn note_slash(&mut self) {
        self.slash_armed = true;
    }

    /// Fold one raw transcript word (the `permissionMode` / `mode` field).
    ///
    /// Returns the mode + source only on an edge; repeated records and
    /// unparseable words yield nothing. `remuda_pending` names the mode a
    /// Remuda push-down is currently waiting for.
    pub fn note(
        &mut self,
        raw: &str,
        remuda_pending: Option<ClaudePermissionMode>,
    ) -> Option<(ClaudePermissionMode, PermissionSource)> {
        let mode = normalize_permission_word(raw)?;
        let source = if remuda_pending == Some(mode) {
            PermissionSource::Remuda
        } else if self.slash_armed {
            PermissionSource::Slash
        } else if !self.launch_consumed && self.launch == Some(mode) {
            self.launch_consumed = true;
            PermissionSource::Launch
        } else {
            PermissionSource::Unknown
        };
        self.slash_armed = false;
        if self.last == Some(mode) {
            return None;
        }
        self.last = Some(mode);
        Some((mode, source))
    }
}

/// Deterministic observation id for a permission-mode edge, stable across the
/// live channel and the journal file tailer (same shape as
/// [`crate::effort_event_id`]).
pub fn permission_event_id(scope: &str, native_key: &str, mode: ClaudePermissionMode) -> EventId {
    let native = format!("permission:{}:{native_key}", permission_mode_wire(mode));
    let id: Id = Id::derive("evt", scope, &native).expect("evt prefix registered");
    EventId::try_from(String::from(id)).expect("derive with the evt prefix yields an EventId")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_normalize_with_tui_aliases() {
        assert_eq!(
            normalize_permission_word("default"),
            Some(ClaudePermissionMode::Manual)
        );
        assert_eq!(
            normalize_permission_word("bypass"),
            Some(ClaudePermissionMode::BypassPermissions)
        );
        assert_eq!(
            normalize_permission_word("plan"),
            Some(ClaudePermissionMode::Plan)
        );
        assert_eq!(normalize_permission_word("nope"), None);
    }

    #[test]
    fn tracker_dedupes_and_attributes_sources() {
        use ClaudePermissionMode::*;
        let mut tracker = LivePermissionTracker::new();
        tracker.mark_launch(Manual);
        // First matching edge is launch…
        assert_eq!(
            tracker.note("manual", None),
            Some((Manual, PermissionSource::Launch))
        );
        // …a `/plan` command makes the next edge slash-attributed.
        tracker.note_slash();
        assert_eq!(
            tracker.note("plan", None),
            Some((Plan, PermissionSource::Slash))
        );
        // A remuda-attributed edge.
        assert_eq!(
            tracker.note("auto", Some(Auto)),
            Some((Auto, PermissionSource::Remuda))
        );
        // Duplicate: no edge.
        assert_eq!(tracker.note("auto", None), None);
        // Bare native cycle is unknown provenance.
        assert_eq!(
            tracker.note("acceptEdits", None),
            Some((AcceptEdits, PermissionSource::Unknown))
        );
        // A stale remuda target does not steal attribution.
        assert_eq!(
            tracker.note("plan", Some(Auto)),
            Some((Plan, PermissionSource::Unknown))
        );
        // A later return to the launch mode after touring others is unknown.
        assert_eq!(
            tracker.note("default", None),
            Some((Manual, PermissionSource::Unknown))
        );
    }

    #[test]
    fn event_ids_are_stable_and_namespaced() {
        let a = permission_event_id("ins_one", "rec_1", ClaudePermissionMode::Plan);
        assert_eq!(
            a,
            permission_event_id("ins_one", "rec_1", ClaudePermissionMode::Plan)
        );
        assert_ne!(
            a,
            permission_event_id("ins_one", "rec_2", ClaudePermissionMode::Plan)
        );
        assert_ne!(
            a,
            permission_event_id("ins_one", "rec_1", ClaudePermissionMode::Auto)
        );
        assert_ne!(
            a,
            permission_event_id("ins_two", "rec_1", ClaudePermissionMode::Plan)
        );
    }
}
