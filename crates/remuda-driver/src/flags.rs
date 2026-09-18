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

/// Extra flags `spec.args` may append after the template, per CLI family.
///
/// One table per family rather than one global table: the CLIs do not share a
/// flag vocabulary, and the global list quietly accepted `--effort` and
/// `--max-budget-usd` for `codex`, which has neither — the launch would have
/// failed at startup with the binary's own parse error instead of here, where
/// the message names the flag.
///
/// A flag enters a table only after it was seen in that binary's own `--help`.
/// The list is an evidence gate, not a wish list: `--no-autoupdate` is absent
/// because claude 2.1.221 has no such flag, however harmless it sounds.
const EXTRA_CLAUDE: &[&str] = &[
    "effort",
    "max-budget-usd",
    "add-dir",
    "mcp-config",
    "name",
    // Verified against `claude --help` (2.1.221). `--agents` takes a JSON
    // object of extra agent definitions and `--ide` is a zero-arg connect
    // toggle; neither touches settings sources, permissions, or persistence.
    "agents",
    "ide",
];

/// Grok inherits the historical set, widened with the effort flag it actually
/// parses (`--reasoning-effort`, with `--effort` a visible alias).
///
/// Widening any of these needs the same `--help` evidence claude got.
const EXTRA_GROK: &[&str] = &[
    "effort",
    "reasoning-effort",
    "max-budget-usd",
    "add-dir",
    "mcp-config",
    "name",
];

/// Codex parses NEITHER `--effort` nor `--reasoning-effort` — both are clap
/// errors at startup (`unexpected argument`); its effort axis is the
/// `-c model_reasoning_effort=…` overlay, emitted by [`crate::effort`].
const EXTRA_CODEX: &[&str] = &["max-budget-usd", "add-dir", "mcp-config", "name"];

/// agy keeps the historical set until a `--help` probe says otherwise.
const EXTRA_AGY: &[&str] = &["effort", "max-budget-usd", "add-dir", "mcp-config", "name"];

/// The allowlist a driver's `spec.args` are checked against.
///
/// `shell-pty` and `generic-pty` are kind-polymorphic — one driver hosts
/// claude, codex, grok, or a login shell depending on `spec.kind`, which this
/// function does not see — so they get the widest table. The narrowing that
/// matters lands on the drivers that do name their CLI.
fn extra_allowlist(driver: DriverKind) -> &'static [&'static str] {
    match driver {
        DriverKind::ClaudePrint
        | DriverKind::ClaudeSdk
        | DriverKind::ClaudePty
        | DriverKind::ClaudeBg => EXTRA_CLAUDE,
        DriverKind::CodexAppserver => EXTRA_CODEX,
        DriverKind::GrokAcp => EXTRA_GROK,
        DriverKind::AgyPrint => EXTRA_AGY,
        DriverKind::ShellPty | DriverKind::GenericPty => EXTRA_CLAUDE,
    }
}

/// Values `--effort` accepts on Claude-shaped CLIs: the five D-028 §9.1
/// levels plus `ultracode`.
///
/// The legacy tier names (`default` / `think` / `think-hard`) are normalized
/// to levels by [`remuda_protocol::EffortSelection`] *before* argv is built,
/// so reaching this allowlist with one of them means a caller hand-wrote a
/// flag that the binary would reject. Refuse it here, where the error names
/// the flag, instead of at launch where it is a startup crash.
const CLAUDE_EFFORT_FLAG_VALUES: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultracode"];

/// Values grok's `--reasoning-effort` (alias `--effort`) accepts: the built-in
/// `/effort` menu. Evidence: grok-build `ReasoningEffort::from_str` + user
/// guide, documented in composer-slider-5.md.
const GROK_EFFORT_FLAG_VALUES: &[&str] = &["low", "medium", "high", "xhigh"];

/// Env names that disable native features. A spec may set these to a falsy
/// value; anything truthy is refused. The full denylist is
/// [`crate::child_env::is_denied`].
const NATIVE_FEATURE_ENV: &[&str] = &["CLAUDE_CODE_SIMPLE", "CLAUDE_CODE_SAFE_MODE"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Flag {
    pub name: String,
    pub value: Option<String>,
}

/// Parse `spec.args` as an argv array and reject banned/reserved/unknown flags.
///
/// Re-exported as `remuda_driver::validate_launch_args` so the Hub can return a
/// fast 400 from the same table. The Node stays the authority — the Hub check
/// is an early mirror, not a second copy of the rules.
pub fn validate_spec_args(driver: DriverKind, args: &[String]) -> DriverResult<Vec<String>> {
    if args.is_empty() {
        return Ok(Vec::new());
    }
    let allowlist = extra_allowlist(driver);
    let flags = parse_argv(args)?;
    let mut seen: Vec<String> = Vec::new();
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
        if !allowlist.contains(&key.as_str()) {
            return Err(DriverError::InvalidLaunchSpec(format!(
                "flag --{} is not on the {driver:?} launch allowlist",
                flag.name
            )));
        }
        // A flag the template already emits is caught by RESERVED; this
        // catches the same flag twice inside `spec.args`. Which of the two
        // wins is the binary's business and differs per flag, so refuse
        // rather than silently pick one.
        if seen.contains(&key) {
            return Err(DriverError::InvalidLaunchSpec(format!(
                "flag --{} is repeated",
                flag.name
            )));
        }
        seen.push(key.clone());
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
        if key == "effort" || key == "reasoning-effort" {
            let value = flag.value.as_deref().unwrap_or_default().trim();
            let allowed = match driver {
                DriverKind::GrokAcp => GROK_EFFORT_FLAG_VALUES,
                // Codex never reaches here (its flags are not on its
                // allowlist); claude/agy/shell/generic take the Claude set.
                _ => CLAUDE_EFFORT_FLAG_VALUES,
            };
            if !allowed.contains(&value) {
                return Err(DriverError::InvalidLaunchSpec(format!(
                    "--{key} {value} is not one of {}",
                    allowed.join(" / ")
                )));
            }
        }
    }
    Ok(args.to_vec())
}

/// Reject env bindings that would set CLAUDE_CODE_SIMPLE / SAFE_MODE.
/// Reject an env name a spec wants injected into the agent child.
///
/// The denylist lives in [`crate::child_env`] so the materializer and the
/// spawn sites cannot disagree. It covers the loader, proxy, TLS, and
/// `REMUDA_` families, not just the two native-feature switches that were
/// listed before (`security-review-2.md` S2).
pub(crate) fn reject_banned_env(name: &str, literal: Option<&str>) -> DriverResult<()> {
    if !crate::child_env::is_denied(name) {
        return Ok(());
    }
    // A denied name is refused whatever its value: for the loader and proxy
    // families the name alone is the vulnerability. The historical
    // native-feature switches keep their "only when truthy" behaviour so a
    // spec may still set them to a falsy value explicitly.
    if NATIVE_FEATURE_ENV.contains(&name)
        && let Some(value) = literal
        && !is_truthy(value)
    {
        return Ok(());
    }
    if let Some(value) = literal {
        return Err(DriverError::NativeFeatureDisabled(format!(
            "environment {name}={value} may not be injected"
        )));
    }
    Err(DriverError::NativeFeatureDisabled(format!(
        "environment {name} may not be forwarded"
    )))
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
            | "reasoning-effort"
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
    // `--ide` is deliberately absent: it is a zero-arg toggle, so listing it
    // here would swallow the following token as its value.
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

    #[test]
    fn effort_values_follow_each_clis_vocabulary() {
        for value in ["low", "medium", "high", "xhigh", "max", "ultracode"] {
            assert!(
                validate_spec_args(
                    DriverKind::ShellPty,
                    &["--effort".into(), value.to_string()]
                )
                .is_ok(),
                "shell {value}"
            );
            assert!(
                validate_spec_args(DriverKind::ShellPty, &[format!("--effort={value}")]).is_ok(),
                "shell {value}="
            );
        }
        // D-028 §9.1 retires the legacy tier names. They are normalized to
        // levels before argv is built, so one arriving here is a bug worth an
        // error rather than a flag the binary would reject at startup.
        for value in ["think", "think-hard", "default", "ultra", ""] {
            assert!(
                validate_spec_args(
                    DriverKind::ShellPty,
                    &["--effort".into(), value.to_string()]
                )
                .is_err(),
                "{value}"
            );
        }
        // Codex parses NO effort flag at all.
        for token in ["--effort", "--reasoning-effort"] {
            assert!(
                validate_spec_args(DriverKind::CodexAppserver, &[token.into(), "high".into()])
                    .is_err()
            );
        }
        // Grok takes the built-in menu values under either spelling, and
        // rejects the invented `quick/standard/max` table as much as `ultra`.
        for flag in ["--effort", "--reasoning-effort"] {
            for value in ["low", "medium", "high", "xhigh"] {
                assert!(
                    validate_spec_args(DriverKind::GrokAcp, &[flag.into(), value.to_string()])
                        .is_ok(),
                    "grok {flag} {value}"
                );
            }
            for value in ["quick", "standard", "max", "ultra", "minimal", ""] {
                assert!(
                    validate_spec_args(DriverKind::GrokAcp, &[flag.into(), value.to_string()])
                        .is_err(),
                    "grok {flag} {value}"
                );
            }
        }
    }

    /// Every denied name, in all three spellings a caller might reach for.
    ///
    /// `--x=v` and `-x` are the spellings a substring search or a naive
    /// `contains("--x")` would miss, and each name on these lists breaks the
    /// D-028 overlay/permission contract when it lands on argv.
    #[test]
    fn reserved_and_banned_names_are_rejected_in_every_spelling() {
        for name in RESERVED.iter().chain(BANNED) {
            for token in [
                format!("--{name}"),
                format!("--{name}=v"),
                format!("-{name}"),
            ] {
                assert!(
                    validate_spec_args(DriverKind::ClaudePrint, std::slice::from_ref(&token))
                        .is_err(),
                    "{token} must be refused"
                );
            }
        }
    }

    /// The same flag twice is refused rather than silently resolved: which
    /// occurrence wins is the binary's business and differs per flag.
    #[test]
    fn a_repeated_flag_is_refused() {
        let repeated = validate_spec_args(
            DriverKind::ClaudePrint,
            &[
                "--add-dir".into(),
                "/srv/a".into(),
                "--add-dir".into(),
                "/srv/b".into(),
            ],
        )
        .expect_err("a repeated flag must fail closed");
        assert!(
            repeated.to_string().contains("repeated"),
            "{repeated} should name the repetition"
        );
        // Mixed spellings are the same flag, so they collide too.
        assert!(
            validate_spec_args(
                DriverKind::ClaudePrint,
                &["--effort=high".into(), "--effort".into(), "low".into()],
            )
            .is_err()
        );
        // Two *different* allowlisted flags remain fine.
        assert!(
            validate_spec_args(
                DriverKind::ClaudePrint,
                &["--effort".into(), "high".into(), "--ide".into()],
            )
            .is_ok()
        );
    }

    /// The allowlist is per-CLI. `--ide` and `--agents` were read off
    /// `claude --help` (2.1.221); codex has neither, so its table must not
    /// accept them just because they are harmless to claude.
    #[test]
    fn the_allowlist_diverges_per_driver() {
        for driver in [
            DriverKind::ClaudePrint,
            DriverKind::ClaudePty,
            DriverKind::ClaudeBg,
        ] {
            assert!(
                validate_spec_args(driver, &["--ide".into()]).is_ok(),
                "{driver:?}"
            );
            assert!(
                validate_spec_args(driver, &["--agents".into(), "{}".into()]).is_ok(),
                "{driver:?}"
            );
        }
        for driver in [
            DriverKind::CodexAppserver,
            DriverKind::GrokAcp,
            DriverKind::AgyPrint,
        ] {
            assert!(
                validate_spec_args(driver, &["--ide".into()]).is_err(),
                "{driver:?} has no --ide"
            );
        }
    }

    /// `--ide` takes no value, so the token after it is its own flag rather
    /// than something the parser swallows.
    #[test]
    fn ide_is_a_zero_arg_toggle() {
        let parsed = parse_argv(&["--ide".into(), "--add-dir".into(), "/srv".into()]).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].value, None);
        assert_eq!(parsed[1].value.as_deref(), Some("/srv"));
    }

    /// A prompt is not a launch flag. Positionals stay refused on every
    /// driver, including the widened claude table.
    #[test]
    fn positional_argv_is_still_refused() {
        for driver in [DriverKind::ClaudePrint, DriverKind::CodexAppserver] {
            let error = validate_spec_args(driver, &["summarize the repo".into()])
                .expect_err("a bare prompt must not reach argv");
            assert!(error.to_string().contains("positional"), "{error}");
        }
        assert!(
            validate_spec_args(
                DriverKind::ClaudePrint,
                &["--effort".into(), "high".into(), "trailing prompt".into()],
            )
            .is_err()
        );
    }
}
