//! Tests for the grok terminal-log tail: the captured 1.0.30 fixture plus
//! [U]-labelled synthesized terminal logs (design doc notation: synthesized
//! from docs, not captured).
//!
//! Every synthesized update frame is cloned from the real shapes of the
//! fixture's `run_terminal_command` call (frames 5 Pending / 7 statusless
//! Running / 8 `status: completed`) with only the call id, event id and result
//! fields repointed, so the fold is exercised through the *real* adapter and
//! projection — never through hand-built payloads.

use super::*;
use crate::adapters::{AdapterHome, FileSignalAdapter, GrokAdapter, GrokLive, LiveIdentity};
use remuda_protocol::{
    ContentBlock, HostId, InstanceId, Knowledge, MutationOperation, ResultStage, RunId,
    ToolCallState, ToolOutcome, ToolResultPayload,
};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const REAL_UPDATES: &str = include_str!("../../../tests/fixtures/grok/tui-updates.jsonl");
const REAL_EVENTS: &str = include_str!("../../../tests/fixtures/grok/tui-events.jsonl");
const SESSION_ID: &str = "01a09c24-46ef-7a03-9c89-88f1bc00bd0c";
const REAL_CALL_ID: &str = "call-spike-1789326032369250000";

struct Harness {
    _dir: TempDir,
    live: GrokLive,
    updates: PathBuf,
    terminal_dir: PathBuf,
}

fn identity() -> LiveIdentity {
    LiveIdentity {
        instance_id: InstanceId::new(),
        host_id: HostId::new(),
        run_id: RunId::new(),
    }
}

fn new_harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let session_dir = dir.path().join("sess");
    let terminal_dir = session_dir.join("terminal");
    fs::create_dir_all(&terminal_dir).unwrap();
    let updates = session_dir.join("updates.jsonl");
    let events = session_dir.join("events.jsonl");
    fs::write(&updates, "").unwrap();
    fs::write(&events, "").unwrap();
    let mut adapter = GrokAdapter::new(AdapterHome {
        home: dir.path().to_path_buf(),
        cwd: dir.path().to_path_buf(),
        pid: None,
    });
    adapter.bind_session_dir(SESSION_ID, session_dir);
    Harness {
        _dir: dir,
        live: GrokLive::new(adapter, identity()),
        updates,
        terminal_dir,
    }
}

fn poll(h: &mut Harness) -> Vec<AdapterObservation> {
    h.live.poll().unwrap()
}

fn append_frame(path: &Path, frame: Value) {
    let mut options = fs::OpenOptions::new();
    options.append(true).create(true);
    options
        .open(path)
        .unwrap()
        .write_all(format!("{}\n", serde_json::to_string(&frame).unwrap()).as_bytes())
        .unwrap();
}

fn append_bytes(path: &Path, bytes: &[u8]) {
    fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

/// [U] synthesized from docs, not captured: clone the captured frame at
/// `base` (5 = Pending `tool_call`, 7 = statusless Running update,
/// 8 = completed) and repoint its call/event ids for a synthetic call.
fn synth_frame(base: usize, call_id: &str, tag: &str) -> Value {
    let real = REAL_UPDATES.lines().nth(base).unwrap();
    let mut frame: Value = serde_json::from_str(real).unwrap();
    frame["params"]["update"]["toolCallId"] = json!(call_id);
    frame["params"]["_meta"]["eventId"] = json!(format!("{call_id}-{tag}"));
    frame
}

/// [U] synthesized completed frame: real frame 8 shape with a chosen
/// `output_file` hint and result text.
fn completed_frame(call_id: &str, output_file: Option<&str>, output_for_prompt: &str) -> Value {
    let mut frame = synth_frame(8, call_id, "completed");
    let raw = &mut frame["params"]["update"]["rawOutput"];
    if let Some(path) = output_file {
        raw["output_file"] = json!(path);
    }
    raw["output_for_prompt"] = json!(output_for_prompt);
    frame
}

fn tool_results(observed: &[AdapterObservation]) -> Vec<&ToolResultPayload> {
    observed
        .iter()
        .filter_map(|observation| match &observation.payload {
            ObservationPayload::ToolResult(result) => Some(&**result),
            _ => None,
        })
        .collect()
}

fn result_text(result: &ToolResultPayload) -> String {
    result
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

fn stage_texts(observed: &[AdapterObservation], stage: ResultStage) -> Vec<(u64, u64, String)> {
    tool_results(observed)
        .into_iter()
        .filter(|result| result.stage == stage)
        .map(|result| {
            (
                result.mutation.revision.0,
                result.mutation.base_revision.map_or(0, |base| base.0),
                result_text(result),
            )
        })
        .collect()
}

fn running_revision(observed: &[AdapterObservation], call_id: &str) -> u64 {
    observed
        .iter()
        .find_map(|observation| match &observation.payload {
            ObservationPayload::ToolCall(call)
                if call.state == ToolCallState::Running
                    && observation.item_id.as_deref() == Some(call_id) =>
            {
                Some(call.mutation.revision.0)
            }
            _ => None,
        })
        .expect("running mutation")
}

#[test]
fn growing_log_emits_only_new_bytes_then_authoritative_final() {
    let mut h = new_harness();
    let call_id = "call-synth-growing";
    let log = h.terminal_dir.join(format!("{call_id}.log"));

    append_frame(&h.updates, synth_frame(5, call_id, "pending"));
    assert!(stage_texts(&poll(&mut h), ResultStage::Partial).is_empty());

    append_frame(&h.updates, synth_frame(7, call_id, "running"));
    append_bytes(&log, b"one\n");
    let first = poll(&mut h);
    assert_eq!(running_revision(&first, call_id), 2);
    assert_eq!(
        stage_texts(&first, ResultStage::Partial),
        vec![(3, 2, "one\n".to_owned())]
    );

    // An unchanged log produces nothing — offsets dedupe, not content hashes.
    assert!(stage_texts(&poll(&mut h), ResultStage::Partial).is_empty());

    append_bytes(&log, b"two\n");
    assert_eq!(
        stage_texts(&poll(&mut h), ResultStage::Partial),
        vec![(4, 3, "two\n".to_owned())]
    );

    // The completed frame lands in the same poll as unread log bytes.
    append_bytes(&log, b"three\n");
    append_frame(&h.updates, completed_frame(call_id, None, "exit: 0\n"));
    let last = poll(&mut h);
    let results = tool_results(&last);
    let partial_pos = results
        .iter()
        .position(|result| result.stage == ResultStage::Partial)
        .expect("flush partial");
    let final_pos = results
        .iter()
        .position(|result| result.stage == ResultStage::Final)
        .expect("final");
    assert!(partial_pos < final_pos, "partial precedes final");
    assert_eq!(
        stage_texts(&last, ResultStage::Partial),
        vec![(5, 4, "three\n".to_owned())]
    );
    let final_result = results[final_pos];
    assert_eq!(final_result.mutation.revision.0, 6);
    assert_eq!(
        final_result.mutation.base_revision.map(|base| base.0),
        Some(5)
    );
    // The Final text is authoritative: it is "exit: 0\n", never the
    // concatenation of the streamed log bytes.
    assert_eq!(result_text(final_result), "exit: 0\n");
    assert_eq!(final_result.outcome, ToolOutcome::Succeeded);
    if let Knowledge::Known { value } = &final_result.exit_code {
        assert_eq!(*value, 0);
    } else {
        panic!("exit code known from the completed frame");
    }

    // The tail stops at the terminal frame: later log growth is silent.
    append_bytes(&log, b"four\n");
    assert!(stage_texts(&poll(&mut h), ResultStage::Partial).is_empty());
}

#[test]
fn log_that_appears_late_is_silent_until_it_exists() {
    let mut h = new_harness();
    let call_id = "call-synth-late";
    let log = h.terminal_dir.join(format!("{call_id}.log"));

    append_frame(&h.updates, synth_frame(5, call_id, "pending"));
    poll(&mut h);
    append_frame(&h.updates, synth_frame(7, call_id, "running"));
    // The file does not exist yet: not an error, no observation, twice.
    assert!(stage_texts(&poll(&mut h), ResultStage::Partial).is_empty());
    assert!(stage_texts(&poll(&mut h), ResultStage::Partial).is_empty());

    // It appears late, already carrying output from before the tail saw it.
    append_bytes(&log, b"late output\n");
    assert_eq!(
        stage_texts(&poll(&mut h), ResultStage::Partial),
        vec![(3, 2, "late output\n".to_owned())]
    );

    append_frame(&h.updates, completed_frame(call_id, None, "exit: 0\n"));
    let last = poll(&mut h);
    let final_result = tool_results(&last)
        .into_iter()
        .find(|result| result.stage == ResultStage::Final)
        .expect("final");
    assert_eq!(final_result.mutation.revision.0, 4);
    assert_eq!(result_text(final_result), "exit: 0\n");
}

#[test]
fn log_that_never_appears_leaves_final_unchanged() {
    let mut h = new_harness();
    let call_id = "call-synth-absent";

    append_frame(&h.updates, synth_frame(5, call_id, "pending"));
    poll(&mut h);
    append_frame(&h.updates, synth_frame(7, call_id, "running"));
    poll(&mut h);
    append_frame(&h.updates, completed_frame(call_id, None, "exit: 0\n"));
    let last = poll(&mut h);
    assert!(stage_texts(&last, ResultStage::Partial).is_empty());
    let final_result = tool_results(&last)
        .into_iter()
        .find(|result| result.stage == ResultStage::Final)
        .expect("final");
    // Byte-identical D-043 arithmetic: Proposed 1, Running 2, Final 3 base 2.
    assert_eq!(final_result.mutation.revision.0, 3);
    assert_eq!(
        final_result.mutation.base_revision.map(|base| base.0),
        Some(2)
    );
    assert_eq!(result_text(final_result), "exit: 0\n");
}

#[test]
fn final_frame_arriving_mid_tail_flushes_before_it() {
    let mut h = new_harness();
    let call_id = "call-synth-midtail";
    let log = h.terminal_dir.join(format!("{call_id}.log"));

    append_frame(&h.updates, synth_frame(5, call_id, "pending"));
    poll(&mut h);
    append_frame(&h.updates, synth_frame(7, call_id, "running"));
    poll(&mut h);
    // Bytes and the completed frame become visible in one 250 ms poll.
    append_bytes(&log, b"burst\n");
    append_frame(&h.updates, completed_frame(call_id, None, "exit: 7\n"));
    let batch = poll(&mut h);
    let results = tool_results(&batch);
    assert_eq!(
        stage_texts(&batch, ResultStage::Partial),
        vec![(3, 2, "burst\n".to_owned())]
    );
    let partial_pos = results
        .iter()
        .position(|result| result.stage == ResultStage::Partial)
        .unwrap();
    let final_pos = results
        .iter()
        .position(|result| result.stage == ResultStage::Final)
        .unwrap();
    assert!(partial_pos < final_pos);
    let final_result = results[final_pos];
    assert_eq!(final_result.mutation.revision.0, 4);
    assert_eq!(result_text(final_result), "exit: 7\n");
}

#[test]
fn output_file_hint_is_never_opened_on_this_host() {
    let mut h = new_harness();
    let call_id = "call-synth-hint";
    let log = h.terminal_dir.join(format!("{call_id}.log"));
    // The hint points outside the session's terminal directory, the way the
    // fixture's `/home/dev/...` capture path does.
    let sentinel = h._dir.path().join("elsewhere.log");
    fs::write(&sentinel, "SENTINEL-FROM-CAPTURE-HOST\n").unwrap();

    append_frame(&h.updates, synth_frame(5, call_id, "pending"));
    poll(&mut h);
    append_frame(&h.updates, synth_frame(7, call_id, "running"));
    append_bytes(&log, b"from-conventional-path\n");
    let running = poll(&mut h);
    let texts = stage_texts(&running, ResultStage::Partial);
    assert_eq!(texts, vec![(3, 2, "from-conventional-path\n".to_owned())]);

    append_frame(
        &h.updates,
        completed_frame(call_id, Some(&sentinel.to_string_lossy()), "done\n"),
    );
    let last = poll(&mut h);
    let all_text: String = tool_results(&last)
        .iter()
        .map(|result| result_text(result))
        .collect::<Vec<_>>()
        .join("");
    assert!(!all_text.contains("SENTINEL"));
    assert!(all_text.contains("done\n"));
}

#[test]
fn partial_utf8_sequence_held_until_its_final_byte() {
    let mut h = new_harness();
    let call_id = "call-synth-utf8";
    let log = h.terminal_dir.join(format!("{call_id}.log"));

    append_frame(&h.updates, synth_frame(5, call_id, "pending"));
    poll(&mut h);
    append_frame(&h.updates, synth_frame(7, call_id, "running"));
    // First two bytes of U+2713 ("✓"): no text yet, no replacement char.
    append_bytes(&log, &[0xE2, 0x9C]);
    assert!(stage_texts(&poll(&mut h), ResultStage::Partial).is_empty());
    // Final byte plus newline decodes the whole character once.
    append_bytes(&log, &[0x93, b'\n']);
    assert_eq!(
        stage_texts(&poll(&mut h), ResultStage::Partial),
        vec![(3, 2, "✓\n".to_owned())]
    );
}

#[test]
fn byte_budgets_bound_each_poll_and_the_whole_call() {
    let mut h = new_harness();
    let call_id = "call-synth-budget";
    let log = h.terminal_dir.join(format!("{call_id}.log"));

    append_frame(&h.updates, synth_frame(5, call_id, "pending"));
    poll(&mut h);
    append_frame(&h.updates, synth_frame(7, call_id, "running"));
    // Two polls' worth of bytes present before the first read: one poll may
    // publish only the per-poll cap, the rest waits for the next poll.
    let big = vec![b'a'; (2 * MAX_BYTES_PER_POLL) as usize];
    append_bytes(&log, &big);
    let first = stage_texts(&poll(&mut h), ResultStage::Partial);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].2.len(), MAX_BYTES_PER_POLL as usize);
    let second = stage_texts(&poll(&mut h), ResultStage::Partial);
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].2.len(), MAX_BYTES_PER_POLL as usize);

    // Reach the per-call cap through bounded polls, never one giant payload.
    append_bytes(&log, &vec![b'b'; MAX_BYTES_PER_CALL as usize]);
    let mut capped_partials = 0;
    let mut previous_revision = 4;
    for _ in 0..64 {
        let batch = stage_texts(&poll(&mut h), ResultStage::Partial);
        if batch.is_empty() {
            break;
        }
        for (revision, base, text) in batch {
            assert!(text.len() <= MAX_BYTES_PER_POLL as usize);
            assert_eq!(base, previous_revision);
            assert_eq!(revision, previous_revision + 1);
            previous_revision = revision;
            capped_partials += 1;
        }
    }
    assert!(capped_partials >= 1);
    // Exactly 1 MiB published across the call; growth past the cap is silent.
    append_bytes(&log, b"past-cap\n");
    assert!(stage_texts(&poll(&mut h), ResultStage::Partial).is_empty());

    // The Final still closes one revision above the last Partial.
    append_frame(&h.updates, completed_frame(call_id, None, "exit: 0\n"));
    let closing = poll(&mut h);
    let final_result = tool_results(&closing)
        .into_iter()
        .find(|result| result.stage == ResultStage::Final)
        .expect("final");
    assert_eq!(final_result.mutation.revision.0, previous_revision + 1);
    assert_eq!(
        final_result.mutation.base_revision.map(|base| base.0),
        Some(previous_revision)
    );
}

/// [U] synthesized frame carrying a different D-043 stable tool name (the
/// frame translator reads `_meta["x.ai/tool"].name`); every other shape is
/// the captured frame 7 Running update.
fn named_frame(base: usize, call_id: &str, tag: &str, name: &str) -> Value {
    let mut frame = synth_frame(base, call_id, tag);
    frame["params"]["update"]["_meta"]["x.ai/tool"]["name"] = json!(name);
    frame
}

/// (revision, base_revision, operation, text) for every Partial result in
/// emission order.
fn partial_mutations(
    observed: &[AdapterObservation],
) -> Vec<(u64, Option<u64>, MutationOperation, String)> {
    tool_results(observed)
        .into_iter()
        .filter(|result| result.stage == ResultStage::Partial)
        .map(|result| {
            (
                result.mutation.revision.0,
                result.mutation.base_revision.map(|base| base.0),
                result.mutation.operation,
                result_text(result),
            )
        })
        .collect()
}

/// (revision, base_revision, operation) for every Running ToolCall mutation
/// in emission order.
fn running_mutations(
    observed: &[AdapterObservation],
    call_id: &str,
) -> Vec<(u64, Option<u64>, MutationOperation)> {
    observed
        .iter()
        .filter_map(|observation| match &observation.payload {
            ObservationPayload::ToolCall(call)
                if call.state == ToolCallState::Running
                    && observation.item_id.as_deref() == Some(call_id) =>
            {
                Some((
                    call.mutation.revision.0,
                    call.mutation.base_revision.map(|base| base.0),
                    call.mutation.operation,
                ))
            }
            _ => None,
        })
        .collect()
}

/// The Proposed ToolCall's journal node id, keyed by the native call id.
fn proposed_node(observed: &[AdapterObservation], call_id: &str) -> remuda_protocol::Id {
    observed
        .iter()
        .find_map(|observation| match &observation.payload {
            ObservationPayload::ToolCall(call)
                if call.state == ToolCallState::Proposed
                    && observation.item_id.as_deref() == Some(call_id) =>
            {
                Some(call.tool_call_id.clone())
            }
            _ => None,
        })
        .expect("proposed tool call")
}

#[test]
fn two_progress_frames_keep_every_mutation_monotonic_with_partials() {
    // [U] a long command produces a second statusless progress frame while
    // bytes keep streaming. D-043 numbers both progress frames with its own
    // counter; the terminal fold must re-number them so neither collides with
    // a Partial already on the node (assemble.ts newerMutation would reject
    // the collision and freeze stdout).
    let mut h = new_harness();
    let call_id = "call-synth-two-progress";
    let log = h.terminal_dir.join(format!("{call_id}.log"));

    append_frame(&h.updates, synth_frame(5, call_id, "pending"));
    poll(&mut h);

    append_frame(&h.updates, synth_frame(7, call_id, "running-1"));
    append_bytes(&log, b"one\n");
    let first = poll(&mut h);
    assert_eq!(
        running_mutations(&first, call_id),
        vec![(2, Some(1), MutationOperation::Replace)]
    );
    assert_eq!(
        partial_mutations(&first),
        vec![(3, Some(2), MutationOperation::Append, "one\n".to_owned())]
    );

    append_frame(&h.updates, synth_frame(7, call_id, "running-2"));
    append_bytes(&log, b"two\n");
    let second = poll(&mut h);
    // The second Running is re-sequenced above the first Partial; the next
    // Partial's base is exactly that running revision, so web accepts it.
    assert_eq!(
        running_mutations(&second, call_id),
        vec![(4, Some(3), MutationOperation::Replace)]
    );
    assert_eq!(
        partial_mutations(&second),
        vec![(
            4 + 1,
            Some(4),
            MutationOperation::Append,
            "two\n".to_owned()
        )]
    );

    append_frame(&h.updates, completed_frame(call_id, None, "exit: 0\n"));
    let last = poll(&mut h);
    let final_result = tool_results(&last)
        .into_iter()
        .find(|result| result.stage == ResultStage::Final)
        .expect("final");
    assert_eq!(final_result.mutation.revision.0, 6);
    assert_eq!(
        final_result.mutation.base_revision.map(|base| base.0),
        Some(5)
    );
}

#[test]
fn partial_node_is_the_d043_tool_call_node() {
    let mut h = new_harness();
    let call_id = "call-synth-node";
    let log = h.terminal_dir.join(format!("{call_id}.log"));

    append_frame(&h.updates, synth_frame(5, call_id, "pending"));
    let opened = poll(&mut h);
    let node = proposed_node(&opened, call_id);

    append_frame(&h.updates, synth_frame(7, call_id, "running"));
    append_bytes(&log, b"node bytes\n");
    let running = poll(&mut h);
    for result in tool_results(&running)
        .into_iter()
        .filter(|result| result.stage == ResultStage::Partial)
    {
        assert_eq!(result.tool_call_id, node);
        assert_eq!(result.mutation.node_id, node);
    }
}

#[test]
fn non_shell_tool_starting_a_log_is_never_tailed() {
    // [U] Running read_file with a stray terminal log beside it: only
    // run_terminal_command gets a tail.
    let mut h = new_harness();
    let call_id = "call-synth-readfile";
    let stray_log = h.terminal_dir.join(format!("{call_id}.log"));

    append_frame(&h.updates, named_frame(5, call_id, "pending", "read_file"));
    poll(&mut h);
    fs::write(&stray_log, "STRAY-LOG-BYTES\n").unwrap();
    append_frame(&h.updates, named_frame(7, call_id, "running", "read_file"));
    assert!(partial_mutations(&poll(&mut h)).is_empty());
    // A second poll with the file unchanged (and still present) stays silent.
    assert!(partial_mutations(&poll(&mut h)).is_empty());

    append_frame(
        &h.updates,
        completed_frame(call_id, None, "file contents\n"),
    );
    let last = poll(&mut h);
    let final_result = tool_results(&last)
        .into_iter()
        .find(|result| result.stage == ResultStage::Final)
        .expect("final");
    // No partials interleaved, so the D-043 arithmetic is untouched.
    assert_eq!(final_result.mutation.revision.0, 3);
    assert_eq!(
        final_result.mutation.base_revision.map(|base| base.0),
        Some(2)
    );
    assert_eq!(result_text(final_result), "file contents\n");
}

#[test]
fn unsafe_native_call_ids_cannot_escape_the_session_directory() {
    // [U] call ids a real client would not emit but the fold must survive:
    // `..` traversal and an absolute id (PathBuf::join would discard the
    // session prefix for the latter). Neither file is ever read.
    let mut h = new_harness();
    let session_dir = h._dir.path().join("sess");
    let traversal_id = "../escaped-call";
    let traversal_sentinel = session_dir.join("escaped-call.log");
    fs::write(&traversal_sentinel, "TRAVERSAL-SENTINEL\n").unwrap();
    let absolute_sentinel = h._dir.path().join("absolute-call.log");
    fs::write(&absolute_sentinel, "ABSOLUTE-SENTINEL\n").unwrap();
    let absolute_id = absolute_sentinel.to_string_lossy().into_owned();

    for call_id in [traversal_id, absolute_id.as_str()] {
        append_frame(&h.updates, synth_frame(5, call_id, "pending"));
        poll(&mut h);
        append_frame(&h.updates, synth_frame(7, call_id, "running"));
        let running = poll(&mut h);
        let texts: String = partial_mutations(&running)
            .into_iter()
            .map(|(_, _, _, text)| text)
            .collect::<Vec<_>>()
            .join("");
        assert!(!texts.contains("SENTINEL"), "id {call_id:?} was tailed");
        append_frame(&h.updates, completed_frame(call_id, None, "exit: 0\n"));
        let last = poll(&mut h);
        let final_result = tool_results(&last)
            .into_iter()
            .find(|result| result.stage == ResultStage::Final)
            .expect("final");
        assert_eq!(final_result.mutation.revision.0, 3);
    }
    // The sentinels were never consumed.
    assert_eq!(
        fs::read_to_string(&traversal_sentinel).unwrap(),
        "TRAVERSAL-SENTINEL\n"
    );
    assert_eq!(
        fs::read_to_string(&absolute_sentinel).unwrap(),
        "ABSOLUTE-SENTINEL\n"
    );
}

#[test]
fn shrunken_log_is_republished_as_a_replace_snapshot() {
    // [U] rotation: the file is replaced by something shorter while the call
    // runs. Replaying from byte zero must Replace, not Append the new head
    // after the old content.
    let mut h = new_harness();
    let call_id = "call-synth-rotated";
    let log = h.terminal_dir.join(format!("{call_id}.log"));

    append_frame(&h.updates, synth_frame(5, call_id, "pending"));
    poll(&mut h);
    append_frame(&h.updates, synth_frame(7, call_id, "running"));
    fs::write(&log, "first-command-output\n").unwrap();
    assert_eq!(
        partial_mutations(&poll(&mut h)),
        vec![(
            3,
            Some(2),
            MutationOperation::Append,
            "first-command-output\n".to_owned()
        )]
    );

    // Shrink below the consumed offset: a new, shorter command log.
    fs::write(&log, "rot\n").unwrap();
    assert_eq!(
        partial_mutations(&poll(&mut h)),
        vec![(4, Some(3), MutationOperation::Replace, "rot\n".to_owned())]
    );

    // Steady growth after rotation is Append deltas again.
    append_bytes(&log, b"ated\n");
    assert_eq!(
        partial_mutations(&poll(&mut h)),
        vec![(5, Some(4), MutationOperation::Append, "ated\n".to_owned())]
    );

    append_frame(&h.updates, completed_frame(call_id, None, "exit: 0\n"));
    let last = poll(&mut h);
    let final_result = tool_results(&last)
        .into_iter()
        .find(|result| result.stage == ResultStage::Final)
        .expect("final");
    assert_eq!(final_result.mutation.revision.0, 6);
    assert_eq!(
        final_result.mutation.base_revision.map(|base| base.0),
        Some(5)
    );
}

#[test]
fn rotation_signal_survives_an_empty_poll_between_truncate_and_new_head() {
    // [U] the file is truncated to zero and the new head is written on a
    // later poll: the rotation detected during the empty poll must latch, so
    // the new head is a Replace rather than an Append glued onto the previous
    // command's output.
    let mut h = new_harness();
    let call_id = "call-synth-rotated-empty";
    let log = h.terminal_dir.join(format!("{call_id}.log"));

    append_frame(&h.updates, synth_frame(5, call_id, "pending"));
    poll(&mut h);
    append_frame(&h.updates, synth_frame(7, call_id, "running"));
    let first_head = "a".repeat(100);
    fs::write(&log, &first_head).unwrap();
    assert_eq!(
        partial_mutations(&poll(&mut h)),
        vec![(3, Some(2), MutationOperation::Append, first_head)]
    );

    // Truncate to zero; this poll detects rotation but reads no bytes.
    fs::write(&log, "").unwrap();
    assert!(partial_mutations(&poll(&mut h)).is_empty());

    // The new command's head lands one poll later — still a Replace.
    fs::write(&log, "new-head\n").unwrap();
    assert_eq!(
        partial_mutations(&poll(&mut h)),
        vec![(
            4,
            Some(3),
            MutationOperation::Replace,
            "new-head\n".to_owned()
        )]
    );

    // Steady growth after the latched Replace is an Append delta again.
    append_bytes(&log, b"tail\n");
    assert_eq!(
        partial_mutations(&poll(&mut h)),
        vec![(5, Some(4), MutationOperation::Append, "tail\n".to_owned())]
    );
}

#[test]
fn safe_log_name_accepts_only_one_normal_component() {
    assert!(super::safe_log_name("call-spike-1789326032369250000"));
    assert!(super::safe_log_name("a.b_c-1"));
    for dangerous in [
        "",
        ".",
        "..",
        "../escape",
        "a/b",
        "a/../b",
        "/tmp/abs.log",
        "nested/dir/call",
        "nul\0id",
    ] {
        assert!(!super::safe_log_name(dangerous), "refused: {dangerous:?}");
    }
}

#[test]
fn real_fixture_emits_no_partials_and_keeps_d043_final() {
    let dir = tempfile::tempdir().unwrap();
    let session_dir = dir.path().join("sess");
    fs::create_dir_all(&session_dir).unwrap();
    // Deliberately no `terminal/` directory: the capture ships no log files.
    fs::write(session_dir.join("updates.jsonl"), REAL_UPDATES).unwrap();
    fs::write(session_dir.join("events.jsonl"), REAL_EVENTS).unwrap();
    let mut adapter = GrokAdapter::new(AdapterHome {
        home: dir.path().to_path_buf(),
        cwd: dir.path().to_path_buf(),
        pid: None,
    });
    adapter.bind_session_dir(SESSION_ID, session_dir);
    let mut live = GrokLive::new(adapter, identity());

    let mut observed = Vec::new();
    for _ in 0..3 {
        observed.extend(live.poll().unwrap());
    }

    assert!(
        stage_texts(&observed, ResultStage::Partial).is_empty(),
        "no terminal logs ship with the 1.0.30 fixture"
    );
    let final_result = observed
        .iter()
        .find_map(|observation| match &observation.payload {
            ObservationPayload::ToolResult(result)
                if result.stage == ResultStage::Final
                    && observation.item_id.as_deref() == Some(REAL_CALL_ID) =>
            {
                Some(&**result)
            }
            _ => None,
        })
        .expect("fixture call final result");
    assert_eq!(final_result.mutation.revision.0, 3);
    assert_eq!(
        final_result.mutation.base_revision.map(|base| base.0),
        Some(2)
    );
    assert_eq!(result_text(final_result), "exit: 0\n");
    assert_eq!(final_result.outcome, ToolOutcome::Succeeded);
}
