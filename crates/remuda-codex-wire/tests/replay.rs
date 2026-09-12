//! Replay captured Codex app-server NDJSON.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use remuda_codex_wire::{
    InitializeResponse, ModelListResponse, RequestId, ServerNotification, ServerRequest,
    ThreadItem, ThreadListResponse, ThreadReadResponse, ThreadResumeResponse, ThreadStartResponse,
    TurnStartResponse, TurnSteerResponse, TypedServerNotification, TypedThreadItem, WireFrame,
    decode_line, is_skippable_line,
};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn session_lines() -> Vec<String> {
    fs::read_to_string(fixture("codex-appserver-session.jsonl"))
        .expect("session fixture")
        .lines()
        .filter(|line| !is_skippable_line(line))
        .map(str::to_owned)
        .collect()
}

#[test]
fn session_fixture_every_line_is_a_frame() {
    let lines = session_lines();
    assert!(
        lines.len() >= 70,
        "expected the full 0.154.0 probe, got {}",
        lines.len()
    );
    for (index, line) in lines.iter().enumerate() {
        decode_line(line).unwrap_or_else(|error| panic!("line {index}: {error}: {line}"));
    }
}

#[test]
fn session_notifications_are_typed() {
    let mut unknown = Vec::new();
    let mut typed_methods = Vec::new();
    for line in session_lines() {
        let WireFrame::Notification(notification) = decode_line(&line).expect("frame") else {
            continue;
        };
        // The mixed capture includes the client→server `initialized`
        // notification; that is not a server notification.
        if notification.method == "initialized" {
            continue;
        }
        let raw = serde_json::from_str(&line).expect("json");
        let inbound =
            ServerNotification::from_method_params(&notification.method, notification.params, raw);
        match inbound {
            ServerNotification::Typed(typed) => typed_methods.push(match typed {
                TypedServerNotification::Error(_) => "error",
                TypedServerNotification::ThreadStarted(_) => "thread/started",
                TypedServerNotification::ThreadStatusChanged(_) => "thread/status/changed",
                TypedServerNotification::ThreadTokenUsageUpdated(_) => "thread/tokenUsage/updated",
                TypedServerNotification::TurnStarted(_) => "turn/started",
                TypedServerNotification::TurnCompleted(_) => "turn/completed",
                TypedServerNotification::ItemStarted(started) => {
                    assert!(
                        !matches!(started.item, ThreadItem::Unknown(_)),
                        "item/started fell through to Unknown: {line}"
                    );
                    "item/started"
                }
                TypedServerNotification::ItemCompleted(completed) => {
                    assert!(
                        !matches!(completed.item, ThreadItem::Unknown(_)),
                        "item/completed fell through to Unknown: {line}"
                    );
                    "item/completed"
                }
                TypedServerNotification::AgentMessageDelta(delta) => {
                    assert_eq!(delta.delta, "OK");
                    "item/agentMessage/delta"
                }
                TypedServerNotification::McpServerStartupStatusUpdated(_) => {
                    "mcpServer/startupStatus/updated"
                }
                TypedServerNotification::Warning(_) => "warning",
                TypedServerNotification::DeprecationNotice(_) => "deprecationNotice",
                TypedServerNotification::AccountRateLimitsUpdated(_) => {
                    "account/rateLimits/updated"
                }
                TypedServerNotification::RemoteControlStatusChanged(_) => {
                    "remoteControl/status/changed"
                }
                TypedServerNotification::ServerRequestResolved(_) => "serverRequest/resolved",
                TypedServerNotification::ThreadNameUpdated(_) => "thread/name/updated",
                TypedServerNotification::ThreadArchived(_) => "thread/archived",
                TypedServerNotification::ThreadUnarchived(_) => "thread/unarchived",
                TypedServerNotification::ThreadDeleted(_) => "thread/deleted",
                TypedServerNotification::ThreadClosed(_) => "thread/closed",
                TypedServerNotification::ThreadReverted(_) => "thread/reverted",
                TypedServerNotification::TurnDiffUpdated(_) => "turn/diff/updated",
                TypedServerNotification::TurnPlanUpdated(_) => "turn/plan/updated",
                TypedServerNotification::ReasoningTextDelta(_) => "item/reasoning/textDelta",
                TypedServerNotification::ReasoningSummaryTextDelta(_) => {
                    "item/reasoning/summaryTextDelta"
                }
                TypedServerNotification::CommandExecutionOutputDelta(_) => {
                    "item/commandExecution/outputDelta"
                }
                TypedServerNotification::FileChangePatchUpdated(_) => {
                    "item/fileChange/patchUpdated"
                }
                TypedServerNotification::PlanDelta(_) => "item/plan/delta",
                TypedServerNotification::ConfigWarning(_) => "configWarning",
                TypedServerNotification::GuardianWarning(_) => "guardianWarning",
                TypedServerNotification::HookStarted(_) => "hook/started",
                TypedServerNotification::HookCompleted(_) => "hook/completed",
            }),
            ServerNotification::Unknown(_) => unknown.push(notification.method),
        }
    }
    assert!(
        unknown.is_empty(),
        "session notifications should be typed, unknown={unknown:?}"
    );
    for method in [
        "thread/started",
        "thread/status/changed",
        "turn/started",
        "turn/completed",
        "item/started",
        "item/completed",
        "item/agentMessage/delta",
        "warning",
        "deprecationNotice",
        "mcpServer/startupStatus/updated",
        "thread/tokenUsage/updated",
        "account/rateLimits/updated",
        "remoteControl/status/changed",
    ] {
        assert!(
            typed_methods.contains(&method),
            "missing typed notification {method}, saw {typed_methods:?}"
        );
    }
}

#[test]
fn session_responses_match_pending_methods() {
    let mut pending: HashMap<String, String> = HashMap::new();
    let mut decoded = 0usize;
    for line in session_lines() {
        match decode_line(&line).expect("frame") {
            WireFrame::Request(request) => {
                pending.insert(request.id.to_string(), request.method);
            }
            WireFrame::Response(response) => {
                let method = pending
                    .remove(&response.id.to_string())
                    .unwrap_or_else(|| panic!("response for unknown id {}", response.id));
                match method.as_str() {
                    "initialize" => {
                        let _: InitializeResponse =
                            serde_json::from_value(response.result).expect("initialize");
                    }
                    "model/list" => {
                        let listed: ModelListResponse =
                            serde_json::from_value(response.result).expect("model/list");
                        assert!(listed.data.iter().any(|model| model.id == "gpt-5.6-sol"));
                    }
                    "thread/start" => {
                        let started: ThreadStartResponse =
                            serde_json::from_value(response.result).expect("thread/start");
                        assert!(!started.thread.id.is_empty());
                        assert!(matches!(
                            started.sandbox,
                            Some(remuda_codex_wire::SandboxPolicy::WorkspaceWrite { .. })
                        ));
                    }
                    "turn/start" => {
                        let started: TurnStartResponse =
                            serde_json::from_value(response.result).expect("turn/start");
                        assert_eq!(
                            started.turn.status,
                            remuda_codex_wire::TurnStatus::InProgress
                        );
                    }
                    "turn/steer" => {
                        let _: TurnSteerResponse =
                            serde_json::from_value(response.result).expect("turn/steer");
                    }
                    "turn/interrupt" => {
                        assert!(response.result.as_object().is_some());
                    }
                    "thread/list" => {
                        let _: ThreadListResponse =
                            serde_json::from_value(response.result).expect("thread/list");
                    }
                    "thread/read" => {
                        let read: ThreadReadResponse =
                            serde_json::from_value(response.result).expect("thread/read");
                        assert!(!read.thread.id.is_empty());
                    }
                    "thread/resume" => {
                        let _: ThreadResumeResponse =
                            serde_json::from_value(response.result).expect("thread/resume");
                    }
                    "account/read" | "thread/loaded/list" => {}
                    other => panic!("unhandled client method {other}"),
                }
                decoded += 1;
            }
            WireFrame::Error(error) => {
                let method = pending.remove(&error.id.to_string());
                assert!(
                    method.as_deref() == Some("thread/list")
                        || method.as_deref() == Some("initialize")
                        || method.as_deref() == Some("turn/steer"),
                    "unexpected error for {method:?}: {}",
                    error.error.message
                );
            }
            _ => {}
        }
    }
    assert!(decoded >= 10, "decoded {decoded} typed responses");
}

#[test]
fn unknown_notification_does_not_fail() {
    let line = r#"{"method":"future/extension","params":{"x":1}}"#;
    match decode_line(line).expect("frame") {
        WireFrame::Notification(notification) => {
            let inbound = ServerNotification::from_method_params(
                &notification.method,
                notification.params,
                serde_json::from_str(line).expect("json"),
            );
            assert!(inbound.is_unknown());
            assert_eq!(inbound.method(), Some("future/extension"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn unknown_item_type_is_unknown_not_error() {
    let line = r#"{"method":"item/started","params":{"item":{"type":"brandNewItem","id":"x"},"threadId":"t","turnId":"u","startedAtMs":1}}"#;
    match decode_line(line).expect("frame") {
        WireFrame::Notification(notification) => {
            let inbound = ServerNotification::from_method_params(
                &notification.method,
                notification.params,
                serde_json::from_str(line).expect("json"),
            );
            match inbound {
                ServerNotification::Typed(TypedServerNotification::ItemStarted(started)) => {
                    assert!(matches!(started.item, ThreadItem::Unknown(_)));
                    assert_eq!(started.item.item_type(), Some("brandNewItem"));
                }
                other => panic!("{other:?}"),
            }
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn approval_fixture_is_a_server_request() {
    let lines = fs::read_to_string(fixture("server-request-approval.jsonl")).expect("fixture");
    let mut saw_request = false;
    let mut saw_reply = false;
    for line in lines.lines().filter(|line| !is_skippable_line(line)) {
        match decode_line(line).expect("frame") {
            WireFrame::Request(request) => {
                let raw = serde_json::from_str(line).expect("json");
                match ServerRequest::from_request(request, raw) {
                    ServerRequest::Typed(
                        remuda_codex_wire::TypedServerRequest::CommandExecutionApproval { .. },
                    ) => {
                        saw_request = true;
                    }
                    other => panic!("{other:?}"),
                }
            }
            WireFrame::Response(response) => {
                assert_eq!(response.id, RequestId::Integer(7));
                assert_eq!(response.result["decision"], "accept");
                let encoded = serde_json::to_value(&response).expect("ser");
                assert!(encoded.get("method").is_none());
                assert!(encoded.get("jsonrpc").is_none());
                saw_reply = true;
            }
            WireFrame::Notification(_) => {}
            other => panic!("{other:?}"),
        }
    }
    assert!(saw_request && saw_reply);
}

#[test]
fn user_and_agent_items_from_session() {
    let mut saw_user = false;
    let mut saw_agent = false;
    for line in session_lines() {
        if let WireFrame::Notification(notification) = decode_line(&line).expect("frame")
            && notification.method == "item/completed"
        {
            let raw = serde_json::from_str(&line).expect("json");
            let inbound = ServerNotification::from_method_params(
                &notification.method,
                notification.params,
                raw,
            );
            if let ServerNotification::Typed(TypedServerNotification::ItemCompleted(completed)) =
                inbound
            {
                match completed.item {
                    ThreadItem::Typed(TypedThreadItem::UserMessage { .. }) => saw_user = true,
                    ThreadItem::Typed(TypedThreadItem::AgentMessage { text, .. }) => {
                        if text == "OK" {
                            saw_agent = true;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    assert!(saw_user && saw_agent);
}
