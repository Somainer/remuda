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
/// `stop_reason` on every one of them, so "flush at stop_reason" cannot mean
/// "flush on the first record that has one". The group is flushed when it is
/// *superseded* — a different message id, a user record, or end of input —
/// which is the only signal that actually marks the end of a run.
#[derive(Debug, Default)]
pub(crate) struct Group {
    key: Option<GroupKey>,
    /// `(apiBlockIndex, arrival order, block)`, ordered on flush.
    blocks: Vec<(Option<u64>, usize, Value)>,
    /// The record the assembled message inherits its envelope from.
    head: Option<Value>,
    seen: usize,
}

impl Group {
    /// Whether `key` continues the run currently buffered.
    pub(crate) fn continues(&self, key: &GroupKey) -> bool {
        self.key.as_ref() == Some(key)
    }

    /// Whether anything is buffered.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    /// Start a new run at `key`, carrying `record` as the envelope source.
    pub(crate) fn start(&mut self, key: GroupKey, record: Value) {
        self.key = Some(key);
        self.head = Some(record);
        self.blocks.clear();
        self.seen = 0;
    }

    /// Buffer one record's content blocks.
    pub(crate) fn push(&mut self, record: &Value, message: &Value) {
        let index = record.get("apiBlockIndex").and_then(Value::as_u64);
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            return;
        };
        for block in blocks {
            self.blocks.push((index, self.seen, block.clone()));
            self.seen += 1;
        }
    }

    /// Take the assembled record, reordered by `apiBlockIndex`.
    ///
    /// Ordering is by `(apiBlockIndex, arrival)`. Records without the field —
    /// every record in claude 2.1.221 — keep pure file order, which is the
    /// order Claude appended them in and therefore already correct.
    pub(crate) fn flush(&mut self) -> Option<Value> {
        let mut head = self.head.take()?;
        let mut blocks = std::mem::take(&mut self.blocks);
        self.key = None;
        self.seen = 0;
        blocks.sort_by_key(|(index, arrival, _)| (index.unwrap_or(0), *arrival));
        let content: Vec<Value> = blocks.into_iter().map(|(_, _, block)| block).collect();
        if let Some(message) = head.get_mut("message").and_then(Value::as_object_mut) {
            message.insert("content".into(), Value::Array(content));
        }
        Some(head)
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

    /// Two records sharing a `message.id` are one message. Flushing must yield
    /// a single record whose content holds both blocks in order.
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
        let flushed = group.flush().expect("flush");
        let content = flushed["message"]["content"].as_array().expect("content");
        assert_eq!(content.len(), 2, "both blocks land in one message");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "tool_use");
        assert!(group.is_empty(), "flushing clears the buffer");
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
        let flushed = group.flush().expect("flush");
        let content = flushed["message"]["content"].as_array().expect("content");
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
