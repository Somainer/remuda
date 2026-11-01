//! D-057 OA1, "never beyond the creator": the one permission-mode rule the
//! Hub (`restrict_permission`) and the Node launch materializer both apply to
//! an Agent-origin create, dispatch and respawn (main-agent.md §4.2 item 3,
//! protocol.md §Origin and §4.3).
//!
//! - A child an Agent creates may use a mode that switches its harness's own
//!   permission control off (Claude `bypassPermissions`/`dontAsk`, Codex
//!   `never`, Grok `always-approve`, Agy `always-proceed`) only when its Agent
//!   creator itself runs with that control off. Any mode that keeps the
//!   harness's control on is allowed.
//! - An omitted mode inherits the creator's own mode word. The framework picks
//!   no new mode and translates nothing between harnesses, so a creator word
//!   outside the child harness's vocabulary is refused with a reason and the
//!   caller names a mode.
//!
//! Bot origin is not governed here: its control-off modes stay refused (D-011).
//! Human origin is unchanged.

use serde_json::Value;

/// The Hub-stamped field on a forwarded Agent-origin `instance.create` /
/// `instance.resume`: the creator chapter's effective permission mode, read
/// at the create's commit point. Never taken from a request body.
pub const CREATOR_PERMISSION_MODE_FIELD: &str = "creatorPermissionMode";

/// The control-off word of every harness (OA1).
pub const CONTROL_OFF_MODES: [&str; 5] = [
    "bypassPermissions",
    "dontAsk",
    "never",
    "always-approve",
    "always-proceed",
];

const CLAUDE: &[&str] = &[
    "manual",
    "auto",
    "acceptEdits",
    "plan",
    "dontAsk",
    "bypassPermissions",
];
const CODEX: &[&str] = &["untrusted", "on-request", "never"];
const GROK: &[&str] = &["native-prompt", "auto", "always-approve"];
const AGY: &[&str] = &["native", "accept-edits", "plan", "always-proceed"];
const GENERIC: &[&str] = &["native"];

/// The canonical permission vocabulary of a harness (agent `kind`).
///
/// `terminal` reads the Claude words because the Node maps its permission
/// through the Claude path; an unknown kind has no vocabulary at all.
#[must_use]
pub fn vocabulary(kind: &str) -> &'static [&'static str] {
    match kind {
        "claude" | "terminal" => CLAUDE,
        "codex" => CODEX,
        "grok" => GROK,
        "agy" => AGY,
        "generic" => GENERIC,
        _ => &[],
    }
}

/// Whether `mode` switches its harness's own permission control off.
#[must_use]
pub fn is_control_off(mode: &str) -> bool {
    CONTROL_OFF_MODES.contains(&mode)
}

/// Whether a Hub stamp names a creator that runs with control off. An absent
/// stamp (an older Hub, or a create the Hub did not admit as Agent-origin)
/// reads as control on: the Node fails closed.
#[must_use]
pub fn stamp_is_control_off(stamp: Option<&str>) -> bool {
    stamp.is_some_and(is_control_off)
}

/// The canonical mode a Node launch of `kind` runs for a recorded mode word.
///
/// This is the Node's own spelling map (`crates/remuda-node/src/native.rs`
/// `instance_spec` / `build_permission_mode`): every alias it accepts and the
/// harness default it falls back to for an absent or unknown word. The Hub
/// reads a creator's recorded `permissionMode` through it, so "the creator's
/// mode" is what the creator's process actually launched with.
#[must_use]
pub fn launched_mode(kind: &str, word: Option<&str>) -> &'static str {
    let word = word.map(str::trim).unwrap_or_default();
    match kind {
        "codex" => match word {
            "never" | "no-request" => "never",
            "on-request" | "onRequest" => "on-request",
            _ => "untrusted",
        },
        "grok" => match word {
            "always-approve" | "alwaysApprove" | "bypassPermissions" => "always-approve",
            "auto" => "auto",
            _ => "native-prompt",
        },
        "agy" => match word {
            "always-proceed" | "alwaysProceed" | "bypassPermissions" => "always-proceed",
            "accept-edits" | "acceptEdits" => "accept-edits",
            "plan" => "plan",
            _ => "native",
        },
        "generic" => "native",
        _ => match word {
            "auto" => "auto",
            "acceptEdits" | "accept-edits" => "acceptEdits",
            "dontAsk" | "dont-ask" => "dontAsk",
            "plan" => "plan",
            "bypassPermissions" | "bypass-permissions" | "bypass" => "bypassPermissions",
            _ => "manual",
        },
    }
}

/// The effective permission mode recorded on an instance (D-035): the
/// transcript-observed `permissionEffective.mode` when the Node reported one,
/// else the mode its launch ran for the requested `permissionMode`.
///
/// `spec` is the instance's stored spec. Shared by the Hub's admission read
/// and the in-writer commit-point re-read, so the two can never disagree.
#[must_use]
pub fn recorded_mode(kind: &str, spec: &Value) -> &'static str {
    if matches!(kind, "claude" | "terminal")
        && let Some(observed) = spec
            .pointer("/permissionEffective/mode")
            .and_then(Value::as_str)
            .and_then(crate::normalize_permission_word)
    {
        return crate::permission_mode_wire(observed);
    }
    launched_mode(kind, spec.get("permissionMode").and_then(Value::as_str))
}

/// Why an Agent-origin mode was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The requested word is not in the child harness's vocabulary.
    NotInVocabulary {
        /// Child harness.
        kind: String,
        /// The word the caller asked for.
        requested: String,
    },
    /// The mode was omitted and the creator's own mode is not a word of the
    /// child harness, so there is nothing to inherit.
    CreatorNotInVocabulary {
        /// Child harness.
        kind: String,
        /// The creator's mode.
        creator: String,
    },
    /// A control-off mode under a creator that keeps its control on.
    BeyondCreator {
        /// Child harness.
        kind: String,
        /// The control-off mode the child would run.
        mode: String,
        /// The creator's mode.
        creator: String,
    },
}

impl Refusal {
    /// Stable lowercase reason code for an error body.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::NotInVocabulary { .. } => "permission-mode-unknown",
            Self::CreatorNotInVocabulary { .. } => "permission-mode-not-inheritable",
            Self::BeyondCreator { .. } => "beyond-creator",
        }
    }

    /// What the caller should do instead.
    #[must_use]
    pub fn hint(&self) -> String {
        match self {
            Self::NotInVocabulary { kind, .. } | Self::CreatorNotInVocabulary { kind, .. } => {
                format!(
                    "name a permissionMode from the {kind} vocabulary: {}",
                    vocabulary(kind).join(", ")
                )
            }
            Self::BeyondCreator { kind, .. } => format!(
                "name a {kind} mode that keeps permission control on: {}",
                vocabulary(kind)
                    .iter()
                    .filter(|mode| !is_control_off(mode))
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInVocabulary { kind, requested } => write!(
                formatter,
                "permissionMode {requested:?} is not a {kind} permission mode"
            ),
            Self::CreatorNotInVocabulary { kind, creator } => write!(
                formatter,
                "no permissionMode was named and the creator's own mode {creator:?} is not a \
                 {kind} permission mode, so there is nothing to inherit (D-057 OA1)"
            ),
            Self::BeyondCreator {
                kind,
                mode,
                creator,
            } => write!(
                formatter,
                "permissionMode {mode:?} switches {kind}'s permission control off, and the \
                 creator runs {creator:?} with its control on: a child never runs beyond its \
                 creator (D-057 OA1)"
            ),
        }
    }
}

/// Admit an Agent-origin child mode under the creator rule.
///
/// `requested` is the caller's word verbatim (`None` when omitted); `creator`
/// is the creator chapter's effective mode ([`recorded_mode`]). Returns the
/// canonical effective mode the child launches with.
pub fn admit(kind: &str, requested: Option<&str>, creator: &str) -> Result<&'static str, Refusal> {
    let words = vocabulary(kind);
    let effective = match requested {
        Some(word) => words
            .iter()
            .find(|candidate| **candidate == word)
            .copied()
            .ok_or_else(|| Refusal::NotInVocabulary {
                kind: kind.to_string(),
                requested: word.to_string(),
            })?,
        None => words
            .iter()
            .find(|candidate| **candidate == creator)
            .copied()
            .ok_or_else(|| Refusal::CreatorNotInVocabulary {
                kind: kind.to_string(),
                creator: creator.to_string(),
            })?,
    };
    if is_control_off(effective) && !is_control_off(creator) {
        return Err(Refusal::BeyondCreator {
            kind: kind.to_string(),
            mode: effective.to_string(),
            creator: creator.to_string(),
        });
    }
    Ok(effective)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HARNESSES: [&str; 4] = ["claude", "codex", "grok", "agy"];

    /// Every creator word of every harness (canonical spellings).
    fn creator_words() -> Vec<&'static str> {
        let mut words = Vec::new();
        for kind in HARNESSES {
            for word in vocabulary(kind) {
                if !words.contains(word) {
                    words.push(*word);
                }
            }
        }
        words
    }

    #[test]
    fn every_harness_has_exactly_one_control_off_word_except_claude_with_two() {
        for kind in HARNESSES {
            let off: Vec<_> = vocabulary(kind)
                .iter()
                .filter(|mode| is_control_off(mode))
                .collect();
            let expected = if kind == "claude" { 2 } else { 1 };
            assert_eq!(off.len(), expected, "{kind}: {off:?}");
        }
        assert!(
            vocabulary("generic")
                .iter()
                .all(|mode| !is_control_off(mode))
        );
        assert!(vocabulary("cursor").is_empty());
    }

    /// The whole OA1 matrix: harness × creator mode × requested mode.
    #[test]
    fn the_creator_rule_matrix() {
        for kind in HARNESSES {
            for creator in creator_words() {
                // Explicit requests: every word of the child's vocabulary, plus
                // a foreign word and a non-word.
                let mut requests: Vec<Option<&str>> =
                    vocabulary(kind).iter().map(|mode| Some(*mode)).collect();
                requests.push(Some("bogus"));
                requests.push(None);
                for requested in requests {
                    let outcome = admit(kind, requested, creator);
                    let word = requested.unwrap_or(creator);
                    let known = vocabulary(kind).contains(&word);
                    match (known, is_control_off(word), is_control_off(creator)) {
                        (false, _, _) => {
                            let refusal = outcome.expect_err("outside the vocabulary");
                            match requested {
                                Some(_) => assert_eq!(
                                    refusal.reason(),
                                    "permission-mode-unknown",
                                    "{kind} {creator} {requested:?}"
                                ),
                                None => assert_eq!(
                                    refusal.reason(),
                                    "permission-mode-not-inheritable",
                                    "{kind} {creator}"
                                ),
                            }
                        }
                        (true, true, false) => {
                            let refusal = outcome.expect_err("beyond the creator");
                            assert_eq!(refusal.reason(), "beyond-creator");
                            assert!(refusal.to_string().contains(word), "{refusal}");
                        }
                        (true, _, _) => {
                            assert_eq!(
                                outcome.expect("admitted"),
                                word,
                                "{kind} {creator} {requested:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn inheritance_copies_the_creator_word_and_translates_nothing() {
        assert_eq!(admit("claude", None, "auto"), Ok("auto"));
        assert_eq!(admit("grok", None, "auto"), Ok("auto"));
        assert_eq!(admit("agy", None, "plan"), Ok("plan"));
        // Same idea, different spelling: no cross-harness translation.
        assert!(admit("agy", None, "acceptEdits").is_err());
        assert!(admit("codex", None, "manual").is_err());
        // A control-off creator passes its own mode down when it is a word of
        // the child, and only then.
        assert_eq!(
            admit("claude", None, "bypassPermissions"),
            Ok("bypassPermissions")
        );
        assert!(admit("codex", None, "bypassPermissions").is_err());
        assert_eq!(admit("codex", Some("never"), "dontAsk"), Ok("never"));
    }

    #[test]
    fn launched_mode_mirrors_the_node_spelling_map() {
        for (kind, word, expected) in [
            ("claude", None, "manual"),
            ("claude", Some("default"), "manual"),
            ("claude", Some("bypass"), "bypassPermissions"),
            ("claude", Some("accept-edits"), "acceptEdits"),
            ("claude", Some("dont-ask"), "dontAsk"),
            ("codex", Some("bypassPermissions"), "untrusted"),
            ("codex", Some("no-request"), "never"),
            ("codex", None, "untrusted"),
            ("grok", Some("bypassPermissions"), "always-approve"),
            ("grok", None, "native-prompt"),
            ("agy", Some("bypassPermissions"), "always-proceed"),
            ("agy", Some("acceptEdits"), "accept-edits"),
            ("generic", Some("bypassPermissions"), "native"),
        ] {
            assert_eq!(launched_mode(kind, word), expected, "{kind} {word:?}");
        }
    }

    #[test]
    fn recorded_mode_prefers_the_observed_effective_mode() {
        let spec = json!({"permissionMode": "manual",
            "permissionEffective": {"mode": "bypassPermissions", "source": "slash"}});
        assert_eq!(recorded_mode("claude", &spec), "bypassPermissions");
        // A transcript word is folded through the TUI alias table.
        let spec = json!({"permissionMode": "auto", "permissionEffective": {"mode": "default"}});
        assert_eq!(recorded_mode("claude", &spec), "manual");
        // No observation: what the launch ran for the requested word.
        assert_eq!(
            recorded_mode("claude", &json!({"permissionMode": "bypass"})),
            "bypassPermissions"
        );
        assert_eq!(recorded_mode("claude", &json!({})), "manual");
        // Only Claude reports an effective mode; other harnesses read the request.
        let spec = json!({"permissionMode": "never", "permissionEffective": {"mode": "auto"}});
        assert_eq!(recorded_mode("codex", &spec), "never");
    }

    #[test]
    fn an_absent_or_control_on_stamp_reads_as_control_on() {
        assert!(!stamp_is_control_off(None));
        assert!(!stamp_is_control_off(Some("auto")));
        assert!(!stamp_is_control_off(Some("bypass")));
        assert!(stamp_is_control_off(Some("bypassPermissions")));
        assert!(stamp_is_control_off(Some("never")));
    }
}
