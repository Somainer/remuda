//! Interaction ↔ card mapping, 10–15 min expiry, first-writer-wins.

use remuda_feishu::{
    AnswerScope, CardAction, ShortcutMiss, ShortcutRef, TicketState, TicketStore, admit,
    parse_event_line, render_interaction_card, session_chat,
};
use remuda_protocol::{Interaction, InteractionAnswer};
use std::time::{Duration, SystemTime};

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

fn load_interaction(name: &str) -> Interaction {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    remuda_protocol::from_json_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn load_card_action(name: &str) -> CardAction {
    let text = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap();
    let line = text
        .lines()
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap();
    match parse_event_line(line).unwrap() {
        remuda_feishu::RawEvent::CardAction(action) => action,
        _ => panic!("card fixture"),
    }
}

#[test]
fn ttl_must_be_ten_to_fifteen_minutes() {
    assert!(TicketStore::new(Duration::from_secs(9 * 60)).is_err());
    assert!(TicketStore::new(Duration::from_secs(16 * 60)).is_err());
    assert!(TicketStore::new(Duration::from_secs(10 * 60)).is_ok());
    assert!(TicketStore::new(Duration::from_secs(15 * 60)).is_ok());
    assert_eq!(
        TicketStore::with_default_ttl().ttl(),
        Duration::from_secs(12 * 60)
    );
}

#[test]
fn approval_allow_maps_to_protocol_answer() {
    let interaction = load_interaction("interaction-approval.json");
    let mut store = TicketStore::with_default_ttl();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let (ticket, card) = store
        .issue(&interaction, "feishu:oc_p2p:main", now)
        .unwrap();
    remuda_feishu::validate_card(&card).unwrap();
    let scope = AnswerScope::Chat("oc_p2p");
    let mut action = load_card_action("card-action-allow.jsonl");
    action.action_value = Some(serde_json::json!({
        "tid": ticket.ticket_id,
        "a": "allow"
    }));
    let mapped = store.answer_card(&action, scope, now).unwrap();
    assert_eq!(mapped.ticket.state, TicketState::Answered);
    match mapped.answer {
        InteractionAnswer::Approval(ans) => {
            assert_eq!(ans.option_id, "allow");
        }
        _ => panic!("approval"),
    }
    let err = store.answer_card(&action, scope, now).unwrap_err();
    assert!(err.to_string().contains("already answered"));
}

#[test]
fn same_interaction_replay_does_not_mint_a_second_ticket() {
    let interaction = load_interaction("interaction-approval.json");
    let mut store = TicketStore::with_default_ttl();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let (a, _) = store
        .issue(&interaction, "feishu:oc_p2p:main", now)
        .unwrap();
    let (b, _) = store
        .issue(&interaction, "feishu:oc_p2p:main", now)
        .unwrap();
    assert_eq!(a.ticket_id, b.ticket_id);
}

#[test]
fn question_form_value_json_string() {
    let interaction = load_interaction("interaction-question.json");
    let mut store = TicketStore::with_default_ttl();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let (ticket, card) = store
        .issue(&interaction, "feishu:oc_p2p:main", now)
        .unwrap();
    remuda_feishu::validate_card(&card).unwrap();
    let scope = AnswerScope::Chat("oc_p2p");
    let mut action = load_card_action("card-action-form.jsonl");
    action.action_value = Some(serde_json::json!({
        "tid": ticket.ticket_id,
        "a": "submit"
    }));
    let mapped = store.answer_card(&action, scope, now).unwrap();
    match mapped.answer {
        InteractionAnswer::Question(ans) => {
            let field = ans.answers.get("beverage").unwrap();
            assert_eq!(field.option_ids, ["tea"]);
        }
        _ => panic!("question"),
    }
}

#[test]
fn expire_due_flips_open_tickets() {
    let interaction = load_interaction("interaction-approval.json");
    let mut store = TicketStore::with_default_ttl();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let (ticket, _) = store
        .issue(&interaction, "feishu:oc_p2p:main", now)
        .unwrap();
    let later = now + Duration::from_secs(12 * 60);
    let expired = store.expire_due(later).unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].0.ticket_id, ticket.ticket_id);
    assert_eq!(
        store.get(&ticket.ticket_id).unwrap().state,
        TicketState::Expired
    );
    let scope = AnswerScope::Chat("oc_p2p");
    let mut action = load_card_action("card-action-allow.jsonl");
    action.action_value = Some(serde_json::json!({
        "tid": ticket.ticket_id,
        "a": "deny"
    }));
    let err = store.answer_card(&action, scope, later).unwrap_err();
    assert!(err.to_string().contains("expired"));
}

#[test]
fn render_interaction_card_matches_kind() {
    let approval = load_interaction("interaction-approval.json");
    let card = render_interaction_card(&approval, "tidtest0001").unwrap();
    assert_eq!(card["header"]["template"], "orange");
    let question = load_interaction("interaction-question.json");
    let card = render_interaction_card(&question, "tidtest0002").unwrap();
    assert_eq!(card["body"]["elements"][0]["tag"], "form");
}

#[test]
fn card_action_non_owner_is_not_this_layer() {
    let mut log = remuda_feishu::InboundLog::memory();
    let mut policy = remuda_feishu::InboundPolicy::default();
    policy
        .owner_open_ids
        .push("ou_owner_aaaaaaaaaaaaaaaaaaaaaaaaaa".into());
    let action = load_card_action("card-action-allow.jsonl");
    // F10: a card's chat gets the message path's gate, and cards carry no
    // `chat_type` — so the chat must first be known from an admitted message.
    log.remember_chat(
        &action.chat_id.clone().unwrap(),
        remuda_feishu::ChatType::P2p,
    )
    .unwrap();
    let event = remuda_feishu::RawEvent::CardAction(action);
    assert!(matches!(
        admit(event, &policy, &mut log, now()).unwrap(),
        remuda_feishu::GateDecision::Take(_)
    ));
}

// ---- F9: a ticket may only be answered from its own session/chat ----

#[test]
fn session_chat_extracts_the_chat_id() {
    assert_eq!(session_chat("feishu:oc_chat:main"), Some("oc_chat"));
    assert_eq!(session_chat("feishu:oc_chat:omt_topic"), Some("oc_chat"));
    assert_eq!(session_chat("feishu::main"), None);
    assert_eq!(session_chat("slack:oc_chat:main"), None);
    assert_eq!(session_chat("feishu:oc_chat"), None);
}

#[test]
fn card_answer_from_another_chat_is_refused() {
    let interaction = load_interaction("interaction-approval.json");
    let mut store = TicketStore::with_default_ttl();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let (ticket, _) = store
        .issue(&interaction, "feishu:oc_victim:main", now)
        .unwrap();
    let mut action = load_card_action("card-action-allow.jsonl");
    action.action_value = Some(serde_json::json!({
        "tid": ticket.ticket_id,
        "a": "allow"
    }));

    let err = store
        .answer_card(&action, AnswerScope::Chat("oc_attacker"), now)
        .unwrap_err();
    assert!(err.to_string().contains("does not belong to"), "{err}");
    // The refused attempt must not have consumed the ticket.
    assert_eq!(
        store.get(&ticket.ticket_id).unwrap().state,
        TicketState::Open
    );

    store
        .answer_card(&action, AnswerScope::Chat("oc_victim"), now)
        .expect("the owning chat still answers");
}

#[test]
fn yes_from_another_topic_cannot_answer_this_topics_ticket() {
    let interaction = load_interaction("interaction-approval.json");
    let mut store = TicketStore::with_default_ttl();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let (ticket, _) = store
        .issue(&interaction, "feishu:oc_chat:omt_topic_a", now)
        .unwrap();
    let callback = remuda_feishu::CallbackValue {
        tid: ticket.ticket_id.clone(),
        a: "once".into(),
    };
    let err = store
        .answer_callback(
            &callback,
            None,
            AnswerScope::Session("feishu:oc_chat:omt_topic_b"),
            now,
        )
        .unwrap_err();
    assert!(err.to_string().contains("does not belong to"), "{err}");
}

// ---- F8: `/yes` binds to one named ticket, never "whatever is newest" ----

#[test]
fn shortcut_refuses_to_guess_between_two_open_tickets() {
    let approval = load_interaction("interaction-approval.json");
    let question = load_interaction("interaction-question.json");
    let key = "feishu:oc_p2p:main";
    let mut store = TicketStore::with_default_ttl();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let (first, _) = store.issue(&approval, key, now).unwrap();

    // One open ticket: unambiguous, so a bare `/yes` still works.
    assert_eq!(
        store
            .resolve_shortcut(key, ShortcutRef::default(), now)
            .unwrap()
            .ticket_id,
        first.ticket_id
    );

    // A second card arrives (the F8 race). A bare `/yes` must now refuse.
    let (second, _) = store
        .issue(&question, key, now + Duration::from_secs(1))
        .unwrap();
    let miss = store
        .resolve_shortcut(key, ShortcutRef::default(), now + Duration::from_secs(2))
        .unwrap_err();
    let ShortcutMiss::Ambiguous(ids) = miss else {
        panic!("expected Ambiguous, got {miss:?}");
    };
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&first.ticket_id));
    assert!(ids.contains(&second.ticket_id));

    // Naming the older ticket still reaches it — the newest does not win by default.
    assert_eq!(
        store
            .resolve_shortcut(
                key,
                ShortcutRef {
                    ticket_id: Some(&first.ticket_id),
                    reply_to: None,
                },
                now + Duration::from_secs(2),
            )
            .unwrap()
            .ticket_id,
        first.ticket_id
    );
}

#[test]
fn shortcut_binds_by_reply_to_the_card_message() {
    let approval = load_interaction("interaction-approval.json");
    let question = load_interaction("interaction-question.json");
    let key = "feishu:oc_p2p:main";
    let mut store = TicketStore::with_default_ttl();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let (first, _) = store.issue(&approval, key, now).unwrap();
    store.set_card_message_id(&first.ticket_id, "om_card_first");
    let (second, _) = store
        .issue(&question, key, now + Duration::from_secs(1))
        .unwrap();
    store.set_card_message_id(&second.ticket_id, "om_card_second");

    let bound = store
        .resolve_shortcut(
            key,
            ShortcutRef {
                ticket_id: None,
                reply_to: Some("om_card_first"),
            },
            now + Duration::from_secs(2),
        )
        .unwrap();
    assert_eq!(bound.ticket_id, first.ticket_id);
}

#[test]
fn shortcut_rejects_a_ticket_id_from_another_session() {
    let approval = load_interaction("interaction-approval.json");
    let mut store = TicketStore::with_default_ttl();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let (ticket, _) = store.issue(&approval, "feishu:oc_other:main", now).unwrap();
    let miss = store
        .resolve_shortcut(
            "feishu:oc_p2p:main",
            ShortcutRef {
                ticket_id: Some(&ticket.ticket_id),
                reply_to: None,
            },
            now,
        )
        .unwrap_err();
    assert_eq!(miss, ShortcutMiss::UnknownTicket(ticket.ticket_id));
}

#[test]
fn expire_session_retires_open_tickets() {
    let approval = load_interaction("interaction-approval.json");
    let question = load_interaction("interaction-question.json");
    let key = "feishu:oc_p2p:main";
    let mut store = TicketStore::with_default_ttl();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let (ticket, _) = store.issue(&approval, key, now).unwrap();
    let (kept, _) = store
        .issue(&question, "feishu:oc_elsewhere:main", now)
        .unwrap();

    let dropped = store.expire_session(key);
    assert_eq!(dropped, vec![ticket.ticket_id.clone()]);
    assert_eq!(
        store.get(&ticket.ticket_id).unwrap().state,
        TicketState::Expired
    );
    assert_eq!(store.get(&kept.ticket_id).unwrap().state, TicketState::Open);
    assert_eq!(
        store
            .resolve_shortcut(key, ShortcutRef::default(), now)
            .err(),
        Some(ShortcutMiss::NoneOpen)
    );
}
