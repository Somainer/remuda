//! JSON argument helpers shared by tool groups.

use anyhow::{Result, anyhow};
use serde_json::Value;

pub(super) fn send_text_from_args(args: &Value) -> Result<String> {
    if let Some(path) = opt_str(args, "file") {
        return std::fs::read_to_string(path).map_err(|err| anyhow!("read {path}: {err}"));
    }
    args.get("text")
        .and_then(Value::as_str)
        .or_else(|| args.get("input").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("text or file is required"))
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
