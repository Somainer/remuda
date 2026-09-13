//! Errors from parsing, compiling, and loading rule manifests.

use std::fmt;

/// Anything that can go wrong before evaluation. Evaluation itself is
/// infallible — a screen that matches nothing yields [`crate::State::Unknown`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The TOML document did not parse.
    #[error("manifest is not valid TOML: {source}")]
    Toml {
        /// Underlying `toml` error.
        source: Box<toml::de::Error>,
    },

    /// A rule named a state outside `idle` / `working` / `blocked` / `unknown`.
    #[error("rule has invalid state: {0}")]
    UnknownState(String),

    /// A rule named a region the engine does not implement.
    #[error("rule uses invalid region: {0}")]
    UnknownRegion(String),

    /// The manifest asks for an engine newer than this one.
    #[error("{manifest} manifest requires engine {required}, current engine is {engine}")]
    EngineTooOld {
        /// Manifest id.
        manifest: String,
        /// Level the manifest asks for.
        required: u32,
        /// Level this engine implements.
        engine: u32,
    },

    /// A manifest or rule invariant was violated.
    #[error("{manifest} manifest is invalid: {0}", RuleContext(.rule, .message))]
    Rule {
        /// Manifest id.
        manifest: String,
        /// Rule id, empty when the problem is manifest-wide.
        rule: String,
        /// What went wrong.
        message: String,
    },

    /// A matcher pattern did not compile.
    #[error("{manifest} rule {rule} contains invalid {kind} pattern {pattern:?}: {source}")]
    Regex {
        /// Manifest id.
        manifest: String,
        /// Rule id.
        rule: String,
        /// `regex` or `line_regex`.
        kind: String,
        /// The offending pattern.
        pattern: String,
        /// Underlying `regex` error.
        source: Box<regex::Error>,
    },

    /// A bundled manifest file could not be found by kind.
    #[error("no bundled manifest for agent kind {0:?}")]
    UnknownKind(String),
}

impl Error {
    pub(crate) fn rule(manifest: &str, rule: &str, message: &str) -> Self {
        Self::Rule {
            manifest: manifest.to_owned(),
            rule: rule.to_owned(),
            message: message.to_owned(),
        }
    }
}

/// Renders `rule <id> <message>` when a rule is named, else just the message.
struct RuleContext<'a>(&'a str, &'a str);

impl fmt::Display for RuleContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            f.write_str(self.1)
        } else {
            write!(f, "rule {} {}", self.0, self.1)
        }
    }
}
