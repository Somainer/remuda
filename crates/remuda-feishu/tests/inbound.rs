//! Inbound fixtures: IM + card.action.trigger, allowlist, commands, session_key.

use remuda_feishu::{
    DropReason, ExplicitCommand, GateDecision, InboundKind, InboundLog, InboundPolicy, Intent,
    admit, parse_event_line,
};
use remuda_protocol::AgentKind;
use serde_json::Value;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

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

fn take_line(name: &str, index: usize, log: &mut InboundLog) -> GateDecision {
    let line = serde_json::to_string(&load_jsonl(name)[index]).unwrap();
    let event = parse_event_line(&line).unwrap();
    admit(event, &policy(), log, now()).unwrap()
}

#[test]
fn p2p_prompt_uses_main_session() {
    let mut log = InboundLog::memory();
    let GateDecision::Take(inbound) = take_line("im-message-p2p.jsonl", 0, &mut log) else {
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
    let mut log = InboundLog::memory();
    assert!(matches!(
        take_line("im-message-p2p.jsonl", 0, &mut log),
        GateDecision::Take(_)
    ));
    assert!(matches!(
        take_line("im-message-p2p.jsonl", 0, &mut log),
        GateDecision::Drop {
            reason: DropReason::Duplicate
        }
    ));
}

#[test]
fn group_requires_bot_mention() {
    let mut log = InboundLog::memory();
    assert!(matches!(
        take_line("im-message-group-no-mention.jsonl", 0, &mut log),
        GateDecision::Drop {
            reason: DropReason::GroupRequiresMention
        }
    ));
    let GateDecision::Take(inbound) = take_line("im-message-group-at.jsonl", 0, &mut log) else {
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
    let mut log = InboundLog::memory();
    let GateDecision::Take(inbound) = take_line("im-message-root-only.jsonl", 0, &mut log) else {
        panic!("take");
    };
    assert_eq!(
        inbound.session_key.as_str(),
        "feishu:oc_group_bbbbbbbbbbbbbbbbbbbbbbbbbb:om_root_only_1"
    );
}

#[test]
fn explicit_commands() {
    let mut log = InboundLog::memory();
    let lines = load_jsonl("im-message-commands.jsonl");
    let mut kinds = Vec::new();
    for value in lines {
        let event = parse_event_line(&serde_json::to_string(&value).unwrap()).unwrap();
        let GateDecision::Take(inbound) = admit(event, &policy(), &mut log, now()).unwrap() else {
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
    assert!(matches!(
        kinds[6],
        Intent::Command(ExplicitCommand::Yes { ticket_id: None })
    ));
    assert!(matches!(
        kinds[7],
        Intent::Command(ExplicitCommand::No { ticket_id: None })
    ));
}

#[test]
fn stranger_and_unknown_chat_are_dropped() {
    let mut log = InboundLog::memory();
    let mut value = load_jsonl("im-message-p2p.jsonl")[0].clone();
    value["sender_id"] = Value::String("ou_stranger".into());
    value["message_id"] = Value::String("om_stranger".into());
    let event = parse_event_line(&value.to_string()).unwrap();
    assert!(matches!(
        admit(event, &policy(), &mut log, now()).unwrap(),
        GateDecision::Drop {
            reason: DropReason::OwnerNotAllowed
        }
    ));

    let mut value = load_jsonl("im-message-group-at.jsonl")[0].clone();
    value["chat_id"] = Value::String("oc_other_group".into());
    value["message_id"] = Value::String("om_other".into());
    let event = parse_event_line(&value.to_string()).unwrap();
    assert!(matches!(
        admit(event, &policy(), &mut log, now()).unwrap(),
        GateDecision::Drop {
            reason: DropReason::ChatNotAllowed
        }
    ));
}

#[test]
fn card_action_parses_callback_json_string() {
    let mut log = InboundLog::memory();
    let GateDecision::Take(inbound) = take_line("card-action-allow.jsonl", 0, &mut log) else {
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
fn card_action_without_chat_id_fails_closed_when_allowlist_set() {
    let mut log = InboundLog::memory();
    let mut value = load_jsonl("card-action-allow.jsonl")[0].clone();
    value["chat_id"] = Value::Null;
    value["event_id"] = Value::String("ev_card_no_chat".into());
    let event = parse_event_line(&value.to_string()).unwrap();
    assert!(matches!(
        admit(event, &policy(), &mut log, now()).unwrap(),
        GateDecision::Drop {
            reason: DropReason::ChatNotAllowed
        }
    ));
}

#[test]
fn group_mention_name_must_match_exactly() {
    let mut log = InboundLog::memory();
    let mut value = load_jsonl("im-message-group-at.jsonl")[0].clone();
    value["mentions"][0]["name"] = Value::String("Remuda intern".into());
    value["mentions"][0]["id"] = Value::String("ou_other".into());
    value["content"] = Value::String("hello team".into());
    value["message_id"] = Value::String("om_grp_substring".into());
    let event = parse_event_line(&value.to_string()).unwrap();
    assert!(matches!(
        admit(event, &policy(), &mut log, now()).unwrap(),
        GateDecision::Drop {
            reason: DropReason::GroupRequiresMention
        }
    ));
}

#[test]
fn card_token_is_redacted_in_debug() {
    let mut log = InboundLog::memory();
    let GateDecision::Take(inbound) = take_line("card-action-allow.jsonl", 0, &mut log) else {
        panic!("card");
    };
    let debug = format!("{inbound:?}");
    assert!(!debug.contains("tok_test_allow"), "{debug}");
    assert!(debug.contains("[redacted]"), "{debug}");
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

// ---- F10: a card click gets the same chat gate as a message in that chat ----

/// With an empty `chat_allowlist` — a valid config, only `owner_open_ids` is
/// required — the message path still drops group messages. A card click in that
/// same group must be dropped too, not admitted by an `is_empty()` shortcut.
#[test]
fn card_action_in_unlisted_group_is_dropped_with_empty_allowlist() {
    let open_allowlist = InboundPolicy {
        owner_open_ids: vec!["ou_owner_aaaaaaaaaaaaaaaaaaaaaaaaaa".into()],
        chat_allowlist: Vec::new(),
        bot_open_id: Some("ou_bot_cccccccccccccccccccccccccccc".into()),
        bot_name: Some("Remuda".into()),
        allow_unaddressed: false,
    };
    let mut log = InboundLog::memory();

    // The message path drops this group outright: groups always need a list entry.
    let mut message = load_jsonl("im-message-group-at.jsonl")[0].clone();
    message["chat_id"] = Value::String("oc_unlisted_group".into());
    message["message_id"] = Value::String("om_unlisted_group".into());
    let event = parse_event_line(&message.to_string()).unwrap();
    assert!(matches!(
        admit(event, &open_allowlist, &mut log, now()).unwrap(),
        GateDecision::Drop {
            reason: DropReason::ChatNotAllowed
        }
    ));

    // The card path must agree. A card carries no chat_type, and no admitted
    // message has taught us this chat, so it is treated as a group: dropped.
    let mut card = load_jsonl("card-action-allow.jsonl")[0].clone();
    card["chat_id"] = Value::String("oc_unlisted_group".into());
    card["event_id"] = Value::String("ev_unlisted_group".into());
    let event = parse_event_line(&card.to_string()).unwrap();
    assert!(
        matches!(
            admit(event, &open_allowlist, &mut log, now()).unwrap(),
            GateDecision::Drop {
                reason: DropReason::ChatNotAllowed
            }
        ),
        "an empty allowlist must not fail open for card clicks"
    );
}

/// A p2p chat learned from an admitted message stays clickable under an empty
/// allowlist — the card path mirrors the message path in both directions.
#[test]
fn card_action_in_known_p2p_is_admitted_with_empty_allowlist() {
    let open_allowlist = InboundPolicy {
        owner_open_ids: vec!["ou_owner_aaaaaaaaaaaaaaaaaaaaaaaaaa".into()],
        chat_allowlist: Vec::new(),
        ..InboundPolicy::default()
    };
    let mut log = InboundLog::memory();
    let event = parse_event_line(&load_jsonl("im-message-p2p.jsonl")[0].to_string()).unwrap();
    assert!(matches!(
        admit(event, &open_allowlist, &mut log, now()).unwrap(),
        GateDecision::Take(_)
    ));

    let event = parse_event_line(&load_jsonl("card-action-allow.jsonl")[0].to_string()).unwrap();
    let GateDecision::Take(inbound) = admit(event, &open_allowlist, &mut log, now()).unwrap()
    else {
        panic!("a card in a known p2p chat must be admitted");
    };
    assert_eq!(inbound.chat_type, Some(remuda_feishu::ChatType::P2p));
}

// ---- F11: delivery idempotency survives a dispatcher restart ----

/// The in-memory ring is empty after a restart, so a Feishu redelivery of the
/// first prompt would create a second instance. The persisted log must catch it.
#[test]
fn redelivery_after_restart_is_still_a_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("inbound.sqlite");
    let line = load_jsonl("im-message-p2p.jsonl")[0].to_string();

    {
        let mut log = InboundLog::open(&path).unwrap();
        let event = parse_event_line(&line).unwrap();
        assert!(matches!(
            admit(event, &policy(), &mut log, now()).unwrap(),
            GateDecision::Take(_)
        ));
        // Same process: the ring catches it.
        let event = parse_event_line(&line).unwrap();
        assert!(matches!(
            admit(event, &policy(), &mut log, now()).unwrap(),
            GateDecision::Drop {
                reason: DropReason::Duplicate
            }
        ));
    }

    // Restart: fresh ring, same file.
    let mut log = InboundLog::open(&path).unwrap();
    let event = parse_event_line(&line).unwrap();
    assert!(
        matches!(
            admit(event, &policy(), &mut log, now()).unwrap(),
            GateDecision::Drop {
                reason: DropReason::Duplicate
            }
        ),
        "a redelivery after restart must not re-run the first prompt"
    );

    // And the chat type learned before the restart is still known, so the F10
    // card gate does not regress to fail-closed for an established chat.
    assert_eq!(
        log.chat_type("oc_p2p_aaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        Some(remuda_feishu::ChatType::P2p)
    );
}

/// Delivery ids outside the retention window are pruned, so the table does not
/// grow without bound — but a replay inside the window is still caught.
#[test]
fn persisted_delivery_ids_are_pruned_after_retention() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("inbound.sqlite");
    let line = load_jsonl("im-message-p2p.jsonl")[0].to_string();
    let retention = Duration::from_secs(60);

    {
        let mut log = InboundLog::open(&path).unwrap().with_retention(retention);
        let event = parse_event_line(&line).unwrap();
        assert!(matches!(
            admit(event, &policy(), &mut log, now()).unwrap(),
            GateDecision::Take(_)
        ));
    }
    {
        // Restart well past the window: the row is pruned and this is a fresh event.
        let mut log = InboundLog::open(&path).unwrap().with_retention(retention);
        let event = parse_event_line(&line).unwrap();
        let later = now() + retention + Duration::from_secs(1);
        assert!(matches!(
            admit(event, &policy(), &mut log, later).unwrap(),
            GateDecision::Take(_)
        ));
    }
}

#[cfg(unix)]
#[test]
fn inbound_log_file_is_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/inbound.sqlite");
    let _log = InboundLog::open(&path).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}
