//! WebAuthn passkey registration and login (D-030).
//!
//! Passkeys are a second *redemption* path into the same device session that
//! the bootstrap access code mints (D-018): a successful passkey login issues
//! the identical Argon2-hashed device token and `remuda_device` cookie. The
//! first credential can therefore only be registered from an already paired
//! device; there is no self-service passkey signup.
//!
//! We use `webauthn-rs-core` directly rather than its safe wrapper crate: the
//! wrapper's builder rejects IP-literal origins (`Url::domain()` returns
//! `None` for `127.0.0.1`) and does not re-export
//! `WebauthnCore::new_unsafe_experts_only`, which is the only constructor that
//! accepts the loopback HTTP origins `remuda dev` and the hub e2e harness run
//! on. The ceremony parameters mirror the wrapper's passkey flow exactly
//! (attestation `none`, user verification required, secure algorithm set);
//! the origin allowlist is assembled per request from Hub config and never
//! contains a hardcoded host.

use crate::auth::{device_cookie, hash_secret, require_device, require_origin, token_prefix};
use crate::config::{HubConfig, random_token};
use crate::error::HubError;
use crate::store::{PasskeyRecord, StoreError};
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use url::Url;
use uuid::Uuid;
use webauthn_rs_core::WebauthnCore;
use webauthn_rs_core::proto::{
    AttestationConveyancePreference, AttestationMetadata, AuthenticationState, COSEAlgorithm,
    CredProtect, Credential, CredentialProtectionPolicy, Mediation, PublicKeyCredential,
    RegisterPublicKeyCredential, RegistrationState, RequestAuthenticationExtensions,
    RequestRegistrationExtensions, UserVerificationPolicy,
};

use crate::AppState;

/// Server-side lifetime of a ceremony challenge.
const CHALLENGE_TTL: Duration = Duration::from_secs(120);
/// Bound on outstanding ceremonies (each finish removes its slot).
const MAX_CHALLENGES: usize = 4096;
/// UUID v5 namespace for the single per-RP operator account.
const USER_NAMESPACE: Uuid = Uuid::NAMESPACE_URL;
/// RP-independent account labels; the credential still binds to the RP id.
const USER_NAME: &str = "operator";
const USER_DISPLAY: &str = "Remuda Operator";
/// Human-supplied labels are capped at this many chars.
const MAX_NAME_LEN: usize = 64;

/// One pending register/login ceremony.
struct Challenge {
    state: ChallengeState,
    rp_id: String,
    /// Every origin offered to the authenticator for this ceremony.
    origins: Vec<String>,
    /// Registration challenges are bound to the device that started them.
    device_id: Option<String>,
    name: Option<String>,
    created: Instant,
}

enum ChallengeState {
    Register(RegistrationState),
    Login(AuthenticationState),
}

/// Process-local, single-use challenge table. Challenges deliberately do not
/// survive a Hub restart: a client whose ceremony is interrupted just starts
/// a new one.
#[derive(Clone, Default)]
pub(crate) struct ChallengeStore(Arc<Mutex<HashMap<String, Challenge>>>);

impl ChallengeStore {
    fn put(&self, id: String, challenge: Challenge) -> Result<(), HubError> {
        let mut table = self
            .0
            .lock()
            .map_err(|_| HubError::Internal("challenge lock poisoned".into()))?;
        let now = Instant::now();
        table.retain(|_, item| now.duration_since(item.created) < CHALLENGE_TTL);
        if table.len() >= MAX_CHALLENGES {
            return Err(HubError::Conflict(
                "too many pending passkey ceremonies".into(),
            ));
        }
        table.insert(id, challenge);
        Ok(())
    }

    /// Remove a challenge, rejecting anything unknown or expired. This makes
    /// every challenge single-use and bounds replay to the TTL.
    fn take(&self, id: &str) -> Result<Challenge, HubError> {
        let mut table = self
            .0
            .lock()
            .map_err(|_| HubError::Internal("challenge lock poisoned".into()))?;
        let challenge = table.remove(id).ok_or(HubError::Unauthenticated)?;
        if Instant::now().duration_since(challenge.created) >= CHALLENGE_TTL {
            return Err(HubError::Unauthenticated);
        }
        Ok(challenge)
    }
}

/// Passkey management and login routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/auth/passkeys/register/start", post(register_start))
        .route("/v1/auth/passkeys/register/finish", post(register_finish))
        .route("/v1/auth/passkeys/login/start", post(login_start))
        .route("/v1/auth/passkeys/login/finish", post(login_finish))
        .route("/v1/auth/passkeys", get(list_passkeys))
        .route(
            "/v1/auth/passkeys/{id}",
            axum::routing::patch(rename_passkey).delete(delete_passkey),
        )
}

/// A WebAuthn instance plus the validated request origin. Built per ceremony
/// because dev/demo origins are request-dependent.
struct Ceremony {
    core: WebauthnCore,
    rp_id: String,
    origins: Vec<String>,
}

fn ceremony(headers: &HeaderMap, config: &HubConfig) -> Result<Ceremony, HubError> {
    let raw = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .ok_or(HubError::Forbidden)?;
    let origin = Url::parse(raw).map_err(|_| HubError::Forbidden)?;
    if !matches!(origin.scheme(), "http" | "https") || origin.host_str().is_none() {
        return Err(HubError::Forbidden);
    }
    let host = origin.host_str().unwrap_or("");
    // Browsers only expose WebAuthn on non-secure origins when they are
    // loopback; everything else must be https.
    let loopback = matches!(host, "127.0.0.1" | "localhost");
    if !loopback && origin.scheme() != "https" {
        return Err(HubError::Forbidden);
    }
    let mut origins: Vec<Url> = Vec::new();
    if let Some(public) = &config.public_origin
        && let Ok(url) = Url::parse(public)
    {
        origins.push(url);
    }
    for allowed in &config.allowed_origins {
        if let Ok(url) = Url::parse(allowed) {
            origins.push(url);
        }
    }
    if loopback {
        origins.push(origin.clone());
    }
    if !origins.iter().any(|allowed| allowed == &origin) {
        return Err(HubError::Forbidden);
    }
    origins.sort();
    origins.dedup();
    // RP id is the request origin's full host: strict (no parent domains),
    // always registrable, and distinct per scheme/host surface. Passkeys are
    // origin-bound, so the intranet origin and loopback dev origins hold
    // separate registrations by design.
    let core = WebauthnCore::new_unsafe_experts_only(
        "Remuda",
        host,
        origins.clone(),
        CHALLENGE_TTL,
        Some(false),
        Some(false),
    );
    Ok(Ceremony {
        core,
        rp_id: host.to_string(),
        origins: origins.iter().map(Url::to_string).collect(),
    })
}

impl Ceremony {
    /// Stable per-RP user handle: one operator account per RP, so a fixed
    /// v5 UUID keeps excludeCredentials and userHandle stable across keys.
    fn user_id(rp_id: &str) -> Uuid {
        Uuid::new_v5(&USER_NAMESPACE, rp_id.as_bytes())
    }
}

/// Rebuild a WebAuthn core for the finish call with exactly the origins the
/// matching start offered.
fn replay_core(rp_id: &str, origins: &[String]) -> Result<WebauthnCore, HubError> {
    let parsed: Vec<Url> = origins
        .iter()
        .map(|origin| Url::parse(origin))
        .collect::<Result<_, _>>()
        .map_err(|_| HubError::Internal("stored origin unparseable".into()))?;
    Ok(WebauthnCore::new_unsafe_experts_only(
        "Remuda",
        rp_id,
        parsed,
        CHALLENGE_TTL,
        Some(false),
        Some(false),
    ))
}

/// Every ceremony failure is the same 401 to the caller: no enumeration
/// signal distinguishes "unknown credential" from "bad signature".
fn ceremony_failed(error: webauthn_rs_core::error::WebauthnError) -> HubError {
    tracing::warn!(%error, "passkey ceremony failed");
    HubError::Unauthenticated
}

fn register_extensions() -> Option<RequestRegistrationExtensions> {
    Some(RequestRegistrationExtensions {
        cred_protect: Some(CredProtect {
            credential_protection_policy: CredentialProtectionPolicy::UserVerificationRequired,
            enforce_credential_protection_policy: Some(false),
        }),
        uvm: Some(true),
        cred_props: Some(true),
        min_pin_length: None,
        hmac_create_secret: None,
    })
}

fn authenticate_extensions() -> Option<RequestAuthenticationExtensions> {
    Some(RequestAuthenticationExtensions {
        appid: None,
        uvm: Some(true),
        hmac_get_secret: None,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegisterStartBody {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegisterFinishBody {
    challenge_id: String,
    attestation: RegisterPublicKeyCredential,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginStartBody {
    /// `"conditional"` requests autofill mediation; anything else shows the
    /// platform picker.
    mediation: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginFinishBody {
    challenge_id: String,
    assertion: PublicKeyCredential,
    #[serde(default)]
    device_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameBody {
    name: String,
}

async fn register_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RegisterStartBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = require_device(&state.store, &headers).await?;
    let name = clean_name(&body.name, "Passkey");
    let ceremony = ceremony(&headers, &state.config)?;
    let user_id = Ceremony::user_id(&ceremony.rp_id);
    // Filter credentials already registered with this RP so the browser does
    // not offer the authenticator a duplicate ceremony (UX only).
    let exclude = state
        .store
        .passkey_credential_ids()
        .await?
        .iter()
        .filter_map(|id| serde_json::from_str(id).ok())
        .collect::<Vec<_>>();
    let builder = ceremony
        .core
        .new_challenge_register_builder(user_id.as_bytes(), USER_NAME, USER_DISPLAY)
        .map_err(ceremony_failed)?
        .attestation(AttestationConveyancePreference::None)
        .credential_algorithms(COSEAlgorithm::secure_algs())
        // Discoverable is the product promise: login asks for no user handle.
        .require_resident_key(true)
        .authenticator_attachment(None)
        .user_verification_policy(UserVerificationPolicy::Required)
        .reject_synchronised_authenticators(false)
        .exclude_credentials(Some(exclude))
        .hints(None)
        .extensions(register_extensions());
    let options = ceremony
        .core
        .generate_challenge_register(builder)
        .map_err(ceremony_failed)?;
    let challenge_id = random_token();
    state.challenges.put(
        challenge_id.clone(),
        Challenge {
            state: ChallengeState::Register(options.1),
            rp_id: ceremony.rp_id,
            origins: ceremony.origins,
            device_id: Some(device.id),
            name: Some(name),
            created: Instant::now(),
        },
    )?;
    Ok(Json(envelope(&challenge_id, options.0)))
}

async fn register_finish(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RegisterFinishBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = require_device(&state.store, &headers).await?;
    let challenge = state.challenges.take(&body.challenge_id)?;
    let ChallengeState::Register(reg) = challenge.state else {
        return Err(HubError::Unauthenticated);
    };
    if challenge.device_id.as_deref() != Some(device.id.as_str()) {
        return Err(HubError::Unauthenticated);
    }
    let core = replay_core(&challenge.rp_id, &challenge.origins)?;
    let credential: Credential = core
        .register_credential(&body.attestation, &reg, None)
        .map_err(ceremony_failed)?;
    let credential_id = to_b64url(&credential.cred_id)
        .ok_or_else(|| HubError::Internal("credential id unrenderable".into()))?;
    let public_key = serde_json::to_string(&credential)
        .map_err(|err| HubError::Internal(format!("credential encode: {err}")))?;
    let transports =
        serde_json::to_string(&credential.transports).unwrap_or_else(|_| "null".into());
    let aaguid = aaguid_of(&credential);
    let name = challenge.name.unwrap_or_else(|| "Passkey".into());
    let created_by = device.id.clone();
    let record = state
        .store
        .insert_passkey(
            credential_id,
            public_key,
            i64::from(credential.counter),
            transports,
            name,
            aaguid,
            created_by,
        )
        .await
        .map_err(|err| match err {
            StoreError::DuplicateCredential => {
                HubError::Conflict("credential already registered".into())
            }
            other => HubError::Store(other),
        })?;
    state
        .store
        .append_audit(
            record.created_by.clone(),
            "passkey.register".into(),
            Some(record.id.clone()),
            json!({ "name": record.name }),
        )
        .await?;
    Ok(Json(passkey_json(
        &record,
        Some(record.created_by.as_str()),
    )))
}

async fn login_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<LoginStartBody>>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let ceremony = ceremony(&headers, &state.config)?;
    let conditional = body
        .and_then(|Json(value)| value.mediation)
        .is_some_and(|value| value == "conditional");
    let builder = ceremony
        .core
        // Empty allow list: the authenticator must discover the credential.
        .new_challenge_authenticate_builder(Vec::new(), Some(UserVerificationPolicy::Required))
        .map_err(ceremony_failed)?
        .extensions(authenticate_extensions())
        .allow_backup_eligible_upgrade(false)
        .hints(None);
    let (mut options, auth) = ceremony
        .core
        .generate_challenge_authenticate(builder)
        .map_err(ceremony_failed)?;
    if conditional {
        options.mediation = Some(Mediation::Conditional);
    }
    let challenge_id = random_token();
    state.challenges.put(
        challenge_id.clone(),
        Challenge {
            state: ChallengeState::Login(auth),
            rp_id: ceremony.rp_id,
            origins: ceremony.origins,
            device_id: None,
            name: None,
            created: Instant::now(),
        },
    )?;
    Ok(Json(envelope(&challenge_id, options)))
}

async fn login_finish(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LoginFinishBody>,
) -> Result<Response, HubError> {
    require_origin(&headers, &state.config)?;
    let challenge = state.challenges.take(&body.challenge_id)?;
    let ChallengeState::Login(mut auth) = challenge.state else {
        return Err(HubError::Unauthenticated);
    };
    // The asserted user handle must be present and equal this RP's operator.
    let expected_user = Ceremony::user_id(&challenge.rp_id);
    if body.assertion.get_user_unique_id() != Some(expected_user.as_bytes()) {
        return Err(HubError::Unauthenticated);
    }
    let credential_id =
        to_b64url(body.assertion.raw_id.clone()).ok_or(HubError::Unauthenticated)?;
    let record = state
        .store
        .passkey_by_credential_id(&credential_id)
        .await?
        .ok_or(HubError::Unauthenticated)?;
    let mut stored: Credential = serde_json::from_str(&record.public_key)
        .map_err(|err| HubError::Internal(format!("stored credential decode: {err}")))?;
    // Restrict the ceremony to the single asserted credential.
    auth.set_allowed_credentials(vec![stored.clone()]);
    let core = replay_core(&challenge.rp_id, &challenge.origins)?;
    let result = core
        .authenticate_credential(&body.assertion, &auth)
        .map_err(ceremony_failed)?;
    if result.cred_id().as_slice() != stored.cred_id.as_slice() {
        return Err(HubError::Unauthenticated);
    }
    // Counter may only advance; core enforces the clone check when either the
    // stored or asserted counter is nonzero.
    if result.counter() > stored.counter {
        stored.counter = result.counter();
    }
    stored.backup_state = result.backup_state();
    if result.backup_eligible() {
        stored.backup_eligible = true;
    }
    let public_key = serde_json::to_string(&stored)
        .map_err(|err| HubError::Internal(format!("credential encode: {err}")))?;
    state
        .store
        .touch_passkey(record.id.clone(), public_key, i64::from(stored.counter))
        .await?;

    // Mint the exact same device session as POST /v1/login.
    let device_name = clean_name(&body.device_name.unwrap_or_default(), &record.name);
    let token = random_token();
    let hash = hash_secret(&token)?;
    let prefix = token_prefix(&token)
        .ok_or_else(|| HubError::Internal("generated device token is not indexable".into()))?
        .to_owned();
    let device = state
        .store
        .insert_device_as(device_name, hash, prefix, "human".to_string(), None)
        .await?;
    state
        .store
        .append_audit(
            device.id.clone(),
            "passkey.login".into(),
            Some(record.id.clone()),
            json!({ "passkeyName": record.name }),
        )
        .await?;
    let response_body = json!({
        "deviceId": device.id,
        "token": token,
        "name": device.name,
    });
    let cookie = device_cookie(&token, state.config.cookie_secure);
    Ok((
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(response_body),
    )
        .into_response())
}

async fn list_passkeys(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = require_device(&state.store, &headers).await?;
    let records = state.store.list_passkeys().await?;
    let items = records
        .iter()
        .map(|record| passkey_json(record, Some(device.id.as_str())))
        .collect::<Vec<_>>();
    Ok(Json(json!({ "items": items })))
}

async fn rename_passkey(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<RenameBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = require_device(&state.store, &headers).await?;
    let name = clean_name(&body.name, "Passkey");
    let record = state
        .store
        .rename_passkey(id.clone(), name)
        .await?
        .ok_or(HubError::NotFound)?;
    state
        .store
        .append_audit(
            device.id,
            "passkey.rename".into(),
            Some(record.id.clone()),
            json!({ "name": record.name }),
        )
        .await?;
    Ok(Json(passkey_json(&record, None)))
}

async fn delete_passkey(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = require_device(&state.store, &headers).await?;
    let ok = state.store.delete_passkey(id.clone()).await?;
    if !ok {
        return Err(HubError::NotFound);
    }
    state
        .store
        .append_audit(device.id, "passkey.delete".into(), Some(id), json!({}))
        .await?;
    Ok(Json(json!({ "ok": true })))
}

fn envelope<T: serde::Serialize>(challenge_id: &str, options: T) -> Value {
    json!({
        "challengeId": challenge_id,
        "options": options,
    })
}

fn passkey_json(record: &PasskeyRecord, current_device_id: Option<&str>) -> Value {
    json!({
        "id": record.id,
        "name": record.name,
        "createdAt": record.created_at,
        "lastUsedAt": record.last_used_at,
        "thisDevice": current_device_id.is_some_and(|id| record.created_by == id),
    })
}

fn aaguid_of(credential: &Credential) -> Option<String> {
    match &credential.attestation.metadata {
        AttestationMetadata::Packed { aaguid } | AttestationMetadata::Tpm { aaguid, .. } => {
            Some(aaguid.to_string())
        }
        _ => None,
    }
}

fn to_b64url<T: serde::Serialize>(value: T) -> Option<String> {
    match serde_json::to_value(value) {
        Ok(Value::String(text)) => Some(text),
        _ => None,
    }
}

fn clean_name(raw: &str, fallback: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.chars().take(MAX_NAME_LEN).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn core_for(rp_id: &str, origins: Vec<Url>) -> WebauthnCore {
        WebauthnCore::new_unsafe_experts_only(
            "Remuda",
            rp_id,
            origins,
            CHALLENGE_TTL,
            Some(false),
            Some(false),
        )
    }

    fn placeholder_state() -> RegistrationState {
        let core = core_for("127.0.0.1", vec![Url::parse("http://127.0.0.1:1").unwrap()]);
        let builder = core
            .new_challenge_register_builder(Uuid::new_v4().as_bytes(), USER_NAME, USER_DISPLAY)
            .unwrap()
            .attestation(AttestationConveyancePreference::None)
            .credential_algorithms(COSEAlgorithm::secure_algs())
            .require_resident_key(true)
            .user_verification_policy(UserVerificationPolicy::Required);
        core.generate_challenge_register(builder).unwrap().1
    }

    #[test]
    fn clean_name_trims_falls_back_and_caps_length() {
        assert_eq!(clean_name("  ", "fallback"), "fallback");
        assert_eq!(clean_name(" key ", "x"), "key");
        assert_eq!(clean_name(&"a".repeat(100), "x").len(), MAX_NAME_LEN);
    }

    #[test]
    fn user_id_is_stable_per_rp_and_distinct_between_rps() {
        let local = Ceremony::user_id("127.0.0.1");
        assert_eq!(local, Ceremony::user_id("127.0.0.1"));
        assert_eq!(local, Uuid::new_v5(&USER_NAMESPACE, b"127.0.0.1"));
        assert_ne!(local, Ceremony::user_id("localhost"));
    }

    #[test]
    fn challenge_store_is_single_use_and_ttl_bounded() {
        let store = ChallengeStore::default();
        store
            .put(
                "k".into(),
                Challenge {
                    state: ChallengeState::Register(placeholder_state()),
                    rp_id: "127.0.0.1".into(),
                    origins: vec!["http://127.0.0.1:1".into()],
                    device_id: None,
                    name: None,
                    created: Instant::now(),
                },
            )
            .unwrap();
        assert!(store.take("k").is_ok());
        assert!(matches!(store.take("k"), Err(HubError::Unauthenticated)));
        store
            .put(
                "old".into(),
                Challenge {
                    state: ChallengeState::Register(placeholder_state()),
                    rp_id: "x".into(),
                    origins: vec![],
                    device_id: None,
                    name: None,
                    created: Instant::now() - CHALLENGE_TTL - Duration::from_secs(1),
                },
            )
            .unwrap();
        assert!(matches!(store.take("old"), Err(HubError::Unauthenticated)));
    }

    #[test]
    fn ceremony_rejects_non_loopback_http_and_unlisted_origins() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.allowed_origins = vec![];
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://10.0.0.1:8080"),
        );
        assert!(matches!(
            ceremony(&headers, &config),
            Err(HubError::Forbidden)
        ));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:8080"),
        );
        let built = ceremony(&headers, &config).expect("loopback origin accepted");
        assert_eq!(built.rp_id, "127.0.0.1");
        assert!(
            built
                .core
                .get_allowed_origins()
                .iter()
                .any(|url| url.as_str() == "http://127.0.0.1:8080/")
        );
    }

    #[test]
    fn ceremony_uses_allowlist_for_https_origin() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = HubConfig::for_test(dir.path().to_path_buf());
        config.allowed_origins = vec!["https://hub.example.net".into()];
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://other.example.net"),
        );
        assert!(matches!(
            ceremony(&headers, &config),
            Err(HubError::Forbidden)
        ));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://hub.example.net"),
        );
        let built = ceremony(&headers, &config).expect("allowlisted origin accepted");
        assert_eq!(built.rp_id, "hub.example.net");
        assert_eq!(
            Ceremony::user_id(&built.rp_id),
            Uuid::new_v5(&USER_NAMESPACE, b"hub.example.net")
        );
    }
}
