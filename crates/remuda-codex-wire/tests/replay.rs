//! One fixture replay of the captured Codex app-server NDJSON session.

use std::fs;
use std::path::PathBuf;

use remuda_codex_wire::{
    ServerNotification, ThreadItem, TypedServerNotification, WireFrame, decode_line,
    is_skippable_line,
};

#[test]
fn session_fixture_replays() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/codex-appserver-session.jsonl");
    let mut notifications = 0usize;
    let mut typed_turn_completed = false;
    let mut typed_item = false;
    let mut thread_started = false;
    let mut turn_started = false;

    for (index, line) in fs::read_to_string(&path)
        .expect("session fixture")
        .lines()
        .filter(|line| !is_skippable_line(line))
        .enumerate()
    {
        let frame =
            decode_line(line).unwrap_or_else(|error| panic!("line {index}: {error}: {line}"));
        match frame {
            WireFrame::Notification(notification) => {
                if notification.method == "initialized" {
                    continue;
                }
                notifications += 1;
                let raw = serde_json::from_str(line).expect("json");
                let inbound = ServerNotification::from_method_params(
                    &notification.method,
                    notification.params,
                    raw,
                );
                match inbound {
                    ServerNotification::Typed(TypedServerNotification::TurnCompleted(_)) => {
                        typed_turn_completed = true;
                    }
                    ServerNotification::Typed(TypedServerNotification::TurnStarted(_)) => {
                        turn_started = true;
                    }
                    ServerNotification::Typed(TypedServerNotification::ThreadStarted(_)) => {
                        thread_started = true;
                    }
                    ServerNotification::Typed(TypedServerNotification::ItemStarted(started)) => {
                        assert!(
                            !matches!(started.item, ThreadItem::Unknown(_)),
                            "item/started Unknown: {line}"
                        );
                        typed_item = true;
                    }
                    ServerNotification::Typed(TypedServerNotification::ItemCompleted(
                        completed,
                    )) => {
                        assert!(
                            !matches!(completed.item, ThreadItem::Unknown(_)),
                            "item/completed Unknown: {line}"
                        );
                        typed_item = true;
                    }
                    ServerNotification::Typed(_) | ServerNotification::Unknown(_) => {}
                }
            }
            WireFrame::Error(_)
            | WireFrame::Request(_)
            | WireFrame::Response(_)
            | WireFrame::Unknown(_) => {}
        }
    }

    assert!(
        notifications > 0,
        "expected server notifications in the fixture"
    );
    assert!(thread_started, "expected typed thread/started");
    assert!(turn_started, "expected typed turn/started");
    assert!(typed_turn_completed, "expected typed turn/completed");
    assert!(typed_item, "expected typed item started/completed");
}
