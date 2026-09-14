//! Read Codex's completed-item rollout JSONL without depending on its wire crate.
//!
//! This pre-spike is deliberately not connected to a driver. The eventual
//! `SignalAdapter` integration belongs to `docs/design/native-pty-first.md` §4.2.
//! Rollouts are item-level evidence, not token deltas or an approval transport.

use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// One source record. Missing source timestamps/ordinals stay unknown rather
/// than being synthesized from wall time or the reader's line count.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexRolloutRecord {
    /// Unmodified source timestamp, when present.
    pub timestamp: Option<String>,
    /// Source ordinal, or an explicit caller fallback from `parse_rollout_line_at`.
    pub ordinal: Option<u64>,
    /// Outer record type, distinguishing response items from event messages.
    pub record_type: String,
    /// Recognized payload or its unknown type name.
    pub event: CodexRolloutEvent,
}

/// Recognized semantic records. Optional fields tolerate older/sparse rollouts;
/// rich payloads retain their original JSON shape for later adapter work.
#[derive(Debug, Clone, PartialEq)]
pub enum CodexRolloutEvent {
    /// Session header and identity.
    SessionMeta {
        /// `payload.id` is the thread ID; newer `session_id` can name its root.
        session_id: Option<String>,
        /// Complete metadata, including version, source, and provider.
        metadata: Value,
    },
    /// A turn began.
    TaskStarted {
        /// Turn ID carried by this record.
        turn_id: Option<String>,
        /// Reported model context window.
        model_context_window: Option<u64>,
    },
    /// A turn completed, possibly with an error.
    TaskComplete {
        /// Turn ID carried by this record.
        turn_id: Option<String>,
        /// Last assistant text, when Codex supplies it.
        last_agent_message: Option<String>,
        /// Terminal failure, when present; completion does not imply success.
        error: Option<Value>,
    },
    /// A completed typed turn item.
    ItemCompleted {
        /// Turn ID carried by this record.
        turn_id: Option<String>,
        /// Unmodified item, including its ID, type, and content.
        item: Value,
    },
    /// Text from a persisted message; non-text blocks are omitted.
    Message {
        /// Source role, or user/assistant for the corresponding event type.
        role: String,
        /// Text blocks joined in source order with newlines.
        text: String,
    },
    /// Available reasoning summary, falling back to visible reasoning content.
    Reasoning {
        /// Empty when only encrypted reasoning is persisted.
        text: String,
    },
    /// A completed function, custom tool, or legacy local shell invocation.
    ToolCall {
        /// Native correlation ID; never replaced with a message/item ID.
        call_id: Option<String>,
        /// Native tool name, or `local_shell` for a legacy shell invocation.
        name: Option<String>,
        /// Function arguments are usually a JSON-encoded string; do not coerce
        /// that string into JSON or discard a custom tool's freeform input.
        input: Value,
    },
    /// A persisted tool result.
    ToolOutput {
        /// Native correlation ID shared with the invocation.
        call_id: Option<String>,
        /// Raw string or structured content, without lossy conversion.
        output: Value,
    },
    /// A usage snapshot or per-response usage record.
    TokenUsage {
        /// Distinguishes cumulative `token_count` from `token_usage_record`.
        source_type: String,
        /// Complete counters and attribution; no aggregation is inferred.
        data: Value,
    },
    /// Effective context recorded by Codex for a turn.
    TurnContext {
        /// Turn ID carried by this record.
        turn_id: Option<String>,
        /// Reported effective model.
        model: Option<String>,
        /// Reported effective reasoning effort.
        effort: Option<String>,
        /// Complete context, including approval and sandbox policies.
        data: Value,
    },
    /// Context compaction occurred.
    Compacted,
    /// A turn was aborted before completion (`event_msg/turn_aborted`).
    ///
    /// P6: the file adapter needs the turn id and reason (observed
    /// `reason:"interrupted"` after `Esc`) to journal an authoritative turn
    /// boundary — an aborted turn emits no `task_complete`.
    TurnAborted {
        /// Turn id carried by the record.
        turn_id: Option<String>,
        /// Abort reason, e.g. `interrupted`.
        reason: Option<String>,
    },
    /// A record outside this parser's recognized subset.
    Unknown {
        /// Nested payload type for event/response records, outer type otherwise.
        r#type: String,
    },
}

/// Decode a complete JSONL line. Unknown types/fields are tolerated; malformed
/// JSON remains an error so the caller can record it without aborting its tail.
/// Duplicate `event_msg`/`response_item` representations are not deduplicated,
/// and records without a turn ID are never assigned to an inferred turn.
pub fn parse_rollout_line(line: &str) -> Result<CodexRolloutRecord, serde_json::Error> {
    let value: Value = serde_json::from_str(line)?;
    let record_type = string(&value, "type").unwrap_or_default();
    let payload = &value["payload"];
    let event = match record_type.as_str() {
        "session_meta" => CodexRolloutEvent::SessionMeta {
            session_id: string(payload, "id").or_else(|| string(payload, "session_id")),
            metadata: payload.clone(),
        },
        "turn_context" => CodexRolloutEvent::TurnContext {
            turn_id: string(payload, "turn_id"),
            model: string(payload, "model"),
            effort: string(payload, "effort"),
            data: payload.clone(),
        },
        "compacted" => CodexRolloutEvent::Compacted,
        "token_usage_record" => CodexRolloutEvent::TokenUsage {
            source_type: record_type.clone(),
            data: payload.clone(),
        },
        "event_msg" => parse_event(payload),
        "response_item" => parse_item(payload),
        _ => CodexRolloutEvent::Unknown {
            r#type: record_type.clone(),
        },
    };
    Ok(CodexRolloutRecord {
        timestamp: string(&value, "timestamp"),
        ordinal: value["ordinal"].as_u64(),
        record_type,
        event,
    })
}

/// Decode with a caller-supplied ordinal for older records that omit one.
/// A source ordinal wins when present; the caller owns its fallback numbering
/// (for example a zero-based physical line index, counting blank/invalid lines).
pub fn parse_rollout_line_at(
    line: &str,
    ordinal: u64,
) -> Result<CodexRolloutRecord, serde_json::Error> {
    let mut record = parse_rollout_line(line)?;
    record.ordinal.get_or_insert(ordinal);
    Ok(record)
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

fn parse_event(payload: &Value) -> CodexRolloutEvent {
    let kind = payload["type"].as_str().unwrap_or_default();
    match kind {
        "task_started" | "turn_started" => CodexRolloutEvent::TaskStarted {
            turn_id: string(payload, "turn_id"),
            model_context_window: payload["model_context_window"].as_u64(),
        },
        "task_complete" | "turn_complete" => CodexRolloutEvent::TaskComplete {
            turn_id: string(payload, "turn_id"),
            last_agent_message: string(payload, "last_agent_message"),
            error: payload.get("error").filter(|v| !v.is_null()).cloned(),
        },
        "item_completed" => CodexRolloutEvent::ItemCompleted {
            turn_id: string(payload, "turn_id"),
            item: payload["item"].clone(),
        },
        "user_message" | "agent_message" => CodexRolloutEvent::Message {
            role: if kind == "user_message" {
                "user"
            } else {
                "assistant"
            }
            .into(),
            text: string(payload, "message").unwrap_or_default(),
        },
        "agent_reasoning" | "agent_reasoning_raw_content" => CodexRolloutEvent::Reasoning {
            text: string(payload, "text").unwrap_or_default(),
        },
        "token_count" | "token_usage_record" => CodexRolloutEvent::TokenUsage {
            source_type: kind.into(),
            data: payload.clone(),
        },
        "context_compacted" => CodexRolloutEvent::Compacted,
        "turn_aborted" => CodexRolloutEvent::TurnAborted {
            turn_id: string(payload, "turn_id"),
            reason: string(payload, "reason"),
        },
        _ => CodexRolloutEvent::Unknown {
            r#type: kind.into(),
        },
    }
}

fn parse_item(payload: &Value) -> CodexRolloutEvent {
    let kind = payload["type"].as_str().unwrap_or_default();
    match kind {
        "message" => CodexRolloutEvent::Message {
            role: string(payload, "role").unwrap_or_default(),
            text: text_blocks(&payload["content"]),
        },
        "reasoning" => {
            let text = text_blocks(&payload["summary"]);
            CodexRolloutEvent::Reasoning {
                text: if text.is_empty() {
                    text_blocks(&payload["content"])
                } else {
                    text
                },
            }
        }
        "function_call" | "custom_tool_call" | "local_shell_call" => CodexRolloutEvent::ToolCall {
            call_id: string(payload, "call_id"),
            name: string(payload, "name")
                .or_else(|| (kind == "local_shell_call").then(|| "local_shell".into())),
            input: payload
                .get("arguments")
                .or_else(|| payload.get("input"))
                .or_else(|| payload.get("action"))
                .cloned()
                .unwrap_or(Value::Null),
        },
        "function_call_output" | "custom_tool_call_output" => CodexRolloutEvent::ToolOutput {
            call_id: string(payload, "call_id"),
            output: payload["output"].clone(),
        },
        _ => CodexRolloutEvent::Unknown {
            r#type: kind.into(),
        },
    }
}

fn text_blocks(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Incremental byte-offset reader for one append-only rollout. Like
/// `TranscriptTail`, it returns only newline-terminated lines. Partial UTF-8
/// stays as bytes until its whole line arrives. Shrinking files reset the tail;
/// replacement with a same-size/larger file requires constructing a new reader.
#[derive(Debug)]
pub struct RolloutTail {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
}

impl RolloutTail {
    /// Start following a rollout at byte zero.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            partial: Vec::new(),
        }
    }

    /// File being followed.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes read, including an incomplete line buffered for the next poll.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Read appended whole lines, omitting empty lines and trimming CRLF.
    /// Invalid UTF-8 in a complete line is replaced with the replacement char.
    pub fn poll(&mut self) -> std::io::Result<Vec<String>> {
        let mut file = std::fs::File::open(&self.path)?;
        if file.metadata()?.len() < self.offset {
            self.offset = 0;
            self.partial.clear();
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut appended = Vec::new();
        self.offset += file.read_to_end(&mut appended)? as u64;
        self.partial.extend_from_slice(&appended);
        let mut lines = Vec::new();
        let mut consumed = 0;
        for (index, byte) in self.partial.iter().enumerate() {
            if *byte == b'\n' {
                let line = String::from_utf8_lossy(&self.partial[consumed..index]);
                let line = line.trim_end_matches('\r');
                if !line.is_empty() {
                    lines.push(line.to_owned());
                }
                consumed = index + 1;
            }
        }
        self.partial.drain(..consumed);
        Ok(lines)
    }
}

/// Locate a thread's rollout under `$CODEX_HOME`, falling back to `$HOME/.codex`.
/// This never assumes the observer's `CODEX_THREAD_ID` belongs to a target PID.
#[must_use]
pub fn locate_rollout(session_id: &str) -> Option<PathBuf> {
    let home = std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|v| PathBuf::from(v).join(".codex")))?;
    locate_rollout_in(&home, session_id)
}

/// Explicit-home variant for shadow homes/tests; searches active then archived
/// rollouts without following symlinks. `session_index.jsonl` has no rollout path
/// and does not establish PID ownership. Only a matching `session_meta.id` does
/// establish the file's thread identity (root `session_id` alone does not).
/// Multiple matching active files, or multiple archived files with no active
/// match, are ambiguous and return `None`. Revert can leave such files behind;
/// callers need live PID/file evidence or Codex's SQLite state to resolve them.
#[must_use]
pub fn locate_rollout_in(codex_home: &Path, session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() || session_id.contains(['/', '\\']) {
        return None;
    }
    for root in ["sessions", "archived_sessions"] {
        let mut found = None;
        let mut pending = vec![codex_home.join(root)];
        while let Some(dir) = pending.pop() {
            if !std::fs::symlink_metadata(&dir).is_ok_and(|metadata| metadata.is_dir()) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut paths = entries.flatten().collect::<Vec<_>>();
            paths.sort_by_key(std::fs::DirEntry::file_name);
            for entry in paths {
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    pending.push(entry.path());
                }
                if !kind.is_file() || entry.path().extension().is_none_or(|v| v != "jsonl") {
                    continue;
                }
                // Read just the metadata line, bounded even for an invalid file.
                let Ok(file) = std::fs::File::open(entry.path()) else {
                    continue;
                };
                let mut first = String::new();
                if std::io::BufReader::new(file)
                    .take(1024 * 1024)
                    .read_line(&mut first)
                    .is_err()
                {
                    continue;
                }
                if let Ok(CodexRolloutRecord {
                    event: CodexRolloutEvent::SessionMeta { metadata, .. },
                    ..
                }) = parse_rollout_line(&first)
                    && metadata["id"].as_str() == Some(session_id)
                {
                    if found.is_some() {
                        return None;
                    }
                    found = Some(entry.path());
                }
            }
        }
        if found.is_some() {
            return found;
        }
    }
    None
}
