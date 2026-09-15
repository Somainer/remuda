//! D-028 §4.5: session-scoped attachment reads for the in-session MCP server.
//!
//! D-027 staged attachments and gave two readers: the operator device that
//! uploaded one, and the Node hosting the instance (`GET /v1/objects/{id}`).
//! Neither is the agent. `require_operator` refuses an instance-bound
//! credential outright, so the MCP server running *inside* a session cannot
//! use that route at all — which is why D-028 needs these two.
//!
//! The scope rule is deliberately narrower than the rest of the Agent surface:
//! an instance credential reads **its own session's** attachments and nothing
//! else. Not a child's, not a parent's. An attachment is staged against one
//! `instance.send`, so there is no case where a sibling session legitimately
//! needs its bytes, and the blast radius of a leaked instance token stays at
//! the images someone already chose to hand that session.
//!
//! Additive to D-027: the staging route, its limits, and the Node pull path
//! are untouched.

use crate::auth::require_origin;
use crate::store::{Device, ObjectRecord, Store, StoreError};
use crate::{AppState, HubError};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine as _;
use remuda_protocol::InputOrigin;
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};

/// Largest attachment this route will inline as base64.
///
/// Mirrors `claude-print`'s `MAX_INLINE_IMAGE_BYTES`: past this the CLI
/// re-compresses or refuses, so delivering the bytes is not an improvement on
/// saying why they were withheld. Smaller than D-027's 5 MiB staging cap, so
/// an object can be staged and still be too large to hand to an agent — the
/// error names the size and media type precisely for that reason.
pub const MAX_INLINE_ATTACHMENT_BYTES: i64 = 3_584 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/attachments", get(list))
        .route("/v1/attachments/{objectId}/content", get(content))
}

/// Query for [`list`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListQuery {
    /// Operators must name a session; an instance credential may omit it.
    instance_id: Option<String>,
}

/// `GET /v1/attachments?instanceId=ins_…` — live attachments for one session.
///
/// The agent calls this to discover what is pending before deciding what to
/// read. Expired rows are filtered here rather than swept, matching the lazy
/// expiry the download route already uses.
async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = crate::agent_scope::caller(&state, &headers).await?;
    let instance_id = session_of(&device, query.instance_id.as_deref())?;
    state
        .store
        .get_instance(instance_id.clone())
        .await?
        .ok_or(HubError::NotFound)?;
    let objects = live_objects(
        &state.store,
        instance_id.clone(),
        crate::config::now_rfc3339(),
    )
    .await?;
    tracing::info!(
        instance_id = %instance_id,
        device_id = %device.id,
        origin = ?crate::agent_scope::origin(&device),
        count = objects.len(),
        "attachment.listed"
    );
    let items: Vec<Value> = objects.iter().map(view).collect();
    Ok(Json(json!({ "instanceId": instance_id, "items": items })))
}

/// `GET /v1/attachments/{objectId}/content` — metadata plus base64 bytes.
///
/// JSON rather than raw bytes because the one caller that needs this route is
/// building an MCP content block, which carries base64 anyway. Encoding here
/// keeps the transfer honest about its own size and lets the size refusal be
/// an ordinary structured error instead of a truncated body.
async fn content(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(object_id): Path<String>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = crate::agent_scope::caller(&state, &headers).await?;
    let object = state
        .store
        .get_object(object_id.clone())
        .await?
        .ok_or(HubError::NotFound)?;
    // Lazy expiry, as on the D-027 download route: a stale row reads as absent.
    if crate::config::now_rfc3339().as_str() >= object.expires_at.as_str() {
        return Err(HubError::NotFound);
    }
    authorize_session(&device, &object.instance_id)?;
    if object.byte_len > MAX_INLINE_ATTACHMENT_BYTES {
        return Err(HubError::BadRequest(format!(
            "RESOURCE_LIMIT: attachment {} is {} bytes ({}); the inline limit is {MAX_INLINE_ATTACHMENT_BYTES}",
            object.object_id, object.byte_len, object.media_type
        )));
    }
    let bytes = state
        .store
        .read_object_bytes(object_id)
        .await?
        .ok_or(HubError::NotFound)?;
    tracing::info!(
        object_id = %object.object_id,
        instance_id = %object.instance_id,
        media_type = %object.media_type,
        bytes = bytes.len(),
        device_id = %device.id,
        origin = ?crate::agent_scope::origin(&device),
        "attachment.read"
    );
    let mut body = view(&object);
    body["encoding"] = json!("base64");
    body["data"] = json!(base64::engine::general_purpose::STANDARD.encode(&bytes));
    Ok(Json(body))
}

/// Which session's attachments this caller may enumerate.
///
/// An instance credential is pinned to its own session: naming a different one
/// is refused rather than silently retargeted, so a confused agent learns it
/// asked for something it cannot have. An operator has no session of its own
/// and must name one.
fn session_of(device: &Device, requested: Option<&str>) -> Result<String, HubError> {
    if let Some(bound) = device.instance_id.as_deref() {
        return match requested {
            None => Ok(bound.to_owned()),
            Some(id) if id == bound => Ok(bound.to_owned()),
            Some(_) => Err(HubError::Forbidden),
        };
    }
    if crate::agent_scope::origin(device) == InputOrigin::Agent {
        return Err(HubError::Forbidden);
    }
    requested
        .map(str::to_owned)
        .ok_or_else(|| HubError::BadRequest("instanceId is required".into()))
}

/// Same rule as [`session_of`], applied to an object's own instance.
fn authorize_session(device: &Device, instance_id: &str) -> Result<(), HubError> {
    match device.instance_id.as_deref() {
        Some(bound) if bound == instance_id => Ok(()),
        Some(_) => Err(HubError::Forbidden),
        // An unrecognized device kind is an Agent with no session at all.
        None if crate::agent_scope::origin(device) == InputOrigin::Agent => {
            Err(HubError::Forbidden)
        }
        None => Ok(()),
    }
}

/// Metadata only. Bytes are added by [`content`].
fn view(object: &ObjectRecord) -> Value {
    json!({
        "objectId": object.object_id,
        "instanceId": object.instance_id,
        "kind": object.kind,
        "mediaType": object.media_type,
        // The sanitised original name when one was supplied (D-027b), else the
        // derived `<obj_id>.<ext>` name.
        "name": object.original_name.clone().unwrap_or_else(|| object.stored_name.clone()),
        "storedName": object.stored_name,
        "size": object.byte_len,
        "digest": object.digest,
        "expiresAt": object.expires_at,
        // 1-based [Image #n]/[File #n] anchor; absent for objects never
        // consumed by a numbered send (listed oldest-first in that case).
        "index": object.anchor,
    })
}

/// Unexpired attachments for one instance, oldest first.
async fn live_objects(
    store: &Store,
    instance_id: String,
    now: String,
) -> Result<Vec<ObjectRecord>, StoreError> {
    store
        .run(move |conn| {
            let mut statement = conn.prepare(
                "SELECT id, instance_id, host_id, media_type, stored_name, original_name, kind,
                        digest, byte_len, expires_at, anchor
                 FROM objects
                 WHERE instance_id = ?1 AND expires_at > ?2
                 ORDER BY COALESCE(anchor, 9223372036854775807), created_at, id",
            )?;
            let rows = statement
                .query_map(params![instance_id, now], |row| {
                    Ok(ObjectRecord {
                        object_id: row.get(0)?,
                        instance_id: row.get(1)?,
                        host_id: row.get(2)?,
                        media_type: row.get(3)?,
                        stored_name: row.get(4)?,
                        original_name: row.get(5)?,
                        kind: row.get(6)?,
                        digest: row.get(7)?,
                        byte_len: row.get(8)?,
                        expires_at: row.get(9)?,
                        anchor: row.get(10)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(kind: &str, instance: Option<&str>) -> Device {
        Device {
            id: "dev_1".into(),
            name: "fixture".into(),
            kind: kind.into(),
            instance_id: instance.map(str::to_owned),
        }
    }

    #[test]
    fn an_instance_credential_is_pinned_to_its_own_session() {
        let agent = device("agent", Some("ins_self"));
        assert_eq!(session_of(&agent, None).unwrap(), "ins_self");
        assert_eq!(session_of(&agent, Some("ins_self")).unwrap(), "ins_self");
        assert!(session_of(&agent, Some("ins_other")).is_err());
        assert!(authorize_session(&agent, "ins_self").is_ok());
        for target in ["ins_other", "ins_parent", "ins_child"] {
            assert!(
                authorize_session(&agent, target).is_err(),
                "{target} must not be readable from ins_self"
            );
        }
    }

    #[test]
    fn an_operator_names_a_session_and_an_unknown_kind_gets_none() {
        for kind in ["human", "bot"] {
            let operator = device(kind, None);
            assert_eq!(session_of(&operator, Some("ins_a")).unwrap(), "ins_a");
            assert!(session_of(&operator, None).is_err());
            assert!(authorize_session(&operator, "ins_a").is_ok());
        }
        // Unknown kinds fail closed as Agent (D-017), and an Agent with no
        // instance binding has no session to read.
        let unknown = device("mystery", None);
        assert!(session_of(&unknown, Some("ins_a")).is_err());
        assert!(authorize_session(&unknown, "ins_a").is_err());
    }

    #[test]
    fn the_inline_ceiling_is_below_the_staging_ceiling() {
        assert!(
            MAX_INLINE_ATTACHMENT_BYTES < crate::config::default_attachment_max_bytes() as i64
        );
        assert_eq!(MAX_INLINE_ATTACHMENT_BYTES, 3_670_016);
    }

    /// A send manifest tags each object with its `[Image #n]` number; the
    /// listing the in-session agent sees must come back in that order so the
    /// model resolves tokens by position.
    #[tokio::test]
    async fn listing_orders_by_send_manifest_anchor() {
        let dir = tempfile::tempdir().expect("dir");
        let store = crate::store::Store::open(dir.path()).expect("store");
        let instance = "ins_anchors".to_owned();
        let stage = |n: u8, digest: &str| crate::store::NewObject {
            instance_id: instance.clone(),
            host_id: "hst_1".into(),
            media_type: "image/png".into(),
            extension: "png".into(),
            original_name: None,
            digest: digest.to_owned(),
            bytes: vec![0x89, b'P', b'N', b'G', n],
            device_id: "dev_1".into(),
            ttl_seconds: 3600,
            instance_budget: 64 * 1024 * 1024,
        };
        let first = store
            .insert_object(stage(1, &"a".repeat(64)))
            .await
            .expect("a");
        let second = store
            .insert_object(stage(2, &"b".repeat(64)))
            .await
            .expect("b");
        // Uploaded oldest-first: first then second. The send references #2
        // first in the prompt, so tag them against token order.
        store
            .tag_object_anchors(vec![
                (second.object_id.clone(), 1),
                (first.object_id.clone(), 2),
            ])
            .await
            .expect("tag");

        let objects = live_objects(&store, instance, crate::config::now_rfc3339())
            .await
            .expect("list");
        let ordered: Vec<(String, Option<i64>)> = objects
            .iter()
            .map(|object| (object.object_id.clone(), object.anchor))
            .collect();
        assert_eq!(
            ordered,
            vec![(second.object_id, Some(1)), (first.object_id, Some(2))],
            "the manifest's token order wins over upload order"
        );
        store.close().await;
    }
}
