//! Inbound fixtures: IM + card.action.trigger, allowlist, commands, session_key.

use remuda_feishu::{
    DropReason, ExplicitCommand, GateDecision, InboundKind, InboundPolicy, Intent, admit,
    parse_event_line,
};
use remuda_protocol::AgentKind;
use serde_json::Value;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn load_jsonl(name: &str) -> Vec<Value> {
    let text = std::fs::read_to_string(fixture(name)).unwrap();
    text.lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn policy() -> InboundPolicy {
    InboundPolicy {
        owner_open_ids: vec!["ou_owner_aaaaaaaaaaaaaaaaaaaaaaaaaa".into()],
        chat_allowlist: vec![
            "oc_p2p_aaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            "oc_group_bbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
        ],
        bot_open_id: Some("ou_bot_cccccccccccccccccccccccccccc".into()),
        bot_name: Some("Remuda".into()),
        allow_unaddressed: false,
    }
}

fn take_line(name: &str, index: usize, dedup: &mut remuda_feishu::Deduper) -> GateDecision {
    let line = serde_json::to_string(&load_jsonl(name)[index]).unwrap();
    let event = parse_event_line(&line).unwrap();
    admit(event, &policy(), dedup).unwrap()
}

#[test]
fn p2p_prompt_uses_main_session() {
    let mut dedup = remuda_feishu::Deduper::default();
    let GateDecision::Take(inbound) = take_line("im-message-p2p.jsonl", 0, &mut dedup) else {
        panic!("expected take");
    };
    assert_eq!(
        inbound.session_key.as_str(),
        "feishu:oc_p2p_aaaaaaaaaaaaaaaaaaaaaaaaaaaa:main"
    );
    assert_eq!(
        inbound.idempotency_key,
        "om_p2p_0000000000000000000000000001"
    );
    match inbound.kind {
        InboundKind::Message { intent, .. } => {
            let Intent::Prompt { text } = intent else {
                panic!("prompt");
            };
            assert!(text.contains("README"));
        }
        _ => panic!("message"),
    }
}

#[test]
fn duplicate_message_id_is_dropped() {
    let mut dedup = remuda_feishu::Deduper::default();
    assert!(matches!(
        take_line("im-message-p2p.jsonl", 0, &mut dedup),
        GateDecision::Take(_)
    ));
    assert!(matches!(
        take_line("im-message-p2p.jsonl", 0, &mut dedup),
        GateDecision::Drop {
            reason: DropReason::Duplicate
        }
    ));
}

#[test]
fn group_requires_bot_mention() {
    let mut dedup = remuda_feishu::Deduper::default();
    assert!(matches!(
        take_line("im-message-group-no-mention.jsonl", 0, &mut dedup),
        GateDecision::Drop {
            reason: DropReason::GroupRequiresMention
        }
    ));
    let GateDecision::Take(inbound) = take_line("im-message-group-at.jsonl", 0, &mut dedup) else {
        panic!("at-bot should pass");
    };
    assert_eq!(
        inbound.session_key.as_str(),
        "feishu:oc_group_bbbbbbbbbbbbbbbbbbbbbbbbbb:omt_topic_1"
    );
    match inbound.kind {
        InboundKind::Message { intent, .. } => {
            assert!(matches!(intent, Intent::Command(ExplicitCommand::Status)));
        }
        _ => panic!("message"),
    }
}

#[test]
fn root_id_is_used_when_thread_id_absent() {
    let mut dedup = remuda_feishu::Deduper::default();
    let GateDecision::Take(inbound) = take_line("im-message-root-only.jsonl", 0, &mut dedup) else {
        panic!("take");
    };
    assert_eq!(
        inbound.session_key.as_str(),
        "feishu:oc_group_bbbbbbbbbbbbbbbbbbbbbbbbbb:om_root_only_1"
    );
}

#[test]
fn explicit_commands() {
    let mut dedup = remuda_feishu::Deduper::default();
    let lines = load_jsonl("im-message-commands.jsonl");
    let mut kinds = Vec::new();
    for value in lines {
        let event = parse_event_line(&serde_json::to_string(&value).unwrap()).unwrap();
        let GateDecision::Take(inbound) = admit(event, &policy(), &mut dedup).unwrap() else {
            panic!("command dropped");
        };
        match inbound.kind {
            InboundKind::Message { intent, .. } => kinds.push(intent),
            _ => panic!("message"),
        }
    }
    assert!(matches!(kinds[0], Intent::Command(ExplicitCommand::New)));
    let Intent::Command(ExplicitCommand::Host { name }) = &kinds[1] else {
        panic!("host");
    };
    assert_eq!(name, "devbox");
    let Intent::Command(ExplicitCommand::Agent { kind, raw }) = &kinds[2] else {
        panic!("agent");
    };
    assert_eq!(*kind, AgentKind::Claude);
    assert_eq!(raw, "claude");
    let Intent::Command(ExplicitCommand::Model { model }) = &kinds[3] else {
        panic!("model");
    };
    assert_eq!(model, "passthrough/auto_model/alwaysday1_max");
    assert!(matches!(kinds[4], Intent::Command(ExplicitCommand::Status)));
    assert!(matches!(kinds[5], Intent::Command(ExplicitCommand::Stop)));
    assert!(matches!(kinds[6], Intent::Command(ExplicitCommand::Yes)));
    assert!(matches!(kinds[7], Intent::Command(ExplicitCommand::No)));
}

#[test]
fn stranger_and_unknown_chat_are_dropped() {
    let mut dedup = remuda_feishu::Deduper::default();
    let mut value = load_jsonl("im-message-p2p.jsonl")[0].clone();
    value["sender_id"] = Value::String("ou_stranger".into());
    value["message_id"] = Value::String("om_stranger".into());
    let event = parse_event_line(&value.to_string()).unwrap();
    assert!(matches!(
        admit(event, &policy(), &mut dedup).unwrap(),
        GateDecision::Drop {
            reason: DropReason::OwnerNotAllowed
        }
    ));

    let mut value = load_jsonl("im-message-group-at.jsonl")[0].clone();
    value["chat_id"] = Value::String("oc_other_group".into());
    value["message_id"] = Value::String("om_other".into());
    let event = parse_event_line(&value.to_string()).unwrap();
    assert!(matches!(
        admit(event, &policy(), &mut dedup).unwrap(),
        GateDecision::Drop {
            reason: DropReason::ChatNotAllowed
        }
    ));
}

#[test]
fn card_action_parses_callback_json_string() {
    let mut dedup = remuda_feishu::Deduper::default();
    let GateDecision::Take(inbound) = take_line("card-action-allow.jsonl", 0, &mut dedup) else {
        panic!("card");
    };
    match inbound.kind {
        InboundKind::CardAction { callback, .. } => {
            let cb = callback.expect("callback");
            assert_eq!(cb.tid, "tidallow01");
            assert_eq!(cb.a, "allow");
        }
        _ => panic!("card kind"),
    }
}

#[test]
fn content_is_not_parsed_as_json() {
    let line = load_jsonl("im-message-p2p.jsonl")[0].clone();
    let event = parse_event_line(&line.to_string()).unwrap();
    let remuda_feishu::RawEvent::Message(msg) = event else {
        panic!("im");
    };
    assert_eq!(msg.content, "please read the README");
    assert!(serde_json::from_str::<Value>(&msg.content).is_err());
}
