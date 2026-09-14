//! Per-harness reasoning-effort argv.
//!
//! One effort selection reaches three different CLIs three different ways, and
//! each CLI has its own vocabulary. Evidence (binary/version + source) in
//! `docs/design/evidence/composer-slider-5.md`:
//!
//! | harness | argv | accepted values |
//! | --- | --- | --- |
//! | claude / agy | `--effort <v>` | low medium high xhigh max, plus `ultracode` |
//! | codex | `-c model_reasoning_effort="<v>"` | minimal low medium high xhigh |
//! | grok | `--reasoning-effort <v>` (alias `--effort`) | low medium high xhigh |
//!
//! The vocabulary and the legacy migration (`ultra`/`quick`/`standard`/
//! `max`…) live in ONE place,
//! [`remuda_protocol::normalize_legacy_effort`]; this module only validates
//! that an already-normalized selection is launchable and renders argv.
//! Codex's `codex` binary rejects a top-level `--effort` at parse time, so a
//! selection must be mapped onto `-c`, never forwarded as a flag.

use crate::error::{DriverError, DriverResult};
use remuda_protocol::{AgentKind, EffortName, EffortSelection};

/// The Codex `-c model_reasoning_effort=…` vocabulary (codex-cli 0.147.0
/// `ReasoningEffort::from_str`): the five values every current model catalog
/// shares. `none`/`max`/`ultra` exist in the enum but are model-gated or
/// auto-review-only, so the driver never launches with them.
pub const CODEX_REASONING_EFFORTS: &[&str] = &["minimal", "low", "medium", "high", "xhigh"];

/// Grok's built-in effort menu (`EFFORT_LEVELS` / user guide `/effort`):
/// xhigh · high · medium · low. The wire enum also parses `none`/`minimal`/
/// `max` as power-user spellings, but no current model menu advertises them.
pub const GROK_REASONING_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];

/// Values `claude --effort` accepts: the five levels. `ultracode` rides as the
/// separate flag value and is handled via [`EffortSelection::flag_value`].
pub const CLAUDE_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

///
/// Build the argv tokens that carry one effort selection for a native-PTY
/// launch. `None` emits nothing: an unpinned effort means the CLI uses its own
/// default rather than inheriting a level nobody chose.
pub fn effort_argv(kind: AgentKind, effort: Option<EffortSelection>) -> DriverResult<Vec<String>> {
    let Some(effort) = effort else {
        return Ok(Vec::new());
    };
    match kind {
        AgentKind::Claude | AgentKind::Agy => {
            // agy shares the Claude Code flag shape; `minimal` is not in it.
            if matches!(effort.name, EffortName::Minimal) {
                return Err(DriverError::InvalidLaunchSpec(format!(
                    "--effort minimal is not a Claude/agy level (one of {})",
                    CLAUDE_EFFORTS.join(" / ")
                )));
            }
            Ok(vec!["--effort".into(), effort.flag_value().into()])
        }
        AgentKind::Codex => {
            if effort.ultracode {
                return Err(DriverError::InvalidLaunchSpec(
                    "ultracode is a Claude-only workflow flag; codex has no such effort".into(),
                ));
            }
            let value = codex_reasoning_value(effort.name)?;
            Ok(vec![
                "-c".into(),
                format!("model_reasoning_effort=\"{value}\""),
            ])
        }
        AgentKind::Grok => {
            if effort.ultracode {
                return Err(DriverError::InvalidLaunchSpec(
                    "ultracode is a Claude-only workflow flag; grok has no such effort".into(),
                ));
            }
            let value = grok_reasoning_value(effort.name)?;
            // Canonical long name; `--effort` is only a visible alias.
            Ok(vec!["--reasoning-effort".into(), value.into()])
        }
        // Terminal launches a login shell; generic is rejected before this
        // path by the preset lookup, but never invent effort flags either way.
        AgentKind::Terminal | AgentKind::Generic => Ok(Vec::new()),
    }
}

/// Map a protocol level onto the Codex `model_reasoning_effort` vocabulary.
fn codex_reasoning_value(name: EffortName) -> DriverResult<&'static str> {
    match name {
        EffortName::Minimal => Ok("minimal"),
        EffortName::Low => Ok("low"),
        EffortName::Medium => Ok("medium"),
        EffortName::High => Ok("high"),
        EffortName::Xhigh => Ok("xhigh"),
        EffortName::Max => Err(DriverError::InvalidLaunchSpec(format!(
            "model_reasoning_effort=max is not advertised by the codex models (one of {})",
            CODEX_REASONING_EFFORTS.join(" / ")
        ))),
    }
}

/// Map a protocol level onto the grok `--reasoning-effort` menu vocabulary.
fn grok_reasoning_value(name: EffortName) -> DriverResult<&'static str> {
    match name {
        EffortName::Low => Ok("low"),
        EffortName::Medium => Ok("medium"),
        EffortName::High => Ok("high"),
        EffortName::Xhigh => Ok("xhigh"),
        EffortName::Minimal | EffortName::Max => Err(DriverError::InvalidLaunchSpec(format!(
            "grok --reasoning-effort takes one of {} (the built-in /effort menu)",
            GROK_REASONING_EFFORTS.join(" / ")
        ))),
    }
}

///
/// Reject effort flags a caller smuggled through `spec.args`: the materializer
/// owns this axis per-kind, and a hand-written flag would either repeat the
/// emitted token or use the wrong vocabulary (codex parses no `--effort` at
/// all). The materializer-emitted tokens never pass through this check.
pub fn ensure_no_effort_in_extras(kind: AgentKind, extras: &[String]) -> DriverResult<()> {
    let banned: &[&str] = match kind {
        AgentKind::Claude | AgentKind::Agy => &["--effort"],
        AgentKind::Codex => &["--effort", "--reasoning-effort"],
        AgentKind::Grok => &["--effort", "--reasoning-effort"],
        AgentKind::Terminal | AgentKind::Generic => &[],
    };
    for token in extras {
        let head = token.split('=').next().unwrap_or(token);
        if banned.contains(&head) {
            return Err(DriverError::InvalidLaunchSpec(format!(
                "flag {head} is reserved for the per-kind effort mapping"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(name: EffortName, ultracode: bool) -> EffortSelection {
        EffortSelection { name, ultracode }
    }

    #[test]
    fn absent_effort_emits_nothing_for_every_kind() {
        for kind in [
            AgentKind::Claude,
            AgentKind::Codex,
            AgentKind::Grok,
            AgentKind::Agy,
        ] {
            assert_eq!(effort_argv(kind, None).unwrap(), Vec::<String>::new());
        }
    }

    #[test]
    fn claude_and_agy_use_the_effort_flag() {
        for kind in [AgentKind::Claude, AgentKind::Agy] {
            assert_eq!(
                effort_argv(kind, Some(sel(EffortName::High, false))).unwrap(),
                vec!["--effort".to_string(), "high".to_string()]
            );
            assert_eq!(
                effort_argv(kind, Some(sel(EffortName::Xhigh, true))).unwrap(),
                vec!["--effort".to_string(), "ultracode".to_string()]
            );
            assert!(effort_argv(kind, Some(sel(EffortName::Minimal, false))).is_err());
        }
    }

    #[test]
    fn codex_maps_one_to_one_onto_the_config_overlay() {
        for (name, value) in [
            (EffortName::Minimal, "minimal"),
            (EffortName::Low, "low"),
            (EffortName::Medium, "medium"),
            (EffortName::High, "high"),
            (EffortName::Xhigh, "xhigh"),
        ] {
            assert_eq!(
                effort_argv(AgentKind::Codex, Some(sel(name, false))).unwrap(),
                vec![
                    "-c".to_string(),
                    format!("model_reasoning_effort=\"{value}\"")
                ]
            );
        }
        // `max`/`ultra` are not in the offered set; never passed through.
        assert!(effort_argv(AgentKind::Codex, Some(sel(EffortName::Max, false))).is_err());
        assert!(effort_argv(AgentKind::Codex, Some(sel(EffortName::Xhigh, true))).is_err());
    }

    #[test]
    fn grok_uses_the_reasoning_effort_flag_with_the_menu_vocabulary() {
        for (name, value) in [
            (EffortName::Low, "low"),
            (EffortName::Medium, "medium"),
            (EffortName::High, "high"),
            (EffortName::Xhigh, "xhigh"),
        ] {
            assert_eq!(
                effort_argv(AgentKind::Grok, Some(sel(name, false))).unwrap(),
                vec!["--reasoning-effort".to_string(), value.to_string()]
            );
        }
        assert!(effort_argv(AgentKind::Grok, Some(sel(EffortName::Minimal, false))).is_err());
        assert!(effort_argv(AgentKind::Grok, Some(sel(EffortName::Max, false))).is_err());
        assert!(effort_argv(AgentKind::Grok, Some(sel(EffortName::Xhigh, true))).is_err());
    }

    #[test]
    fn extras_cannot_smuggle_effort_flags() {
        for kind in [AgentKind::Claude, AgentKind::Codex, AgentKind::Grok] {
            assert!(ensure_no_effort_in_extras(kind, &[]).is_ok());
            assert!(ensure_no_effort_in_extras(kind, &["--effort=xhigh".into()]).is_err());
        }
        assert!(
            ensure_no_effort_in_extras(AgentKind::Codex, &["--reasoning-effort=high".into()])
                .is_err()
        );
        assert!(
            ensure_no_effort_in_extras(
                AgentKind::Grok,
                &["--reasoning-effort".into(), "high".into()]
            )
            .is_err()
        );
    }
}
