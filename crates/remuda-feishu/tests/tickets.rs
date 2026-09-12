//! Interaction ↔ card mapping, 10–15 min expiry, first-writer-wins.

use remuda_feishu::{
    CardAction, TicketState, TicketStore, admit, parse_event_line, render_interaction_card,
};
use remuda_protocol::{Interaction, InteractionAnswer};
use std::time::{Duration, SystemTime};

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
    let mut action = load_card_action("card-action-allow.jsonl");
    action.action_value = Some(serde_json::json!({
        "tid": ticket.ticket_id,
        "a": "allow"
    }));
    let mapped = store.answer_card(&action, now).unwrap();
    assert_eq!(mapped.ticket.state, TicketState::Answered);
    match mapped.answer {
        InteractionAnswer::Approval(ans) => {
            assert_eq!(ans.option_id, "allow");
        }
        _ => panic!("approval"),
    }
    let err = store.answer_card(&action, now).unwrap_err();
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
    let mut action = load_card_action("card-action-form.jsonl");
    action.action_value = Some(serde_json::json!({
        "tid": ticket.ticket_id,
        "a": "submit"
    }));
    let mapped = store.answer_card(&action, now).unwrap();
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
    let mut action = load_card_action("card-action-allow.jsonl");
    action.action_value = Some(serde_json::json!({
        "tid": ticket.ticket_id,
        "a": "deny"
    }));
    let err = store.answer_card(&action, later).unwrap_err();
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
    let mut dedup = remuda_feishu::Deduper::default();
    let mut policy = remuda_feishu::InboundPolicy::default();
    policy
        .owner_open_ids
        .push("ou_owner_aaaaaaaaaaaaaaaaaaaaaaaaaa".into());
    let action = load_card_action("card-action-allow.jsonl");
    let event = remuda_feishu::RawEvent::CardAction(action);
    assert!(matches!(
        admit(event, &policy, &mut dedup).unwrap(),
        remuda_feishu::GateDecision::Take(_)
    ));
}
