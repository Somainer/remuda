//! D-027 / D-027b: turn prompt attachment blocks into what each agent CLI can
//! read.
//!
//! The Node materializes every attachment to disk and describes it with two
//! adjacent blocks: an `image`/`file` [`MediaBlock`] naming the Hub object,
//! and a `resource` block whose `uri` is the local `file://` path. Drivers
//! differ in what they can do with that:
//!
//! - `claude-print` inlines image bytes as a base64 image content block, the
//!   one remote path verified to work.
//! - the PTY family can only type text, so they mention the absolute path and
//!   rely on the agent's own file-reading tool.
//!
//! D-027b (2026-09-15) added arbitrary files. A file is *never* inlined:
//! before the user text the expansion carries one line per file,
//! `[File #n] <name> (<mime>, <size>) saved at <absolute path>`, so
//! claude/codex/grok can open it with their file tools.
//!
//! Both kinds need the same path, which is why this lives here rather than in
//! one driver.

use crate::{DriverError, DriverResult};
use remuda_protocol::hubnode::AttachmentKind;
use remuda_protocol::ContentBlock;
use std::path::PathBuf;

/// One attachment a driver can deliver, resolved to a local file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptAttachment {
    /// Hub object identity, for diagnostics and journal notes.
    pub object_id: String,
    /// Image vs. arbitrary file (D-027b).
    pub kind: AttachmentKind,
    /// Media type as accepted by the Hub at upload.
    pub media_type: String,
    /// Sanitised display name (landed basename).
    pub name: String,
    /// Absolute path on this host.
    pub path: PathBuf,
    /// Stored byte length, for the `(mime, size)` mention.
    pub byte_len: Option<u64>,
    /// 1-based `[Image #n]`/`[File #n]` anchor from the prompt manifest, when
    /// numbered.
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

    /// `#1` / `#2` marker matching the prompt's anchor, or "" unanchored.
    fn number_label(&self) -> String {
        self.anchor.map(|n| format!("#{n}")).unwrap_or_default()
    }
}

/// Collect the attachments carried by a prompt's blocks.
///
/// Pairs each `image`/`file` media block with the `resource` block that
/// carries its path. A media block with no resolvable local path is skipped
/// rather than guessed at: the caller decides whether that is fatal.
#[must_use]
pub fn attachments_of(blocks: &[ContentBlock]) -> Vec<PromptAttachment> {
    let mut found = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        let Some((media, kind)) = media_of(block) else {
            continue;
        };
        let object_id = media.object_id.to_string();
        // The Node emits the resource block immediately after its media block;
        // fall back to a scan so a reordered producer still resolves.
        let path = blocks
            .get(index + 1)
            .and_then(local_path_for(&object_id))
            .or_else(|| blocks.iter().find_map(local_path_for(&object_id)));
        let Some(path) = path else {
            continue;
        };
        let name = media
            .name
            .clone()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| object_id.clone())
            });
        found.push(PromptAttachment {
            object_id,
            kind,
            media_type: media.media_type.clone(),
            name,
            path,
            byte_len: media.size,
            anchor: media.anchor,
        });
    }
    found
}

/// Match an `image` or `file` media block; audio rides the file path path.
fn media_of(block: &ContentBlock) -> Option<(&remuda_protocol::MediaBlock, AttachmentKind)> {
    match block {
        ContentBlock::Image(media) => Some((media, AttachmentKind::Image)),
        ContentBlock::File(media) | ContentBlock::Audio(media) => {
            Some((media, AttachmentKind::File))
        }
        _ => None,
    }
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

/// Human-readable byte size for the `(mime, size)` mention.
#[must_use]
pub fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{} KB", bytes / KB)
    } else if bytes < 1024 * MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * MB as f64))
    }
}

/// One `[File #n] … saved at …` line per non-image attachment (D-027b).
///
/// The line form is fixed by design: the anchor, sanitised name, MIME type,
/// human size and the absolute path on one line, before the user text, so any
/// of claude/codex/grok can open the file with its own file tools.
#[must_use]
pub fn file_mention_lines(attachments: &[PromptAttachment]) -> Vec<String> {
    attachments
        .iter()
        .filter(|attachment| attachment.kind == AttachmentKind::File)
        .map(|attachment| {
            let anchor = attachment
                .anchor
                .map(|n| format!("[File #{n}] "))
                .unwrap_or_default();
            let size = attachment.byte_len.map(human_size).unwrap_or_else(|| "?".into());
            format!(
                "{anchor}{} ({}, {size}) saved at {}",
                attachment.name,
                attachment.media_type,
                attachment.display_path()
            )
        })
        .collect()
}

/// Prepend the file-reference lines to prompt text (D-027b).
#[must_use]
pub fn with_file_lines(text: &str, attachments: &[PromptAttachment]) -> String {
    let lines = file_mention_lines(attachments);
    if lines.is_empty() {
        return text.to_owned();
    }
    let mut out = lines.join("\n");
    out.push('\n');
    if !text.is_empty() {
        out.push('\n');
        out.push_str(text);
    }
    out
}

/// Prompt text for a driver that can only type, with attachment references.
///
/// Non-image files get a `[File #n] … saved at …` line *before* the user text;
/// images keep the MVP's read-instruction footer after it. The wording for
/// images is deliberately imperative: Codex is not known to act on a path
/// merely mentioned in prose (design §3, UNVERIFIED).
pub fn text_with_path_mentions(blocks: &[ContentBlock]) -> DriverResult<String> {
    let attachments = attachments_of(blocks);
    let text = text_of(blocks);
    if text.is_empty() && attachments.is_empty() {
        return Err(DriverError::InvalidLaunchSpec(
            "prompt has no text blocks".into(),
        ));
    }
    let with_files = with_file_lines(&text, &attachments);
    Ok(append_image_mentions(&with_files, &attachments))
}

/// Append one block per image under a read instruction, after the prompt.
///
/// Numbered prompts (2026-09-15) keep the prompt's `[Image #n]` numbering
/// visible on every line and name the Hub objectId, so an agent with the
/// Remuda MCP tools can call `remuda_attachments_list` to map the number onto
/// `remuda_attachment`; the absolute path stays on the same line as the
/// fallback for a harness whose MCP server is not injected.
#[must_use]
fn append_image_mentions(text: &str, attachments: &[PromptAttachment]) -> String {
    let images: Vec<&PromptAttachment> = attachments
        .iter()
        .filter(|attachment| attachment.kind == AttachmentKind::Image)
        .collect();
    if images.is_empty() {
        return text.to_owned();
    }
    let mut out = String::from(text);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    let numbered = images.iter().any(|attachment| attachment.anchor.is_some());
    if numbered {
        out.push_str(&format!(
            "图片按正文中的 [Image #n] 编号（共 {} 张）。可用 MCP 工具 remuda_attachments_list \
             把编号解析成 objectId 后调用 remuda_attachment 读取；也可以直接读取下面的本地路径：\n",
            images.len()
        ));
    } else {
        out.push_str(if images.len() == 1 {
            "请读取下面这个本地图片文件（附件）：\n"
        } else {
            "请读取下面这些本地图片文件（附件）：\n"
        });
    }
    for attachment in images {
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

    fn image_blocks(object_id: &str, path: &str) -> Vec<ContentBlock> {
        let id = remuda_protocol::Id::try_from(object_id.to_owned()).expect("id");
        vec![
            ContentBlock::Image(Box::new(MediaBlock {
                object_id: id.clone(),
                media_type: "image/png".into(),
                name: Some("shot.png".into()),
                anchor: None,
                size: None,
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

    fn file_blocks(
        object_id: &str,
        path: &str,
        name: &str,
        mime: &str,
        size: u64,
        anchor: Option<u32>,
    ) -> Vec<ContentBlock> {
        let id = remuda_protocol::Id::try_from(object_id.to_owned()).expect("id");
        vec![
            ContentBlock::File(Box::new(MediaBlock {
                object_id: id.clone(),
                media_type: mime.into(),
                name: Some(name.into()),
                anchor,
                size: Some(size),
            })),
            ContentBlock::Resource(Box::new(ResourceBlock {
                uri: format!("file://{path}"),
                media_type: Knowledge::Known { value: mime.into() },
                object_id: Some(id),
            })),
        ]
    }

    fn object_id() -> String {
        remuda_protocol::Id::new("obj").expect("id").to_string()
    }

    #[test]
    fn an_image_block_resolves_through_its_resource_block() {
        let id = object_id();
        let found = attachments_of(&image_blocks(&id, "/data/attachments/shot.png"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].object_id, id);
        assert_eq!(found[0].kind, AttachmentKind::Image);
        assert_eq!(found[0].media_type, "image/png");
        assert_eq!(found[0].display_path(), "/data/attachments/shot.png");
    }

    #[test]
    fn a_file_block_resolves_with_kind_name_and_size() {
        let id = object_id();
        let mut blocks = file_blocks(
            &id,
            "/data/attachments/report.pdf",
            "report.pdf",
            "application/pdf",
            24 * 1024,
            Some(1),
        );
        blocks.push(ContentBlock::Text(Box::new(TextBlock {
            text: "summarise".into(),
        })));
        let found = attachments_of(&blocks);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, AttachmentKind::File);
        assert_eq!(found[0].name, "report.pdf");
        assert_eq!(found[0].byte_len, Some(24 * 1024));
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
        let mut only_image = image_blocks(&object_id(), "/data/shot.png");
        only_image.remove(1);
        assert!(attachments_of(&only_image).is_empty());
    }

    /// A resource block naming a different object must not be borrowed.
    #[test]
    fn a_resource_block_for_another_object_does_not_match() {
        let mut mismatched = image_blocks(&object_id(), "/data/shot.png");
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
        let mut remote = image_blocks(&id, "/data/shot.png");
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
    fn image_path_mentions_instruct_the_agent_to_read_the_file() {
        let mentioned =
            text_with_path_mentions(&image_blocks(&object_id(), "/data/attachments/shot.png"))
                .expect("text");
        assert!(mentioned.starts_with("what colour is the image?"));
        assert!(mentioned.contains("请读取"));
        assert!(
            mentioned.contains("附件: /data/attachments/shot.png"),
            "{mentioned}"
        );
    }

    /// File references precede the user text; image footers follow it.
    #[test]
    fn file_lines_lead_the_prompt_and_image_mentions_follow() {
        let id_1 = object_id();
        let id_2 = object_id();
        let mut blocks = file_blocks(
            &id_1,
            "/data/attachments/report.pdf",
            "report.pdf",
            "application/pdf",
            1_536_000,
            Some(1),
        );
        blocks.extend(image_blocks(&id_2, "/data/attachments/shot.png"));
        let mentioned = text_with_path_mentions(&blocks).expect("text");
        let file_line = mentioned
            .lines()
            .find(|line| line.contains("saved at"))
            .expect("file line");
        assert_eq!(
            file_line,
            "[File #1] report.pdf (application/pdf, 1.5 MB) saved at \
             /data/attachments/report.pdf"
        );
        let file_at = mentioned.find(file_line).unwrap();
        let text_at = mentioned.find("what colour is the image?").unwrap();
        let footer_at = mentioned.find("请读取").unwrap();
        assert!(file_at < text_at, "file line precedes user text: {mentioned}");
        assert!(text_at < footer_at, "image footer follows user text: {mentioned}");
    }

    /// Unanchored files keep the same line without a `[File #n]` prefix.
    #[test]
    fn unanchored_file_line_has_no_token() {
        let id = object_id();
        let blocks = file_blocks(
            &id,
            "/data/notes.txt",
            "notes.txt",
            "text/plain",
            42,
            None,
        );
        let mentioned = text_with_path_mentions(&blocks).expect("text");
        let first = mentioned.lines().next().expect("first line");
        assert_eq!(
            first,
            "notes.txt (text/plain, 42 B) saved at /data/notes.txt"
        );
    }

    /// Numbered image sends keep the prompt's [Image #n] numbering on each
    /// line and name the objectId, so the MCP path and the fallback agree.
    #[test]
    fn numbered_image_mentions_pair_the_token_number_with_object_and_path() {
        let id_1 = remuda_protocol::Id::new("obj").expect("id");
        let id_2 = remuda_protocol::Id::new("obj").expect("id");
        let media = |id: &remuda_protocol::Id, mime: &str, name: &str, anchor: u32| {
            ContentBlock::Image(Box::new(MediaBlock {
                object_id: id.clone(),
                media_type: mime.into(),
                name: Some(name.into()),
                anchor: Some(anchor),
                size: None,
            }))
        };
        let resource = |id: &remuda_protocol::Id, mime: &str, path: &str| {
            ContentBlock::Resource(Box::new(ResourceBlock {
                uri: format!("file://{path}"),
                media_type: Knowledge::Known { value: mime.into() },
                object_id: Some(id.clone()),
            }))
        };
        let blocks = vec![
            media(&id_1, "image/png", "a.png", 1),
            resource(&id_1, "image/png", "/data/a.png"),
            media(&id_2, "image/jpeg", "b.jpg", 2),
            resource(&id_2, "image/jpeg", "/data/b.jpg"),
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

    /// A file attachment with no accompanying text still produces a usable
    /// prompt rather than failing as "no text blocks".
    #[test]
    fn a_file_alone_still_produces_a_prompt() {
        let id = object_id();
        let blocks = file_blocks(&id, "/data/report.pdf", "report.pdf", "application/pdf", 10, None);
        let mentioned = text_with_path_mentions(&blocks).expect("text");
        assert!(
            mentioned.contains("report.pdf (application/pdf, 10 B) saved at /data/report.pdf"),
            "{mentioned}"
        );
    }

    #[test]
    fn human_sizes_are_compact() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2 KB");
        assert_eq!(human_size(1_536_000), "1.5 MB");
    }

    #[test]
    fn an_empty_prompt_is_still_rejected() {
        assert!(text_with_path_mentions(&[]).is_err());
    }
}
