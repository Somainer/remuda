//! Hub `/v1/providers` registry. Auth tokens live in [`FileSecretStore`], never in GET JSON.

use crate::AppState;
use crate::auth::{require_device, require_origin};
use crate::error::HubError;
use crate::http::map_store;
use crate::provider_models::{self, ProviderModel};
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
        .route("/v1/providers/discover", post(discover_models))
        .route(
            "/v1/providers/{id}",
            get(get_provider)
                .patch(patch_provider)
                .delete(delete_provider),
        )
        .route("/v1/providers/{id}/test", post(test_provider))
        .route(
            "/v1/providers/{id}/supply",
            get(get_supply).put(declare_supply),
        )
        .route(
            "/v1/providers/{id}/supply/events",
            post(report_supply_event),
        )
        .route("/v1/providers/{id}/usage", get(provider_usage))
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
    models: Value,
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
    /// Declared supply envelope (coordinator §4.2); optional.
    #[serde(default)]
    supply: Option<remuda_protocol::SupplyProfile>,
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
    models: Option<Value>,
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
    /// Replace the declared supply envelope (observations merged, not wiped).
    #[serde(default)]
    supply: Option<remuda_protocol::SupplyProfile>,
}

/// `POST /v1/providers/discover` body: probe a gateway before it is saved.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiscoverBody {
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    /// Token for an unsaved profile. Never echoed back.
    #[serde(default)]
    token: Option<String>,
    /// Reuse a saved profile's stored token instead of sending one.
    #[serde(default)]
    profile_id: Option<String>,
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
    // Secret-less profiles are legal: a native login (§4.1) has no token to
    // store, and a supply declaration may precede the token entry. A gateway
    // profile still requires its token.
    let token = body
        .auth_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if kind == "gateway" && token.is_none() {
        return Err(HubError::BadRequest(
            "authToken is required on create".into(),
        ));
    }
    let default_gateway = body.default_gateway && kind == "gateway";
    let scope = validate_scope(&state, body.scope.as_deref()).await?;
    let models = provider_models::from_value(&body.models);
    let default_model = resolve_default_model(body.default_model.as_deref(), &models)?;
    let (fingerprint, last4) = token
        .map(|token| fingerprint_secret(token.as_bytes()))
        .unzip();
    let id = crate::config::new_id("pvp").map_err(|err| HubError::Internal(err.to_string()))?;
    let secret_name = token.map(|_| vault_name(&id));
    if let (Some(token), Some(secret_name)) = (token, secret_name.as_ref()) {
        state
            .secrets
            .put(secret_name, token.as_bytes())
            .map_err(|err| HubError::Internal(format!("secret store: {err}")))?;
    }
    let supply = body.supply.unwrap_or_default();
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
            secret_name.clone(),
            last4,
            fingerprint,
            supply,
        )
        .await
    {
        Ok(profile) => profile,
        Err(err) => {
            if let Some(secret_name) = &secret_name {
                let _ = state.secrets.delete(secret_name);
            }
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
    let models = body.models.as_ref().map(provider_models::from_value);
    // `defaultModel` must name an enabled model of whichever catalog ends up
    // stored: the patched one when models are being replaced, else the saved one.
    let effective = models.as_deref().unwrap_or(&existing.models);
    let default_model = match (&body.default_model, &models) {
        (Some(requested), _) => Some(resolve_default_model(requested.as_deref(), effective)?),
        // Replacing the catalog can orphan the saved default; carry it forward
        // only while it is still enabled.
        (None, Some(_)) => Some(carry_default_model(
            existing.default_model.as_deref(),
            effective,
        )),
        (None, None) => None,
    };
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
            default_model,
            headers_map,
            default_gateway,
            scope,
            secret_name,
            secret_last4,
            secret_fingerprint,
            body.supply,
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
    let result = probe_models(
        &profile.base_url,
        &profile.headers,
        secret.expose_str().ok(),
    )
    .await;
    let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let message = result
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let _ = state.store.record_provider_test(id, ok, message).await;
    Ok(Json(result))
}

/// `POST /v1/providers/discover`
///
/// Probes a gateway's model list **before** the profile exists, so the form can
/// offer a checklist instead of a free-text box. Operator devices only; the
/// supplied token is used for the one request and never echoed back.
async fn discover_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<DiscoverBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    crate::agent_scope::require_operator(&state, &headers).await?;
    // An existing profile supplies its base URL, headers and stored token when
    // the operator re-probes without retyping the secret.
    let saved = match body
        .profile_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(id) => Some(
            state
                .store
                .get_provider(id.to_string())
                .await?
                .ok_or(HubError::NotFound)?,
        ),
        None => None,
    };
    let raw_base = match (body.base_url.trim(), saved.as_ref()) {
        ("", Some(profile)) => profile.base_url.clone(),
        (base, _) => base.to_string(),
    };
    let base_url = validate_base_url(&raw_base, "gateway")?;
    let headers_map = if body.headers.is_empty() {
        saved
            .as_ref()
            .map(|profile| profile.headers.clone())
            .unwrap_or_default()
    } else {
        sanitize_headers(body.headers)?
    };
    let supplied = body
        .token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let stored = match (&supplied, saved.as_ref()) {
        (None, Some(profile)) => load_secret(&state.secrets, profile).ok(),
        _ => None,
    };
    let token = supplied
        .as_deref()
        .or_else(|| stored.as_ref().and_then(|s| s.expose_str().ok()));
    Ok(Json(probe_models(&base_url, &headers_map, token).await))
}

/// `defaultModel` must be one of the enabled models; empty falls back to the
/// first enabled one so a catalog always has a usable prefill.
fn resolve_default_model(
    requested: Option<&str>,
    models: &[ProviderModel],
) -> Result<Option<String>, HubError> {
    let enabled = provider_models::enabled_ids(models);
    let Some(requested) = requested.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(enabled.first().map(|id| (*id).to_string()));
    };
    if enabled.is_empty() {
        // No catalog at all: the operator types the model in New Session.
        return Ok(Some(requested.to_string()));
    }
    if !enabled.contains(&requested) {
        return Err(HubError::BadRequest(format!(
            "defaultModel {requested} is not one of the enabled models"
        )));
    }
    Ok(Some(requested.to_string()))
}

/// Keep the saved default when the replacement catalog still enables it,
/// otherwise fall back to the first enabled model. Never an error: replacing
/// the catalog is a legitimate way to retire the old default.
fn carry_default_model(saved: Option<&str>, models: &[ProviderModel]) -> Option<String> {
    let enabled = provider_models::enabled_ids(models);
    match saved.map(str::trim).filter(|s| !s.is_empty()) {
        Some(saved) if enabled.contains(&saved) => Some(saved.to_string()),
        _ => enabled.first().map(|id| (*id).to_string()),
    }
}

/// D-021 Claude waterfall for a chosen host, then attach overlay metadata (never the token).
pub async fn resolve_and_attach(
    state: &AppState,
    host: &HostRecord,
    spec: &mut Value,
) -> Result<(), HubError> {
    resolve_and_attach_with_project(state, host, spec, None).await
}

/// Waterfall with a project layer (explicit > project > host > global; design
/// §6). `project` is the stored `Project.provider` reference, which never
/// contains a secret — only a profile id and a delegation shape.
pub async fn resolve_and_attach_with_project(
    state: &AppState,
    host: &HostRecord,
    spec: &mut Value,
    project: Option<&remuda_protocol::ProjectProviderRef>,
) -> Result<(), HubError> {
    strip_provider_secrets(spec);
    let kind = spec.get("kind").and_then(Value::as_str).unwrap_or("claude");
    if kind != "claude" {
        return attach_provider_to_spec(state, spec).await;
    }
    let profiles = state.store.list_providers(None).await?;
    let delegation = spec.get("delegation").and_then(Value::as_str);
    let provider_profile_id = spec.get("providerProfileId").and_then(Value::as_str);
    let project_profile_id = project
        .and_then(|provider| provider.profile_id.as_deref())
        .filter(|id| provider_resolve::is_real_profile_id(id));
    let project_delegation = project
        .and_then(|provider| provider.delegation.as_deref())
        .filter(|value| !value.is_empty());
    let resolved = provider_resolve::resolve(ResolveInput {
        host,
        profiles: &profiles,
        delegation,
        provider_profile_id,
        project_profile_id,
        project_delegation,
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
        // A secret-less native-login account row: its only job is to declare
        // supply for a host CLI login the Hub holds no token for (§4.1).
        "native" => Ok("native".into()),
        other => Err(HubError::BadRequest(format!(
            "kind must be gateway, direct, or native, not {other}"
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
    if kind == "direct" || kind == "native" {
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

/// Probe both gateway listings and union the result. Shared by `/test` and
/// `/discover` so both report the same shape and the same timeouts.
///
/// A gateway may serve a different catalog per header: astergate answers a
/// plain `Authorization: Bearer` GET with its full OpenAI-style list but
/// returns only six `claude-*` ids once `anthropic-version` is set. Probing one
/// surface therefore under-reports, so both run and the ids are unioned, each
/// tagged with the listing it came from.
async fn probe_models(
    base_url: &str,
    extra_headers: &BTreeMap<String, String>,
    token: Option<&str>,
) -> Value {
    let (openai, anthropic) = tokio::join!(
        probe_surface(
            base_url,
            extra_headers,
            token,
            provider_models::SURFACE_OPENAI
        ),
        probe_surface(
            base_url,
            extra_headers,
            token,
            provider_models::SURFACE_ANTHROPIC
        ),
    );
    // Prefer whichever surface answered; a gateway that speaks only one still
    // reports that one's catalog and its status.
    let primary = if openai.ok { &openai } else { &anthropic };
    let models = provider_models::union_catalogs(openai.models.clone(), anthropic.models.clone());
    let reachable = openai.reachable || anthropic.reachable;
    let ok = openai.ok || anthropic.ok;
    let status = primary.status;
    let latency = openai.latency.max(anthropic.latency);
    let message = if ok {
        let surfaces = match (openai.ok, anthropic.ok) {
            (true, true) => "openai+anthropic",
            (true, false) => "openai",
            _ => "anthropic",
        };
        if models.is_empty() {
            format!("reachable ({}); no models listed", status.unwrap_or(200))
        } else {
            format!(
                "reachable ({}); {} models via {surfaces}",
                status.unwrap_or(200),
                models.len()
            )
        }
    } else {
        primary.message.clone()
    };
    json!({
        "ok": ok,
        "reachable": reachable,
        "status": status,
        "latencyMs": latency,
        "message": message,
        "models": models.iter().map(ProviderModel::to_json).collect::<Vec<_>>(),
    })
}

/// One listing's outcome.
struct SurfaceProbe {
    ok: bool,
    reachable: bool,
    status: Option<u16>,
    latency: u64,
    message: String,
    models: Vec<ProviderModel>,
}

/// `GET {baseUrl}/v1/models` on one surface. `anthropic` adds the
/// `anthropic-version` header; `openai` sends only the bearer credentials.
async fn probe_surface(
    base_url: &str,
    extra_headers: &BTreeMap<String, String>,
    token: Option<&str>,
    surface: &str,
) -> SurfaceProbe {
    let url = models_url(base_url);
    let started = Instant::now();
    let client = match reqwest::Client::builder()
        .timeout(TEST_TIMEOUT)
        .connect_timeout(Duration::from_secs(3))
        .no_proxy()
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            return SurfaceProbe {
                ok: false,
                reachable: false,
                status: None,
                latency: 0,
                message: format!("unreachable: {err}"),
                models: Vec::new(),
            };
        }
    };
    let mut request = client.get(&url);
    if let Some(token) = token.filter(|s| !s.is_empty()) {
        request = request
            .header("Authorization", format!("Bearer {token}"))
            .header("x-api-key", token);
        if surface == provider_models::SURFACE_ANTHROPIC {
            request = request.header("anthropic-version", "2023-06-01");
        }
    }
    for (name, value) in extra_headers {
        request = request.header(name, value);
    }
    match request.send().await {
        Ok(response) => {
            let latency = started.elapsed().as_millis() as u64;
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            let ok = (200..300).contains(&status);
            let models = if ok {
                provider_models::tag_surface(provider_models::normalize_catalog(&body), surface)
            } else {
                Vec::new()
            };
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
            SurfaceProbe {
                ok,
                reachable: true,
                status: Some(status),
                latency,
                message,
                models,
            }
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
            SurfaceProbe {
                ok: false,
                reachable: false,
                status: None,
                latency,
                message: format!("unreachable: {reason}"),
                models: Vec::new(),
            }
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

// ── declared supply + observed events (coordinator §4.2, §4.5) ─────────────

/// `GET /v1/providers/:id/supply` — declared + observed supply envelope.
async fn get_supply(
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
    Ok(Json(json!({
        "id": profile.id,
        "name": profile.name,
        "supply": profile.supply,
        "catalogRevision": crate::model_catalog::CATALOG_REVISION,
        "catalogUpdated": crate::model_catalog::CATALOG_UPDATED,
    })))
}

/// `PUT /v1/providers/:id/supply` — replace the user-declared envelope while
/// preserving observed runtime fields (cooldowns, lastError) and merged
/// window observations.
async fn declare_supply(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(declared): Json<remuda_protocol::SupplyProfile>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    require_device(&state.store, &headers).await?;
    let existing = state
        .store
        .get_provider(id.clone())
        .await?
        .ok_or(HubError::NotFound)?;
    let merged = merge_declared_supply(existing.supply, declared);
    let profile = state
        .store
        .update_provider_supply(id, merged.clone())
        .await
        .map_err(map_store)?;
    let _ = state
        .store
        .append_audit(
            "operator".into(),
            "supply.declare".into(),
            Some(profile.id.clone()),
            json!({"priority": merged.priority,
                   "concurrencyMax": merged.concurrency.max,
                   "windows": merged.windows.len()}),
        )
        .await;
    Ok(Json(json!({ "id": profile.id, "supply": profile.supply })))
}

/// Preserve runtime-managed fields when an operator re-declares.
fn merge_declared_supply(
    current: remuda_protocol::SupplyProfile,
    mut declared: remuda_protocol::SupplyProfile,
) -> remuda_protocol::SupplyProfile {
    // Declared windows keep runtime cooldown/usedPercent state when the
    // declaration still names them by id (observed fields are never cleared).
    for window in &mut declared.windows {
        if let Some(existing) = current.windows.iter().find(|w| w.id == window.id) {
            if window.cooldown_until.is_none() {
                window.cooldown_until = existing.cooldown_until;
            }
            if window.used_percent.is_none() {
                window.used_percent = existing.used_percent;
            }
            if window.observed_at.is_none() {
                window.observed_at = existing.observed_at;
            }
            window.backoff_attempts = existing.backoff_attempts;
        }
    }
    // Operator never manages runtime state directly; carry the observed
    // windows that the new declaration dropped.
    for existing in current.windows {
        if !declared.windows.iter().any(|w| w.id == existing.id)
            && existing.cooldown_until.is_some()
        {
            declared.windows.push(existing);
        }
    }
    declared.state = current.state;
    declared.cooldown_until = current.cooldown_until;
    declared.last_error = current.last_error;
    declared
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SupplyEventBody {
    /// `textual` (429/529 line + optional httpStatus) or `structured`
    /// (Codex `account/rateLimits/updated` shape).
    #[serde(rename = "type")]
    event_type: String,
    /// Raw screen/journal text for textual events.
    #[serde(default)]
    text: String,
    /// HTTP status when the text came from a probe/response.
    #[serde(default)]
    http_status: Option<u16>,
    /// Model/family this event concerns (defaults to the profile's workhorse).
    #[serde(default)]
    model: Option<String>,
    /// Structured window frames.
    #[serde(default)]
    windows: Vec<remuda_protocol::RateLimitWindow>,
}

/// `POST /v1/providers/:id/supply/events` — report observed supply evidence
/// (probe results, synthetic tests, future Node relay). A 429 cools, a 529
/// records but cools nothing.
async fn report_supply_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<SupplyEventBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    require_device(&state.store, &headers).await?;
    let profile = state
        .store
        .get_provider(id.clone())
        .await?
        .ok_or(HubError::NotFound)?;
    let mut supply = profile.supply.clone();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let (action, cooled) = match body.event_type.as_str() {
        "textual" => {
            let model_id = body
                .model
                .or_else(|| profile.default_model.clone())
                .unwrap_or_default();
            let family = crate::supply::family_of(&profile, &model_id);
            let signal = crate::supply::observe_textual_event(
                &mut supply.windows,
                &family,
                body.http_status,
                &body.text,
                now,
            );
            match signal {
                Some(crate::supply::RateSignal::RateLimited) => ("supply.cooldown", vec![family]),
                Some(crate::supply::RateSignal::FleetOverloaded) => {
                    supply.last_error =
                        Some("upstream fleet overload (529); windows not cooled".to_string());
                    ("supply.overload-observed", Vec::new())
                }
                None => {
                    return Err(HubError::BadRequest(
                        "text did not match a 429/rate-limit or 529/overload signal".into(),
                    ));
                }
            }
        }
        "structured" => {
            if body.windows.is_empty() {
                return Err(HubError::BadRequest(
                    "structured event requires windows[]".into(),
                ));
            }
            let observed = remuda_protocol::ObservedRateLimits {
                windows: body.windows,
            };
            crate::supply::apply_structured_windows(&mut supply.windows, &observed, now);
            ("supply.rate-limits-observed", Vec::new())
        }
        other => {
            return Err(HubError::BadRequest(format!(
                "event type must be textual or structured, not {other}"
            )));
        }
    };
    supply.state = crate::supply::refresh_state(&mut supply.windows, None, now);
    supply.cooldown_until = supply
        .windows
        .iter()
        .filter(|w| w.is_account_level())
        .filter_map(|w| w.cooldown_until)
        .max();
    if action == "supply.cooldown" {
        supply.last_error = Some(format!("429 observed for families {cooled:?}"));
    }
    let saved = state
        .store
        .update_provider_supply(id, supply.clone())
        .await
        .map_err(map_store)?;
    let _ = state
        .store
        .append_audit(
            "operator".into(),
            action.into(),
            Some(saved.id.clone()),
            json!({"cooledFamilies": cooled, "state": format!("{:?}", supply.state)}),
        )
        .await;
    Ok(Json(json!({ "id": saved.id, "supply": saved.supply })))
}

/// `GET /v1/providers/:id/usage?budgetMaxUsd=…` — Hub-side usage aggregation
/// (§4.5): per-model token/cost totals from journaled `usage` events plus the
/// estimate×1.15 budget band. Money is always an estimate.
async fn provider_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<Value>, HubError> {
    crate::agent_scope::require_operator(&state, &headers).await?;
    let profile = state
        .store
        .get_provider(id.clone())
        .await?
        .ok_or(HubError::NotFound)?;
    let models: Vec<Value> = {
        let store = state.store.clone();
        let ids: Vec<String> = profile.models.iter().map(|m| m.id.clone()).collect();
        let mut out = Vec::new();
        for model in ids {
            let id_for = id.clone();
            let model_for = model.clone();
            let agg = store
                .run(move |conn| {
                    crate::usage_store::aggregate_supply(conn, &id_for, &model_for)
                        .map_err(crate::store::StoreError::from)
                })
                .await
                .map_err(map_store)?;
            if agg.events == 0 {
                continue;
            }
            let budget_max = query.budget_max_usd;
            out.push(json!({
                "model": model,
                "events": agg.events,
                "totalTokens": agg.total_tokens,
                "inputTokens": agg.input_tokens,
                "outputTokens": agg.output_tokens,
                "estimatedUsd": agg.cost_usd,
                "budgetStatus": match agg.budget_status(budget_max) {
                    crate::usage_store::BudgetStatus::Ok => "ok",
                    crate::usage_store::BudgetStatus::Warn => "warn",
                    crate::usage_store::BudgetStatus::Stop => "stop",
                },
                "estimated": true,
            }));
        }
        out
    };
    Ok(Json(json!({
        "id": profile.id,
        "models": models,
        "budgetStopFactor": crate::usage_store::BUDGET_STOP_FACTOR,
        "note": "costs are estimates (估算); hard cap band is estimate x 1.15",
    })))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UsageQuery {
    /// Optional estimated USD budget to grade the band against.
    #[serde(default)]
    budget_max_usd: Option<f64>,
}
