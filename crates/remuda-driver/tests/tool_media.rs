//! Image blocks in a stdout `user` tool_result (D-045 §6.2, c-cua-media).
//!
//! The live claude-print mapper stages a result image through the injected
//! stager and carries only its object id; with no stager the image degrades to
//! a text block. The base64 payload is never serialized into an observation.

use std::sync::{Arc, Mutex};

use remuda_driver::claude_print::StdoutMapper;
use remuda_protocol::{
    ContentBlock, DriverKind, Id, ObservationPayload, ToolMediaError, ToolMediaStager,
};
use serde_json::json;

#[derive(Debug)]
struct FakeStager {
    fail: bool,
    staged: Mutex<Vec<(String, String, usize)>>,
}

impl ToolMediaStager for FakeStager {
    fn max_bytes(&self) -> u64 {
        4 * 1024 * 1024
    }
    fn stage(&self, name: &str, media_type: &str, bytes: Vec<u8>) -> Result<Id, ToolMediaError> {
        if self.fail {
            return Err(ToolMediaError::Unstageable("offline".into()));
        }
        self.staged
            .lock()
            .unwrap()
            .push((name.to_owned(), media_type.to_owned(), bytes.len()));
        Ok(Id::new("obj").unwrap())
    }
}

/// 1x1 PNG, base64.
fn png() -> String {
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="
        .to_owned()
}

fn user_frame(content: serde_json::Value) -> serde_json::Value {
    json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_cua_1",
                "content": content.clone(),
            }],
        },
        // Claude mirrors the content array here.
        "toolUseResult": {
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_cua_1",
                "content": content,
            }],
        },
    })
}

fn map_result(
    mapper: &mut StdoutMapper,
    content: serde_json::Value,
) -> remuda_protocol::ToolResultPayload {
    let observations = mapper.map(user_frame(content)).unwrap();
    observations
        .into_iter()
        .find_map(|observation| match observation.body {
            ObservationPayload::ToolResult(result) => Some(*result),
            _ => None,
        })
        .expect("one tool result")
}

fn result_blocks(mapper: &mut StdoutMapper, content: serde_json::Value) -> Vec<ContentBlock> {
    map_result(mapper, content).blocks
}

#[test]
fn mixed_result_is_text_plus_staged_reference() {
    let stager = Arc::new(FakeStager {
        fail: false,
        staged: Mutex::new(Vec::new()),
    });
    let mut mapper =
        StdoutMapper::new(DriverKind::ClaudePrint, "sess").with_media_stager(stager.clone());
    let blocks = result_blocks(
        &mut mapper,
        json!([
            {"type": "text", "text": "clicked"},
            {"type": "image", "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": png(),
            }},
        ]),
    );
    assert_eq!(blocks.len(), 2);
    match &blocks[0] {
        ContentBlock::Text(text) => assert_eq!(text.text, "clicked"),
        other => panic!("{other:?}"),
    }
    match &blocks[1] {
        ContentBlock::Image(media) => {
            assert_eq!(media.media_type, "image/png");
            assert_eq!(media.name.as_deref(), Some("screen-1.png"));
            assert_eq!(media.size, Some(70));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(stager.staged.lock().unwrap().len(), 1);
}

#[test]
fn whole_result_payload_carries_no_base64_with_mirrored_sidecar() {
    let stager = Arc::new(FakeStager {
        fail: false,
        staged: Mutex::new(Vec::new()),
    });
    let mut mapper = StdoutMapper::new(DriverKind::ClaudePrint, "sess").with_media_stager(stager);
    let result = map_result(
        &mut mapper,
        json!([
            {"type": "image", "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": png(),
            }},
        ]),
    );
    let raw = serde_json::to_string(&result).unwrap();
    assert!(
        !raw.contains(png().as_str()),
        "mirrored sidecar leaked base64"
    );
}

#[test]
fn image_bytes_are_never_inlined_without_a_stager() {
    let mut mapper = StdoutMapper::new(DriverKind::ClaudePrint, "sess");
    let blocks = result_blocks(
        &mut mapper,
        json!([
            {"type": "image", "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": png(),
            }},
        ]),
    );
    // One fallback text block — no image, no base64, result not dropped.
    assert_eq!(blocks.len(), 1);
    let serialized = serde_json::to_string(&blocks).unwrap();
    assert!(!serialized.contains(png().as_str()));
    match &blocks[0] {
        ContentBlock::Text(text) => {
            assert!(text.text.contains("image/png"));
            assert!(text.text.contains("no object route"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn text_array_result_is_the_joined_text_not_json() {
    // Regression: the live mapper used to `to_string()` an array content,
    // emitting raw JSON (and any image base64) into the result text.
    let mut mapper = StdoutMapper::new(DriverKind::ClaudePrint, "sess");
    let blocks = result_blocks(
        &mut mapper,
        json!([
            {"type": "text", "text": "line one\n"},
            {"type": "text", "text": "line two"},
        ]),
    );
    match &blocks[0] {
        ContentBlock::Text(text) => assert_eq!(text.text, "line one\nline two"),
        other => panic!("{other:?}"),
    }
}
