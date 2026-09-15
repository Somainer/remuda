//! Claude settings precedence exercised by isolated fake-harness fixtures.
//!
//! This models the scalar and additive-hook contract, not Claude's full config
//! loader. The real precedence is verified separately by native evidence.
//! `home/managed-settings.json` is a fixture-only stand-in for managed policy;
//! no system-managed or ambient user configuration is read.

use std::path::Path;

use serde_json::{Value, json};

/// Resolve user < project < local < CLI overlay < managed fixture settings.
/// Missing optional files contribute nothing; malformed files fail closed.
pub fn merged_claude_settings(
    home: &Path,
    cwd: &Path,
    overlay: Option<&Path>,
) -> Result<Value, String> {
    let mut merged = json!({});
    let mut paths = vec![
        home.join("settings.json"),
        cwd.join(".claude/settings.json"),
        cwd.join(".claude/settings.local.json"),
    ];
    paths.extend(overlay.map(Path::to_path_buf));
    paths.push(home.join("managed-settings.json"));
    for path in paths {
        if !path.is_file() {
            continue;
        }
        let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        let settings: Value = serde_json::from_slice(&bytes)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if !settings.is_object() {
            return Err(format!("{}: settings must be an object", path.display()));
        }
        merge(&mut merged, settings);
    }
    Ok(merged)
}

fn merge(target: &mut Value, incoming: Value) {
    match (target, incoming) {
        (Value::Object(target), Value::Object(incoming)) => {
            for (key, value) in incoming {
                merge(target.entry(key).or_insert(Value::Null), value);
            }
        }
        (Value::Array(target), Value::Array(incoming)) => {
            for value in incoming {
                if !target.contains(&value) {
                    target.push(value);
                }
            }
        }
        (target, incoming) => *target = incoming,
    }
}
