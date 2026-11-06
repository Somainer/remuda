//! cb-command-queue (protocol.md §7.2, 2026-11-05; §8.5): editing and
//! withdrawing a prompt this Node holds in a PTY instance's delivery queue.
//!
//! A PTY carrier keeps an accepted `instance.send` in its queue while the
//! instance is busy; until the queue claims it for the driver write, it has
//! not reached the harness. The queue registers each prompt it accepts
//! ([`HeldInputs::hold`]) and claims it right before the write
//! ([`HeldInputs::claim`]). The Hub's `command.withdraw` and `command.edit`
//! take the same lock, so exactly one of a withdrawal and a delivery wins:
//! a claimed prompt answers `already-delivered`, and a withdrawn one is never
//! written. The queue's tick applies a withdrawal or an edit to prompts still
//! waiting behind the head ([`HeldInputs::change`]).
//!
//! The registry is memory only, like the queue it mirrors: a restarted Node
//! interrupts every queued prompt, and a command it no longer holds answers
//! `already-delivered`.

use crate::{DevNode, NodeError};
use remuda_protocol::CommandId;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Mutex;

/// Hub→Node: withdraw a held prompt.
pub(crate) const METHOD_COMMAND_WITHDRAW: &str = "command.withdraw";
/// Hub→Node: replace a held prompt's text.
pub(crate) const METHOD_COMMAND_EDIT: &str = "command.edit";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// In the queue, not claimed.
    Waiting,
    /// Withdrawn; the queue drops it without writing.
    Withdrawn,
    /// Claimed for the driver write: past editing.
    Delivering,
}

#[derive(Debug, Clone)]
struct Held {
    state: State,
    /// The Hub revision this prompt carries; `None` until the first edit, when
    /// the Hub's `expectedRevision` is adopted as the starting point.
    revision: Option<u64>,
    /// Edited text the queue has not applied yet.
    text: Option<String>,
}

/// What the delivery claim decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Claim {
    /// Write it; with the edited text if an edit has not been applied yet.
    Deliver(Option<String>),
    /// It was withdrawn: never write it.
    Withdrawn,
}

/// A change the queue applies to a prompt still waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Change {
    Withdrawn,
    Edited(String),
}

/// The Node-wide registry of prompts PTY queues hold.
#[derive(Default)]
pub(crate) struct HeldInputs {
    inner: Mutex<HashMap<CommandId, Held>>,
}

impl HeldInputs {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<CommandId, Held>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A queue accepted `command_id`.
    pub(crate) fn hold(&self, command_id: &CommandId) {
        self.lock().insert(
            command_id.clone(),
            Held {
                state: State::Waiting,
                revision: None,
                text: None,
            },
        );
    }

    /// The delivery claim, right before the driver write. A prompt the
    /// registry never saw is delivered unchanged.
    pub(crate) fn claim(&self, command_id: &CommandId) -> Claim {
        let mut held = self.lock();
        match held.get_mut(command_id) {
            Some(entry) if entry.state == State::Withdrawn => {
                held.remove(command_id);
                Claim::Withdrawn
            }
            Some(entry) => {
                entry.state = State::Delivering;
                Claim::Deliver(entry.text.take())
            }
            None => Claim::Deliver(None),
        }
    }

    /// The claimed write did not happen (control went away between the
    /// claim and the driver write): the prompt is undelivered and waits
    /// again, so it can still be withdrawn or edited. An edit the claim
    /// handed over is already in the queued prompt, so nothing is restored
    /// to the entry.
    pub(crate) fn unclaim(&self, command_id: &CommandId) {
        if let Some(entry) = self.lock().get_mut(command_id)
            && entry.state == State::Delivering
        {
            entry.state = State::Waiting;
        }
    }

    /// The queue is done with `command_id` (written, failed or interrupted).
    pub(crate) fn release(&self, command_id: &CommandId) {
        self.lock().remove(command_id);
    }

    /// A withdrawal or an unapplied edit of a prompt still waiting. A
    /// withdrawn entry is removed: the queue drops the prompt now.
    pub(crate) fn change(&self, command_id: &CommandId) -> Option<Change> {
        let mut held = self.lock();
        let entry = held.get_mut(command_id)?;
        match entry.state {
            State::Withdrawn => {
                held.remove(command_id);
                Some(Change::Withdrawn)
            }
            State::Waiting => entry.text.take().map(Change::Edited),
            State::Delivering => None,
        }
    }

    /// `command.withdraw`: first-wins against [`Self::claim`].
    pub(crate) fn withdraw(&self, command_id: &CommandId) -> &'static str {
        match self.lock().get_mut(command_id) {
            Some(entry) if entry.state == State::Waiting => {
                entry.state = State::Withdrawn;
                "withdrawn"
            }
            Some(entry) if entry.state == State::Withdrawn => "withdrawn",
            _ => "already-delivered",
        }
    }

    /// `command.edit`: a revision CAS, first-wins against [`Self::claim`].
    /// Returns the outcome and the revision the prompt is now at.
    pub(crate) fn edit(
        &self,
        command_id: &CommandId,
        expected: u64,
        text: &str,
    ) -> (&'static str, u64) {
        let mut held = self.lock();
        let Some(entry) = held.get_mut(command_id) else {
            return ("already-delivered", expected);
        };
        if entry.state != State::Waiting {
            return ("already-delivered", entry.revision.unwrap_or(expected));
        }
        let current = entry.revision.unwrap_or(expected);
        if current != expected {
            return ("revision-conflict", current);
        }
        entry.revision = Some(expected + 1);
        entry.text = Some(text.to_owned());
        ("edited", expected + 1)
    }
}

fn command_id(params: &Value) -> Result<CommandId, NodeError> {
    let raw = params
        .get("commandId")
        .and_then(Value::as_str)
        .ok_or_else(|| NodeError::InvalidRequest("commandId is required".into()))?;
    Ok(CommandId::from_str(raw)?)
}

/// A U64 as a JSON integer or a decimal string.
fn revision(params: &Value) -> Result<u64, NodeError> {
    let value = params.get("expectedRevision");
    value
        .and_then(Value::as_u64)
        .or_else(|| {
            value
                .and_then(Value::as_str)
                .and_then(|raw| raw.parse().ok())
        })
        .ok_or_else(|| NodeError::InvalidRequest("expectedRevision is required".into()))
}

impl DevNode {
    /// Whether `method` is one of the held-input methods.
    pub(crate) fn is_held_input_method(method: &str) -> bool {
        matches!(method, METHOD_COMMAND_WITHDRAW | METHOD_COMMAND_EDIT)
    }

    /// `command.withdraw` / `command.edit` (protocol.md §7.2, 2026-11-05).
    pub(crate) fn held_input_rpc(&self, method: &str, params: &Value) -> Result<Value, NodeError> {
        let command_id = command_id(params)?;
        let held = &self.inner.fences.held;
        if method == METHOD_COMMAND_WITHDRAW {
            let outcome = held.withdraw(&command_id);
            tracing::info!(category = "held-input", command_id = %command_id.as_id(), outcome, "withdraw");
            return Ok(json!({ "commandId": command_id, "outcome": outcome }));
        }
        let expected = revision(params)?;
        let prompt = params
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| NodeError::InvalidRequest("command.edit requires prompt".into()))?;
        let (outcome, revision) = held.edit(&command_id, expected, prompt);
        tracing::info!(category = "held-input", command_id = %command_id.as_id(), outcome, revision, "edit");
        Ok(json!({ "commandId": command_id, "outcome": outcome, "revision": revision }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_withdrawal_and_a_delivery_claim_have_exactly_one_winner() {
        let held = HeldInputs::default();
        let first = CommandId::new();
        held.hold(&first);
        assert_eq!(held.withdraw(&first), "withdrawn");
        assert_eq!(
            held.claim(&first),
            Claim::Withdrawn,
            "withdrawn first: never written"
        );
        assert_eq!(held.withdraw(&first), "already-delivered", "and forgotten");

        let second = CommandId::new();
        held.hold(&second);
        assert_eq!(held.claim(&second), Claim::Deliver(None));
        assert_eq!(held.withdraw(&second), "already-delivered", "claimed first");
        assert_eq!(held.edit(&second, 0, "late").0, "already-delivered");

        let unknown = CommandId::new();
        assert_eq!(held.withdraw(&unknown), "already-delivered");
        assert_eq!(held.claim(&unknown), Claim::Deliver(None));
    }

    #[test]
    fn edits_compare_revisions_and_reach_the_write() {
        let held = HeldInputs::default();
        let id = CommandId::new();
        held.hold(&id);
        assert_eq!(
            held.edit(&id, 2, "one"),
            ("edited", 3),
            "the Hub's revision is adopted"
        );
        assert_eq!(held.edit(&id, 2, "stale"), ("revision-conflict", 3));
        assert_eq!(held.edit(&id, 3, "two"), ("edited", 4));
        assert_eq!(held.change(&id), Some(Change::Edited("two".into())));
        assert_eq!(held.change(&id), None, "applied once");
        assert_eq!(held.edit(&id, 4, "three"), ("edited", 5));
        assert_eq!(held.claim(&id), Claim::Deliver(Some("three".into())));
        held.release(&id);
        assert_eq!(held.withdraw(&id), "already-delivered");
    }

    #[test]
    fn an_unclaimed_prompt_can_be_withdrawn_or_edited_again() {
        let held = HeldInputs::default();
        let id = CommandId::new();
        held.hold(&id);
        assert_eq!(held.edit(&id, 0, "one"), ("edited", 1));
        assert_eq!(held.claim(&id), Claim::Deliver(Some("one".into())));
        assert_eq!(held.edit(&id, 1, "late").0, "already-delivered");
        held.unclaim(&id);
        assert_eq!(
            held.edit(&id, 1, "two"),
            ("edited", 2),
            "the revision stays"
        );
        assert_eq!(held.claim(&id), Claim::Deliver(Some("two".into())));
        held.unclaim(&id);
        assert_eq!(held.withdraw(&id), "withdrawn");
        held.unclaim(&id);
        assert_eq!(
            held.claim(&id),
            Claim::Withdrawn,
            "unclaim never revives it"
        );
        held.unclaim(&CommandId::new());
    }

    #[test]
    fn a_withdrawal_reaches_a_prompt_waiting_behind_the_head() {
        let held = HeldInputs::default();
        let id = CommandId::new();
        held.hold(&id);
        assert_eq!(held.withdraw(&id), "withdrawn");
        assert_eq!(held.withdraw(&id), "withdrawn", "idempotent");
        assert_eq!(held.change(&id), Some(Change::Withdrawn));
        assert_eq!(held.change(&id), None);
    }
}
