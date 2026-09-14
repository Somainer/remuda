//! Hook payload → [`Interaction`] entity (D-028 §4.4 tier A).
//!
//! The card a human answers is built here, from the harness's own request
//! rather than from a screen scrape. That is the whole point of tier A: a
//! `PermissionRequest` names its tool and carries its real `tool_input`, so
//! the approval shows what will actually run instead of whatever the TUI
//! happened to render (§2.2, "precise tool input summary").
//!
//! Two identities travel with the entity and they are not the same thing:
//!
//! - `meta.id` is the `InteractionId` devices answer through.
//! - `request_key.native` is [`NativeRequestKey::Hook`], holding the
//!   [`DecisionKey`](crate::pending::DecisionKey) the parked hook is filed
//!   under. Claude's `PermissionRequest` carries no `tool_use_id` of its own
//!   (measured), so this key is minted here and is the only way back to the
//!   waiting process.

use crate::decision::PermissionRequestEvent;
use crate::event::HookEvent;
use remuda_protocol::{
    ApprovalRequest, DeadlineSource, DecisionEffect, DecisionOption, DeliveryState,
    ElicitationMode, ElicitationRequest, EntityMeta, HostId, Id, InstanceId, Interaction,
    InteractionCarrier, InteractionId, InteractionKind, InteractionRequest, InteractionRequestKey,
    InteractionState, Knowledge, NativeRequestKey, RunId, Timestamp, U64,
};

/// Option id for a plain one-shot approval.
pub const ALLOW_ONCE: &str = "allow-once";
/// Option id for refusing.
pub const DENY: &str = "deny";
/// Prefix for the allow-always options built from `permission_suggestions`.
///
/// Suffixed with the suggestion's index so the answer names exactly which
/// grant the human chose, rather than "some always-allow".
pub const ALLOW_ALWAYS_PREFIX: &str = "allow-always-";

/// Identity the entity is stamped with.
#[derive(Debug, Clone)]
pub struct ApprovalContext {
    /// Owning instance.
    pub instance_id: InstanceId,
    /// Owning host.
    pub host_id: HostId,
    /// Current run.
    pub run_id: RunId,
    /// Key the waiting hook is parked under.
    pub decision_key: Id,
    /// Stamp for `created_at` / `updated_at`.
    pub now: Timestamp,
    /// Deadline the bounded wait will enforce.
    pub deadline: Timestamp,
}

/// Index of a suggestion inside an allow-always option id.
///
/// Returns `None` for any other option id, so a caller cannot read a plain
/// allow as a permission grant.
#[must_use]
pub fn allow_always_index(option_id: &str) -> Option<usize> {
    option_id.strip_prefix(ALLOW_ALWAYS_PREFIX)?.parse().ok()
}

/// Build the approval card for a `PermissionRequest`.
///
/// `blocking` is true and `answerable` is true: the agent really is parked on
/// this answer, which is what lets the UI say so honestly.
pub fn approval_interaction(
    request: &PermissionRequestEvent,
    context: &ApprovalContext,
) -> Result<Interaction, remuda_protocol::WireValueError> {
    let mut options = vec![DecisionOption {
        id: ALLOW_ONCE.into(),
        label: "Allow once".into(),
        effect: DecisionEffect::AllowOnce,
        native_value_ref: Id::new("obj")?,
    }];
    // One button per suggestion the harness offered. Built from the payload
    // rather than hardcoded, so a harness that offers nothing shows no
    // always-allow button and one that offers two shows two.
    for (index, suggestion) in request.suggestions.iter().enumerate() {
        options.push(DecisionOption {
            id: format!("{ALLOW_ALWAYS_PREFIX}{index}"),
            label: suggestion.label(),
            effect: DecisionEffect::AllowSession,
            native_value_ref: Id::new("obj")?,
        });
    }
    options.push(DecisionOption {
        id: DENY.into(),
        label: "Deny".into(),
        effect: DecisionEffect::Deny,
        native_value_ref: Id::new("obj")?,
    });
    Ok(Interaction {
        meta: EntityMeta {
            id: InteractionId::new(),
            revision: U64(1),
            created_at: context.now.clone(),
            updated_at: context.now.clone(),
        },
        instance_id: context.instance_id.clone(),
        run_id: Some(context.run_id.clone()),
        host_id: context.host_id.clone(),
        kind: InteractionKind::Approval,
        request_key: InteractionRequestKey {
            native: NativeRequestKey::Hook {
                invocation_id: context.decision_key.clone(),
            },
            process_generation: U64(1),
            run_generation: Some(U64(1)),
            connection_epoch: Id::new("epoch")?,
        },
        request_version: U64(1),
        state: InteractionState::Pending,
        // The agent is genuinely parked on the socket waiting for this.
        blocking: true,
        answerable: true,
        carrier: InteractionCarrier::HarnessHook,
        request: InteractionRequest::Approval(Box::new(ApprovalRequest {
            title: request.tool_name.clone(),
            // The real tool input, not a screen scrape of it.
            description: request.input_summary(),
            tool_call_id: None,
            action_ref: Id::new("obj")?,
            options,
            requested_permissions_ref: None,
            input_digest: digest_of(&request.tool_input)?,
        })),
        deadline: Knowledge::Known {
            value: context.deadline.clone(),
        },
        deadline_source: DeadlineSource::RuntimePolicy,
        answer: Knowledge::Unknown {
            reason: "pending".into(),
            evidence_event_ids: Vec::new(),
        },
        delivery: DeliveryState::NotSent,
        resolution: Knowledge::Unknown {
            reason: "pending".into(),
            evidence_event_ids: Vec::new(),
        },
    })
}

/// Build the card for an `Elicitation` (MCP form / url).
///
/// Mode is read from the payload: a `url` elicitation is a link to open, a
/// form is a schema to fill. All three actions are allowed, because declining
/// and cancelling are different answers to the server.
pub fn elicitation_interaction(
    event: &HookEvent,
    context: &ApprovalContext,
) -> Result<Interaction, remuda_protocol::WireValueError> {
    let url = event.text("url").map(ToOwned::to_owned);
    let mode = if url.is_some() {
        ElicitationMode::Url
    } else {
        ElicitationMode::Form
    };
    let title = event
        .text("message")
        .or_else(|| event.text("title"))
        .unwrap_or("The agent needs some information")
        .to_owned();
    Ok(Interaction {
        meta: EntityMeta {
            id: InteractionId::new(),
            revision: U64(1),
            created_at: context.now.clone(),
            updated_at: context.now.clone(),
        },
        instance_id: context.instance_id.clone(),
        run_id: Some(context.run_id.clone()),
        host_id: context.host_id.clone(),
        kind: InteractionKind::Elicitation,
        request_key: InteractionRequestKey {
            native: NativeRequestKey::Hook {
                invocation_id: context.decision_key.clone(),
            },
            process_generation: U64(1),
            run_generation: Some(U64(1)),
            connection_epoch: Id::new("epoch")?,
        },
        request_version: U64(1),
        state: InteractionState::Pending,
        blocking: true,
        answerable: true,
        carrier: InteractionCarrier::HarnessHook,
        request: InteractionRequest::Elicitation(Box::new(ElicitationRequest {
            title,
            mode,
            schema_ref: None,
            schema_dialect: event
                .payload
                .get("schema")
                .is_some()
                .then(|| "https://json-schema.org/draft/2020-12/schema".to_owned()),
            url,
            native_extension: None,
            allowed_actions: vec![
                remuda_protocol::ElicitationAction::Accept,
                remuda_protocol::ElicitationAction::Decline,
                remuda_protocol::ElicitationAction::Cancel,
            ],
        })),
        deadline: Knowledge::Known {
            value: context.deadline.clone(),
        },
        deadline_source: DeadlineSource::RuntimePolicy,
        answer: Knowledge::Unknown {
            reason: "pending".into(),
            evidence_event_ids: Vec::new(),
        },
        delivery: DeliveryState::NotSent,
        resolution: Knowledge::Unknown {
            reason: "pending".into(),
            evidence_event_ids: Vec::new(),
        },
    })
}

/// Digest of the tool input the card was built from.
///
/// The broker checks the answer's digest against this, so an approval cannot
/// be replayed onto a request whose input has changed underneath it.
fn digest_of(
    input: &serde_json::Value,
) -> Result<remuda_protocol::Digest, remuda_protocol::WireValueError> {
    use sha2::{Digest as _, Sha256};
    let bytes = serde_json::to_vec(input).unwrap_or_default();
    let hash = Sha256::digest(&bytes);
    remuda_protocol::Digest::try_from(format!("sha256:{hash:x}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::HookEvent;

    fn context() -> ApprovalContext {
        ApprovalContext {
            instance_id: InstanceId::new(),
            host_id: HostId::new(),
            run_id: RunId::new(),
            decision_key: Id::new("hook").unwrap(),
            now: Timestamp::try_from("2026-09-14T00:00:00.000Z".to_owned()).unwrap(),
            deadline: Timestamp::try_from("2026-09-14T00:15:00.000Z".to_owned()).unwrap(),
        }
    }

    fn request(payload: serde_json::Value) -> PermissionRequestEvent {
        PermissionRequestEvent::from_event(&HookEvent {
            name: "PermissionRequest".into(),
            ppid: 4242,
            payload,
        })
        .expect("a permission request")
    }

    fn recorded() -> PermissionRequestEvent {
        request(serde_json::json!({
            "tool_name": "Write",
            "tool_input": {"file_path": "/tmp/probe.txt", "content": "hello"},
            "permission_suggestions": [
                {"type": "setMode", "mode": "acceptEdits", "destination": "session"}
            ],
        }))
    }

    #[test]
    fn the_card_shows_the_real_tool_input_and_is_carried_by_the_hook() {
        let interaction = approval_interaction(&recorded(), &context()).unwrap();
        assert_eq!(interaction.carrier, InteractionCarrier::HarnessHook);
        assert_eq!(interaction.kind, InteractionKind::Approval);
        let InteractionRequest::Approval(approval) = &interaction.request else {
            panic!("expected an approval");
        };
        assert_eq!(approval.title, "Write");
        // The precise input §2.2 asks for, not a screen scrape.
        assert_eq!(approval.description, "/tmp/probe.txt");
    }

    #[test]
    fn the_agent_is_parked_so_the_card_says_blocking_and_answerable() {
        let interaction = approval_interaction(&recorded(), &context()).unwrap();
        assert!(interaction.blocking, "the hook really is waiting");
        assert!(interaction.answerable);
        assert_eq!(interaction.state, InteractionState::Pending);
    }

    #[test]
    fn the_decision_key_travels_as_the_native_request_key() {
        // It is the only route back to the parked process: the payload has no
        // tool_use_id of its own.
        let context = context();
        let interaction = approval_interaction(&recorded(), &context).unwrap();
        assert_eq!(
            interaction.request_key.native,
            NativeRequestKey::Hook {
                invocation_id: context.decision_key.clone()
            }
        );
    }

    #[test]
    fn a_suggestion_becomes_an_allow_always_button() {
        let interaction = approval_interaction(&recorded(), &context()).unwrap();
        let InteractionRequest::Approval(approval) = &interaction.request else {
            panic!("expected an approval");
        };
        let ids: Vec<_> = approval.options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, vec!["allow-once", "allow-always-0", "deny"]);
        let always = &approval.options[1];
        assert_eq!(always.label, "Always allow (acceptEdits)");
        assert_eq!(always.effect, DecisionEffect::AllowSession);
    }

    #[test]
    fn no_suggestions_means_no_always_allow_button() {
        // Offering a grant the harness never proposed would be inventing an
        // option we cannot honour.
        let interaction = approval_interaction(
            &request(serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "ls"}})),
            &context(),
        )
        .unwrap();
        let InteractionRequest::Approval(approval) = &interaction.request else {
            panic!("expected an approval");
        };
        let ids: Vec<_> = approval.options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, vec!["allow-once", "deny"]);
    }

    #[test]
    fn every_suggestion_gets_its_own_button() {
        let interaction = approval_interaction(
            &request(serde_json::json!({
                "tool_name": "Bash",
                "tool_input": {"command": "ls"},
                "permission_suggestions": [
                    {"type": "setMode", "mode": "acceptEdits", "destination": "session"},
                    {"type": "addRules", "rules": [{"toolName": "Bash"}]},
                ],
            })),
            &context(),
        )
        .unwrap();
        let InteractionRequest::Approval(approval) = &interaction.request else {
            panic!("expected an approval");
        };
        assert_eq!(approval.options.len(), 4);
        assert_eq!(approval.options[2].label, "Always allow this rule");
    }

    #[test]
    fn an_allow_always_option_names_which_suggestion_it_grants() {
        assert_eq!(allow_always_index("allow-always-0"), Some(0));
        assert_eq!(allow_always_index("allow-always-2"), Some(2));
        // A plain allow must never read as a permission grant.
        assert_eq!(allow_always_index("allow-once"), None);
        assert_eq!(allow_always_index("deny"), None);
        assert_eq!(allow_always_index("allow-always-x"), None);
    }

    #[test]
    fn the_digest_covers_the_input_so_a_stale_answer_cannot_be_replayed() {
        let first = approval_interaction(&recorded(), &context()).unwrap();
        let other = approval_interaction(
            &request(serde_json::json!({
                "tool_name": "Write",
                "tool_input": {"file_path": "/tmp/other.txt", "content": "hello"},
            })),
            &context(),
        )
        .unwrap();
        let digest = |interaction: &Interaction| {
            let InteractionRequest::Approval(approval) = &interaction.request else {
                panic!("expected an approval");
            };
            approval.input_digest.clone()
        };
        assert_ne!(digest(&first), digest(&other));
        // Same input, same digest — the answer has to match what was shown.
        assert_eq!(
            digest(&first),
            digest(&approval_interaction(&recorded(), &context()).unwrap())
        );
    }

    #[test]
    fn the_card_carries_the_deadline_the_wait_will_enforce() {
        // The UI needs it to say how long the agent will hold.
        let context = context();
        let interaction = approval_interaction(&recorded(), &context).unwrap();
        assert_eq!(
            interaction.deadline,
            Knowledge::Known {
                value: context.deadline.clone()
            }
        );
        assert_eq!(interaction.deadline_source, DeadlineSource::RuntimePolicy);
    }

    fn elicitation(payload: serde_json::Value) -> Interaction {
        elicitation_interaction(
            &HookEvent {
                name: "Elicitation".into(),
                ppid: 4242,
                payload,
            },
            &context(),
        )
        .unwrap()
    }

    #[test]
    fn a_form_elicitation_allows_all_three_actions() {
        // Decline and cancel are different answers to the MCP server, so the
        // card must offer both rather than collapsing them.
        let interaction = elicitation(serde_json::json!({
            "message": "Which account?",
            "schema": {"type": "object"},
        }));
        assert_eq!(interaction.kind, InteractionKind::Elicitation);
        assert_eq!(interaction.carrier, InteractionCarrier::HarnessHook);
        let InteractionRequest::Elicitation(request) = &interaction.request else {
            panic!("expected an elicitation");
        };
        assert_eq!(request.mode, ElicitationMode::Form);
        assert_eq!(request.title, "Which account?");
        assert_eq!(request.allowed_actions.len(), 3);
    }

    #[test]
    fn a_url_elicitation_keeps_its_link() {
        let interaction = elicitation(serde_json::json!({
            "message": "Authorise access",
            "url": "https://example.test/authorise",
        }));
        let InteractionRequest::Elicitation(request) = &interaction.request else {
            panic!("expected an elicitation");
        };
        assert_eq!(request.mode, ElicitationMode::Url);
        assert_eq!(
            request.url.as_deref(),
            Some("https://example.test/authorise")
        );
    }

    #[test]
    fn an_elicitation_without_a_message_still_says_something() {
        let interaction = elicitation(serde_json::json!({}));
        let InteractionRequest::Elicitation(request) = &interaction.request else {
            panic!("expected an elicitation");
        };
        assert!(!request.title.trim().is_empty());
    }
}
