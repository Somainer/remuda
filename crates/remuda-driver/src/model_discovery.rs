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

use remuda_protocol::{
    EffectiveModel, EffortSource, ModelCacheInfo, ModelCacheScope, ModelCatalogInfo,
    ModelListSource, ModelPayload, ModelSelectionPath, ObservationPayload, Timestamp,
    parse_gateway_models_json,
};
use std::sync::Arc;

/// Aliases Claude Code's `/model` resolves without any discovery. Kept in the
/// protocol vocabulary so the web fallback and the driver agree.
pub(crate) const BUILTIN_MODEL_ALIASES: &[&str] = &["opus", "sonnet", "haiku", "auto"];

/// Env var Claude Code gates gateway model discovery behind.
const DISCOVERY_ENV: &str = "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY";

/// A parsed `cache/gateway-models.json` document: the id list plus the relay
/// metadata the file recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GatewayCache {
    ids: Vec<String>,
    base_url: Option<String>,
    fetched_at: Option<String>,
}

impl GatewayCache {
    fn non_empty(self) -> Option<Self> {
        (!self.ids.is_empty()).then_some(self)
    }
}

/// Parse a gateway-models cache document, also keeping `baseUrl` and
/// `fetchedAt` (ignored by the id-only protocol parser).
pub(crate) fn parse_gateway_cache(body: &str) -> GatewayCache {
    let ids = parse_gateway_models_json(body);
    let (base_url, fetched_at) = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .map(|value| {
            let string_field = |key: &str| {
                value
                    .get(key)
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            };
            (string_field("baseUrl"), string_field("fetchedAt"))
        })
        .unwrap_or((None, None));
    GatewayCache {
        ids,
        base_url,
        fetched_at,
    }
}

/// Read and parse one cache file, if non-empty.
fn read_gateway_cache(path: &std::path::Path) -> Option<GatewayCache> {
    std::fs::read_to_string(path)
        .ok()
        .map(|body| parse_gateway_cache(&body))
        .and_then(GatewayCache::non_empty)
}

/// Whether the discovery gate env var was present (and non-empty) in the
/// environment the child launched with.
fn discovery_env_present(env: &[(&str, &str)]) -> bool {
    env.iter()
        .any(|(name, value)| *name == DISCOVERY_ENV && !value.trim().is_empty())
}

/// The ids the session's *own* CLI accepts per a resolved catalog: the
/// scoped cache, settings, or builtin answer. A host-fallback answer yields
/// `None` — those ids came from the operator user's relay view, which the
/// scoped session may reject.
pub(crate) fn own_ids(catalog: &ModelCatalogInfo) -> Option<Vec<String>> {
    let host_fallback = catalog
        .cache
        .as_ref()
        .is_some_and(|cache| matches!(cache.scope, ModelCacheScope::HostFallback));
    (!host_fallback).then(|| catalog.models.clone())
}

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
    let discovery_env = Some(discovery_env_present(env));

    // 1. Gateway discovery cache: the session's own scoped dir first, the
    //    host user's conventional dir as a fallback. The scope is recorded so
    //    the UI can flag a fallback list the session's own CLI may reject.
    let scoped_path = config_dir.map(|dir| dir.join("cache").join("gateway-models.json"));
    let host_path = host_config_dir.map(|dir| dir.join("cache").join("gateway-models.json"));
    let gateway = scoped_path
        .as_deref()
        .and_then(read_gateway_cache)
        .map(|cache| (ModelCacheScope::ScopedConfigDir, cache))
        .or_else(|| {
            host_path
                .as_deref()
                .and_then(read_gateway_cache)
                .map(|cache| (ModelCacheScope::HostFallback, cache))
        });

    if let Some((scope, cache)) = gateway {
        // The current/launch id always survives a stale cache.
        let models = dedupe(
            cache
                .ids
                .into_iter()
                .chain(current.iter().map(|s| (*s).to_owned())),
        );
        return ModelCatalogInfo {
            models,
            source: ModelListSource::GatewayDiscovery,
            observed_at,
            cache: Some(ModelCacheInfo {
                scope,
                base_url: cache.base_url,
                fetched_at: cache.fetched_at,
            }),
            discovery_env,
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
            cache: None,
            discovery_env,
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
        cache: None,
        discovery_env,
    }
}

/// Poll briefly for the session's *own* scoped gateway cache, which the CLI
/// writes a beat after the first launch (measured: the promotion-time
/// resolution often only sees the host fallback). Returns the refreshed
/// catalog once the scoped cache answers a different list; `None` if the
/// promotion-time answer was already scoped or the cache never lands within
/// the bounded window.
pub(crate) async fn await_scoped_catalog(
    config_dir: std::path::PathBuf,
    host_config_dir: Option<std::path::PathBuf>,
    settings_path: Option<std::path::PathBuf>,
    env: Vec<(String, String)>,
    current: Option<String>,
    initial: &ModelCatalogInfo,
) -> Option<ModelCatalogInfo> {
    if initial
        .cache
        .as_ref()
        .is_some_and(|cache| matches!(cache.scope, ModelCacheScope::ScopedConfigDir))
    {
        return None;
    }
    let env_ref: Vec<(&str, &str)> = env
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    // Measured landing time is seconds, not minutes; bound the wait so a
    // session that never discovers cannot pin a task for life.
    const INTERVAL_MS: u64 = 1_000;
    const MAX_ATTEMPTS: u32 = 60;
    for _ in 0..MAX_ATTEMPTS {
        tokio::time::sleep(std::time::Duration::from_millis(INTERVAL_MS)).await;
        let resolved = resolve_catalog(
            Some(&config_dir),
            host_config_dir.as_deref(),
            settings_path.as_deref(),
            &env_ref,
            current.as_deref(),
        );
        let scoped = resolved
            .cache
            .as_ref()
            .is_some_and(|cache| matches!(cache.scope, ModelCacheScope::ScopedConfigDir));
        if scoped && resolved.models != initial.models {
            return Some(resolved);
        }
        if scoped {
            // Same ids, but the provenance changed from host fallback to the
            // session's own — still worth re-stamping so the warning clears.
            return Some(resolved);
        }
    }
    None
}

/// Build the model observation payload that carries a refreshed catalog to
/// the picker. The effective model does not change — the last proven
/// effective id (launch seed or settled verdict) rides along, attributed
/// exactly as it was observed. `None` when no model is known yet.
pub(crate) fn refresh_payload(
    catalog: ModelCatalogInfo,
    last_effective: Option<(String, EffortSource, Option<ModelSelectionPath>)>,
) -> Option<ObservationPayload> {
    let (id, source, selection_path) = last_effective?;
    Some(ObservationPayload::Model(Box::new(ModelPayload {
        requested: None,
        effective: EffectiveModel {
            id,
            source,
            observed_at: crate::claude_pty::now_ts().ok()?,
        },
        raw: None,
        catalog: Some(catalog),
        selection_path,
    })))
}

/// Wait for the session's own scoped gateway cache, refresh the bridge's
/// catalog membership, and build the catalog-only model observation payload.
/// Returns `None` when the promotion-time answer already was the scoped
/// cache or it never landed within the bounded window.
pub(crate) async fn scoped_refresh_payload(
    config_dir: std::path::PathBuf,
    host_config_dir: Option<std::path::PathBuf>,
    settings_path: Option<std::path::PathBuf>,
    env: Vec<(String, String)>,
    current: Option<String>,
    initial: ModelCatalogInfo,
    bridge: Arc<crate::model::ModelBridge>,
) -> Option<ObservationPayload> {
    let fresh = await_scoped_catalog(
        config_dir,
        host_config_dir,
        settings_path,
        env,
        current,
        &initial,
    )
    .await?;
    // A catalog-only edge must not masquerade as the verdict of a switch the
    // user started concurrently: wait briefly for the in-flight /model to
    // settle before emitting the refresh.
    for _ in 0..60 {
        if bridge.pending().is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    bridge.set_own_catalog(own_ids(&fresh));
    refresh_payload(fresh, bridge.last_effective())
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
                "baseUrl": "https://relay.example.invalid/v1",
                "fetchedAt": "2026-09-18T10:00:00.000Z",
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
        let cache = info.cache.expect("cache provenance recorded");
        assert_eq!(cache.scope, ModelCacheScope::ScopedConfigDir);
        assert_eq!(
            cache.base_url.as_deref(),
            Some("https://relay.example.invalid/v1")
        );
        assert_eq!(
            cache.fetched_at.as_deref(),
            Some("2026-09-18T10:00:00.000Z")
        );
        assert_eq!(info.discovery_env, Some(false));
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
            serde_json::json!({
                "baseUrl": "https://relay.example.invalid/v1",
                "models": [{"id": "host/x"}]
            })
            .to_string(),
        )
        .unwrap();
        let info = resolve_catalog(
            Some(&scoped),
            Some(&host),
            None,
            &[("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1")],
            None,
        );
        assert_eq!(info.source, ModelListSource::GatewayDiscovery);
        assert_eq!(info.models, vec!["host/x"]);
        assert_eq!(info.discovery_env, Some(true));
        let cache = info.cache.as_ref().expect("cache provenance recorded");
        assert_eq!(cache.scope, ModelCacheScope::HostFallback);
        assert_eq!(
            cache.base_url.as_deref(),
            Some("https://relay.example.invalid/v1")
        );
        assert_eq!(crate::model_discovery::own_ids(&info), None);
        let _ = std::fs::remove_dir_all(&scoped);
        let _ = std::fs::remove_dir_all(&host);
    }

    #[test]
    fn own_ids_follow_the_scoped_cache() {
        let dir = tmp("own");
        std::fs::create_dir_all(dir.join("cache")).unwrap();
        std::fs::write(
            dir.join("cache/gateway-models.json"),
            serde_json::json!({"models": [{"id": "scoped/a"}]}).to_string(),
        )
        .unwrap();
        let info = resolve_catalog(Some(&dir), None, None, &[], None);
        assert_eq!(
            crate::model_discovery::own_ids(&info),
            Some(vec!["scoped/a".to_owned()])
        );
        let _ = std::fs::remove_dir_all(&dir);
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
