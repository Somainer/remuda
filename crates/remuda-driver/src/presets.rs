//! Carrier-independent per-kind launch presets; D-028 §5.1.
//!
//! These tables used to live inside [`crate::generic_pty`], which made them
//! reachable only through the Herdr driver. Native PTY launches need exactly
//! the same argv and the same yolo gating, so the table moved here and both
//! carriers read one copy. Values are unchanged from the Herdr-era table —
//! D-028 §5.1 requires re-verifying each against its binary pin, not editing
//! them during the move.

use crate::error::{DriverError, DriverResult};
use crate::materializer::LaunchOrigin;
use remuda_protocol::{AgentKind, ClaudePermissionMode, InstanceSpec, PermissionMode};

/// Per-kind PTY launch conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KindPreset {
    /// Product id (`claude`, `codex`, `grok`, `agy`, `gemini`).
    pub id: &'static str,
    /// Default executable on PATH.
    pub binary: &'static str,
    /// Herdr `agent.start --kind`. Ignored by the native carrier.
    pub herdr_kind: &'static str,
    /// Non-interactive / yolo argv appended when missing.
    pub yolo_argv: &'static [&'static str],
    /// Optional CLI flag that receives the herdr agent name.
    pub name_flag: Option<&'static str>,
    /// Whether SessionStart journal hooks are injected.
    pub journals: bool,
    /// Treat herdr `done` as wait-until idle.
    pub done_means_idle: bool,
    /// Env name that redirects this harness's config home, when it has one.
    ///
    /// D-028 §5.1: codex reads `CODEX_HOME`, grok reads `GROK_HOME`; claude
    /// takes a `--settings` overlay on argv instead, and agy has neither.
    pub home_env: Option<&'static str>,
    /// Whether the launch overlay is passed as `--settings <path>`.
    pub settings_flag: bool,
    /// Flag that carries an explicit model pin, when this CLI takes one on an
    /// interactive launch.
    ///
    /// D-036 / model-pin-1: the native PTY agent path used to emit no model
    /// flag at all, so a dispatch `--model` was recorded in every audit record
    /// and roster row while the host's own default answered every turn. Only
    /// claude is wired here: codex and grok take their model through their own
    /// config/subcommand vocabulary, and agy's interactive launch has no pin
    /// flag to give — inventing a spelling for them would be a launch crash,
    /// not a fix.
    pub model_flag: Option<&'static str>,
}

/// Built-in presets for dogfood kinds.
pub const PRESETS: &[KindPreset] = &[
    KindPreset {
        id: "claude",
        binary: "claude",
        herdr_kind: "claude",
        yolo_argv: &["--dangerously-skip-permissions"],
        name_flag: None,
        journals: true,
        done_means_idle: true,
        home_env: None,
        settings_flag: true,
        model_flag: Some("--model"),
    },
    KindPreset {
        id: "codex",
        binary: "codex",
        herdr_kind: "codex",
        yolo_argv: &["--dangerously-bypass-approvals-and-sandbox"],
        name_flag: None,
        journals: false,
        done_means_idle: true,
        home_env: Some("CODEX_HOME"),
        settings_flag: false,
        model_flag: None,
    },
    KindPreset {
        id: "grok",
        binary: "grok",
        herdr_kind: "grok",
        yolo_argv: &["--always-approve"],
        name_flag: None,
        journals: false,
        done_means_idle: true,
        home_env: Some("GROK_HOME"),
        settings_flag: false,
        model_flag: None,
    },
    KindPreset {
        id: "agy",
        binary: "agy",
        herdr_kind: "agy",
        yolo_argv: &["--dangerously-skip-permissions"],
        name_flag: None,
        journals: false,
        done_means_idle: true,
        home_env: None,
        settings_flag: false,
        model_flag: None,
    },
    KindPreset {
        id: "gemini",
        binary: "gemini",
        herdr_kind: "gemini",
        yolo_argv: &["--yolo"],
        name_flag: None,
        journals: false,
        done_means_idle: true,
        home_env: None,
        settings_flag: false,
        model_flag: None,
    },
];

/// Look up a preset by product id.
pub fn preset_by_id(id: &str) -> Option<&'static KindPreset> {
    PRESETS
        .iter()
        .find(|preset| preset.id.eq_ignore_ascii_case(id))
}

/// Look up a preset from [`AgentKind`] (and optional spec args for gemini).
pub fn preset_for_spec(spec: &InstanceSpec) -> DriverResult<&'static KindPreset> {
    let id = match spec.kind {
        AgentKind::Claude => "claude",
        AgentKind::Codex => "codex",
        AgentKind::Grok => "grok",
        AgentKind::Agy => "agy",
        AgentKind::Generic => spec
            .args
            .iter()
            .find(|arg| preset_by_id(arg).is_some())
            .map(String::as_str)
            .or(spec.model_id.as_deref())
            .unwrap_or("gemini"),
        AgentKind::Terminal => {
            return Err(DriverError::InvalidLaunchSpec(
                "kind terminal launches a login shell, not an agent preset".into(),
            ));
        }
    };
    preset_by_id(id)
        .ok_or_else(|| DriverError::InvalidLaunchSpec(format!("no launch preset for kind {id}")))
}

/// Append the preset's yolo argv when — and only when — D-011 / D-017 allow it.
///
/// Two independent gates, both required: the spec explicitly asks for
/// `bypassPermissions`, and the launch did not originate from an agent. A bot
/// dispatcher may pass a human's explicit bypass through, but never mint one.
pub fn merge_yolo_argv(
    argv: &mut Vec<String>,
    preset: &KindPreset,
    permission: &PermissionMode,
    origin: LaunchOrigin,
) {
    if !matches!(origin, LaunchOrigin::Human | LaunchOrigin::Bot)
        || !matches!(permission, PermissionMode::Claude(mode) if mode.mode == ClaudePermissionMode::BypassPermissions)
    {
        return;
    }
    for flag in preset.yolo_argv {
        if !argv.iter().any(|token| token == flag) {
            argv.push((*flag).to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_protocol::{ClaudeInteractionMode, ClaudePermission};

    #[test]
    fn presets_cover_dogfood_kinds() {
        for id in ["claude", "codex", "grok", "agy", "gemini"] {
            assert!(preset_by_id(id).is_some(), "{id}");
        }
        assert!(preset_by_id("codex").unwrap().yolo_argv[0].contains("bypass"));
        assert!(preset_by_id("claude").unwrap().journals);
        assert!(!preset_by_id("codex").unwrap().journals);
        assert_eq!(preset_by_id("codex").unwrap().home_env, Some("CODEX_HOME"));
        assert_eq!(preset_by_id("grok").unwrap().home_env, Some("GROK_HOME"));
        assert_eq!(preset_by_id("claude").unwrap().home_env, None);
        assert!(preset_by_id("claude").unwrap().settings_flag);
    }

    #[test]
    fn yolo_requires_explicit_bypass_and_non_agent_origin_for_every_preset() {
        for name in ["claude", "codex", "grok", "agy", "gemini"] {
            let preset = preset_by_id(name).unwrap();
            for origin in [LaunchOrigin::Human, LaunchOrigin::Bot, LaunchOrigin::Agent] {
                for mode in [
                    ClaudePermissionMode::Manual,
                    ClaudePermissionMode::DontAsk,
                    ClaudePermissionMode::BypassPermissions,
                ] {
                    let mut argv = vec!["--name".into(), "worker".into()];
                    let permission = PermissionMode::Claude(Box::new(ClaudePermission {
                        mode,
                        interaction: ClaudeInteractionMode::NativeTty,
                    }));
                    merge_yolo_argv(&mut argv, preset, &permission, origin);
                    let allowed = mode == ClaudePermissionMode::BypassPermissions
                        && origin != LaunchOrigin::Agent;
                    for flag in preset.yolo_argv {
                        assert_eq!(
                            argv.iter().any(|arg| arg == flag),
                            allowed,
                            "{name} {origin:?} {mode:?}"
                        );
                    }
                }
            }
        }
    }
}
