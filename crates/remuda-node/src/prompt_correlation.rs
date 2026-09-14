//! Prompt→command correlation (workbench C2).
//!
//! A prompt can reach the harness two ways: as a Remuda command (the web
//! composer → `instance.send`, or the create-time initial prompt) or as
//! keystrokes somebody typed natively into the attached PTY. Both produce the
//! same downstream evidence — a `UserPromptSubmit` hook event and a `user`
//! record in the transcript — and before C2 that evidence had no link back to
//! the command, so the web rendered its optimistic bubble *and* a second user
//! node.
//!
//! The correlator is the single join point. When a command's prompt is
//! written, the queue registers `(commandId, nodeId, text, t0)`. Every prompt
//! observation that arrives afterwards asks the correlator whether it belongs
//! to a pending command:
//!
//! - exact text matches before whitespace-normalised text;
//! - FIFO order, so two queued commands with the same text are attributed in
//!   delivery order;
//! - each entry may match **once per channel** — the hook turn event and the
//!   transcript user record are separate observations of the same submit and
//!   must both carry the same `commandId`;
//! - a match is bounded by [`PROMPT_CORRELATION_WINDOW`]. Claude fires
//!   `UserPromptSubmit` when a prompt enters its *native* queue, which for a
//!   steer/queue can be a whole turn after Remuda wrote it, hence the generous
//!   bound;
//! - nothing with different text ever matches, and a prompt with no pending
//!   command simply gets no id — that is the "typed natively" case the web
//!   must keep rendering as an ordinary user node.
//!
//! State is in-memory on purpose: it correlates *live* observations inside
//! one Node process. After a restart the transcript pump re-hydrates history
//! without pending entries, so old records correctly stay `commandId: null` —
//! the web's optimistic bubble is in-memory too and has the same lifetime.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use remuda_protocol::{
    CommandId, ContentStatus, Id, MessageOrigin, MessagePayload, MessageRole, MutationOperation,
    Observation, ObservationPayload, SourceChannel, U64,
};

/// How long after a command was registered its prompt observations may still
/// be attributed to it. See the module note on the native queue.
pub const PROMPT_CORRELATION_WINDOW: Duration = Duration::from_secs(600);

/// Which evidence channel produced the observation. The hook fires first and
/// the transcript record lands later (polled), so each is a separate slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PromptChannel {
    /// `UserPromptSubmit` hook lifecycle observation.
    Hook,
    /// Transcript-derived (or stdout-derived) `message` observation.
    Transcript,
}

/// A command whose prompt observations have not all arrived yet.
struct PendingPrompt {
    command_id: CommandId,
    /// Node the Node-synthesised queued/delivered message was emitted with;
    /// the transcript record joins this node instead of minting another.
    node_id: Id,
    text: String,
    normalized: String,
    registered_at: Instant,
    hook_matched: bool,
    transcript_matched: bool,
}

/// Per-instance registry of prompts delivered through commands.
pub struct PromptCorrelator {
    pending: Mutex<VecDeque<PendingPrompt>>,
}

/// The command a prompt observation belongs to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PromptMatch {
    /// Delivering command whose id is stamped onto the observation.
    pub command_id: CommandId,
    /// Node id the transcript record joins instead of minting its own.
    pub node_id: Id,
}

impl Default for PromptCorrelator {
    fn default() -> Self {
        Self::new()
    }
}

impl PromptCorrelator {
    /// Empty registry.
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(VecDeque::new()),
        }
    }

    /// Register a prompt written on behalf of `command_id`.
    ///
    /// `node_id` is the Node-owned message node already journaled for the
    /// queued/delivered prompt; the matching transcript record joins it.
    pub fn register(&self, command_id: CommandId, node_id: Id, text: String) {
        let normalized = normalize_ws(&text);
        let mut pending = self.pending.lock().expect("prompt correlator");
        pending.push_back(PendingPrompt {
            command_id,
            node_id,
            text,
            normalized,
            registered_at: Instant::now(),
            hook_matched: false,
            transcript_matched: false,
        });
        prune(&mut pending);
    }

    /// Attribute one incoming prompt observation.
    ///
    /// `None` means no pending command owns this text (a natively typed
    /// prompt, or evidence arriving outside the window); the caller leaves
    /// `command_id` unset.
    pub fn match_prompt(&self, text: &str, channel: PromptChannel) -> Option<PromptMatch> {
        if text.trim().is_empty() {
            return None;
        }
        let mut pending = self.pending.lock().expect("prompt correlator");
        prune(&mut pending);
        let already = |entry: &PendingPrompt| match channel {
            PromptChannel::Hook => entry.hook_matched,
            PromptChannel::Transcript => entry.transcript_matched,
        };
        // Exact text first, FIFO. Two identical commands queue in order and
        // their observations arrive in the same order.
        let exact = pending
            .iter()
            .position(|entry| !already(entry) && entry.text == text);
        // Normalised whitespace only as a second pass; never across texts.
        let normalized = normalize_ws(text);
        let index = exact.or_else(|| {
            pending
                .iter()
                .position(|entry| !already(entry) && entry.normalized == normalized)
        })?;
        let entry = pending.get_mut(index)?;
        match channel {
            PromptChannel::Hook => entry.hook_matched = true,
            PromptChannel::Transcript => entry.transcript_matched = true,
        }
        let matched = PromptMatch {
            command_id: entry.command_id.clone(),
            node_id: entry.node_id.clone(),
        };
        if entry.hook_matched && entry.transcript_matched {
            pending.remove(index);
        }
        Some(matched)
    }

    /// A command will never produce more prompt evidence (cancelled before
    /// dispatch, or its send failed). Drop anything it registered so a prompt
    /// the human types afterwards cannot be attributed to it.
    pub fn cancel(&self, command_id: &CommandId) {
        let mut pending = self.pending.lock().expect("prompt correlator");
        pending.retain(|entry| &entry.command_id != command_id);
    }

    /// Attribute one driver-sourced observation in place.
    ///
    /// Two shapes are prompt evidence (everything else is untouched):
    ///
    /// - a hook-channel `UserPromptSubmit` lifecycle — its `prompt` related
    ///   id carries the text; the matched command id is added as `commandId`;
    /// - a human user `message` from the transcript (or stdout) channel — the
    ///   matched id is stamped **and the node joins the Node-synthesised
    ///   queued message**, replacing its content at the next revision, so the
    ///   web folds hook/queue/transcript into one user node instead of
    ///   drawing the transcript record beside the optimistic bubble.
    pub fn correlate(&self, observation: &mut Observation) {
        match &mut observation.body {
            ObservationPayload::Lifecycle(lifecycle) => {
                if observation.source.channel != SourceChannel::Hook {
                    return;
                }
                let native = match lifecycle.as_mut() {
                    remuda_protocol::LifecyclePayload::Native(native) => native,
                    remuda_protocol::LifecyclePayload::Entity(_) => return,
                };
                if native.native_name != "UserPromptSubmit" {
                    return;
                }
                let Some(prompt) = native.related_ids.get("prompt").cloned() else {
                    return;
                };
                if let Some(matched) = self.match_prompt(&prompt, PromptChannel::Hook) {
                    native.related_ids.insert(
                        "commandId".into(),
                        matched.command_id.as_id().as_str().to_owned(),
                    );
                }
            }
            ObservationPayload::Message(payload) => {
                let transcript_like = matches!(
                    observation.source.channel,
                    SourceChannel::Transcript | SourceChannel::Stdout
                );
                if !transcript_like
                    || payload.role != MessageRole::User
                    || payload.origin != Some(MessageOrigin::Human)
                {
                    return;
                }
                let text = payload
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        remuda_protocol::ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if let Some(matched) = self.match_prompt(&text, PromptChannel::Transcript) {
                    join_queued_node(payload, matched);
                }
            }
            _ => {}
        }
    }
}

/// Mutation chain the Node emits for a prompt delivered through a command:
/// revision 1 opens the node as `queued`, revision 2 replaces it as
/// complete/interrupted when the bytes are written (see `pty_queue.rs`). The
/// transcript record joins as revision 3, replacing the content with the
/// authoritative transcript value on the *same* node — one message id, one
/// rendered bubble.
const JOINED_REVISION: u64 = 3;

fn join_queued_node(payload: &mut Box<MessagePayload>, matched: PromptMatch) {
    payload.command_id = Some(matched.command_id);
    payload.mutation.operation = MutationOperation::Replace;
    payload.mutation.base_revision = Some(U64(JOINED_REVISION - 1));
    payload.mutation.revision = U64(JOINED_REVISION);
    payload.mutation.node_id = matched.node_id.clone();
    payload.message_id = matched.node_id;
    payload.status = ContentStatus::Complete;
}

/// Collapse runs of whitespace so a trailing newline the PTY paste adds does
/// not defeat an otherwise identical prompt.
fn normalize_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Drop entries outside the correlation window.
fn prune(pending: &mut VecDeque<PendingPrompt>) {
    while pending
        .front()
        .is_some_and(|entry| entry.registered_at.elapsed() > PROMPT_CORRELATION_WINDOW)
    {
        pending.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(n: u8) -> (CommandId, Id, String) {
        (
            CommandId::new(),
            Id::new("obj").unwrap(),
            format!("prompt {n}"),
        )
    }

    #[test]
    fn hook_and_transcript_for_one_submit_share_the_command_then_drop() {
        let correlator = PromptCorrelator::new();
        let (command, node, text) = entry(1);
        correlator.register(command.clone(), node.clone(), text.clone());

        let hook = correlator
            .match_prompt(&text, PromptChannel::Hook)
            .expect("hook");
        assert_eq!(hook.command_id, command);
        assert_eq!(hook.node_id, node);
        let transcript = correlator
            .match_prompt(&text, PromptChannel::Transcript)
            .expect("transcript");
        assert_eq!(transcript.command_id, command);

        // A second submit with the same text after the entry closed is native.
        assert!(
            correlator
                .match_prompt(&text, PromptChannel::Hook)
                .is_none()
        );
    }

    #[test]
    fn exact_text_wins_over_an_older_normalised_candidate() {
        let correlator = PromptCorrelator::new();
        // The older entry only matches after whitespace normalisation; the
        // newer one is byte-exact and must win despite FIFO ordering.
        let (older, _, _) = entry(1);
        correlator.register(older, Id::new("obj").unwrap(), "a  b".into());
        let (exact, exact_node, _) = entry(2);
        correlator.register(exact.clone(), exact_node.clone(), "a b".into());

        let matched = correlator
            .match_prompt("a b", PromptChannel::Transcript)
            .expect("exact match");
        assert_eq!(matched.command_id, exact);
        assert_eq!(matched.node_id, exact_node);
    }

    #[test]
    fn two_identical_commands_attribute_fifo() {
        let correlator = PromptCorrelator::new();
        let (c1, n1, text) = entry(1);
        let c2 = CommandId::new();
        correlator.register(c1.clone(), n1.clone(), text.clone());
        correlator.register(c2.clone(), Id::new("obj").unwrap(), text.clone());

        assert_eq!(
            correlator
                .match_prompt(&text, PromptChannel::Hook)
                .unwrap()
                .command_id,
            c1
        );
        assert_eq!(
            correlator
                .match_prompt(&text, PromptChannel::Hook)
                .unwrap()
                .command_id,
            c2
        );
        assert_eq!(
            correlator
                .match_prompt(&text, PromptChannel::Transcript)
                .unwrap()
                .command_id,
            c1
        );
        assert_eq!(
            correlator
                .match_prompt(&text, PromptChannel::Transcript)
                .unwrap()
                .command_id,
            c2
        );
        assert!(
            correlator
                .match_prompt(&text, PromptChannel::Transcript)
                .is_none()
        );
    }

    #[test]
    fn different_text_never_matches() {
        let correlator = PromptCorrelator::new();
        let (command, _, _) = entry(1);
        correlator.register(command, Id::new("obj").unwrap(), "real prompt".into());
        assert!(
            correlator
                .match_prompt("something else entirely", PromptChannel::Hook)
                .is_none()
        );
        assert!(
            correlator
                .match_prompt("", PromptChannel::Transcript)
                .is_none()
        );
    }

    #[test]
    fn a_prompt_with_no_pending_command_is_native() {
        let correlator = PromptCorrelator::new();
        assert!(
            correlator
                .match_prompt("typed by a human", PromptChannel::Transcript)
                .is_none()
        );
    }
}
