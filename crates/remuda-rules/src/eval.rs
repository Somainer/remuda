//! Matcher evaluation and priority resolution.
//!
//! Gate semantics, matching herdr's: a gate is a conjunction over whichever of
//! its six fields are present. Every `contains` string must appear, every
//! `regex` must match the region text, every `line_regex` must hit some line,
//! every `all` sub-gate must hold, at least one `any` sub-gate must hold, and
//! no `not` sub-gate may hold. An absent field imposes nothing.
//!
//! Resolution: the highest-priority matching rule wins; ties break on document
//! order so a manifest's own ordering is the tiebreak and the outcome is
//! deterministic. A winning `skip_state_update` rule suppresses the update
//! instead of setting a state.

use crate::manifest::{CompiledGate, CompiledManifest, CompiledRule, State};
use crate::region::Region;
use crate::screen::Screen;

/// What the engine concluded about one screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// The state to apply.
    ///
    /// [`State::Unknown`] means no rule matched. Per D-028 §10 that is an
    /// honest "nothing spoke", **not** a proof of completion, and callers must
    /// never fold it into [`State::Idle`].
    pub state: State,
    /// Id of the rule that won, if any.
    pub rule: Option<String>,
    /// The region that rule looked at, in its TOML spelling.
    pub region: Option<String>,
    /// Priority of the winning rule.
    pub priority: Option<i64>,
    /// The region lines the winning rule matched on, as evidence.
    pub evidence: Vec<String>,
    /// True when the winner carried `skip_state_update`: a transcript viewer
    /// or model picker is on screen and the caller must leave the state alone
    /// rather than write [`State::Unknown`] (anchor ③).
    pub skip_state_update: bool,
    /// The winner's `visible_idle` flag.
    pub visible_idle: bool,
    /// The winner's `visible_blocker` flag.
    pub visible_blocker: bool,
    /// The winner's `visible_working` flag.
    pub visible_working: bool,
}

impl Verdict {
    /// The verdict for a screen no rule spoke about.
    #[must_use]
    pub fn unknown() -> Self {
        Self {
            state: State::Unknown,
            rule: None,
            region: None,
            priority: None,
            evidence: Vec::new(),
            skip_state_update: false,
            visible_idle: false,
            visible_blocker: false,
            visible_working: false,
        }
    }

    /// True when a rule matched, whether or not it sets a state.
    #[must_use]
    pub fn matched(&self) -> bool {
        self.rule.is_some()
    }
}

impl CompiledManifest {
    /// Evaluate every rule against `screen` and return the winner's verdict.
    ///
    /// Evaluation is pure and infallible: no match yields
    /// [`Verdict::unknown`].
    #[must_use]
    pub fn evaluate(&self, screen: &Screen) -> Verdict {
        self.evaluate_all(screen)
            .into_iter()
            .next()
            .map_or_else(Verdict::unknown, |m| m.verdict)
    }

    /// Every matching rule, best first.
    ///
    /// Ordering is by descending priority, then ascending document order. This
    /// is what makes resolution total and stable: no two rules ever compare
    /// equal, because document position is unique.
    #[must_use]
    pub fn evaluate_all(&self, screen: &Screen) -> Vec<Match> {
        let mut matches: Vec<Match> = self
            .rules
            .iter()
            .enumerate()
            .filter_map(|(order, rule)| {
                evaluate_rule(rule, screen).map(|evidence| Match {
                    order,
                    priority: rule.priority,
                    verdict: verdict_for(rule, evidence),
                })
            })
            .collect();
        matches.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.order.cmp(&b.order)));
        matches
    }
}

/// One matching rule, with the position it occupied in the manifest.
#[derive(Debug, Clone)]
pub struct Match {
    /// Zero-based index of the rule in the manifest, the tiebreak key.
    pub order: usize,
    /// The rule's priority, the primary sort key.
    pub priority: i64,
    /// The verdict this rule would produce.
    pub verdict: Verdict,
}

fn verdict_for(rule: &CompiledRule, evidence: Vec<String>) -> Verdict {
    Verdict {
        state: rule.state,
        rule: Some(rule.id.clone()),
        region: Some(rule.region.as_toml()),
        priority: Some(rule.priority),
        evidence,
        skip_state_update: rule.skip_state_update,
        visible_idle: rule.visible_idle,
        visible_blocker: rule.visible_blocker,
        visible_working: rule.visible_working,
    }
}

/// Test one rule; on a match return the region lines that carried it.
fn evaluate_rule(rule: &CompiledRule, screen: &Screen) -> Option<Vec<String>> {
    let lines = rule.region.extract(screen);
    // An empty region cannot satisfy a positive matcher, and every rule is
    // required to have one — so skip the work. This is also what keeps an
    // absent OSC payload from matching `regex = ['\S']`-style catch-alls.
    if lines.is_empty() {
        return None;
    }
    let text = lines.join("\n");
    let mut evidence = Vec::new();
    if gate_matches(&rule.gate, &text, &lines, &mut evidence) {
        if evidence.is_empty() {
            // A gate that matched only on whole-region regexes has no single
            // line to blame; quote the region so the verdict still explains
            // itself.
            evidence.clone_from(&lines);
        }
        evidence.dedup();
        Some(evidence)
    } else {
        None
    }
}

/// Evaluate a gate, appending the lines that carried the match to `evidence`.
fn gate_matches(
    gate: &CompiledGate,
    text: &str,
    lines: &[String],
    evidence: &mut Vec<String>,
) -> bool {
    let lowered = text.to_lowercase();
    for needle in &gate.contains {
        if !lowered.contains(needle.as_str()) {
            return false;
        }
        if let Some(line) = lines
            .iter()
            .find(|l| l.to_lowercase().contains(needle.as_str()))
        {
            evidence.push(line.clone());
        }
    }
    for re in &gate.regex {
        if !re.is_match(text) {
            return false;
        }
    }
    for re in &gate.line_regex {
        match lines.iter().find(|l| re.is_match(l)) {
            Some(line) => evidence.push(line.clone()),
            None => return false,
        }
    }
    for sub in &gate.all {
        if !gate_matches(sub, text, lines, evidence) {
            return false;
        }
    }
    if !gate.any.is_empty() {
        // Only the winning branch contributes evidence, so probe each branch
        // into a scratch buffer and keep the first that holds.
        let mut hit = false;
        for sub in &gate.any {
            let mut scratch = Vec::new();
            if gate_matches(sub, text, lines, &mut scratch) {
                evidence.append(&mut scratch);
                hit = true;
                break;
            }
        }
        if !hit {
            return false;
        }
    }
    for sub in &gate.not {
        let mut scratch = Vec::new();
        if gate_matches(sub, text, lines, &mut scratch) {
            return false;
        }
    }
    true
}

/// Region a caller can evaluate directly, without a manifest.
///
/// Useful for carriers that want to inspect what a region resolves to before
/// wiring up rules.
#[must_use]
pub fn region_lines(region: Region, screen: &Screen) -> Vec<String> {
    region.extract(screen)
}
