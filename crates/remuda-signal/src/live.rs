//! The live layer: hook events gain the `turn.live` phase vocabulary plus real
//! running tool nodes (live-view design §2.2, §2.3, §3.1).
//!
//! ## Why phases are tags, not second observations
//!
//! [`crate::map::map_event`] keeps producing exactly the raw lifecycle
//! observation the journal has always gotten — its `native_name` is load
//! bearing (`hook_activity` matches `UserPromptSubmit`/`Stop`/… and the message
//! fold in the Node matches `MessageDisplay`). Emitting a *second* lifecycle
//! per phase would also break the driver contract that one delivered hook
//! event is one observation on its stream (`shell_pty` tests). The phase
//! therefore rides along on that one raw observation as the `phase` / `since`
//! / `provision` / `tier` / `completeness` tag set in `related_ids`: one wire
//! event per transition, never one per frame.
//!
//! Tool *content* is different: a running tool had no representation at all
//! (`ToolCallState::Running` and `ResultStage::Final` had zero emitters), so
//! `PreToolUse` / `PostToolUse` additionally produce real
//! [`ToolCallPayload`] / [`ToolResultPayload`] observations on node ids both
//! the hook and the transcript derive independently via [`Id::derive`].
//!
//! ## Safety rules (design §2.2)
//!
//! - A phase is not an `Activity`. Only the raw `UserPromptSubmit` / `Stop` /
//!   `StopFailure` observations move activity in the Node; this layer never
//!   changes those names or statuses.
//! - Content is gated, lifecycle is not. A foreign `claude` whose ppid this bus
//!   never bound still supplies phase tags; it may never supply a tool call or
//!   result. The `allowed` flag is that gate.
//! - `SubagentStop` never reaches this layer (`classify` keeps it
//!   `Diagnostic`).
//! - `unknown` never collapses: a phase no event produced is simply absent and
//!   the projection latches the previous one. `thinking` and `tool-output`
//!   have no claude channel (design §0.3, §2.3), so this mapper never emits
//!   their tags — the variants exist for file/RPC adapters.

use crate::event::HookEvent;
use crate::map::{Mapped, MappedKind, phase};
use remuda_protocol::{
    ContentBlock, Id, Knowledge, LifecyclePayload, LifecycleTopic, MutationOperation,
    NativeLifecycle, NodeMutation, ObservationPayload, ResultStage, Severity, TextBlock,
    ToolCallPayload, ToolCallState, ToolCategory, ToolOutcome, ToolResultPayload, U64,
};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

/// Tag carrying the live phase spelling (`related_ids["phase"]`).
pub const PHASE_KEY: &str = "phase";
/// Tag carrying the RFC3339-ms phase-start anchor (`related_ids["since"]`).
pub const SINCE_KEY: &str = "since";
/// Fidelity tag keys (design §2.7).
pub const PROVISION_KEY: &str = "provision";
/// Tag key naming the channel that produced the phase.
pub const TIER_KEY: &str = "tier";
/// Tag key naming how complete this record is (`structured`/`partial`).
pub const COMPLETENESS_KEY: &str = "completeness";
/// Hook-produced values for the fidelity tags.
pub const PROVISION_NATIVE: &str = "native";
/// The hook tier spelling, matching `SignalTier`/`SourceChannel`.
pub const TIER_HOOK: &str = "hook";

/// The native lifecycle name a confirmed cancel is journaled under.
pub const INTERRUPTED_NAME: &str = "interrupted";

/// The nine live phases, verbatim from design §2.2 / harness-parity §6.2 so the
/// parity matrix is the acceptance table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// `UserPromptSubmit`: the turn was accepted.
    PromptAccepted,
    /// Reasoning before the first action. No claude channel; never emitted here.
    Thinking,
    /// `PreToolUse`: a tool started.
    ToolStarted,
    /// Live tool output. No claude channel; reserved for RPC adapters.
    ToolOutput,
    /// `PostToolUse` / `PostToolUseFailure` / `PostToolBatch`: a tool finished.
    ToolFinished,
    /// `MessageDisplay`: assistant text is streaming (line level).
    TextStreaming,
    /// `Stop` / `StopFailure`: the turn ended.
    TurnEnded,
    /// `PermissionRequest` / `Elicitation` / `Notification`: blocked on a human.
    Blocked,
    /// The turn was cancelled (synthesized by the bus).
    Interrupted,
}

impl Phase {
    /// Wire spelling carried in `related_ids["phase"]`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::PromptAccepted => "prompt-accepted",
            Phase::Thinking => "thinking",
            Phase::ToolStarted => "tool-started",
            Phase::ToolOutput => "tool-output",
            Phase::ToolFinished => "tool-finished",
            Phase::TextStreaming => "text-streaming",
            Phase::TurnEnded => "turn-ended",
            Phase::Blocked => "blocked",
            Phase::Interrupted => "interrupted",
        }
    }
}

/// Derive the node id a tool call occupies on every channel.
///
/// The hook bus and the transcript tailer must call this with exactly the same
/// arguments — the owning instance id and the harness's own `tool_use_id` — so
/// a transcript record that lands seconds later upgrades the hook's node in
/// place instead of drawing a second card (design §2.3). Do not namespace
/// `native_id` here: the transcript's tool block id is the same string.
#[must_use]
pub fn tool_node_id(scope: &str, native_id: &str) -> Option<Id> {
    Id::derive("obj", scope, native_id).ok()
}

/// What folding one hook event adds: phase tags merged onto the raw lifecycle,
/// and gated content observations emitted beside it.
#[derive(Debug, Default)]
pub struct LiveFold {
    /// Tags merged into the raw observation's `related_ids` (lifecycle;
    /// ungated). Present on every raw event that belongs to a phase, including
    /// re-deliveries — they describe the evidence.
    pub related: BTreeMap<String, String>,
    /// True only when this event opened a new latched phase. The raw
    /// re-delivery of an already-latched phase (a second chunk, a re-fired
    /// hook) is tagged but is not a new transition — that distinction is the
    /// "one observation per transition" guard (design §4.1).
    pub transition: bool,
    /// Content observations (`ToolCall` / `ToolResult`); gated behind
    /// `owns(ppid)`.
    pub extras: Vec<ObservationPayload>,
}

/// State folded across one bus's hook stream. Pure: no IO, no clock beyond the
/// injected `now`.
#[derive(Debug, Default)]
pub struct LiveState {
    /// Instance id, the [`Id::derive`] scope.
    scope: String,
    /// Tool calls seen via `PreToolUse` (or a batch) and not yet finished.
    open: HashSet<String>,
    /// Tool calls already reported finished, so a later `PostToolBatch`
    /// cannot emit a second result.
    finished: HashSet<String>,
    /// The phase currently latched, so a repeated hook is not a new transition.
    current: Option<Phase>,
    /// Anchor of the latched phase; later chunks of one episode share it.
    phase_since: Option<String>,
    /// Last `prompt_id` seen; a grouping key, never a dedupe key (design §2.2).
    last_prompt_id: Option<String>,
}

impl LiveState {
    /// Fresh state scoped to one instance.
    #[must_use]
    pub fn new(scope: impl Into<String>) -> Self {
        Self {
            scope: scope.into(),
            ..Self::default()
        }
    }

    /// Currently latched phase.
    #[must_use]
    pub fn phase(&self) -> Option<Phase> {
        self.current
    }

    /// Fold one classified hook event. `allowed` is the bus's content gate
    /// (`owns(ppid)`): phase tags are returned regardless; tool call/result
    /// payloads only when `allowed`.
    pub fn observe(
        &mut self,
        event: &HookEvent,
        mapped: &Mapped,
        now: &str,
        allowed: bool,
    ) -> LiveFold {
        let Some(phase) = phase(event, mapped.kind) else {
            return LiveFold::default();
        };
        let mut fold = LiveFold::default();
        match mapped.kind {
            MappedKind::TurnStarted => {
                // A new prompt starts a fresh tool/phase slate. prompt_id
                // groups only; each submit is its own transition.
                self.open.clear();
                self.finished.clear();
                self.last_prompt_id = event.text("prompt_id").map(ToOwned::to_owned);
                self.phase_tags(event, phase, now, None, &mut fold.related);
                // Every submit is its own transition even when queued prompts
                // share one prompt_id (D-028 §14 risk 6): prompt_id groups, it
                // must never dedupe. Force the latch past equality.
                self.current = Some(phase);
                self.phase_since = Some(now.to_owned());
                fold.transition = true;
            }
            MappedKind::TurnEnded => {
                self.phase_tags(event, phase, now, None, &mut fold.related);
                fold.related.insert(
                    "outcome".into(),
                    if event.name == "StopFailure" {
                        "failed"
                    } else {
                        "completed"
                    }
                    .into(),
                );
                self.open.clear();
                self.finished.clear();
                fold.transition = self.latch(phase, now);
            }
            MappedKind::InteractionObserved => {
                // PermissionRequest then its delayed Notification is one wait.
                if self.current != Some(Phase::Blocked) {
                    self.phase_tags(event, phase, now, None, &mut fold.related);
                    fold.transition = self.latch(phase, now);
                }
            }
            MappedKind::MessageDelta => {
                // One episode: the first chunk anchors `since`; every chunk
                // still carries the phase so a late tailer reading one chunk
                // knows what it is.
                let since = self.phase_since.clone().unwrap_or_else(|| now.to_owned());
                self.phase_tags(event, phase, &since, None, &mut fold.related);
                fold.transition = self.latch(phase, &since);
            }
            MappedKind::ToolStarted => {
                let Some(native_id) = event.text("tool_use_id") else {
                    return fold;
                };
                self.phase_tags(
                    event,
                    phase,
                    now,
                    Some((native_id, event.text("tool_name"))),
                    &mut fold.related,
                );
                // Hooks can re-fire; the card opens once. The phase still tags
                // the raw re-delivery (raw evidence is journaled regardless).
                if !self.open.contains(native_id)
                    && !self.finished.contains(native_id)
                    && allowed
                    && let Some(payload) = self.tool_call(event, native_id, event.text("tool_name"))
                {
                    fold.extras.push(payload);
                }
                self.open.insert(native_id.to_owned());
                fold.transition = self.latch(phase, now);
            }
            MappedKind::ToolFinished | MappedKind::ToolFailed => {
                let failed = mapped.kind == MappedKind::ToolFailed;
                if event.name == "PostToolBatch" {
                    if let Some(calls) = event.payload.get("tool_calls").and_then(Value::as_array) {
                        for call in calls {
                            let Some(native_id) = call.get("tool_use_id").and_then(Value::as_str)
                            else {
                                continue;
                            };
                            self.finish_tool(
                                event,
                                native_id,
                                call.get("tool_name").and_then(Value::as_str),
                                call.get("tool_response"),
                                failed,
                                phase,
                                now,
                                allowed,
                                &mut fold,
                            );
                        }
                    }
                } else if let Some(native_id) = event.text("tool_use_id") {
                    self.finish_tool(
                        event,
                        native_id,
                        event.text("tool_name"),
                        event.payload.get("tool_response"),
                        failed,
                        phase,
                        now,
                        allowed,
                        &mut fold,
                    );
                }
            }
            // Session start/end, diagnostics, SubagentStop: no live phase.
            _ => {}
        }
        fold
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_tool(
        &mut self,
        event: &HookEvent,
        native_id: &str,
        tool_name: Option<&str>,
        response: Option<&Value>,
        failed: bool,
        phase: Phase,
        now: &str,
        allowed: bool,
        fold: &mut LiveFold,
    ) {
        // PostToolUse always precedes PostToolBatch for the same call. A
        // summary for an already-finished call is the same transition, so it
        // carries no phase tag and no result.
        if self.finished.contains(native_id) {
            return;
        }
        self.phase_tags(
            event,
            phase,
            now,
            Some((native_id, tool_name)),
            &mut fold.related,
        );
        // duration_ms excludes permission-prompt and hook time (claude-channels
        // §1.3): tool time, never wall time. No schema field exists for it on
        // ToolResult, so it rides the phase as a labelled tag.
        if let Some(duration_ms) = event.payload.get("duration_ms").and_then(Value::as_u64) {
            fold.related
                .insert("durationMs".into(), duration_ms.to_string());
        }
        let was_open = self.open.remove(native_id);
        if allowed {
            fold.extras
                .push(self.tool_result(native_id, response, failed, was_open, event));
        }
        self.finished.insert(native_id.to_owned());
        fold.transition = self.latch(phase, now);
    }

    fn latch(&mut self, phase: Phase, now: &str) -> bool {
        if self.current == Some(phase) {
            false
        } else {
            self.current = Some(phase);
            self.phase_since = Some(now.to_owned());
            true
        }
    }

    fn tool_call(
        &self,
        event: &HookEvent,
        native_id: &str,
        tool_name: Option<&str>,
    ) -> Option<ObservationPayload> {
        let node = tool_node_id(&self.scope, native_id)?;
        let name = tool_name.unwrap_or("unknown");
        Some(ObservationPayload::ToolCall(Box::new(ToolCallPayload {
            mutation: NodeMutation {
                node_id: node.clone(),
                revision: U64(1),
                operation: MutationOperation::Open,
                base_revision: None,
            },
            tool_call_id: node,
            parent_tool_call_id: None,
            tool_name: knowledge(name.to_owned()),
            display_title: knowledge(name.to_owned()),
            category: tool_category(name),
            input: match event.payload.get("tool_input") {
                Some(value) => knowledge(value.clone()),
                None => unknown("not-emitted"),
            },
            input_text_delta: None,
            state: ToolCallState::Running,
            executor: unknown("not-emitted"),
        })))
    }

    fn tool_result(
        &self,
        native_id: &str,
        response: Option<&Value>,
        failed: bool,
        was_open: bool,
        event: &HookEvent,
    ) -> ObservationPayload {
        let node = tool_node_id(&self.scope, native_id)
            .unwrap_or_else(|| Id::new("obj").expect("obj prefix"));
        let interrupted = response.is_some_and(response_interrupted)
            || event.payload.get("is_interrupt").and_then(Value::as_bool) == Some(true);
        let outcome = if interrupted {
            ToolOutcome::Cancelled
        } else if failed {
            ToolOutcome::Failed
        } else {
            ToolOutcome::Succeeded
        };
        let text = response.map(tool_response_text).unwrap_or_default();
        // A result whose open we saw closes revision 2; an orphan result
        // (missed/batch-only) matches the transcript mapper's rev-1 close.
        let (revision, base_revision) = if was_open {
            (U64(2), Some(U64(1)))
        } else {
            (U64(1), None)
        };
        ObservationPayload::ToolResult(Box::new(ToolResultPayload {
            mutation: NodeMutation {
                node_id: node.clone(),
                revision,
                operation: MutationOperation::Close,
                base_revision,
            },
            tool_call_id: node,
            stage: ResultStage::Final,
            outcome,
            blocks: if text.is_empty() {
                Vec::new()
            } else {
                vec![ContentBlock::Text(Box::new(TextBlock { text }))]
            },
            structured_result: match response {
                Some(value) => knowledge(value.clone()),
                None => match event.payload.get("error") {
                    Some(error) => knowledge(error.clone()),
                    None => unknown("not-emitted"),
                },
            },
            exit_code: response
                .and_then(tool_exit_code)
                .map_or_else(|| unknown("not-emitted"), knowledge),
            changes: Vec::new(),
        }))
    }

    /// Fill the common `turn.live` tag set for one phase transition.
    fn phase_tags(
        &self,
        event: &HookEvent,
        phase: Phase,
        since: &str,
        tool: Option<(&str, Option<&str>)>,
        related: &mut BTreeMap<String, String>,
    ) {
        related.insert(PHASE_KEY.into(), phase.as_str().into());
        // The elapsed anchor; the browser renders `now - since`, locally.
        related.insert(SINCE_KEY.into(), since.to_owned());
        related.insert(PROVISION_KEY.into(), PROVISION_NATIVE.into());
        related.insert(TIER_KEY.into(), TIER_HOOK.into());
        // Hook evidence is structured, except streamed text: a line-level
        // display echo is Partial by construction (design §2.5, §2.7).
        related.insert(
            COMPLETENESS_KEY.into(),
            if phase == Phase::TextStreaming {
                "partial"
            } else {
                "structured"
            }
            .into(),
        );
        if let Some(prompt_id) = event.text("prompt_id").or(self.last_prompt_id.as_deref()) {
            related.insert("promptId".into(), prompt_id.to_owned());
        }
        if let Some((tool_call_id, tool_name)) = tool {
            related.insert("toolCallId".into(), tool_call_id.to_owned());
            if let Some(tool_name) = tool_name {
                related.insert("toolName".into(), tool_name.to_owned());
            }
        }
        if phase == Phase::TextStreaming {
            if let Some(message_id) = event.text("message_id") {
                related.insert("messageId".into(), message_id.to_owned());
            }
            if let Some(index) = event.payload.get("index").and_then(Value::as_u64) {
                related.insert("chunkIndex".into(), index.to_string());
            }
            if let Some(is_final) = event.payload.get("final").and_then(Value::as_bool) {
                related.insert("final".into(), is_final.to_string());
            }
        }
    }
}

/// Build the lifecycle observation carrying the interruption tags, used by the
/// bus when it synthesizes the confirmed cancel.
#[must_use]
pub fn interrupted_lifecycle(event: &HookEvent, now: &str) -> ObservationPayload {
    let mut related = BTreeMap::new();
    related.insert(PHASE_KEY.into(), Phase::Interrupted.as_str().into());
    related.insert(SINCE_KEY.into(), now.to_owned());
    related.insert(PROVISION_KEY.into(), PROVISION_NATIVE.into());
    related.insert(TIER_KEY.into(), TIER_HOOK.into());
    related.insert(COMPLETENESS_KEY.into(), "structured".into());
    related.insert("outcome".into(), "cancelled".into());
    if let Some(prompt_id) = event.text("prompt_id") {
        related.insert("promptId".into(), prompt_id.to_owned());
    }
    ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
        NativeLifecycle {
            topic: LifecycleTopic::Turn,
            native_name: INTERRUPTED_NAME.into(),
            native_id: match event.text("turn_id") {
                Some(turn_id) => knowledge(turn_id.to_owned()),
                None => unknown("not-emitted"),
            },
            status: knowledge("idle".to_owned()),
            related_ids: related,
            data_ref: None,
            severity: Severity::Info,
            affects_completion: false,
        },
    ))))
}

fn knowledge<T>(value: T) -> Knowledge<T> {
    Knowledge::Known { value }
}

fn unknown<T>(reason: &str) -> Knowledge<T> {
    Knowledge::Unknown {
        reason: reason.into(),
        evidence_event_ids: Vec::new(),
    }
}

/// Tool category for a native name. Kept byte-identical to the transcript
/// mapper's `tool_category` in `remuda-journal::claude` so hook and transcript
/// payloads for one tool never disagree.
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

/// `tool_response.interrupted` — the tool was cancelled mid-run.
fn response_interrupted(response: &Value) -> bool {
    response
        .get("interrupted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Best-effort exit code from a tool response.
fn tool_exit_code(response: &Value) -> Option<i32> {
    response
        .get("exitCode")
        .or_else(|| response.get("exit_code"))
        .and_then(Value::as_i64)
        .and_then(|code| i32::try_from(code).ok())
}

/// Read the human-readable result text from a `tool_response`.
fn tool_response_text(response: &Value) -> String {
    match response {
        Value::String(text) => text.clone(),
        Value::Object(_) => {
            if let Some(text) = response
                .get("stdout")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                return text.to_owned();
            }
            if let Some(items) = response.get("content").and_then(Value::as_array) {
                return items
                    .iter()
                    .filter_map(|item| item.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("");
            }
            String::new()
        }
        _ => String::new(),
    }
}
