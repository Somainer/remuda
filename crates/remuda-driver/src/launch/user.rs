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
//! ## Who wins on the provider (gateway-carryover-1, 2026-09-17)
//!
//! The host layer being *lowest* is not enough, because the two layers do not
//! describe the provider through the same keys. A host `env.ANTHROPIC_MODEL`
//! outranks the overlay's `model` key inside Claude itself, and a host
//! `env.ANTHROPIC_BASE_URL` survives an overlay that only sets `model` — so a
//! plain low-to-high merge still let the host redirect a session the operator
//! had pointed at a Hub gateway.
//!
//! So for `gateway` and `direct` delegation the overlay is **authoritative**,
//! not merely higher: [`merge_provider_overlay_over_user`] strips the host's
//! endpoint, credential and model variables ([`is_overridden_provider_env`])
//! plus its top-level `model` before merging. Everything else the host
//! configured — hooks, `permissions`, `theme`, `statusLine`, effort, custom
//! keys, unrelated `env` — is untouched, which is the whole point of keeping
//! the host layer as the base.
//!
//! Delegation `none` (跟随主机 / native login) is unchanged: there the host's
//! settings *are* the requested provider, and they carry over verbatim.
//!
//! ## Credentials
//!
//! Gateway credentials reach the overlay only by being copied out of the
//! launching user's own settings files on this host. Remuda never invents,
//! substitutes or injects provider credentials into the document. Values under
//! `env` stay in the 0600 instance file; they are never journaled, logged (the
//! one debug log this path emits goes through [`redact_settings`]) or shown in
//! the web — the launch audit records the file digest and env *names* only.

use crate::error::DriverResult;
use serde_json::{Value, json};
use std::path::Path;

/// File names read, in Claude's user-level precedence order.
const SHARED_SETTINGS: &str = "settings.json";
const LOCAL_SETTINGS: &str = "settings.local.json";

/// Load the launching user's effective user-level settings from `config_dir`.
///
/// `settings.json` is merged first and `settings.local.json` overlays it with
/// [`merge_settings_layers`]. Returns `Ok(None)` when no usable settings are
/// available: no files, a present-but-unreadable file, or a malformed/non-object
/// document all degrade to Remuda-only overlay rather than failing the launch.
/// The overlay is additive to the harness configuration — a bad user file must
/// never turn instance creation into an error, so the degradation is logged
/// (with no file contents) and treated like an absent layer.
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

/// Read one settings file. `Ok(None)` means the file does not exist or could
/// not be used (unreadable, malformed JSON, non-object): the launch proceeds
/// without that layer instead of failing.
fn read_settings_object(path: &Path) -> DriverResult<Option<Value>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "user settings unreadable; launching without that layer"
            );
            return Ok(None);
        }
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) if value.is_object() => Ok(Some(value)),
        Ok(_) => {
            tracing::warn!(
                path = %path.display(),
                "user settings must be a JSON object; launching without that layer"
            );
            Ok(None)
        }
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "user settings are not valid JSON; launching without that layer"
            );
            Ok(None)
        }
    }
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

/// Provider-routing env variables the Hub's overlay is authoritative over.
///
/// When a `gateway`/`direct` overlay applies, every one of these is removed
/// from the host user's layer before the overlay is merged on top. Leaving any
/// of them would let the host redirect a session the Hub pointed elsewhere:
/// `ANTHROPIC_MODEL` and the `ANTHROPIC_DEFAULT_*_MODEL` trio outrank the
/// settings `model` key, `CLAUDE_CODE_SUBAGENT_MODEL` re-points subagents, and
/// `CLAUDE_CODE_MAX_CONTEXT_TOKENS` describes a window the Hub's model may not
/// have (gateway-carryover-1, 2026-09-17).
///
/// D-047 `via` delivery needs no new entries and no new code path: the relay
/// overlay is written as an ordinary `gateway` overlay whose `ANTHROPIC_BASE_URL`
/// is the per-instance loopback listener and whose `ANTHROPIC_AUTH_TOKEN` is the
/// minted relay bearer. Evicting the host's `ANTHROPIC_BASE_URL` /
/// `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` here is therefore also what
/// keeps a proxied session from silently falling back onto the host's own
/// gateway, and it happens exactly as it did before the relay existed.
const OVERRIDDEN_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "ANTHROPIC_VERTEX_BASE_URL",
    "AWS_BEARER_TOKEN_BEDROCK",
    "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
    "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
    "CLAUDE_CODE_SKIP_VERTEX_AUTH",
    "CLAUDE_CODE_SUBAGENT_MODEL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
];

/// Top-level settings keys the Hub's overlay is authoritative over.
///
/// The settings `model` key is one of the two channels a shell-pty launch has
/// for the requested model (the other is the `--model` argv its materializer
/// emits since model-pin-1), so a host `model` left in place silently
/// wins whenever neither carries one.
const OVERRIDDEN_KEYS: &[&str] = &["model"];

/// Env variables that *name a model*, as opposed to describing an endpoint or
/// carrying a credential.
///
/// These are the subset a model pin owns on its own (model-pin-1). Each
/// one outranks the settings `model` key in Claude Code, so leaving any of them
/// in the host layer lets the host answer for a model the requester pinned —
/// which is exactly how the 2026-09-18 demo ran every worker on the host's
/// default while every record claimed the pin.
///
/// Endpoint and credential variables are deliberately absent: under delegation
/// `none` the host still owns *where* the session talks and *as whom*. Only the
/// model changes hands.
const MODEL_ENV: &[&str] = &[
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "CLAUDE_CODE_SUBAGENT_MODEL",
];

/// Whether `name` names an env variable that selects a model.
#[must_use]
pub fn is_model_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    MODEL_ENV.contains(&upper.as_str())
}

/// Make an explicit model pin authoritative over the host user's own settings.
///
/// Belt-and-braces half of model-pin-1. Delegation `none` (跟随主机)
/// legitimately lets the host's settings describe the provider — but it must
/// stop meaning the host also owns the *model*. When the launch carries an
/// explicit pin, every host key that names a model is removed from the base
/// layer and the pin is written into `model`, so the one key Claude reads for a
/// model says what was asked for whichever way the argv went.
///
/// `pin` is written verbatim: a `[1m]` context suffix is part of the id.
///
/// The host's endpoint and credentials are untouched — see [`MODEL_ENV`]. Under
/// `gateway`/`direct`, [`merge_provider_overlay_over_user`] already takes the
/// whole provider and runs first; this only adds the model on top.
pub fn apply_model_pin(settings: &mut Value, pin: &str) {
    let pin = pin.trim();
    if pin.is_empty() {
        return;
    }
    if !settings.is_object() {
        *settings = Value::Object(serde_json::Map::new());
    }
    let Some(map) = settings.as_object_mut() else {
        return;
    };
    if let Some(env) = map.get_mut("env").and_then(Value::as_object_mut) {
        // Case-insensitively, for the same reason `strip_overridden` is: a host
        // that spelled the variable in lower case still exports it.
        let doomed = env
            .keys()
            .filter(|name| is_model_env(name))
            .cloned()
            .collect::<Vec<_>>();
        for name in doomed {
            env.remove(&name);
        }
    }
    map.insert("model".to_owned(), Value::String(pin.to_owned()));
}

/// Whether `name` names a provider endpoint/model variable the Hub owns.
#[must_use]
pub fn is_overridden_provider_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    OVERRIDDEN_ENV.contains(&upper.as_str())
}

/// Layer the Hub's provider overlay over the host user's own settings.
///
/// This is the three-way precedence the native carrier needs
/// (gateway-carryover-1). The host layer is the **base**, so hooks,
/// permissions, theme, `statusLine`, effort and every custom key keep working;
/// the Hub's `overlay` is **authoritative** on top for where the session talks
/// and which model answers.
///
/// Concretely, before the merge every [`OVERRIDDEN_ENV`] variable and every
/// [`OVERRIDDEN_KEYS`] entry is stripped from the host layer, so the host
/// cannot redirect a session the operator pointed at a gateway — not even
/// through a variable the overlay itself does not set. Anything the overlay
/// does not claim (the user's own `env` entries, their `permissions`, their
/// hooks) survives untouched.
///
/// Use it only for `gateway`/`direct` delegation. `none` (跟随主机 / native
/// login) is the case where the host's settings *are* the answer: pass them
/// through with [`merge_settings_layers`] instead.
///
/// A D-047 `via` launch is a `gateway` merge as far as this function knows:
/// the overlay's base URL is the worker's loopback relay listener and its
/// token is the per-instance relay bearer, so stripping [`OVERRIDDEN_ENV`]
/// leaves the session pointed at the listener and authenticated only by the
/// bearer — never the host's gateway credential and never the real gateway
/// origin, neither of which exists on the worker host.
#[must_use]
pub fn merge_provider_overlay_over_user(user: &Value, overlay: &Value) -> Value {
    let mut base = user.clone();
    strip_overridden(&mut base);
    merge_settings_layers(&base, overlay)
}

/// Remove the host's provider-routing keys from a settings layer in place.
fn strip_overridden(settings: &mut Value) {
    let Some(map) = settings.as_object_mut() else {
        return;
    };
    for key in OVERRIDDEN_KEYS {
        map.remove(*key);
    }
    if let Some(env) = map.get_mut("env").and_then(Value::as_object_mut) {
        // Case-insensitively: a host that spelled the variable in lower case
        // still exports it, so matching only the canonical spelling would let
        // it through.
        let doomed = env
            .keys()
            .filter(|name| is_overridden_provider_env(name))
            .cloned()
            .collect::<Vec<_>>();
        for name in doomed {
            env.remove(&name);
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
///
/// D-047 needs no special case: a `via` overlay carries its per-instance relay
/// bearer under the same `ANTHROPIC_AUTH_TOKEN` name, so a logged merged
/// document redacts the bearer the same way it redacted the gateway token.
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
    fn malformed_or_nonobject_settings_degrade_to_no_layer_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        // A broken shared file must never fail the launch: the caller falls
        // back to Remuda-only overlay and instance creation proceeds.
        write(dir.path(), SHARED_SETTINGS, r#"{"env": "#);
        assert_eq!(load_effective_user_settings(dir.path()).unwrap(), None);

        // A valid local file still overlays when shared is unusable.
        write(dir.path(), LOCAL_SETTINGS, r#"{"verbose": true}"#);
        assert_eq!(
            load_effective_user_settings(dir.path())
                .unwrap()
                .map(|v| v["verbose"].as_bool()),
            Some(Some(true))
        );

        // A non-object document degrades the same way.
        let dir2 = tempfile::tempdir().unwrap();
        write(dir2.path(), SHARED_SETTINGS, r#"[1, 2]"#);
        assert_eq!(load_effective_user_settings(dir2.path()).unwrap(), None);
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

    /// The launching user's real settings, shaped like the host in the
    /// gateway-carryover-1 report: their own gateway, their own model, and the
    /// model env trio that outranks a settings `model` key inside Claude.
    fn host_settings() -> Value {
        json!({
            "model": "ark/seed-evolving[1m]",
            "theme": "dark",
            "statusLine": {"type": "command", "command": "echo host"},
            "permissions": {"allow": ["Read", "Bash(git diff:*)"], "deny": ["Read(./.env)"]},
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "host-stop"}]}]},
            "enabledPlugins": {"host-plugin": true},
            "env": {
                "ANTHROPIC_BASE_URL": "https://host-native.example/api",
                "ANTHROPIC_AUTH_TOKEN": "host-token-placeholder",
                "ANTHROPIC_MODEL": "ark/seed-evolving[1m]",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "ark/host-opus",
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "ark/host-sonnet",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "ark/host-haiku",
                "CLAUDE_CODE_SUBAGENT_MODEL": "ark/host-subagent",
                "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "1000000",
                "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY": "1",
                "HOST_ONLY": "keep-me"
            }
        })
    }

    /// What the Hub delivers for delegation gateway (profile Doubao AI).
    fn hub_overlay() -> Value {
        json!({
            "model": "passthrough/ark/seed-evolving",
            "env": {
                "ANTHROPIC_BASE_URL": "https://gateway.example/v1",
                "ANTHROPIC_AUTH_TOKEN": "hub-token-placeholder",
                "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY": "1"
            }
        })
    }

    #[test]
    fn host_settings_alone_carry_over_untouched_for_native_delegation() {
        // 跟随主机 / 原生登录态: nothing is stripped, because here the host's
        // settings *are* the requested provider.
        let host = host_settings();
        let merged = merge_settings_layers(&json!({}), &host);
        assert_eq!(merged, host, "delegation none must not rewrite the host");
    }

    #[test]
    fn the_hub_overlay_wins_the_endpoint_token_and_model_over_the_host() {
        let merged = merge_provider_overlay_over_user(&host_settings(), &hub_overlay());

        // Where the session talks and who answers is the Hub's decision.
        assert_eq!(
            merged["env"]["ANTHROPIC_BASE_URL"], "https://gateway.example/v1",
            "the host's own gateway must not survive"
        );
        assert_eq!(
            merged["env"]["ANTHROPIC_AUTH_TOKEN"], "hub-token-placeholder",
            "the host's token must not be what authenticates"
        );
        assert_eq!(
            merged["model"], "passthrough/ark/seed-evolving",
            "the requested model wins; for shell-pty this key is the only channel"
        );
        assert_eq!(
            merged["env"]["CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"],
            "1"
        );
    }

    #[test]
    fn the_hosts_model_and_endpoint_variables_are_stripped_when_the_overlay_applies() {
        let merged = merge_provider_overlay_over_user(&host_settings(), &hub_overlay());
        let env = merged["env"].as_object().expect("env object");

        // Each of these outranks the overlay's `model` key or re-points the
        // session, so leaving any one of them lets the host redirect the run.
        for name in [
            "ANTHROPIC_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "CLAUDE_CODE_SUBAGENT_MODEL",
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
        ] {
            assert!(!env.contains_key(name), "{name} must be stripped");
        }
        // And no host value survives anywhere in the document.
        let rendered = merged.to_string();
        assert!(
            !rendered.contains("host-native.example"),
            "the host endpoint leaked: {rendered}"
        );
        assert!(
            !rendered.contains("ark/seed-evolving[1m]"),
            "the host model leaked: {rendered}"
        );
        assert!(!rendered.contains("ark/host-"), "a host model leaked");
    }

    #[test]
    fn an_overlay_less_gateway_launch_still_strips_the_host_provider_config() {
        // Delegation asked for a gateway but nothing was delivered. The host
        // must not answer for it, or this failure is indistinguishable from the
        // regression: strip, and let the harness's own login decide.
        let merged = merge_provider_overlay_over_user(&host_settings(), &json!({}));
        assert!(merged.get("model").is_none());
        let env = merged["env"].as_object().expect("env object");
        assert!(!env.contains_key("ANTHROPIC_BASE_URL"));
        assert!(!env.contains_key("ANTHROPIC_AUTH_TOKEN"));
        assert_eq!(env["HOST_ONLY"], "keep-me", "unrelated env still survives");
    }

    #[test]
    fn the_users_hooks_permissions_and_custom_settings_survive_the_overlay() {
        let merged = merge_provider_overlay_over_user(&host_settings(), &hub_overlay());

        // The whole reason the host layer is the base rather than discarded.
        assert_eq!(
            merged["hooks"]["Stop"][0]["hooks"][0]["command"], "host-stop",
            "the user's own hooks must keep firing"
        );
        assert_eq!(merged["permissions"]["allow"][0], "Read");
        assert_eq!(merged["permissions"]["allow"][1], "Bash(git diff:*)");
        assert_eq!(merged["permissions"]["deny"][0], "Read(./.env)");
        assert_eq!(merged["theme"], "dark");
        assert_eq!(merged["statusLine"]["command"], "echo host");
        assert_eq!(merged["enabledPlugins"]["host-plugin"], true);
        assert_eq!(
            merged["env"]["HOST_ONLY"], "keep-me",
            "only provider-routing variables are stripped, not the user's env"
        );
    }

    #[test]
    fn a_lowercase_or_mixed_case_host_variable_is_still_stripped() {
        // The env block is a map, and a host that spelled the variable in
        // lower case still exports it, so canonical-spelling-only matching
        // would let the redirect straight through.
        let host = json!({"env": {"anthropic_base_url": "https://host-native.example", "Anthropic_Model": "m"}});
        let merged = merge_provider_overlay_over_user(&host, &hub_overlay());
        let rendered = merged.to_string();
        assert!(!rendered.contains("host-native.example"), "{rendered}");
        assert!(is_overridden_provider_env("anthropic_base_url"));
        assert!(!is_overridden_provider_env("HOST_ONLY"));
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
