//! Transcript authorship and regrouping on real recorded sessions (D-028 §7).
//!
//! The user's report was that the 结构 view showed text they never sent:
//! «结构化界面会把追加的 prompt 信息也额外展示了 … 1 是有重复，2 是容易让人误解是我发了这些信息».
//! A Claude transcript files skill bodies, slash-command markup and background
//! task notifications under `role: "user"`, so the role alone cannot separate
//! the human's words from text injected on their behalf.
//!
//! These fixtures are real claude 2.1.221 sessions (see `fixtures/SOURCES.md`),
//! not hand-written shapes, because the whole point is which records the
//! harness actually writes.

use remuda_driver::{DriverKind, TranscriptMapper};
use remuda_protocol::{
    ContentBlock, HostId, Id, InstanceId, MessageOrigin, MessagePayload, MessageRole,
    ObservationPayload, RunId,
};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn hydrate(name: &str) -> Vec<remuda_protocol::Observation> {
    let body = std::fs::read_to_string(fixture(name)).expect("fixture");
    let mut mapper = TranscriptMapper::new(
        DriverKind::ShellPty,
        InstanceId::new(),
        RunId::new(),
        Id::new("obj").expect("journal id"),
        HostId::new(),
        "55555555-6666-4777-8888-999999999999".into(),
        "shell-pty".into(),
    );
    let mut out: Vec<_> = body
        .lines()
        .flat_map(|line| mapper.map_line(line).expect("map transcript line"))
        .collect();
    // The mapper holds an assistant run open until something supersedes it, so
    // the last turn only lands once the tailer flushes (D-028 §7).
    out.extend(mapper.flush().expect("flush"));
    out
}

fn text_of(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Every user-role message with its classified origin, in transcript order.
fn user_messages(observations: &[remuda_protocol::Observation]) -> Vec<(MessageOrigin, String)> {
    observations
        .iter()
        .filter_map(|observation| match &observation.body {
            ObservationPayload::Message(payload) if payload.role == MessageRole::User => {
                Some((payload.origin_or_human(), text_of(&payload.blocks)))
            }
            _ => None,
        })
        .collect()
}

fn assistant_messages(observations: &[remuda_protocol::Observation]) -> Vec<&MessagePayload> {
    observations
        .iter()
        .filter_map(|observation| match &observation.body {
            ObservationPayload::Message(payload) if payload.role == MessageRole::Assistant => {
                Some(payload.as_ref())
            }
            _ => None,
        })
        .collect()
}

/// A `/skill` turn writes two user records the human never typed. Both must
/// still reach the journal — dropping them loses the reason the agent answered
/// the way it did — but neither may claim to be the user's own words.
#[test]
fn a_skill_invocation_and_its_body_are_marked_as_injected() {
    let observations = hydrate("claude-transcript-skill.jsonl");
    let users = user_messages(&observations);
    assert!(
        !users.is_empty(),
        "injected records stay in the journal rather than being dropped"
    );
    let invocation = users
        .iter()
        .find(|(_, text)| text.contains("<command-name>"))
        .expect("the slash-command markup is journaled");
    assert_eq!(
        invocation.0,
        MessageOrigin::InjectedSkill,
        "command markup is not something the user typed"
    );
    let body = users
        .iter()
        .find(|(_, text)| text.contains("Base directory for this skill"))
        .expect("the expanded skill body is journaled");
    assert_eq!(
        body.0,
        MessageOrigin::InjectedSkill,
        "the skill body is injected, not the user's words"
    );
    assert!(
        !users
            .iter()
            .any(|(origin, _)| *origin == MessageOrigin::Human),
        "this session has no human-typed prompt at all: {users:?}"
    );
}

/// The case in the user's screenshots: a background Workflow reports back as a
/// `user` record. Rendering it as a "You" bubble is what made it look like they
/// had sent the notification themselves.
#[test]
fn a_background_task_notification_is_not_the_users_words() {
    let observations = hydrate("claude-transcript-workflow.jsonl");
    let users = user_messages(&observations);
    let notification = users
        .iter()
        .find(|(_, text)| text.contains("<task-notification>"))
        .expect("the task notification is journaled");
    assert_eq!(notification.0, MessageOrigin::HookContext);

    // The real prompt in the same session is still the user's.
    let prompt = users
        .iter()
        .find(|(_, text)| text.contains("Use the Workflow tool"))
        .expect("the human prompt is journaled");
    assert_eq!(
        prompt.0,
        MessageOrigin::Human,
        "the actual prompt must keep rendering as the user's own"
    );
}

/// Tool results share the `user` role with prompts. They belong inside their
/// tool card, so they must never be classified as something to put in a bubble.
#[test]
fn tool_results_are_never_classified_as_human() {
    for name in [
        "claude-transcript-skill.jsonl",
        "claude-transcript-workflow.jsonl",
        "claude-transcript-tools.jsonl",
    ] {
        for (origin, text) in user_messages(&hydrate(name)) {
            assert_ne!(
                (origin, text.contains("tool_result")),
                (MessageOrigin::Human, true),
                "{name}: a tool result must not be attributed to the user"
            );
        }
    }
}

/// The regrouping fix: a `thinking` record and a `text` record sharing one
/// `message.id` are one assistant message. Before the fix each record was fed
/// to the stdout mapper as a whole message, which is what slid the per-message
/// tool bookkeeping by one.
#[test]
fn records_sharing_a_message_id_become_one_assistant_message() {
    let observations = hydrate("claude-transcript-skill.jsonl");
    let ids: std::collections::BTreeSet<_> = assistant_messages(&observations)
        .iter()
        .map(|payload| payload.message_id.clone())
        .collect();
    assert_eq!(
        ids.len(),
        1,
        "the two records of one message.id must not mint two messages"
    );
    // The thinking block travels as a thought, and the text as the message, so
    // both halves of that single message survive the regrouping. (open + close
    // are two mutations of one node, hence the id set rather than a count.)
    let thoughts: std::collections::BTreeSet<_> = observations
        .iter()
        .filter_map(|observation| match &observation.body {
            ObservationPayload::Thought(payload) => Some(payload.thought_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        thoughts.len(),
        1,
        "the thinking block is not lost in regrouping"
    );
}

/// `queue-operation` and `permission-mode` are the queue ledger and the mode
/// drift record. §7 lists them as must-keep; the old mapper dropped every
/// record that was not `user` / `assistant`.
#[test]
fn the_queue_ledger_and_mode_drift_reach_the_journal() {
    let observations = hydrate("claude-transcript-skill.jsonl");
    let named: Vec<String> = observations
        .iter()
        .filter_map(|observation| match &observation.body {
            ObservationPayload::Lifecycle(payload) => match payload.as_ref() {
                remuda_protocol::LifecyclePayload::Native(native) => {
                    Some(native.native_name.clone())
                }
                remuda_protocol::LifecyclePayload::Entity(_) => None,
            },
            _ => None,
        })
        .collect();
    assert!(
        named.iter().any(|name| name == "queue-operation"),
        "the native queue ledger is evidence for what the composer showed: {named:?}"
    );
}

/// A human prompt recorded twice (enqueued, then delivered) is one message.
/// Both records share a `promptId`, so the pair is a duplicate only when the
/// text matches too.
#[test]
fn one_prompt_recorded_twice_renders_once() {
    let mut mapper = TranscriptMapper::new(
        DriverKind::ShellPty,
        InstanceId::new(),
        RunId::new(),
        Id::new("obj").expect("journal id"),
        HostId::new(),
        "66666666-7777-4888-8999-aaaaaaaaaaaa".into(),
        "shell-pty".into(),
    );
    let enqueued = serde_json::json!({
        "type": "user", "uuid": "u-1", "promptId": "p-1", "promptSource": "typed",
        "message": {"role": "user", "content": "run the tests"},
    });
    let delivered = serde_json::json!({
        "type": "user", "uuid": "u-2", "promptId": "p-1", "promptSource": "typed",
        "message": {"role": "user", "content": "run the tests"},
    });
    let other = serde_json::json!({
        "type": "user", "uuid": "u-3", "promptId": "p-1", "promptSource": "typed",
        "message": {"role": "user", "content": "and then lint"},
    });
    let mut out = mapper.map_record(enqueued).expect("first");
    out.extend(mapper.map_record(delivered).expect("second"));
    assert_eq!(
        user_messages(&out).len(),
        1,
        "the delivered copy of a queued prompt is not a second message"
    );
    // A different prompt queued under the same id is a real second message.
    let more = mapper.map_record(other).expect("third");
    assert_eq!(
        user_messages(&more).len(),
        1,
        "a distinct prompt sharing the promptId must still be shown"
    );
}

/// Re-reading the same line (a tail that restarts at offset 0) must not
/// duplicate the conversation.
#[test]
fn a_replayed_line_is_not_emitted_twice() {
    let mut mapper = TranscriptMapper::new(
        DriverKind::ShellPty,
        InstanceId::new(),
        RunId::new(),
        Id::new("obj").expect("journal id"),
        HostId::new(),
        "77777777-8888-4999-8aaa-bbbbbbbbbbbb".into(),
        "shell-pty".into(),
    );
    let line = r#"{"type":"user","uuid":"u-9","message":{"role":"user","content":"hello"}}"#;
    let first = mapper.map_line(line).expect("first");
    let again = mapper.map_line(line).expect("replay");
    assert_eq!(user_messages(&first).len(), 1);
    assert!(
        user_messages(&again).is_empty(),
        "the same record uuid must not produce a second message"
    );
}
