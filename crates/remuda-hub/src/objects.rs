//! D-027 / D-027b: attachment staging — images and arbitrary files.
//!
//! A browser uploads bytes here, gets back an `obj_…` id, and puts only that
//! id on the `instance.send` command. The Node then pulls the bytes back over
//! HTTP and materializes them next to the instance. Bytes never travel in a
//! command frame: those are capped at 1 MiB and the prompt at 64 KiB.
//!
//! Images (PNG, JPEG, GIF, WebP) are magic-byte sniffed as in the D-027 MVP.
//! Every other file is accepted with its declared `Content-Type` (D-027b,
//! 2026-09-15): bytes are never executed, GET serves non-images with an
//! `attachment` disposition plus `nosniff`, and the original filename is
//! sanitised before it is echoed anywhere.

use crate::auth::{require_origin, verify_secret};
use crate::store::{ObjectRecord, StoreError};
use crate::{AppState, HubError};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use remuda_protocol::hubnode::{
    AttachmentKind, extension_for_media_type, sanitize_attachment_name,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// Slack above the configured per-file ceiling, so an oversized upload fails
/// with our `RESOURCE_LIMIT` message rather than an opaque 413 from the body
/// layer.
const BODY_LIMIT_SLACK: usize = 1024 * 1024;
/// Total live staged bytes per instance. Sized for eight 25 MiB files (D-027b).
pub const MAX_INSTANCE_BYTES: i64 = 256 * 1024 * 1024;
/// Attachments per `instance.send`. Enforced where the command is built; kept
/// here so both ends quote the same number.
pub const MAX_ATTACHMENTS_PER_SEND: usize = 8;
/// Staging lifetime. Must exceed a typical Node offline window, because a
/// command queued for an offline Node is only pulled once it reconnects.
pub const OBJECT_TTL_SECONDS: i64 = 24 * 60 * 60;
/// Longest accepted declared MIME string.
const MAX_MEDIA_TYPE_LEN: usize = 255;

pub fn routes(max_object_bytes: usize) -> Router<AppState> {
    Router::new()
        .route("/v1/objects", post(upload))
        .route("/v1/objects/{id}", get(download))
        .layer(DefaultBodyLimit::max(max_object_bytes + BODY_LIMIT_SLACK))
}

/// Allowed image media types, keyed by the extension used for the on-disk
/// internal name.
///
/// The sniffed type wins: a caller's `Content-Type` only has to agree with it.
fn sniff_image(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
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
    let declared = media_type_essence(declared);
    if declared.is_empty() || declared == "application/octet-stream" {
        // No useful claim; the sniff stands on its own.
        return true;
    }
    if sniffed == "image/jpeg" {
        return matches!(declared.as_str(), "image/jpeg" | "image/jpg");
    }
    declared == sniffed
}

/// Lowercased MIME essence (`Image/PNG; charset=…` -> `image/png`).
fn media_type_essence(raw: &str) -> String {
    raw.split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Validate a declared media type for a non-image upload: essence only,
/// well-formed tokens, length-capped. We never trust parameters and never
/// store them. `application/octet-stream` (and an empty claim) stand for an
/// unknown binary.
fn accepted_file_media_type(raw: &str) -> Result<String, String> {
    let essence = media_type_essence(raw);
    if essence.is_empty() || essence == "application/octet-stream" {
        return Ok("application/octet-stream".to_owned());
    }
    if essence.len() > MAX_MEDIA_TYPE_LEN {
        return Err(format!(
            "media type is longer than {MAX_MEDIA_TYPE_LEN} bytes"
        ));
    }
    let Some((main, sub)) = essence.split_once('/') else {
        return Err(format!("media type {essence} is not type/subunit"));
    };
    let valid_token = |part: &str| {
        !part.is_empty()
            && part.chars().all(|c| {
                c.is_ascii_alphanumeric()
                    || matches!(c, '!' | '#' | '$' | '&' | '-' | '^' | '_' | '.' | '+' | '~')
            })
    };
    if !valid_token(main) || !valid_token(sub) {
        return Err(format!("media type {essence} contains invalid characters"));
    }
    Ok(essence)
}

/// Query for [`upload`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadQuery {
    /// Instance to stage this attachment for.
    instance_id: String,
    /// Original filename, sanitised server-side (D-027b). Optional: without
    /// it the object gets a derived `<obj_id>.<ext>` name.
    #[serde(default)]
    name: Option<String>,
}

/// `POST /v1/objects?instanceId=ins_…[&name=report.pdf]` — stage one
/// attachment for a later `instance.send`.
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
    let max_bytes = state.config.attachment_max_bytes;
    if body.len() > max_bytes {
        return Err(HubError::BadRequest(format!(
            "RESOURCE_LIMIT: attachment is {} bytes; the limit is {max_bytes}",
            body.len()
        )));
    }
    let declared = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    // Images keep the D-027 magic-byte contract; everything else is accepted
    // with a validated declared type and never sniffed into a privileged type.
    let (media_type, extension) = if let Some((media_type, extension)) = sniff_image(&body) {
        if !declared_matches(declared, media_type) {
            return Err(HubError::BadRequest(format!(
                "Content-Type {declared} disagrees with the sniffed type {media_type}"
            )));
        }
        (media_type.to_owned(), extension.to_owned())
    } else {
        let media_type = accepted_file_media_type(declared).map_err(HubError::BadRequest)?;
        // A file that merely *claims* image/* but carries no image magic is
        // not an image: delivering it as one would hand a browser a renderer.
        let media_type = if media_type.starts_with("image/") {
            "application/octet-stream".to_owned()
        } else {
            media_type
        };
        let extension = extension_for_media_type(&media_type).to_owned();
        (media_type, extension)
    };

    // The original name is user metadata: sanitise, do not trust. An absent
    // or blank name falls back to the derived `<obj_id>.<ext>`; a non-empty
    // name that fails sanitisation (path separators, control characters,
    // oversize) is a 400 — a legitimate browser File name is always a bare
    // basename, so accepting a hostile value as null would hide a caller bug.
    let original_name = match query
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        None => None,
        Some(name) => Some(
            sanitize_attachment_name(name)
                .ok_or_else(|| HubError::BadRequest(format!("invalid attachment name {name}")))?,
        ),
    };

    let digest = crate::config::sha256_hex(&body);
    let record = state
        .store
        .insert_object(crate::store::NewObject {
            instance_id: instance_id.clone(),
            host_id: instance.host_id.clone(),
            media_type: media_type.clone(),
            extension,
            original_name: original_name.clone(),
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
        kind = %record.kind,
        media_type = %record.media_type,
        original_name = ?record.original_name,
        instance_id = %record.instance_id,
        device_id = %device.id,
        "attachment.uploaded"
    );
    Ok(Json(json!({
        "objectId": record.object_id,
        "kind": record.kind,
        "mediaType": record.media_type,
        "size": record.byte_len,
        "digest": record.digest,
        "name": record.original_name,
        "storedName": record.stored_name,
        "instanceId": record.instance_id,
        "expiresAt": record.expires_at,
    })))
}

/// Name a send manifest entry should carry: the sanitised original name when
/// one exists, otherwise the derived internal name.
fn manifest_name(object: &ObjectRecord) -> String {
    object
        .original_name
        .clone()
        .unwrap_or_else(|| object.stored_name.clone())
}

/// Normalize and check the `attachments` array on an `instance.send` payload.
///
/// Runs before the command is queued so a bad reference fails the send at the
/// HTTP boundary, where the caller can still act on it, rather than on the
/// Node after the command has been accepted. Each referenced object must
/// exist, be unexpired, and already be bound to this very instance — a
/// caller cannot attach another session's file by quoting its id.
///
/// The metadata written back is the Hub's own, not the caller's: media type,
/// kind, digest, size and name come from the stored row, so the Node
/// materializes what was actually accepted at upload time.
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
    // (objectId, anchor) persisted so remuda_attachments_list reports the
    // same token numbers the prompt carries.
    let mut anchor_tags: Vec<(String, i64)> = Vec::new();
    for (position, entry) in entries.into_iter().enumerate() {
        // Older clients omit `index`; the 1-based array position then stands
        // in (the manifest is ordered by token appearance).
        let index = entry
            .get("index")
            .and_then(Value::as_u64)
            .map(|n| n as i64)
            .filter(|n| *n >= 1)
            .unwrap_or(position as i64 + 1);
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
        anchor_tags.push((object.object_id.clone(), index));
        resolved.push(json!({
            "objectId": object.object_id,
            "kind": object.kind,
            "mediaType": object.media_type,
            "name": manifest_name(&object),
            "size": object.byte_len,
            "digest": object.digest,
            "index": index,
        }));
    }
    state.store.tag_object_anchors(anchor_tags).await?;
    payload["attachments"] = json!(resolved);
    Ok(())
}

/// `GET /v1/objects/{id}` — read staged bytes back.
///
/// Two callers are allowed, and each is bound to the object's instance:
/// the operator device that can already see the instance, and the Node
/// hosting it. A host credential therefore cannot read another host's
/// attachments.
///
/// Images are served inline so the composer's thumbnails and download links
/// both work; non-images always carry `attachment` disposition with the
/// sanitised filename, plus `nosniff`, so the browser never renders one as
/// HTML or script (D-027b).
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
    let disposition = match AttachmentKind::from_media_type(&object.media_type) {
        AttachmentKind::Image => "inline".to_owned(),
        AttachmentKind::File => {
            // Filename is already sanitised; strip quotes/backslashes defensively
            // so the header value itself cannot break.
            let file_name = manifest_name(&object).replace(['"', '\\', '\r', '\n'], "_");
            format!("attachment; filename=\"{file_name}\"")
        }
    };
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, object.media_type.clone()),
            (header::CONTENT_DISPOSITION, disposition),
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
    fn sniff_recognises_the_image_allowlist_and_nothing_else() {
        assert_eq!(
            sniff_image(b"\x89PNG\r\n\x1a\nrest").unwrap().0,
            "image/png"
        );
        assert_eq!(
            sniff_image(&[0xFF, 0xD8, 0xFF, 0xE0]).unwrap().0,
            "image/jpeg"
        );
        assert_eq!(sniff_image(b"GIF89a....").unwrap().0, "image/gif");
        assert_eq!(
            sniff_image(b"RIFF\0\0\0\0WEBPVP8 ").unwrap().0,
            "image/webp"
        );
        assert!(sniff_image(b"%PDF-1.7").is_none());
        assert!(sniff_image(b"<svg xmlns=").is_none());
        assert!(
            sniff_image(b"GIF").is_none(),
            "a truncated header is not a match"
        );
        assert!(sniff_image(b"RIFF\0\0\0\0WAVE").is_none());
    }

    #[test]
    fn declared_type_must_agree_with_the_sniffed_image() {
        assert!(declared_matches("image/png", "image/png"));
        assert!(declared_matches("image/png; charset=binary", "image/png"));
        assert!(declared_matches("IMAGE/PNG", "image/png"));
        assert!(declared_matches("image/jpg", "image/jpeg"));
        assert!(declared_matches("", "image/png"));
        assert!(declared_matches("application/octet-stream", "image/gif"));
        assert!(!declared_matches("image/png", "image/gif"));
        assert!(!declared_matches("text/html", "image/png"));
    }

    #[test]
    fn arbitrary_file_types_are_accepted_as_essence() {
        assert_eq!(
            accepted_file_media_type("application/pdf").unwrap(),
            "application/pdf"
        );
        assert_eq!(
            accepted_file_media_type("text/plain; charset=utf-8").unwrap(),
            "text/plain"
        );
        assert_eq!(
            accepted_file_media_type("Application/JSON").unwrap(),
            "application/json"
        );
        assert_eq!(
            accepted_file_media_type("").unwrap(),
            "application/octet-stream"
        );
        assert_eq!(
            accepted_file_media_type("application/octet-stream").unwrap(),
            "application/octet-stream"
        );
        assert_eq!(
            accepted_file_media_type("video/mp4; codecs=avc1").unwrap(),
            "video/mp4"
        );
    }

    #[test]
    fn malformed_or_hostile_file_types_are_rejected() {
        assert!(accepted_file_media_type("text").is_err());
        assert!(
            accepted_file_media_type("text/html;").is_ok(),
            "parameters are dropped"
        );
        assert!(accepted_file_media_type("text/ht ml").is_err());
        assert!(accepted_file_media_type(&format!("text/{}", "a".repeat(260))).is_err());
    }

    #[test]
    fn filenames_are_sanitised_for_the_manifest() {
        assert_eq!(
            sanitize_attachment_name("Q3 report.pdf").as_deref(),
            Some("Q3 report.pdf")
        );
        assert_eq!(sanitize_attachment_name("../../etc/passwd"), None);
        assert_eq!(sanitize_attachment_name("a\\b.pdf"), None);
        assert_eq!(sanitize_attachment_name("evil\0.pdf"), None);
        assert_eq!(sanitize_attachment_name("  "), None);
    }
}
