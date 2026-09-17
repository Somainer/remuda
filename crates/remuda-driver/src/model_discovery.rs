//! Discovery of the model list a running Claude session can actually switch
//! to. The picker must show the gateway's discovered models, not a static
//! guess.
//!
//! Sources, measured on a gateway-configured host (2.1.272), highest fidelity
//! first:
//!
//! 1. **gateway discovery** — `<config dir>/cache/gateway-models.json`,
//!    written by Claude Code when `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY`
//!    is set. Shape: `{ baseUrl, fetchedAt, models: [{id, display_name,
//!    description}] }`. A scoped `CLAUDE_CONFIG_DIR` may start without the
//!    cache (discovery lands on first network refresh), so the host user's
//!    conventional `~/.claude/cache/gateway-models.json` is consulted next —
//!    the same relay serves both.
//! 2. **settings/env** — `settings.json` `model` / `modelSettings` keys plus
//!    the launch environment's `ANTHROPIC_MODEL` and
//!    `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL`.
//! 3. **builtin** — the alias set the `/model` command always knows.

use remuda_protocol::{ModelCatalogInfo, ModelListSource, Timestamp, parse_gateway_models_json};

/// Aliases Claude Code's `/model` resolves without any discovery. Kept in the
/// protocol vocabulary so the web fallback and the driver agree.
pub(crate) const BUILTIN_MODEL_ALIASES: &[&str] = &["opus", "sonnet", "haiku", "auto"];

/// One configured `(name, value)` setting from the session's settings.json.
pub(crate) trait SettingsView {
    /// Top-level `model` pin, when set.
    fn model(&self) -> Option<&str>;
    /// Keys of `modelSettings` (per-model overrides carry no usable list
    /// entries beyond their key, which is a model id).
    fn model_settings_keys(&self) -> Vec<String>;
    /// `env.ANTHROPIC_*MODEL` values configured in settings.
    fn env_models(&self) -> Vec<String>;
}

/// Settings.json parsed view (best effort; an unreadable/missing file is
/// simply empty).
struct JsonSettings {
    model: Option<String>,
    model_settings_keys: Vec<String>,
    env_models: Vec<String>,
}

impl SettingsView for JsonSettings {
    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
    fn model_settings_keys(&self) -> Vec<String> {
        self.model_settings_keys.clone()
    }
    fn env_models(&self) -> Vec<String> {
        self.env_models.clone()
    }
}

fn read_settings(path: &std::path::Path) -> JsonSettings {
    let empty = JsonSettings {
        model: None,
        model_settings_keys: Vec::new(),
        env_models: Vec::new(),
    };
    let Ok(bytes) = std::fs::read(path) else {
        return empty;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return empty;
    };
    let model = value
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let model_settings_keys = value
        .get("modelSettings")
        .and_then(|v| v.as_object())
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default();
    let env_names = [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    ];
    let env_models = value
        .get("env")
        .and_then(|v| v.as_object())
        .map(|env| {
            env_names
                .iter()
                .filter_map(|name| env.get(*name).and_then(|v| v.as_str()))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    JsonSettings {
        model,
        model_settings_keys,
        env_models,
    }
}

fn dedupe(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out = Vec::new();
    for item in items {
        let item = item.trim().to_owned();
        if !item.is_empty() && !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

/// Resolve the session's switchable model list.
///
/// * `config_dir` — the session's `CLAUDE_CONFIG_DIR` (scoped on a Remuda
///   launch); its discovery cache is read first.
/// * `host_config_dir` — fallback conventional dir (the operator's
///   `~/.claude`) for the cache when the scoped dir has not been populated
///   yet.
/// * `settings_path` — settings.json to read (`config_dir/settings.json`
///   normally); `None` skips file settings.
/// * `env` — the process environment `(name, value)` pairs the child actually
///   launched with.
/// * `current` — the launch/current model, always kept in the list when
///   non-empty even if discovery omitted it.
pub(crate) fn resolve_catalog(
    config_dir: Option<&std::path::Path>,
    host_config_dir: Option<&std::path::Path>,
    settings_path: Option<&std::path::Path>,
    env: &[(&str, &str)],
    current: Option<&str>,
) -> ModelCatalogInfo {
    let observed_at = crate::claude_pty::now_ts().unwrap_or_else(|_| {
        Timestamp::try_from("1970-01-01T00:00:00.000Z".to_string()).expect("constant timestamp")
    });

    // 1. Gateway discovery cache.
    let mut cache_path = config_dir.map(|dir| dir.join("cache").join("gateway-models.json"));
    let gateway_ids = cache_path
        .as_deref()
        .map(std::fs::read_to_string)
        .and_then(Result::ok)
        .map(|body| parse_gateway_models_json(&body))
        .filter(|ids| !ids.is_empty())
        .or_else(|| {
            cache_path = host_config_dir.map(|dir| dir.join("cache").join("gateway-models.json"));
            cache_path
                .as_deref()
                .map(std::fs::read_to_string)
                .and_then(Result::ok)
                .map(|body| parse_gateway_models_json(&body))
                .filter(|ids| !ids.is_empty())
        });

    if let Some(ids) = gateway_ids {
        // The current/launch id always survives a stale cache.
        let models = dedupe(
            ids.into_iter()
                .chain(current.iter().map(|s| (*s).to_owned())),
        );
        return ModelCatalogInfo {
            models,
            source: ModelListSource::GatewayDiscovery,
            observed_at,
        };
    }

    // 2. Settings + environment.
    let file_settings = settings_path.map(read_settings);
    let mut settings_models: Vec<String> = Vec::new();
    if let Some(view) = file_settings.as_ref().map(|s| s as &dyn SettingsView) {
        if let Some(model) = view.model() {
            settings_models.push(model.to_owned());
        }
        settings_models.extend(view.model_settings_keys());
        settings_models.extend(view.env_models());
    }
    let env_names = [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    ];
    for (name, value) in env {
        if env_names.contains(name) {
            settings_models.push((*value).to_owned());
        }
    }
    if let Some(current) = current {
        settings_models.push(current.to_owned());
    }
    let settings_models = dedupe(settings_models);
    if !settings_models.is_empty() {
        return ModelCatalogInfo {
            models: settings_models,
            source: ModelListSource::Settings,
            observed_at,
        };
    }

    // 3. Builtin aliases.
    ModelCatalogInfo {
        models: BUILTIN_MODEL_ALIASES
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
        source: ModelListSource::Builtin,
        observed_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "remuda-c-modelsync-discovery-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn gateway_cache_wins_and_keeps_current() {
        let dir = tmp("gateway");
        let cache = dir.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(
            cache.join("gateway-models.json"),
            serde_json::json!({
                "models": [
                    {"id": "ark/a", "display_name": "A"},
                    {"id": "model_hub/b"}
                ]
            })
            .to_string(),
        )
        .unwrap();
        let info = resolve_catalog(Some(&dir), None, None, &[], Some("opus"));
        assert_eq!(info.source, ModelListSource::GatewayDiscovery);
        assert_eq!(info.models, vec!["ark/a", "model_hub/b", "opus"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_scoped_cache_falls_back_to_host_cache() {
        let scoped = tmp("scoped");
        std::fs::create_dir_all(scoped.join("cache")).unwrap();
        let host = tmp("host");
        std::fs::create_dir_all(host.join("cache")).unwrap();
        std::fs::write(
            host.join("cache/gateway-models.json"),
            serde_json::json!({"models": [{"id": "host/x"}]}).to_string(),
        )
        .unwrap();
        let info = resolve_catalog(Some(&scoped), Some(&host), None, &[], None);
        assert_eq!(info.source, ModelListSource::GatewayDiscovery);
        assert_eq!(info.models, vec!["host/x"]);
        let _ = std::fs::remove_dir_all(&scoped);
        let _ = std::fs::remove_dir_all(&host);
    }

    #[test]
    fn settings_and_env_resolve_without_discovery() {
        let dir = tmp("settings");
        std::fs::write(
            dir.join("settings.json"),
            serde_json::json!({
                "model": "ark/seed-evolving[1m]",
                "modelSettings": {"model_hub/es1_orange_o48": {"effortLevel": "medium"}},
                "env": {"ANTHROPIC_MODEL": "model_hub/es1_orange_o48[1m]"}
            })
            .to_string(),
        )
        .unwrap();
        let info = resolve_catalog(
            Some(&dir),
            None,
            Some(&dir.join("settings.json")),
            &[
                ("ANTHROPIC_DEFAULT_HAIKU_MODEL", "model_hub/h"),
                ("PATH", "/bin"),
            ],
            None,
        );
        assert_eq!(info.source, ModelListSource::Settings);
        assert!(info.models.contains(&"ark/seed-evolving[1m]".to_string()));
        assert!(
            info.models
                .contains(&"model_hub/es1_orange_o48".to_string())
        );
        assert!(
            info.models
                .contains(&"model_hub/es1_orange_o48[1m]".to_string())
        );
        assert!(info.models.contains(&"model_hub/h".to_string()));
        assert!(!info.models.iter().any(|m| m == "/bin"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_known_falls_back_to_builtin() {
        let info = resolve_catalog(None, None, None, &[], None);
        assert_eq!(info.source, ModelListSource::Builtin);
        assert_eq!(
            info.models,
            BUILTIN_MODEL_ALIASES
                .iter()
                .map(|s| (*s).to_owned())
                .collect::<Vec<_>>()
        );
    }
}
