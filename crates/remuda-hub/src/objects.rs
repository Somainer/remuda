//! D-027: attachment staging for image passthrough.
//!
//! A browser uploads image bytes here, gets back an `obj_…` id, and puts only
//! that id on the `instance.send` command. The Node then pulls the bytes back
//! over HTTP and materializes them next to the instance. Bytes never travel in
//! a command frame: those are capped at 1 MiB and the prompt at 64 KiB.
//!
//! Deliberately narrow for the MVP — no `DELETE`, no thumbnails, no
//! server-side re-render. EXIF is stripped in the browser by re-encoding
//! through a canvas, so the Hub never decodes an image and never links an
//! image library.

use crate::auth::{require_origin, verify_secret};
use crate::store::{ObjectRecord, StoreError};
use crate::{AppState, HubError};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

/// Largest single attachment. The body limit below is deliberately a little
/// larger so an oversized upload fails with our `RESOURCE_LIMIT` message
/// rather than an opaque 413 from the body layer.
pub const MAX_OBJECT_BYTES: usize = 5 * 1024 * 1024;
/// Request body ceiling for the objects routes only.
const BODY_LIMIT: usize = 6 * 1024 * 1024;
/// Total live staged bytes per instance.
pub const MAX_INSTANCE_BYTES: i64 = 64 * 1024 * 1024;
/// Attachments per `instance.send`. Enforced where the command is built; kept
/// here so both ends quote the same number.
pub const MAX_ATTACHMENTS_PER_SEND: usize = 4;
/// Staging lifetime. Must exceed a typical Node offline window, because a
/// command queued for an offline Node is only pulled once it reconnects.
pub const OBJECT_TTL_SECONDS: i64 = 24 * 60 * 60;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/objects", post(upload))
        .route("/v1/objects/{id}", get(download))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
}

/// Allowed media types, keyed by the extension used for the on-disk name.
///
/// The sniffed type wins: a caller's `Content-Type` only has to agree with it.
fn sniff(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(("image/png", "png"));
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(("image/jpeg", "jpg"));
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(("image/gif", "gif"));
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some(("image/webp", "webp"));
    }
    None
}

/// `image/jpeg` and `image/jpg` name the same format; everything else must match.
fn declared_matches(declared: &str, sniffed: &str) -> bool {
    let declared = declared
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if declared.is_empty() || declared == "application/octet-stream" {
        // No useful claim; the sniff stands on its own.
        return true;
    }
    if sniffed == "image/jpeg" {
        return matches!(declared.as_str(), "image/jpeg" | "image/jpg");
    }
    declared == sniffed
}

/// Query for [`upload`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadQuery {
    /// Instance to stage this attachment for.
    instance_id: String,
}

/// `POST /v1/objects?instanceId=ins_…` — stage one image for a later
/// `instance.send`.
///
/// Raw body plus `Content-Type`; no multipart, so no new dependency.
///
/// The target instance arrives as a query parameter rather than the
/// `X-Remuda-Instance-Id` header the design sketched: that header already
/// means something else. `agent_scope::caller` reads it as *narrowing the
/// caller to that instance's agent*, which would turn every upload into an
/// Agent-origin request and make the route unconditionally 403. The instance
/// binds the object for quota accounting and for the Node-side read check.
async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    // Human or Bot only. An Agent-origin caller is refused outright in the
    // MVP; giving agents an upload channel needs an approval category first.
    let device = crate::agent_scope::require_operator(&state, &headers).await?;

    let instance_id = query.instance_id.trim().to_owned();
    if instance_id.is_empty() {
        return Err(HubError::BadRequest("instanceId is required".into()));
    }
    let instance = state
        .store
        .get_instance(instance_id.clone())
        .await?
        .ok_or(HubError::NotFound)?;

    if body.is_empty() {
        return Err(HubError::BadRequest("attachment body is empty".into()));
    }
    if body.len() > MAX_OBJECT_BYTES {
        return Err(HubError::BadRequest(format!(
            "RESOURCE_LIMIT: attachment is {} bytes; the limit is {MAX_OBJECT_BYTES}",
            body.len()
        )));
    }
    let Some((media_type, extension)) = sniff(&body) else {
        return Err(HubError::BadRequest(
            "attachment must be a PNG, JPEG, GIF or WebP image".into(),
        ));
    };
    let declared = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !declared_matches(declared, media_type) {
        return Err(HubError::BadRequest(format!(
            "Content-Type {declared} disagrees with the sniffed type {media_type}"
        )));
    }

    let digest = crate::config::sha256_hex(&body);
    // The stored name is derived, never caller-supplied: a fixed
    // `<obj_id>.<ext>` removes path traversal structurally (D-027).
    let record = state
        .store
        .insert_object(crate::store::NewObject {
            instance_id: instance_id.clone(),
            host_id: instance.host_id.clone(),
            media_type: media_type.to_owned(),
            extension: extension.to_owned(),
            digest,
            bytes: body.to_vec(),
            device_id: device.id.clone(),
            ttl_seconds: OBJECT_TTL_SECONDS,
            instance_budget: MAX_INSTANCE_BYTES,
        })
        .await
        .map_err(map_object_error)?;

    tracing::info!(
        object_id = %record.object_id,
        digest = %record.digest,
        bytes = record.byte_len,
        media_type = %record.media_type,
        instance_id = %record.instance_id,
        device_id = %device.id,
        "attachment.uploaded"
    );
    Ok(Json(json!({
        "objectId": record.object_id,
        "mediaType": record.media_type,
        "size": record.byte_len,
        "digest": record.digest,
        "name": record.stored_name,
        "instanceId": record.instance_id,
        "expiresAt": record.expires_at,
    })))
}

/// Normalize and check the `attachments` array on an `instance.send` payload.
///
/// Runs before the command is queued so a bad reference fails the send at the
/// HTTP boundary, where the caller can still act on it, rather than on the
/// Node after the command has been accepted. Each referenced object must
/// exist, be unexpired, and already be bound to this very instance — a
/// caller cannot attach another session's image by quoting its id.
///
/// The metadata written back is the Hub's own, not the caller's: media type
/// and name come from the stored row, so the Node materializes what was
/// actually sniffed at upload time.
pub async fn validate_send_attachments(
    state: &AppState,
    device: &crate::store::Device,
    instance: &crate::store::InstanceRecord,
    payload: &mut Value,
) -> Result<(), HubError> {
    let Some(raw) = payload.get("attachments") else {
        return Ok(());
    };
    if raw.is_null() {
        payload
            .as_object_mut()
            .map(|object| object.remove("attachments"));
        return Ok(());
    }
    let entries = raw
        .as_array()
        .ok_or_else(|| HubError::BadRequest("attachments must be an array".into()))?
        .clone();
    if entries.is_empty() {
        payload
            .as_object_mut()
            .map(|object| object.remove("attachments"));
        return Ok(());
    }
    // An Agent-origin caller has no upload channel in the MVP, so it has no
    // legitimate way to hold an object id either.
    if crate::agent_scope::origin(device) == remuda_protocol::InputOrigin::Agent {
        return Err(HubError::Forbidden);
    }
    if entries.len() > MAX_ATTACHMENTS_PER_SEND {
        return Err(HubError::BadRequest(format!(
            "RESOURCE_LIMIT: {} attachments; the limit is {MAX_ATTACHMENTS_PER_SEND} per message",
            entries.len()
        )));
    }

    let now = crate::config::now_rfc3339();
    let mut resolved = Vec::with_capacity(entries.len());
    for entry in entries {
        let object_id = entry
            .get("objectId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| HubError::BadRequest("each attachment needs an objectId".into()))?
            .to_owned();
        let object = state
            .store
            .get_object(object_id.clone())
            .await?
            .ok_or_else(|| {
                HubError::BadRequest(format!("attachment {object_id} is unknown or expired"))
            })?;
        if object.expires_at.as_str() <= now.as_str() {
            return Err(HubError::BadRequest(format!(
                "attachment {object_id} has expired; upload it again"
            )));
        }
        if object.instance_id != instance.instance_id {
            return Err(HubError::Forbidden);
        }
        resolved.push(json!({
            "objectId": object.object_id,
            "mediaType": object.media_type,
            "name": object.stored_name,
            "size": object.byte_len,
        }));
    }
    payload["attachments"] = json!(resolved);
    Ok(())
}

/// `GET /v1/objects/{id}` — read staged bytes back.
///
/// Two callers are allowed, and each is bound to the object's instance:
/// the operator device that can already see the instance, and the Node
/// hosting it. A host credential therefore cannot read another host's
/// attachments.
async fn download(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, HubError> {
    require_origin(&headers, &state.config)?;
    let object = state
        .store
        .get_object(id.clone())
        .await?
        .ok_or(HubError::NotFound)?;
    // Lazy expiry: a stale row reads as absent and is swept on the next upload.
    if crate::config::now_rfc3339().as_str() >= object.expires_at.as_str() {
        return Err(HubError::NotFound);
    }
    authorize_read(&state, &headers, &object).await?;

    let bytes = state
        .store
        .read_object_bytes(id)
        .await?
        .ok_or(HubError::NotFound)?;
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, object.media_type.clone()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", object.stored_name),
            ),
            (
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff".to_owned(),
            ),
            (header::CACHE_CONTROL, "private, no-store".to_owned()),
        ],
        bytes,
    )
        .into_response())
}

/// Accept the owning Node or a non-Agent device; refuse everything else.
async fn authorize_read(
    state: &AppState,
    headers: &HeaderMap,
    object: &ObjectRecord,
) -> Result<(), HubError> {
    if let Some(host_id) = authenticated_host(state, headers).await? {
        return if host_id == object.host_id {
            Ok(())
        } else {
            // A Node may only read attachments for instances it hosts.
            Err(HubError::Forbidden)
        };
    }
    let device = crate::agent_scope::require_operator(state, headers).await?;
    let _ = device;
    Ok(())
}

/// Resolve a Bearer token to a host, reusing the same prefix index and Argon2
/// verification the `/v1/node` handshake uses. Returns `None` when the token
/// is not a host token, so the device path can still run.
async fn authenticated_host(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<String>, HubError> {
    let Some(token) = crate::auth::presented_token(headers) else {
        return Ok(None);
    };
    Ok(state.store.find_host_by_token(token, verify_secret).await?)
}

fn map_object_error(error: StoreError) -> HubError {
    match &error {
        StoreError::Id(message) if message.starts_with("RESOURCE_LIMIT") => {
            HubError::BadRequest(message.clone())
        }
        _ => HubError::Store(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_recognises_the_allowlist_and_nothing_else() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest").unwrap().0, "image/png");
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]).unwrap().0, "image/jpeg");
        assert_eq!(sniff(b"GIF89a....").unwrap().0, "image/gif");
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 ").unwrap().0, "image/webp");
        assert!(sniff(b"%PDF-1.7").is_none());
        assert!(sniff(b"<svg xmlns=").is_none());
        assert!(sniff(b"GIF").is_none(), "a truncated header is not a match");
        assert!(sniff(b"RIFF\0\0\0\0WAVE").is_none());
    }

    #[test]
    fn declared_type_must_agree_with_the_sniffed_one() {
        assert!(declared_matches("image/png", "image/png"));
        assert!(declared_matches("image/png; charset=binary", "image/png"));
        assert!(declared_matches("IMAGE/PNG", "image/png"));
        assert!(declared_matches("image/jpg", "image/jpeg"));
        assert!(declared_matches("", "image/png"));
        assert!(declared_matches("application/octet-stream", "image/gif"));
        assert!(!declared_matches("image/png", "image/gif"));
        assert!(!declared_matches("text/html", "image/png"));
    }
}
