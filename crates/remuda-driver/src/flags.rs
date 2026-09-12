//! Allowlist argv parser. Rejects banned flags by token, not substring search.

use crate::error::{DriverError, DriverResult};
use remuda_protocol::DriverKind;

/// Claude flags that must never be emitted or accepted from `InstanceSpec.args`.
const BANNED: &[&str] = &[
    "bare",
    "safe-mode",
    "no-session-persistence",
    "continue",
    "c",
    "cwd",
    "restricted",
    "strict-mcp-config",
    "disallowedtools",
    "disallowed-tools",
    "disable-slash-commands",
    "system-prompt",
    "append-system-prompt",
];

/// Flags the materializer itself emits; `spec.args` may not repeat them.
const RESERVED: &[&str] = &[
    "print",
    "p",
    "input-format",
    "output-format",
    "verbose",
    "include-partial-messages",
    "include-hook-events",
    "forward-subagent-text",
    "replay-user-messages",
    "permission-mode",
    "permission-prompts",
    "permission-prompt-tool",
    "setting-sources",
    "settings",
    "model",
    "session-id",
    "resume",
    "fork-session",
    "bg",
    "background",
    "dangerously-skip-permissions",
    "allow-dangerously-skip-permissions",
];

/// Extra flags `spec.args` may append after the template.
const EXTRA_ALLOWLIST: &[&str] = &["effort", "max-budget-usd", "add-dir", "mcp-config", "name"];

/// Env names that disable native features and must not be injected.
const BANNED_ENV: &[&str] = &["CLAUDE_CODE_SIMPLE", "CLAUDE_CODE_SAFE_MODE"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Flag {
    pub name: String,
    pub value: Option<String>,
}

/// Parse `spec.args` as an argv array and reject banned/reserved/unknown flags.
pub(crate) fn validate_spec_args(
    _driver: DriverKind,
    args: &[String],
) -> DriverResult<Vec<String>> {
    if args.is_empty() {
        return Ok(Vec::new());
    }
    let flags = parse_argv(args)?;
    for flag in &flags {
        let key = canonical(&flag.name);
        if is_banned(&key) {
            return Err(DriverError::NativeFeatureDisabled(format!(
                "flag --{} is prohibited",
                flag.name
            )));
        }
        if RESERVED.contains(&key.as_str()) {
            return Err(DriverError::InvalidLaunchSpec(format!(
                "flag --{} is reserved for the materializer",
                flag.name
            )));
        }
        if !EXTRA_ALLOWLIST.contains(&key.as_str()) {
            return Err(DriverError::InvalidLaunchSpec(format!(
                "flag --{} is not on the launch allowlist",
                flag.name
            )));
        }
        if key == "setting-sources" {
            let empty = flag
                .value
                .as_deref()
                .is_none_or(|value| value.trim().is_empty());
            if empty {
                return Err(DriverError::NativeFeatureDisabled(
                    "empty --setting-sources is prohibited".into(),
                ));
            }
        }
    }
    Ok(args.to_vec())
}

/// Reject env bindings that would set CLAUDE_CODE_SIMPLE / SAFE_MODE.
pub(crate) fn reject_banned_env(name: &str, literal: Option<&str>) -> DriverResult<()> {
    if !BANNED_ENV.contains(&name) {
        return Ok(());
    }
    if let Some(value) = literal
        && is_truthy(value)
    {
        return Err(DriverError::NativeFeatureDisabled(format!(
            "environment {name}={value} disables native features"
        )));
    }
    if literal.is_none() {
        return Err(DriverError::NativeFeatureDisabled(format!(
            "environment {name} may not be forwarded"
        )));
    }
    Ok(())
}

pub(crate) fn is_banned_flag_token(token: &str) -> bool {
    parse_one_name(token).is_some_and(|name| is_banned(&canonical(&name)))
}

fn is_banned(canonical_name: &str) -> bool {
    BANNED.contains(&canonical_name)
}

fn is_truthy(value: &str) -> bool {
    matches!(value, "1" | "true" | "TRUE" | "yes" | "on")
}

fn canonical(name: &str) -> String {
    name.trim_start_matches('-').to_ascii_lowercase()
}

fn parse_one_name(token: &str) -> Option<String> {
    if let Some(rest) = token.strip_prefix("--") {
        let name = rest.split_once('=').map(|(n, _)| n).unwrap_or(rest);
        return Some(name.to_string());
    }
    if let Some(rest) = token.strip_prefix('-')
        && rest.len() == 1
    {
        return Some(rest.to_string());
    }
    None
}

fn parse_argv(args: &[String]) -> DriverResult<Vec<Flag>> {
    let mut flags = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let token = &args[index];
        if let Some(rest) = token.strip_prefix("--") {
            if rest.is_empty() {
                return Err(DriverError::InvalidLaunchSpec("invalid flag --".into()));
            }
            if let Some((name, value)) = rest.split_once('=') {
                flags.push(Flag {
                    name: name.to_string(),
                    value: Some(value.to_string()),
                });
                index += 1;
                continue;
            }
            let name = rest.to_string();
            let key = canonical(&name);
            if takes_value(&key) {
                let value = args.get(index + 1).ok_or_else(|| {
                    DriverError::InvalidLaunchSpec(format!("flag --{name} requires a value"))
                })?;
                if value.starts_with('-') && !takes_value_allowing_dash(&key) {
                    return Err(DriverError::InvalidLaunchSpec(format!(
                        "flag --{name} requires a value"
                    )));
                }
                flags.push(Flag {
                    name,
                    value: Some(value.clone()),
                });
                index += 2;
            } else {
                flags.push(Flag { name, value: None });
                index += 1;
            }
            continue;
        }
        if let Some(rest) = token.strip_prefix('-')
            && !rest.starts_with('-')
            && rest.len() == 1
        {
            flags.push(Flag {
                name: rest.to_string(),
                value: None,
            });
            index += 1;
            continue;
        }
        return Err(DriverError::InvalidLaunchSpec(format!(
            "positional argv {token:?} is not allowed (prompts are not launch flags)"
        )));
    }
    Ok(flags)
}

fn takes_value(canonical_name: &str) -> bool {
    matches!(
        canonical_name,
        "effort"
            | "max-budget-usd"
            | "add-dir"
            | "mcp-config"
            | "name"
            | "model"
            | "session-id"
            | "settings"
            | "setting-sources"
            | "permission-mode"
            | "permission-prompts"
            | "permission-prompt-tool"
            | "input-format"
            | "output-format"
            | "resume"
            | "agents"
    )
}

fn takes_value_allowing_dash(_canonical_name: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banned_flags_are_token_matches_not_substrings() {
        assert!(is_banned_flag_token("--bare"));
        assert!(is_banned_flag_token("--bare=1"));
        assert!(!is_banned_flag_token("--not-bare"));
        assert!(
            validate_spec_args(DriverKind::ClaudePrint, &["--effort".into(), "high".into()])
                .is_ok()
        );
        assert!(validate_spec_args(DriverKind::ClaudePrint, &["--bare".into()]).is_err());
    }
}
