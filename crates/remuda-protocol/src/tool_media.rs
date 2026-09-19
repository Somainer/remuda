//! Media blocks inside tool results (D-045 §6.2 / codex-cua.md §6).
//!
//! A screenshot a computer-use tool returns is ordinary result content, not a
//! special observation: no new `ObservationKind`, bytes live in the Hub object
//! store, and what travels in the journal is only an `objectId` reference
//! ([`ContentBlock::Image`] + [`MediaBlock`]).
//!
//! Every native mapper (the transcript tailer and the live drivers) folds
//! image items through [`tool_result_content_blocks`] so the degradation shape
//! has one implementation:
//!
//! * bytes are staged through a Node-supplied [`ToolMediaStager`] (the host
//!   token object endpoint);
//! * an oversize, undecodable or unstageable image becomes a **text block**
//!   naming the media type and the byte count — never a dropped result, never a
//!   panic, and never inline base64 in an observation or the journal;
//! * a carrier with no object route supplies no stager, so every image
//!   degrades to that same text block instead of leaking bytes.

use crate::hubnode::extension_for_media_type;
use crate::{ContentBlock, Id, MediaBlock, TextBlock};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

/// Stages media bytes a tool produced into the Hub object store and returns
/// the object id the Hub minted.
///
/// Mappers call this synchronously while folding a native result; the Node's
/// implementation bridges to its host-token HTTP client. The returned id
/// becomes the block's only byte-bearing reference.
pub trait ToolMediaStager: Send + Sync + std::fmt::Debug {
    /// Per-object byte ceiling the receiver enforces; checked before bytes
    /// are sent so an oversize screenshot never leaves the process.
    fn max_bytes(&self) -> u64;

    /// Stage `bytes` under `name` and return the Hub object id.
    ///
    /// Implementations must never log the bytes themselves (D-045 Q6).
    fn stage(&self, name: &str, media_type: &str, bytes: Vec<u8>) -> Result<Id, ToolMediaError>;
}

/// Why an image could not become a [`ContentBlock::Image`]. Every variant
/// degrades to an honest text block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolMediaError {
    /// The decoded image is larger than the receiver's object ceiling.
    TooLarge {
        /// Decoded byte count the block carried.
        size: u64,
        /// The ceiling that applied.
        limit: u64,
    },
    /// The object endpoint refused the bytes or could not be reached.
    Unstageable(String),
}

/// Default media type for an image item that does not name one. CUA
/// screenshots are PNGs (`codex-cua.md` §6.1).
pub const DEFAULT_IMAGE_MEDIA_TYPE: &str = "image/png";

/// Fold the native `content` of one `tool_result` into protocol blocks.
///
/// `content` is the value of the native block's `content` field: a bare
/// string, the mixed text/image content array Anthropic-shaped transcripts and
/// stream frames carry, or absent.
///
/// Text is collected exactly as the legacy text-only mapper did — every text
/// item joined with no separator — so a result without images is
/// byte-identical to the old behaviour (a single text block, kept even when
/// empty). When images are present, the joined text leads and image-derived
/// blocks follow in array order.
pub fn tool_result_content_blocks(
    content: Option<&Value>,
    stager: Option<&dyn ToolMediaStager>,
) -> Vec<ContentBlock> {
    let Some(Value::Array(items)) = content else {
        // Bare string content (or anything non-array) keeps the legacy shape.
        let text = match content {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        return vec![ContentBlock::Text(Box::new(TextBlock { text }))];
    };

    let text = items
        .iter()
        .filter_map(text_item)
        .collect::<Vec<_>>()
        .join("");
    let mut image_blocks: Vec<ContentBlock> = Vec::new();
    let mut image_no = 0u32;
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("image") {
            continue;
        }
        image_no += 1;
        image_blocks.push(fold_image_item(item, image_no, stager));
    }

    if image_blocks.is_empty() {
        // Preserve the historical single-text-block result exactly.
        return vec![ContentBlock::Text(Box::new(TextBlock { text }))];
    }
    let mut blocks = Vec::new();
    if !text.is_empty() {
        blocks.push(ContentBlock::Text(Box::new(TextBlock { text })));
    }
    blocks.extend(image_blocks);
    blocks
}

/// The text of one content item.
///
/// Deliberately not gated on `type == "text"`: the legacy text-only mappers
/// took every item's `text` field, so parity (byte-identical text-only
/// results) means keeping that extraction for all items.
fn text_item(item: &Value) -> Option<String> {
    item.get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

/// Turn one native image item into an image reference block or the honest
/// text fallback.
fn fold_image_item(
    item: &Value,
    image_no: u32,
    stager: Option<&dyn ToolMediaStager>,
) -> ContentBlock {
    let media_type = item_media_type(item);
    let data_value = item
        .get("source")
        .and_then(|source| source.get("data"))
        .or_else(|| item.get("data"));
    let Some(data_value) = data_value else {
        let source = item
            .get("source")
            .and_then(|source| source.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("absent");
        return fallback_block(
            &media_type,
            None,
            &format!("image carried no data (source \"{source}\")"),
        );
    };
    let Value::String(raw) = data_value else {
        return fallback_block(&media_type, None, "image data is not a string");
    };
    let encoded_len = raw.len() as u64;
    let trimmed = strip_base64_whitespace(raw);
    let Ok(bytes) = STANDARD.decode(trimmed.as_bytes()) else {
        return fallback_block(&media_type, Some(encoded_len), "undecodable base64");
    };
    if bytes.is_empty() {
        return fallback_block(&media_type, Some(0), "empty image");
    }
    let size = bytes.len() as u64;
    let Some(stager) = stager else {
        return fallback_block(&media_type, Some(size), "no object route to stage bytes");
    };
    let limit = stager.max_bytes();
    if size > limit {
        return fallback_block(
            &media_type,
            Some(size),
            &format!("over the {limit}-byte object limit"),
        );
    }
    let name = image_name(&media_type, image_no);
    match stager.stage(&name, &media_type, bytes) {
        Ok(object_id) => ContentBlock::Image(Box::new(MediaBlock {
            object_id,
            media_type,
            name: Some(name),
            anchor: None,
            size: Some(size),
        })),
        Err(ToolMediaError::TooLarge { size, limit }) => fallback_block(
            &media_type,
            Some(size),
            &format!("over the {limit}-byte object limit"),
        ),
        Err(ToolMediaError::Unstageable(reason)) => fallback_block(
            &media_type,
            Some(size),
            &format!("staging failed: {reason}"),
        ),
    }
}

/// Media type claimed by a native image item, tolerating the Anthropic
/// (`source.media_type`) and MCP (`mimeType`) spellings.
fn item_media_type(item: &Value) -> String {
    let claimed = item
        .get("source")
        .and_then(|source| source.get("media_type").or_else(|| source.get("mediaType")))
        .or_else(|| item.get("media_type"))
        .or_else(|| item.get("mimeType"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    claimed.map_or_else(|| DEFAULT_IMAGE_MEDIA_TYPE.to_owned(), ToOwned::to_owned)
}

/// Base64 payload tolerates embedded whitespace/newlines (some encoders wrap
/// lines); whitespace is stripped before the strict decode.
fn strip_base64_whitespace(raw: &str) -> String {
    raw.chars().filter(|ch| !ch.is_ascii_whitespace()).collect()
}

/// The block's synthetic screenshot name, `screen-<n>.<ext>` (codex-cua.md
/// §6.1); the extension follows the claimed media type.
fn image_name(media_type: &str, image_no: u32) -> String {
    format!("screen-{image_no}.{}", extension_for_media_type(media_type))
}

/// One text block honestly naming the image that could not be attached.
fn fallback_block(media_type: &str, size: Option<u64>, reason: &str) -> ContentBlock {
    let size = size.map_or_else(|| "unknown size".to_owned(), |n| format!("{n} bytes"));
    ContentBlock::Text(Box::new(TextBlock {
        text: format!("[image not attached: {media_type}, {size} — {reason}]"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default, Debug)]
    struct FakeStager {
        limit: u64,
        fail: Option<&'static str>,
        staged: Mutex<Vec<(String, String, usize)>>,
    }

    impl ToolMediaStager for FakeStager {
        fn max_bytes(&self) -> u64 {
            self.limit
        }
        fn stage(
            &self,
            name: &str,
            media_type: &str,
            bytes: Vec<u8>,
        ) -> Result<Id, ToolMediaError> {
            if let Some(reason) = self.fail {
                return Err(ToolMediaError::Unstageable(reason.into()));
            }
            if bytes.len() as u64 > self.limit {
                return Err(ToolMediaError::TooLarge {
                    size: bytes.len() as u64,
                    limit: self.limit,
                });
            }
            self.staged
                .lock()
                .unwrap()
                .push((name.to_owned(), media_type.to_owned(), bytes.len()));
            Id::new("obj").map_err(|_| ToolMediaError::Unstageable("id".into()))
        }
    }

    fn png_pixel() -> String {
        // 1x1 PNG, 67 bytes.
        STANDARD.encode([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00])
    }

    #[test]
    fn text_only_is_one_joined_text_block() {
        let content = serde_json::json!(["one", {"type": "text", "text": "two"}]);
        // The first bare string is not a typed text item and is ignored,
        // matching the journal's legacy filter.
        let blocks = tool_result_content_blocks(Some(&content), None);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::Text(text) => assert_eq!(text.text, "two"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bare_string_and_absent_content_keep_legacy_shape() {
        let blocks = tool_result_content_blocks(Some(&Value::String("ok".into())), None);
        match &blocks[0] {
            ContentBlock::Text(text) => assert_eq!(text.text, "ok"),
            other => panic!("{other:?}"),
        }
        let blocks = tool_result_content_blocks(None, None);
        match &blocks[0] {
            ContentBlock::Text(text) => assert_eq!(text.text, ""),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn mixed_text_and_image_becomes_text_plus_reference() {
        let content = serde_json::json!([
            {"type": "text", "text": "seen:"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": png_pixel()}},
        ]);
        let stager = FakeStager {
            limit: 1024,
            ..FakeStager::default()
        };
        let blocks = tool_result_content_blocks(Some(&content), Some(&stager));
        assert_eq!(blocks.len(), 2);
        match &blocks[0] {
            ContentBlock::Text(text) => assert_eq!(text.text, "seen:"),
            other => panic!("{other:?}"),
        }
        match &blocks[1] {
            ContentBlock::Image(media) => {
                assert_eq!(media.media_type, "image/png");
                assert_eq!(media.name.as_deref(), Some("screen-1.png"));
                assert_eq!(media.size, Some(10));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(stager.staged.lock().unwrap().len(), 1);
    }

    #[test]
    fn image_only_emits_one_reference() {
        let content = serde_json::json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": png_pixel()}},
        ]);
        let stager = FakeStager {
            limit: 1024,
            ..FakeStager::default()
        };
        let blocks = tool_result_content_blocks(Some(&content), Some(&stager));
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], ContentBlock::Image(_)));
    }

    #[test]
    fn malformed_base64_degrades_to_named_text_block() {
        let content = serde_json::json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "@@not base64@@"}},
        ]);
        let stager = FakeStager {
            limit: 1024,
            ..FakeStager::default()
        };
        let blocks = tool_result_content_blocks(Some(&content), Some(&stager));
        match &blocks[0] {
            ContentBlock::Text(text) => {
                assert!(text.text.contains("image/png"), "{}", text.text);
                assert!(text.text.contains("undecodable base64"), "{}", text.text);
            }
            other => panic!("{other:?}"),
        }
        assert!(stager.staged.lock().unwrap().is_empty());
    }

    #[test]
    fn missing_stager_degrades_without_inlining_bytes() {
        let content = serde_json::json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": png_pixel()}},
        ]);
        let blocks = tool_result_content_blocks(Some(&content), None);
        match &blocks[0] {
            ContentBlock::Text(text) => {
                assert!(text.text.contains("image/png"), "{}", text.text);
                assert!(text.text.contains("no object route"), "{}", text.text);
                assert!(!text.text.contains(&png_pixel()));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn oversize_degrades_with_named_limit() {
        let content = serde_json::json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": png_pixel()}},
        ]);
        let stager = FakeStager {
            limit: 4,
            ..FakeStager::default()
        };
        let blocks = tool_result_content_blocks(Some(&content), Some(&stager));
        match &blocks[0] {
            ContentBlock::Text(text) => {
                assert!(text.text.contains("4-byte object limit"), "{}", text.text)
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn mcp_shape_and_mimetype_are_accepted() {
        let content = serde_json::json!([
            {"type": "image", "mimeType": "image/png", "data": png_pixel()},
        ]);
        let stager = FakeStager {
            limit: 1024,
            ..FakeStager::default()
        };
        let blocks = tool_result_content_blocks(Some(&content), Some(&stager));
        assert!(matches!(&blocks[0], ContentBlock::Image(_)));
    }
}
