//! Image content blocks in a tool result (D-045 §6.2, c-cua-media).
//!
//! The transcript mapper stages an image through the injected
//! [`ToolMediaStager`] and carries only the returned object id; an image the
//! stager cannot take degrades to a text block naming media type and size.
//! Bytes are never inlined into the observation.

use std::sync::{Arc, Mutex};

use remuda_journal::{MapContext, NativeIds, digest_of, map_claude_line};
use remuda_protocol::{
    ContentBlock, FileCursor, HostId, Id, InstanceId, ObservationPayload, SourceChannel,
    ToolMediaError, ToolMediaStager, U64,
};
use serde_json::json;

#[derive(Debug)]
struct FakeStager {
    limit: u64,
    fail: bool,
    staged: Mutex<Vec<(String, String, usize)>>,
}

impl ToolMediaStager for FakeStager {
    fn max_bytes(&self) -> u64 {
        self.limit
    }
    fn stage(&self, name: &str, media_type: &str, bytes: Vec<u8>) -> Result<Id, ToolMediaError> {
        if self.fail {
            return Err(ToolMediaError::Unstageable("connection refused".into()));
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
        Ok(Id::new("obj").unwrap())
    }
}

fn context(instance: &InstanceId, stager: Option<Arc<dyn ToolMediaStager>>) -> MapContext {
    let mut ctx = MapContext::claude_file(
        instance.clone(),
        Id::new("obj").unwrap(),
        HostId::new(),
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        SourceChannel::Transcript,
    );
    ctx = ctx.with_media_stager(stager);
    ctx
}

fn map_record(record: serde_json::Value, ctx: &MapContext) -> Vec<ObservationPayload> {
    let instance = ctx.instance_id.clone();
    let mut ids = NativeIds::new(instance.as_id().as_str());
    let line = serde_json::to_vec(&record).unwrap();
    let cursor = FileCursor {
        file_identity: Id::new("obj").unwrap(),
        file_generation: U64(1),
        offset: U64(0),
        length: U64(line.len() as u64),
        digest: digest_of(&line),
    };
    map_claude_line(ctx, &mut ids, &line, &cursor)
        .unwrap()
        .into_iter()
        .map(|envelope| envelope.body)
        .collect()
}

/// Build a native user record carrying the tool_result. Claude mirrors the
/// content array into `toolUseResult`, so the helper mirrors it too: the
/// producer must scrub base64 out of both places.
fn tool_result_record(content: serde_json::Value) -> serde_json::Value {
    json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_screenshot_1",
                "content": content,
            }],
        },
        "toolUseResult": {
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_screenshot_1",
                "content": content,
            }],
        },
    })
}

fn first_result(payloads: &[ObservationPayload]) -> &remuda_protocol::ToolResultPayload {
    payloads
        .iter()
        .find_map(|payload| match payload {
            ObservationPayload::ToolResult(result) => Some(result.as_ref()),
            _ => None,
        })
        .expect("one tool result")
}

/// A 1x1 PNG (67 raw bytes), base64 encoded.
fn png_1x1() -> String {
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(PNG)
}

#[test]
fn mixed_text_and_image_result_becomes_text_plus_object_reference() {
    let stager = Arc::new(FakeStager {
        limit: 4 * 1024 * 1024,
        fail: false,
        staged: Mutex::new(Vec::new()),
    });
    let instance = InstanceId::new();
    let ctx = context(&instance, Some(stager.clone()));
    let payloads = map_record(
        tool_result_record(json!([
            {"type": "text", "text": "window focused"},
            {"type": "image", "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": png_1x1(),
            }},
        ])),
        &ctx,
    );
    let result = first_result(&payloads);
    assert_eq!(result.blocks.len(), 2);
    match &result.blocks[0] {
        ContentBlock::Text(text) => assert_eq!(text.text, "window focused"),
        other => panic!("{other:?}"),
    }
    match &result.blocks[1] {
        ContentBlock::Image(media) => {
            assert!(media.object_id.as_str().starts_with("obj_"));
            assert_eq!(media.media_type, "image/png");
            assert_eq!(media.name.as_deref(), Some("screen-1.png"));
            assert_eq!(media.size, Some(67));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(stager.staged.lock().unwrap().len(), 1);
}

#[test]
fn whole_result_payload_carries_no_base64_with_mirrored_sidecar() {
    // Regression: the screenshot's base64 rode into structured_result via the
    // mirrored toolUseResult even after blocks were folded correctly.
    let stager = Arc::new(FakeStager {
        limit: 4 * 1024 * 1024,
        fail: false,
        staged: Mutex::new(Vec::new()),
    });
    let instance = InstanceId::new();
    let ctx = context(&instance, Some(stager));
    let payloads = map_record(
        tool_result_record(json!([
            {"type": "text", "text": "window focused"},
            {"type": "image", "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": png_1x1(),
            }},
        ])),
        &ctx,
    );
    let result = first_result(&payloads);
    let raw = serde_json::to_string(result).unwrap();
    assert!(!raw.contains(&png_1x1()), "sidecar still carries base64");
    // The object id survives in the scrubbed sidecar.
    let object_id = match &result.blocks[1] {
        ContentBlock::Image(media) => media.object_id.as_str().to_owned(),
        other => panic!("{other:?}"),
    };
    assert!(raw.contains(&object_id));
}

#[test]
fn image_only_result_is_one_reference_and_never_inlines_bytes() {
    let stager = Arc::new(FakeStager {
        limit: 4 * 1024 * 1024,
        fail: false,
        staged: Mutex::new(Vec::new()),
    });
    let instance = InstanceId::new();
    let ctx = context(&instance, Some(stager));
    let payloads = map_record(
        tool_result_record(json!([
            {"type": "image", "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": png_1x1(),
            }},
        ])),
        &ctx,
    );
    let result = first_result(&payloads);
    assert_eq!(result.blocks.len(), 1);
    assert!(matches!(&result.blocks[0], ContentBlock::Image(_)));
    // The encoded payload never appears anywhere in the FULL serialized
    // result (blocks plus the mirrored structured_result sidecar).
    let raw = serde_json::to_string(result).unwrap();
    assert!(!raw.contains(&png_1x1()));
}

#[test]
fn malformed_image_degrades_to_named_text_block() {
    let stager = Arc::new(FakeStager {
        limit: 4 * 1024 * 1024,
        fail: false,
        staged: Mutex::new(Vec::new()),
    });
    let instance = InstanceId::new();
    let ctx = context(&instance, Some(stager.clone()));
    let payloads = map_record(
        tool_result_record(json!([
            {"type": "text", "text": "before"},
            {"type": "image", "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": "@@not base64@@",
            }},
        ])),
        &ctx,
    );
    let result = first_result(&payloads);
    match result.blocks.last().unwrap() {
        ContentBlock::Text(text) => {
            assert!(text.text.contains("image/png"), "{}", text.text);
            assert!(text.text.contains("undecodable base64"), "{}", text.text);
        }
        other => panic!("{other:?}"),
    }
    assert!(stager.staged.lock().unwrap().is_empty());
}

#[test]
fn unstageable_image_degrades_without_dropping_the_result() {
    let stager = Arc::new(FakeStager {
        limit: 4 * 1024 * 1024,
        fail: true,
        staged: Mutex::new(Vec::new()),
    });
    let instance = InstanceId::new();
    let ctx = context(&instance, Some(stager));
    let payloads = map_record(
        tool_result_record(json!([
            {"type": "text", "text": "ok"},
            {"type": "image", "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": png_1x1(),
            }},
        ])),
        &ctx,
    );
    let result = first_result(&payloads);
    // Text survives and the failed image is a fallback block, not dropped.
    assert_eq!(result.blocks.len(), 2);
    match &result.blocks[1] {
        ContentBlock::Text(text) => {
            assert!(text.text.contains("67 bytes"), "{}", text.text);
            assert!(text.text.contains("staging unavailable"), "{}", text.text);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn text_only_result_is_byte_identical_without_a_stager() {
    let instance = InstanceId::new();
    let ctx = context(&instance, None);
    let payloads = map_record(
        tool_result_record(json!([
            {"type": "text", "text": "plain "},
            {"type": "text", "text": "result"},
        ])),
        &ctx,
    );
    let result = first_result(&payloads);
    assert_eq!(result.blocks.len(), 1);
    match &result.blocks[0] {
        ContentBlock::Text(text) => assert_eq!(text.text, "plain result"),
        other => panic!("{other:?}"),
    }
}
