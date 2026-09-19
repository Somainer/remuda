//! Media blocks inside tool results (D-045 §6.2 / codex-cua.md §6).
//!
//! A screenshot a computer-use tool returns is ordinary result content, not a
//! special observation: no new `ObservationKind`, bytes live in the Hub object
//! store, and what travels in the journal is only an `objectId` reference
//! ([`ContentBlock::Image`] + [`MediaBlock`]).
//!
//! Every native mapper (the transcript tailer and the live drivers) folds
//! image items through [`fold_tool_result`] so the degradation shape has one
//! implementation:
//!
//! * bytes are staged through a Node-supplied [`ToolMediaStager`] (the host
//!   token object endpoint);
//! * an oversize, undecodable or unstageable image becomes a **text block**
//!   naming the media type and the byte count — never a dropped result, never a
//!   panic, and never inline base64 in an observation or the journal;
//! * a carrier with no object route supplies no stager, so every image
//!   degrades to that same text block instead of leaking bytes;
//! * content items that are neither text nor image (an MCP `resource`, say)
//!   become an opaque text note rather than silently vanishing.
//!
//! The fold is synchronous because the native mappers are; callers on the
//! Node's async runtime must run it in `spawn_blocking` (the staging bridge
//! parks the folding thread until the object endpoint answers).

use crate::hubnode::extension_for_media_type;
use crate::{ContentBlock, Id, MediaBlock, TextBlock};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

/// Stages media bytes a tool produced into the Hub object store and returns
/// the object id the Hub minted.
///
/// Mappers call this synchronously while folding a native result; the Node's
/// implementation bridges to its host-token HTTP client on a dedicated
/// thread. The returned id becomes the block's only byte-bearing reference.
pub trait ToolMediaStager: Send + Sync + std::fmt::Debug {
    /// Per-object byte ceiling the receiver enforces; checked against the
    /// encoded length before anything is decoded or sent.
    fn max_bytes(&self) -> u64;

    /// Stage `bytes` under `name` and return the Hub object id.
    ///
    /// Implementations must never log the bytes themselves (D-045 Q6).
    fn stage(&self, name: &str, media_type: &str, bytes: Vec<u8>) -> Result<Id, ToolMediaError>;
}

/// Why an image could not become a [`ContentBlock::Image`]. Every variant
/// degrades to an honest text block; the error detail stays on this side of
/// the mapper and is never rendered to the user (it can carry a Hub URL).
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

/// Decoded-size ceiling used when no stager advertises one: large enough for
/// every legitimate screenshot, small enough that folding an image the
/// carrier cannot stage never has to decode a multi-gigabyte string.
pub const TOOL_RESULT_FALLBACK_MAX_BYTES: u64 = 25 * 1024 * 1024;

/// Maximum number of images folded from one tool result; further image items
/// degrade to one shared note instead of staging without bound.
pub const MAX_IMAGES_PER_RESULT: usize = 8;

/// Marker field the producers put on a pre-folded content value: an array of
/// already-built [`ContentBlock`]s. The async Node pipelines fold a native
/// line on a blocking thread first, replacing its content with this marker,
/// so the synchronous mappers never park a tokio worker.
pub const FOLDED_MARKER: &str = "remudaFoldedBlocks";

/// One image's fate, in array order: the object id when staging succeeded.
#[derive(Debug, Clone)]
pub struct ImageOutcome {
    /// Media type the item claimed.
    pub media_type: String,
    /// Decoded size when the item carried decodable bytes.
    pub size: Option<u64>,
    /// Object id the bytes were staged under; `None` when the image degraded.
    pub object_id: Option<Id>,
}

/// The fold output: result blocks plus per-image outcomes (used to scrub the
/// same bytes out of the mirrored `toolUseResult` sidecar).
#[derive(Debug, Clone)]
pub struct FoldedToolResult {
    /// Blocks in result order (joined text first, then images/notes).
    pub blocks: Vec<ContentBlock>,
    /// One outcome per image item, in array order (including capped ones).
    pub images: Vec<ImageOutcome>,
}

/// Fold the native `content` of one `tool_result` into protocol blocks.
#[must_use]
pub fn tool_result_content_blocks(
    content: Option<&Value>,
    stager: Option<&dyn ToolMediaStager>,
) -> Vec<ContentBlock> {
    fold_tool_result(content, stager).blocks
}

/// Fold the native `content` of one `tool_result`.
///
/// `content` is the value of the native block's `content` field: a bare
/// string, the mixed text/image content array Anthropic-shaped transcripts and
/// stream frames carry, a pre-folded marker, or absent/other.
///
/// Text is collected exactly as the legacy text-only mapper did — every text
/// item joined with no separator — so a result without images is
/// byte-identical to the old behaviour (a single text block, kept even when
/// empty). When images are present, the joined text leads and image-derived
/// blocks follow in array order.
#[must_use]
pub fn fold_tool_result(
    content: Option<&Value>,
    stager: Option<&dyn ToolMediaStager>,
) -> FoldedToolResult {
    let Some(content) = content else {
        return FoldedToolResult {
            blocks: vec![empty_text()],
            images: Vec::new(),
        };
    };
    // Pre-folded by the Node's blocking prep pass: blocks as-is.
    if let Some(Value::Array(blocks)) = content.get(FOLDED_MARKER) {
        let mut parsed = Vec::with_capacity(blocks.len());
        let mut images = Vec::new();
        for block in blocks {
            if let Ok(block) = serde_json::from_value::<ContentBlock>(block.clone()) {
                if let ContentBlock::Image(media) = &block {
                    images.push(ImageOutcome {
                        media_type: media.media_type.clone(),
                        size: media.size,
                        object_id: Some(media.object_id.clone()),
                    });
                }
                parsed.push(block);
            }
        }
        return FoldedToolResult {
            blocks: parsed,
            images,
        };
    }
    let Value::Array(items) = content else {
        // Bare string keeps the legacy shape; anything else (an object that is
        // not a pre-fold marker) is serialised rather than silently dropped,
        // matching the live driver's historical `to_string()` behaviour.
        let text = match content {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        return FoldedToolResult {
            blocks: vec![ContentBlock::Text(Box::new(TextBlock { text }))],
            images: Vec::new(),
        };
    };

    let text = items
        .iter()
        .filter_map(text_item)
        .collect::<Vec<_>>()
        .join("");
    let limit = stager.map_or(TOOL_RESULT_FALLBACK_MAX_BYTES, ToolMediaStager::max_bytes);
    let mut media_blocks: Vec<ContentBlock> = Vec::new();
    let mut images: Vec<ImageOutcome> = Vec::new();
    let mut image_no = 0u32;
    let mut saw_unknown = false;
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("image") => {
                image_no += 1;
                let (block, outcome) = if image_no as usize > MAX_IMAGES_PER_RESULT {
                    (
                        fallback_block(
                            &item_media_type(item),
                            None,
                            "over the per-result image cap",
                        ),
                        ImageOutcome {
                            media_type: item_media_type(item),
                            size: None,
                            object_id: None,
                        },
                    )
                } else {
                    fold_image_item(item, image_no, stager, limit)
                };
                media_blocks.push(block);
                images.push(outcome);
            }
            Some("text") | None => {}
            Some(other) => {
                // resource / resource_link / future item types: an opaque
                // note in item position, never an empty card and never bytes.
                saw_unknown = true;
                media_blocks.push(fallback_block(
                    "unknown",
                    None,
                    &format!("unsupported item type \"{other}\""),
                ));
            }
        }
    }

    // Pure text result: preserve the historical single-text-block shape.
    if media_blocks.is_empty() && !saw_unknown {
        return FoldedToolResult {
            blocks: vec![ContentBlock::Text(Box::new(TextBlock { text }))],
            images,
        };
    }
    let mut ordered = Vec::new();
    if !text.is_empty() {
        ordered.push(ContentBlock::Text(Box::new(TextBlock { text })));
    }
    ordered.extend(media_blocks);
    FoldedToolResult {
        blocks: ordered,
        images,
    }
}

fn empty_text() -> ContentBlock {
    ContentBlock::Text(Box::new(TextBlock {
        text: String::new(),
    }))
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

/// Turn one native image item into an image reference block (or the honest
/// text fallback) plus its outcome.
fn fold_image_item(
    item: &Value,
    image_no: u32,
    stager: Option<&dyn ToolMediaStager>,
    limit: u64,
) -> (ContentBlock, ImageOutcome) {
    let media_type = item_media_type(item);
    let outcome = |object_id: Option<Id>, size: Option<u64>| ImageOutcome {
        media_type: media_type.clone(),
        size,
        object_id,
    };
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
        let block = fallback_block(
            &media_type,
            None,
            &format!("image carried no data (source \"{source}\")"),
        );
        return (block, outcome(None, None));
    };
    let Value::String(raw) = data_value else {
        return (
            fallback_block(&media_type, None, "image data is not a string"),
            outcome(None, None),
        );
    };
    let encoded_len = raw.len() as u64;
    // Size check on the *encoded* length before decoding, so a multi-hundred-
    // MiB base64 string never gets allocated as decoded bytes (D-045 §6.2).
    let estimated = base64_decoded_len(encoded_len);
    if estimated > limit {
        let block = fallback_block(
            &media_type,
            Some(estimated),
            &format!("over the {limit}-byte object limit"),
        );
        return (block, outcome(None, Some(estimated)));
    }
    let trimmed = strip_base64_whitespace(raw);
    let Ok(bytes) = STANDARD.decode(trimmed.as_bytes()) else {
        return (
            fallback_block(&media_type, Some(encoded_len), "undecodable base64"),
            outcome(None, Some(encoded_len)),
        );
    };
    if bytes.is_empty() {
        return (
            fallback_block(&media_type, Some(0), "empty image"),
            outcome(None, Some(0)),
        );
    }
    let size = bytes.len() as u64;
    let Some(stager) = stager else {
        return (
            fallback_block(&media_type, Some(size), "no object route to stage bytes"),
            outcome(None, Some(size)),
        );
    };
    if size > limit {
        let block = fallback_block(
            &media_type,
            Some(size),
            &format!("over the {limit}-byte object limit"),
        );
        return (block, outcome(None, Some(size)));
    }
    let name = image_name(&media_type, image_no);
    match stager.stage(&name, &media_type, bytes) {
        Ok(object_id) => (
            ContentBlock::Image(Box::new(MediaBlock {
                object_id: object_id.clone(),
                media_type: media_type.clone(),
                name: Some(name),
                anchor: None,
                size: Some(size),
            })),
            outcome(Some(object_id), Some(size)),
        ),
        Err(ToolMediaError::TooLarge { size, limit }) => (
            fallback_block(
                &media_type,
                Some(size),
                &format!("over the {limit}-byte object limit"),
            ),
            outcome(None, Some(size)),
        ),
        // The error detail is intentionally not rendered: it can carry the
        // Hub's URL. The card sees a fixed, user-safe reason.
        Err(ToolMediaError::Unstageable(_)) => (
            fallback_block(&media_type, Some(size), "staging unavailable"),
            outcome(None, Some(size)),
        ),
    }
}

/// Upper bound on the decoded byte length of a base64 string this long,
/// ignoring whitespace (the decoder strips it, so this can over-estimate —
/// safe for a pre-allocation guard).
fn base64_decoded_len(encoded_len: u64) -> u64 {
    encoded_len
        .saturating_mul(3)
        .saturating_div(4)
        .saturating_add(3)
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

/// One text block honestly naming the image/item that could not be attached.
fn fallback_block(media_type: &str, size: Option<u64>, reason: &str) -> ContentBlock {
    let size = size.map_or_else(|| "unknown size".to_owned(), |n| format!("{n} bytes"));
    ContentBlock::Text(Box::new(TextBlock {
        text: format!("[image not attached: {media_type}, {size} — {reason}]"),
    }))
}

/// Replace every data-bearing image item mirrored in a `toolUseResult`
/// sidecar with the object id (staged) or a fixed media-type/bytes note
/// (degraded). Claude Code mirrors the native content array there, so without
/// this scrub the screenshot's base64 still rides into `structured_result`.
///
/// Outcomes are consumed in array order, matching image items encountered in
/// any depth-first walk. Items already scrubbed (flagged
/// `remudaMedia`) are left untouched, so the call is idempotent.
pub fn sanitize_tool_result_sidecar(sidecar: &mut Value, images: &[ImageOutcome]) {
    let mut next = images.iter();
    scrub_value(sidecar, &mut next);
}

fn scrub_value<'a>(value: &mut Value, next: &mut impl Iterator<Item = &'a ImageOutcome>) {
    match value {
        Value::Array(items) => {
            for item in items {
                scrub_value(item, next);
            }
        }
        Value::Object(map) => {
            let is_image = map
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t.eq_ignore_ascii_case("image"));
            let already = map
                .get("remudaMedia")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if is_image
                && !already
                && carries_image_data(map)
                && let Some(outcome) = next.next()
            {
                scrub_image_object(map, outcome);
                return;
            }
            for (_, child) in map.iter_mut() {
                scrub_value(child, next);
            }
        }
        Value::String(_) | Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn carries_image_data(map: &serde_json::Map<String, Value>) -> bool {
    map.get("data").and_then(Value::as_str).is_some()
        || map
            .get("source")
            .and_then(Value::as_object)
            .and_then(|source| source.get("data"))
            .and_then(Value::as_str)
            .is_some()
}

fn scrub_image_object(map: &mut serde_json::Map<String, Value>, outcome: &ImageOutcome) {
    let replacement = match &outcome.object_id {
        Some(id) => Value::String(id.as_str().to_owned()),
        None => Value::String(match outcome.size {
            Some(size) => format!("[image not attached: {}, {size} bytes]", outcome.media_type),
            None => format!("[image not attached: {}]", outcome.media_type),
        }),
    };
    if let Some(source) = map.get_mut("source").and_then(Value::as_object_mut) {
        source.remove("data");
        source.insert("remudaData".to_owned(), replacement.clone());
    }
    if let Some(data) = map.get_mut("data") {
        *data = replacement;
    }
    map.insert("remudaMedia".to_owned(), Value::Bool(true));
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
        let folded = fold_tool_result(Some(&content), None);
        assert_eq!(folded.blocks.len(), 1);
        match &folded.blocks[0] {
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
            ContentBlock::Text(text) => assert!(text.text.is_empty()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn non_array_object_content_is_serialised_not_dropped() {
        // An object content the driver would historically `to_string()`.
        let content = serde_json::json!({"weird": {"nested": true}});
        let blocks = tool_result_content_blocks(Some(&content), None);
        match &blocks[0] {
            ContentBlock::Text(text) => assert!(text.text.contains("weird")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_array_items_get_an_opaque_note() {
        let content = serde_json::json!([
            {"type": "resource", "resource": {"uri": "file:///x"}},
            {"type": "resource_link", "uri": "file:///y"}
        ]);
        let blocks = tool_result_content_blocks(Some(&content), None);
        let notes: Vec<_> = blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            notes
                .iter()
                .any(|t| t.contains("unsupported item type \"resource\""))
        );
        assert!(
            notes
                .iter()
                .any(|t| t.contains("unsupported item type \"resource_link\""))
        );
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
        let folded = fold_tool_result(Some(&content), Some(&stager));
        assert_eq!(folded.blocks.len(), 2);
        match &folded.blocks[0] {
            ContentBlock::Text(text) => assert_eq!(text.text, "seen:"),
            other => panic!("{other:?}"),
        }
        match &folded.blocks[1] {
            ContentBlock::Image(media) => {
                assert_eq!(media.media_type, "image/png");
                assert_eq!(media.name.as_deref(), Some("screen-1.png"));
                assert_eq!(media.size, Some(10));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(folded.images.len(), 1);
        assert!(folded.images[0].object_id.is_some());
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
    fn oversize_degrades_without_decoding() {
        // ~100 encoded chars ≈ 78 decoded bytes; limit 16, checked first.
        let big = "A".repeat(100);
        let content = serde_json::json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": big}},
        ]);
        let stager = FakeStager {
            limit: 16,
            ..FakeStager::default()
        };
        let folded = fold_tool_result(Some(&content), Some(&stager));
        match &folded.blocks[0] {
            ContentBlock::Text(text) => {
                assert!(text.text.contains("16-byte object limit"), "{}", text.text)
            }
            other => panic!("{other:?}"),
        }
        // Never reached the stager, so nothing was decoded-and-sent.
        assert!(stager.staged.lock().unwrap().is_empty());
        assert!(folded.images[0].object_id.is_none());
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

    #[test]
    fn more_than_eight_images_degrades_the_rest() {
        let items: Vec<Value> = (0..(MAX_IMAGES_PER_RESULT + 2))
            .map(|_| {
                serde_json::json!({"type": "image",
                    "source": {"type": "base64", "media_type": "image/png", "data": png_pixel()}})
            })
            .collect();
        let stager = FakeStager {
            limit: 1024,
            ..FakeStager::default()
        };
        let folded = fold_tool_result(Some(&serde_json::json!(items)), Some(&stager));
        assert_eq!(stager.staged.lock().unwrap().len(), MAX_IMAGES_PER_RESULT);
        assert!(folded.blocks.iter().any(
            |b| matches!(b, ContentBlock::Text(t) if t.text.contains("per-result image cap"))
        ));
    }

    #[test]
    fn folded_marker_round_trips() {
        let blocks = vec![
            ContentBlock::Text(Box::new(TextBlock { text: "ok".into() })),
            ContentBlock::Image(Box::new(MediaBlock {
                object_id: Id::new("obj").unwrap(),
                media_type: "image/png".into(),
                name: Some("screen-1.png".into()),
                anchor: None,
                size: Some(70),
            })),
        ];
        let content = serde_json::json!({ FOLDED_MARKER: blocks });
        let folded = fold_tool_result(Some(&content), None);
        assert_eq!(folded.blocks, blocks);
        assert_eq!(folded.images.len(), 1);
    }

    #[test]
    fn sidecar_scrub_replaces_data_with_object_id_and_is_idempotent() {
        let object_id = Id::new("obj").unwrap();
        let outcomes = vec![
            ImageOutcome {
                media_type: "image/png".into(),
                size: Some(70),
                object_id: Some(object_id.clone()),
            },
            ImageOutcome {
                media_type: "image/png".into(),
                size: Some(70),
                object_id: None,
            },
        ];
        let mut sidecar = serde_json::json!({
            "content": [
                {"type": "text", "text": "x"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                {"type": "image", "mimeType": "image/png", "data": "BBBB"}
            ]
        });
        sanitize_tool_result_sidecar(&mut sidecar, &outcomes);
        let text = sidecar.to_string();
        assert!(!text.contains("AAAA"));
        assert!(!text.contains("BBBB"));
        assert!(text.contains(object_id.as_str()));
        assert!(text.contains("image not attached"));
        // Second pass changes nothing (flagged items are skipped).
        let before = sidecar.to_string();
        sanitize_tool_result_sidecar(&mut sidecar, &outcomes);
        assert_eq!(before, sidecar.to_string());
    }
}
