//! The hook wire: what the relay sends, what the Node answers.
//!
//! Field names follow Claude's own hook payload spelling (snake_case) so a
//! recorded payload can be replayed verbatim in a test. The shapes here were
//! captured from `claude` 2.1.270; see
//! `crates/remuda-testing/fixtures/hooks/claude-hook-session.jsonl`.

use serde::{Deserialize, Serialize};

/// One hook invocation, as the relay forwards it over the socket.
///
/// `ppid` is the relay's parent — the agent process itself — and is how the
/// Node binds a hook stream to the shell-pty instance whose foreground process
/// group leader is that pid. Without it a second `claude` started in a second
/// terminal would hydrate the wrong instance, because the payload's own
/// `session_id` is not yet known to anyone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookEnvelope {
    /// Per-instance credential minted by the driver; see [`crate::socket`].
    pub credential: String,
    /// `hook_event_name` as the relay was invoked with it (`SessionStart`, …).
    pub event: String,
    /// Pid of the agent process that ran this hook.
    pub ppid: i32,
    /// The hook's own JSON payload, unmodified.
    pub payload: serde_json::Value,
}

/// What the Node sends back. An empty decision is `{}` on the wire, which
/// every hook event treats as "no opinion".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HookReply {
    /// Decision body handed to the agent verbatim, or `None` for `{}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<serde_json::Value>,
}

impl HookReply {
    /// The no-opinion reply: the agent falls back to its own behaviour.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// The JSON the relay prints on stdout.
    #[must_use]
    pub fn to_hook_json(&self) -> serde_json::Value {
        self.decision
            .clone()
            .unwrap_or_else(|| serde_json::json!({}))
    }
}

/// A decoded hook event: the name we recognise plus the payload it arrived with.
#[derive(Debug, Clone, PartialEq)]
pub struct HookEvent {
    /// Recognised event name, verbatim from the harness.
    pub name: String,
    /// Agent pid (the relay's parent).
    pub ppid: i32,
    /// Original payload.
    pub payload: serde_json::Value,
}

impl HookEvent {
    /// Build from a forwarded envelope.
    #[must_use]
    pub fn from_envelope(envelope: HookEnvelope) -> Self {
        Self {
            name: envelope.event,
            ppid: envelope.ppid,
            payload: envelope.payload,
        }
    }

    /// A string field of the payload, empty and whitespace-only rejected.
    #[must_use]
    pub fn text(&self, key: &str) -> Option<&str> {
        self.payload
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    /// `session_id`, when the harness supplied one.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.text("session_id")
    }

    /// `transcript_path`, when the harness supplied one.
    #[must_use]
    pub fn transcript_path(&self) -> Option<&str> {
        self.text("transcript_path")
    }

    /// True when this event must not be read as "the agent is working".
    ///
    /// `SubagentStop` fires with no subagent at all — a recap or summary is
    /// enough (design §3.1 [V]) — so it is never turn evidence in either
    /// direction.
    #[must_use]
    pub fn is_subagent_stop(&self) -> bool {
        self.name == "SubagentStop"
    }
}

/// Every event the overlay registers.
///
/// `PermissionRequest` and `Elicitation` are answered from P5 onward: the hook
/// blocks on the socket until the interaction broker resolves, and the reply
/// is the agent's decision (see [`crate::decision`]).
pub const REGISTERED_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "StopFailure",
    "SessionEnd",
    "SubagentStart",
    "SubagentStop",
    "Notification",
    "PreToolUse",
    "PostToolUse",
    "PostToolBatch",
    "MessageDisplay",
    "PermissionRequest",
    "Elicitation",
];

/// Events whose hook blocks the agent until the Node answers.
///
/// The relay gives these the full broker-aligned wait; everything else is
/// fire-and-forget and gets a short one.
pub const BLOCKING_EVENTS: &[&str] = &["PermissionRequest", "Elicitation"];

/// True when `event` blocks the agent while the hook runs.
#[must_use]
pub fn is_blocking(event: &str) -> bool {
    BLOCKING_EVENTS.contains(&event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_decision_is_an_empty_object_on_the_wire() {
        // Claude reads `{}` as "no opinion"; anything else, including `null`,
        // risks being parsed as a decision.
        assert_eq!(HookReply::empty().to_hook_json(), serde_json::json!({}));
    }

    #[test]
    fn blank_payload_strings_read_as_absent() {
        let event = HookEvent {
            name: "SessionStart".into(),
            ppid: 1,
            payload: serde_json::json!({"session_id": "  ", "transcript_path": "/t.jsonl"}),
        };
        assert_eq!(event.session_id(), None, "whitespace is not a session id");
        assert_eq!(event.transcript_path(), Some("/t.jsonl"));
    }

    #[test]
    fn only_the_two_observe_only_events_block() {
        assert!(is_blocking("PermissionRequest"));
        assert!(is_blocking("Elicitation"));
        for event in ["SessionStart", "Stop", "MessageDisplay", "Notification"] {
            assert!(!is_blocking(event), "{event} must not block the agent");
        }
    }
}
