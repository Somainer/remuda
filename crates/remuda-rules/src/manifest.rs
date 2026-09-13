//! Rule manifest model: the on-disk TOML shape and its compiled form.
//!
//! The TOML shape is herdr's (D-028 §10) and is parsed verbatim from the files
//! under `rules/`. Compiling turns the string patterns into [`regex::Regex`]
//! values once, so evaluation is allocation-light.

use std::fmt;
use std::str::FromStr;

use regex::Regex;
use serde::Deserialize;

use crate::error::Error;
use crate::region::Region;

/// Highest `min_engine_version` this engine implements.
///
/// herdr rejects a manifest whose `min_engine_version` exceeds the running
/// engine ("manifest requires engine N, current engine is M"); we do the same
/// so a newer rule file cannot be silently half-applied. Engine 3 is what the
/// herdr 0.9.0 manifests under `rules/` ask for, and it is the level at which
/// `top_non_empty_lines` is available.
pub const ENGINE_VERSION: u32 = 3;

/// The state a rule asserts when it matches.
///
/// `done` is deliberately absent: D-028 §10 derives it from per-device seen
/// state (`done = idle ∧ unread since the last turn`), never from a screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Accepting input.
    Idle,
    /// A turn is in flight.
    Working,
    /// Waiting on a human: approval, question, trust dialog.
    Blocked,
    /// No rule spoke. Never collapses into [`State::Idle`].
    Unknown,
}

impl State {
    /// The lowercase wire spelling, matching the TOML.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for State {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "idle" => Ok(Self::Idle),
            "working" => Ok(Self::Working),
            "blocked" => Ok(Self::Blocked),
            "unknown" => Ok(Self::Unknown),
            other => Err(Error::UnknownState(other.to_owned())),
        }
    }
}

/// A matcher gate: leaf matchers plus `all` / `any` / `not` combinators.
///
/// A gate holds any mix of the three leaf matcher kinds and the three
/// combinators; the whole gate is a conjunction over whichever are present
/// (see [`crate::eval`] for the exact semantics). herdr's own validator rejects
/// a gate with no positive matcher and a `not` gate that is empty, so
/// [`Manifest::compile`] rejects those too.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gate {
    /// Case-insensitive substring matchers; all listed strings must be present.
    #[serde(default)]
    pub contains: Vec<String>,
    /// Regexes tested against the whole region text; all must match.
    #[serde(default)]
    pub regex: Vec<String>,
    /// Regexes tested per line; every listed pattern must hit some line.
    #[serde(default)]
    pub line_regex: Vec<String>,
    /// Sub-gates that must all hold.
    #[serde(default)]
    pub all: Vec<Gate>,
    /// Sub-gates of which at least one must hold.
    #[serde(default)]
    pub any: Vec<Gate>,
    /// Sub-gates of which none may hold.
    #[serde(default)]
    pub not: Vec<Gate>,
}

impl Gate {
    /// True when the gate carries no matcher and no combinator at all.
    fn is_empty(&self) -> bool {
        self.contains.is_empty()
            && self.regex.is_empty()
            && self.line_regex.is_empty()
            && self.all.is_empty()
            && self.any.is_empty()
            && self.not.is_empty()
    }

    /// True when the gate asserts something positive, i.e. it is more than a
    /// bare `not`. herdr requires this of every rule's top-level gate.
    fn has_positive(&self) -> bool {
        !self.contains.is_empty()
            || !self.regex.is_empty()
            || !self.line_regex.is_empty()
            || !self.all.is_empty()
            || !self.any.is_empty()
    }
}

/// One rule as written in the TOML.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Rule name, unique within the manifest; reported in the verdict.
    pub id: String,
    /// State this rule asserts on a match.
    pub state: String,
    /// Higher wins. Ties break on document order (see [`crate::eval`]).
    pub priority: i64,
    /// Where on the screen the matchers look.
    pub region: String,
    /// Marks the match as positive evidence of idleness.
    #[serde(default)]
    pub visible_idle: bool,
    /// Marks the match as positive evidence of a human-blocking dialog.
    #[serde(default)]
    pub visible_blocker: bool,
    /// Marks the match as positive evidence of an in-flight turn.
    #[serde(default)]
    pub visible_working: bool,
    /// Match means "do not touch the state at all" — transcript viewers and
    /// model pickers look exactly like approval dialogs (D-028 §10 anchor ③).
    #[serde(default)]
    pub skip_state_update: bool,
    /// The rule's matcher gate.
    #[serde(flatten)]
    pub gate: Gate,
}

/// A manifest as written in the TOML: one agent kind, one file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Agent kind, e.g. `claude`.
    pub id: String,
    /// Datestamped manifest version, e.g. `2026.09.04.1`.
    pub version: String,
    /// Engine level the rules need; rejected above [`ENGINE_VERSION`].
    pub min_engine_version: u32,
    /// Upstream edit date.
    pub updated_at: String,
    /// Other names this manifest answers to.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// The rules, in document order.
    pub rules: Vec<Rule>,
}

impl Manifest {
    /// Parse a manifest from TOML text.
    ///
    /// # Errors
    /// Returns [`Error::Toml`] if the document does not parse.
    pub fn parse(text: &str) -> Result<Self, Error> {
        toml::from_str(text).map_err(|source| Error::Toml {
            source: Box::new(source),
        })
    }

    /// Parse and compile in one step.
    ///
    /// # Errors
    /// See [`Manifest::parse`] and [`Manifest::compile`].
    pub fn compile_str(text: &str) -> Result<CompiledManifest, Error> {
        Self::parse(text)?.compile()
    }

    /// Compile every regex and validate the manifest's invariants.
    ///
    /// # Errors
    /// Returns [`Error::EngineTooOld`] when the manifest needs a newer engine,
    /// and [`Error::Rule`] for an empty rule set, a duplicate or empty rule id,
    /// an unknown state or region, a gate with no positive matcher, an empty
    /// `not` gate, a `skip_state_update` rule that is not `unknown` or that
    /// also carries visible-state evidence, or a bad regex.
    pub fn compile(self) -> Result<CompiledManifest, Error> {
        if self.min_engine_version > ENGINE_VERSION {
            return Err(Error::EngineTooOld {
                manifest: self.id,
                required: self.min_engine_version,
                engine: ENGINE_VERSION,
            });
        }
        if self.rules.is_empty() {
            return Err(Error::rule(
                &self.id,
                "",
                "manifest must contain at least one rule",
            ));
        }

        let mut rules = Vec::with_capacity(self.rules.len());
        let mut seen: Vec<&str> = Vec::with_capacity(self.rules.len());
        for rule in &self.rules {
            if rule.id.is_empty() {
                return Err(Error::rule(&self.id, "", "rule id must not be empty"));
            }
            if seen.contains(&rule.id.as_str()) {
                return Err(Error::rule(&self.id, &rule.id, "duplicate rule id"));
            }
            seen.push(&rule.id);
            rules.push(CompiledRule::compile(&self.id, rule)?);
        }

        Ok(CompiledManifest {
            id: self.id,
            version: self.version,
            min_engine_version: self.min_engine_version,
            updated_at: self.updated_at,
            aliases: self.aliases,
            rules,
        })
    }
}

/// A gate with its regexes compiled.
#[derive(Debug)]
pub struct CompiledGate {
    pub(crate) contains: Vec<String>,
    pub(crate) regex: Vec<Regex>,
    pub(crate) line_regex: Vec<Regex>,
    pub(crate) all: Vec<CompiledGate>,
    pub(crate) any: Vec<CompiledGate>,
    pub(crate) not: Vec<CompiledGate>,
}

impl CompiledGate {
    fn compile(manifest: &str, rule: &str, gate: &Gate, in_not: bool) -> Result<Self, Error> {
        if in_not && gate.is_empty() {
            return Err(Error::rule(
                manifest,
                rule,
                "not gate must contain a matcher",
            ));
        }
        let build = |kind: &str, pats: &[String]| -> Result<Vec<Regex>, Error> {
            pats.iter()
                .map(|p| {
                    Regex::new(p).map_err(|source| Error::Regex {
                        manifest: manifest.to_owned(),
                        rule: rule.to_owned(),
                        kind: kind.to_owned(),
                        pattern: p.clone(),
                        source: Box::new(source),
                    })
                })
                .collect()
        };
        let sub = |gates: &[Gate], in_not: bool| -> Result<Vec<Self>, Error> {
            gates
                .iter()
                .map(|g| Self::compile(manifest, rule, g, in_not))
                .collect()
        };
        Ok(Self {
            // herdr lowercases both sides of a `contains` test; pre-fold the
            // needle so evaluation only folds the haystack.
            contains: gate.contains.iter().map(|s| s.to_lowercase()).collect(),
            regex: build("regex", &gate.regex)?,
            line_regex: build("line_regex", &gate.line_regex)?,
            all: sub(&gate.all, in_not)?,
            any: sub(&gate.any, in_not)?,
            not: sub(&gate.not, true)?,
        })
    }
}

/// A rule with its region parsed and its regexes compiled.
#[derive(Debug)]
pub struct CompiledRule {
    /// Rule name, reported in [`crate::Verdict::rule`].
    pub id: String,
    /// State asserted on a match.
    pub state: State,
    /// Higher wins.
    pub priority: i64,
    /// Where the matchers look.
    pub region: Region,
    /// Positive evidence of idleness.
    pub visible_idle: bool,
    /// Positive evidence of a blocking dialog.
    pub visible_blocker: bool,
    /// Positive evidence of an in-flight turn.
    pub visible_working: bool,
    /// Match suppresses the state update entirely.
    pub skip_state_update: bool,
    pub(crate) gate: CompiledGate,
}

impl CompiledRule {
    fn compile(manifest: &str, rule: &Rule) -> Result<Self, Error> {
        let state = rule.state.parse::<State>()?;
        let region = rule.region.parse::<Region>()?;
        if !rule.gate.has_positive() {
            return Err(Error::rule(
                manifest,
                &rule.id,
                "must contain a positive matcher",
            ));
        }
        // herdr's two skip_state_update invariants, kept verbatim so a
        // hand-edited manifest cannot quietly turn a viewer rule into a
        // state-setting one.
        if rule.skip_state_update {
            if state != State::Unknown {
                return Err(Error::rule(
                    manifest,
                    &rule.id,
                    r#"uses skip_state_update without state = "unknown""#,
                ));
            }
            if rule.visible_idle || rule.visible_blocker || rule.visible_working {
                return Err(Error::rule(
                    manifest,
                    &rule.id,
                    "uses skip_state_update with visible state evidence",
                ));
            }
        }
        Ok(Self {
            id: rule.id.clone(),
            state,
            priority: rule.priority,
            region,
            visible_idle: rule.visible_idle,
            visible_blocker: rule.visible_blocker,
            visible_working: rule.visible_working,
            skip_state_update: rule.skip_state_update,
            gate: CompiledGate::compile(manifest, &rule.id, &rule.gate, false)?,
        })
    }
}

/// A manifest ready to evaluate against a screen.
#[derive(Debug)]
pub struct CompiledManifest {
    /// Agent kind.
    pub id: String,
    /// Manifest version.
    pub version: String,
    /// Engine level the rules need.
    pub min_engine_version: u32,
    /// Upstream edit date.
    pub updated_at: String,
    /// Other names this manifest answers to.
    pub aliases: Vec<String>,
    /// Rules in document order; [`crate::eval`] does the priority sort.
    pub rules: Vec<CompiledRule>,
}

impl CompiledManifest {
    /// True when `name` is this manifest's id or one of its aliases.
    #[must_use]
    pub fn matches(&self, name: &str) -> bool {
        self.id == name || self.aliases.iter().any(|a| a == name)
    }
}
