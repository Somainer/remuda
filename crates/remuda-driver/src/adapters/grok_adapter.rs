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
//! * **Tools** follow the `protocol.md` §5.7 grok-acp row (D-043): the stable
//!   tool identity is `_meta["x.ai/tool"].name` — never the human `title`,
//!   which only feeds `display_title`. The category resolves through the
//!   design-doc §3.1 name table and falls back to the frame's `kind`. A
//!   statusless `tool_call_update` is the **Running** mutation (same node,
//!   `Replace`, revision 2); fields the frame omits keep their previous value.
//!   The terminal update closes the node with a `ToolResult` whose revision is
//!   strictly greater. `content[]` maps to text blocks and `FileChange`s;
//!   non-text content is surfaced, never silently dropped. A hook-denied call
//!   ends `failed` with `error:"denied: …"`/`Hook denied`; that maps to
//!   [`ToolOutcome::Denied`].
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
    AgentKind, ChangeApplication, ContentBlock, ContentStatus, FileChange, Id, Knowledge,
    LifecyclePayload, LifecycleTopic, MutationOperation, NativeLifecycle, NodeMutation,
    ObservationPayload, ResultStage, Severity, TextBlock, ThoughtPayload, ThoughtRepresentation,
    ToolCallPayload, ToolCallState, ToolCategory, ToolOutcome, ToolResultPayload, U64, UsageScope,
};
use serde_json::Value;

use crate::adapters::{
    AdapterBinding, AdapterHome, AdapterObservation, FileSignalAdapter, first_mutation,
    message_chunk, message_close, message_payload, node_id, not_emitted, thought_chunk,
    turn_lifecycle,
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

/// Per-tool-call state for revision-monotonic tool translation (D-043).
///
/// `revision` is the last revision emitted on the node: the Proposed
/// `tool_call` opens at 1, each statusless progress update replaces at the
/// next revision, and the terminal result closes one revision above that, so
/// `assemble.ts` `newerMutation` never drops the closing fact.
struct ToolCallTrack {
    /// Journal node shared by the call and its result.
    node: Id,
    /// Last emitted revision for this node.
    revision: u64,
    /// Stable name from `_meta["x.ai/tool"].name`.
    name: Option<String>,
    /// Frame `kind`, used only for the category fallback.
    kind: Option<String>,
    /// Human title; the payload falls back to `name` when this stays absent.
    display_title: Option<String>,
    /// Last seen `rawInput`.
    input: Option<Value>,
    /// The terminal result already emitted for this call.
    finished: bool,
}

impl ToolCallTrack {
    fn new(node: Id) -> Self {
        Self {
            node,
            revision: 1,
            name: None,
            kind: None,
            display_title: None,
            input: None,
            finished: false,
        }
    }
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
    /// Per-call translation state: revision counter and carry-over fields for
    /// the "absent fields keep their previous value" rule (protocol §5.7).
    tools: HashMap<String, ToolCallTrack>,
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
            tools: HashMap::new(),
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
                    if self.tools.contains_key(&call_id) {
                        continue;
                    }
                    let update = &record.frame["params"]["update"];
                    // Protocol §5.7: the stable name is `_meta["x.ai/tool"]
                    // .name`; `title` is the display sentence only.
                    let name = meta_field(update, "name");
                    let kind = meta_field(update, "kind").or_else(|| string(update, "kind"));
                    let title = string(update, "title");
                    let input = update.get("rawInput").cloned();
                    let category = categorize(name.as_deref(), kind.as_deref());
                    let display_title = title.clone().or_else(|| name.clone());
                    let id = node_id(&mut self.nodes, &format!("call:{call_id}"));
                    self.tools.insert(
                        call_id.clone(),
                        ToolCallTrack {
                            node: id.clone(),
                            revision: 1,
                            name: name.clone(),
                            kind,
                            display_title: display_title.clone(),
                            input: input.clone(),
                            finished: false,
                        },
                    );
                    let payload = grok_tool_call_payload(
                        id,
                        name,
                        display_title,
                        category,
                        input,
                        ToolCallState::Proposed,
                        MutationOperation::Open,
                        1,
                        None,
                    );
                    let mut observed = AdapterObservation::structured(payload);
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
                    // A statusless update is the in-progress (Running) mutation;
                    // only a status-bearing update is terminal.
                    let observed = match string(update, "status") {
                        None => self.running_update(&call_id, update, prompt.as_deref()),
                        Some(status) => {
                            self.final_update(&call_id, update, &status, prompt.as_deref())
                        }
                    };
                    if let Some(observed) = observed {
                        out.push(observed);
                    }
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
            // Close (not a fresh Open, as before) with the accumulated text —
            // the same open/append/close contract as the message stream.
            let text = std::mem::take(&mut stream.thought);
            out.push(
                AdapterObservation::structured(thought_close_payload(
                    node,
                    stream.thought_chunks + 1,
                    stream.thought_chunks,
                    text,
                    status,
                ))
                .with_turn(prompt),
            );
        }
        out
    }

    /// Translate a statusless `tool_call_update` into the Running mutation:
    /// same node, `Replace`, next revision. Fields the frame omits keep their
    /// previous value (protocol §5.7 "不含字段保持旧值"). A progress frame for
    /// a call whose Pending frame was never seen is ignored.
    fn running_update(
        &mut self,
        call_id: &str,
        update: &Value,
        prompt: Option<&str>,
    ) -> Option<AdapterObservation> {
        let track = self.tools.get_mut(call_id)?;
        if track.finished {
            return None;
        }
        if let Some(name) = meta_field(update, "name") {
            track.name = Some(name);
        }
        if let Some(kind) = meta_field(update, "kind").or_else(|| string(update, "kind")) {
            track.kind = Some(kind);
        }
        if let Some(title) = string(update, "title") {
            track.display_title = Some(title);
        }
        if let Some(input) = update.get("rawInput") {
            track.input = Some(input.clone());
        }
        let category = categorize(track.name.as_deref(), track.kind.as_deref());
        track.revision += 1;
        let revision = track.revision;
        let payload = grok_tool_call_payload(
            track.node.clone(),
            track.name.clone(),
            track.display_title.clone().or_else(|| track.name.clone()),
            category,
            track.input.clone(),
            ToolCallState::Running,
            MutationOperation::Replace,
            revision,
            Some(revision - 1),
        );
        let mut observed = AdapterObservation::structured(payload);
        observed.item_id = Some(call_id.to_owned());
        if let Some(prompt) = prompt {
            observed.turn_id = Some(prompt.to_owned());
        }
        Some(observed)
    }

    /// Translate the terminal `tool_call_update` into the Final result, closing
    /// the node at a revision strictly greater than the last call revision.
    fn final_update(
        &mut self,
        call_id: &str,
        update: &Value,
        status: &str,
        prompt: Option<&str>,
    ) -> Option<AdapterObservation> {
        // A tail joined mid-call may see the terminal frame first; still give
        // the result its node (the file is replayed from zero on restart, so a
        // later Pending frame dedupes through `self.tools`).
        let track = self.tools.entry(call_id.to_owned()).or_insert_with(|| {
            let node = node_id(&mut self.nodes, &format!("call:{call_id}"));
            ToolCallTrack::new(node)
        });
        if track.finished {
            return None;
        }
        track.finished = true;
        track.revision += 1;
        let revision = track.revision;
        let base_revision = revision - 1;
        let id = track.node.clone();

        let raw = update.get("rawOutput");
        let error = update.get("error").filter(|value| !value.is_null());
        let exit_code = raw
            .and_then(|value| value.get("exit_code"))
            .and_then(Value::as_i64)
            .and_then(|code| i32::try_from(code).ok());
        let outcome = tool_outcome(status, error, exit_code);
        let applied = status == "completed" && error.is_none();
        let (mut blocks, changes) = tool_content(update, applied);
        if !blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Text(_)))
            && let Some(text) = raw
                .and_then(|value| value.get("output_for_prompt"))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        {
            blocks.push(ContentBlock::Text(Box::new(TextBlock {
                text: text.to_owned(),
            })));
        }
        let payload = grok_tool_result_payload(
            id,
            revision,
            base_revision,
            blocks,
            changes,
            Some(update.clone()),
            exit_code,
            outcome,
        );
        let mut observed = AdapterObservation::structured(payload);
        observed.item_id = Some(call_id.to_owned());
        if let Some(prompt) = prompt {
            observed.turn_id = Some(prompt.to_owned());
        }
        Some(observed)
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

/// Read a string field of `_meta["x.ai/tool"]` (the pointer escapes `/` as
/// `~1`, per RFC 6901).
fn meta_field(update: &Value, field: &str) -> Option<String> {
    update
        .pointer(&format!("/_meta/x.ai~1tool/{field}"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Name → category table, from
/// docs/design/grok-structural-translation.md §3.1 (the web registry in
/// `toolRegistry.ts` mirrors the same table).
fn category_for_name(name: &str) -> Option<ToolCategory> {
    match name {
        "run_terminal_command" => Some(ToolCategory::Shell),
        "read_file" | "list_dir" => Some(ToolCategory::FileRead),
        "write" | "search_replace" => Some(ToolCategory::FileWrite),
        "grep" | "web_search" | "web_fetch" | "open_page" | "open_page_with_find" => {
            Some(ToolCategory::Search)
        }
        "spawn_subagent" => Some(ToolCategory::Agent),
        "workflow" => Some(ToolCategory::Workflow),
        "search_tool" | "use_tool" => Some(ToolCategory::Mcp),
        // x_* is the x.ai extension family (search/query tools).
        named if named.starts_with("x_") => Some(ToolCategory::Search),
        _ => None,
    }
}

/// Resolve the category: name table first, then the frame `kind`, `Other`
/// last. Measured kinds are `execute` / `write` / `edit` / `ask_user` /
/// `other` (design doc §3.1); unknown kinds must not invent a category.
fn categorize(name: Option<&str>, kind: Option<&str>) -> ToolCategory {
    if let Some(category) = name.and_then(category_for_name) {
        return category;
    }
    match kind {
        Some("execute") => ToolCategory::Shell,
        Some("write") | Some("edit") => ToolCategory::FileWrite,
        _ => ToolCategory::Other,
    }
}

/// Build a grok `ToolCall` observation payload with explicit revision
/// arithmetic. Revision 1 is the Proposed `Open`; later revisions are
/// `Replace` mutations (the Running progress frames).
#[allow(clippy::too_many_arguments)]
fn grok_tool_call_payload(
    id: Id,
    name: Option<String>,
    display_title: Option<String>,
    category: ToolCategory,
    input: Option<Value>,
    state: ToolCallState,
    operation: MutationOperation,
    revision: u64,
    base_revision: Option<u64>,
) -> ObservationPayload {
    let mutation = if revision == 1 && operation == MutationOperation::Open {
        first_mutation(id.clone())
    } else {
        NodeMutation {
            node_id: id.clone(),
            revision: U64(revision),
            operation,
            base_revision: base_revision.map(U64),
        }
    };
    ObservationPayload::ToolCall(Box::new(ToolCallPayload {
        mutation,
        tool_call_id: id,
        parent_tool_call_id: None,
        tool_name: name.map_or_else(not_emitted, |value| Knowledge::Known { value }),
        display_title: display_title.map_or_else(not_emitted, |value| Knowledge::Known { value }),
        category,
        input: input.map_or_else(not_emitted, |value| Knowledge::Known { value }),
        input_text_delta: None,
        state,
        executor: Knowledge::NotApplicable,
    }))
}

/// Build the Final `ToolResult` at an explicit revision strictly greater than
/// the last call mutation, so the timeline never freezes on an older revision.
#[allow(clippy::too_many_arguments)]
fn grok_tool_result_payload(
    id: Id,
    revision: u64,
    base_revision: u64,
    blocks: Vec<ContentBlock>,
    changes: Vec<FileChange>,
    structured: Option<Value>,
    exit_code: Option<i32>,
    outcome: ToolOutcome,
) -> ObservationPayload {
    ObservationPayload::ToolResult(Box::new(ToolResultPayload {
        mutation: NodeMutation {
            node_id: id.clone(),
            revision: U64(revision),
            operation: MutationOperation::Close,
            base_revision: Some(U64(base_revision)),
        },
        tool_call_id: id,
        stage: ResultStage::Final,
        outcome,
        blocks,
        structured_result: structured.map_or_else(not_emitted, |value| Knowledge::Known { value }),
        exit_code: exit_code.map_or_else(
            || Knowledge::Unknown {
                reason: "not-emitted".into(),
                evidence_event_ids: Vec::new(),
            },
            |code| Knowledge::Known { value: code },
        ),
        changes,
    }))
}

/// Close a thought node with the accumulated text, mirroring `message_close`
/// (one Open stream, one terminal Close snapshot).
fn thought_close_payload(
    node: Id,
    revision: u64,
    base_revision: u64,
    text: String,
    status: ContentStatus,
) -> ObservationPayload {
    ObservationPayload::Thought(Box::new(ThoughtPayload {
        mutation: NodeMutation {
            node_id: node.clone(),
            revision: U64(revision),
            operation: MutationOperation::Close,
            base_revision: Some(U64(base_revision)),
        },
        thought_id: node,
        representation: ThoughtRepresentation::Text,
        text: Some(text),
        part_index: 0,
        status,
    }))
}

/// Map a terminal update's grok `content[]` array to protocol blocks and file
/// changes. `applied` decides a diff's `ChangeApplication`.
fn tool_content(update: &Value, applied: bool) -> (Vec<ContentBlock>, Vec<FileChange>) {
    let Some(blocks) = update.get("content").and_then(Value::as_array) else {
        return (Vec::new(), Vec::new());
    };
    let locations = update.get("locations").and_then(Value::as_array);
    let mut out_blocks = Vec::new();
    let mut changes = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("content") => map_content_inner(block.get("content"), &mut out_blocks),
            Some("diff") => changes.push(map_diff(block, locations, applied)),
            Some("terminal") => {
                // A native terminal reference names its output sink; it is
                // never a promise that Remuda can attach to the tty
                // (protocol §5.7).
                out_blocks.push(text_block(terminal_marker(block)));
            }
            // Unknown block types stay visible rather than vanishing.
            Some(other) => out_blocks.push(text_block(format!("[grok content block: {other}]"))),
            None => {}
        }
    }
    (out_blocks, changes)
}

/// Map the inner object of a `{type:"content"}` block; typed non-text content
/// degrades to a labelled text marker instead of being dropped.
fn map_content_inner(inner: Option<&Value>, out: &mut Vec<ContentBlock>) {
    let Some(inner) = inner else {
        return;
    };
    match inner.get("type").and_then(Value::as_str) {
        Some("text") => {
            if let Some(text) = inner
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                out.push(text_block(text.to_owned()));
            }
        }
        Some(other) => out.push(text_block(format!("[grok content block: {other}]"))),
        None => {}
    }
}

/// Translate one `{type:"diff"}` content block into a `FileChange`.
///
/// The diff block shape below is **synthesized from docs, not captured**
/// ([U] in the design doc): the shipped grok 1.0.30 fixture contains no diff
/// content, so the exact native field spelling is revisited at the 1.0.34
/// recapture (design doc PR7).
fn map_diff(block: &Value, locations: Option<&Vec<Value>>, applied: bool) -> FileChange {
    let path = block
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .or_else(|| block.pointer("/diff/path").and_then(Value::as_str))
        .or_else(|| {
            locations
                .and_then(|items| items.first())
                .and_then(|item| item.get("path").or_else(|| item.get("uri")))
                .and_then(Value::as_str)
        })
        .unwrap_or("")
        .to_owned();
    FileChange {
        path,
        diff: diff_text(block),
        application: if applied {
            ChangeApplication::Applied
        } else {
            ChangeApplication::Unknown
        },
    }
}

/// Best-effort diff text extraction for the [U] diff block shape.
fn diff_text(block: &Value) -> String {
    if let Some(text) = block
        .get("diff")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        return text.to_owned();
    }
    if let Some(text) = block
        .get("patch")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        return text.to_owned();
    }
    if let Some(object) = block.get("diff").filter(|value| value.is_object()) {
        if let Some(text) = object
            .get("diff")
            .or_else(|| object.get("patch"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            return text.to_owned();
        }
        return object.to_string();
    }
    block.to_string()
}

/// Human marker for a `{type:"terminal"}` block; tries the likely native id
/// fields ([U] — the fixture has no terminal content blocks).
fn terminal_marker(block: &Value) -> String {
    let terminal_id = ["terminalId", "terminal_id", "id", "sessionId", "path"]
        .into_iter()
        .find_map(|key| {
            block
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        });
    match terminal_id {
        Some(id) => format!("[grok terminal: {id}]"),
        None => "[grok terminal]".to_owned(),
    }
}

fn text_block(text: String) -> ContentBlock {
    ContentBlock::Text(Box::new(TextBlock { text }))
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
