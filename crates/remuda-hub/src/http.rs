//! REST surface used by the Web client.

use crate::AppState;
use crate::auth::{device_cookie, hash_secret, require_device, require_origin};
use crate::error::HubError;
use crate::store::CommandRecord;
use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginBody {
    bootstrap_token: String,
    #[serde(default = "default_device_name")]
    device_name: String,
    #[serde(default)]
    device_kind: Option<String>,
}

fn default_device_name() -> String {
    "device".to_string()
}

#[derive(Deserialize)]
pub struct InstanceListQuery {
    #[serde(rename = "hostId")]
    host_id: Option<String>,
    #[serde(default, rename = "includeHistory")]
    include_history: bool,
}

#[derive(Deserialize)]
pub struct CreateInstanceBody {
    #[serde(rename = "hostId")]
    host_id: Option<String>,
    #[serde(rename = "workspaceId")]
    workspace_id: Option<String>,
    #[serde(default = "default_kind")]
    kind: String,
    #[serde(default = "default_driver")]
    driver: String,
    model: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    /// Host-absolute claude executable for this session.
    #[serde(default, rename = "binaryPath")]
    binary_path: Option<String>,
    /// Expected digest of `binaryPath`; the Node refuses a mismatch.
    #[serde(default, rename = "binarySha256")]
    binary_sha256: Option<String>,
    #[serde(rename = "providerProfileId")]
    provider_profile_id: Option<String>,
    #[serde(rename = "permissionMode")]
    permission_mode: Option<String>,
    title: Option<String>,
    prompt: Option<String>,
    /// Live instance name (stored as title when title is omitted).
    #[serde(default)]
    name: Option<String>,
    /// Working directory recorded on the instance workspace / spec.
    #[serde(default)]
    cwd: Option<String>,
    /// Worktree name created by `remuda worktree create`.
    #[serde(default)]
    worktree: Option<String>,
    #[serde(default, rename = "requiredCapabilities")]
    required_capabilities: Option<Value>,
    #[serde(default)]
    placement: Option<Value>,
    #[serde(default)]
    delegation: Option<String>,
    #[serde(default, rename = "settingsOverlayPath")]
    settings_overlay_path: Option<String>,
    #[serde(default, rename = "claudeConfigDir")]
    claude_config_dir: Option<String>,
    #[serde(default, rename = "maxBudgetUsd")]
    max_budget_usd: Option<serde_json::Value>,
    /// Native effort selection persisted on the instance spec.
    #[serde(default)]
    effort: Option<Value>,
}

fn default_kind() -> String {
    "claude".into()
}
fn default_driver() -> String {
    "claude-print".into()
}

#[derive(Deserialize)]
pub struct CommandBody {
    #[serde(rename = "commandId")]
    command_id: Option<String>,
    #[serde(default = "default_operation")]
    operation: String,
    #[serde(default)]
    payload: Value,
    #[serde(rename = "idempotencyKey")]
    idempotency_key: Option<String>,
}

fn default_operation() -> String {
    "instance.send".into()
}

/// `POST /v1/instances/{id}/resume` body (D-026).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeBody {
    /// `structured` keeps the parent's driver; `terminal` continues in claude-pty.
    #[serde(default = "default_resume_mode")]
    mode: String,
    /// Optional first prompt for the resumed session.
    #[serde(default)]
    prompt: Option<String>,
}

fn default_resume_mode() -> String {
    "structured".into()
}

/// How long one (instance, mode) resume stays idempotent.
const RESUME_IDEMPOTENCY_WINDOW: Duration = Duration::from_secs(60);

/// Exited instances older than this cannot be resumed.
///
/// Claude prunes its own transcripts, and a `--resume` against a pruned
/// session starts an empty conversation that merely looks continuous. The
/// cutoff keeps that failure visible instead of silent (D-026).
const RESUME_MAX_AGE_DAYS: i64 = 30;

/// REST routes (login, hosts, instances, journal). Placement/fleet merge later.
pub fn routes() -> Router<crate::AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/login", post(login))
        .route("/v1/worktrees", get(list_worktrees).post(create_worktree))
}

#[derive(Deserialize)]
pub struct JournalQuery {
    #[serde(rename = "afterSeq")]
    after_seq: Option<String>,
}

#[derive(Deserialize)]
pub struct WorktreeListQuery {
    #[serde(rename = "hostId")]
    host_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorktreeBody {
    host_id: Option<String>,
    workspace_id: Option<String>,
    name: String,
    #[serde(default)]
    base: Option<String>,
}

const WORKTREE_RPC_TIMEOUT: Duration = Duration::from_secs(60);

/// `GET /healthz`
pub async fn healthz() -> Json<Value> {
    Json(json!({ "ok": true }))
}

/// `POST /v1/login` — bootstrap access code → device token + cookie.
///
/// D-018: the access code pairs **devices** only — it never enrolls a Node —
/// and it expires after `bootstrap_ttl_hours`, rotatable via
/// `remuda hub rotate-bootstrap`.
///
/// It is deliberately *not* one-shot per device name. `deviceName` is
/// caller-supplied and unauthenticated, so refusing a repeat name stops no
/// attacker (they pick another name) while breaking legitimate repeat logins:
/// `HubClient` holds its device token in memory only, so every CLI invocation
/// and the `--with-dispatcher` combined mode re-login under a fixed name.
/// Genuine one-shot-per-device needs the client to persist its device token
/// first; until then TTL plus rotation is the enforceable half of A2.
pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> Result<Response, HubError> {
    require_origin(&headers, &state.config)?;
    if !crate::config::secret_eq(&body.bootstrap_token, &state.config.bootstrap_token) {
        return Err(HubError::Unauthenticated);
    }
    if !crate::auth::bootstrap_within_ttl(&state.config.data_dir, state.config.bootstrap_ttl_hours)
    {
        tracing::warn!("bootstrap access code expired; rotate with `remuda hub rotate-bootstrap`");
        return Err(HubError::Unauthenticated);
    }
    let token = crate::config::random_token();
    let hash = hash_secret(&token)?;
    let prefix = crate::auth::token_prefix(&token)
        .ok_or_else(|| HubError::Internal("generated device token is not indexable".into()))?
        .to_owned();
    let kind = body.device_kind.unwrap_or_else(|| "human".into());
    if !matches!(kind.as_str(), "human" | "bot") {
        return Err(HubError::BadRequest(
            "deviceKind must be human or bot; instance credentials are Hub-issued".into(),
        ));
    }
    let device = state
        .store
        .insert_device_as(body.device_name, hash, prefix, kind, None)
        .await?;
    let cookie = device_cookie(&token, state.config.cookie_secure);
    let body = json!({
        "deviceId": device.id,
        "token": token,
        "name": device.name,
    });
    Ok((StatusCode::OK, [(header::SET_COOKIE, cookie)], Json(body)).into_response())
}

/// `POST /v1/hosts/enroll-token` — mint a single-use Node enroll token (D-018).
///
/// Requires an authenticated device. The plaintext is returned once and only
/// its Argon2 hash is stored.
pub async fn mint_enroll_token(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = require_device(&state.store, &headers).await?;
    let token = crate::config::random_token();
    let hash = hash_secret(&token)?;
    let ttl_minutes = state.config.enroll_token_ttl_minutes.max(1);
    let expires_at = rfc3339_in(i64::try_from(ttl_minutes).unwrap_or(60).saturating_mul(60));
    let id = state
        .store
        .insert_enroll_token(
            hash,
            crate::auth::token_prefix(&token).map(str::to_string),
            device.id.clone(),
            expires_at.clone(),
        )
        .await?;
    tracing::info!(
        enroll_token_id = %id,
        device = %device.id,
        "minted node enroll token"
    );
    Ok(Json(json!({
        "enrollTokenId": id,
        "token": token,
        "expiresAt": expires_at,
    })))
}

/// RFC3339 UTC `secs` from now.
fn rfc3339_in(secs: i64) -> String {
    let t = time::OffsetDateTime::now_utc() + time::Duration::seconds(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond()
    )
}

/// `GET /v1/hosts`
pub async fn list_hosts(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, HubError> {
    crate::agent_scope::require_operator(&state, &headers).await?;
    let items: Vec<Value> = crate::placement::hosts_with_live_links(&state)
        .await?
        .iter()
        .map(crate::registry::host_view)
        .collect();
    Ok(Json(json!({ "items": items, "nextCursor": null })))
}

/// `GET /v1/instances`
pub async fn list_instances(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<InstanceListQuery>,
) -> Result<Json<Value>, HubError> {
    crate::agent_scope::require_operator(&state, &headers).await?;
    let mut items = state.store.list_instances(query.host_id).await?;
    if !query.include_history {
        items.retain(|instance| instance.last_error.as_deref() != Some("host-lost"));
    }
    Ok(Json(json!({ "items": items, "nextCursor": null })))
}

/// `GET /v1/instances/:id`
pub async fn get_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(instance_id): Path<String>,
) -> Result<Json<Value>, HubError> {
    crate::agent_scope::require_instance_read(&state, &headers, &instance_id).await?;
    let instance = state
        .store
        .get_instance(instance_id)
        .await?
        .ok_or(HubError::NotFound)?;
    Ok(Json(
        serde_json::to_value(instance).map_err(|err| HubError::Internal(err.to_string()))?,
    ))
}

#[derive(Deserialize)]
pub struct DeleteInstanceQuery {
    /// `force=1` stops a live Instance first instead of refusing.
    #[serde(default)]
    force: Option<u8>,
}

/// `DELETE /v1/instances/:id` — permanently remove a session.
///
/// Human and Bot devices only (the agent-route middleware already refuses
/// agents, and `require_operator` refuses them again at the handler).
/// A live Instance is refused with `409` unless `?force=1`, which stops it and
/// settles it as `exited` before removal. Deleting an Instance that is already
/// gone returns `404`, so a repeated `DELETE` is idempotent rather than an
/// error the UI has to special-case.
///
/// Removal covers the Hub's own record — journal, commands, interactions,
/// fleet membership — and asks the owning Node to purge its per-instance data
/// directory (launch artifacts, pty logs). The agent's native transcripts
/// under the user's home are never touched.
pub async fn delete_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(instance_id): Path<String>,
    Query(query): Query<DeleteInstanceQuery>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = crate::agent_scope::require_operator(&state, &headers).await?;
    let instance = state
        .store
        .get_instance(instance_id.clone())
        .await?
        .ok_or(HubError::NotFound)?;
    let force = query.force == Some(1);
    let live = !matches!(instance.lifecycle.as_str(), "exited" | "failed" | "closed");

    if live {
        if !force {
            return Err(HubError::Conflict(format!(
                "instance is {}; stop it first or retry with ?force=1",
                instance.lifecycle
            )));
        }
        stop_before_delete(&state, &instance).await?;
    }

    // Ask the Node to drop its own copy first. A Node that is offline or has
    // never heard of the instance must not block the delete: the Hub row is
    // what the user asked to remove, and the Node reconciles on reconnect.
    let purge = match state
        .nodes
        .call(
            &instance.host_id,
            "instance.purge",
            json!({ "instanceId": instance_id }),
            // The Node waits for a just-closed driver to finish exiting before
            // it can remove the directory, so allow more than an RPC round trip.
            Duration::from_secs(10),
        )
        .await
    {
        Ok(Some(response)) if response.get("error").is_some() => {
            tracing::warn!(
                %instance_id,
                host_id = %instance.host_id,
                response = %response,
                "node rejected instance.purge; deleting the hub record anyway"
            );
            "node-rejected"
        }
        Ok(Some(_)) => "purged",
        Ok(None) => {
            tracing::info!(
                %instance_id,
                host_id = %instance.host_id,
                "node offline at delete; its instance directory is purged on reconnect"
            );
            "node-offline"
        }
        Err(error) => {
            tracing::warn!(
                %instance_id,
                host_id = %instance.host_id,
                %error,
                "instance.purge failed; deleting the hub record anyway"
            );
            "purge-failed"
        }
    };

    // The audit row outlives the journal it describes, so write it first.
    state
        .store
        .append_audit(
            device.id.clone(),
            "instance.delete".into(),
            Some(instance_id.clone()),
            json!({
                "hostId": instance.host_id,
                "lifecycle": instance.lifecycle,
                "forced": force,
                "nodePurge": purge,
            }),
        )
        .await
        .map_err(map_store)?;

    let deleted = state
        .store
        .delete_instance(instance_id.clone())
        .await
        .map_err(map_store)?;
    if !deleted {
        return Err(HubError::NotFound);
    }
    tracing::info!(%instance_id, device_id = %device.id, forced = force, "instance deleted");
    Ok(Json(json!({
        "deleted": true,
        "instanceId": instance_id,
        "nodePurge": purge,
    })))
}

/// Stop a live Instance so it can be deleted, settling it as `exited`.
///
/// Best effort by design: a Node that is gone can never answer, and refusing
/// to delete in that case is exactly the wedge this endpoint exists to clear.
/// The row is projected to `exited` either way.
async fn stop_before_delete(
    state: &AppState,
    instance: &crate::store::InstanceRecord,
) -> Result<(), HubError> {
    let (command, created) = state
        .store
        .queue_command(
            None,
            Some(instance.instance_id.clone()),
            instance.host_id.clone(),
            "instance.close".into(),
            json!({ "instanceId": instance.instance_id, "origin": "human" }),
            None,
        )
        .await
        .map_err(map_store)?;
    if created {
        let live = state.nodes.kind_of(&instance.host_id).await.is_some();
        // A close the Node refuses or never receives must not fail the delete.
        if let Err(error) = forward_if_online(state, command, live).await {
            tracing::warn!(
                instance_id = %instance.instance_id,
                %error,
                "close before delete did not settle; deleting anyway"
            );
        }
    }
    if state
        .store
        .settle_instance_exited(instance.instance_id.clone(), "deleted-by-operator".into())
        .await
        .map_err(map_store)?
    {
        crate::ws::publish_hub_diagnostic(
            state,
            &instance.instance_id,
            "deleted_by_operator",
            "instance stopped for deletion",
        )
        .await;
    }
    Ok(())
}

/// `POST /v1/instances` — index + forward `instance.create` when the Node is online.
/// Map a wire driver name to the kind whose allowlist applies.
///
/// An unknown or kind-polymorphic driver falls back to the claude table, which
/// is the widest — the Node re-validates with the real driver, so guessing
/// narrow here would reject flags that would actually have worked.
fn driver_kind_for_args(driver: &str) -> remuda_protocol::DriverKind {
    use remuda_protocol::DriverKind;
    match driver {
        "codex-appserver" => DriverKind::CodexAppserver,
        "grok-acp" => DriverKind::GrokAcp,
        "agy-print" => DriverKind::AgyPrint,
        _ => DriverKind::ClaudePrint,
    }
}

/// Fold the chosen host's launch defaults into the spec.
///
/// Session values REPLACE the host default rather than concatenating. Merging
/// two arg lists would produce duplicate flags, which the allowlist refuses —
/// so a host default and a session arg together would fail the launch for a
/// caller who did nothing wrong.
fn merge_host_launch_defaults(
    spec: &mut serde_json::Map<String, Value>,
    host: &crate::store::HostRecord,
    body: &CreateInstanceBody,
) {
    if body.args.is_empty()
        && let Some(args) = host.default_launch_args.as_ref().filter(|a| !a.is_empty())
    {
        spec.insert("args".into(), json!(args));
    }
    if !spec.contains_key("binaryPath")
        && let Some(path) = host
            .claude_binary_path
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    {
        spec.insert("binaryPath".into(), json!(path));
    }
}

pub async fn create_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateInstanceBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = crate::agent_scope::caller(&state, &headers).await?;
    let title = body.title.clone().or(body.name.clone());
    let workspace_id = body.workspace_id.clone();
    let mut spec = json!({
        "kind": body.kind,
        "driver": body.driver,
        "model": body.model,
        "args": body.args,
        "providerProfileId": body.provider_profile_id,
        "permissionMode": body.permission_mode,
        "workspaceId": workspace_id,
        "prompt": body.prompt,
        "title": title,
        "name": body.name,
        "cwd": body.cwd,
        "worktree": body.worktree,
        "requiredCapabilities": body.required_capabilities,
    });
    if let Some(obj) = spec.as_object_mut() {
        if let Some(delegation) = &body.delegation {
            obj.insert("delegation".into(), json!(delegation));
        }
        if let Some(path) = &body.settings_overlay_path {
            obj.insert("settingsOverlayPath".into(), json!(path));
        }
        if let Some(path) = &body.claude_config_dir {
            obj.insert("claudeConfigDir".into(), json!(path));
        }
        if let Some(budget) = &body.max_budget_usd {
            obj.insert("maxBudgetUsd".into(), budget.clone());
        }
        if let Some(effort) = &body.effort {
            obj.insert("effort".into(), effort.clone());
        }
        if let Some(path) = body
            .binary_path
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            obj.insert("binaryPath".into(), json!(path));
        }
        if let Some(digest) = body
            .binary_sha256
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            obj.insert("binarySha256".into(), json!(digest));
        }
    }
    // Fail fast on a bad flag rather than making the caller wait for the Node
    // to refuse it. Same table the Node uses; the Node re-checks regardless.
    if !body.args.is_empty() {
        remuda_driver::validate_launch_args(driver_kind_for_args(&body.driver), &body.args)
            .map_err(|error| HubError::BadRequest(error.to_string()))?;
    }
    let placement =
        crate::placement::Placement::from_value(body.placement.as_ref(), body.host_id.as_deref())?;
    let place_spec = crate::placement::PlaceSpec::from_json(&spec);
    let hosts = crate::placement::pick_hosts(&state, &placement, &place_spec).await?;
    let mut reasons = Vec::new();
    let mut provider_reasons = Vec::new();
    let mut chosen = None;
    for host in hosts {
        if let Some(obj) = spec.as_object_mut() {
            obj.insert("hostId".into(), json!(host.host_id));
            merge_host_launch_defaults(obj, &host, &body);
        }
        match crate::providers::resolve_and_attach(&state, &host, &mut spec).await {
            Ok(()) => {
                chosen = Some(host);
                break;
            }
            Err(HubError::Unsatisfiable {
                reasons: host_reasons,
            }) => {
                reasons.extend(host_reasons);
            }
            Err(HubError::ProviderNotConfigured {
                reasons: host_reasons,
            }) => {
                provider_reasons.extend(host_reasons);
            }
            Err(err) => return Err(err),
        }
    }
    let host = chosen.ok_or_else(|| {
        if provider_reasons.is_empty() {
            HubError::Unsatisfiable {
                reasons: if reasons.is_empty() {
                    vec!["placement returned no host".into()]
                } else {
                    reasons
                },
            }
        } else {
            HubError::ProviderNotConfigured {
                reasons: provider_reasons,
            }
        }
    })?;
    crate::agent_scope::prepare_create(
        &state,
        &headers,
        &device,
        &host.host_id,
        &body.driver,
        &mut spec,
    )
    .await?;
    let (instance, command) = crate::placement::spawn_on_host(
        &state,
        &host,
        crate::placement::SpawnRequest {
            kind: body.kind,
            driver: body.driver,
            workspace_id,
            title,
            prompt: body.prompt,
            spec,
            operation: "instance.create",
            idempotency_key: None,
        },
    )
    .await?;
    Ok(Json(
        json!({ "instance": instance, "command": command, "hostId": host.host_id }),
    ))
}

/// `POST /v1/instances/:id/resume` — continue an exited session (D-026).
///
/// Resume never revives the exited process. It creates a new Instance on the
/// same host and workspace whose driver launches `claude --resume <sessionId>`
/// with the parent's provider, permission and model settings, so the old
/// instance keeps its history and the new one keeps the conversation.
pub async fn resume_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(instance_id): Path<String>,
    Json(body): Json<ResumeBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    // Resuming spends host capacity and starts a native process, so it stays a
    // human/bot action; an Agent asking to resume its own parent would escape
    // the scope its instance credential was issued for (D-017).
    crate::agent_scope::require_operator(&state, &headers).await?;
    let parent = state
        .store
        .get_instance(instance_id.clone())
        .await?
        .ok_or(HubError::NotFound)?;
    let mode = match body.mode.as_str() {
        "structured" => ResumeMode::Structured,
        "terminal" => ResumeMode::Terminal,
        other => {
            return Err(HubError::BadRequest(format!(
                "unknown resume mode {other}; expected structured or terminal"
            )));
        }
    };
    if parent.kind != "claude" {
        return Err(HubError::Conflict(format!(
            "resume supports Claude sessions; this one is {}",
            parent.kind
        )));
    }
    let session_id = parent.native_session_id.clone().ok_or_else(|| {
        HubError::Conflict(
            "this session never reported a native session id, so there is no transcript to resume"
                .into(),
        )
    })?;
    if let Some(age) = resume_age_days(&parent.updated_at)
        && age > RESUME_MAX_AGE_DAYS
    {
        return Err(HubError::Conflict(format!(
            "session last ran {age} days ago; transcripts older than {RESUME_MAX_AGE_DAYS} days are not resumable"
        )));
    }
    let driver = mode.driver(&parent.driver);
    let idempotency_key = format!(
        "resume:{instance_id}:{}:{}",
        mode.as_str(),
        resume_window_index()
    );
    if let Some(existing) = state
        .store
        .find_resume_child(instance_id.clone(), driver.to_string(), session_id.clone())
        .await?
    {
        return Ok(Json(json!({
            "instance": existing,
            "hostId": existing.host_id,
            "mode": mode.as_str(),
            "replayed": true,
        })));
    }

    let mut spec = parent.spec_for_resume();
    if let Some(object) = spec.as_object_mut() {
        object.insert("driver".into(), json!(driver));
        object.insert("resumeSessionId".into(), json!(session_id));
        object.insert("resumedFrom".into(), json!(instance_id));
        object.insert("parentInstanceId".into(), json!(instance_id));
        object.insert("hostId".into(), json!(parent.host_id));
        object.insert("prompt".into(), json!(body.prompt));
        // The child reports its own identity; inheriting the parent's would
        // make a stale id look freshly observed.
        object.remove("nativeSessionId");
        object.remove("nativeTranscriptPath");
    }
    let host = state
        .store
        .get_host(parent.host_id.clone())
        .await?
        .ok_or(HubError::NotFound)?;
    if state.nodes.kind_of(&host.host_id).await.is_none() {
        return Err(HubError::HostOffline {
            host_id: host.host_id.clone(),
        });
    }
    let host = crate::store::Store::with_live_link(host, true);
    crate::providers::resolve_and_attach(&state, &host, &mut spec).await?;

    let title = parent
        .title
        .clone()
        .map(|title| format!("{title} (resumed)"))
        .or_else(|| Some("resumed session".to_string()));
    let (instance, command) = crate::placement::spawn_on_host(
        &state,
        &host,
        crate::placement::SpawnRequest {
            kind: parent.kind.clone(),
            driver: driver.to_string(),
            workspace_id: parent.workspace_id.clone(),
            title,
            prompt: body.prompt.clone(),
            spec,
            operation: "instance.resume",
            idempotency_key: Some(idempotency_key),
        },
    )
    .await?;
    // Both ends of the link are journaled so history shows where a
    // conversation continued, and where a resumed one came from.
    let now = crate::config::now_rfc3339();
    journal_resume_link(
        &state,
        &parent.host_id,
        &instance_id,
        "resumed-into",
        &instance.instance_id,
        &session_id,
        &now,
    )
    .await;
    journal_resume_link(
        &state,
        &parent.host_id,
        &instance.instance_id,
        "resumed-from",
        &instance_id,
        &session_id,
        &now,
    )
    .await;
    Ok(Json(json!({
        "instance": instance,
        "command": command,
        "hostId": host.host_id,
        "mode": mode.as_str(),
        "replayed": false,
    })))
}

/// Which driver a resume target launches.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ResumeMode {
    /// Keep the parent's driver (structured transcript).
    Structured,
    /// Continue the same native session inside a real terminal.
    Terminal,
}

impl ResumeMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Structured => "structured",
            Self::Terminal => "terminal",
        }
    }

    /// Structured keeps whatever the parent used; terminal always needs a PTY.
    ///
    /// D-028 §5.6: a `shell-pty` parent resumes as `shell-pty` on **both**
    /// modes. That driver now has a semantic resume (a new PTY with the
    /// harness's own resume flag prefilled), and unification means it already
    /// has both projections — so there is nothing for the mode to choose
    /// between, and routing it to `claude-pty` would move a native-carrier
    /// session onto herdr just because the user clicked a different button.
    /// This is the second of the two spots §5.6 names as "necessarily 409
    /// today": the driver's was `CapabilityUnsupported`, and this was a Hub
    /// that could not name the driver at all.
    fn driver(self, parent_driver: &str) -> &'static str {
        if parent_driver == "shell-pty" {
            return "shell-pty";
        }
        match self {
            Self::Terminal => "claude-pty",
            Self::Structured if parent_driver == "claude-pty" => "claude-pty",
            Self::Structured => "claude-print",
        }
    }
}

/// Whole days between `updated_at` and now; `None` when unparseable.
fn resume_age_days(updated_at: &str) -> Option<i64> {
    let then =
        time::OffsetDateTime::parse(updated_at, &time::format_description::well_known::Rfc3339)
            .ok()?;
    let seconds = (time::OffsetDateTime::now_utc() - then).whole_seconds();
    Some(seconds / 86_400)
}

/// Bucket wall-clock time so a double-click reuses one idempotency key.
fn resume_window_index() -> u64 {
    let window = RESUME_IDEMPOTENCY_WINDOW.as_secs().max(1);
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() / window)
        .unwrap_or(0)
}

/// Journal one side of the parent/child resume link, best effort.
#[allow(clippy::too_many_arguments)]
async fn journal_resume_link(
    state: &AppState,
    host_id: &str,
    instance_id: &str,
    relation: &str,
    other_instance_id: &str,
    session_id: &str,
    now: &str,
) {
    let event = json!({
        "kind": "lifecycle",
        "observedAt": now,
        "payload": {
            "type": "native",
            "topic": "session",
            "nativeName": relation,
            "nativeId": { "state": "known", "value": session_id },
            "status": { "state": "known", "value": relation },
            "relatedIds": { "instanceId": other_instance_id },
            "dataRef": null,
            "severity": "info",
            "affectsCompletion": false,
        },
    });
    if let Err(error) = state
        .store
        .append_journal(host_id.to_string(), instance_id.to_string(), None, event)
        .await
    {
        tracing::warn!(%error, instance_id, relation, "resume link journal append failed");
    }
}

/// `POST /v1/instances/:id/commands`
pub async fn post_command(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(instance_id): Path<String>,
    Json(body): Json<CommandBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = crate::agent_scope::caller(&state, &headers).await?;
    let instance = state
        .store
        .get_instance(instance_id.clone())
        .await?
        .ok_or(HubError::NotFound)?;
    let mut payload = body.payload;
    if payload.is_null() {
        payload = json!({});
    }
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("instanceId".to_string(), json!(instance_id.clone()));
    }
    if !payload.is_object() {
        return Err(HubError::BadRequest(
            "command payload must be an object".into(),
        ));
    }
    crate::agent_scope::authorize_command(
        &state,
        &headers,
        &device,
        &instance,
        &body.operation,
        &mut payload,
    )
    .await?;
    if body.operation == "instance.send" {
        crate::objects::validate_send_attachments(&state, &device, &instance, &mut payload).await?;
    }
    let (command, created) = state
        .store
        .queue_command(
            body.command_id,
            Some(instance_id.clone()),
            instance.host_id.clone(),
            body.operation,
            payload,
            body.idempotency_key,
        )
        .await
        .map_err(map_store)?;
    if command.operation == "instance.configure" {
        state
            .store
            .patch_instance_configure(instance_id, command.payload.clone())
            .await
            .map_err(map_store)?;
    }
    if !created {
        return Ok(Json(json!({ "command": command, "replayed": true })));
    }
    let live = state.nodes.kind_of(&instance.host_id).await.is_some();
    let command = forward_if_online(&state, command, live).await?;
    Ok(Json(json!({ "command": command, "replayed": false })))
}

/// `GET /v1/worktrees` — catalog from the Node (`worktree.list`).
pub async fn list_worktrees(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<WorktreeListQuery>,
) -> Result<Json<Value>, HubError> {
    crate::agent_scope::require_operator(&state, &headers).await?;
    let host = pick_worktree_host(&state, query.host_id.as_deref()).await?;
    match call_node(
        &state,
        &host.host_id,
        "worktree.list",
        json!({ "hostId": host.host_id }),
    )
    .await
    {
        Ok(body) => {
            let mut value = body;
            if let Some(obj) = value.as_object_mut() {
                obj.entry("hostId".to_string())
                    .or_insert_with(|| json!(host.host_id));
                obj.entry("items".to_string()).or_insert_with(|| json!([]));
                obj.entry("nextCursor".to_string()).or_insert(Value::Null);
            }
            Ok(Json(value))
        }
        Err(HubError::Unsatisfiable { .. }) => Ok(Json(json!({
            "hostId": host.host_id,
            "workspaceRoot": null,
            "items": [],
            "nextCursor": null
        }))),
        Err(err) => Err(err),
    }
}

/// `POST /v1/worktrees` — `git worktree add -b wt/<name>/…` on the Node.
pub async fn create_worktree(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateWorktreeBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    require_device(&state.store, &headers).await?;
    if body.name.is_empty() {
        return Err(HubError::BadRequest("worktree name required".into()));
    }
    let host = pick_worktree_host(&state, body.host_id.as_deref()).await?;
    // `path` / `repo` are deliberately not forwarded: the Node picks the
    // directory under its own `<repo>/../remuda-wt` root (security-review-2 M4).
    let params = json!({
        "hostId": host.host_id,
        "name": body.name,
        "workspaceId": body.workspace_id,
        "base": body.base.as_deref().unwrap_or("main"),
    });
    let created = call_node(&state, &host.host_id, "worktree.create", params).await?;
    Ok(Json(created))
}

async fn pick_worktree_host(
    state: &AppState,
    host_id: Option<&str>,
) -> Result<crate::store::HostRecord, HubError> {
    let placement = crate::placement::Placement::from_value(None, host_id)?;
    let spec = crate::placement::PlaceSpec::from_json(&json!({}));
    crate::placement::pick_hosts(state, &placement, &spec)
        .await?
        .into_iter()
        .next()
        .ok_or(HubError::Unsatisfiable {
            reasons: vec!["no online host for worktree".into()],
        })
}

pub(crate) async fn call_node(
    state: &AppState,
    host_id: &str,
    method: &str,
    params: Value,
) -> Result<Value, HubError> {
    match state
        .nodes
        .call(host_id, method, params, WORKTREE_RPC_TIMEOUT)
        .await
    {
        Ok(Some(response)) => {
            if let Some(error) = response.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("node worktree rpc failed");
                return Err(HubError::BadRequest(message.to_string()));
            }
            Ok(response.get("result").cloned().unwrap_or(response))
        }
        Ok(None) => Err(HubError::Unsatisfiable {
            reasons: vec![format!("host {host_id} is not connected")],
        }),
        Err(err) => Err(err),
    }
}

/// `GET /v1/instances/:id/journal`
pub async fn get_journal(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(instance_id): Path<String>,
    Query(query): Query<JournalQuery>,
) -> Result<Json<Value>, HubError> {
    crate::agent_scope::require_instance_read(&state, &headers, &instance_id).await?;
    if state
        .store
        .get_instance(instance_id.clone())
        .await?
        .is_none()
    {
        return Err(HubError::NotFound);
    }
    let after = query
        .after_seq
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let (events, durable) = state.store.read_journal(instance_id.clone(), after).await?;
    Ok(Json(json!({
        "instanceId": instance_id,
        "durableSeq": durable.to_string(),
        "events": events,
    })))
}

pub(crate) async fn forward_if_online(
    state: &AppState,
    command: CommandRecord,
    host_online: bool,
) -> Result<CommandRecord, HubError> {
    if !host_online {
        return Ok(command);
    }
    let first = state
        .store
        .mark_forward_intent(command.command_id.clone())
        .await?;
    if !first {
        return state
            .store
            .get_command(command.command_id.clone())
            .await?
            .ok_or(HubError::NotFound);
    }
    let mut params = command.payload.clone();
    let object = params
        .as_object_mut()
        .ok_or_else(|| HubError::BadRequest("command payload must be an object".into()))?;
    object.insert("commandId".into(), json!(command.command_id));
    if command.operation == "instance.create" {
        let instance_id = command.instance_id.clone().ok_or(HubError::NotFound)?;
        let token = crate::agent_scope::instance_token(state.store.clone(), instance_id).await?;
        // Credential travels only on the authenticated carrier, never in the
        // persisted command, returned HTTP body, or launch recipe.
        object.insert("agentCredential".into(), json!({"token":token}));
        params = crate::providers::with_launch_secret(state, &command.host_id, params).await?;
    }
    match state
        .nodes
        .call(
            &command.host_id,
            &command.operation,
            params,
            Duration::from_millis(state.config.command_accept_timeout_ms.max(1)),
        )
        .await
    {
        Ok(Some(response)) if node_accepted(&response, &command.command_id) => {
            let accepted = state
                .store
                .mark_accepted(command.command_id.clone())
                .await?;
            schedule_create_settlement_watch(state, &accepted);
            Ok(accepted)
        }
        Ok(Some(response)) if response.get("error").is_some() => {
            tracing::warn!(
                command_id = %command.command_id,
                response = %response,
                "node did not durably accept command; will not resend"
            );
            if let Some(settled) =
                settle_stop_for_unknown_instance(state, &command, &response).await?
            {
                return Ok(settled);
            }
            fail_unaccepted_create(state, &command, node_rpc_error_message(&response)).await
        }
        Ok(Some(response)) => {
            tracing::warn!(
                command_id = %command.command_id,
                response = %response,
                "node did not durably accept command; will not resend"
            );
            state
                .store
                .get_command(command.command_id.clone())
                .await?
                .ok_or(HubError::NotFound)
        }
        Ok(None) => {
            tracing::debug!(command_id = %command.command_id, "node offline at forward; not resent");
            state
                .store
                .get_command(command.command_id.clone())
                .await?
                .ok_or(HubError::NotFound)
        }

        Err(err) => {
            tracing::warn!(error = %err, command_id = %command.command_id, "node rpc unknown; will not resend");
            // The request may still be executing on the Node (protocol §2.5:
            // never resend on a missing ACK). Mark reconciliation in progress;
            // the Node's mirrored journal is what converges this row to
            // accepted/settled even though the RPC reply was lost.
            let reconciled = state
                .store
                .mark_reconciling(command.command_id.clone())
                .await?;
            Ok(reconciled)
        }
    }
}

/// Settle a stop/close the Node cannot honour because it lost the instance.
///
/// After a Node restart the Hub still holds `running` rows the new process
/// knows nothing about. `instance.close` then fails forever and the row keeps a
/// placement slot. When the Node answers "not found", the Hub projects the row
/// to `exited` and settles the command instead of leaving it hanging.
/// Returns `Some(command)` when it took ownership of the outcome.
async fn settle_stop_for_unknown_instance(
    state: &AppState,
    command: &CommandRecord,
    response: &Value,
) -> Result<Option<CommandRecord>, HubError> {
    if !matches!(
        command.operation.as_str(),
        "instance.close" | "instance.cancel"
    ) {
        return Ok(None);
    }
    if !node_reports_unknown_instance(response) {
        return Ok(None);
    }
    let Some(instance_id) = command.instance_id.clone() else {
        return Ok(None);
    };
    tracing::warn!(
        command_id = %command.command_id,
        %instance_id,
        operation = %command.operation,
        "node does not know this instance; settling the stop as exited"
    );
    if state
        .store
        .settle_instance_exited(instance_id.clone(), "node-lost-instance".into())
        .await
        .map_err(map_store)?
    {
        crate::ws::publish_hub_diagnostic(
            state,
            &instance_id,
            "node_lost_instance",
            "stop requested for an instance the node does not know; settled as exited",
        )
        .await;
    }
    let settled = state
        .store
        .mark_settled(command.command_id.clone(), command.host_id.clone())
        .await
        .map_err(map_store)?;
    Ok(Some(settled))
}

/// True when a Node RPC error says the instance is gone (`-32004` / "not found").
fn node_reports_unknown_instance(response: &Value) -> bool {
    if response.pointer("/error/code").and_then(Value::as_i64) == Some(-32004) {
        return true;
    }
    response
        .pointer("/error/message")
        .and_then(Value::as_str)
        .is_some_and(|message| {
            let message = message.to_ascii_lowercase();
            message.contains("not found") || message.contains("unknown instance")
        })
}

async fn fail_unaccepted_create(
    state: &AppState,
    command: &CommandRecord,
    message: String,
) -> Result<CommandRecord, HubError> {
    if command.operation == "instance.create"
        && let Some(instance_id) = command.instance_id.clone()
    {
        state
            .store
            .fail_instance(instance_id, message.clone())
            .await?;
        return Err(HubError::BadRequest(message));
    }
    state
        .store
        .get_command(command.command_id.clone())
        .await?
        .ok_or(HubError::NotFound)
}

fn node_rpc_error_message(response: &Value) -> String {
    response
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| {
            response
                .pointer("/error/data/message")
                .and_then(Value::as_str)
        })
        .unwrap_or("node did not accept instance.create")
        .trim()
        .trim_start_matches("invalid request: ")
        .to_string()
}

fn node_accepted(response: &Value, command_id: &str) -> bool {
    if response.get("error").is_some() {
        return false;
    }
    let result = response.get("result").unwrap_or(response);
    if let Some(returned_id) = result.pointer("/command/commandId").and_then(Value::as_str)
        && returned_id != command_id
    {
        return false;
    }
    matches!(
        result.pointer("/command/state").and_then(Value::as_str),
        Some("accepted" | "settled")
    ) || result.get("accepted").and_then(Value::as_bool) == Some(true)
}

fn schedule_create_settlement_watch(state: &AppState, command: &CommandRecord) {
    if command.operation != "instance.create" || command.state != "accepted" {
        return;
    }
    let state = state.clone();
    let command_id = command.command_id.clone();
    let timeout = Duration::from_millis(state.config.create_settle_timeout_ms());
    tokio::spawn(async move {
        tokio::time::sleep(timeout).await;
        match state
            .store
            .mark_settlement_timed_out(command_id.clone())
            .await
        {
            Ok(Some(_)) => tracing::warn!(
                %command_id,
                timeout_ms = timeout.as_millis(),
                "create command accepted but settlement observation is overdue"
            ),
            Ok(None) => {}
            Err(error) => tracing::error!(
                %command_id,
                %error,
                "failed to record create settlement timeout"
            ),
        }
    });
}

pub(crate) fn map_store(err: crate::store::StoreError) -> HubError {
    match &err {
        crate::store::StoreError::Id(msg)
            if msg.contains("unknown host") || msg.contains("unknown provider") =>
        {
            HubError::NotFound
        }
        crate::store::StoreError::Id(msg)
            if msg.contains("reused") || msg.contains("idempotency") =>
        {
            HubError::Conflict(msg.clone())
        }
        crate::store::StoreError::Id(msg) => HubError::BadRequest(msg.clone()),
        other => HubError::Store(other.clone_as_internal()),
    }
}

impl crate::store::StoreError {
    fn clone_as_internal(&self) -> Self {
        crate::store::StoreError::Id(self.to_string())
    }
}
