//! Structured provider model catalog: the stored list, the legacy string
//! migration, and the gateway `/v1/models` normalizer shared by `/test` and
//! `/discover`. Nothing here ever touches an auth token.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Context window at or above this many tokens earns the `1m` tag.
const LONG_CONTEXT_TOKENS: u64 = 1_000_000;

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
        })
    }
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
    let mut out: Vec<String> = Vec::new();
    if let Some(list) = item.get("tags").and_then(Value::as_array) {
        for tag in list.iter().filter_map(Value::as_str) {
            let tag = tag.trim();
            if !tag.is_empty() && !out.iter().any(|existing| existing == tag) {
                out.push(tag.to_string());
            }
        }
    }
    if context_window(item).is_some_and(|n| n >= LONG_CONTEXT_TOKENS)
        && !out.iter().any(|tag| tag == "1m")
    {
        out.push("1m".into());
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
}
