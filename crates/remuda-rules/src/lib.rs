//! Screen-signal rule engine for agent state detection (D-028 §10).
//!
//! herdr's "state classifier" is not a model: it is a table of TOML rules with
//! regions, priorities, matchers and combinators. This crate holds that table
//! as a versioned asset under `rules/` — extracted verbatim from the herdr
//! 0.9.0 binary, see the crate README — plus the engine that evaluates it
//! against a rendered terminal grid.
//!
//! ```
//! use remuda_rules::{Screen, State, bundled};
//!
//! let manifest = bundled("claude").expect("bundled claude manifest");
//! let screen = Screen::new(vec![
//!     "· Exploring the repo… (3s · esc to interrupt)".to_owned(),
//! ])
//! .with_cols(80);
//!
//! let verdict = manifest.evaluate(&screen);
//! assert_eq!(verdict.state, State::Working);
//! assert_eq!(verdict.rule.as_deref(), Some("live_turn_working"));
//! ```
//!
//! # What this crate does not do
//!
//! - **`done` is not a detection result.** D-028 §10 derives it from per-device
//!   seen state (`done = idle ∧ unread since the last turn`), elsewhere.
//! - **`unknown` is not `idle`.** No rule matching means nothing spoke; it is
//!   not evidence that the agent finished. Never collapse the two.
//! - **Latching is the caller's job.** Anchor ④ — a blinking `⚠ Action
//!   Required` drops frames while the pane is unfocused — needs state held
//!   across frames, which a pure per-frame evaluator cannot do. Hold `blocked`
//!   until a positive `visible_idle` or `visible_working` verdict arrives; the
//!   flags on [`Verdict`] are there for exactly that.
//! - **Signal tiering is the caller's job.** Screen is the bottom of the
//!   `Hook > File > OSC > Screen` ladder (D-028 §4.3).

#![cfg_attr(not(test), deny(clippy::unwrap_used))]

mod error;
mod eval;
mod manifest;
mod region;
mod screen;

pub use error::Error;
pub use eval::{Match, Verdict, region_lines};
pub use manifest::{
    CompiledGate, CompiledManifest, CompiledRule, ENGINE_VERSION, Gate, Manifest, Rule, State,
};
pub use region::Region;
pub use screen::Screen;

/// Every manifest bundled with the crate, as `(kind, TOML text)`.
///
/// Regenerate with `scripts/extract-herdr-rules.py`; see the crate README.
pub const BUNDLED: &[(&str, &str)] = &[
    ("agy", include_str!("../rules/agy.toml")),
    ("amp", include_str!("../rules/amp.toml")),
    ("claude", include_str!("../rules/claude.toml")),
    ("cline", include_str!("../rules/cline.toml")),
    ("codex", include_str!("../rules/codex.toml")),
    ("copilot", include_str!("../rules/copilot.toml")),
    ("cursor", include_str!("../rules/cursor.toml")),
    ("devin", include_str!("../rules/devin.toml")),
    ("droid", include_str!("../rules/droid.toml")),
    ("gemini", include_str!("../rules/gemini.toml")),
    ("grok", include_str!("../rules/grok.toml")),
    ("hermes", include_str!("../rules/hermes.toml")),
    ("kilo", include_str!("../rules/kilo.toml")),
    ("kimi", include_str!("../rules/kimi.toml")),
    ("kiro", include_str!("../rules/kiro.toml")),
    ("maki", include_str!("../rules/maki.toml")),
    ("muse", include_str!("../rules/muse.toml")),
    ("opencode", include_str!("../rules/opencode.toml")),
    ("pi", include_str!("../rules/pi.toml")),
    ("qodercli", include_str!("../rules/qodercli.toml")),
    ("qwen", include_str!("../rules/qwen.toml")),
];

/// Load and compile the bundled manifest for `kind`.
///
/// `kind` is matched against each manifest's `id` first, then its `aliases`,
/// so `antigravity` resolves to the `agy` manifest.
///
/// # Errors
/// Returns [`Error::UnknownKind`] when no bundled manifest claims `kind`, and
/// the compile errors from [`Manifest::compile`] otherwise.
pub fn bundled(kind: &str) -> Result<CompiledManifest, Error> {
    if let Some((_, text)) = BUNDLED.iter().find(|(id, _)| *id == kind) {
        return Manifest::compile_str(text);
    }
    // Alias lookup needs the manifests parsed, so it is the slower second pass.
    for (_, text) in BUNDLED {
        let manifest = Manifest::parse(text)?;
        if manifest.aliases.iter().any(|a| a == kind) {
            return manifest.compile();
        }
    }
    Err(Error::UnknownKind(kind.to_owned()))
}
