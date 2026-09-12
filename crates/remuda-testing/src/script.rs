//! JSONL playback scripts for `fake-claude`.

use crate::paths::{ScriptKind, script_kind_from_name, script_source};
use serde_json::Value;
use std::path::Path;

/// One script line: either an emitted NDJSON frame or a host wait.
#[derive(Clone, Debug)]
pub enum ScriptStep {
    /// Write this object to stdout (after placeholder rewrite).
    Emit(Value),
    /// Block until a `control_response` with this `request_id` arrives.
    ExpectControlResponse {
        /// Host `control_response.response.request_id`.
        request_id: String,
    },
}

/// Load a script from `FAKE_CLAUDE_SCRIPT` (path or `ok`/`approval`/`askuser`/`workflow`).
pub fn load_script_from_env() -> Result<Vec<ScriptStep>, String> {
    match std::env::var("FAKE_CLAUDE_SCRIPT") {
        Ok(value) if !value.is_empty() => load_script(&value),
        _ => parse_script_text(script_source(ScriptKind::Ok)),
    }
}

/// Load by bundled name or filesystem path.
pub fn load_script(spec: &str) -> Result<Vec<ScriptStep>, String> {
    if let Some(kind) = script_kind_from_name(spec) {
        return parse_script_text(script_source(kind));
    }
    let path = Path::new(spec);
    let text = std::fs::read_to_string(path).map_err(|err| format!("{spec}: {err}"))?;
    parse_script_text(&text)
}

/// Parse JSONL into steps. Blank lines are skipped.
pub fn parse_script_text(text: &str) -> Result<Vec<ScriptStep>, String> {
    let mut steps = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value =
            serde_json::from_str(line).map_err(|err| format!("script line {}: {err}", idx + 1))?;
        if value.get("expect").and_then(Value::as_str) == Some("control_response") {
            let request_id = value
                .get("request_id")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("script line {}: expect missing request_id", idx + 1))?
                .to_string();
            steps.push(ScriptStep::ExpectControlResponse { request_id });
            continue;
        }
        steps.push(ScriptStep::Emit(value));
    }
    Ok(steps)
}

/// `when` filter on an emit frame (`allow` / `deny`), if any.
pub fn when_filter(value: &Value) -> Option<&str> {
    value.get("when").and_then(Value::as_str)
}

/// Strip helper keys that are not part of the Claude wire.
pub fn strip_helper_keys(mut value: Value) -> Value {
    if let Some(obj) = value.as_object_mut() {
        obj.remove("when");
        obj.remove("expect");
    }
    value
}
