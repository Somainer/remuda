//! §9.1 effective-effort tracking shared by every Claude transcript mapper.
//!
//! The driver's live mapper (`remuda-driver::TranscriptMapper`) and the
//! journal tailer (`remuda-journal`) must agree on what an assistant record
//! says about effort and how a `/effort` slash record attributes the change,
//! so the state machine lives here rather than being copied.
//!
//! Claude transcript shape measured on 2.1.221
//! (`docs/design/evidence/effort-sync-1.md`):
//!
//! - assistant records carry a top-level `effort: "low|medium|high|xhigh|max"`
//!   and a top-level `perTurnEffort: string | null`;
//! - `/effort xhigh` is journaled as a `user` record whose message content is
//!   `<command-name>/effort</command-name> … <command-args>xhigh</command-args>`;
//! - `ultracode` reads back as level `xhigh`; the workflow boolean is not on
//!   the assistant record.

use crate::{EffortName, EffortSource};

/// An effort level as read off an assistant transcript record.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ObservedEffort {
    /// Observed level.
    pub name: EffortName,
    /// Observed dynamic-workflow flag; `None` unless positively observed via
    /// an immediately preceding `/effort ultracode`.
    pub ultracode: Option<bool>,
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
    ultracode_pending: bool,
    /// A Remuda switch awaiting its read-back at this level.
    awaiting: Option<EffortName>,
}

impl Default for EffortTracker {
    fn default() -> Self {
        Self {
            last: None,
            pending_source: EffortSource::Unknown,
            ultracode_pending: false,
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
    pub fn observe(
        &mut self,
        effort: Option<&str>,
        per_turn: Option<&str>,
    ) -> Option<(ObservedEffort, EffortSource)> {
        let raw = effort
            .filter(|value| !value.is_empty())
            .or_else(|| per_turn.filter(|value| !value.is_empty()))?;
        let name = Self::parse_level(raw)?;
        let ultracode = if self.ultracode_pending {
            Some(true)
        } else {
            None
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
}
