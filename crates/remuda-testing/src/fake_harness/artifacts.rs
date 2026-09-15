//! On-disk artifacts each dialect writes.
//!
//! Shapes mirror the captured sessions the merged parsers were built against:
//! - claude → `~/.claude/projects/<encoded cwd>/<session>.jsonl` (one record
//!   per content block, `apiBlockIndex`, `queue-operation`, `attachment`);
//! - codex → `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl` plus
//!   `session_index.jsonl`;
//! - grok → `$GROK_HOME/sessions/<enc cwd>/<session>/{updates,events}.jsonl`
//!   and `$GROK_HOME/active_sessions.json`.
//!
//! Every writer appends whole JSON lines and flushes, so real
//! `TranscriptTail` / `RolloutTail` / `SessionTail` polls observe records the
//! same way they do against the real binaries.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use remuda_driver::claude_transcript::encode_project_dir;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::fake_harness::clock::FakeClock;
use crate::fake_harness::script::UsageSpec;

/// Files the harness owns for one running session.
pub struct ArtifactSet {
    /// Dialect-specific roots, used at shutdown.
    kind: ArtifactKind,
    /// Claude transcript / codex rollout / grok updates writer.
    main: File,
    /// Grok events writer (only for grok).
    events: Option<File>,
    /// Session metadata shared with the engine.
    pub meta: SessionMeta,
    /// Grok session directory (holds `terminal/` logs and the registry entry).
    grok_dir: Option<PathBuf>,
    /// Session-monotonic ACP event sequence (`<session>-<n>`).
    grok_seq: u64,
}

/// Dialect for the artifact set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactKind {
    /// Claude transcript JSONL.
    Claude,
    /// Codex rollout JSONL.
    Codex,
    /// Grok ACP updates/events JSONL.
    Grok,
}

/// Identity shared by engine and writers.
#[derive(Clone, Debug)]
pub struct SessionMeta {
    /// Session / thread id.
    pub session_id: String,
    /// Working directory.
    pub cwd: PathBuf,
    /// Model label reported in artifacts.
    pub model: String,
    /// Client version string.
    pub version: String,
}

/// Where a fresh session's artifacts live.
pub struct ArtifactPaths {
    /// Main JSONL file.
    pub main: PathBuf,
    /// Grok events JSONL, when applicable.
    pub events: Option<PathBuf>,
    /// Grok session directory, when applicable.
    pub grok_dir: Option<PathBuf>,
    /// `session_index.jsonl` for codex.
    pub session_index: Option<PathBuf>,
}

impl ArtifactSet {
    /// Create all files for a new session.
    pub fn create(
        kind: ArtifactKind,
        home: &Path,
        meta: SessionMeta,
        clock: &FakeClock,
    ) -> std::io::Result<(Self, ArtifactPaths)> {
        let paths = artifact_paths(kind, home, &meta, clock);
        if let Some(parent) = paths.main.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let main = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&paths.main)?;
        let events = if let Some(path) = &paths.events {
            std::fs::create_dir_all(path.parent().expect("events parent"))?;
            Some(OpenOptions::new().create(true).append(true).open(path)?)
        } else {
            None
        };
        let set = Self {
            kind,
            main,
            events,
            meta,
            grok_dir: paths.grok_dir.clone(),
            grok_seq: 0,
        };
        if let Some(path) = &paths.grok_dir {
            std::fs::write(path.join("usage.json"), "{}\n")?;
        }
        Ok((set, paths))
    }

    /// Paths for a session, dialect-specific.
    #[must_use]
    pub fn paths_for(
        kind: ArtifactKind,
        home: &Path,
        meta: &SessionMeta,
        clock: &FakeClock,
    ) -> ArtifactPaths {
        artifact_paths(kind, home, meta, clock)
    }

    /// Resume an existing session by appending to already-open files. The ACP
    /// event sequence restarts at zero rather than scanning the old tail.
    pub fn from_existing(
        kind: ArtifactKind,
        main: File,
        events: Option<File>,
        meta: SessionMeta,
        grok_dir: Option<PathBuf>,
    ) -> std::io::Result<Self> {
        Ok(Self {
            kind,
            main,
            events,
            meta,
            grok_dir,
            grok_seq: 0,
        })
    }

    /// Append one JSON line to the main file.
    pub fn append(&mut self, value: Value) -> std::io::Result<()> {
        writeln!(self.main, "{}", value).and_then(|()| self.main.flush())
    }

    /// Append one JSON line to the grok events file.
    pub fn append_event(&mut self, value: Value) -> std::io::Result<()> {
        if let Some(file) = self.events.as_mut() {
            writeln!(file, "{}", value)?;
            file.flush()?;
        }
        Ok(())
    }

    /// Dialect.
    #[must_use]
    pub fn kind(&self) -> ArtifactKind {
        self.kind
    }

    /// Grok session directory, if any.
    #[must_use]
    pub fn grok_dir(&self) -> Option<&Path> {
        self.grok_dir.as_deref()
    }
}

fn artifact_paths(
    kind: ArtifactKind,
    home: &Path,
    meta: &SessionMeta,
    clock: &FakeClock,
) -> ArtifactPaths {
    match kind {
        ArtifactKind::Claude => {
            let dir = home.join("projects").join(encode_project_dir(&meta.cwd));
            ArtifactPaths {
                main: dir.join(format!("{}.jsonl", meta.session_id)),
                events: None,
                grok_dir: None,
                session_index: None,
            }
        }
        ArtifactKind::Codex => {
            // `rollout-YYYY-MM-DDTHH-MM-SS-<session>.jsonl` under
            // `sessions/YYYY/MM/DD` (the real client uses local date; the
            // fake's clock is UTC and tests pin it explicitly).
            let stamp = clock.rfc3339();
            let date = &stamp[..10];
            let year = &date[..4];
            let month = &date[5..7];
            let day = &date[8..10];
            let file_stamp = format!("{}T{}-{}", date, &stamp[11..13], &stamp[14..16]);
            let file_stamp = format!("{file_stamp}-{}", &stamp[17..19]);
            let dir = home.join("sessions").join(year).join(month).join(day);
            ArtifactPaths {
                main: dir.join(format!("rollout-{file_stamp}-{}.jsonl", meta.session_id)),
                events: None,
                grok_dir: None,
                session_index: Some(home.join("session_index.jsonl")),
            }
        }
        ArtifactKind::Grok => {
            let dir = home
                .join("sessions")
                .join(remuda_driver::grok_session::encode_session_cwd(&meta.cwd))
                .join(&meta.session_id);
            ArtifactPaths {
                main: dir.join("updates.jsonl"),
                events: Some(dir.join("events.jsonl")),
                grok_dir: Some(dir),
                session_index: None,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Claude transcript records
// ---------------------------------------------------------------------------

/// Mutable per-session counters for the claude writer.
#[derive(Debug, Default)]
pub struct ClaudeCounters {
    /// Content-block index within the current assistant message.
    pub api_block_index: u64,
    /// Running per-message assistant uuid parents.
    pub parent_uuid: String,
}

impl ClaudeCounters {
    /// Start a session with a fixed root parent.
    #[must_use]
    pub fn new() -> Self {
        Self {
            api_block_index: 0,
            parent_uuid: Uuid::nil().to_string(),
        }
    }
}

/// Append a claude `user` record (typed prompt or queued delivery).
#[must_use]
pub fn claude_user_record(
    meta: &SessionMeta,
    clock: &FakeClock,
    prompt: &str,
    queued: bool,
) -> Value {
    let mut record = json!({
        "parentUuid": Uuid::nil().to_string(),
        "isSidechain": false,
        "promptId": Uuid::new_v4().to_string(),
        "type": "user",
        "message": { "role": "user", "content": prompt },
        "uuid": Uuid::new_v4().to_string(),
        "timestamp": clock.rfc3339(),
        "permissionMode": "default",
        "origin": { "kind": "human" },
        "promptSource": if queued { "queued" } else { "typed" },
        "userType": "external",
        "entrypoint": "cli",
        "sessionId": meta.session_id,
        "version": meta.version,
        "gitBranch": "HEAD"
    });
    let _ = queued;
    if let Some(obj) = record.as_object_mut() {
        obj.insert("session_id".into(), json!(meta.session_id));
    }
    record
}

/// One assistant content block record (`text`, `thinking`, or `tool_use`).
///
/// `effort` stamps the top-level `effort` / `perTurnEffort` fields a real claude
/// transcript carries (D-028 §9.1), so the driver effort read-back can be
/// exercised against the fake harness. `None` omits both fields.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn claude_assistant_block(
    meta: &SessionMeta,
    clock: &FakeClock,
    message_id: &str,
    api_block_index: u64,
    block: Value,
    stop_reason: &str,
    usage: &UsageSpec,
    effort: Option<&str>,
) -> Value {
    let mut record = json!({
        "parentUuid": Uuid::nil().to_string(),
        "isSidechain": false,
        "message": {
            "model": meta.model,
            "id": message_id,
            "type": "message",
            "role": "assistant",
            "content": [block],
            "container": null,
            "stop_reason": stop_reason,
            "stop_sequence": null,
            "stop_details": null,
            "usage": claude_usage(usage)
        },
        "apiBlockIndex": api_block_index,
        "type": "assistant",
        "uuid": Uuid::new_v4().to_string(),
        "timestamp": clock.rfc3339(),
        "session_id": meta.session_id,
        "sessionId": meta.session_id,
        "userType": "external",
        "entrypoint": "cli",
        "version": meta.version,
        "gitBranch": "HEAD"
    });
    if let Some(effort) = effort
        && let Some(obj) = record.as_object_mut()
    {
        obj.insert("effort".into(), json!(effort));
        obj.insert("perTurnEffort".into(), Value::Null);
    }
    record
}

/// The `user` records a typed `/effort <word>` produces in a real claude
/// transcript: the command markup and the local-command stdout line (D-028
/// §9.1), sharing one `promptId` as the real binary writes them.
#[must_use]
pub fn claude_effort_slash_records(
    meta: &SessionMeta,
    clock: &FakeClock,
    word: &str,
    stdout: &str,
) -> Vec<Value> {
    let prompt_id = Uuid::new_v4().to_string();
    let make = |uuid: &str, content: &str| {
        json!({
            "parentUuid": Uuid::nil().to_string(),
            "isSidechain": false,
            "promptId": prompt_id,
            "type": "user",
            "message": { "role": "user", "content": content },
            "uuid": uuid,
            "timestamp": clock.rfc3339(),
            "userType": "external",
            "entrypoint": "cli",
            "sessionId": meta.session_id,
            "session_id": meta.session_id,
            "version": meta.version,
            "gitBranch": "HEAD"
        })
    };
    vec![
        make(
            &Uuid::new_v4().to_string(),
            &format!(
                "<command-name>/effort</command-name>\n<command-message>effort</command-message>\n\
                 <command-args>{word}</command-args>"
            ),
        ),
        make(
            &Uuid::new_v4().to_string(),
            &format!("<local-command-stdout>{stdout}</local-command-stdout>"),
        ),
    ]
}

fn claude_usage(usage: &UsageSpec) -> Value {
    json!({
        "input_tokens": usage.input_tokens.unwrap_or(42),
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": usage.cached_tokens.unwrap_or(0),
        "output_tokens": usage.output_tokens.unwrap_or(17),
        "output_tokens_details": { "thinking_tokens": usage.reasoning_tokens.unwrap_or(0) },
        "service_tier": "standard"
    })
}

/// A claude `queue-operation` line.
#[must_use]
pub fn claude_queue_op(
    meta: &SessionMeta,
    clock: &FakeClock,
    operation: &str,
    content: Option<&str>,
    reason: Option<&str>,
) -> Value {
    let mut record = json!({
        "type": "queue-operation",
        "operation": operation,
        "timestamp": clock.rfc3339(),
        "sessionId": meta.session_id
    });
    if let Some(content) = content {
        record["content"] = json!(content);
    }
    if let Some(reason) = reason {
        record["reason"] = json!(reason);
    }
    record
}

/// A claude `attachment.queued_command` line. The attachment retains the
/// **enqueue** timestamp even when written after the tool result.
#[must_use]
pub fn claude_queued_attachment(
    meta: &SessionMeta,
    enqueue_timestamp: &str,
    prompt: &str,
) -> Value {
    json!({
        "parentUuid": Uuid::nil().to_string(),
        "isSidechain": false,
        "attachment": {
            "type": "queued_command",
            "prompt": prompt,
            "source_uuid": Uuid::new_v4().to_string(),
            "commandMode": "prompt",
            "origin": { "kind": "human" },
            "timestamp": enqueue_timestamp
        },
        "type": "attachment",
        "uuid": Uuid::new_v4().to_string(),
        "timestamp": enqueue_timestamp,
        "rendered": [{
            "content": format!(
                "<system-reminder>\nThe user sent a new message while you were working:\n{prompt}\n</system-reminder>"
            )
        }],
        "session_id": meta.session_id,
        "sessionId": meta.session_id,
        "userType": "external",
        "entrypoint": "cli",
        "version": meta.version,
        "gitBranch": "HEAD"
    })
}

/// Tool-result `user` record.
#[must_use]
pub fn claude_tool_result(
    meta: &SessionMeta,
    clock: &FakeClock,
    tool_use_id: &str,
    source_assistant_uuid: &str,
    result: &Value,
    is_error: bool,
) -> Value {
    let text = match result {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let content = if text.is_empty() {
        "(Bash completed with no output)".to_owned()
    } else {
        text
    };
    json!({
        "parentUuid": Uuid::nil().to_string(),
        "isSidechain": false,
        "type": "user",
        "message": {
            "role": "user",
            "content": [{
                "tool_use_id": tool_use_id,
                "type": "tool_result",
                "content": content,
                "is_error": is_error
            }]
        },
        "uuid": Uuid::new_v4().to_string(),
        "timestamp": clock.rfc3339(),
        "toolUseResult": {
            "stdout": if is_error { "" } else { &content },
            "stderr": "",
            "interrupted": false,
            "isImage": false,
            "noOutputExpected": content.is_empty()
        },
        "sourceToolAssistantUUID": source_assistant_uuid,
        "session_id": meta.session_id,
        "sessionId": meta.session_id,
        "userType": "external",
        "entrypoint": "cli",
        "version": meta.version,
        "gitBranch": "HEAD"
    })
}

// ---------------------------------------------------------------------------
// Codex rollout records
// ---------------------------------------------------------------------------

/// Mutable per-session codex counters.
#[derive(Debug)]
pub struct CodexCounters {
    /// Source ordinal written onto every record.
    pub ordinal: u64,
}

impl Default for CodexCounters {
    fn default() -> Self {
        Self { ordinal: 1 }
    }
}

impl CodexCounters {
    /// First writable ordinal (session_meta is ordinal 0).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl ArtifactSet {
    /// Write a rollout line with timestamp/ordinal envelope for a top-level
    /// record whose outer `type` is the payload's own type
    /// (`token_usage_record`, `turn_context`, `world_state`, …).
    pub fn codex_top_record(
        &mut self,
        payload: Value,
        clock: &FakeClock,
        counters: &mut CodexCounters,
    ) -> std::io::Result<()> {
        let ty = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let record = json!({
            "timestamp": clock.rfc3339(),
            "ordinal": counters.ordinal,
            "type": ty,
            "payload": payload
        });
        counters.ordinal += 1;
        self.append(record)
    }

    /// Write a rollout line with timestamp/ordinal envelope.
    pub fn codex_record(
        &mut self,
        payload: Value,
        clock: &FakeClock,
        counters: &mut CodexCounters,
    ) -> std::io::Result<()> {
        let record = json!({
            "timestamp": clock.rfc3339(),
            "ordinal": counters.ordinal,
            "type": payload_type_envelope(&payload),
            "payload": payload
        });
        counters.ordinal += 1;
        self.append(record)
    }
}

/// The outer envelope `type` is the payload type for top-level records and
/// `event_msg` / `response_item` for nested ones.
fn payload_type_envelope(payload: &Value) -> String {
    match payload.get("type").and_then(Value::as_str) {
        Some(
            "task_started"
            | "task_complete"
            | "item_completed"
            | "token_count"
            | "turn_aborted"
            | "thread_settings_applied",
        ) => "event_msg".to_owned(),
        Some("message" | "reasoning" | "function_call" | "function_call_output") => {
            "response_item".to_owned()
        }
        Some(other) => other.to_owned(),
        None => "unknown".to_owned(),
    }
}

/// Codex session_meta header.
#[must_use]
pub fn codex_session_meta(meta: &SessionMeta, clock: &FakeClock) -> Value {
    json!({
        "session_id": meta.session_id,
        "id": meta.session_id,
        "timestamp": clock.rfc3339(),
        "cwd": meta.cwd.to_string_lossy(),
        "originator": "codex-tui",
        "cli_version": meta.version,
        "source": "cli",
        "thread_source": "user",
        "model_provider": "fake",
        "history_mode": "paginated"
    })
}

/// `event_msg/task_started`.
#[must_use]
pub fn codex_task_started(meta: &SessionMeta, clock: &FakeClock, turn_id: &str) -> Value {
    json!({
        "type": "task_started",
        "turn_id": turn_id,
        "started_at": clock.secs(),
        "model_context_window": 258_400,
        "collaboration_mode_kind": "default",
        "model": meta.model
    })
}

/// `response_item/message` for a user prompt (plain text, no environment context).
#[must_use]
pub fn codex_user_message(turn_id: &str, prompt: &str) -> Value {
    json!({
        "type": "message",
        "id": format!("msg_{}", Uuid::new_v4().simple()),
        "role": "user",
        "content": [{ "type": "input_text", "text": prompt }],
        "internal_chat_message_metadata_passthrough": { "turn_id": turn_id }
    })
}

/// `event_msg/item_completed` with a `UserMessage` item.
#[must_use]
pub fn codex_user_item_completed(
    meta: &SessionMeta,
    clock: &FakeClock,
    turn_id: &str,
    prompt: &str,
) -> Value {
    let _ = meta;
    let now_ms = clock.ms() as u64;
    json!({
        "type": "item_completed",
        "thread_id": meta.session_id,
        "turn_id": turn_id,
        "item": {
            "type": "UserMessage",
            "id": Uuid::new_v4().to_string(),
            "content": [{ "type": "text", "text": prompt, "text_elements": [] }]
        },
        "started_at_ms": now_ms,
        "completed_at_ms": now_ms
    })
}

/// `response_item/function_call`; arguments are a JSON-encoded string.
#[must_use]
pub fn codex_function_call(call_id: &str, name: &str, input: &Value, turn_id: &str) -> Value {
    json!({
        "type": "function_call",
        "id": format!("fc_{}", Uuid::new_v4().simple()),
        "name": name,
        "arguments": input.to_string(),
        "call_id": call_id,
        "internal_chat_message_metadata_passthrough": { "turn_id": turn_id }
    })
}

/// `event_msg/item_completed` for a CommandExecution item.
#[must_use]
/// Outcome of one codex command, either a normal exit or a hook rejection.
#[derive(Clone, Copy, Debug)]
pub struct CommandOutcome<'a> {
    /// Captured stdout.
    pub output: &'a str,
    /// Process exit code (null in the artifact when rejected).
    pub exit_code: i32,
    /// Rejection message from a PermissionRequest/PreToolUse deny.
    pub rejected: Option<&'a str>,
}

/// `event_msg/item_completed` for a CommandExecution item.
#[must_use]
pub fn codex_command_item_completed(
    meta: &SessionMeta,
    clock: &FakeClock,
    turn_id: &str,
    call_id: &str,
    command: &str,
    outcome: CommandOutcome<'_>,
) -> Value {
    let now_ms = clock.ms() as u64;
    let status = if let Some(reason) = outcome.rejected {
        json!({ "type": "error", "message": format!("Rejected(\"{reason}\")") })
    } else {
        json!("completed")
    };
    json!({
        "type": "item_completed",
        "thread_id": meta.session_id,
        "turn_id": turn_id,
        "item": {
            "type": "CommandExecution",
            "id": call_id,
            "process_id": 0,
            "command": [{ "command": command, "arguments": [] }],
            "cwd": meta.cwd.to_string_lossy(),
            "status": status,
            "stdout": outcome.output,
            "stderr": "",
            "aggregated_output": outcome.output,
            "exit_code": if outcome.rejected.is_some() { Value::Null } else { json!(outcome.exit_code) },
            "duration_ms": 0
        },
        "started_at_ms": now_ms,
        "completed_at_ms": now_ms
    })
}

/// `response_item/function_call_output`.
#[must_use]
pub fn codex_function_output(
    call_id: &str,
    output: &str,
    rejected: Option<&str>,
    turn_id: &str,
) -> Value {
    let text = rejected.map_or_else(
        || output.to_owned(),
        |reason| format!("exec_command failed: Rejected(\"{reason}\")"),
    );
    json!({
        "type": "function_call_output",
        "id": format!("fco_{}", Uuid::new_v4().simple()),
        "call_id": call_id,
        "output": text,
        "internal_chat_message_metadata_passthrough": { "turn_id": turn_id }
    })
}

/// Assistant message item.
#[must_use]
pub fn codex_assistant_message(turn_id: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "id": "msg_spike".to_owned() + &Uuid::new_v4().simple().to_string(),
        "role": "assistant",
        "content": [{ "type": "output_text", "text": text }],
        "internal_chat_message_metadata_passthrough": {
            "turn_id": turn_id,
            "content_item_kinds": ["unknown"]
        }
    })
}

/// `token_usage_record`.
#[must_use]
pub fn codex_token_usage(
    meta: &SessionMeta,
    turn_id: &str,
    response_id: &str,
    usage: &UsageSpec,
) -> Value {
    let input = usage.input_tokens.unwrap_or(100);
    let output = usage.output_tokens.unwrap_or(20);
    let reasoning = usage.reasoning_tokens.unwrap_or(5);
    let cached = usage.cached_tokens.unwrap_or(10);
    let total = input + output;
    let counters = json!({
        "input_tokens": input,
        "cached_input_tokens": cached,
        "cache_write_input_tokens": 0,
        "output_tokens": output,
        "reasoning_output_tokens": reasoning,
        "total_tokens": total
    });
    json!({
        "type": "token_usage_record",
        "thread_id": meta.session_id,
        "turn_id": turn_id,
        "session_id": meta.session_id,
        "root_turn_id": turn_id,
        "response_id": response_id,
        "usage": counters,
        "turn_token_usage": counters,
        "thread_token_usage": counters
    })
}

/// `event_msg/task_complete`.
#[must_use]
pub fn codex_task_complete(turn_id: &str, last_message: &str, clock: &FakeClock) -> Value {
    json!({
        "type": "task_complete",
        "turn_id": turn_id,
        "last_agent_message": last_message,
        "started_at": clock.secs(),
        "completed_at": clock.secs(),
        "duration_ms": 0,
        "time_to_first_token_ms": 0
    })
}

/// `event_msg/turn_aborted` with reason `interrupted`.
#[must_use]
pub fn codex_turn_aborted(turn_id: &str, clock: &FakeClock) -> Value {
    json!({
        "type": "turn_aborted",
        "turn_id": turn_id,
        "reason": "interrupted",
        "started_at": clock.secs(),
        "completed_at": clock.secs(),
        "duration_ms": 0
    })
}

/// Append a `session_index.jsonl` name entry.
pub fn codex_session_index(
    path: &Path,
    meta: &SessionMeta,
    name: &str,
    clock: &FakeClock,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(
        file,
        "{}",
        json!({
            "id": meta.session_id,
            "thread_name": name,
            "updated_at": clock.rfc3339()
        })
    )
}

// ---------------------------------------------------------------------------
// Grok updates / events
// ---------------------------------------------------------------------------

/// Per-turn grok ids.
#[derive(Clone, Debug)]
pub struct GrokTurnIds {
    /// ACP prompt id.
    pub prompt_id: String,
}

impl Default for GrokTurnIds {
    fn default() -> Self {
        Self {
            prompt_id: Uuid::new_v4().to_string(),
        }
    }
}

impl GrokTurnIds {
    /// Fresh ids for a new turn.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl ArtifactSet {
    /// Append one ACP `session/update` frame (standard or `_x.ai` extension).
    /// The frame `eventId` is session-monotonic, matching the captured grok
    /// session where ids run `<session>-2`, `<session>-4`, … across turns.
    pub fn grok_update(
        &mut self,
        clock: &FakeClock,
        _ids: &GrokTurnIds,
        update: Value,
        extension: bool,
    ) -> std::io::Result<()> {
        self.grok_seq += 1;
        let event_id = format!("{}-{}", self.meta.session_id, self.grok_seq);
        let method = if extension {
            "_x.ai/session/update"
        } else {
            "session/update"
        };
        let frame = json!({
            "timestamp": clock.secs(),
            "method": method,
            "params": {
                "sessionId": self.meta.session_id,
                "update": update,
                "_meta": {
                    "eventId": event_id,
                    "agentTimestampMs": clock.ms()
                }
            }
        });
        self.append(frame)
    }

    /// Append an extension `hook_execution` update, one run per entry.
    pub fn grok_hook_execution(
        &mut self,
        clock: &FakeClock,
        event_name: &str,
        handler_name: &str,
        status: &str,
        elapsed_ms: u64,
    ) -> std::io::Result<()> {
        let update = json!({
            "sessionUpdate": "hook_execution",
            "event_name": event_name,
            "runs": [{
                "name": handler_name,
                "status": { "status": status, "elapsed_ms": elapsed_ms }
            }]
        });
        self.grok_update(
            clock,
            &GrokTurnIds {
                prompt_id: String::new(),
            },
            update,
            true,
        )
    }

    /// Append one `events.jsonl` record.
    pub fn grok_event(&mut self, clock: &FakeClock, mut data: Value) -> std::io::Result<()> {
        if let Some(obj) = data.as_object_mut() {
            obj.entry("ts").or_insert_with(|| json!(clock.rfc3339()));
            obj.entry("session_id")
                .or_insert_with(|| json!(self.meta.session_id));
        }
        self.append_event(data)
    }
}

/// Build a content-chunk update (`user_message_chunk` / `agent_message_chunk` /
/// `agent_thought_chunk`).
#[must_use]
pub fn grok_chunk_update(kind: &str, text: &str, ids: &GrokTurnIds, model: &str) -> Value {
    let mut update = json!({
        "sessionUpdate": kind,
        "content": { "type": "text", "text": text },
        "_meta": { "promptId": ids.prompt_id }
    });
    if kind == "user_message_chunk" {
        update["_meta"] = json!({ "modelId": model, "promptIndex": 0 });
    }
    update
}

/// `tool_call` update.
#[must_use]
pub fn grok_tool_call(tool_call_id: &str, name: &str, input: &Value) -> Value {
    json!({
        "sessionUpdate": "tool_call",
        "toolCallId": tool_call_id,
        "title": name,
        "rawInput": input
    })
}

/// Completed `tool_call_update` with shell-style `rawOutput`.
#[must_use]
pub fn grok_tool_update_completed(
    tool_call_id: &str,
    command: &str,
    output: &str,
    exit_code: i32,
    cwd: &Path,
    failed: bool,
) -> Value {
    let status = if failed { "failed" } else { "completed" };
    let prompt_output = format!("exit: {exit_code}\n");
    json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": tool_call_id,
        "status": status,
        "content": [{ "type": "content", "content": { "type": "text", "text": output } }],
        "locations": [],
        "rawOutput": {
            "type": "Bash",
            "output": [],
            "output_for_prompt": prompt_output,
            "exit_code": if failed { Value::Null } else { json!(exit_code) },
            "command": command,
            "truncated": false,
            "signal": null,
            "timed_out": false,
            "current_dir": cwd.to_string_lossy(),
            "total_bytes": output.len()
        }
    })
}

/// Failed `tool_call_update` (hook deny).
#[must_use]
pub fn grok_tool_update_failed(tool_call_id: &str, reason: &str) -> Value {
    json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": tool_call_id,
        "status": "failed",
        "content": [{
            "type": "content",
            "content": { "type": "text", "text": format!("Hook denied: {reason}") }
        }],
        "rawOutput": null,
        "locations": []
    })
}

/// `turn_completed` extension update.
#[must_use]
pub fn grok_turn_completed(ids: &GrokTurnIds, stop_reason: &str, elapsed_ms: u64) -> Value {
    json!({
        "sessionUpdate": "turn_completed",
        "prompt_id": ids.prompt_id,
        "stop_reason": stop_reason,
        "elapsed_ms": elapsed_ms
    })
}

/// Write the grok `active_sessions.json` registry (one entry).
pub fn grok_write_registry(
    home: &Path,
    meta: &SessionMeta,
    pid: u32,
    clock: &FakeClock,
) -> std::io::Result<()> {
    let body = json!([{
        "session_id": meta.session_id,
        "pid": pid,
        "cwd": meta.cwd.to_string_lossy(),
        "opened_at": clock.rfc3339()
    }]);
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(home.join("active_sessions.json"))?;
    writeln!(file, "{}", body)
}
