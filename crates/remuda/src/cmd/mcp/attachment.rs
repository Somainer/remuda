//! D-028 §4.5: hand an attachment to the agent as an MCP content block.
//!
//! D-027 could only deliver real image bytes through `claude-print`'s base64
//! prompt block; every other harness got an absolute path in prose and had to
//! open the file itself — which codex does only when told explicitly, and grok
//! cannot do at all. An MCP tool inverts that: claude, codex and grok all
//! accept `image` content blocks in a tool result, so the same call works
//! everywhere and the path mention becomes a fallback rather than the ceiling.
//!
//! Scope is the Hub's to enforce, not this process's: the tools pass the
//! caller's own credential, and `GET /v1/attachments/...` pins every read to
//! that credential's session. An instance token reaches its own attachments
//! and nothing else. The checks in [`super::scope`] refuse the call earlier,
//! so a misconfigured operator credential cannot enumerate a session it was
//! not asked about either.

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::{
    Tool,
    args::{opt_str, required_str},
};

/// Mirrors the Hub's `MAX_INLINE_ATTACHMENT_BYTES` and `claude-print`'s
/// `MAX_INLINE_IMAGE_BYTES`. Checked here too so the refusal explains itself
/// in the agent's own transcript when the Hub is an older build.
const MAX_INLINE_ATTACHMENT_BYTES: u64 = 3_584 * 1024;

pub(super) fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "remuda_attachments_list",
            "List attachments staged for this session (objectId, mediaType, size). Call this to discover what `remuda_attachment` can fetch.",
            json!({
                "type": "object",
                "properties": {
                    "instanceId": {
                        "type": "string",
                        "description": "Session to list. Omit inside an agent session: it defaults to your own, and naming another session is refused."
                    }
                }
            }),
            |client, args| Box::pin(async move { list(client, opt_str(&args, "instanceId")).await }),
        ),
        Tool::new(
            "remuda_attachment",
            "Fetch one attachment staged for this session by objectId. Images come back as an image content block you can see directly; text comes back as text. Other media types are refused with their size and type.",
            json!({
                "type": "object",
                "required": ["objectId"],
                "properties": {
                    "objectId": {
                        "type": "string",
                        "description": "`obj_…` id, as named in the prompt's attachment mention or by remuda_attachments_list."
                    }
                }
            }),
            |client, args| {
                Box::pin(async move { fetch(client, required_str(&args, "objectId")?).await })
            },
        )
        .blocks(),
    ]
}

async fn list(client: &super::HubClient, instance_id: Option<&str>) -> Result<Value> {
    let path = match instance_id {
        Some(id) => format!("/v1/attachments?instanceId={id}"),
        None => "/v1/attachments".to_owned(),
    };
    let body = client.get(&path).await?;
    let items: Vec<Value> = body
        .get("items")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(summary).collect())
        .unwrap_or_default();
    Ok(json!({
        "instanceId": body.get("instanceId").cloned().unwrap_or(Value::Null),
        "count": items.len(),
        "items": items,
    }))
}

/// Drop `digest` and `name` from the listing: neither helps the agent decide
/// what to fetch, and the derived name only invites a path guess.
fn summary(item: &Value) -> Value {
    json!({
        "objectId": item.get("objectId").cloned().unwrap_or(Value::Null),
        "mediaType": item.get("mediaType").cloned().unwrap_or(Value::Null),
        "size": item.get("size").cloned().unwrap_or(Value::Null),
        "expiresAt": item.get("expiresAt").cloned().unwrap_or(Value::Null),
    })
}

/// Fetch one attachment and shape it into MCP content blocks.
async fn fetch(client: &super::HubClient, object_id: &str) -> Result<Value> {
    let body = client
        .get(&format!("/v1/attachments/{object_id}/content"))
        .await?;
    content_blocks(&body)
}

/// Turn one `AttachmentContent` body into the tool result.
///
/// Split from [`fetch`] so the mapping — including every refusal — is testable
/// without a Hub.
pub(super) fn content_blocks(body: &Value) -> Result<Value> {
    let object_id = body
        .get("objectId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("attachment response has no objectId"))?;
    let media_type = body
        .get("mediaType")
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream");
    let size = body.get("size").and_then(Value::as_u64).unwrap_or(0);
    if size > MAX_INLINE_ATTACHMENT_BYTES {
        bail!(
            "RESOURCE_LIMIT: attachment {object_id} is {size} bytes ({media_type}); \
             the limit is {MAX_INLINE_ATTACHMENT_BYTES}"
        );
    }
    let data = body
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("attachment {object_id} has no data"))?;
    // Trust the Hub's own count over the encoded length, but refuse anything
    // that decodes larger than the cap regardless of what `size` claimed.
    let decoded_len = decoded_len(data);
    if decoded_len > MAX_INLINE_ATTACHMENT_BYTES {
        bail!(
            "RESOURCE_LIMIT: attachment {object_id} decodes to {decoded_len} bytes \
             ({media_type}); the limit is {MAX_INLINE_ATTACHMENT_BYTES}"
        );
    }

    let block = if media_type.starts_with("image/") {
        json!({"type": "image", "data": data, "mimeType": media_type})
    } else if media_type.starts_with("text/") {
        json!({"type": "text", "text": decode_text(data, object_id, media_type)?})
    } else {
        bail!(
            "attachment {object_id} is {media_type} ({size} bytes); only image/* and text/* \
             can be delivered as content blocks"
        );
    };
    Ok(json!({ "content": [block] }))
}

/// Byte length standard base64 decodes to, without decoding.
fn decoded_len(data: &str) -> u64 {
    let len = data.len() as u64;
    let padding = data.bytes().rev().take_while(|byte| *byte == b'=').count() as u64;
    (len / 4) * 3 - padding.min(2)
}

fn decode_text(data: &str, object_id: &str, media_type: &str) -> Result<String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|error| anyhow!("attachment {object_id} is not valid base64: {error}"))?;
    String::from_utf8(bytes).map_err(|_| {
        anyhow!("attachment {object_id} is declared {media_type} but is not valid UTF-8")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(media_type: &str, data: &str, size: u64) -> Value {
        json!({
            "objectId": "obj_1",
            "instanceId": "ins_self",
            "mediaType": media_type,
            "name": "obj_1.png",
            "size": size,
            "digest": "0".repeat(64),
            "expiresAt": "2026-09-15T00:00:00.000Z",
            "encoding": "base64",
            "data": data,
        })
    }

    #[test]
    fn an_image_becomes_an_image_block_with_its_sniffed_mime() {
        let result = content_blocks(&body("image/png", "iVBORw0KGgo=", 8)).unwrap();
        let block = &result["content"][0];
        assert_eq!(block["type"], json!("image"));
        assert_eq!(block["mimeType"], json!("image/png"));
        assert_eq!(block["data"], json!("iVBORw0KGgo="));
        // The base64 is passed through untouched: re-encoding would be a
        // chance to corrupt bytes the Hub already verified.
        assert!(block.get("text").is_none());
    }

    #[test]
    fn text_is_decoded_into_a_text_block() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode("hello 附件");
        let result = content_blocks(&body("text/plain", &encoded, 13)).unwrap();
        assert_eq!(result["content"][0]["type"], json!("text"));
        assert_eq!(result["content"][0]["text"], json!("hello 附件"));
    }

    #[test]
    fn invalid_utf8_declared_as_text_is_refused_rather_than_mangled() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode([0xFF, 0xFE, 0x00]);
        let error = content_blocks(&body("text/plain", &encoded, 3)).unwrap_err();
        assert!(error.to_string().contains("not valid UTF-8"), "{error}");
    }

    #[test]
    fn an_oversize_attachment_is_refused_with_its_size_and_type() {
        let over = MAX_INLINE_ATTACHMENT_BYTES + 1;
        let error = content_blocks(&body("image/png", "iVBORw0KGgo=", over)).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("RESOURCE_LIMIT"), "{text}");
        assert!(text.contains(&over.to_string()), "{text}");
        assert!(text.contains("image/png"), "{text}");

        // A dishonest `size` does not get the payload past the cap.
        let big = "A".repeat((MAX_INLINE_ATTACHMENT_BYTES as usize + 16) / 3 * 4 + 8);
        let error = content_blocks(&body("image/png", &big, 10)).unwrap_err();
        assert!(error.to_string().contains("decodes to"), "{error}");
    }

    #[test]
    fn a_non_image_non_text_type_is_refused_with_size_and_mime() {
        let error = content_blocks(&body("application/pdf", "JVBERi0=", 512)).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("application/pdf"), "{text}");
        assert!(text.contains("512 bytes"), "{text}");
        assert!(text.contains("image/* and text/*"), "{text}");
    }

    #[test]
    fn decoded_len_matches_a_real_decode() {
        use base64::Engine as _;
        for len in 0..24usize {
            let raw = vec![0x5Au8; len];
            let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
            assert_eq!(decoded_len(&encoded), len as u64, "len {len} -> {encoded}");
        }
    }

    #[test]
    fn a_listing_keeps_only_what_the_agent_needs_to_choose() {
        let item = json!({
            "objectId": "obj_1", "instanceId": "ins_self", "mediaType": "image/png",
            "name": "obj_1.png", "size": 168, "digest": "abc", "expiresAt": "2026-09-15T00:00:00Z"
        });
        let summary = summary(&item);
        assert_eq!(summary["objectId"], json!("obj_1"));
        assert_eq!(summary["mediaType"], json!("image/png"));
        assert_eq!(summary["size"], json!(168));
        for dropped in ["digest", "name", "instanceId"] {
            assert!(summary.get(dropped).is_none(), "{dropped} leaked");
        }
    }

    /// End to end over `tools/call` against a fake Hub, which is how an agent
    /// actually reaches these: the image must arrive as an `image` block on
    /// the MCP result, not as JSON stringified into a text block.
    mod against_a_fake_hub {
        use crate::cmd::hub_client::connect_for_test;
        use crate::cmd::mcp::handle_rpc;
        use crate::cmd::test_hub::{MockHub, spawn_mock_hub, spawn_mock_hub_as};
        use serde_json::{Value, json};

        async fn call(mock: &MockHub, name: &str, arguments: Value) -> Value {
            let client = connect_for_test(format!("http://{}", mock.addr), "fixture".into())
                .expect("client");
            handle_rpc(
                &json!({"jsonrpc":"2.0", "id":1, "method":"tools/call",
                        "params":{"name":name, "arguments":arguments}}),
                &client,
            )
            .await
            .expect("response")["result"]
                .clone()
        }

        #[tokio::test]
        async fn an_image_arrives_as_an_image_block_and_text_as_text() {
            let mock = spawn_mock_hub().await;
            let result = call(&mock, "remuda_attachment", json!({"objectId":"obj_png"})).await;
            assert_eq!(result["isError"], json!(false), "{result}");
            assert_eq!(result["content"][0]["type"], json!("image"));
            assert_eq!(result["content"][0]["mimeType"], json!("image/png"));
            let data = result["content"][0]["data"].as_str().expect("data");
            // Decodes back to the PNG signature the Hub sniffed at upload.
            use base64::Engine as _;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(data)
                .expect("base64");
            assert!(decoded.starts_with(b"\x89PNG\r\n\x1a\n"), "{data}");
            assert_eq!(decoded.len(), 168);
            assert_eq!(result["content"].as_array().expect("content").len(), 1);

            let result = call(&mock, "remuda_attachment", json!({"objectId":"obj_txt"})).await;
            assert_eq!(result["isError"], json!(false), "{result}");
            assert_eq!(result["content"][0]["type"], json!("text"));
            assert_eq!(result["content"][0]["text"], json!("hello agent"));
        }

        #[tokio::test]
        async fn listing_reports_this_session_only() {
            let mock = spawn_mock_hub().await;
            let result = call(&mock, "remuda_attachments_list", json!({})).await;
            assert_eq!(result["isError"], json!(false), "{result}");
            let text = result["content"][0]["text"].as_str().expect("text");
            let body: Value = serde_json::from_str(text).expect("json");
            assert_eq!(body["instanceId"], json!("ins_test"));
            assert_eq!(body["count"], json!(2));
            assert_eq!(body["items"][0]["objectId"], json!("obj_png"));
            assert_eq!(body["items"][1]["mediaType"], json!("text/plain"));
        }

        #[tokio::test]
        async fn oversize_and_unsupported_types_are_errors_not_silent_truncation() {
            let mock = spawn_mock_hub().await;
            let result = call(&mock, "remuda_attachment", json!({"objectId":"obj_big"})).await;
            assert_eq!(result["isError"], json!(true), "{result}");
            let text = result["content"][0]["text"].as_str().expect("text");
            assert!(text.contains("RESOURCE_LIMIT"), "{text}");
            assert!(text.contains("image/png"), "{text}");
            // No bytes came back alongside the refusal.
            assert!(result["content"][0].get("data").is_none(), "{result}");

            let result = call(&mock, "remuda_attachment", json!({"objectId":"obj_pdf"})).await;
            assert_eq!(result["isError"], json!(true), "{result}");
            let text = result["content"][0]["text"].as_str().expect("text");
            assert!(text.contains("application/pdf"), "{text}");
        }

        /// An attachment staged for a different session is refused by the Hub,
        /// and the refusal survives as an MCP error rather than an empty block.
        #[tokio::test]
        async fn another_sessions_attachment_is_refused_by_the_hub() {
            let mock = spawn_mock_hub().await;
            let result = call(&mock, "remuda_attachment", json!({"objectId":"obj_other"})).await;
            assert_eq!(result["isError"], json!(true), "{result}");
            let text = result["content"][0]["text"].as_str().expect("text");
            assert!(text.contains("403") || text.contains("forbidden"), "{text}");
        }

        /// An Agent caller naming someone else's session is stopped before any
        /// Hub request: the client-side rule is the loud half of the Hub's.
        #[tokio::test]
        async fn an_agent_naming_another_session_never_reaches_the_hub() {
            let mock = spawn_mock_hub_as(
                json!({"origin":"agent", "instanceId":"ins_test", "hostId":"hst_1",
                       "children":["ins_child"]}),
            )
            .await;
            for name in ["remuda_attachment", "remuda_attachments_list"] {
                let result = call(
                    &mock,
                    name,
                    json!({"instanceId":"ins_child", "objectId":"obj_png"}),
                )
                .await;
                assert_eq!(result["isError"], json!(true), "{name}: {result}");
                let text = result["content"][0]["text"].as_str().expect("text");
                assert!(text.contains("own session"), "{name}: {text}");
            }
            // A direct child is not an exception, so only /v1/caller was hit.
            assert!(
                mock.requests
                    .lock()
                    .await
                    .iter()
                    .all(|request| request == "GET /v1/caller"),
                "{:?}",
                mock.requests.lock().await
            );

            // Its own session is allowed, explicitly or by omission.
            for arguments in [json!({}), json!({"instanceId":"ins_test"})] {
                let result = call(&mock, "remuda_attachments_list", arguments.clone()).await;
                assert_eq!(result["isError"], json!(false), "{arguments}: {result}");
            }
        }
    }
}
