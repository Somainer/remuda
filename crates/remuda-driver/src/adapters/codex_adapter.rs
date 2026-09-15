//! Codex rollout → journal observations (D-028 §3, §6, §7, §13 P6).
//!
//! What the rollout gives us, and what it does not — every line here traces to
//! [codex-signals-1](../../../docs/design/evidence/codex-signals-1.md):
//!
//! * **Turn boundaries** are `event_msg/task_started` (working) and
//!   `task_complete` (idle) or `turn_aborted{reason:"interrupted"}` (idle,
//!   not a failure: the composer must come back). The `notify` hook also
//!   reports turn completion, but it fires for *title-generation child
//!   threads* too, so it can only corroborate a thread-filtered rollout event
//!   and is never a boundary by itself (§3 [V]).
//! * **Content is item-level.** Only completed items are mapped — codex writes
//!   no text deltas, and advertising token streaming from this file is
//!   explicitly forbidden (§7). `item_completed` of `UserMessage` /
//!   `AgentMessage` / `Reasoning` yields one complete message/thought.
//! * **Tools** come from two representations: `response_item/function_call`
//!   (the proposal, with its JSON `arguments`) and `item_completed` of
//!   `CommandExecution` (the result: status, stdout, exit code). They share
//!   `call_id`; `function_call_output` duplicates the result and is used only
//!   when no `CommandExecution` ever arrives for the call. A late completion
//!   can land under the **old** `turn_id` after an abort (evidence ordinal
//!   86); results are keyed by `call_id`, never re-assigned to the current
//!   turn.
//! * **Usage** is `token_usage_record` (per response, additive) via
//!   [`crate::usage::codex::CodexUsage`]; the cumulative `token_count`
//!   snapshots are fed to the same extractor and refused by its aggregator.
//! * **There is no queue/steer provenance in this file.** Delivered messages
//!   look like ordinary messages; same/different `turn_id` is the only
//!   evidence of steer-vs-queue (§7 [V]). The adapter invents nothing.
//! * **No approval decision exists in this file.** PermissionRequest is a hook
//!   channel (P5); the rollout only shows the resulting rejection string.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use remuda_protocol::{AgentKind, Id, ObservationPayload, ToolOutcome, UsageScope};
use serde_json::Value;

use crate::adapters::{
    AdapterBinding, AdapterHome, AdapterObservation, FileSignalAdapter, message_payload, node_id,
    thought_payload, tool_call_payload, tool_result_payload, turn_lifecycle,
};
use crate::codex_rollout::{
    CodexRolloutEvent, RolloutTail, locate_rollout_in, parse_rollout_line_at,
};
use crate::error::DriverResult;
use crate::usage::codex::CodexUsage;
use crate::usage::{UsageAggregator, to_usage_payload};

/// File-tail adapter for one codex TUI session.
pub struct CodexAdapter {
    home: AdapterHome,
    /// Hook-confirmed session id, when the hook channel reached us first.
    confirmed: Option<String>,
    /// Bound session + its rollout tail.
    binding: Option<AdapterBinding>,
    tail: Option<RolloutTail>,
    /// Native id → journal node id (messages, thoughts, tool calls).
    nodes: HashMap<String, Id>,
    /// Tool calls already announced, so an item without a `function_call`
    /// record does not announce the same call twice.
    announced_tools: HashSet<String>,
    /// Tool calls that already have a result.
    results: HashSet<String>,
    /// Turns already announced as started.
    started_turns: HashSet<String>,
    /// Turns already announced as ended (complete or aborted).
    ended_turns: HashSet<String>,
    /// Per-turn usage snapshot revisions (§5.5: snapshot replaces).
    usage_revisions: HashMap<String, u64>,
    session_usage_revision: u64,
    /// Additive usage extractor + aggregator (refuses cumulative snapshots).
    usage: CodexUsage,
    usage_totals: UsageAggregator,
    /// Optional fallback model for usage records whose turn had no context.
    fallback_model: Option<String>,
}

impl CodexAdapter {
    /// Create an unbound adapter for a shadow `CODEX_HOME`.
    #[must_use]
    pub fn new(home: AdapterHome) -> Self {
        Self {
            home,
            confirmed: None,
            binding: None,
            tail: None,
            nodes: HashMap::new(),
            announced_tools: HashSet::new(),
            results: HashSet::new(),
            started_turns: HashSet::new(),
            ended_turns: HashSet::new(),
            usage_revisions: HashMap::new(),
            session_usage_revision: 0,
            usage: CodexUsage::new(),
            usage_totals: UsageAggregator::new(),
            fallback_model: None,
        }
    }

    /// Seed a model used to price usage when `turn_context` never reported one.
    #[must_use]
    pub fn with_fallback_model(mut self, model: impl Into<String>) -> Self {
        let model = model.into();
        self.fallback_model = Some(model.clone());
        self.usage = self.usage.with_fallback_model(model);
        self
    }

    /// The hook channel confirmed this native session; discovery prefers it.
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

    /// Bind directly to a known rollout file (a `SessionStart` hook or a test
    /// harness may resolve it without going through cwd discovery).
    pub fn bind_rollout(&mut self, session_id: &str, path: PathBuf) {
        if !session_id.trim().is_empty() {
            self.confirmed = Some(session_id.trim().to_owned());
            self.bind(session_id.trim().to_owned(), path);
        }
    }

    /// Bind the rollout tail for a discovered session.
    fn bind(&mut self, session_id: String, path: PathBuf) {
        self.tail = Some(RolloutTail::new(path.clone()));
        self.binding = Some(AdapterBinding {
            session_id: session_id.clone(),
            directory: path.parent().map(|parent| parent.to_path_buf()),
            main_file: Some(path),
        });
    }

    /// Find the session's rollout.
    ///
    /// Preference order: the hook-confirmed id (P1 channel A), then the newest
    /// `session_index.jsonl` name entry. `locate_rollout_in` then verifies the
    /// file's `session_meta.id` and refuses ambiguous matches — the index is a
    /// name index, not a PID registry (§A4 [V]).
    fn discover(&mut self) -> DriverResult<bool> {
        if let Some(id) = self
            .confirmed
            .clone()
            .or_else(|| newest_indexed(&self.home.home))
            && let Some(path) = locate_rollout_in(&self.home.home, &id)
        {
            self.bind(id, path);
            return Ok(true);
        }
        Ok(false)
    }

    /// Map one parsed rollout line to zero or more observations.
    fn on_line(&mut self, ordinal: u64, line: &str) -> Vec<AdapterObservation> {
        let Ok(record) = parse_rollout_line_at(line, ordinal) else {
            return Vec::new();
        };
        // Usage first: the extractor is stateful (turn→model map) and must see
        // records in file order regardless of what else they map to.
        if let Some(event) = self.usage.on_record(&record) {
            self.usage_totals.push(&event);
        }
        let turn = turn_id(&record);
        match &record.event {
            CodexRolloutEvent::SessionMeta { session_id, .. } => {
                if let Some(id) = session_id {
                    self.confirmed = Some(id.clone());
                }
                Vec::new()
            }
            CodexRolloutEvent::TaskStarted { turn_id, .. } => {
                let Some(turn_id) = turn_id else {
                    return Vec::new();
                };
                if self.started_turns.insert(turn_id.clone()) {
                    vec![
                        AdapterObservation::structured(turn_lifecycle("task_started", "working"))
                            .with_turn(turn_id),
                    ]
                } else {
                    Vec::new()
                }
            }
            CodexRolloutEvent::TaskComplete {
                turn_id,
                last_agent_message,
                ..
            } => self.end_turn(
                turn_id.as_deref(),
                "task_complete",
                last_agent_message.clone(),
            ),
            CodexRolloutEvent::ItemCompleted { turn_id, item } => {
                self.on_completed_item(turn_id.clone(), item)
            }
            CodexRolloutEvent::ToolCall {
                call_id,
                name,
                input,
            } => {
                if let Some(call_id) = call_id
                    && self.announced_tools.insert(call_id.clone())
                {
                    let id = node_id(&mut self.nodes, &format!("call:{call_id}"));
                    let mut observed = AdapterObservation::structured(tool_call_payload(
                        id,
                        name.clone(),
                        structured_input(name.as_deref(), input),
                    ));
                    observed.item_id = Some(call_id.clone());
                    if let Some(turn) = turn {
                        observed.turn_id = Some(turn);
                    }
                    return vec![observed];
                }
                Vec::new()
            }
            CodexRolloutEvent::ToolOutput { call_id, output } => {
                // Duplicate of the `CommandExecution` item result; only use it
                // when that item never arrives for the call.
                let Some(call_id) = call_id else {
                    return Vec::new();
                };
                if self.announced_tools.contains(call_id) || self.results.contains(call_id) {
                    return Vec::new();
                }
                self.results.insert(call_id.clone());
                let id = node_id(&mut self.nodes, &format!("call:{call_id}"));
                let text = output.as_str().map(str::to_owned);
                let observed = AdapterObservation::structured(tool_result_payload(
                    id,
                    text,
                    Some(output.clone()),
                    None,
                    ToolOutcome::Unknown,
                ));
                match turn {
                    Some(turn) => vec![observed.with_turn(turn)],
                    None => vec![observed],
                }
            }
            CodexRolloutEvent::Message { .. }
            | CodexRolloutEvent::Reasoning { .. }
            | CodexRolloutEvent::TokenUsage { .. }
            | CodexRolloutEvent::TurnContext { .. }
            | CodexRolloutEvent::Compacted => Vec::new(),
            // The merged parser types the abort; only an explicit interruption
            // is a turn end here — a future/unknown abort reason must not
            // silently mean "interrupted" (A5: the only observed value is
            // "interrupted").
            CodexRolloutEvent::TurnAborted { turn_id, reason } => {
                if reason.as_deref() == Some("interrupted") {
                    self.end_turn(turn_id.as_deref(), "turn_aborted", None)
                } else {
                    Vec::new()
                }
            }
            CodexRolloutEvent::Unknown { .. } => Vec::new(),
        }
    }

    /// Map an `item_completed.item` object.
    fn on_completed_item(
        &mut self,
        turn_id: Option<String>,
        item: &Value,
    ) -> Vec<AdapterObservation> {
        let Some(kind) = item.get("type").and_then(Value::as_str) else {
            return Vec::new();
        };
        let native_id = item
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("anon:{kind}"));
        let mut out = Vec::new();
        match kind {
            "UserMessage" | "AgentMessage" => {
                let key = format!("msg:{native_id}");
                if self.nodes.contains_key(&key) {
                    return out;
                }
                let id = node_id(&mut self.nodes, &key);
                let role = if kind == "UserMessage" {
                    remuda_protocol::MessageRole::User
                } else {
                    remuda_protocol::MessageRole::Assistant
                };
                let text = item_text(item);
                let mut observed = AdapterObservation::structured(message_payload(id, role, text));
                observed.turn_id = turn_id;
                observed.item_id = Some(native_id);
                out.push(observed);
            }
            "Reasoning" => {
                let key = format!("thought:{native_id}");
                if self.nodes.contains_key(&key) {
                    return out;
                }
                let id = node_id(&mut self.nodes, &key);
                if let Some(text) = reasoning_text(item) {
                    let mut observed = AdapterObservation::structured(thought_payload(id, text));
                    observed.turn_id = turn_id;
                    out.push(observed);
                }
            }
            "CommandExecution" => {
                let call_id = native_id.clone();
                if !self.results.insert(call_id.clone()) {
                    return out;
                }
                // The proposal may have arrived as `function_call` already; if
                // not, announce it from the command item so the result is not
                // orphaned.
                let id = node_id(&mut self.nodes, &format!("call:{call_id}"));
                if !self.announced_tools.contains(&call_id) {
                    self.announced_tools.insert(call_id.clone());
                    let command = command_string(item);
                    let input = command
                        .clone()
                        .map(|command| serde_json::json!({ "command": command }));
                    let mut proposed = AdapterObservation::structured(tool_call_payload(
                        id.clone(),
                        Some("Bash".into()),
                        input,
                    ));
                    proposed.turn_id = turn_id.clone();
                    proposed.item_id = Some(call_id.clone());
                    out.push(proposed);
                }
                let (outcome, exit_code) = command_outcome(item);
                let text = item
                    .get("aggregated_output")
                    .or_else(|| item.get("stdout"))
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned);
                let mut result = AdapterObservation::structured(tool_result_payload(
                    id,
                    text,
                    Some(item.clone()),
                    exit_code,
                    outcome,
                ));
                result.turn_id = turn_id;
                result.item_id = Some(call_id);
                out.push(result);
            }
            _ => {}
        }
        out
    }

    /// Emit an end-of-turn lifecycle plus that turn's usage snapshot.
    fn end_turn(
        &mut self,
        turn_id: Option<&str>,
        name: &'static str,
        last_message: Option<String>,
    ) -> Vec<AdapterObservation> {
        let Some(turn_id) = turn_id else {
            return Vec::new();
        };
        if !self.ended_turns.insert(turn_id.to_owned()) {
            return Vec::new();
        }
        let mut out = Vec::new();
        // The last assistant message can arrive only with task_complete in
        // sparse rollouts; if no AgentMessage item carried it, still surface
        // it (item-level, complete).
        if let Some(text) = last_message.filter(|text| !text.is_empty()) {
            let key = "msg:task-complete:".to_owned() + turn_id;
            if !self.nodes.contains_key(&key) {
                let id = node_id(&mut self.nodes, &key);
                out.push(
                    AdapterObservation::structured(message_payload(
                        id,
                        remuda_protocol::MessageRole::Assistant,
                        text,
                    ))
                    .with_turn(turn_id),
                );
            }
        }
        out.push(AdapterObservation::structured(turn_lifecycle(name, "idle")).with_turn(turn_id));
        // Per-turn usage snapshot (replaces prior revisions for the turn).
        if let Some(totals) = self.usage_totals.turn(turn_id)
            && totals.events > 0
        {
            let revision = {
                let entry = self.usage_revisions.entry(turn_id.to_owned()).or_insert(0);
                *entry += 1;
                *entry
            };
            let payload = to_usage_payload(UsageScope::Turn, turn_id.to_owned(), revision, totals);
            out.push(
                AdapterObservation::structured(ObservationPayload::Usage(Box::new(payload)))
                    .with_turn(turn_id),
            );
        }
        // Session snapshot each turn end as well (monotonic revision).
        self.session_usage_revision += 1;
        let payload = to_usage_payload(
            UsageScope::Session,
            self.binding
                .as_ref()
                .map(|binding| binding.session_id.clone())
                .unwrap_or_else(|| "codex-session".into()),
            self.session_usage_revision,
            self.usage_totals.session(),
        );
        out.push(AdapterObservation::structured(ObservationPayload::Usage(
            Box::new(payload),
        )));
        out
    }
}

impl FileSignalAdapter for CodexAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Codex
    }

    fn session_id(&self) -> Option<&str> {
        self.binding
            .as_ref()
            .map(|binding| binding.session_id.as_str())
    }

    fn confirm_session(&mut self, session_id: &str) {
        CodexAdapter::confirm_session(self, session_id);
    }

    fn poll(&mut self) -> DriverResult<Vec<AdapterObservation>> {
        if self.tail.is_none() && !self.discover()? {
            return Ok(Vec::new());
        }
        let Some(tail) = self.tail.as_mut() else {
            return Ok(Vec::new());
        };
        let lines = tail.poll()?;
        let mut out = Vec::new();
        // Physical line index: the tail returns only non-empty lines, so use the
        // parser's own ordinal when present and a running counter otherwise.
        let mut ordinal = 0;
        for line in lines {
            ordinal += 1;
            out.extend(self.on_line(ordinal, &line));
        }
        Ok(out)
    }
}

/// Turn id carried by a record, when present.
fn turn_id(record: &crate::codex_rollout::CodexRolloutRecord) -> Option<String> {
    use crate::codex_rollout::CodexRolloutEvent::*;
    match &record.event {
        TaskStarted { turn_id, .. }
        | TaskComplete { turn_id, .. }
        | ItemCompleted { turn_id, .. }
        | TurnContext { turn_id, .. } => turn_id.clone(),
        _ => None,
    }
}

/// Extract text from a message item, joining its content blocks.
fn item_text(item: &Value) -> String {
    item.get("content")
        .and_then(|content| content.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .or_else(|| block.get("content").and_then(Value::as_str))
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Extract visible reasoning text from a reasoning item (summary first).
fn reasoning_text(item: &Value) -> Option<String> {
    let summary = item
        .get("summary")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|text| !text.is_empty());
    summary.or_else(|| Some(item_text(item)).filter(|text| !text.is_empty()))
}

/// Best-effort command string from a `CommandExecution` item.
fn command_string(item: &Value) -> Option<String> {
    item.get("command")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            item.get("command").and_then(Value::as_array).map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| {
                        part.get("command")
                            .and_then(Value::as_str)
                            .or_else(|| part.as_str())
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        })
}

/// Map a `CommandExecution` status/exit code to outcome + exit code.
fn command_outcome(item: &Value) -> (ToolOutcome, Option<i32>) {
    let exit_code = item
        .get("exit_code")
        .and_then(Value::as_i64)
        .and_then(|code| i32::try_from(code).ok());
    // A hook rejection is persisted as an error status carrying `Rejected(…)`.
    let rejected = item
        .get("status")
        .and_then(Value::as_object)
        .and_then(|status| status.get("message").and_then(Value::as_str))
        .map(|message| message.contains("Rejected"))
        .unwrap_or(false);
    if rejected {
        return (ToolOutcome::Denied, None);
    }
    match exit_code {
        Some(0) => (ToolOutcome::Succeeded, Some(0)),
        Some(code) => (ToolOutcome::Failed, Some(code)),
        None => (ToolOutcome::Unknown, None),
    }
}

/// Coerce a tool call's arguments into structured JSON when possible.
fn structured_input(name: Option<&str>, input: &Value) -> Option<Value> {
    match input {
        Value::String(text) if !text.is_empty() => serde_json::from_str(text).ok().or_else(|| {
            Some(serde_json::json!({
                "command": text,
            }))
        }),
        Value::Null => name.map(|name| serde_json::json!({ "tool": name })),
        other => Some(other.clone()),
    }
}

/// Newest entry of `$CODEX_HOME/session_index.jsonl`, by file order.
///
/// The index is append-only names (§A4 [V]); the last line is the most recently
/// renamed session, which is the least-bad guess only when no hook confirmed an
/// id. Missing/garbled files yield nothing.
fn newest_indexed(home: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(home.join("session_index.jsonl")).ok()?;
    text.lines()
        .rev()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .and_then(|row| row.get("id").and_then(Value::as_str).map(str::to_owned))
}

#[cfg(test)]
mod tests;
