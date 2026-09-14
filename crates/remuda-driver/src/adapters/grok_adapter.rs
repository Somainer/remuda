//! Grok `updates.jsonl` / `events.jsonl` → journal observations
//! (D-028 §3, §6, §7, §13 P6).
//!
//! Every rule below traces to
//! [grok-signals-1](../../../docs/design/evidence/grok-signals-1.md):
//!
//! * **Lifecycle** comes from `events.jsonl`: `turn_started` (working) and
//!   `turn_ended` with `outcome` `completed` / `cancelled` (idle either way).
//!   The `trigger` (`ctrl_c` / `send_now`) is retained in the turn id's
//!   related data but never read as a failure. `turn_completed` in
//!   `updates.jsonl` is *not* success by itself — its `stop_reason` can be
//!   `cancelled` (§A1 [V]); it is the chunk-close signal, the event is the
//!   boundary.
//! * **Streaming is chunk-level.** `agent_message_chunk` /
//!   `agent_thought_chunk` append to one node per native prompt id; the
//!   `turn_completed` frame closes that node with the accumulated text and the
//!   completion state. The fixture proves the server can emit multiple SSE
//!   deltas that the client persists as one line, so this promises chunk
//!   granularity, never token granularity.
//! * **Tools** are `tool_call` (name in `title`, input in `rawInput`) followed
//!   by `tool_call_update` carrying `status` and `rawOutput.exit_code`. A
//!   hook-denied call ends `failed` with `error:"denied: …"`/`Hook denied`;
//!   that maps to [`ToolOutcome::Denied`].
//! * **Permissions have no answer channel in files.**
//!   `permission_requested` / `permission_resolved` are post-hoc summaries, and
//!   `wait_ms:0` does not mean no visible prompt (§A5 [V]); they are journaled
//!   as permission lifecycles only. `PermissionRequest` is not a grok hook —
//!   PreToolUse deny/ask is, and is wired through the hook channel.
//! * **Discovery** is `active_sessions.json` by pid, then cwd; registry
//!   membership is discovery only, never liveness (the entry is removed before
//!   process exit, §A2 [V]).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use remuda_protocol::{
    AgentKind, ContentStatus, Id, Knowledge, LifecyclePayload, LifecycleTopic, NativeLifecycle,
    ObservationPayload, Severity, ToolOutcome, UsageScope,
};
use serde_json::Value;

use crate::adapters::{
    AdapterBinding, AdapterHome, AdapterObservation, FileSignalAdapter, message_chunk,
    message_close, message_payload, node_id, thought_chunk, thought_payload, tool_call_payload,
    tool_result_payload, turn_lifecycle,
};
use crate::error::DriverResult;
use crate::grok_session::{
    GrokSessionEvent, GrokSessionUpdate, SessionSelector, SessionTail, locate_session,
    parse_event_line, parse_update_line,
};
use crate::usage::grok::{usage_from_update_frame, usage_from_usage_json};
use crate::usage::{UsageAggregator, to_usage_payload};

/// One prompt's accumulated streamed content.
#[derive(Default)]
struct Stream {
    /// Observed chunks (revision counter).
    chunks: u64,
    /// Accumulated assistant text, for the close snapshot.
    text: String,
    /// Observed thought chunks.
    thought_chunks: u64,
    thought: String,
}

/// File-tail adapter for one grok TUI session.
pub struct GrokAdapter {
    home: AdapterHome,
    /// Hook-confirmed session id.
    confirmed: Option<String>,
    binding: Option<AdapterBinding>,
    updates: Option<SessionTail>,
    events: Option<SessionTail>,
    /// Native node ids (tools, user messages).
    nodes: HashMap<String, Id>,
    /// Streamed nodes per prompt id.
    message_nodes: HashMap<String, Id>,
    thought_nodes: HashMap<String, Id>,
    /// Per-prompt accumulators.
    streams: HashMap<String, Stream>,
    /// Prompts already closed.
    closed: HashSet<String>,
    /// Tool results already emitted.
    results: HashSet<String>,
    /// Frames already processed (native event id), restart-safe dedupe.
    seen_events: HashSet<String>,
    /// Last usage.json contents fed to the aggregator.
    usage_json: Option<String>,
    usage_totals: UsageAggregator,
    session_usage_revision: u64,
}

impl GrokAdapter {
    /// Create an unbound adapter for a shadow `GROK_HOME`.
    #[must_use]
    pub fn new(home: AdapterHome) -> Self {
        Self {
            home,
            confirmed: None,
            binding: None,
            updates: None,
            events: None,
            nodes: HashMap::new(),
            message_nodes: HashMap::new(),
            thought_nodes: HashMap::new(),
            streams: HashMap::new(),
            closed: HashSet::new(),
            results: HashSet::new(),
            seen_events: HashSet::new(),
            usage_json: None,
            usage_totals: UsageAggregator::new(),
            session_usage_revision: 0,
        }
    }

    /// The hook channel confirmed this native session.
    pub fn confirm_session(&mut self, session_id: &str) {
        if session_id.trim().is_empty() {
            return;
        }
        self.confirmed = Some(session_id.trim().to_owned());
    }

    /// Currently bound session.
    #[must_use]
    pub fn binding(&self) -> Option<&AdapterBinding> {
        self.binding.as_ref()
    }

    /// Bind directly to a known session directory.
    ///
    /// Production discovery normally goes through `active_sessions.json`
    /// (pid/cwd), but a `SessionStart` hook or a test harness may hand over
    /// the resolved directory directly. The path must already contain
    /// `updates.jsonl` / `events.jsonl`.
    pub fn bind_session_dir(&mut self, session_id: &str, directory: PathBuf) {
        if !session_id.trim().is_empty() {
            self.confirmed = Some(session_id.trim().to_owned());
            self.bind(session_id.trim().to_owned(), directory);
        }
    }

    fn bind(&mut self, session_id: String, directory: PathBuf) {
        self.updates = Some(SessionTail::new(directory.join("updates.jsonl")));
        self.events = Some(SessionTail::new(directory.join("events.jsonl")));
        self.binding = Some(AdapterBinding {
            session_id,
            directory: Some(directory),
            main_file: None,
        });
    }

    /// pid match first (the registry entry is the TUI process), then cwd.
    fn discover(&mut self) -> DriverResult<bool> {
        let selector = match self.home.pid {
            Some(pid) => SessionSelector::Pid(pid),
            None => SessionSelector::Cwd(&self.home.cwd),
        };
        if let Some(located) = locate_session(&self.home.home, selector)? {
            self.bind(located.session.session_id, located.directory);
            return Ok(true);
        }
        // Pid failed; cwd is the documented fallback, but do not try cwd twice.
        if self.home.pid.is_some()
            && let Some(located) =
                locate_session(&self.home.home, SessionSelector::Cwd(&self.home.cwd))?
        {
            self.bind(located.session.session_id, located.directory);
            return Ok(true);
        }
        Ok(false)
    }

    fn session_id(&self) -> String {
        self.binding
            .as_ref()
            .map(|binding| binding.session_id.clone())
            .or_else(|| self.confirmed.clone())
            .unwrap_or_else(|| "grok-session".to_owned())
    }

    /// Drain `updates.jsonl`.
    fn poll_updates(&mut self) -> DriverResult<Vec<AdapterObservation>> {
        let Some(tail) = self.updates.as_mut() else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for line in tail.poll()? {
            let Ok(record) = parse_update_line(&line) else {
                continue;
            };
            let event_id = record
                .frame
                .pointer("/params/_meta/eventId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if !event_id.is_empty() && !self.seen_events.insert(event_id.clone()) {
                continue;
            }
            let prompt = record
                .frame
                .pointer("/params/update/_meta/promptId")
                .or_else(|| record.frame.pointer("/params/update/prompt_id"))
                .or_else(|| record.frame.pointer("/params/_meta/promptId"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            match record.update {
                GrokSessionUpdate::UserMessageChunk { content } => {
                    let key = format!("user:{}", event_id);
                    if self.nodes.contains_key(&key) {
                        continue;
                    }
                    let id = node_id(&mut self.nodes, &key);
                    let text = content_text(&content);
                    if let Some(text) = text {
                        let mut observed = AdapterObservation::partial(message_payload(
                            id,
                            remuda_protocol::MessageRole::User,
                            text,
                        ));
                        // A user echo is complete once persisted.
                        observed.completeness = remuda_protocol::Completeness::Structured;
                        if let Some(prompt) = &prompt {
                            observed.turn_id = Some(prompt.clone());
                        }
                        out.push(observed);
                    }
                }
                GrokSessionUpdate::AgentMessageChunk { content } => {
                    if let Some(prompt) = &prompt
                        && let Some(text) = content_text(&content)
                    {
                        let stream = self.streams.entry(prompt.clone()).or_default();
                        stream.chunks += 1;
                        stream.text.push_str(&text);
                        let first = stream.chunks == 1;
                        let node = self
                            .message_nodes
                            .entry(prompt.clone())
                            .or_insert_with(|| node_id(&mut self.nodes, &format!("msg:{prompt}")))
                            .clone();
                        out.push(
                            AdapterObservation::partial(message_chunk(
                                node,
                                stream.chunks,
                                first,
                                text,
                            ))
                            .with_turn(prompt),
                        );
                    }
                }
                GrokSessionUpdate::AgentThoughtChunk { content } => {
                    if let Some(prompt) = &prompt
                        && let Some(text) = content_text(&content)
                    {
                        let stream = self.streams.entry(prompt.clone()).or_default();
                        stream.thought_chunks += 1;
                        stream.thought.push_str(&text);
                        let first = stream.thought_chunks == 1;
                        let node = self
                            .thought_nodes
                            .entry(prompt.clone())
                            .or_insert_with(|| {
                                node_id(&mut self.nodes, &format!("thought:{prompt}"))
                            })
                            .clone();
                        let mut observed = AdapterObservation::partial(thought_chunk(
                            node,
                            stream.thought_chunks,
                            first,
                            text,
                        ));
                        observed.turn_id = Some(prompt.clone());
                        out.push(observed);
                    }
                }
                GrokSessionUpdate::ToolCall { tool_call_id } => {
                    let Some(call_id) = tool_call_id else {
                        continue;
                    };
                    let key = format!("call:{call_id}");
                    if self.nodes.contains_key(&key) {
                        continue;
                    }
                    let id = node_id(&mut self.nodes, &key);
                    let update = &record.frame["params"]["update"];
                    let name = string(update, "title").or_else(|| {
                        update
                            .pointer("/_meta/x.ai~1tool/name")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    });
                    let input = update.get("rawInput").cloned();
                    let mut observed =
                        AdapterObservation::structured(tool_call_payload(id, name, input));
                    observed.item_id = Some(call_id);
                    if let Some(prompt) = &prompt {
                        observed.turn_id = Some(prompt.clone());
                    }
                    out.push(observed);
                }
                GrokSessionUpdate::ToolCallUpdate { tool_call_id } => {
                    let Some(call_id) = tool_call_id else {
                        continue;
                    };
                    let update = &record.frame["params"]["update"];
                    // A statusless update is an in-progress mutation; the only
                    // terminal update carries status. Ignore the progress one.
                    let Some(status) = string(update, "status") else {
                        continue;
                    };
                    if !self.results.insert(call_id.clone()) {
                        continue;
                    }
                    let id = node_id(&mut self.nodes, &format!("call:{call_id}"));
                    let raw = update.get("rawOutput");
                    let text = update
                        .pointer("/content/0/content/text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .map(str::to_owned)
                        .or_else(|| {
                            raw.and_then(|value| value.get("output_for_prompt"))
                                .and_then(Value::as_str)
                                .filter(|text| !text.is_empty())
                                .map(str::to_owned)
                        });
                    let exit_code = raw
                        .and_then(|value| value.get("exit_code"))
                        .and_then(Value::as_i64)
                        .and_then(|code| i32::try_from(code).ok());
                    let outcome = tool_outcome(&status, update.get("error"), exit_code);
                    let structured = Some(update.clone());
                    let mut observed = AdapterObservation::structured(tool_result_payload(
                        id, text, structured, exit_code, outcome,
                    ));
                    observed.item_id = Some(call_id);
                    if let Some(prompt) = &prompt {
                        observed.turn_id = Some(prompt.clone());
                    }
                    out.push(observed);
                }
                GrokSessionUpdate::HookExecution | GrokSessionUpdate::Unknown { .. } => {
                    // turn_completed closes the streamed nodes; usage frames are
                    // handled separately below.
                    let kind = match &record.update {
                        GrokSessionUpdate::Unknown { kind } => kind.clone(),
                        _ => record
                            .frame
                            .pointer("/params/update/sessionUpdate")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                    };
                    if kind == "turn_completed"
                        && let Some(prompt) = prompt
                    {
                        out.extend(self.close_prompt(&prompt, &record.frame));
                    }
                }
            }
            if let Some(event) = usage_from_update_frame(&record.frame) {
                self.usage_totals.push(&event);
            }
        }
        Ok(out)
    }

    /// Emit close snapshots for one finished prompt.
    fn close_prompt(&mut self, prompt: &str, frame: &Value) -> Vec<AdapterObservation> {
        if !self.closed.insert(prompt.to_owned()) {
            return Vec::new();
        }
        let stop_reason = frame
            .pointer("/params/update/stop_reason")
            .and_then(Value::as_str)
            .unwrap_or("end_turn");
        let status = if stop_reason == "cancelled" {
            ContentStatus::Interrupted
        } else {
            ContentStatus::Complete
        };
        let stream = self.streams.entry(prompt.to_owned()).or_default();
        let mut out = Vec::new();
        if stream.chunks > 0
            && let Some(node) = self.message_nodes.get(prompt).cloned()
        {
            let text = std::mem::take(&mut stream.text);
            out.push(
                AdapterObservation::structured(message_close(
                    node,
                    stream.chunks + 1,
                    Some(stream.chunks),
                    text,
                    status,
                ))
                .with_turn(prompt),
            );
        }
        if stream.thought_chunks > 0
            && let Some(node) = self.thought_nodes.get(prompt).cloned()
        {
            let text = std::mem::take(&mut stream.thought);
            out.push(AdapterObservation::structured(thought_payload(node, text)).with_turn(prompt));
        }
        out
    }

    /// Drain `events.jsonl`.
    fn poll_events(&mut self) -> DriverResult<Vec<AdapterObservation>> {
        let Some(tail) = self.events.as_mut() else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        let mut ended_turn = false;
        for line in tail.poll()? {
            let Ok(record) = parse_event_line(&line) else {
                continue;
            };
            match record.event {
                GrokSessionEvent::TurnStarted {
                    turn_number,
                    model_id,
                    ..
                } => {
                    let mut observed =
                        AdapterObservation::structured(turn_lifecycle("turn_started", "working"));
                    let mut related = std::collections::BTreeMap::new();
                    if let Some(number) = turn_number {
                        related.insert("turnNumber".into(), number.to_string());
                    }
                    if let Some(model) = model_id {
                        related.insert("model".into(), model);
                    }
                    if !related.is_empty()
                        && let ObservationPayload::Lifecycle(box_payload) = &mut observed.payload
                        && let LifecyclePayload::Native(native) = box_payload.as_mut()
                    {
                        native.related_ids = related;
                    }
                    out.push(observed);
                }
                GrokSessionEvent::Unknown { kind } => match kind.as_str() {
                    "turn_ended" => {
                        let cancelled =
                            record.data.get("outcome").and_then(Value::as_str) == Some("cancelled");
                        let trigger = record
                            .data
                            .pointer("/cancellation_context/trigger")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let mut observed =
                            AdapterObservation::structured(turn_lifecycle("turn_ended", "idle"));
                        if let ObservationPayload::Lifecycle(box_payload) = &mut observed.payload
                            && let LifecyclePayload::Native(native) = box_payload.as_mut()
                        {
                            native.related_ids.insert(
                                "outcome".into(),
                                if cancelled { "cancelled" } else { "completed" }.into(),
                            );
                            if !trigger.is_empty() {
                                native.related_ids.insert("trigger".into(), trigger.into());
                            }
                        }
                        out.push(observed);
                        ended_turn = true;
                    }
                    // Post-hoc permission bookkeeping (§A5): journaled, never
                    // an answer channel. The wait_ms=0 case is not "no prompt".
                    "permission_requested" => {
                        if let Some(payload) =
                            permission_lifecycle("permission_requested", "waiting", &record.data)
                        {
                            out.push(AdapterObservation::structured(payload));
                        }
                    }
                    "permission_resolved" => {
                        if let Some(payload) =
                            permission_lifecycle("permission_resolved", "idle", &record.data)
                        {
                            out.push(AdapterObservation::structured(payload));
                        }
                    }
                    _ => {}
                },
                GrokSessionEvent::PhaseChanged { .. } | GrokSessionEvent::FirstToken => {}
            }
        }
        if ended_turn {
            self.emit_session_usage(&mut out);
        }
        Ok(out)
    }

    /// Read `usage.json` once per new content and emit a session snapshot.
    fn emit_session_usage(&mut self, out: &mut Vec<AdapterObservation>) {
        let Ok(text) = std::fs::read_to_string(
            self.binding
                .as_ref()
                .and_then(|binding| binding.directory.clone())
                .unwrap_or_else(|| self.home.home.clone())
                .join("usage.json"),
        ) else {
            return;
        };
        if self.usage_json.as_deref() == Some(text.as_str()) {
            return;
        }
        self.usage_json = Some(text.clone());
        let Ok(doc) = serde_json::from_str::<Value>(text.trim()) else {
            return;
        };
        for event in usage_from_usage_json(&doc) {
            self.usage_totals.push(&event);
        }
        self.session_usage_revision += 1;
        let payload = to_usage_payload(
            UsageScope::Session,
            self.session_id(),
            self.session_usage_revision,
            self.usage_totals.session(),
        );
        out.push(AdapterObservation::structured(ObservationPayload::Usage(
            Box::new(payload),
        )));
    }
}

impl FileSignalAdapter for GrokAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Grok
    }

    fn session_id(&self) -> Option<&str> {
        self.binding
            .as_ref()
            .map(|binding| binding.session_id.as_str())
    }

    fn confirm_session(&mut self, session_id: &str) {
        GrokAdapter::confirm_session(self, session_id);
    }

    fn poll(&mut self) -> DriverResult<Vec<AdapterObservation>> {
        if self.binding.is_none() && !self.discover()? {
            return Ok(Vec::new());
        }
        let mut out = self.poll_updates()?;
        out.extend(self.poll_events()?);
        Ok(out)
    }
}

/// Text of a content block, tolerating non-text content (kept opaque).
fn content_text(content: &Value) -> Option<String> {
    match content.get("type").and_then(Value::as_str) {
        Some("text") => content
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

/// Map a grok tool status/error to the protocol outcome.
fn tool_outcome(status: &str, error: Option<&Value>, exit_code: Option<i32>) -> ToolOutcome {
    let denied = error
        .map(|value| {
            let text = value.as_str().unwrap_or_default();
            text.contains("denied") || text.contains("Hook denied")
        })
        .unwrap_or(false)
        || status == "denied";
    if denied {
        return ToolOutcome::Denied;
    }
    match status {
        "completed" => {
            if exit_code == Some(0) {
                ToolOutcome::Succeeded
            } else if exit_code.is_some() {
                ToolOutcome::Failed
            } else {
                ToolOutcome::Unknown
            }
        }
        "failed" | "error" => ToolOutcome::Failed,
        "cancelled" => ToolOutcome::Cancelled,
        _ => ToolOutcome::Unknown,
    }
}

/// Build a permission lifecycle observation from an events.jsonl record.
fn permission_lifecycle(
    name: &'static str,
    status: &'static str,
    data: &Value,
) -> Option<ObservationPayload> {
    let mut related = std::collections::BTreeMap::new();
    if let Some(tool) = data.get("tool_name").and_then(Value::as_str) {
        related.insert("toolName".into(), tool.into());
    }
    if let Some(decision) = data.get("decision").and_then(Value::as_str) {
        related.insert("decision".into(), decision.into());
    }
    Some(ObservationPayload::Lifecycle(Box::new(
        LifecyclePayload::Native(Box::new(NativeLifecycle {
            topic: LifecycleTopic::Permission,
            native_name: name.into(),
            native_id: Knowledge::NotApplicable,
            status: Knowledge::Known {
                value: status.into(),
            },
            related_ids: related,
            data_ref: None,
            severity: Severity::Info,
            affects_completion: false,
        })),
    )))
}

#[cfg(test)]
mod tests;
