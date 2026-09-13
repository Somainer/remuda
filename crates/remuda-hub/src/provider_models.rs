//! Structured provider model catalog: the stored list, the legacy string
//! migration, and the gateway `/v1/models` normalizer shared by `/test` and
//! `/discover`. Nothing here ever touches an auth token.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Context window at or above this many tokens earns the `1m` tag.
const LONG_CONTEXT_TOKENS: u64 = 1_000_000;

/// The Anthropic-style listing (`anthropic-version` header).
pub const SURFACE_ANTHROPIC: &str = "anthropic";
/// The plain OpenAI-style listing (`Authorization: Bearer` only).
pub const SURFACE_OPENAI: &str = "openai";

/// One entry of a profile's model catalog.
///
/// `enabled` is what New Session may offer; a discovered-but-unchecked model
/// stays in the list so the operator sees it without exposing it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModel {
    /// Wire id passed through to the gateway verbatim.
    pub id: String,
    /// New Session may select this model.
    #[serde(default = "enabled_default")]
    pub enabled: bool,
    /// Human label reported by the gateway (`display_name` / `name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Context window in tokens when the gateway reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Short chips such as `1m`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Which gateway listings reported this id (`anthropic`, `openai`).
    ///
    /// A gateway may serve a different catalog per header, so the probe unions
    /// both and records where each id came from. Empty for a manually typed id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub surfaces: Vec<String>,
}

fn enabled_default() -> bool {
    true
}

impl ProviderModel {
    /// An enabled model with no metadata (manual entry, legacy row).
    pub fn plain(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            enabled: true,
            label: None,
            context_window: None,
            tags: Vec::new(),
            surfaces: Vec::new(),
        }
    }

    /// Public REST view.
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "enabled": self.enabled,
            "label": self.label,
            "contextWindow": self.context_window,
            "tags": self.tags,
            "surfaces": self.surfaces,
        })
    }
}

/// Union two catalogs, keeping the richest metadata for an id served by both.
///
/// A gateway can answer `/v1/models` differently per header — astergate returns
/// its full OpenAI-style list to a plain Bearer request but only six `claude-*`
/// ids when `anthropic-version` is set — so a single probe under-reports. Order
/// follows first appearance, `primary` first. No id is ever filtered by prefix.
pub fn union_catalogs(
    primary: Vec<ProviderModel>,
    secondary: Vec<ProviderModel>,
) -> Vec<ProviderModel> {
    let mut out: Vec<ProviderModel> = Vec::new();
    for model in primary.into_iter().chain(secondary) {
        if model.id.is_empty() {
            continue;
        }
        match out.iter_mut().find(|existing| existing.id == model.id) {
            Some(existing) => {
                // Both listings saw it: record the surface, keep any metadata
                // the first listing lacked.
                for surface in model.surfaces {
                    if !existing.surfaces.contains(&surface) {
                        existing.surfaces.push(surface);
                    }
                }
                if existing.label.is_none() {
                    existing.label = model.label;
                }
                if existing.context_window.is_none() {
                    existing.context_window = model.context_window;
                }
                for tag in model.tags {
                    if !existing.tags.contains(&tag) {
                        existing.tags.push(tag);
                    }
                }
            }
            None => out.push(model),
        }
    }
    out
}

/// Stamp every entry with the listing it came from.
pub fn tag_surface(mut models: Vec<ProviderModel>, surface: &str) -> Vec<ProviderModel> {
    for model in &mut models {
        if !model.surfaces.iter().any(|s| s == surface) {
            model.surfaces.push(surface.to_string());
        }
    }
    models
}

/// Ids of the models New Session may offer.
pub fn enabled_ids(models: &[ProviderModel]) -> Vec<&str> {
    models
        .iter()
        .filter(|m| m.enabled)
        .map(|m| m.id.as_str())
        .collect()
}

/// Accept a structured list, a legacy `["id", …]` list, or a bare string.
///
/// Unknown shapes drop out rather than failing the row: a profile whose
/// catalog cannot be read is still usable with a manually typed model.
pub fn parse_models_json(raw: &str) -> Vec<ProviderModel> {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    from_value(&value)
}

/// Normalize one already-parsed catalog value (array of strings or objects).
pub fn from_value(value: &Value) -> Vec<ProviderModel> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    let mut out: Vec<ProviderModel> = Vec::new();
    for item in items {
        let model = match item {
            Value::String(id) => ProviderModel::plain(id.trim()),
            Value::Object(_) => {
                let Some(id) = item.get("id").and_then(Value::as_str) else {
                    continue;
                };
                ProviderModel {
                    id: id.trim().to_string(),
                    enabled: item.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                    label: string_field(item, &["label", "displayName", "display_name", "name"]),
                    context_window: context_window(item),
                    tags: tags(item),
                    surfaces: string_list(item, "surfaces"),
                }
            }
            _ => continue,
        };
        if model.id.is_empty() || out.iter().any(|existing| existing.id == model.id) {
            continue;
        }
        out.push(model);
    }
    out
}

/// True when `raw` is a legacy `["id", …]` catalog that wants rewriting.
pub fn is_legacy_json(raw: &str) -> bool {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .is_some_and(|items| items.iter().any(Value::is_string))
}

/// Normalize a gateway `/v1/models` body.
///
/// Handles the Anthropic shape (`{"data": [{"id", "display_name", …}]}`), the
/// OpenAI shape (`{"object": "list", "data": [{"id", …}]}`), a `{"models": …}`
/// wrapper, and a bare top-level array. Discovered models come back enabled so
/// the checklist starts with everything the gateway offers.
pub fn normalize_catalog(body: &str) -> Vec<ProviderModel> {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    for pointer in ["/data", "/models", "/result/data", "/body/data"] {
        if let Some(array) = value.pointer(pointer).filter(|v| v.is_array()) {
            let models = from_value(array);
            if !models.is_empty() {
                return models;
            }
        }
    }
    from_value(&value)
}

fn string_field(item: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(text) = item.get(*key).and_then(Value::as_str) {
            let text = text.trim();
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    None
}

fn context_window(item: &Value) -> Option<u64> {
    for key in [
        "contextWindow",
        "context_window",
        "context_length",
        "max_context_tokens",
        "max_input_tokens",
        "max_tokens",
    ] {
        if let Some(n) = item.get(key).and_then(Value::as_u64).filter(|n| *n > 0) {
            return Some(n);
        }
    }
    None
}

fn tags(item: &Value) -> Vec<String> {
    let mut out: Vec<String> = string_list(item, "tags");
    if context_window(item).is_some_and(|n| n >= LONG_CONTEXT_TOKENS)
        && !out.iter().any(|tag| tag == "1m")
    {
        out.push("1m".into());
    }
    out
}

/// Deduplicated non-empty strings from an array field.
fn string_list(item: &Value, key: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(list) = item.get(key).and_then(Value::as_array) {
        for entry in list.iter().filter_map(Value::as_str) {
            let entry = entry.trim();
            if !entry.is_empty() && !out.iter().any(|existing| existing == entry) {
                out.push(entry.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_a_legacy_string_catalog_to_enabled_rows() {
        let legacy = r#"["passthrough/auto", "passthrough/auto_model"]"#;
        assert!(is_legacy_json(legacy));
        let models = parse_models_json(legacy);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0], ProviderModel::plain("passthrough/auto"));
        assert!(models.iter().all(|m| m.enabled));
        assert_eq!(
            enabled_ids(&models),
            vec!["passthrough/auto", "passthrough/auto_model"]
        );
        // Re-reading the migrated form is a no-op.
        let structured = serde_json::to_string(&models).unwrap();
        assert!(!is_legacy_json(&structured));
        assert_eq!(parse_models_json(&structured), models);
    }

    #[test]
    fn keeps_disabled_rows_out_of_the_enabled_set() {
        let raw = r#"[{"id":"a","enabled":false},{"id":"b","enabled":true}]"#;
        let models = parse_models_json(raw);
        assert_eq!(models.len(), 2);
        assert!(!models[0].enabled);
        assert_eq!(enabled_ids(&models), vec!["b"]);
    }

    #[test]
    fn drops_blank_duplicate_and_unreadable_catalogs() {
        let models = parse_models_json(r#"["a", "a", "", {"id":"a"}, {"noid":1}, 7]"#);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "a");
        assert!(parse_models_json("not json").is_empty());
        assert!(parse_models_json("{}").is_empty());
    }

    #[test]
    fn normalizes_the_anthropic_shape_with_display_names() {
        let body = r#"{"data":[
            {"type":"model","id":"claude-fable-5-1","display_name":"Fable 5.1","created_at":"2026-01-01"},
            {"type":"model","id":"claude-opus-5","display_name":"Opus 5"}
        ],"has_more":false}"#;
        let models = normalize_catalog(body);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "claude-fable-5-1");
        assert_eq!(models[0].label.as_deref(), Some("Fable 5.1"));
        assert!(models[0].enabled);
        assert_eq!(models[0].context_window, None);
        assert!(models[0].tags.is_empty());
    }

    #[test]
    fn normalizes_the_openai_shape_and_a_bare_array() {
        let openai = r#"{"object":"list","data":[
            {"id":"gw/small","object":"model","owned_by":"gw"},
            {"id":"gw/large","object":"model","context_length":1048576}
        ]}"#;
        let models = normalize_catalog(openai);
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["gw/small", "gw/large"]
        );
        assert_eq!(models[1].context_window, Some(1_048_576));
        assert_eq!(models[1].tags, vec!["1m".to_string()]);

        assert_eq!(normalize_catalog(r#"["a","b"]"#).len(), 2);
        assert_eq!(normalize_catalog(r#"{"models":["a"]}"#).len(), 1);
    }

    #[test]
    fn carries_gateway_tags_and_context_metadata() {
        let body =
            r#"{"data":[{"id":"m","name":"M","context_window":2000000,"tags":["beta","beta"]}]}"#;
        let models = normalize_catalog(body);
        assert_eq!(models[0].label.as_deref(), Some("M"));
        assert_eq!(models[0].context_window, Some(2_000_000));
        assert_eq!(models[0].tags, vec!["beta".to_string(), "1m".to_string()]);
    }

    #[test]
    fn an_unreadable_or_empty_body_yields_no_models() {
        assert!(normalize_catalog("").is_empty());
        assert!(normalize_catalog("<html>401</html>").is_empty());
        assert!(normalize_catalog(r#"{"error":{"message":"nope"}}"#).is_empty());
    }

    #[test]
    fn unions_the_two_listings_a_gateway_serves_per_header() {
        // astergate's shape: the plain listing is broad, the anthropic one is
        // a short claude-only subset that overlaps it.
        let openai = tag_surface(
            normalize_catalog(
                r#"{"object":"list","data":[
                    {"id":"passthrough/ark/seed-evolving"},
                    {"id":"cursor/gpt-5"},
                    {"id":"claude-opus-5"}
                ]}"#,
            ),
            SURFACE_OPENAI,
        );
        let anthropic = tag_surface(
            normalize_catalog(
                r#"{"data":[
                    {"type":"model","id":"claude-opus-5","display_name":"Opus 5","context_window":1048576},
                    {"type":"model","id":"claude-haiku-4-5"}
                ]}"#,
            ),
            SURFACE_ANTHROPIC,
        );
        let merged = union_catalogs(openai, anthropic);
        assert_eq!(
            merged.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec![
                "passthrough/ark/seed-evolving",
                "cursor/gpt-5",
                "claude-opus-5",
                "claude-haiku-4-5",
            ],
            "no id is dropped and none is filtered by prefix"
        );
        // Served by both listings, so both surfaces are recorded...
        let shared = &merged[2];
        assert_eq!(shared.surfaces, vec!["openai", "anthropic"]);
        // ...and the metadata only the anthropic listing carried is kept.
        assert_eq!(shared.label.as_deref(), Some("Opus 5"));
        assert_eq!(shared.context_window, Some(1_048_576));
        assert_eq!(shared.tags, vec!["1m".to_string()]);
        assert_eq!(merged[0].surfaces, vec!["openai"]);
        assert_eq!(merged[3].surfaces, vec!["anthropic"]);
    }

    #[test]
    fn union_is_stable_when_one_listing_is_empty_or_both_agree() {
        let only = tag_surface(normalize_catalog(r#"["a","b"]"#), SURFACE_OPENAI);
        let merged = union_catalogs(only.clone(), Vec::new());
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].surfaces, vec!["openai"]);
        // A gateway answering both headers identically must not duplicate ids.
        let same = tag_surface(normalize_catalog(r#"["a","b"]"#), SURFACE_ANTHROPIC);
        let merged = union_catalogs(only, same);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].surfaces, vec!["openai", "anthropic"]);
    }

    #[test]
    fn surfaces_survive_the_stored_round_trip() {
        let models = tag_surface(normalize_catalog(r#"["a"]"#), SURFACE_OPENAI);
        let stored = serde_json::to_string(&models).unwrap();
        let read = parse_models_json(&stored);
        assert_eq!(read[0].surfaces, vec!["openai"]);
        // A row saved before surfaces existed simply has none.
        assert!(
            parse_models_json(r#"[{"id":"a","enabled":true}]"#)[0]
                .surfaces
                .is_empty()
        );
    }
}
