//! Card JSON 2.0 templates vs the create-lark-card structural contract.

use remuda_feishu::{
    CardKitStream, PROGRESS_ELEMENT_ID, render_approval_card, render_completion_card,
    render_expired_card, render_progress_card, render_question_card, render_recorded_card,
    validate_card,
};
use remuda_protocol::{Interaction, QuestionField};
use serde_json::{Value, json};

fn load_interaction(name: &str) -> Interaction {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let bytes = std::fs::read(path).unwrap();
    remuda_protocol::from_json_slice(&bytes).unwrap()
}

fn find_callbacks(value: &Value, out: &mut Vec<Value>) {
    match value {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("callback")
                && let Some(v) = map.get("value")
            {
                out.push(v.clone());
            }
            for v in map.values() {
                find_callbacks(v, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                find_callbacks(v, out);
            }
        }
        _ => {}
    }
}

#[test]
fn approval_buttons_carry_tid_and_a() {
    let card = render_approval_card(
        "tidallow01",
        "Read README.md",
        "Claude wants to Read a file",
        [
            remuda_protocol::DecisionEffect::AllowSession,
            remuda_protocol::DecisionEffect::Deny,
            remuda_protocol::DecisionEffect::AllowOnce,
        ],
    )
    .unwrap();
    validate_card(&card).unwrap();
    let mut cbs = Vec::new();
    find_callbacks(&card, &mut cbs);
    let actions: Vec<&str> = cbs
        .iter()
        .filter_map(|v| v.get("a").and_then(Value::as_str))
        .collect();
    assert_eq!(actions, ["allow", "deny", "once"]);
    assert!(
        cbs.iter()
            .all(|v| v.get("tid").and_then(Value::as_str) == Some("tidallow01"))
    );
}

#[test]
fn question_form_has_exactly_one_submit() {
    let interaction = load_interaction("interaction-question.json");
    let remuda_protocol::InteractionRequest::Question(req) = &interaction.request else {
        panic!("question fixture");
    };
    let card = render_question_card("tidquest01", &req.title, &req.fields).unwrap();
    validate_card(&card).unwrap();
    let form = &card["body"]["elements"][0];
    assert_eq!(form["tag"], "form");
    assert_eq!(form["name"], "ask");
    let submits: Vec<_> = form["elements"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|el| {
            el.get("tag").and_then(Value::as_str) == Some("button")
                && el.get("form_action_type").and_then(Value::as_str) == Some("submit")
        })
        .collect();
    assert_eq!(submits.len(), 1);
    assert_eq!(submits[0]["action_type"], "form_submit");
    let fields: Vec<_> = form["elements"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|el| el.get("tag").and_then(Value::as_str) == Some("select_static"))
        .collect();
    assert_eq!(fields.len(), 1);
    assert!(fields[0].get("behaviors").is_none());
}

#[test]
fn progress_card_is_static_with_cardkit_element_id() {
    let card = render_progress_card("Working", "Read", "opening file", 12).unwrap();
    assert_eq!(card["config"]["streaming_mode"], false);
    assert_eq!(
        card["body"]["elements"][0]["element_id"],
        PROGRESS_ELEMENT_ID
    );
    let stream = CardKitStream::default();
    assert_eq!(stream.max_hz, 10);
    assert_eq!(stream.element_id, PROGRESS_ELEMENT_ID);
    let cfg = CardKitStream::streaming_config();
    assert_eq!(cfg["streaming_mode"], true);
}

#[test]
fn completion_recorded_expired_validate() {
    validate_card(&render_completion_card("Done", "short conclusion", true).unwrap()).unwrap();
    validate_card(&render_recorded_card("Answer recorded").unwrap()).unwrap();
    validate_card(&render_expired_card("Expired").unwrap()).unwrap();
}

#[test]
fn rejects_legacy_envelope_and_schema_1() {
    let bad = json!({"msg_type":"interactive","card":{"schema":"2.0"}});
    assert!(validate_card(&bad).is_err());
    let v1 = json!({
        "schema": "1.0",
        "config": { "compact_width": false },
        "body": { "elements": [] }
    });
    assert!(validate_card(&v1).is_err());
}

#[test]
fn question_field_round_trip_from_protocol_fixture() {
    let interaction = load_interaction("interaction-question.json");
    let fields: &[QuestionField] = match &interaction.request {
        remuda_protocol::InteractionRequest::Question(req) => &req.fields,
        _ => panic!("kind"),
    };
    assert_eq!(fields[0].id, "beverage");
}
