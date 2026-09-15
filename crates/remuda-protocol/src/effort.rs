//! §9.1 effective-effort tracking shared by every Claude transcript mapper.
//!
//! The driver's live mapper (`remuda-driver::TranscriptMapper`) and the
//! journal tailer (`remuda-journal`) must agree on what an assistant record
//! says about effort and how a `/effort` slash record attributes the change,
//! so the state machine lives here rather than being copied.
//!
//! Claude transcript shape measured on 2.1.221 and re-measured on 2.1.272
//! (`docs/design/evidence/effort-sync-1.md`, `effort-sync-2.md`):
//!
//! - assistant records carry a top-level `effort: "low|medium|high|xhigh|max"`
//!   and a top-level `perTurnEffort: string | null`;
//! - `/effort xhigh` is journaled as a `user` record whose message content is
//!   `<command-name>/effort</command-name> … <command-args>xhigh</command-args>`;
//! - a SECOND user record carries the verdict in
//!   `<local-command-stdout>…</local-command-stdout>` — that line, not the
//!   slash record and not the next assistant record, is the in-session
//!   acceptance channel (the slash record lands even when the confirmation
//!   dialog is dismissed or the argument is invalid);
//! - `ultracode` reads back as level `xhigh`; the stdout verdict says
//!   "Set effort level to ultracode (this session only) …" and `attachment`
//!   records `ultra_effort_enter|exit` ride the next prompt.

use crate::{EffortName, EffortSource, EventId, Id};

/// Deterministic observation id for an effort edge read from one native
/// assistant record.
///
/// Both channels that can observe the edge — the driver's live transcript
/// hydrator and the journal file tailer — process the *same* native record, so
/// deriving the event id from `(instance scope, assistant record id, level)`
/// via [`Id::derive`] makes the observation stable across channels: re-tailing
/// the file after the live event was already journaled does not mint a second
/// identity for the same edge.
pub fn effort_event_id(scope: &str, assistant_native_id: &str, name: EffortName) -> EventId {
    let native = format!("effort:{}:{assistant_native_id}", name_wire(name));
    // Id::derive is infallible for the registered `evt` prefix and valid
    // `(scope, native)` strings; the branded constructor enforces the prefix.
    let id: Id = Id::derive("evt", scope, &native).expect("evt prefix registered");
    EventId::try_from(String::from(id)).expect("derive with the evt prefix yields an EventId")
}

fn name_wire(name: EffortName) -> &'static str {
    match name {
        EffortName::Low => "low",
        EffortName::Medium => "medium",
        EffortName::High => "high",
        EffortName::Xhigh => "xhigh",
        EffortName::Max => "max",
        // Claude assistant records never report the Codex/Grok-only `minimal`
        // word; it only exists on launch argv for those harnesses.
        EffortName::Minimal => "minimal",
    }
}

/// An effort level as read off an assistant transcript record.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ObservedEffort {
    /// Observed level.
    pub name: EffortName,
    /// Observed dynamic-workflow flag; positively true after
    /// `Set effort level to ultracode`, positively false after a plain level
    /// accept, `None` until a verdict says either way.
    pub ultracode: Option<bool>,
}

/// What the `<local-command-stdout>` sibling of a `/effort` slash record says.
///
/// The slash record itself only proves the bytes were submitted; on 2.1.272
/// it is written even for a dismissed dialog or an invalid argument. The
/// stdout line is the verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortStdout {
    /// `Set effort level to <level>` — the switch is in effect now.
    Accepted(ObservedEffort),
    /// `Kept effort level as <level>` — the confirmation dialog was dismissed.
    Kept,
    /// `Invalid argument: <word>. Valid options are: …`.
    Invalid,
    /// `Effort level set to auto` or unrelated stdout.
    Other,
}

/// Parse the verdict out of a `/effort` `<local-command-stdout>` line.
///
/// Measured strings (2.1.221/2.1.272):
/// - `Set effort level to xhigh (saved as your default for new sessions): …`
/// - `Set effort level to ultracode (this session only): xhigh + dynamic …`
/// - `Set effort level: high (saved as your default for new sessions)` (older
///   spelling, accepted defensively)
/// - `Kept effort level as xhigh`
/// - `Invalid argument: bogus. Valid options are: low, medium, …`
/// - `Effort level set to auto`
pub fn parse_effort_stdout(text: &str) -> EffortStdout {
    let t = text.trim();
    if t.contains("Set effort level to ultracode") {
        return EffortStdout::Accepted(ObservedEffort {
            name: EffortName::Xhigh,
            ultracode: Some(true),
        });
    }
    if let Some(rest) = t.strip_prefix("Set effort level to ")
        && let Some(name) = first_word(rest).and_then(EffortTracker::parse_level)
    {
        return EffortStdout::Accepted(ObservedEffort {
            name,
            // A plain level accept positively turns ultracode off.
            ultracode: Some(false),
        });
    }
    // Older spelling: `Set effort level: high (…)`.
    if let Some(rest) = t.strip_prefix("Set effort level: ")
        && let Some(name) = first_word(rest).and_then(EffortTracker::parse_level)
    {
        return EffortStdout::Accepted(ObservedEffort {
            name,
            ultracode: Some(false),
        });
    }
    if t.starts_with("Kept effort level") {
        return EffortStdout::Kept;
    }
    if t.starts_with("Invalid argument") && t.contains("Valid options are") {
        return EffortStdout::Invalid;
    }
    EffortStdout::Other
}

fn first_word(rest: &str) -> Option<&str> {
    let w = rest
        .split(|c: char| c.is_whitespace() || c == ':' || c == '(')
        .next()
        .unwrap_or("");
    (!w.is_empty()).then_some(w)
}

/// Transcript-side effective-effort state: dedup and source attribution.
///
/// Edges only: [`EffortTracker::observe`] returns `None` for an unchanged
/// level, so a long turn with many assistant records emits once.
///
/// Pure state shared by both transcript mappers; not a wire type.
#[derive(Debug, Clone)]
pub struct EffortTracker {
    last: Option<ObservedEffort>,
    pending_source: EffortSource,
    /// Ultracode flag armed by a slash record, pending a stdout verdict or
    /// assistant record (older builds with no stdout line).
    ultracode_pending: bool,
    /// Ultracode positively confirmed ON; kept until a stdout accept/
    /// attachment positively turns it off. Assistant records carry no flag,
    /// so without this latch a later xhigh record would silently drop it.
    ultracode_on: bool,
    /// A Remuda switch awaiting its read-back at this level.
    awaiting: Option<EffortName>,
}

impl Default for EffortTracker {
    fn default() -> Self {
        Self {
            last: None,
            pending_source: EffortSource::Unknown,
            ultracode_pending: false,
            ultracode_on: false,
            awaiting: None,
        }
    }
}

impl EffortTracker {
    /// Parse the five Claude levels plus the `ultracode` spelling into an
    /// [`EffortName`]. `auto` and unknown words yield `None` — they are not
    /// levels.
    pub fn parse_level(word: &str) -> Option<EffortName> {
        match word.trim().to_ascii_lowercase().as_str() {
            "low" => Some(EffortName::Low),
            "medium" => Some(EffortName::Medium),
            "high" => Some(EffortName::High),
            "xhigh" | "ultracode" => Some(EffortName::Xhigh),
            "max" => Some(EffortName::Max),
            _ => None,
        }
    }

    /// Construct an empty tracker (no level observed yet).
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a `/effort <word>` slash record. `from_remuda` says whether
    /// Remuda typed the bytes (otherwise the human gets the credit). Returns
    /// false for words that are not level switches (`auto`, invalid).
    pub fn note_slash(&mut self, word: &str, from_remuda: bool) -> bool {
        let Some(name) = Self::parse_level(word) else {
            return false;
        };
        self.ultracode_pending = word.trim().eq_ignore_ascii_case("ultracode");
        self.pending_source = if from_remuda {
            EffortSource::Remuda
        } else {
            EffortSource::Slash
        };
        // A slash record also satisfies an awaiting switch when it was ours.
        if from_remuda {
            self.awaiting = Some(name);
        }
        true
    }

    /// Settle a switch from its `<local-command-stdout>` verdict. Returns an
    /// edge observation for an accept that changes the effective level; a
    /// dismiss/invalid clears the awaiting attribution and returns `None`.
    ///
    /// This is the only channel that proves acceptance immediately (the slash
    /// record is written before the dialog resolves). The caller resolves its
    /// switch bridge independently of the edge: a switch to the level already
    /// in effect is still an accepted switch.
    pub fn note_stdout(
        &mut self,
        stdout: &str,
        from_remuda: bool,
    ) -> Option<(ObservedEffort, EffortSource)> {
        match parse_effort_stdout(stdout) {
            EffortStdout::Accepted(observed) => {
                self.ultracode_on = observed.ultracode == Some(true);
                let mut source = if from_remuda {
                    EffortSource::Remuda
                } else {
                    EffortSource::Slash
                };
                if self.awaiting == Some(observed.name) {
                    source = EffortSource::Remuda;
                    self.awaiting = None;
                }
                self.ultracode_pending = false;
                let edge = self.last != Some(observed);
                self.last = Some(observed);
                if edge {
                    self.pending_source = EffortSource::Unknown;
                    Some((observed, source))
                } else {
                    None
                }
            }
            // No level change; a later natural edge must not be credited to a
            // Remuda switch that Claude actually refused.
            EffortStdout::Kept | EffortStdout::Invalid => {
                self.awaiting = None;
                self.ultracode_pending = false;
                None
            }
            EffortStdout::Other => None,
        }
    }

    /// Feed the `ultra_effort_enter|exit` attachment 2.1.272 rides on the next
    /// prompt. Returns an edge observation when the effective flag changes.
    pub fn note_ultra_attachment(
        &mut self,
        enters: bool,
    ) -> Option<(ObservedEffort, EffortSource)> {
        self.ultracode_on = enters;
        let last = self.last?;
        let observed = ObservedEffort {
            name: last.name,
            ultracode: Some(enters),
        };
        let edge = self.last != Some(observed);
        self.last = Some(observed);
        edge.then_some((observed, EffortSource::Slash))
    }

    /// Declare the launch-level source before the first assistant record.
    pub fn mark_launch(&mut self) {
        self.pending_source = EffortSource::Launch;
    }

    /// Arm a Remuda switch awaiting read-back at `name`.
    pub fn arm_awaiting(&mut self, name: EffortName) {
        self.awaiting = Some(name);
    }

    /// Feed one assistant record's raw `effort` / `perTurnEffort` strings;
    /// `perTurnEffort` is the fallback. Returns an edge observation only.
    ///
    /// Assistant records corroborate the level but never carry the ultracode
    /// flag, so the flag latched by [`Self::note_stdout`] is preserved while
    /// the level stays xhigh.
    pub fn observe(
        &mut self,
        effort: Option<&str>,
        per_turn: Option<&str>,
    ) -> Option<(ObservedEffort, EffortSource)> {
        let raw = effort
            .filter(|value| !value.is_empty())
            .or_else(|| per_turn.filter(|value| !value.is_empty()))?;
        let name = Self::parse_level(raw)?;
        // Assistant records never carry the flag. When they corroborate the
        // level a verdict already established, inherit that known flag (so an
        // xhigh record after `Set … ultracode:false` does not look like an
        // edge). Otherwise only the latched-ON state can produce Some(true).
        let same_level_last = self
            .last
            .filter(|last| last.name == name)
            .and_then(|last| last.ultracode);
        let ultracode =
            if self.ultracode_pending || (self.ultracode_on && name == EffortName::Xhigh) {
                Some(true)
            } else {
                same_level_last
            };
        let observed = ObservedEffort { name, ultracode };

        let mut source = self.pending_source;
        if self.awaiting == Some(name) {
            source = EffortSource::Remuda;
            self.awaiting = None;
        }

        let edge = self.last != Some(observed);
        self.last = Some(observed);
        if edge {
            self.pending_source = EffortSource::Unknown;
            self.ultracode_pending = false;
            Some((observed, source))
        } else {
            None
        }
    }
}

/// Extract the argument word of a `/effort` slash-command transcript record.
///
/// Such records carry message content shaped:
/// `<command-name>/effort</command-name> … <command-args>xhigh</command-args>`.
pub fn slash_effort_word(content: &str) -> Option<String> {
    if !content.contains("<command-name>/effort</command-name>") {
        return None;
    }
    let start = content.find("<command-args>")? + "<command-args>".len();
    let end = content[start..].find("</command-args>")?;
    let word = content[start..start + end].trim();
    if word.is_empty() {
        None
    } else {
        Some(word.to_ascii_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_record_is_an_edge_then_unchanged_records_are_deduped() {
        let mut tracker = EffortTracker::new();
        tracker.mark_launch();
        let first = tracker.observe(Some("high"), None).expect("edge");
        assert_eq!(first.0.name, EffortName::High);
        assert_eq!(first.1, EffortSource::Launch);
        assert!(tracker.observe(Some("high"), None).is_none());
    }

    #[test]
    fn a_user_slash_marks_the_next_new_level() {
        let mut tracker = EffortTracker::new();
        tracker.observe(Some("high"), None);
        assert!(tracker.note_slash("xhigh", false));
        let edge = tracker.observe(Some("xhigh"), None).unwrap();
        assert_eq!(edge.1, EffortSource::Slash);
        assert_eq!(edge.0.ultracode, None);
    }

    #[test]
    fn ultracode_reads_back_as_xhigh_with_the_flag() {
        let mut tracker = EffortTracker::new();
        tracker.observe(Some("high"), None);
        tracker.note_slash("ultracode", false);
        let edge = tracker.observe(Some("xhigh"), None).unwrap();
        assert_eq!(edge.0.name, EffortName::Xhigh);
        assert_eq!(edge.0.ultracode, Some(true));
        assert_eq!(edge.1, EffortSource::Slash);
    }

    #[test]
    fn remuda_awaiting_wins_attribution() {
        let mut tracker = EffortTracker::new();
        tracker.observe(Some("high"), None);
        tracker.arm_awaiting(EffortName::Max);
        assert_eq!(
            tracker.observe(Some("max"), None).unwrap().1,
            EffortSource::Remuda
        );
    }

    #[test]
    fn auto_and_garbage_are_not_levels() {
        assert!(EffortTracker::parse_level("auto").is_none());
        assert!(EffortTracker::parse_level("bogus").is_none());
        assert_eq!(EffortTracker::parse_level("XHigh"), Some(EffortName::Xhigh));
    }

    #[test]
    fn slash_word_shape() {
        let content = "<command-name>/effort</command-name>\n<command-args>max</command-args>";
        assert_eq!(slash_effort_word(content).as_deref(), Some("max"));
        assert!(slash_effort_word("<command-name>/clear</command-name>").is_none());
    }

    #[test]
    fn stdout_verdict_parses_every_measured_shape() {
        // Measured verbatim on 2.1.272.
        match parse_effort_stdout(
            "Set effort level to xhigh (saved as your default for new sessions): Deeper …",
        ) {
            EffortStdout::Accepted(o) => {
                assert_eq!(o.name, EffortName::Xhigh);
                assert_eq!(o.ultracode, Some(false));
            }
            other => panic!("{other:?}"),
        }
        match parse_effort_stdout(
            "Set effort level to ultracode (this session only): xhigh + dynamic workflow \
             orchestration",
        ) {
            EffortStdout::Accepted(o) => {
                assert_eq!(o.name, EffortName::Xhigh);
                assert_eq!(o.ultracode, Some(true));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            parse_effort_stdout("Kept effort level as xhigh"),
            EffortStdout::Kept
        );
        assert_eq!(
            parse_effort_stdout(
                "Invalid argument: bogus. Valid options are: low, medium, high, xhigh, max, \
                 ultracode, auto"
            ),
            EffortStdout::Invalid
        );
        assert_eq!(
            parse_effort_stdout("Effort level set to auto"),
            EffortStdout::Other
        );
        // Older build spelling.
        match parse_effort_stdout("Set effort level: high (saved as your default for new sessions)")
        {
            EffortStdout::Accepted(o) => assert_eq!(o.name, EffortName::High),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stdout_accept_is_an_immediate_edge_without_an_assistant_record() {
        // This is the core read-back fix: the level is settled by the verdict
        // line, not by the next turn's assistant record.
        let mut tracker = EffortTracker::new();
        tracker.observe(Some("low"), None);
        tracker.note_slash("xhigh", true);
        let edge = tracker
            .note_stdout(
                "Set effort level to xhigh (saved as your default for new sessions): …",
                true,
            )
            .expect("edge");
        assert_eq!(edge.0.name, EffortName::Xhigh);
        assert_eq!(edge.1, EffortSource::Remuda);
        // A same-level assistant record afterwards is deduped, carrying no flag.
        assert!(tracker.observe(Some("xhigh"), None).is_none());
    }

    #[test]
    fn dismissed_dialog_and_bad_arg_do_not_change_the_level() {
        let mut tracker = EffortTracker::new();
        tracker.observe(Some("xhigh"), None);
        // Esc on the dialog: slash record armed, verdict says kept.
        tracker.note_slash("max", true);
        assert!(
            tracker
                .note_stdout("Kept effort level as xhigh", true)
                .is_none()
        );
        // The refused awaiting must not credit a later natural max edge.
        assert!(tracker.observe(Some("xhigh"), None).is_none());
        // Invalid argument: same story.
        tracker.note_slash("bogus", false);
        assert!(
            tracker
                .note_stdout(
                    "Invalid argument: bogus. Valid options are: low, medium, high, xhigh, max",
                    false
                )
                .is_none()
        );
    }

    #[test]
    fn ultracode_flag_is_sticky_across_later_xhigh_records_until_exit() {
        let mut tracker = EffortTracker::new();
        tracker.observe(Some("high"), None);
        tracker
            .note_stdout(
                "Set effort level to ultracode (this session only): xhigh + dynamic …",
                false,
            )
            .expect("edge to ultracode");
        // The attachment that rides the next prompt is consistent (no edge).
        assert!(tracker.note_ultra_attachment(true).is_none());
        // Many xhigh assistant records later still carry the flag.
        let again = tracker.observe(Some("xhigh"), None);
        assert!(again.is_none(), "deduped, flag latched: {again:?}");
        // Switch back to high: stdout positively turns the flag off.
        let back = tracker
            .note_stdout(
                "Set effort level to high (saved as your default for new sessions): …",
                false,
            )
            .expect("edge back to high");
        assert_eq!(back.0.name, EffortName::High);
        assert_eq!(back.0.ultracode, Some(false));
    }

    #[test]
    fn a_hand_typed_switch_attributes_slash_and_settles_from_stdout() {
        let mut tracker = EffortTracker::new();
        tracker.observe(Some("high"), None);
        tracker.note_slash("low", false);
        let edge = tracker
            .note_stdout("Set effort level to low (saved as your default …)", false)
            .expect("edge");
        assert_eq!(edge.0.name, EffortName::Low);
        assert_eq!(edge.1, EffortSource::Slash);
    }

    #[test]
    fn effort_event_id_is_stable_across_channels_and_namespaced_per_instance() {
        // The live hydrator and the file tailer both observe the same native
        // assistant record; they must draw one event id (live-view §2.3).
        let a = effort_event_id("ins_one", "msg_42", EffortName::Xhigh);
        let b = effort_event_id("ins_one", "msg_42", EffortName::Xhigh);
        assert_eq!(a, b);
        // A different record or level is a different edge.
        assert_ne!(a, effort_event_id("ins_one", "msg_43", EffortName::Xhigh));
        assert_ne!(a, effort_event_id("ins_one", "msg_42", EffortName::High));
        // Ids never collide across sessions.
        assert_ne!(a, effort_event_id("ins_two", "msg_42", EffortName::Xhigh));
        assert!(a.as_id().as_str().starts_with("evt_"));
    }
}
