//! One-shot, bounded read of one subagent's own transcript.
//!
//! Subagents (Workflow members and plain Agent/Task tasks) are Claude
//! sub-sessions inside the SAME Remuda session — never Remuda instances. Their
//! full conversation lands in a sidechain transcript the main-transcript
//! mapper deliberately skips (`isSidechain`):
//!
//! - Workflow members: `<session>/subagents/workflows/wf_<run>/agent-<id>.jsonl`
//! - Plain Agent tasks: `<session>/subagents/agent-<id>.jsonl`
//!
//! The live journal never streams these bytes (the live budget stays hook +
//! main transcript); the drill-in view fetches them on demand through this
//! reader, which maps each record through the SAME pipeline the main
//! transcript uses ([`map_claude_line`]).

use crate::claude::{NativeIds, map_claude_line};
use crate::envelope::Envelope;
use crate::error::Error;
use crate::source::MapContext;
use crate::util::{digest_of, parse_timestamp};
use remuda_protocol::{FileCursor, Id, Knowledge, SourceChannel, Timestamp};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// An `agent-<id>.jsonl` larger than this is not read; the drill-in degrades
/// to a "transcript unavailable" note. Matches the live tailer's cap.
pub const MAX_SUBAGENT_JSONL: u64 = 8 * 1024 * 1024;

/// Which kind of subagent the located transcript belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentKind {
    /// A Workflow member, with the native run id (`wf_…`).
    Workflow {
        /// Native workflow run id.
        run_id: String,
    },
    /// A plain foreground/background Agent (Task) tool subagent.
    Agent,
}

/// Header facts folded off the raw sidechain records for the drill-in header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubagentTranscriptMeta {
    /// Native agent id, echoed back.
    pub agent_id: String,
    /// Workflow run id when the file lived under `workflows/wf_*`.
    pub run_id: Option<String>,
    /// The prompt literal (first user record's text).
    pub prompt: Option<String>,
    /// Last reported model id.
    pub model: Option<String>,
    /// Summed token usage (input + output + cache) across assistant records.
    pub tokens: Option<u64>,
    /// Number of `tool_use` blocks.
    pub calls: u64,
    /// Name of the last tool used.
    pub latest_tool: Option<String>,
    /// First record timestamp.
    pub started_at: Option<Timestamp>,
    /// Last record timestamp.
    pub ended_at: Option<Timestamp>,
    /// Last assistant text block, the subagent's final words.
    pub final_text: Option<String>,
}

/// A located, mapped subagent transcript.
pub struct SubagentTranscript {
    /// Header facts.
    pub meta: SubagentTranscriptMeta,
    /// Mapped observations in file order, ready for the web's own assembler.
    pub envelopes: Vec<Envelope>,
}

/// The agent id charset harnesses actually emit (hex-ish ids); validated so a
/// caller-supplied id can never traverse out of the session directory.
fn valid_agent_id(agent_id: &str) -> bool {
    !agent_id.is_empty()
        && agent_id.len() <= 128
        && agent_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Locate `agent-<id>.jsonl` under one session directory.
///
/// Workflow run directories are checked first (the common case); a plain
/// `subagents/agent-<id>.jsonl` is the fallback for non-workflow Agent tasks.
/// Returns the path and its kind when the file exists.
#[must_use]
pub fn locate_agent_file(session_dir: &Path, agent_id: &str) -> Option<(PathBuf, SubagentKind)> {
    if !valid_agent_id(agent_id) {
        return None;
    }
    let file_name = format!("agent-{agent_id}.jsonl");
    let runs_dir = session_dir.join("subagents").join("workflows");
    if let Ok(entries) = fs::read_dir(&runs_dir) {
        // Sorted so the answer is deterministic if a retried agent id were
        // ever shared between run dirs.
        let mut runs: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        runs.sort();
        for run_dir in runs {
            let candidate = run_dir.join(&file_name);
            if candidate.is_file()
                && let Some(run_id) = run_dir
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .filter(|name| name.starts_with("wf_"))
            {
                return Some((candidate, SubagentKind::Workflow { run_id }));
            }
        }
    }
    let plain = session_dir.join("subagents").join(&file_name);
    if plain.is_file() {
        return Some((plain, SubagentKind::Agent));
    }
    None
}

/// Read and map one subagent transcript.
///
/// `Ok(None)` means the agent has no transcript yet (`启动中` in the UI); an
/// unreadable/oversized file is an error so the caller can distinguish "not
/// started" from "the host cannot serve it".
///
/// # Errors
///
/// Propagates filesystem and mapping errors.
pub fn read_subagent_transcript(
    session_dir: &Path,
    agent_id: &str,
    mut ctx: MapContext,
) -> Result<Option<SubagentTranscript>, Error> {
    if !valid_agent_id(agent_id) {
        return Err(Error::Protocol(format!("invalid agent id: {agent_id:?}")));
    }
    let Some((path, kind)) = locate_agent_file(session_dir, agent_id) else {
        return Ok(None);
    };
    let metadata = fs::metadata(&path)?;
    if metadata.len() > MAX_SUBAGENT_JSONL {
        return Err(Error::Protocol(format!(
            "subagent transcript {agent_id} exceeds {} bytes",
            MAX_SUBAGENT_JSONL
        )));
    }
    // On-demand evidence is a replay even when the context was built for live
    // tailing; the channel stays `transcript` (sidechain file).
    ctx.channel = SourceChannel::Transcript;
    ctx.delivery = remuda_protocol::SourceDelivery::Replay;

    let bytes = fs::read(&path)?;
    let file_identity = Id::new("obj")?;
    let mut ids = NativeIds::new(ctx.instance_id.as_id().as_str());
    let mut envelopes = Vec::new();
    let mut meta = SubagentTranscriptMeta {
        agent_id: agent_id.to_owned(),
        run_id: match &kind {
            SubagentKind::Workflow { run_id } => Some(run_id.clone()),
            SubagentKind::Agent => None,
        },
        ..SubagentTranscriptMeta::default()
    };

    let mut offset: u64 = 0;
    for raw_line in bytes.split(|b| *b == b'\n') {
        let line = raw_line.strip_suffix(b"\r" as &[u8]).unwrap_or(raw_line);
        // Advance the cursor over the line *and* its newline so offsets match
        // what FileTail would report.
        let consumed = line.len() as u64 + 1;
        if line.is_empty() {
            offset += consumed;
            continue;
        }
        let cursor = FileCursor {
            file_identity: file_identity.clone(),
            file_generation: remuda_protocol::U64(1),
            offset: remuda_protocol::U64(offset),
            length: remuda_protocol::U64(line.len() as u64),
            digest: digest_of(line),
        };
        offset += consumed;
        fold_meta(line, &mut meta);
        envelopes.extend(map_claude_line(&ctx, &mut ids, line, &cursor)?);
    }

    Ok(Some(SubagentTranscript { meta, envelopes }))
}

/// Fold the header facts the drill-in shows, mirroring the live tailer's
/// `poll_agent_jsonl` accounting so the two paths reconcile.
fn fold_meta(line: &[u8], meta: &mut SubagentTranscriptMeta) {
    let Ok(value) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    if let Knowledge::Known { value: ts } =
        parse_timestamp(value.get("timestamp").and_then(Value::as_str).unwrap_or(""))
    {
        if meta.started_at.is_none() {
            meta.started_at = Some(ts.clone());
        }
        meta.ended_at = Some(ts);
    }
    let Some(message) = value.get("message") else {
        return;
    };
    if value.get("type").and_then(Value::as_str) == Some("user")
        && meta.prompt.is_none()
        && let Some(text) = content_text(message.get("content").unwrap_or(&Value::Null))
    {
        meta.prompt = Some(text);
    }
    if let Some(model) = message.get("model").and_then(Value::as_str) {
        meta.model = Some(model.to_owned());
    }
    if let Some(usage) = message.get("usage") {
        let total = [
            "input_tokens",
            "output_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ]
        .iter()
        .filter_map(|key| usage.get(key).and_then(Value::as_u64))
        .sum::<u64>();
        if total > 0 {
            meta.tokens = Some(meta.tokens.unwrap_or(0) + total);
        }
    }
    if let Some(blocks) = message.get("content").and_then(Value::as_array) {
        let mut last_text: Option<String> = None;
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("tool_use")
                && let Some(name) = block.get("name").and_then(Value::as_str)
            {
                meta.calls += 1;
                meta.latest_tool = Some(name.to_owned());
            }
            if block.get("type").and_then(Value::as_str) == Some("text")
                && let Some(text) = block.get("text").and_then(Value::as_str)
                && !text.trim().is_empty()
            {
                last_text = Some(text.to_owned());
            }
        }
        if let Some(text) = last_text {
            meta.final_text = Some(text);
        }
    }
}

fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => blocks
            .iter()
            .find_map(|block| block.get("text").and_then(Value::as_str).map(str::to_owned)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_ids_are_path_safe() {
        assert!(valid_agent_id("aae139d44933cefe2"));
        assert!(valid_agent_id("a-b_1"));
        assert!(!valid_agent_id(""));
        assert!(!valid_agent_id("../etc"));
        assert!(!valid_agent_id("a/b"));
        assert!(!valid_agent_id(&"a".repeat(129)));
    }

    #[test]
    fn unknown_agent_is_none_not_error() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = MapContext::claude_file(
            remuda_protocol::InstanceId::new(),
            Id::new("obj").unwrap(),
            remuda_protocol::HostId::new(),
            "sid",
            SourceChannel::Transcript,
        );
        let result = read_subagent_transcript(temp.path(), "deadbeef", ctx).unwrap();
        assert!(result.is_none());
    }
}
