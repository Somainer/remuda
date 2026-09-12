//! JSON argument helpers shared by tool groups.

use anyhow::{Result, anyhow};
use serde_json::Value;

/// Prompt text for a send tool.
///
/// `file` is deliberately unsupported: reading an agent-supplied path here is
/// arbitrary local file read, and the contents come back out through the
/// journal (`security-review-2.md` M5). `remuda instance send --file` stays
/// available to humans on the CLI.
pub(super) fn send_text_from_args(args: &Value) -> Result<String> {
    reject_removed_args(args, &["file"])?;
    args.get("text")
        .and_then(Value::as_str)
        .or_else(|| args.get("input").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("text is required"))
}

/// Fail loudly when an agent passes a parameter that was removed for security,
/// rather than silently ignoring it and doing something else.
///
/// Used for the local-file reads (`file` / `promptFile`, M5) and the
/// caller-chosen worktree location (`path` / `repo`, M4).
pub(super) fn reject_removed_args(args: &Value, removed: &[&str]) -> Result<()> {
    for key in removed {
        if args.get(*key).is_some_and(|value| !value.is_null()) {
            return Err(anyhow!(
                "{key} is not accepted by MCP tools; use the remuda CLI"
            ));
        }
    }
    Ok(())
}

pub(super) fn string_list(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(Value::String(s)) => s
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

pub(super) fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    opt_str(args, key).ok_or_else(|| anyhow!("{key} is required"))
}

pub(super) fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}
