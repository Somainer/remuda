//! Transcript record classification and assistant-record regrouping (D-028 §7).
//!
//! Two defects this module exists to fix, both of them visible in the 结构 view:
//!
//! 1. **Regrouping.** A Claude transcript record carries *one* content block,
//!    and 2–7 consecutive records share one `message.id`. Feeding each record
//!    to the stdout mapper as if it were a whole assistant message made the
//!    per-message tool bookkeeping slide by one. Records are buffered by
//!    `(requestId, message.id)` and replayed as a single message.
//!
//! 2. **Authorship.** `role: "user"` does not mean "the human typed this".
//!    Claude files skill bodies, slash-command expansions, local command
//!    output, hook context, task notifications, tool results and compaction
//!    summaries under the same role. Rendering them all as the user's own
//!    words is the duplication the user reported. Each record is classified by
//!    evidence into a [`MessageOrigin`] that travels on the journal message, so
//!    the UI can collapse injections instead of clients re-guessing.
//!
//! Measured against claude 2.1.221; see `docs/design/evidence/native-pty-3.md`.

use remuda_protocol::MessageOrigin;
use serde_json::Value;

/// The text of a record's `message.content`, whatever shape it took.
///
/// A record is either a bare string or an array of blocks. Classification only
/// needs the text, so both collapse to one string here.
pub(crate) fn record_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// True when the record's content array holds a `tool_result` block.
fn has_tool_result(message: &Value) -> bool {
    message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
}

/// Claude's own statement of who wrote a record, when it makes one.
///
/// `origin.kind` and `promptSource` appear on roughly 1.5% of records in the
/// wild (measured over ~40k local records), but where they exist they are the
/// harness speaking directly and therefore outrank every heuristic below.
fn declared_origin(record: &Value) -> Option<MessageOrigin> {
    if let Some(kind) = record.pointer("/origin/kind").and_then(Value::as_str) {
        match kind {
            "human" => return Some(MessageOrigin::Human),
            "task-notification" => return Some(MessageOrigin::HookContext),
            // `coordinator` / `peer` are other Remuda-like drivers talking to
            // this agent. They are not the human at this keyboard, but they are
            // a real correspondent rather than injected boilerplate, so they
            // stay visible.
            "coordinator" | "peer" => return Some(MessageOrigin::Human),
            _ => {}
        }
    }
    match record.get("promptSource").and_then(Value::as_str) {
        // `typed` is the strongest positive evidence there is: a human pressed
        // keys. `sdk` means a caller drove `-p`, which is still the requester.
        Some("typed" | "sdk" | "queued") => Some(MessageOrigin::Human),
        Some("system") => Some(MessageOrigin::HookContext),
        _ => None,
    }
}

/// Classify one `user`-role transcript record by evidence.
///
/// Order matters: the structural flags (`isCompactSummary`, `sourceToolUseID`,
/// a `tool_result` block, `isMeta`) are facts about the record, so they are
/// checked before the text sniffing, which is only a shape guess. Claude's own
/// declaration wins over both.
///
/// Anything unrecognised is [`MessageOrigin::Human`], not `Unknown`: a
/// misclassification must show one row too many, never silently swallow
/// something the user actually said.
#[must_use]
pub(crate) fn classify_user_record(record: &Value, message: &Value) -> MessageOrigin {
    if record.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
        return MessageOrigin::Compaction;
    }
    if record
        .get("sourceToolUseID")
        .and_then(Value::as_str)
        .is_some()
        || has_tool_result(message)
    {
        return MessageOrigin::ToolResult;
    }
    if let Some(declared) = declared_origin(record) {
        return declared;
    }
    // The expanded skill body arrives flagged as meta rather than marked up.
    if record.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return MessageOrigin::InjectedSkill;
    }
    let text = record_text(message);
    let head = text.trim_start();
    if head.starts_with("<command-message>")
        || head.starts_with("<command-name>")
        || head.starts_with("<command-args>")
    {
        return MessageOrigin::InjectedSkill;
    }
    if head.starts_with("<local-command-stdout>") || head.starts_with("<local-command-stderr>") {
        return MessageOrigin::InjectedCommandOutput;
    }
    if head.starts_with("<system-reminder>") || head.starts_with("<task-notification>") {
        return MessageOrigin::HookContext;
    }
    // The banner a resumed-after-compaction session opens with.
    if head.starts_with("Caveat: The messages below were generated by the user while running") {
        return MessageOrigin::Compaction;
    }
    MessageOrigin::Human
}

/// Identity a run of assistant records is grouped under.
///
/// `requestId` disambiguates the (rare but real) case of one `message.id` being
/// reused across retries. It is absent in claude 2.1.221, so the key degrades
/// to `message.id` alone rather than refusing to group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroupKey {
    request_id: Option<String>,
    message_id: String,
    parent_tool_use_id: Option<String>,
}

impl GroupKey {
    /// Read the grouping identity out of an assistant record, if it has one.
    pub(crate) fn of(record: &Value, message: &Value) -> Option<Self> {
        Some(Self {
            request_id: record
                .get("requestId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            message_id: message.get("id").and_then(Value::as_str)?.to_owned(),
            parent_tool_use_id: record
                .get("parentToolUseId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        })
    }
}

/// Assistant records buffered under one `(requestId, message.id)`.
///
/// Claude writes one content block per record and repeats the same
/// `stop_reason` on every one of them. c-ctxusage r3 split the buffer into two
/// independent channels so a poll boundary can never lose usage:
/// - **content** blocks are drained as they arrive (each block is sent
///   downstream exactly once), while
/// - the **usage/provenance** state (`key`, `head`, `last_usage`,
///   `saw_stop_reason`) is retained until the run is *superseded* — a
///   different message id, a user record, or end of input.
///
/// Draining content at a poll therefore never resets counters: a record with a
/// stop_reason followed by a later same-`message.id` record with different
/// counters still finalises with the LAST counters (a provisional snapshot is
/// replaced by revision instead of being frozen keep-first).
#[derive(Debug, Default)]
pub(crate) struct Group {
    key: Option<GroupKey>,
    /// The record the assembled message inherits its envelope from.
    head: Option<Value>,
    /// Envelope of the FIRST record whose blocks are not drained yet.
    ///
    /// `head` is always the run's FIRST record (kept for key/model/usage
    /// provenance), but a content drain must forward the envelope of the
    /// record whose blocks it actually carries: a second poll drains a later
    /// record, which has its OWN uuid. Reusing `head`'s uuid made the stdout
    /// mapper reject the assembled frame as an already-seen snapshot AFTER the
    /// blocks were taken, dropping every later block of one message
    /// (c-ctxusage r4 item 1 regression).
    pending_head: Option<Value>,
    /// `(apiBlockIndex, arrival order, block)` not yet drained as content.
    blocks: Vec<(Option<u64>, usize, Value)>,
    /// The LAST record's `message.usage` for the group (never a sum): the final
    /// per-call counters.
    last_usage: Option<Value>,
    /// Raw top-level `timestamp` of the record that supplied `last_usage`, used
    /// as the usage observation's native time (c-ctxusage r3 item 5).
    last_usage_at: Option<String>,
    /// True once a buffered record carried a non-null `message.stop_reason`.
    saw_stop_reason: bool,
    /// Global arrival counter (drives same-`apiBlockIndex` tie ordering).
    seen: usize,
    /// Number of usage snapshots already emitted for this message; the next
    /// emit is revision + 1.
    usage_revision: u64,
    /// New usage (or a freshly-seen stop_reason) arrived since the last usage
    /// emit. A poll only (re)publishes when this is set.
    usage_dirty: bool,
}

/// One content drain: the assembled record carrying only blocks not yet sent.
pub(crate) struct DrainedContent {
    pub record: Value,
}

/// A usage snapshot ready to publish for the current group.
pub(crate) struct PendingUsage {
    /// Assistant message id (`message.id`).
    pub message_id: String,
    /// Top-level `requestId` when the record carried one.
    pub request_id: Option<String>,
    /// Model from `message.model` or the top-level record.
    pub model: Option<String>,
    /// The group's last `message.usage` object.
    pub usage: Value,
    /// Snapshot revision for this message (1 for the first publish).
    pub revision: u64,
    /// Raw top-level record timestamp for the usage-bearing record.
    pub native_at: Option<String>,
}

impl Group {
    /// Whether `key` continues the run currently buffered.
    pub(crate) fn continues(&self, key: &GroupKey) -> bool {
        self.key.as_ref() == Some(key)
    }

    /// Whether anything is buffered (test helper).
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    /// Start a new run at `key`, carrying `record` as the envelope source.
    pub(crate) fn start(&mut self, key: GroupKey, record: Value) {
        self.key = Some(key);
        self.head = Some(record.clone());
        self.pending_head = None;
        self.blocks.clear();
        self.last_usage = None;
        self.last_usage_at = None;
        self.saw_stop_reason = false;
        self.seen = 0;
        self.usage_revision = 0;
        self.usage_dirty = false;
        if let Some(message) = record.pointer("/message") {
            self.note_usage(&record, message);
        }
        if record
            .pointer("/message/stop_reason")
            .and_then(Value::as_str)
            .is_some()
        {
            self.saw_stop_reason = true;
        }
    }

    /// Buffer one record's content blocks and fold in its usage/stop metadata.
    pub(crate) fn push(&mut self, record: &Value, message: &Value) {
        self.note_usage(record, message);
        if !self.saw_stop_reason && message.get("stop_reason").and_then(Value::as_str).is_some() {
            self.saw_stop_reason = true;
            // A stop without fresh usage still makes the buffered usage
            // publishable at the next poll.
            if self.last_usage.is_some() {
                self.usage_dirty = true;
            }
        }
        let index = record.get("apiBlockIndex").and_then(Value::as_u64);
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            return;
        };
        if self.blocks.is_empty() {
            // Earliest record not yet drained: its envelope carries the next
            // drain's uuid.
            self.pending_head = Some(record.clone());
        }
        for block in blocks {
            self.blocks.push((index, self.seen, block.clone()));
            self.seen += 1;
        }
    }

    /// Fold one message's `usage` (if object-shaped) into the retained counters
    /// and remember when it was reported.
    fn note_usage(&mut self, record: &Value, message: &Value) {
        if let Some(usage) = message.get("usage").filter(|value| value.is_object()) {
            self.last_usage = Some(usage.clone());
            self.last_usage_at = record
                .get("timestamp")
                .and_then(Value::as_str)
                .map(str::to_owned);
            self.usage_dirty = true;
        }
    }

    /// Drain only the content blocks not yet sent, assembled on the retained
    /// envelope. Does NOT touch the usage/key/stop state.
    pub(crate) fn drain_content(&mut self) -> Option<DrainedContent> {
        if self.blocks.is_empty() {
            return None;
        }
        // The first undrained record's envelope; fall back to `head` only for
        // robustness (a record with blocks always set `pending_head`).
        let envelope = self.pending_head.take().or_else(|| self.head.clone())?;
        let mut assembled = envelope;
        let mut blocks = std::mem::take(&mut self.blocks);
        blocks.sort_by_key(|(index, arrival, _)| (index.unwrap_or(0), *arrival));
        let content: Vec<Value> = blocks.into_iter().map(|(_, _, block)| block).collect();
        if let Some(message) = assembled.get_mut("message").and_then(Value::as_object_mut) {
            message.insert("content".into(), Value::Array(content));
        }
        Some(DrainedContent { record: assembled })
    }

    /// Whether a poll may publish a provisional usage snapshot: the run has
    /// seen its stop_reason and new usage/stop metadata is pending.
    pub(crate) fn usage_publishable_at_poll(&self) -> bool {
        self.saw_stop_reason && self.usage_dirty && self.last_usage.is_some()
    }

    /// Whether a supersede must publish the final usage: it was never
    /// published, or fresh counters arrived after the provisional publish.
    pub(crate) fn usage_publishable_on_supersede(&self) -> bool {
        self.last_usage.is_some() && (self.usage_dirty || self.usage_revision == 0)
    }

    /// Take the next usage snapshot (advancing the revision), clearing the
    /// dirty flag but retaining the run so a later same-id record can publish a
    /// higher revision. `None` when the run carries no usage.
    pub(crate) fn take_usage(&mut self) -> Option<PendingUsage> {
        let head = self.head.as_ref()?;
        let usage = self.last_usage.clone()?;
        self.usage_revision += 1;
        self.usage_dirty = false;
        let message_id = head
            .pointer("/message/id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let request_id = head
            .get("requestId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let model = head
            .pointer("/message/model")
            .or_else(|| head.get("model"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        Some(PendingUsage {
            message_id,
            request_id,
            model,
            usage,
            revision: self.usage_revision,
            native_at: self.last_usage_at.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(record: Value) -> MessageOrigin {
        let message = record.get("message").cloned().unwrap_or(Value::Null);
        classify_user_record(&record, &message)
    }

    /// The plain case: text the human typed is the only thing that renders as
    /// their own words.
    #[test]
    fn a_typed_prompt_is_the_humans_own_words() {
        assert_eq!(
            user(json!({"type": "user", "message": {"content": "run the tests"}})),
            MessageOrigin::Human
        );
    }

    /// `/skill` invocations file the command markup under the user's role. This
    /// is half of the duplication the user reported: the markup *and* the
    /// expanded body both appeared as things they had said.
    #[test]
    fn a_slash_command_invocation_is_an_injection_not_a_prompt() {
        assert_eq!(
            user(json!({
                "type": "user",
                "message": {"content": "<command-message>p3probe</command-message>\n<command-name>/p3probe</command-name>"},
            })),
            MessageOrigin::InjectedSkill
        );
    }

    /// The expanded skill body arrives flagged `isMeta`, with no markup to
    /// sniff — the flag is the only evidence available.
    #[test]
    fn an_expanded_skill_body_is_recognised_by_its_meta_flag() {
        assert_eq!(
            user(json!({
                "type": "user",
                "isMeta": true,
                "message": {"content": [{"type": "text", "text": "Base directory for this skill: /w/.claude/skills/x\n\n# Workflow authoring reference"}]},
            })),
            MessageOrigin::InjectedSkill
        );
    }

    /// Tool results are the bulk of every transcript (~40k of 41k user records
    /// measured). They belong inside their tool card, never in a bubble.
    #[test]
    fn a_tool_result_is_classified_from_its_block_or_its_source_id() {
        assert_eq!(
            user(json!({
                "type": "user",
                "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}]},
            })),
            MessageOrigin::ToolResult
        );
        assert_eq!(
            user(json!({
                "type": "user",
                "sourceToolUseID": "toolu_1",
                "message": {"content": [{"type": "text", "text": "ok"}]},
            })),
            MessageOrigin::ToolResult
        );
    }

    /// A background task reporting back is not the user speaking, even though
    /// Claude gives it `promptSource: "sdk"`. The explicit `origin.kind` is the
    /// more specific statement and has to win.
    #[test]
    fn a_task_notification_outranks_its_prompt_source() {
        assert_eq!(
            user(json!({
                "type": "user",
                "origin": {"kind": "task-notification"},
                "promptSource": "sdk",
                "message": {"content": "<task-notification>\n<task-id>w7d</task-id>\n</task-notification>"},
            })),
            MessageOrigin::HookContext
        );
    }

    /// Hook `additionalContext` and system reminders reach the model as user
    /// turns; they are context, not conversation.
    #[test]
    fn injected_context_and_command_output_are_separated() {
        assert_eq!(
            user(
                json!({"type": "user", "message": {"content": "<system-reminder>be careful</system-reminder>"}})
            ),
            MessageOrigin::HookContext
        );
        assert_eq!(
            user(
                json!({"type": "user", "message": {"content": "<local-command-stdout>ok</local-command-stdout>"}})
            ),
            MessageOrigin::InjectedCommandOutput
        );
    }

    /// A compaction summary is the transcript talking about itself.
    #[test]
    fn a_compact_summary_is_not_a_turn() {
        assert_eq!(
            user(json!({
                "type": "user",
                "isCompactSummary": true,
                "message": {"content": "Here is a summary of the conversation so far."},
            })),
            MessageOrigin::Compaction
        );
    }

    /// A record that matches nothing renders as the user's words. Showing one
    /// row too many is recoverable; hiding what someone said is not.
    #[test]
    fn an_unrecognised_record_stays_visible_as_human() {
        assert_eq!(
            user(json!({"type": "user", "message": {"content": "[Request interrupted by user]"}})),
            MessageOrigin::Human
        );
    }

    /// Claude's own `typed` marker beats a text shape that merely looks like
    /// markup — a human can legitimately paste an XML-ish line.
    #[test]
    fn a_declared_typed_prompt_outranks_the_text_sniffer() {
        assert_eq!(
            user(json!({
                "type": "user",
                "promptSource": "typed",
                "origin": {"kind": "human"},
                "message": {"content": "<command-name>/foo</command-name> — why does this print twice?"},
            })),
            MessageOrigin::Human
        );
    }

    /// Structural evidence outranks even a `typed` declaration: a tool result
    /// carrying `promptSource` must not become a user bubble.
    #[test]
    fn structural_tool_result_evidence_outranks_a_declaration() {
        assert_eq!(
            user(json!({
                "type": "user",
                "promptSource": "typed",
                "sourceToolUseID": "toolu_9",
                "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_9", "content": "x"}]},
            })),
            MessageOrigin::ToolResult
        );
    }

    /// Two records sharing a `message.id` are one message. Draining content
    /// yields a single record whose content holds both blocks in order, while
    /// the run (identity/counters) is retained for a possible later record.
    #[test]
    fn records_sharing_a_message_id_reassemble_into_one_message() {
        let mut group = Group::default();
        let first = json!({
            "type": "assistant",
            "message": {"id": "msg_1", "role": "assistant", "stop_reason": "tool_use",
                        "content": [{"type": "text", "text": "let me look"}]},
        });
        let second = json!({
            "type": "assistant",
            "message": {"id": "msg_1", "role": "assistant", "stop_reason": "tool_use",
                        "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}}]},
        });
        let key = GroupKey::of(&first, &first["message"]).expect("key");
        assert!(group.is_empty());
        group.start(key.clone(), first.clone());
        group.push(&first, &first["message"]);
        assert!(group.continues(&key), "the second record continues the run");
        group.push(&second, &second["message"]);
        let drained = group.drain_content().expect("drain");
        let content = drained.record["message"]["content"]
            .as_array()
            .expect("content");
        assert_eq!(content.len(), 2, "both blocks land in one message");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "tool_use");
        // Content is drained exactly once; the run itself is retained.
        assert!(group.drain_content().is_none(), "no undrained blocks left");
        assert!(!group.is_empty(), "the run identity/counters are retained");
        assert!(
            group.continues(&key),
            "a later same-id record still continues"
        );
    }

    /// When `apiBlockIndex` is present it is authoritative, because the file
    /// order it corrects is exactly the bug this reassembly exists to fix.
    #[test]
    fn api_block_index_reorders_records_that_arrived_out_of_order() {
        let mut group = Group::default();
        let late = json!({
            "type": "assistant", "apiBlockIndex": 1,
            "message": {"id": "m", "content": [{"type": "tool_use", "id": "t", "name": "Bash"}]},
        });
        let early = json!({
            "type": "assistant", "apiBlockIndex": 0,
            "message": {"id": "m", "content": [{"type": "text", "text": "first"}]},
        });
        group.start(
            GroupKey::of(&late, &late["message"]).expect("key"),
            late.clone(),
        );
        group.push(&late, &late["message"]);
        group.push(&early, &early["message"]);
        let drained = group.drain_content().expect("drain");
        let content = drained.record["message"]["content"]
            .as_array()
            .expect("content");
        assert_eq!(content[0]["type"], "text", "index 0 sorts first");
        assert_eq!(content[1]["type"], "tool_use");
    }

    /// A different `message.id` is a different message; it must not be folded
    /// into the run in progress.
    #[test]
    fn a_new_message_id_does_not_continue_the_run() {
        let first = json!({"type": "assistant", "message": {"id": "msg_1", "content": []}});
        let other = json!({"type": "assistant", "message": {"id": "msg_2", "content": []}});
        let mut group = Group::default();
        group.start(
            GroupKey::of(&first, &first["message"]).expect("key"),
            first.clone(),
        );
        let next = GroupKey::of(&other, &other["message"]).expect("key");
        assert!(!group.continues(&next));
    }

    /// One `message.id` reused across two requests is two messages. The field
    /// is absent in 2.1.221, so this guards the shape rather than today's data.
    #[test]
    fn a_differing_request_id_splits_a_reused_message_id() {
        let first = json!({"type": "assistant", "requestId": "req_a", "message": {"id": "m", "content": []}});
        let retry = json!({"type": "assistant", "requestId": "req_b", "message": {"id": "m", "content": []}});
        let mut group = Group::default();
        group.start(GroupKey::of(&first, &first["message"]).expect("key"), first);
        assert!(!group.continues(&GroupKey::of(&retry, &retry["message"]).expect("key")));
    }
}
