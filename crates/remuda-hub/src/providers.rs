//! Hub `/v1/providers` registry. Auth tokens live in [`FileSecretStore`], never in GET JSON.

use crate::AppState;
use crate::auth::{require_device, require_origin};
use crate::error::HubError;
use crate::http::map_store;
use crate::provider_resolve::{self, ResolveInput};
use crate::store::{HostRecord, ProviderRecord};
use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use remuda_driver::{FileSecretStore, Secret, fingerprint_secret};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const TEST_TIMEOUT: Duration = Duration::from_secs(8);

/// Provider HTTP surface.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/providers", get(list_providers).post(create_provider))
        .route(
            "/v1/providers/{id}",
            get(get_provider)
                .patch(patch_provider)
                .delete(delete_provider),
        )
        .route("/v1/providers/{id}/test", post(test_provider))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateBody {
    name: String,
    #[serde(default = "default_kind")]
    kind: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    default_model: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    auth_token: Option<String>,
    #[serde(default)]
    default_gateway: bool,
    #[serde(default)]
    scope: Option<String>,
}

fn default_kind() -> String {
    "gateway".into()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PatchBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    models: Option<Vec<String>>,
    #[serde(default)]
    default_model: Option<Option<String>>,
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    auth_token: Option<String>,
    #[serde(default)]
    default_gateway: Option<bool>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Deserialize, Default)]
struct ListQuery {
    #[serde(default, rename = "hostId")]
    host_id: Option<String>,
}

/// `GET /v1/providers`
async fn list_providers(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, HubError> {
    crate::agent_scope::require_operator(&state, &headers).await?;
    let host_id = query
        .host_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let items: Vec<Value> = state
        .store
        .list_providers(host_id)
        .await?
        .iter()
        .map(ProviderRecord::to_json)
        .collect();
    Ok(Json(json!({ "items": items, "nextCursor": null })))
}

/// `GET /v1/providers/:id`
async fn get_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, HubError> {
    crate::agent_scope::require_operator(&state, &headers).await?;
    let profile = state
        .store
        .get_provider(id)
        .await?
        .ok_or(HubError::NotFound)?;
    Ok(Json(profile.to_json()))
}

/// `POST /v1/providers`
async fn create_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    require_device(&state.store, &headers).await?;
    let kind = normalize_kind(&body.kind)?;
    let name = validate_name(&body.name)?;
    let base_url = validate_base_url(&body.base_url, &kind)?;
    let headers_map = sanitize_headers(body.headers)?;
    let token = body
        .auth_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| HubError::BadRequest("authToken is required on create".into()))?;
    let default_gateway = body.default_gateway && kind == "gateway";
    let scope = validate_scope(&state, body.scope.as_deref()).await?;
    let models = sanitize_models(body.models);
    let default_model = body
        .default_model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| models.first().cloned());
    let (fingerprint, last4) = fingerprint_secret(token.as_bytes());
    let id = crate::config::new_id("pvp").map_err(|err| HubError::Internal(err.to_string()))?;
    let secret_name = vault_name(&id);
    state
        .secrets
        .put(&secret_name, token.as_bytes())
        .map_err(|err| HubError::Internal(format!("secret store: {err}")))?;
    let profile = match state
        .store
        .insert_provider(
            id.clone(),
            name,
            kind,
            base_url,
            models,
            default_model,
            headers_map,
            default_gateway,
            scope,
            Some(secret_name.clone()),
            Some(last4),
            Some(fingerprint),
        )
        .await
    {
        Ok(profile) => profile,
        Err(err) => {
            let _ = state.secrets.delete(&secret_name);
            return Err(map_store(err));
        }
    };
    Ok(Json(profile.to_json()))
}

/// `PATCH /v1/providers/:id`
async fn patch_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PatchBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    require_device(&state.store, &headers).await?;
    let existing = state
        .store
        .get_provider(id.clone())
        .await?
        .ok_or(HubError::NotFound)?;
    let kind = match &body.kind {
        Some(kind) => normalize_kind(kind)?,
        None => existing.kind.clone(),
    };
    let name = body.name.as_deref().map(validate_name).transpose()?;
    let base_url = match &body.base_url {
        Some(url) => Some(validate_base_url(url, &kind)?),
        None => None,
    };
    let headers_map = body.headers.map(sanitize_headers).transpose()?;
    let models = body.models.map(sanitize_models);
    let default_gateway = body.default_gateway.map(|flag| flag && kind == "gateway");
    let scope = match body.scope.as_deref() {
        Some(raw) => Some(validate_scope(&state, Some(raw)).await?),
        None => None,
    };
    let mut secret_last4 = None;
    let mut secret_fingerprint = None;
    let mut secret_name = None;
    if let Some(token) = body
        .auth_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let vault = existing
            .secret_name
            .clone()
            .unwrap_or_else(|| vault_name(&existing.id));
        state
            .secrets
            .put(&vault, token.as_bytes())
            .map_err(|err| HubError::Internal(format!("secret store: {err}")))?;
        let (fingerprint, last4) = fingerprint_secret(token.as_bytes());
        secret_last4 = Some(Some(last4));
        secret_fingerprint = Some(Some(fingerprint));
        secret_name = Some(Some(vault));
    }
    let profile = state
        .store
        .update_provider(
            id,
            name,
            body.kind.map(|_| kind),
            base_url,
            models,
            body.default_model,
            headers_map,
            default_gateway,
            scope,
            secret_name,
            secret_last4,
            secret_fingerprint,
        )
        .await
        .map_err(map_store)?;
    Ok(Json(profile.to_json()))
}

/// `DELETE /v1/providers/:id`
async fn delete_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    require_device(&state.store, &headers).await?;
    let existing = state
        .store
        .delete_provider(id)
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    if let Some(name) = existing.secret_name.as_deref() {
        let _ = state.secrets.delete(name);
    }
    Ok(Json(json!({ "ok": true })))
}

/// `POST /v1/providers/:id/test`
async fn test_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    require_device(&state.store, &headers).await?;
    let profile = state
        .store
        .get_provider(id.clone())
        .await?
        .ok_or(HubError::NotFound)?;
    let secret = load_secret(&state.secrets, &profile)?;
    let result = probe_gateway(&profile, secret.expose_str().ok()).await;
    let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let message = result
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let _ = state.store.record_provider_test(id, ok, message).await;
    Ok(Json(result))
}

/// D-021 Claude waterfall for a chosen host, then attach overlay metadata (never the token).
pub async fn resolve_and_attach(
    state: &AppState,
    host: &HostRecord,
    spec: &mut Value,
) -> Result<(), HubError> {
    strip_provider_secrets(spec);
    let kind = spec.get("kind").and_then(Value::as_str).unwrap_or("claude");
    if kind != "claude" {
        return attach_provider_to_spec(state, spec).await;
    }
    let profiles = state.store.list_providers(None).await?;
    let delegation = spec.get("delegation").and_then(Value::as_str);
    let provider_profile_id = spec.get("providerProfileId").and_then(Value::as_str);
    let resolved = provider_resolve::resolve(ResolveInput {
        host,
        profiles: &profiles,
        delegation,
        provider_profile_id,
    })?;
    provider_resolve::apply_to_spec(spec, &resolved);
    Ok(())
}

/// Attach a public overlay snapshot to an instance spec (never the token).
pub async fn attach_provider_to_spec(state: &AppState, spec: &mut Value) -> Result<(), HubError> {
    strip_provider_secrets(spec);
    let Some(obj) = spec.as_object_mut() else {
        return Ok(());
    };
    let delegation = obj
        .get("delegation")
        .and_then(Value::as_str)
        .unwrap_or("none");
    if delegation == "none" {
        return Ok(());
    }
    let requested = obj
        .get("providerProfileId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(profile) = resolve_profile_for_launch(state, delegation, requested).await? else {
        return Ok(());
    };
    let model = obj.get("model").and_then(Value::as_str).map(str::to_string);
    obj.insert("providerProfileId".into(), json!(profile.id));
    obj.insert(
        "providerOverlay".into(),
        profile.overlay_spec(model.as_deref()),
    );
    Ok(())
}

/// Inject `providerAuthToken` into Node RPC params. Do not persist or log the return.
pub async fn with_launch_secret(
    state: &AppState,
    host_id: &str,
    mut params: Value,
) -> Result<Value, HubError> {
    let spec = params.get("spec").unwrap_or(&params);
    let delegation = spec.get("delegation").and_then(Value::as_str).unwrap_or("");
    if delegation == "none" {
        return Ok(params);
    }
    let profile_id = spec
        .get("providerProfileId")
        .and_then(Value::as_str)
        .or_else(|| {
            spec.pointer("/providerOverlay/profileId")
                .and_then(Value::as_str)
        })
        .map(str::to_string);
    let Some(profile_id) = profile_id else {
        return Ok(params);
    };
    if profile_id == "none" || profile_id == "native" {
        return Ok(params);
    }
    let Some(profile) = state.store.get_provider(profile_id).await? else {
        return Ok(params);
    };
    if !provider_resolve::secret_release_allowed(&profile, host_id) {
        return Err(HubError::Forbidden);
    }
    let secret = load_secret(&state.secrets, &profile)?;
    let token = secret
        .expose_str()
        .map_err(|err| HubError::Internal(err.to_string()))?
        .to_string();
    let target = if params.get("spec").is_some() {
        params.get_mut("spec")
    } else {
        Some(&mut params)
    };
    if let Some(obj) = target.and_then(Value::as_object_mut) {
        obj.insert("providerAuthToken".into(), json!(token));
    }
    Ok(params)
}

async fn resolve_profile_for_launch(
    state: &AppState,
    delegation: &str,
    requested: Option<&str>,
) -> Result<Option<ProviderRecord>, HubError> {
    let alias = requested.filter(|id| provider_resolve::is_real_profile_id(id));
    if let Some(id) = alias {
        return Ok(Some(
            state
                .store
                .get_provider(id.to_string())
                .await?
                .ok_or_else(|| HubError::BadRequest(format!("unknown provider profile {id}")))?,
        ));
    }
    if delegation == "gateway" {
        return Ok(state.store.default_gateway("universal".into()).await?);
    }
    if delegation == "direct" {
        let items = state.store.list_providers(None).await?;
        return Ok(items.into_iter().find(|p| p.kind == "direct"));
    }
    Ok(None)
}

async fn validate_scope(state: &AppState, raw: Option<&str>) -> Result<String, HubError> {
    let scope = provider_resolve::normalize_scope(raw.unwrap_or("universal"))
        .map_err(HubError::BadRequest)?;
    if let Some(host_id) = scope.strip_prefix("host:")
        && state.store.get_host(host_id.to_string()).await?.is_none()
    {
        return Err(HubError::BadRequest(format!("unknown host {host_id}")));
    }
    Ok(scope)
}

fn load_secret(store: &FileSecretStore, profile: &ProviderRecord) -> Result<Secret, HubError> {
    let name = profile
        .secret_name
        .as_deref()
        .ok_or_else(|| HubError::BadRequest("provider profile has no stored auth token".into()))?;
    store
        .get(name)
        .map_err(|_| HubError::BadRequest("provider profile has no stored auth token".into()))
}

fn vault_name(profile_id: &str) -> String {
    format!("provider-{profile_id}")
}

fn normalize_kind(kind: &str) -> Result<String, HubError> {
    match kind.trim() {
        "gateway" => Ok("gateway".into()),
        "direct" => Ok("direct".into()),
        other => Err(HubError::BadRequest(format!(
            "kind must be gateway or direct, not {other}"
        ))),
    }
}

fn validate_name(name: &str) -> Result<String, HubError> {
    let name = name.trim();
    if name.is_empty() || name.len() > 128 {
        return Err(HubError::BadRequest(
            "name must be 1..=128 characters".into(),
        ));
    }
    Ok(name.to_string())
}

fn validate_base_url(raw: &str, kind: &str) -> Result<String, HubError> {
    let raw = raw.trim();
    if kind == "direct" && raw.is_empty() {
        return Ok(String::new());
    }
    if raw.is_empty() {
        return Err(HubError::BadRequest(
            "gateway profiles require a baseUrl".into(),
        ));
    }
    let uri: axum::http::Uri = raw
        .parse()
        .map_err(|_| HubError::BadRequest("baseUrl is not a valid URL".into()))?;
    match uri.scheme_str() {
        Some("http") | Some("https") => {}
        _ => {
            return Err(HubError::BadRequest("baseUrl must be http or https".into()));
        }
    }
    if uri.host().is_none() {
        return Err(HubError::BadRequest("baseUrl must include a host".into()));
    }
    Ok(raw.trim_end_matches('/').to_string())
}

fn sanitize_headers(
    headers: BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, HubError> {
    let mut out = BTreeMap::new();
    for (name, value) in headers {
        let key = name.trim();
        if key.is_empty() {
            continue;
        }
        let lower = key.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "authorization" | "x-api-key" | "api-key" | "proxy-authorization"
        ) {
            return Err(HubError::BadRequest(
                "put the auth token in authToken, not headers".into(),
            ));
        }
        if value.len() > 4096 {
            return Err(HubError::BadRequest("header value is too large".into()));
        }
        out.insert(key.to_string(), value);
    }
    Ok(out)
}

fn sanitize_models(models: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for model in models {
        let model = model.trim();
        if model.is_empty() || out.iter().any(|existing: &String| existing == model) {
            continue;
        }
        out.push(model.to_string());
    }
    out
}

fn strip_provider_secrets(spec: &mut Value) {
    let Some(obj) = spec.as_object_mut() else {
        return;
    };
    for key in [
        "authToken",
        "providerAuthToken",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
    ] {
        obj.remove(key);
    }
}

fn models_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    }
}

async fn probe_gateway(profile: &ProviderRecord, token: Option<&str>) -> Value {
    let url = models_url(&profile.base_url);
    let started = Instant::now();
    let client = match reqwest::Client::builder()
        .timeout(TEST_TIMEOUT)
        .connect_timeout(Duration::from_secs(3))
        .no_proxy()
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            return json!({
                "ok": false,
                "reachable": false,
                "status": null,
                "latencyMs": 0,
                "message": format!("unreachable: {err}"),
                "models": [],
            });
        }
    };
    let mut request = client.get(&url);
    if let Some(token) = token.filter(|s| !s.is_empty()) {
        request = request
            .header("Authorization", format!("Bearer {token}"))
            .header("x-api-key", token)
            .header("anthropic-version", "2023-06-01");
    }
    for (name, value) in &profile.headers {
        request = request.header(name, value);
    }
    match request.send().await {
        Ok(response) => {
            let latency = started.elapsed().as_millis() as u64;
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            let models = parse_model_ids(&body);
            let reachable = true;
            let ok = (200..300).contains(&status);
            let message = if ok {
                if models.is_empty() {
                    format!("reachable ({status}); no models listed")
                } else {
                    format!("reachable ({status}); {} models", models.len())
                }
            } else if status == 401 || status == 403 {
                format!("reachable but auth failed ({status})")
            } else {
                format!("reachable but HTTP {status}")
            };
            json!({
                "ok": ok,
                "reachable": reachable,
                "status": status,
                "latencyMs": latency,
                "message": message,
                "models": models,
            })
        }
        Err(err) => {
            let latency = started.elapsed().as_millis() as u64;
            let reason = if err.is_timeout() {
                format!("timed out contacting {url}")
            } else if err.is_connect() {
                format!("could not connect to {url}")
            } else {
                sanitize_probe_error(&err, token)
            };
            json!({
                "ok": false,
                "reachable": false,
                "status": null,
                "latencyMs": latency,
                "message": format!("unreachable: {reason}"),
                "models": [],
            })
        }
    }
}

fn sanitize_probe_error(err: &reqwest::Error, token: Option<&str>) -> String {
    let mut text = err.to_string();
    if let Some(token) = token.filter(|s| !s.is_empty()) {
        text = text.replace(token, "[redacted]");
    }
    text
}

fn parse_model_ids(body: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    if let Some(data) = value.get("data").and_then(Value::as_array) {
        for item in data {
            if let Some(id) = item.get("id").and_then(Value::as_str) {
                ids.push(id.to_string());
            }
        }
    }
    if ids.is_empty()
        && let Some(models) = value.get("models").and_then(Value::as_array)
    {
        for item in models {
            if let Some(id) = item
                .as_str()
                .or_else(|| item.get("id").and_then(Value::as_str))
            {
                ids.push(id.to_string());
            }
        }
    }
    ids
}
