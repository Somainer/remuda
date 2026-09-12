//! Dispatcher runtime: recorded inbound fixtures, routing, tickets, throttle.

use remuda_feishu::{
    ApiCall, ConsumeEvent, DispatchAction, Dispatcher, FakeInstanceApi, FollowEvent, FollowPage,
    InboundPolicy, LarkCli, RouteDefaults, SessionStatus, admit, parse_event_line,
};
use remuda_protocol::{AgentKind, InstanceId, Interaction};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
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

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

fn load_line(name: &str, index: usize) -> String {
    let text = std::fs::read_to_string(fixture(name)).unwrap();
    text.lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .nth(index)
        .unwrap()
        .to_string()
}

fn consume_event(name: &str, index: usize) -> ConsumeEvent {
    let event = parse_event_line(&load_line(name, index)).unwrap();
    ConsumeEvent::Event {
        event_key: "im.message.receive_v1".into(),
        event: Box::new(event),
    }
}

fn dispatcher() -> Dispatcher<FakeInstanceApi> {
    Dispatcher::memory(
        FakeInstanceApi::default(),
        LarkCli::dry_run(),
        policy(),
        RouteDefaults {
            host: "devbox".into(),
            agent: AgentKind::Claude,
            model: Some("passthrough/example".into()),
        },
    )
    .unwrap()
}

fn ins(n: u8) -> InstanceId {
    InstanceId::try_from(format!("ins_01993ab0-0000-7000-8000-00000000000{n}")).unwrap()
}

#[tokio::test]
async fn prompt_creates_then_second_prompt_sends() {
    let mut disp = dispatcher();
    // FakeInstanceApi is inside Dispatcher; drive via consume + inspect sessions.
    let reports = disp
        .handle_consume(consume_event("im-message-p2p.jsonl", 0), now())
        .await
        .unwrap();
    assert!(
        reports
            .iter()
            .any(|r| matches!(r.action, DispatchAction::Created { .. })),
        "{reports:?}"
    );
    let key = "feishu:oc_p2p_aaaaaaaaaaaaaaaaaaaaaaaaaaaa:main";
    let row = disp.sessions().get(key).unwrap().unwrap();
    assert_eq!(row.status, SessionStatus::Live);
    assert!(row.instance_id.is_some());
    let first_id = row.instance_id.clone().unwrap();

    let reports = disp
        .handle_consume(consume_event("im-message-second-prompt.jsonl", 0), now())
        .await
        .unwrap();
    assert!(
        reports.iter().any(|r| matches!(
            &r.action,
            DispatchAction::Sent { instance_id } if instance_id == first_id.as_id().as_str()
        )),
        "{reports:?}"
    );
    let row = disp.sessions().get(key).unwrap().unwrap();
    assert_eq!(row.instance_id, Some(first_id));
}

#[tokio::test]
async fn commands_pin_route_status_stop_and_new() {
    let mut disp = dispatcher();
    disp.handle_consume(consume_event("im-message-commands.jsonl", 1), now())
        .await
        .unwrap(); // /host
    disp.handle_consume(consume_event("im-message-commands.jsonl", 2), now())
        .await
        .unwrap(); // /agent
    disp.handle_consume(consume_event("im-message-commands.jsonl", 3), now())
        .await
        .unwrap(); // /model
    let key = "feishu:oc_p2p_aaaaaaaaaaaaaaaaaaaaaaaaaaaa:main";
    let row = disp.sessions().get(key).unwrap().unwrap();
    assert_eq!(row.host, "devbox");
    assert_eq!(row.agent, AgentKind::Claude);
    assert_eq!(
        row.model.as_deref(),
        Some("passthrough/auto_model/alwaysday1_max")
    );

    disp.handle_consume(consume_event("im-message-p2p.jsonl", 0), now())
        .await
        .unwrap();
    let live = disp.sessions().get(key).unwrap().unwrap();
    assert_eq!(live.status, SessionStatus::Live);

    let reports = disp
        .handle_consume(consume_event("im-message-commands.jsonl", 4), now())
        .await
        .unwrap(); // /status
    assert!(reports.iter().any(|r| r.action == DispatchAction::Status));
    assert!(
        disp.outbound()
            .recorded()
            .iter()
            .any(|cmd| cmd.argv.iter().any(|a| a.contains("instance")))
    );

    let reports = disp
        .handle_consume(consume_event("im-message-commands.jsonl", 5), now())
        .await
        .unwrap(); // /stop
    assert!(
        reports
            .iter()
            .any(|r| matches!(r.action, DispatchAction::Cancelled { .. }))
    );
    assert_eq!(
        disp.sessions().get(key).unwrap().unwrap().status,
        SessionStatus::Stopped
    );

    disp.handle_consume(consume_event("im-message-commands.jsonl", 0), now())
        .await
        .unwrap(); // /new
    assert_eq!(
        disp.sessions().get(key).unwrap().unwrap().status,
        SessionStatus::Idle
    );
}

#[tokio::test]
async fn tool_boundary_posts_static_progress_output_does_not() {
    let api = FakeInstanceApi::default();
    let id = ins(2);
    api.set_next_id(id.clone());
    api.set_follow(
        &id,
        FollowPage {
            next_seq: 4,
            events: vec![
                FollowEvent::Output {
                    text: "token token token".into(),
                },
                FollowEvent::ToolBoundary {
                    name: "Read".into(),
                    summary: "README.md".into(),
                    elapsed_secs: 3,
                },
                FollowEvent::Completed {
                    conclusion: "done".into(),
                    ok: true,
                },
            ],
        },
    );
    let mut disp =
        Dispatcher::memory(api, LarkCli::dry_run(), policy(), RouteDefaults::default()).unwrap();
    let reports = disp
        .handle_consume(consume_event("im-message-p2p.jsonl", 0), now())
        .await
        .unwrap();
    let kinds: Vec<_> = reports
        .iter()
        .filter_map(|r| match &r.action {
            DispatchAction::CardPosted { kind } => Some(kind.as_str()),
            _ => None,
        })
        .collect();
    assert!(kinds.contains(&"progress"), "{reports:?}");
    assert!(kinds.contains(&"completion"), "{reports:?}");
    assert_eq!(
        kinds.iter().filter(|k| **k == "progress").count(),
        1,
        "output tokens must not emit extra progress cards"
    );
    assert!(
        disp.outbound()
            .recorded()
            .iter()
            .any(|cmd| cmd.argv.iter().any(|a| a.contains("interactive")))
    );
}

#[tokio::test]
async fn follow_interaction_then_yes_shortcut() {
    let api = FakeInstanceApi::default();
    let id = ins(3);
    api.set_next_id(id.clone());
    let interaction: Interaction = remuda_protocol::from_json_slice(
        &std::fs::read(fixture("interaction-approval.json")).unwrap(),
    )
    .unwrap();
    api.set_follow(
        &id,
        FollowPage {
            next_seq: 2,
            events: vec![FollowEvent::Interaction(Box::new(interaction))],
        },
    );
    let mut disp =
        Dispatcher::memory(api, LarkCli::dry_run(), policy(), RouteDefaults::default()).unwrap();
    disp.handle_consume(consume_event("im-message-p2p.jsonl", 0), now())
        .await
        .unwrap();
    let key = "feishu:oc_p2p_aaaaaaaaaaaaaaaaaaaaaaaaaaaa:main";
    assert!(disp.tickets().latest_open(key, now()).is_some());

    let reports = disp
        .handle_consume(consume_event("im-message-commands.jsonl", 6), now())
        .await
        .unwrap(); // /yes
    assert!(
        reports
            .iter()
            .any(|r| matches!(r.action, DispatchAction::Answered { .. })),
        "{reports:?}"
    );
}

#[tokio::test]
async fn card_action_fixture_maps_ticket_and_responds() {
    let api = FakeInstanceApi::default();
    let id = ins(1); // matches interaction-approval.json instanceId
    api.set_next_id(id.clone());
    let interaction: Interaction = remuda_protocol::from_json_slice(
        &std::fs::read(fixture("interaction-approval.json")).unwrap(),
    )
    .unwrap();
    api.set_follow(
        &id,
        FollowPage {
            next_seq: 1,
            events: vec![FollowEvent::Interaction(Box::new(interaction))],
        },
    );
    let mut disp =
        Dispatcher::memory(api, LarkCli::dry_run(), policy(), RouteDefaults::default()).unwrap();
    disp.handle_consume(consume_event("im-message-p2p.jsonl", 0), now())
        .await
        .unwrap();
    let key = "feishu:oc_p2p_aaaaaaaaaaaaaaaaaaaaaaaaaaaa:main";
    let ticket = disp.tickets().latest_open(key, now()).unwrap().clone();

    let mut event = parse_event_line(&load_line("card-action-allow.jsonl", 0)).unwrap();
    if let remuda_feishu::RawEvent::CardAction(action) = &mut event {
        action.action_value = Some(serde_json::json!({
            "tid": ticket.ticket_id,
            "a": "allow"
        }));
        action.event_id = Some("ev_card_allow_unique".into());
    }
    let reports = disp
        .handle_consume(
            ConsumeEvent::Event {
                event_key: "card.action.trigger".into(),
                event: Box::new(event),
            },
            now(),
        )
        .await
        .unwrap();
    assert!(
        reports
            .iter()
            .any(|r| matches!(r.action, DispatchAction::Answered { .. })),
        "{reports:?}"
    );
}

#[tokio::test]
async fn sqlite_persists_session_key_to_instance() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.sqlite");
    let api = FakeInstanceApi::default();
    let id = ins(4);
    api.set_next_id(id.clone());
    let mut disp = Dispatcher::open(
        &path,
        api,
        LarkCli::dry_run(),
        policy(),
        RouteDefaults::default(),
    )
    .unwrap();
    disp.handle_consume(consume_event("im-message-p2p.jsonl", 0), now())
        .await
        .unwrap();
    drop(disp);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    let store = remuda_feishu::SessionStore::open(&path).unwrap();
    let key = "feishu:oc_p2p_aaaaaaaaaaaaaaaaaaaaaaaaaaaa:main";
    let row = store.get(key).unwrap().unwrap();
    assert_eq!(row.instance_id, Some(id));
    assert_eq!(row.status, SessionStatus::Live);
}

#[tokio::test]
async fn group_without_mention_is_dropped() {
    let mut disp = dispatcher();
    let reports = disp
        .handle_consume(consume_event("im-message-group-no-mention.jsonl", 0), now())
        .await
        .unwrap();
    assert!(reports.is_empty());
}

#[test]
fn admit_still_parses_command_fixture() {
    let mut dedup = remuda_feishu::Deduper::default();
    let event = parse_event_line(&load_line("im-message-commands.jsonl", 0)).unwrap();
    let remuda_feishu::GateDecision::Take(inbound) = admit(event, &policy(), &mut dedup).unwrap()
    else {
        panic!("take");
    };
    assert_eq!(
        inbound.session_key.as_str(),
        "feishu:oc_p2p_aaaaaaaaaaaaaaaaaaaaaaaaaaaa:main"
    );
}

#[tokio::test]
async fn fake_api_records_create_then_send() {
    let api = FakeInstanceApi::default();
    let id = ins(5);
    api.set_next_id(id.clone());
    let mut disp =
        Dispatcher::memory(api, LarkCli::dry_run(), policy(), RouteDefaults::default()).unwrap();
    disp.handle_consume(consume_event("im-message-p2p.jsonl", 0), now())
        .await
        .unwrap();
    disp.handle_consume(consume_event("im-message-second-prompt.jsonl", 0), now())
        .await
        .unwrap();
    let calls = disp.api().calls();
    assert!(
        calls.iter().any(|c| matches!(c, ApiCall::Create(_))),
        "{calls:?}"
    );
    assert!(
        calls.iter().any(|c| matches!(c, ApiCall::Send(_))),
        "{calls:?}"
    );
    let key = "feishu:oc_p2p_aaaaaaaaaaaaaaaaaaaaaaaaaaaa:main";
    assert_eq!(
        disp.sessions().get(key).unwrap().unwrap().instance_id,
        Some(id)
    );
}
