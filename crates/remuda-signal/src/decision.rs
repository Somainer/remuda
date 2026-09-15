//! Blocking hook decisions: what the Node sends back to a waiting agent
//! (D-028 §4.4 tier A, §14 risk 1).
//!
//! Two events block the agent while their hook runs — `PermissionRequest` and
//! `Elicitation` — and for those the reply on the socket *is* the answer. This
//! module owns the exact JSON that answer has to be, and the reading of the
//! request that produced it.
//!
//! # The shape, and why it is not the one the design doc predicted
//!
//! D-028 §3.1 records that `PermissionRequest` takes `{"behavior": …}` and that
//! `permissionDecision` (the `PreToolUse` key) is silently ignored. The second
//! half is right and the first half is incomplete. Measured against the
//! installed `claude` 2.1.221 ([`evidence/native-pty-5.md`]):
//!
//! | reply | result |
//! |---|---|
//! | `{"behavior":"allow"}` | **ignored** — the dialog still rendered and the tool was refused |
//! | `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}` | applied — "Allowed by PermissionRequest hook", no dialog |
//!
//! The binary's own reader explains it: the generic hook-output handler
//! switches on `hookSpecificOutput.hookEventName`, and only the
//! `PermissionRequest` arm assigns `permissionRequestResult`. A bare
//! `behavior` key never reaches that arm. So [`HookDecision::to_hook_json`]
//! emits the nested form — the one that was measured to land — and
//! [`decision_behavior`] *reads* both, because a reply we did not write (a
//! user's own hook, a future version) is still worth understanding.
//!
//! Emitting the shape that is silently dropped is precisely the §14 risk 1
//! failure: the human presses Approve, Remuda reports success, and the agent
//! sits on its own dialog having never heard. That is why the wrong shape is a
//! test here rather than a comment.
//!
//! [`evidence/native-pty-5.md`]: ../../../docs/design/evidence/native-pty-5.md

use crate::event::HookEvent;
use serde::{Deserialize, Serialize};

/// A permission suggestion the harness offered alongside the request.
///
/// Claude sends these in `permission_suggestions`; the measured shape for a
/// `Write` was `{"type":"setMode","mode":"acceptEdits","destination":"session"}`.
/// Remuda does not interpret the contents: an "allow always" answer hands the
/// suggestion straight back in `updatedPermissions`, so a suggestion Remuda
/// does not understand still works, and a future suggestion kind needs no
/// code change. The typed fields exist only to label the button.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionSuggestion {
    /// Verbatim suggestion, handed back unmodified on allow-always.
    #[serde(flatten)]
    pub value: serde_json::Value,
}

impl PermissionSuggestion {
    /// A short human label for the button, best-effort.
    ///
    /// Never fails: an unrecognised suggestion still gets a generic label
    /// rather than being hidden, because hiding it would silently drop an
    /// option the harness offered.
    #[must_use]
    pub fn label(&self) -> String {
        let text = |key: &str| {
            self.value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        };
        match (text("type"), text("mode")) {
            (Some("setMode"), Some(mode)) => format!("Always allow ({mode})"),
            (Some("addRules"), _) => "Always allow this rule".to_owned(),
            (Some(kind), _) => format!("Always allow ({kind})"),
            (None, _) => "Always allow".to_owned(),
        }
    }
}

/// What the Node decided for a blocking event.
///
/// Serialised by [`to_hook_json`](Self::to_hook_json), which is the only place
/// the wire spelling lives.
#[derive(Debug, Clone, PartialEq)]
pub enum HookDecision {
    /// Let the tool call proceed.
    Allow {
        /// Replacement tool input, when the human edited it. `None` keeps the
        /// model's own input.
        updated_input: Option<serde_json::Value>,
        /// Permission grants to persist, echoed from `permission_suggestions`.
        ///
        /// Empty is "allow once". Non-empty is the allow-always answer, and
        /// was measured to suppress the *next* request for the same tool
        /// entirely (the mode line flipped to "accept edits on").
        updated_permissions: Vec<serde_json::Value>,
    },
    /// Refuse the tool call, with a reason the model sees.
    Deny {
        /// Shown to the model, and on screen as `Error: <message>`.
        message: String,
    },
    /// Answer an `Elicitation` (MCP form / url).
    Elicitation {
        /// `accept` / `decline` / `cancel`.
        action: ElicitationAction,
        /// Form content for `accept`.
        content: Option<serde_json::Value>,
    },
}

/// The three actions an `Elicitation` accepts.
///
/// Spelled as the harness spells them; mirrors
/// `remuda_protocol::ElicitationAction` without making this crate depend on
/// the answer types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ElicitationAction {
    /// The human filled the form in.
    Accept,
    /// The human refused.
    Decline,
    /// Dismissed without an answer; also what a timeout sends.
    Cancel,
}

impl ElicitationAction {
    /// Wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Decline => "decline",
            Self::Cancel => "cancel",
        }
    }
}

impl HookDecision {
    /// The deny a bounded wait falls back to (§4.4: "timeout is always deny").
    ///
    /// A blocking hook that is never answered must not become an allow by
    /// default, and must not hang either: the agent would sit with a dialog
    /// nobody is looking at.
    #[must_use]
    pub fn timed_out() -> Self {
        Self::Deny {
            message: "Remuda: no decision was made before the approval deadline".to_owned(),
        }
    }

    /// The JSON the relay prints for `event`.
    ///
    /// The event name is required because the harness matches
    /// `hookSpecificOutput.hookEventName` against the event it invoked: the
    /// measured error for a mismatch is
    /// `Hook returned incorrect event name: expected 'PreToolUse' but got
    /// 'PermissionRequest'`, and the decision is dropped.
    #[must_use]
    pub fn to_hook_json(&self, event: &str) -> serde_json::Value {
        let mut specific = serde_json::Map::new();
        specific.insert("hookEventName".into(), serde_json::json!(event));
        match self {
            Self::Allow {
                updated_input,
                updated_permissions,
            } => {
                let mut decision = serde_json::Map::new();
                decision.insert("behavior".into(), serde_json::json!("allow"));
                if let Some(input) = updated_input {
                    decision.insert("updatedInput".into(), input.clone());
                }
                if !updated_permissions.is_empty() {
                    decision.insert(
                        "updatedPermissions".into(),
                        serde_json::json!(updated_permissions),
                    );
                }
                specific.insert("decision".into(), serde_json::Value::Object(decision));
            }
            Self::Deny { message } => {
                specific.insert(
                    "decision".into(),
                    serde_json::json!({"behavior": "deny", "message": message}),
                );
            }
            Self::Elicitation { action, content } => {
                specific.insert("action".into(), serde_json::json!(action.as_str()));
                if let Some(content) = content {
                    specific.insert("content".into(), content.clone());
                }
            }
        }
        serde_json::json!({ "hookSpecificOutput": serde_json::Value::Object(specific) })
    }
}

/// Read the behaviour out of a decision reply, in either spelling.
///
/// Accepts the nested form we emit, the bare `behavior` form the design doc
/// predicted, and the `permissionDecision` spelling `PreToolUse` uses — not
/// because we send them, but because reading a reply is a different job from
/// writing one, and a reply from someone else's hook is still information.
#[must_use]
pub fn decision_behavior(reply: &serde_json::Value) -> Option<&str> {
    let nested = reply
        .get("hookSpecificOutput")
        .and_then(|output| output.get("decision"))
        .and_then(|decision| decision.get("behavior"));
    nested
        .or_else(|| reply.get("behavior"))
        .or_else(|| reply.get("permissionDecision"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// A `PermissionRequest` payload, read into the fields an approval card needs.
///
/// Field spellings are claude's own, captured live (§ evidence native-pty-5).
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequestEvent {
    /// Tool the model wants to run, e.g. `Write`.
    pub tool_name: String,
    /// The model's own tool input, verbatim. This is the *real* input the
    /// design doc asks the card to show, not a screen scrape of it.
    pub tool_input: serde_json::Value,
    /// Suggestions the harness offered, if any. Drives the allow-always button.
    pub suggestions: Vec<PermissionSuggestion>,
    /// `tool_use_id`, when the harness sent one.
    ///
    /// Measured absent on claude 2.1.221's `PermissionRequest` (the sibling
    /// `PreToolUse` for the same call *does* carry it), so nothing may key on
    /// it being present.
    pub tool_use_id: Option<String>,
    /// Permission mode at request time (`default`, `acceptEdits`, …).
    pub permission_mode: Option<String>,
}

impl PermissionRequestEvent {
    /// Read a `PermissionRequest` hook payload.
    ///
    /// Returns `None` for any other event, so a caller cannot accidentally
    /// build an approval card out of a `Notification`.
    #[must_use]
    pub fn from_event(event: &HookEvent) -> Option<Self> {
        if event.name != "PermissionRequest" {
            return None;
        }
        Some(Self {
            // A request with no tool name is still a request; refusing to
            // build a card would leave the agent blocked with nothing shown.
            tool_name: event.text("tool_name").unwrap_or("tool").to_owned(),
            tool_input: event
                .payload
                .get("tool_input")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            suggestions: event
                .payload
                .get("permission_suggestions")
                .and_then(serde_json::Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .cloned()
                        .map(|value| PermissionSuggestion { value })
                        .collect()
                })
                .unwrap_or_default(),
            tool_use_id: event.text("tool_use_id").map(ToOwned::to_owned),
            permission_mode: event.text("permission_mode").map(ToOwned::to_owned),
        })
    }

    /// One-line summary of the tool input for a card title line.
    ///
    /// Prefers the fields a human reads first — a command, a file path — and
    /// falls back to compact JSON. Never the empty string: a card with a blank
    /// body asks somebody to approve something they cannot see.
    #[must_use]
    pub fn input_summary(&self) -> String {
        let text = |key: &str| {
            self.tool_input
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        };
        if let Some(command) = text("command") {
            return truncate(command);
        }
        if let Some(path) = text("file_path").or_else(|| text("path")) {
            return truncate(path);
        }
        if let Some(url) = text("url") {
            return truncate(url);
        }
        if self.tool_input.is_null() {
            return self.tool_name.clone();
        }
        truncate(
            &serde_json::to_string(&self.tool_input).unwrap_or_else(|_| self.tool_name.clone()),
        )
    }
}

/// Longest summary kept, in characters.
const SUMMARY_CHARS: usize = 400;

fn truncate(value: &str) -> String {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= SUMMARY_CHARS {
        return collapsed;
    }
    let head: String = collapsed.chars().take(SUMMARY_CHARS).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &str, payload: serde_json::Value) -> HookEvent {
        HookEvent {
            name: name.into(),
            ppid: 4242,
            payload,
        }
    }

    /// The live payload from claude 2.1.221, verbatim (evidence native-pty-5).
    fn recorded_request() -> HookEvent {
        event(
            "PermissionRequest",
            serde_json::json!({
                "session_id": "fb571289-9767-4bf2-9354-131360aff059",
                "transcript_path": "/tmp/w/s.jsonl",
                "cwd": "/tmp/remuda-p5/p2",
                "prompt_id": "bef8e47f-8c22-4eca-b8f2-139d6ea37195",
                "permission_mode": "default",
                "effort": {"level": "xhigh"},
                "hook_event_name": "PermissionRequest",
                "tool_name": "Write",
                "tool_input": {"file_path": "/tmp/remuda-p5/p2/probe.txt", "content": "hello"},
                "permission_suggestions": [
                    {"type": "setMode", "mode": "acceptEdits", "destination": "session"}
                ],
            }),
        )
    }

    #[test]
    fn allow_is_emitted_in_the_nested_shape_the_harness_actually_reads() {
        // Measured: the bare `{"behavior":"allow"}` form is silently ignored —
        // the dialog stays up and the tool is refused. Only this shape applied.
        let json = HookDecision::Allow {
            updated_input: None,
            updated_permissions: Vec::new(),
        }
        .to_hook_json("PermissionRequest");
        assert_eq!(
            json,
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": {"behavior": "allow"},
                }
            })
        );
    }

    #[test]
    fn a_decision_never_uses_the_key_that_is_silently_ignored() {
        // `permissionDecision` is the PreToolUse key; on PermissionRequest it
        // is dropped without a word, which is the worst possible failure for
        // an approval: the human is told it worked.
        for decision in [
            HookDecision::Allow {
                updated_input: None,
                updated_permissions: Vec::new(),
            },
            HookDecision::Deny {
                message: "no".into(),
            },
        ] {
            let text = decision.to_hook_json("PermissionRequest").to_string();
            assert!(!text.contains("permissionDecision"), "{text}");
            // The bare top-level `behavior` is the shape measured to be
            // dropped; it must never be the whole reply.
            let json = decision.to_hook_json("PermissionRequest");
            assert!(
                json.get("behavior").is_none(),
                "a top-level behavior key is the ignored shape: {json}"
            );
        }
    }

    #[test]
    fn the_event_name_is_echoed_because_a_mismatch_drops_the_decision() {
        // Measured error when it disagrees: "Hook returned incorrect event
        // name: expected 'PreToolUse' but got 'PermissionRequest'".
        let json = HookDecision::Deny {
            message: "no".into(),
        }
        .to_hook_json("Elicitation");
        assert_eq!(json["hookSpecificOutput"]["hookEventName"], "Elicitation");
    }

    #[test]
    fn deny_carries_the_reason_the_model_and_the_screen_both_show() {
        let json = HookDecision::Deny {
            message: "denied by remuda p5 test".into(),
        }
        .to_hook_json("PermissionRequest");
        assert_eq!(
            json["hookSpecificOutput"]["decision"],
            serde_json::json!({"behavior": "deny", "message": "denied by remuda p5 test"})
        );
    }

    #[test]
    fn allow_always_hands_the_suggestion_back_verbatim() {
        // Measured: with this echoed back, the *next* Write fired no
        // PermissionRequest at all and the mode line flipped to accept-edits.
        // Echoing verbatim is what makes an unrecognised suggestion still work.
        let suggestion = serde_json::json!({
            "type": "setMode", "mode": "acceptEdits", "destination": "session"
        });
        let json = HookDecision::Allow {
            updated_input: None,
            updated_permissions: vec![suggestion.clone()],
        }
        .to_hook_json("PermissionRequest");
        assert_eq!(
            json["hookSpecificOutput"]["decision"]["updatedPermissions"],
            serde_json::json!([suggestion])
        );
    }

    #[test]
    fn allow_once_sends_no_permission_grant_at_all() {
        // An empty array would still be a grant-shaped key; absence is what
        // "just this once" means.
        let json = HookDecision::Allow {
            updated_input: None,
            updated_permissions: Vec::new(),
        }
        .to_hook_json("PermissionRequest");
        assert!(
            json["hookSpecificOutput"]["decision"]
                .get("updatedPermissions")
                .is_none()
        );
    }

    #[test]
    fn a_timeout_denies_rather_than_allowing_or_hanging() {
        // §4.4: the bounded wait exists so an unanswered approval fails
        // closed. An allow-by-default here would run tools nobody approved.
        let HookDecision::Deny { message } = HookDecision::timed_out() else {
            panic!("a timeout must deny");
        };
        assert!(message.contains("deadline"), "{message}");
    }

    #[test]
    fn an_elicitation_answers_with_an_action_and_its_content() {
        let json = HookDecision::Elicitation {
            action: ElicitationAction::Accept,
            content: Some(serde_json::json!({"name": "ada"})),
        }
        .to_hook_json("Elicitation");
        assert_eq!(
            json,
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "Elicitation",
                    "action": "accept",
                    "content": {"name": "ada"},
                }
            })
        );
    }

    #[test]
    fn a_declined_elicitation_sends_no_content() {
        let json = HookDecision::Elicitation {
            action: ElicitationAction::Decline,
            content: None,
        }
        .to_hook_json("Elicitation");
        assert_eq!(json["hookSpecificOutput"]["action"], "decline");
        assert!(json["hookSpecificOutput"].get("content").is_none());
    }

    #[test]
    fn both_spellings_of_a_reply_can_be_read_back() {
        // We only ever emit the nested one, but a user's own hook may answer
        // in either, and understanding it is free.
        assert_eq!(
            decision_behavior(&serde_json::json!({
                "hookSpecificOutput": {"decision": {"behavior": "allow"}}
            })),
            Some("allow")
        );
        assert_eq!(
            decision_behavior(&serde_json::json!({"behavior": "deny"})),
            Some("deny")
        );
        assert_eq!(
            decision_behavior(&serde_json::json!({"permissionDecision": "allow"})),
            Some("allow")
        );
        assert_eq!(decision_behavior(&serde_json::json!({})), None);
    }

    #[test]
    fn our_own_allow_reads_back_as_an_allow() {
        // Round-trip: the writer and the reader must agree, or the fallback
        // logic would see its own applied decision as unapplied.
        let json = HookDecision::Allow {
            updated_input: None,
            updated_permissions: Vec::new(),
        }
        .to_hook_json("PermissionRequest");
        assert_eq!(decision_behavior(&json), Some("allow"));
    }

    #[test]
    fn the_recorded_request_yields_the_real_tool_input_not_a_screen_scrape() {
        let request = PermissionRequestEvent::from_event(&recorded_request()).expect("a request");
        assert_eq!(request.tool_name, "Write");
        assert_eq!(
            request.tool_input,
            serde_json::json!({"file_path": "/tmp/remuda-p5/p2/probe.txt", "content": "hello"})
        );
        assert_eq!(request.permission_mode.as_deref(), Some("default"));
        assert_eq!(request.suggestions.len(), 1);
        assert_eq!(request.suggestions[0].label(), "Always allow (acceptEdits)");
    }

    #[test]
    fn the_recorded_request_has_no_tool_use_id_so_nothing_may_require_one() {
        // The sibling PreToolUse for the same call does carry one; the
        // PermissionRequest does not. Keying on it would drop every approval.
        let request = PermissionRequestEvent::from_event(&recorded_request()).expect("a request");
        assert_eq!(request.tool_use_id, None);
    }

    #[test]
    fn only_a_permission_request_becomes_an_approval() {
        for name in ["Notification", "PreToolUse", "Elicitation", "Stop"] {
            assert!(
                PermissionRequestEvent::from_event(&event(name, serde_json::json!({}))).is_none(),
                "{name} must not become an approval card"
            );
        }
    }

    #[test]
    fn the_summary_prefers_what_a_human_reads_first() {
        let summary = |input: serde_json::Value| {
            PermissionRequestEvent::from_event(&event(
                "PermissionRequest",
                serde_json::json!({"tool_name": "T", "tool_input": input}),
            ))
            .expect("request")
            .input_summary()
        };
        assert_eq!(
            summary(serde_json::json!({"command": "rm -rf /tmp/x"})),
            "rm -rf /tmp/x"
        );
        assert_eq!(
            summary(serde_json::json!({"file_path": "/w/a.txt"})),
            "/w/a.txt"
        );
        // Anything else is still shown, as compact JSON, rather than blank.
        assert_eq!(summary(serde_json::json!({"q": 1})), "{\"q\":1}");
    }

    #[test]
    fn a_summary_is_never_blank_and_never_unbounded() {
        // Blank asks a human to approve something invisible; unbounded lets a
        // tool input set the size of every card and journal row.
        let empty = PermissionRequestEvent::from_event(&event(
            "PermissionRequest",
            serde_json::json!({"tool_name": "Write"}),
        ))
        .expect("request");
        assert_eq!(empty.input_summary(), "Write");
        let huge = PermissionRequestEvent::from_event(&event(
            "PermissionRequest",
            serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "x".repeat(5000)}}),
        ))
        .expect("request");
        assert!(huge.input_summary().chars().count() <= SUMMARY_CHARS + 1);
    }

    #[test]
    fn a_multiline_command_is_summarised_on_one_line() {
        // The summary shares a line with the tool name in the UI; a raw
        // newline there breaks the card layout and the journal row.
        let request = PermissionRequestEvent::from_event(&event(
            "PermissionRequest",
            serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "a\n  b\n  c"}}),
        ))
        .expect("request");
        assert_eq!(request.input_summary(), "a b c");
    }

    #[test]
    fn an_unrecognised_suggestion_is_still_offered() {
        // Hiding it would silently drop an option the harness offered.
        let suggestion = PermissionSuggestion {
            value: serde_json::json!({"type": "somethingNew"}),
        };
        assert_eq!(suggestion.label(), "Always allow (somethingNew)");
        let bare = PermissionSuggestion {
            value: serde_json::json!({}),
        };
        assert_eq!(bare.label(), "Always allow");
    }
}
