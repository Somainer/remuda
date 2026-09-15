//! D-027: turn prompt attachment blocks into what each agent CLI can read.
//!
//! The Node materializes every attachment to disk and describes it with two
//! adjacent blocks: an `image` [`MediaBlock`] naming the Hub object, and a
//! `resource` block whose `uri` is the local `file://` path. Drivers differ in
//! what they can do with that:
//!
//! - `claude-print` inlines the bytes as a base64 image content block, the one
//!   remote path verified to work.
//! - the PTY family can only type text, so they mention the absolute path and
//!   rely on the agent's own file-reading tool.
//!
//! Both need the same path, which is why this lives here rather than in one
//! driver.

use crate::{DriverError, DriverResult};
use remuda_protocol::ContentBlock;
use std::path::PathBuf;

/// One attachment a driver can deliver, resolved to a local file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptAttachment {
    /// Hub object identity, for diagnostics and journal notes.
    pub object_id: String,
    /// Media type as sniffed by the Hub at upload.
    pub media_type: String,
    /// Absolute path on this host.
    pub path: PathBuf,
    /// 1-based `[Image #n]` anchor from the prompt manifest, when numbered.
    pub anchor: Option<u32>,
}

impl PromptAttachment {
    /// Read the bytes back for a driver that inlines them.
    ///
    /// The Node wrote this file moments ago, so a failure here means the disk
    /// or the instance directory went away — worth surfacing, not papering
    /// over with a text-only fallback.
    pub fn read(&self) -> DriverResult<Vec<u8>> {
        std::fs::read(&self.path).map_err(DriverError::Io)
    }

    /// Path as a string for a prompt mention.
    pub fn display_path(&self) -> String {
        self.path.display().to_string()
    }

    /// `#1` / `#2` marker matching the prompt's `[Image #n]`, or "" unanchored.
    fn number_label(&self) -> String {
        self.anchor.map(|n| format!("#{n}")).unwrap_or_default()
    }
}

/// Collect the attachments carried by a prompt's blocks.
///
/// Pairs each `image` block with the `resource` block that carries its path.
/// An image block with no resolvable local path is skipped rather than
/// guessed at: the caller decides whether that is fatal.
#[must_use]
pub fn attachments_of(blocks: &[ContentBlock]) -> Vec<PromptAttachment> {
    let mut found = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        let ContentBlock::Image(media) = block else {
            continue;
        };
        let object_id = media.object_id.to_string();
        // The Node emits the resource block immediately after its image block;
        // fall back to a scan so a reordered producer still resolves.
        let path = blocks
            .get(index + 1)
            .and_then(local_path_for(&object_id))
            .or_else(|| blocks.iter().find_map(local_path_for(&object_id)));
        let Some(path) = path else {
            continue;
        };
        found.push(PromptAttachment {
            object_id,
            media_type: media.media_type.clone(),
            path,
            anchor: media.anchor,
        });
    }
    found
}

/// Match a `resource` block that names this object and carries a `file://` uri.
fn local_path_for(object_id: &str) -> impl Fn(&ContentBlock) -> Option<PathBuf> + '_ {
    move |block| {
        let ContentBlock::Resource(resource) = block else {
            return None;
        };
        if resource
            .object_id
            .as_ref()
            .map(ToString::to_string)
            .as_deref()
            != Some(object_id)
        {
            return None;
        }
        resource
            .uri
            .strip_prefix("file://")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
    }
}

/// Concatenated text of a prompt's text blocks.
#[must_use]
pub fn text_of(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Prompt text for a driver that can only type, with attachment paths appended.
///
/// The wording is deliberately imperative. Codex is not known to act on a path
/// merely mentioned in prose (design §3, UNVERIFIED), so the instruction says
/// to read the file rather than leaving the agent to infer it.
pub fn text_with_path_mentions(blocks: &[ContentBlock]) -> DriverResult<String> {
    let text = text_of(blocks);
    let attachments = attachments_of(blocks);
    if text.is_empty() && attachments.is_empty() {
        return Err(DriverError::InvalidLaunchSpec(
            "prompt has no text blocks".into(),
        ));
    }
    Ok(append_path_mentions(&text, &attachments))
}

/// Append one attachment block per image under a read instruction.
///
/// Numbered prompts (2026-09-15) keep the prompt's `[Image #n]` numbering
/// visible on every line and name the Hub objectId, so an agent with the
/// Remuda MCP tools can call `remuda_attachments_list` to map the number onto
/// `remuda_attachment`; the absolute path stays on the same line as the
/// fallback for a harness whose MCP server is not injected.
#[must_use]
pub fn append_path_mentions(text: &str, attachments: &[PromptAttachment]) -> String {
    if attachments.is_empty() {
        return text.to_owned();
    }
    let mut out = String::from(text);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    let numbered = attachments
        .iter()
        .any(|attachment| attachment.anchor.is_some());
    if numbered {
        out.push_str(&format!(
            "图片按正文中的 [Image #n] 编号（共 {} 张）。可用 MCP 工具 remuda_attachments_list \
             把编号解析成 objectId 后调用 remuda_attachment 读取；也可以直接读取下面的本地路径：\n",
            attachments.len()
        ));
    } else {
        out.push_str(if attachments.len() == 1 {
            "请读取下面这个本地图片文件（附件）：\n"
        } else {
            "请读取下面这些本地图片文件（附件）：\n"
        });
    }
    for attachment in attachments {
        if numbered {
            // 附件 #2: obj_0199… (image/jpeg) /data/.../obj_0199….jpg
            out.push_str("附件 ");
            out.push_str(&attachment.number_label());
            out.push_str(": ");
            out.push_str(&attachment.object_id);
            out.push_str(" (");
            out.push_str(&attachment.media_type);
            out.push_str(") ");
            out.push_str(&attachment.display_path());
            out.push('\n');
        } else {
            out.push_str("附件: ");
            out.push_str(&attachment.display_path());
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_protocol::{Knowledge, MediaBlock, ResourceBlock, TextBlock};

    fn blocks(object_id: &str, path: &str) -> Vec<ContentBlock> {
        let id = remuda_protocol::Id::try_from(object_id.to_owned()).expect("id");
        vec![
            ContentBlock::Image(Box::new(MediaBlock {
                object_id: id.clone(),
                media_type: "image/png".into(),
                name: Some("shot.png".into()),
                anchor: None,
            })),
            ContentBlock::Resource(Box::new(ResourceBlock {
                uri: format!("file://{path}"),
                media_type: Knowledge::Known {
                    value: "image/png".into(),
                },
                object_id: Some(id),
            })),
            ContentBlock::Text(Box::new(TextBlock {
                text: "what colour is the image?".into(),
            })),
        ]
    }

    fn object_id() -> String {
        remuda_protocol::Id::new("obj").expect("id").to_string()
    }

    #[test]
    fn an_image_block_resolves_through_its_resource_block() {
        let id = object_id();
        let found = attachments_of(&blocks(&id, "/data/attachments/shot.png"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].object_id, id);
        assert_eq!(found[0].media_type, "image/png");
        assert_eq!(found[0].display_path(), "/data/attachments/shot.png");
    }

    /// A text-only prompt is the overwhelmingly common case and must be
    /// completely unaffected.
    #[test]
    fn a_text_only_prompt_has_no_attachments_and_keeps_its_text() {
        let blocks = vec![ContentBlock::Text(Box::new(TextBlock {
            text: "hello".into(),
        }))];
        assert!(attachments_of(&blocks).is_empty());
        assert_eq!(text_with_path_mentions(&blocks).expect("text"), "hello");
    }

    /// An image with no local path is skipped, not guessed at.
    #[test]
    fn an_image_without_a_resource_block_is_skipped() {
        let mut only_image = blocks(&object_id(), "/data/shot.png");
        only_image.remove(1);
        assert!(attachments_of(&only_image).is_empty());
    }

    /// A resource block naming a different object must not be borrowed.
    #[test]
    fn a_resource_block_for_another_object_does_not_match() {
        let mut mismatched = blocks(&object_id(), "/data/shot.png");
        mismatched[1] = ContentBlock::Resource(Box::new(ResourceBlock {
            uri: "file:///data/other.png".into(),
            media_type: Knowledge::Known {
                value: "image/png".into(),
            },
            object_id: Some(remuda_protocol::Id::new("obj").expect("id")),
        }));
        assert!(attachments_of(&mismatched).is_empty());
    }

    /// A non-file uri is not a local path.
    #[test]
    fn a_remote_resource_uri_is_not_treated_as_a_local_path() {
        let id = object_id();
        let mut remote = blocks(&id, "/data/shot.png");
        remote[1] = ContentBlock::Resource(Box::new(ResourceBlock {
            uri: "https://hub.example/v1/objects/obj_1".into(),
            media_type: Knowledge::Known {
                value: "image/png".into(),
            },
            object_id: Some(remuda_protocol::Id::try_from(id).expect("id")),
        }));
        assert!(attachments_of(&remote).is_empty());
    }

    #[test]
    fn path_mentions_instruct_the_agent_to_read_the_file() {
        let mentioned =
            text_with_path_mentions(&blocks(&object_id(), "/data/attachments/shot.png"))
                .expect("text");
        assert!(mentioned.starts_with("what colour is the image?"));
        assert!(mentioned.contains("请读取"));
        assert!(
            mentioned.contains("附件: /data/attachments/shot.png"),
            "{mentioned}"
        );
    }

    /// Numbered sends keep the prompt's [Image #n] numbering on each line and
    /// name the objectId, so the MCP path and the fallback path agree.
    #[test]
    fn numbered_mentions_pair_the_token_number_with_object_and_path() {
        let id_1 = remuda_protocol::Id::new("obj").expect("id");
        let id_2 = remuda_protocol::Id::new("obj").expect("id");
        let blocks = vec![
            ContentBlock::Image(Box::new(MediaBlock {
                object_id: id_1.clone(),
                media_type: "image/png".into(),
                name: Some("a.png".into()),
                anchor: Some(1),
            })),
            ContentBlock::Resource(Box::new(ResourceBlock {
                uri: "file:///data/a.png".into(),
                media_type: Knowledge::Known {
                    value: "image/png".into(),
                },
                object_id: Some(id_1),
            })),
            ContentBlock::Image(Box::new(MediaBlock {
                object_id: id_2.clone(),
                media_type: "image/jpeg".into(),
                name: Some("b.jpg".into()),
                anchor: Some(2),
            })),
            ContentBlock::Resource(Box::new(ResourceBlock {
                uri: "file:///data/b.jpg".into(),
                media_type: Knowledge::Known {
                    value: "image/jpeg".into(),
                },
                object_id: Some(id_2),
            })),
            ContentBlock::Text(Box::new(TextBlock {
                text: "compare [Image #1] with [Image #2]".into(),
            })),
        ];
        let mentioned = text_with_path_mentions(&blocks).expect("text");
        assert!(
            mentioned.contains("compare [Image #1] with [Image #2]"),
            "{mentioned}"
        );
        let line_1 = mentioned
            .lines()
            .find(|line| line.contains("附件 #1:"))
            .expect("#1 line");
        let line_2 = mentioned
            .lines()
            .find(|line| line.contains("附件 #2:"))
            .expect("#2 line");
        assert!(
            line_1.contains("(image/png)") && line_1.ends_with("/data/a.png"),
            "{line_1}"
        );
        assert!(
            line_2.contains("(image/jpeg)") && line_2.ends_with("/data/b.jpg"),
            "{line_2}"
        );
        // #1 must precede #2 even though the driver iterates block order.
        assert!(mentioned.find("附件 #1:").unwrap() < mentioned.find("附件 #2:").unwrap());
    }

    /// An attachment with no accompanying text still produces a usable prompt
    /// rather than failing as "no text blocks".
    #[test]
    fn an_attachment_alone_still_produces_a_prompt() {
        let mut no_text = blocks(&object_id(), "/data/shot.png");
        no_text.pop();
        let mentioned = text_with_path_mentions(&no_text).expect("text");
        assert!(mentioned.contains("附件: /data/shot.png"), "{mentioned}");
    }

    #[test]
    fn an_empty_prompt_is_still_rejected() {
        assert!(text_with_path_mentions(&[]).is_err());
    }
}
