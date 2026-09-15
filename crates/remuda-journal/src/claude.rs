//! Claude transcript JSONL tailer and mapper; `protocol.md` §5.6.

use crate::Error;
use crate::envelope::Envelope;
use crate::source::{FileTail, MapContext, Source, SourceResume};
use crate::util::{known, parse_timestamp, timestamp_now, unknown};
use remuda_protocol::{
    Completeness, ContentBlock, ContentStatus, EffortEffective, EffortPayload, EffortTracker,
    EventId, FileCursor, Id, Knowledge, LifecyclePayload, LifecycleTopic, MessageOrigin,
    MessagePayload, MessagePhase, MessageRole, MutationOperation, NativeLifecycle,
    NativeRequestKey, NodeMutation, ObservationPayload, ObservationSource, OpaqueImpact,
    OpaquePayload, OpaqueReason, ResultStage, Severity, SourceChannel, SourceCursor, TextBlock,
    ThoughtPayload, ThoughtRepresentation, ToolCallPayload, ToolCallState, ToolCategory,
    ToolOutcome, ToolResultPayload, U64,
};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;

/// Maps native message / tool ids to Remuda `obj_` identities across a tail.
///
/// Tool / message / thought ids are a pure function of `(scope, native id)` via
/// [`Id::derive`], with `scope` = owning instance id. That is what lets the hook
/// relay and this transcript tailer converge on one node for one tool call
/// (live-view design §2.3): the hook observes `tool_use_id` at tool start and
/// the transcript records the same string seconds later, and both draw the
/// same card. Workflow/member ids stay random: no live channel shares their id
/// space.
#[derive(Debug, Clone)]
pub struct NativeIds {
    scope: String,
    messages: HashMap<String, (Id, u64)>,
    tools: HashMap<String, Id>,
    thoughts: HashMap<String, Id>,
    workflows: HashMap<String, Id>,
    members: HashMap<String, Id>,
    /// §9.1 effective-effort edges across this tail.
    effort: EffortTracker,
}

impl NativeIds {
    /// Empty identity map scoped to one instance.
    pub fn new(scope: impl Into<String>) -> Self {
        Self {
            scope: scope.into(),
            messages: HashMap::new(),
            tools: HashMap::new(),
            thoughts: HashMap::new(),
            workflows: HashMap::new(),
            members: HashMap::new(),
            effort: EffortTracker::new(),
        }
    }

    /// Deterministic event id for an §9.1 effort edge read from one assistant
    /// record. Same `(instance, record, level)` from the live channel and this
    /// tailer draws the same id (see [`remuda_protocol::effort_event_id`]).
    pub(crate) fn effort_event(
        &self,
        assistant_native_id: &str,
        name: remuda_protocol::EffortName,
    ) -> EventId {
        remuda_protocol::effort_event_id(&self.scope, assistant_native_id, name)
    }

    /// Deterministic id for a native object in this instance's scope.
    fn derived(&self, native: &str) -> Result<Id, Error> {
        Ok(Id::derive("obj", &self.scope, native)?)
    }

    pub(crate) fn message(&mut self, native: &str) -> Result<(Id, U64, MutationOperation), Error> {
        if let Some((id, rev)) = self.messages.get_mut(native) {
            *rev += 1;
            return Ok((id.clone(), U64(*rev), MutationOperation::Append));
        }
        let id = self.derived(native)?;
        self.messages.insert(native.to_owned(), (id.clone(), 1));
        Ok((id, U64(1), MutationOperation::Open))
    }

    pub(crate) fn tool(&mut self, native: &str) -> Result<Id, Error> {
        if let Some(id) = self.tools.get(native) {
            return Ok(id.clone());
        }
        let id = self.derived(native)?;
        self.tools.insert(native.to_owned(), id.clone());
        Ok(id)
    }

    pub(crate) fn thought(&mut self, native: &str) -> Result<Id, Error> {
        if let Some(id) = self.thoughts.get(native) {
            return Ok(id.clone());
        }
        let id = self.derived(native)?;
        self.thoughts.insert(native.to_owned(), id.clone());
        Ok(id)
    }

    pub(crate) fn workflow(&mut self, native: &str) -> Result<Id, Error> {
        if let Some(id) = self.workflows.get(native) {
            return Ok(id.clone());
        }
        let id = Id::new("obj")?;
        self.workflows.insert(native.to_owned(), id.clone());
        Ok(id)
    }

    pub(crate) fn member(&mut self, native: &str) -> Result<Id, Error> {
        if let Some(id) = self.members.get(native) {
            return Ok(id.clone());
        }
        let id = Id::new("obj")?;
        self.members.insert(native.to_owned(), id.clone());
        Ok(id)
    }
}

/// Tails `~/.claude/projects/<enc>/<sid>.jsonl` by byte offset.
#[derive(Debug, Clone)]
pub struct ClaudeJsonlTailer {
    tail: FileTail,
    ctx: MapContext,
    ids: NativeIds,
}

impl ClaudeJsonlTailer {
    /// Tail `path` from offset 0.
    pub fn new(path: impl Into<PathBuf>, ctx: MapContext) -> Result<Self, Error> {
        let ids = NativeIds::new(ctx.instance_id.as_id().as_str());
        Ok(Self {
            tail: FileTail::new(path)?,
            ctx,
            ids,
        })
    }

    /// Resume a previous tail.
    pub fn resume(path: impl Into<PathBuf>, ctx: MapContext, resume: SourceResume) -> Self {
        let ids = NativeIds::new(ctx.instance_id.as_id().as_str());
        Self {
            tail: FileTail::from_resume(path, resume),
            ctx,
            ids,
        }
    }

    /// Mapping context.
    pub fn context(&self) -> &MapContext {
        &self.ctx
    }

    /// File tail state.
    pub fn tail(&self) -> &FileTail {
        &self.tail
    }

    /// Persistable cursor.
    pub fn source_resume(&self) -> SourceResume {
        self.tail.resume()
    }

    /// Read newly completed lines and map them to envelopes (raw bytes attached).
    pub fn poll(&mut self) -> Result<Vec<Envelope>, Error> {
        let mut out = Vec::new();
        for (line, cursor) in self.tail.poll()? {
            out.extend(Source::map_line(self, &line, cursor)?);
        }
        Ok(out)
    }

    /// Append polled envelopes into `journal` and persist the file cursor.
    pub async fn ingest(&mut self, journal: &crate::Journal) -> Result<Vec<U64>, Error> {
        let instance = self.ctx.instance_id.clone();
        let envelopes = self.poll()?;
        let mut seqs = Vec::with_capacity(envelopes.len());
        for envelope in envelopes {
            seqs.push(journal.append(&instance, envelope).await?);
        }
        journal
            .put_source_resume(&instance, self.source_resume())
            .await?;
        Ok(seqs)
    }
}

impl Source for ClaudeJsonlTailer {
    fn name(&self) -> &'static str {
        "claude-jsonl"
    }

    fn map_line(&mut self, line: &[u8], cursor: FileCursor) -> Result<Vec<Envelope>, Error> {
        map_claude_line(&self.ctx, &mut self.ids, line, &cursor)
    }
}

/// Map one Claude JSONL line (transcript or stream-json).
pub fn map_claude_line(
    ctx: &MapContext,
    ids: &mut NativeIds,
    line: &[u8],
    cursor: &FileCursor,
) -> Result<Vec<Envelope>, Error> {
    let parsed = match serde_json::from_slice::<Value>(line) {
        Ok(value) => value,
        Err(_) => {
            return Ok(vec![opaque(
                ctx,
                cursor,
                line,
                "malformed-jsonl",
                OpaqueReason::Malformed,
                Completeness::Opaque,
                None,
            )?]);
        }
    };
    map_claude_value(ctx, ids, &parsed, line, cursor)
}

pub(crate) fn map_claude_value(
    ctx: &MapContext,
    ids: &mut NativeIds,
    value: &Value,
    line: &[u8],
    cursor: &FileCursor,
) -> Result<Vec<Envelope>, Error> {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
    if kind == "keep_alive" {
        return Ok(Vec::new());
    }
    match kind {
        "user" => map_user(ctx, ids, value, line, cursor),
        "assistant" => map_assistant(ctx, ids, value, line, cursor),
        "system" => map_system(ctx, value, line, cursor),
        "result" => Ok(vec![lifecycle(
            ctx,
            cursor,
            line,
            value,
            LifecycleTopic::Turn,
            "result",
            Completeness::Structured,
            true,
        )?]),
        "control_request" | "control_response" | "control_cancel_request" => Ok(vec![lifecycle(
            ctx,
            cursor,
            line,
            value,
            LifecycleTopic::Diagnostic,
            kind,
            Completeness::Partial,
            false,
        )?]),
        "rate_limit_event" => Ok(vec![lifecycle(
            ctx,
            cursor,
            line,
            value,
            LifecycleTopic::Diagnostic,
            "rate_limit_event",
            Completeness::Partial,
            false,
        )?]),
        "ai-title" => Ok(vec![lifecycle(
            ctx,
            cursor,
            line,
            value,
            LifecycleTopic::Session,
            "title",
            Completeness::Partial,
            false,
        )?]),
        "queue-operation" | "last-prompt" => Ok(vec![lifecycle(
            ctx,
            cursor,
            line,
            value,
            LifecycleTopic::Diagnostic,
            kind,
            Completeness::Partial,
            false,
        )?]),
        "atis-latch" => Ok(vec![opaque(
            ctx,
            cursor,
            line,
            kind,
            OpaqueReason::UnknownType,
            Completeness::Opaque,
            native_uuid(value),
        )?]),
        "attachment" => map_attachment(ctx, ids, value, line, cursor),
        "stream_event" => Ok(vec![opaque(
            ctx,
            cursor,
            line,
            "stream_event",
            OpaqueReason::UnmappedFields,
            Completeness::Partial,
            native_uuid(value),
        )?]),
        "launched" | "started" => Ok(vec![opaque(
            ctx,
            cursor,
            line,
            kind,
            OpaqueReason::UnmappedFields,
            Completeness::Partial,
            native_uuid(value),
        )?]),
        "" => Ok(vec![opaque(
            ctx,
            cursor,
            line,
            "missing-type",
            OpaqueReason::Malformed,
            Completeness::Opaque,
            native_uuid(value),
        )?]),
        other => Ok(vec![opaque(
            ctx,
            cursor,
            line,
            other,
            OpaqueReason::UnknownType,
            Completeness::Opaque,
            native_uuid(value),
        )?]),
    }
}

fn map_user(
    ctx: &MapContext,
    ids: &mut NativeIds,
    value: &Value,
    line: &[u8],
    cursor: &FileCursor,
) -> Result<Vec<Envelope>, Error> {
    let message = value.get("message").cloned().unwrap_or(Value::Null);
    let content = message.get("content").cloned().unwrap_or(Value::Null);
    let uuid = native_uuid(value);
    let mut out = Vec::new();
    let text = user_record_text(&content);
    // §9.1: the `/effort` slash record arms attribution; its stdout verdict
    // is what settles the level (the slash record lands even on reject).
    if let Some(word) = remuda_protocol::slash_effort_word(&text) {
        ids.effort.note_slash(&word, false);
    } else if text.contains("<local-command-stdout>") {
        let stdout = extract_local_stdout(&text);
        if let Some((observed, source)) = ids.effort.note_stdout(&stdout, false) {
            out.push(effort_edge_envelope(
                ctx,
                ids,
                value,
                line,
                cursor,
                "effort-stdout",
                observed,
                source,
                Some(&stdout),
            )?);
        }
    }
    match content {
        Value::String(text) => {
            out.push(user_message(
                ctx,
                ids,
                value,
                line,
                cursor,
                &text,
                uuid.as_deref(),
            )?);
        }
        Value::Array(blocks) => {
            let mut texts = Vec::new();
            for block in &blocks {
                let btype = block.get("type").and_then(Value::as_str).unwrap_or("");
                if btype == "tool_result" {
                    out.push(tool_result(ctx, ids, value, line, cursor, block)?);
                } else if btype == "text" {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        texts.push(t.to_owned());
                    }
                } else if btype == "tool_use" {
                    out.push(tool_call(ctx, ids, value, line, cursor, block, 0)?);
                } else {
                    out.push(opaque(
                        ctx,
                        cursor,
                        line,
                        btype,
                        OpaqueReason::UnmappedFields,
                        Completeness::Partial,
                        uuid.clone(),
                    )?);
                }
            }
            if !texts.is_empty() {
                out.push(user_message(
                    ctx,
                    ids,
                    value,
                    line,
                    cursor,
                    &texts.join(""),
                    uuid.as_deref(),
                )?);
            }
        }
        _ => {
            out.push(opaque(
                ctx,
                cursor,
                line,
                "user",
                OpaqueReason::Malformed,
                Completeness::Opaque,
                uuid,
            )?);
        }
    }
    Ok(out)
}

/// §9.1 effective-effort envelope for an effort edge.
///
/// `native_key` is the assistant message id for assistant-record edges (the
/// live channel and this tailer derive the same id) or a deterministic key
/// derived from the `/effort` stdout verdict record, which settles a switch
/// before any next-turn assistant record exists.
#[allow(clippy::too_many_arguments)]
fn effort_envelope(
    ctx: &MapContext,
    ids: &NativeIds,
    value: &Value,
    line: &[u8],
    cursor: &FileCursor,
    native_key: &str,
    observed: remuda_protocol::ObservedEffort,
    source: remuda_protocol::EffortSource,
    raw: Option<&str>,
) -> Result<Envelope, Error> {
    let event_id = ids.effort_event(native_key, observed.name);
    let mut env = envelope(
        ctx,
        cursor,
        line,
        value,
        Completeness::Structured,
        native_uuid(value).as_deref(),
        ObservationPayload::Effort(Box::new(EffortPayload {
            requested: None,
            effective: EffortEffective {
                name: observed.name,
                ultracode: observed.ultracode,
                source,
                observed_at: timestamp_now()?,
            },
            raw: raw.map(str::to_owned),
        })),
    )?;
    env.event_id = Some(event_id);
    Ok(env)
}

/// Envelope for an edge settled by a `/effort` stdout verdict / ultra
/// attachment — records that carry no assistant message id.
#[allow(clippy::too_many_arguments)]
fn effort_edge_envelope(
    ctx: &MapContext,
    ids: &NativeIds,
    value: &Value,
    line: &[u8],
    cursor: &FileCursor,
    native_suffix: &str,
    observed: remuda_protocol::ObservedEffort,
    source: remuda_protocol::EffortSource,
    raw: Option<&str>,
) -> Result<Envelope, Error> {
    let native = native_uuid(value)
        .map(|uuid| format!("{native_suffix}:{uuid}"))
        .unwrap_or_else(|| format!("{native_suffix}:{}", cursor.offset.0));
    effort_envelope(
        ctx, ids, value, line, cursor, &native, observed, source, raw,
    )
}

/// Plain-text view of a user record's content (string or text-block array).
fn user_record_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Text inside `<local-command-stdout>…</local-command-stdout>`.
fn extract_local_stdout(content: &str) -> String {
    let Some(start) = content.find("<local-command-stdout>") else {
        return content.trim().to_owned();
    };
    let body = &content[start + "<local-command-stdout>".len()..];
    let end = body.find("</local-command-stdout>").unwrap_or(body.len());
    body[..end].trim().to_owned()
}

fn map_assistant(
    ctx: &MapContext,
    ids: &mut NativeIds,
    value: &Value,
    line: &[u8],
    cursor: &FileCursor,
) -> Result<Vec<Envelope>, Error> {
    let message = value.get("message").cloned().unwrap_or(Value::Null);
    let content = message
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let native_msg = message
        .get("id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| native_uuid(value));
    let mut out = Vec::new();
    // §9.1: every assistant record carries the effective level; emit on edges
    // before the content blocks so the journal orders the change first.
    let raw_effort = value.get("effort").and_then(Value::as_str);
    let raw_per_turn = value.get("perTurnEffort").and_then(Value::as_str);
    if let Some((observed, source)) = ids.effort.observe(raw_effort, raw_per_turn) {
        let effort_native = native_msg
            .clone()
            .unwrap_or_else(|| format!("assistant-{}", cursor.offset.0));
        out.push(effort_envelope(
            ctx,
            ids,
            value,
            line,
            cursor,
            &effort_native,
            observed,
            source,
            raw_effort.or(raw_per_turn),
        )?);
    }
    let has_tool = content
        .iter()
        .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"));
    let has_text = content
        .iter()
        .any(|b| b.get("type").and_then(Value::as_str) == Some("text"));
    for (index, block) in content.iter().enumerate() {
        let btype = block.get("type").and_then(Value::as_str).unwrap_or("");
        match btype {
            "thinking" | "redacted_thinking" => {
                out.push(thought(
                    ctx,
                    ids,
                    value,
                    line,
                    cursor,
                    block,
                    native_msg.as_deref(),
                    index as u32,
                    btype == "redacted_thinking",
                )?);
            }
            "text" => {
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                let phase = if has_tool {
                    MessagePhase::Commentary
                } else {
                    MessagePhase::Final
                };
                out.push(assistant_message(
                    ctx,
                    ids,
                    value,
                    line,
                    cursor,
                    text,
                    native_msg.as_deref(),
                    phase,
                )?);
            }
            "tool_use" => {
                out.push(tool_call(
                    ctx,
                    ids,
                    value,
                    line,
                    cursor,
                    block,
                    index as u32,
                )?);
            }
            other => {
                out.push(opaque(
                    ctx,
                    cursor,
                    line,
                    other,
                    OpaqueReason::UnmappedFields,
                    Completeness::Partial,
                    native_msg.clone(),
                )?);
            }
        }
    }
    if out.is_empty() && !has_text {
        out.push(opaque(
            ctx,
            cursor,
            line,
            "assistant",
            OpaqueReason::UnmappedFields,
            Completeness::Partial,
            native_msg,
        )?);
    }
    Ok(out)
}

fn map_system(
    ctx: &MapContext,
    value: &Value,
    line: &[u8],
    cursor: &FileCursor,
) -> Result<Vec<Envelope>, Error> {
    let subtype = value.get("subtype").and_then(Value::as_str).unwrap_or("");
    let (topic, name, completeness, affects) = match subtype {
        "init" => (
            LifecycleTopic::Session,
            "init",
            Completeness::Structured,
            false,
        ),
        "hook_started" => (
            LifecycleTopic::Hook,
            "hook_started",
            Completeness::Structured,
            false,
        ),
        "hook_response" | "hook_progress" => (
            LifecycleTopic::Hook,
            subtype,
            Completeness::Structured,
            false,
        ),
        "permission_denied" => (
            LifecycleTopic::Permission,
            "permission_denied",
            Completeness::Structured,
            false,
        ),
        "status" | "session_state_changed" => (
            LifecycleTopic::Session,
            subtype,
            Completeness::Partial,
            false,
        ),
        "api_retry" | "informational" | "local_command_output" | "thinking_tokens" => (
            LifecycleTopic::Diagnostic,
            subtype,
            Completeness::Partial,
            false,
        ),
        "task_started" | "task_progress" | "task_updated" | "task_notification" => {
            (LifecycleTopic::Task, subtype, Completeness::Partial, false)
        }
        s if s.contains("compact") => (
            LifecycleTopic::Session,
            "compact",
            Completeness::Structured,
            false,
        ),
        "" => {
            return Ok(vec![opaque(
                ctx,
                cursor,
                line,
                "system",
                OpaqueReason::UnknownType,
                Completeness::Opaque,
                native_uuid(value),
            )?]);
        }
        other => {
            return Ok(vec![opaque(
                ctx,
                cursor,
                line,
                other,
                OpaqueReason::UnknownType,
                Completeness::Opaque,
                native_uuid(value),
            )?]);
        }
    };
    Ok(vec![lifecycle(
        ctx,
        cursor,
        line,
        value,
        topic,
        name,
        completeness,
        affects,
    )?])
}

fn map_attachment(
    ctx: &MapContext,
    ids: &mut NativeIds,
    value: &Value,
    line: &[u8],
    cursor: &FileCursor,
) -> Result<Vec<Envelope>, Error> {
    let atype = value
        .pointer("/attachment/type")
        .and_then(Value::as_str)
        .unwrap_or("attachment");
    // §9.1: 2.1.272 rides the ultracode enter/exit attachment on the next
    // prompt; emit the effort flag edge it implies.
    if atype == "ultra_effort_enter" || atype == "ultra_effort_exit" {
        if let Some((observed, source)) = ids
            .effort
            .note_ultra_attachment(atype == "ultra_effort_enter")
        {
            return Ok(vec![effort_edge_envelope(
                ctx, ids, value, line, cursor, atype, observed, source, None,
            )?]);
        }
        return Ok(Vec::new());
    }
    let (topic, name, completeness, reason) = match atype {
        "hook_success" | "hook_failure" => (
            Some(LifecycleTopic::Hook),
            atype,
            Completeness::Partial,
            None,
        ),
        "model" | "mode" | "permission-mode" | "agent_listing_delta" | "skill_listing" => (
            Some(LifecycleTopic::Configuration),
            atype,
            Completeness::Partial,
            None,
        ),
        "edited_text_file" => (
            None,
            atype,
            Completeness::Partial,
            Some(OpaqueReason::UnmappedFields),
        ),
        "environment" | "prompt_snapshot" | "instructions" | "nested_memory"
        | "session_context" | "date" => (
            None,
            atype,
            Completeness::Opaque,
            Some(OpaqueReason::UnmappedFields),
        ),
        "file-history-snapshot" | "file-history-delta" => (
            None,
            atype,
            Completeness::Opaque,
            Some(OpaqueReason::UnmappedFields),
        ),
        other => (
            None,
            other,
            Completeness::Opaque,
            Some(OpaqueReason::UnknownType),
        ),
    };
    if let Some(topic) = topic {
        Ok(vec![lifecycle(
            ctx,
            cursor,
            line,
            value,
            topic,
            name,
            completeness,
            false,
        )?])
    } else {
        Ok(vec![opaque(
            ctx,
            cursor,
            line,
            name,
            reason.unwrap_or(OpaqueReason::UnknownType),
            completeness,
            native_uuid(value),
        )?])
    }
}

/// Who actually wrote a `user` record (protocol §5.2 `origin`).
///
/// Claude files skill bodies, slash-command expansions, local command output,
/// hook context and compaction summaries under `role: "user"`, so the role
/// alone cannot separate the human's words from text injected on their behalf.
/// This mirrors the driver-side classifier in
/// `remuda-driver::claude_transcript_records`; the two read the same file
/// format and must agree.
///
/// Anything unrecognised stays `Human`: showing one row too many is
/// recoverable, silently hiding what someone said is not.
fn user_origin(value: &Value) -> MessageOrigin {
    if value.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
        return MessageOrigin::Compaction;
    }
    if value
        .get("sourceToolUseID")
        .and_then(Value::as_str)
        .is_some()
    {
        return MessageOrigin::ToolResult;
    }
    // Claude's own statement, where it makes one, outranks the text shape.
    if let Some(kind) = value.pointer("/origin/kind").and_then(Value::as_str) {
        match kind {
            "human" | "coordinator" | "peer" => return MessageOrigin::Human,
            "task-notification" => return MessageOrigin::HookContext,
            _ => {}
        }
    }
    match value.get("promptSource").and_then(Value::as_str) {
        Some("typed" | "sdk" | "queued") => return MessageOrigin::Human,
        Some("system") => return MessageOrigin::HookContext,
        _ => {}
    }
    if value.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return MessageOrigin::InjectedSkill;
    }
    let text = match value.pointer("/message/content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    let head = text.trim_start();
    if head.starts_with("<command-message>")
        || head.starts_with("<command-name>")
        || head.starts_with("<command-args>")
    {
        MessageOrigin::InjectedSkill
    } else if head.starts_with("<local-command-stdout>")
        || head.starts_with("<local-command-stderr>")
    {
        MessageOrigin::InjectedCommandOutput
    } else if head.starts_with("<system-reminder>") || head.starts_with("<task-notification>") {
        MessageOrigin::HookContext
    } else {
        MessageOrigin::Human
    }
}

fn user_message(
    ctx: &MapContext,
    ids: &mut NativeIds,
    value: &Value,
    line: &[u8],
    cursor: &FileCursor,
    text: &str,
    native: Option<&str>,
) -> Result<Envelope, Error> {
    let key = native
        .map(ToOwned::to_owned)
        .or_else(|| native_uuid(value))
        .unwrap_or_else(|| format!("user-{}", cursor.offset.0));
    let (node_id, revision, operation) = ids.message(&key)?;
    envelope(
        ctx,
        cursor,
        line,
        value,
        Completeness::Structured,
        native,
        ObservationPayload::Message(Box::new(MessagePayload {
            mutation: NodeMutation {
                node_id: node_id.clone(),
                revision,
                operation,
                base_revision: None,
            },
            message_id: node_id,
            role: MessageRole::User,
            phase: MessagePhase::Input,
            blocks: vec![ContentBlock::Text(Box::new(TextBlock {
                text: text.to_owned(),
            }))],
            target_block: None,
            parent_tool_call_id: None,
            native_origin: known("claude-jsonl".into()),
            origin: Some(user_origin(value)),
            command_id: None,
            status: ContentStatus::Complete,
        })),
    )
}

#[allow(clippy::too_many_arguments)]
fn assistant_message(
    ctx: &MapContext,
    ids: &mut NativeIds,
    value: &Value,
    line: &[u8],
    cursor: &FileCursor,
    text: &str,
    native: Option<&str>,
    phase: MessagePhase,
) -> Result<Envelope, Error> {
    let key = native
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("assistant-{}", cursor.offset.0));
    let (node_id, revision, operation) = ids.message(&key)?;
    envelope(
        ctx,
        cursor,
        line,
        value,
        Completeness::Structured,
        native,
        ObservationPayload::Message(Box::new(MessagePayload {
            mutation: NodeMutation {
                node_id: node_id.clone(),
                revision,
                operation,
                base_revision: None,
            },
            message_id: node_id,
            role: MessageRole::Assistant,
            phase,
            blocks: vec![ContentBlock::Text(Box::new(TextBlock {
                text: text.to_owned(),
            }))],
            target_block: None,
            parent_tool_call_id: parent_tool(ids, value),
            native_origin: known("claude-jsonl".into()),
            // Assistant output is never an injected user record.
            origin: Some(MessageOrigin::Human),
            command_id: None,
            status: ContentStatus::Complete,
        })),
    )
}

#[allow(clippy::too_many_arguments)]
fn thought(
    ctx: &MapContext,
    ids: &mut NativeIds,
    value: &Value,
    line: &[u8],
    cursor: &FileCursor,
    block: &Value,
    native_msg: Option<&str>,
    part_index: u32,
    redacted: bool,
) -> Result<Envelope, Error> {
    let key = format!("{}:thought:{part_index}", native_msg.unwrap_or("thought"));
    let thought_id = ids.thought(&key)?;
    let thinking = block.get("thinking").and_then(Value::as_str);
    let empty = thinking.map(str::is_empty).unwrap_or(true);
    let representation = if redacted || empty {
        ThoughtRepresentation::Redacted
    } else {
        ThoughtRepresentation::Text
    };
    envelope(
        ctx,
        cursor,
        line,
        value,
        Completeness::Structured,
        native_msg,
        ObservationPayload::Thought(Box::new(ThoughtPayload {
            mutation: NodeMutation {
                node_id: thought_id.clone(),
                revision: U64(1),
                operation: MutationOperation::Open,
                base_revision: None,
            },
            thought_id,
            representation,
            text: if empty {
                None
            } else {
                thinking.map(ToOwned::to_owned)
            },
            part_index,
            status: ContentStatus::Complete,
        })),
    )
}

fn tool_call(
    ctx: &MapContext,
    ids: &mut NativeIds,
    value: &Value,
    line: &[u8],
    cursor: &FileCursor,
    block: &Value,
    _index: u32,
) -> Result<Envelope, Error> {
    let native_id = block.get("id").and_then(Value::as_str).unwrap_or("tool");
    let tool_call_id = ids.tool(native_id)?;
    let name = block
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let input = block
        .get("input")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    envelope(
        ctx,
        cursor,
        line,
        value,
        Completeness::Structured,
        Some(native_id),
        ObservationPayload::ToolCall(Box::new(ToolCallPayload {
            mutation: NodeMutation {
                node_id: tool_call_id.clone(),
                revision: U64(1),
                operation: MutationOperation::Open,
                base_revision: None,
            },
            tool_call_id,
            parent_tool_call_id: parent_tool(ids, value),
            tool_name: known(name.to_owned()),
            display_title: known(name.to_owned()),
            category: tool_category(name),
            input: known(input),
            input_text_delta: None,
            state: ToolCallState::Proposed,
            executor: unknown("not-emitted"),
        })),
    )
}

fn tool_result(
    ctx: &MapContext,
    ids: &mut NativeIds,
    value: &Value,
    line: &[u8],
    cursor: &FileCursor,
    block: &Value,
) -> Result<Envelope, Error> {
    let native_id = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let tool_call_id = ids.tool(native_id)?;
    let is_error = block
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = tool_result_text(block);
    envelope(
        ctx,
        cursor,
        line,
        value,
        Completeness::Structured,
        Some(native_id),
        ObservationPayload::ToolResult(Box::new(ToolResultPayload {
            mutation: NodeMutation {
                node_id: tool_call_id.clone(),
                revision: U64(1),
                operation: MutationOperation::Close,
                base_revision: None,
            },
            tool_call_id,
            stage: ResultStage::Final,
            outcome: if is_error {
                ToolOutcome::Failed
            } else {
                ToolOutcome::Succeeded
            },
            blocks: vec![ContentBlock::Text(Box::new(TextBlock { text }))],
            structured_result: unknown("not-emitted"),
            exit_code: unknown("not-emitted"),
            changes: Vec::new(),
        })),
    )
}

fn tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn tool_category(name: &str) -> ToolCategory {
    match name {
        "Bash" | "bash" => ToolCategory::Shell,
        "Read" | "NotebookRead" => ToolCategory::FileRead,
        "Write" | "Edit" | "NotebookEdit" => ToolCategory::FileWrite,
        "Grep" | "Glob" | "WebSearch" | "WebFetch" => ToolCategory::Search,
        "Workflow" => ToolCategory::Workflow,
        "Task" | "Agent" => ToolCategory::Agent,
        n if n.starts_with("mcp__") => ToolCategory::Mcp,
        _ => ToolCategory::Other,
    }
}

fn parent_tool(ids: &mut NativeIds, value: &Value) -> Option<Id> {
    let native = value
        .get("parent_tool_use_id")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("message")
                .and_then(|m| m.get("parent_tool_use_id"))
                .and_then(Value::as_str)
        })?;
    ids.tool(native).ok()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn lifecycle(
    ctx: &MapContext,
    cursor: &FileCursor,
    line: &[u8],
    value: &Value,
    topic: LifecycleTopic,
    native_name: &str,
    completeness: Completeness,
    affects_completion: bool,
) -> Result<Envelope, Error> {
    let native_id = native_uuid(value).or_else(|| {
        value
            .get("session_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    });
    let native_item = native_id.clone();
    let status = value
        .pointer("/rate_limit_info/status")
        .and_then(Value::as_str)
        .or_else(|| value.get("subtype").and_then(Value::as_str))
        .or_else(|| value.get("session_state").and_then(Value::as_str));
    envelope(
        ctx,
        cursor,
        line,
        value,
        completeness,
        native_item.as_deref(),
        ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
            NativeLifecycle {
                topic,
                native_name: native_name.to_owned(),
                native_id: match native_id {
                    Some(id) => known(id),
                    None => unknown("not-emitted"),
                },
                status: match status {
                    Some(s) => known(s.to_owned()),
                    None => unknown("not-emitted"),
                },
                related_ids: related_ids(value),
                data_ref: None,
                severity: Severity::Info,
                affects_completion,
            },
        )))),
    )
}

pub(crate) fn opaque(
    ctx: &MapContext,
    cursor: &FileCursor,
    line: &[u8],
    native_type: &str,
    reason: OpaqueReason,
    completeness: Completeness,
    native_event: Option<String>,
) -> Result<Envelope, Error> {
    let dummy = crate::util::placeholder_digest();
    envelope(
        ctx,
        cursor,
        line,
        &Value::Null,
        completeness,
        native_event.as_deref(),
        ObservationPayload::Opaque(Box::new(OpaquePayload {
            native_type: native_type.to_owned(),
            reason,
            raw_ref: remuda_protocol::RawRef {
                object_id: Id::new("obj")?,
                offset: U64(0),
                length: U64(line.len() as u64),
                digest: dummy,
                media_type: "application/jsonl; charset=utf-8".into(),
                redaction: remuda_protocol::Redaction::None,
            },
            affects: vec![OpaqueImpact::Presentation],
            summary: Some(native_type.to_owned()),
        })),
    )
}

pub(crate) fn envelope(
    ctx: &MapContext,
    cursor: &FileCursor,
    line: &[u8],
    value: &Value,
    completeness: Completeness,
    native_item: Option<&str>,
    body: ObservationPayload,
) -> Result<Envelope, Error> {
    let native_event = native_uuid(value);
    let native_at = value
        .get("timestamp")
        .and_then(Value::as_str)
        .map(parse_timestamp)
        .unwrap_or_else(|| unknown("not-emitted"));
    let env = Envelope {
        journal_id: ctx.journal_id.clone(),
        instance_id: ctx.instance_id.clone(),
        run_id: ctx.run_id.clone(),
        host_id: ctx.host_id.clone(),
        process_generation: ctx.process_generation,
        run_generation: ctx.run_generation,
        observed_at: timestamp_now()?,
        native_at,
        source: ObservationSource {
            driver_kind: ctx.driver_kind,
            driver_version: ctx.driver_version.clone(),
            adapter_version: ctx.adapter_version.clone(),
            channel: ctx.channel,
            delivery: ctx.delivery,
            native_session_id: known(ctx.native_session_id.clone()),
            native_turn_id: unknown("not-emitted"),
            native_agent_id: if ctx.channel == SourceChannel::WorkflowJournal {
                unknown("not-emitted")
            } else {
                Knowledge::NotApplicable
            },
            native_item_id: match native_item {
                Some(item) => known(item.to_owned()),
                None => unknown("not-emitted"),
            },
            native_event_id: match native_event {
                Some(id) => known(id),
                None => unknown("not-emitted"),
            },
            native_request_id: NativeRequestKey::None,
            source_cursor: SourceCursor::File(Box::new(cursor.clone())),
        },
        completeness,
        evidence_event_ids: Vec::new(),
        event_id: None,
        body,
        raw: None,
    };
    Ok(env.with_jsonl_raw(line.to_vec()))
}

fn native_uuid(value: &Value) -> Option<String> {
    value
        .get("uuid")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn related_ids(value: &Value) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    if let Some(sid) = value.get("session_id").and_then(Value::as_str) {
        map.insert("session_id".into(), sid.into());
    }
    if let Some(hid) = value.get("hook_id").and_then(Value::as_str) {
        map.insert("hook_id".into(), hid.into());
    }
    if let Some(tid) = value.get("task_id").and_then(Value::as_str) {
        map.insert("task_id".into(), tid.into());
    }
    map
}
