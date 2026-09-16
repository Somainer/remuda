//! The launching user's own Claude settings, carried into a scoped native home.
//!
//! When the native carrier pins `CLAUDE_CONFIG_DIR` at a Remuda-managed home
//! (a fresh per-instance directory, or the Node-wide `REMUDA_CLAUDE_CONFIG_DIR`),
//! the harness's normal user-settings layer points at an empty directory.
//! Without the merge in this module every user-level key — `env` (the gateway
//! base URL/token/models), `model`, `modelSettings`, `statusLine`,
//! `enabledPlugins`, `theme`, `permissions`, … — silently disappears, which is
//! the macOS native-carrier regression this fixes (native-config-1,
//! 2026-09-16).
//!
//! ## Precedence, low to high
//!
//! ```text
//! <user home>/settings.json
//!   < user home>/settings.local.json        (Claude's user-level precedence)
//!   < launch-request explicit --settings>   (the operator/caller overlay)
//!   < Remuda hooks + forced terminal keys > (materialised by `overlay.rs`)
//! ```
//!
//! The merged document is written exactly once, to the 0600 per-instance
//! `<instance dir>/launch/settings.json`, and handed to the harness with
//! `--settings`. Nothing here ever writes to the user's own directory.
//!
//! When the child inherits the operator's real config (no native-home pin), or
//! is pinned at a directory the caller explicitly chose, the harness reads
//! those files itself and the caller must NOT ask for this merge: copying
//! hooks in as well would run them twice. The Node decides which case a launch
//! is in and passes the user home only for the scoped-home case.
//!
//! ## Credentials
//!
//! Gateway credentials reach the overlay only by being copied out of the
//! launching user's own settings files on this host. Remuda never invents,
//! substitutes or injects provider credentials into the document. Values under
//! `env` stay in the 0600 instance file; they are never journaled, logged (the
//! one debug log this path emits goes through [`redact_settings`]) or shown in
//! the web — the launch audit records the file digest and env *names* only.

use crate::error::{DriverError, DriverResult};
use serde_json::{Value, json};
use std::path::Path;

/// File names read, in Claude's user-level precedence order.
const SHARED_SETTINGS: &str = "settings.json";
const LOCAL_SETTINGS: &str = "settings.local.json";

/// Load the launching user's effective user-level settings from `config_dir`.
///
/// `settings.json` is merged first and `settings.local.json` overlays it with
/// [`merge_settings_layers`]. Returns `Ok(None)` when neither file exists. A
/// present-but-unreadable or malformed file is an error, not a silent skip:
/// proceeding without it could send traffic to the wrong endpoint with no
/// credential while looking like a normal launch.
pub fn load_effective_user_settings(config_dir: &Path) -> DriverResult<Option<Value>> {
    let shared = read_settings_object(&config_dir.join(SHARED_SETTINGS))?;
    let local = read_settings_object(&config_dir.join(LOCAL_SETTINGS))?;
    Ok(match (shared, local) {
        (None, None) => None,
        (Some(shared), None) => Some(shared),
        (None, Some(local)) => Some(local),
        (Some(shared), Some(local)) => Some(merge_settings_layers(&shared, &local)),
    })
}

/// Read one settings file. `Ok(None)` means the file does not exist.
fn read_settings_object(path: &Path) -> DriverResult<Option<Value>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        DriverError::SettingsIsolationUnavailable(format!(
            "{} is not valid settings JSON: {error}",
            path.display()
        ))
    })?;
    if !value.is_object() {
        return Err(DriverError::SettingsIsolationUnavailable(format!(
            "{} must contain a JSON settings object",
            path.display()
        )));
    }
    Ok(Some(value))
}

/// Merge two settings layers in Claude's precedence order: `lower` first,
/// `upper` wins.
///
/// - Objects merge key by key, recursively.
/// - `hooks` merge **per event**: `upper` matchers are appended after `lower`
///   matchers, with exact duplicates removed, so hooks registered at both
///   layers both run (verified against claude 2.1.270).
/// - Other arrays and scalars are replaced by `upper`, matching the documented
///   "highest precedence wins" behaviour.
#[must_use]
pub fn merge_settings_layers(lower: &Value, upper: &Value) -> Value {
    let mut merged = lower.clone();
    merge_into(&mut merged, upper);
    merged
}

/// Merge `upper` into `lower` in place using the rules of
/// [`merge_settings_layers`].
pub fn merge_into(lower: &mut Value, upper: &Value) {
    match (&mut *lower, upper) {
        (Value::Object(lower_map), Value::Object(upper_map)) => {
            for (key, upper_value) in upper_map {
                if key == "hooks" {
                    merge_hooks_block(lower_map, upper_value);
                    continue;
                }
                match lower_map.get_mut(key) {
                    Some(existing) if existing.is_object() && upper_value.is_object() => {
                        merge_into(existing, upper_value);
                    }
                    _ => {
                        lower_map.insert(key.clone(), upper_value.clone());
                    }
                }
            }
        }
        _ => *lower = upper.clone(),
    }
}

/// Merge a `hooks` object per event, appending matchers with dedup.
fn merge_hooks_block(lower_map: &mut serde_json::Map<String, Value>, upper_hooks: &Value) {
    let Some(upper_events) = upper_hooks.as_object() else {
        lower_map.insert("hooks".into(), upper_hooks.clone());
        return;
    };
    let hooks = lower_map
        .entry("hooks".to_owned())
        .or_insert_with(|| json!({}));
    let Some(events) = hooks.as_object_mut() else {
        *hooks = upper_hooks.clone();
        return;
    };
    for (event, upper_value) in upper_events {
        let Some(upper_matchers) = upper_value.as_array() else {
            events.insert(event.clone(), upper_value.clone());
            continue;
        };
        match events
            .entry(event.clone())
            .or_insert_with(|| json!([]))
            .as_array_mut()
        {
            Some(matchers) => {
                for matcher in upper_matchers {
                    if !matchers.contains(matcher) {
                        matchers.push(matcher.clone());
                    }
                }
            }
            None => {
                events.insert(event.clone(), upper_value.clone());
            }
        }
    }
}

/// Merge an explicit caller `--settings` document over the generated overlay.
///
/// Same precedence rules as [`merge_into`] (objects merge recursively, other
/// values are replaced by `upper`), except `hooks`: per event the explicit
/// matchers lead and the generated matchers absent from the explicit document
/// are appended, so a user/operator hook keeps its position *before* Remuda's
/// relay registration while the relay is never dropped. Exact duplicate
/// matchers (the re-passed generated file on a renderer relaunch) collapse.
pub fn merge_explicit_on_top(base: &mut Value, upper: &Value) {
    let (Some(base_map), Some(upper_map)) = (base.as_object_mut(), upper.as_object()) else {
        *base = upper.clone();
        return;
    };
    for (key, upper_value) in upper_map {
        if key == "hooks" {
            merge_explicit_hooks(base_map, upper_value);
            continue;
        }
        match base_map.get_mut(key) {
            Some(existing) if existing.is_object() && upper_value.is_object() => {
                merge_explicit_on_top(existing, upper_value);
            }
            _ => {
                base_map.insert(key.clone(), upper_value.clone());
            }
        }
    }
}

/// Hooks union for [`merge_explicit_on_top`]: explicit matchers first.
fn merge_explicit_hooks(base_map: &mut serde_json::Map<String, Value>, upper_hooks: &Value) {
    let Some(upper_events) = upper_hooks.as_object() else {
        base_map.insert("hooks".into(), upper_hooks.clone());
        return;
    };
    let hooks = base_map
        .entry("hooks".to_owned())
        .or_insert_with(|| json!({}));
    let Some(events) = hooks.as_object_mut() else {
        *hooks = upper_hooks.clone();
        return;
    };
    for (event, upper_value) in upper_events {
        let Some(upper_matchers) = upper_value.as_array() else {
            events.insert(event.clone(), upper_value.clone());
            continue;
        };
        let mut union: Vec<Value> = upper_matchers.clone();
        if let Some(Some(existing)) = events.get(event).map(Value::as_array) {
            for matcher in existing {
                if !union.contains(matcher) {
                    union.push(matcher.clone());
                }
            }
        }
        events.insert(event.clone(), Value::Array(union));
    }
}

/// Mask credential-bearing values out of a settings document for safe logging.
///
/// Any scalar (or array/object subtree) whose key names a credential is
/// replaced with `"[redacted]"`: names containing `TOKEN`, `API_KEY`,
/// `SECRET`, `PASSWORD` or `CREDENTIAL` (case-insensitive), which covers
/// `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, provider keys and helper
/// configs. Non-secret values — model ids, theme, statusline, gateway env
/// names without credentials — stay visible for diagnostics.
#[must_use]
pub fn redact_settings(value: &Value) -> Value {
    let mut redacted = value.clone();
    redact_in_place(&mut redacted);
    redacted
}

fn redact_in_place(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if is_credential_name(key) {
                    *child = json!("[redacted]");
                } else {
                    redact_in_place(child);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_in_place),
        _ => {}
    }
}

/// Whether a settings key's value must never leave the instance file.
fn is_credential_name(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    [
        "TOKEN",
        "API_KEY",
        "APIKEY",
        "SECRET",
        "PASSWORD",
        "CREDENTIAL",
    ]
    .iter()
    .any(|marker| upper.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, name: &str, body: &str) {
        fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn settings_json_then_settings_local_json_merge_in_claudes_order() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            SHARED_SETTINGS,
            r#"{
                "model": "shared-model",
                "theme": "dark",
                "env": {"ANTHROPIC_MODEL": "shared-gateway", "KEEP": "yes"},
                "permissions": {"allow": ["Read"]}
            }"#,
        );
        write(
            dir.path(),
            LOCAL_SETTINGS,
            r#"{
                "model": "local-model",
                "env": {"ANTHROPIC_MODEL": "local-gateway", "EXTRA": "x"},
                "verbose": true
            }"#,
        );
        let merged = load_effective_user_settings(dir.path()).unwrap().unwrap();
        assert_eq!(merged["model"], "local-model", "local wins scalars");
        assert_eq!(merged["theme"], "dark", "untouched shared keys survive");
        assert_eq!(merged["verbose"], true);
        assert_eq!(merged["env"]["ANTHROPIC_MODEL"], "local-gateway");
        assert_eq!(merged["env"]["KEEP"], "yes", "env merges per variable");
        assert_eq!(merged["env"]["EXTRA"], "x");
        assert_eq!(merged["permissions"]["allow"][0], "Read");
    }

    #[test]
    fn a_single_or_missing_settings_file_is_handled() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_effective_user_settings(dir.path()).unwrap().is_none());
        write(dir.path(), SHARED_SETTINGS, r#"{"model": "m"}"#);
        let merged = load_effective_user_settings(dir.path()).unwrap().unwrap();
        assert_eq!(merged["model"], "m");

        let only_local = tempfile::tempdir().unwrap();
        write(only_local.path(), LOCAL_SETTINGS, r#"{"verbose": true}"#);
        let merged = load_effective_user_settings(only_local.path())
            .unwrap()
            .unwrap();
        assert_eq!(merged["verbose"], true);
    }

    #[test]
    fn a_malformed_settings_file_fails_the_load_rather_than_dropping_it() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), SHARED_SETTINGS, r#"{"env": "#);
        assert!(load_effective_user_settings(dir.path()).is_err());
        write(dir.path(), SHARED_SETTINGS, r#"[1, 2]"#);
        assert!(load_effective_user_settings(dir.path()).is_err());
    }

    #[test]
    fn hooks_from_both_layers_run_and_exact_duplicates_do_not_stack() {
        let lower = json!({
            "hooks": {"Stop": [{"matcher": "a", "hooks": [{"command": "one"}]}]},
            "model": "lower"
        });
        let upper = json!({
            "hooks": {
                "Stop": [
                    {"matcher": "a", "hooks": [{"command": "one"}]},
                    {"matcher": "b", "hooks": [{"command": "two"}]}
                ],
                "SessionEnd": [{"hooks": [{"command": "three"}]}]
            }
        });
        let merged = merge_settings_layers(&lower, &upper);
        let stop = merged["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "the duplicate matcher is appended only once");
        assert_eq!(stop[0]["hooks"][0]["command"], "one");
        assert_eq!(stop[1]["hooks"][0]["command"], "two");
        assert_eq!(
            merged["hooks"]["SessionEnd"][0]["hooks"][0]["command"],
            "three"
        );
        assert_eq!(merged["model"], "lower");
    }

    #[test]
    fn non_hook_arrays_are_replaced_not_concatenated() {
        let lower = json!({"enabledPlugins": {"a": true}, "modelSettings": ["m1"]});
        let upper = json!({"modelSettings": ["m2", "m3"]});
        let merged = merge_settings_layers(&lower, &upper);
        assert_eq!(
            merged["modelSettings"],
            json!(["m2", "m3"]),
            "higher precedence replaces a plain array"
        );
        assert_eq!(merged["enabledPlugins"]["a"], true);
    }

    #[test]
    fn redaction_masks_every_credential_value_and_keeps_the_rest() {
        let settings = json!({
            "model": "ark/gateway-model",
            "env": {
                "ANTHROPIC_BASE_URL": "https://gateway.example.invalid",
                "ANTHROPIC_AUTH_TOKEN": "super-secret-token-value",
                "ANTHROPIC_API_KEY": "sk-ant-0123456789",
                "ANTHROPIC_MODEL": "ark/gateway-model",
                "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY": "1"
            },
            "statusLine": {"type": "command", "command": "echo ready"},
            "nested": {"apiKeyHelper": "echo super-secret-token-value"}
        });
        let redacted = redact_settings(&settings).to_string();
        assert!(redacted.contains("[redacted]"));
        assert!(!redacted.contains("super-secret-token-value"), "{redacted}");
        assert!(!redacted.contains("sk-ant-0123456789"), "{redacted}");
        // Non-secret configuration stays diagnostically useful.
        assert!(redacted.contains("ark/gateway-model"), "{redacted}");
        assert!(
            redacted.contains("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"),
            "{redacted}"
        );
        assert!(redacted.contains("echo ready"), "{redacted}");
    }
}
