//! Workflow run producer tests against the spike's real recorded run dirs.
//!
//! Fixtures under `tests/fixtures/workflow/` are the exact files captured from
//! claude 2.1.221 (`docs/design/evidence/workflow-progress-signals-1.md`):
//! `runs-221/` is the real 4-agent two-phase run plus the run-level failure
//! run; the 2.1.270-shaped run is assembled here from the same real agent
//! transcripts, with 2.1.270 journal/meta shapes (`label`/`phase` on
//! `started`, `description`/`workflowPhase`/`model` in meta).

use remuda_journal::{MapContext, WorkflowJournalTailer, WorkflowLaunch};
use remuda_protocol::{
    HostId, Id, InstanceId, Knowledge, ObservationPayload, RunId, SourceChannel, WorkflowState,
};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn context(session: &str) -> MapContext {
    let mut ctx = MapContext::claude_file(
        InstanceId::new(),
        Id::new("obj").unwrap(),
        HostId::new(),
        session,
        SourceChannel::WorkflowJournal,
    );
    ctx.run_id = Some(RunId::new());
    ctx
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workflow")
}

/// Copy the recorded tree into a writable temp dir so tests can append lines.
fn copy_run(tag: &str) -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let wf_root = session.join("subagents/workflows");
    fs::create_dir_all(&wf_root).unwrap();
    copy_dir(&fixtures().join("runs-221").join(tag), &wf_root.join(tag));
    // Script copy.
    let scripts = session.join("workflows/scripts");
    fs::create_dir_all(&scripts).unwrap();
    fs::copy(
        fixtures().join("scripts/spike-wf-1.js"),
        scripts.join(format!("spike-wf-1-{tag}.js")),
    )
    .unwrap();
    (tmp, session)
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
    }
}

fn bodies(envelopes: Vec<remuda_journal::Envelope>) -> Vec<ObservationPayload> {
    envelopes.into_iter().map(|env| env.body).collect()
}

#[test]
fn launch_emits_a_running_run_snapshot_synchronously() {
    let (_tmp, session) = copy_run("wf_c3422384-cb1");
    let mut tailer = WorkflowJournalTailer::new(&session, context("sess"));
    let out = tailer
        .launch(WorkflowLaunch {
            run_id: "wf_c3422384-cb1".into(),
            task_id: Some("wn1eumvco".into()),
            tool_call_id: Some("toolu_wf".into()),
            transcript_dir: Some(session.join("subagents/workflows/wf_c3422384-cb1")),
            script_path: None,
            script_source: None,
        })
        .unwrap();
    let run = out
        .iter()
        .find_map(|env| match &env.body {
            ObservationPayload::WorkflowRun(run) => Some(run),
            _ => None,
        })
        .expect("one running run snapshot");
    use remuda_protocol::Knowledge;
    assert_eq!(run.state, WorkflowState::Running);
    assert!(matches!(run.name, Some(Knowledge::Known { ref value }) if value == "spike-wf-1"));
    let totals = run.totals.as_ref().unwrap();
    assert!(totals.total_known);
    assert_eq!(totals.agents_total.0, 4);
    // Seeded from the script: both phases are queued.
    let phase_states: Vec<_> = out
        .iter()
        .filter_map(|env| match &env.body {
            ObservationPayload::WorkflowPhase(phase) => Some(phase),
            _ => None,
        })
        .collect();
    assert_eq!(phase_states.len(), 2);
    assert!(
        phase_states
            .iter()
            .all(|phase| phase.state == WorkflowState::Queued)
    );
}

#[test]
fn the_recorded_221_run_folds_real_members_with_model_tokens_and_phases() {
    let (_tmp, session) = copy_run("wf_c3422384-cb1");
    let mut tailer = WorkflowJournalTailer::new(&session, context("sess"));
    tailer
        .launch(WorkflowLaunch {
            run_id: "wf_c3422384-cb1".into(),
            task_id: Some("wn1eumvco".into()),
            tool_call_id: Some("toolu_wf".into()),
            transcript_dir: Some(session.join("subagents/workflows/wf_c3422384-cb1")),
            script_path: None,
            script_source: None,
        })
        .unwrap();
    let bodies = bodies(tailer.poll().unwrap());

    // 2.1.221 journal: 4 started + 4 result.
    let members: Vec<_> = bodies
        .iter()
        .filter_map(|body| match body {
            ObservationPayload::WorkflowMember(member) => Some(member),
            _ => None,
        })
        .collect();
    let completed: Vec<_> = members
        .iter()
        .filter(|member| member.state == WorkflowState::Completed)
        .collect();
    assert_eq!(completed.len(), 4, "all four agents completed");

    // Label/phase recovered from the script join even though 2.1.221 emits
    // neither: every member carries a phase and a non-degraded label.
    use remuda_protocol::Knowledge;
    for member in &completed {
        assert!(member.phase_id.is_some(), "{}", "phase joined from script");
        let label = match &member.label {
            Knowledge::Known { value } => value.clone(),
            other => panic!("known label, got {other:?}"),
        };
        assert!(
            label.contains("alpha:") || label.contains("beta:"),
            "script label, got {label}"
        );
        assert!(member.duration_ms.is_some_and(|ms| ms.0 > 0));
        assert!(member.calls.is_some());
        // c-wfcard: the three real timestamps ride every completed snapshot.
        assert!(member.started_at.is_some(), "started_at from agent jsonl");
        assert!(member.ended_at.is_some(), "ended_at for a stopped member");
        assert!(
            member.last_progress_at.is_some(),
            "last_progress_at from the newest transcript line"
        );
    }

    // The real transcript carries model + token usage for the agents that ran.
    let with_tokens = completed
        .iter()
        .filter(|member| member.tokens.is_some())
        .count();
    assert!(with_tokens >= 3, "tokens recovered from agent jsonl");

    // Totals reconcile: 4/4 done, zero running, completion run snapshot.
    let run = bodies
        .iter()
        .rev()
        .find_map(|body| match body {
            ObservationPayload::WorkflowRun(run) => Some(run),
            _ => None,
        })
        .unwrap();
    assert_eq!(run.totals.as_ref().unwrap().agents_done.0, 4);
    assert_eq!(run.totals.as_ref().unwrap().agents_running.0, 0);
    assert!(run.note.is_none(), "real data never carries the note");
    // c-wfcard: the launch instant rides the run snapshot for queue waits.
    assert!(
        run.launched_at.is_some(),
        "launched_at from the launch fold"
    );
}

#[test]
fn task_notification_marks_the_run_completed_with_a_summary() {
    let (tmp, session) = copy_run("wf_c3422384-cb1");
    let transcript = tmp.path().join("main.jsonl");
    fs::write(&transcript, b"pre-existing line stays unread\n").unwrap();
    let mut tailer = WorkflowJournalTailer::new(&session, context("sess"));
    tailer.set_main_transcript(&transcript).unwrap();
    tailer
        .launch(WorkflowLaunch {
            run_id: "wf_c3422384-cb1".into(),
            task_id: Some("wn1eumvco".into()),
            tool_call_id: Some("toolu_wf".into()),
            transcript_dir: Some(session.join("subagents/workflows/wf_c3422384-cb1")),
            script_path: None,
            script_source: None,
        })
        .unwrap();
    tailer.poll().unwrap();

    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap();
    writeln!(
        file,
        "{{\"type\":\"user\",\"message\":{{\"content\":\"<task-notification>\\n<task-id>wn1eumvco</task-id>\\n<tool-use-id>toolu_wf</tool-use-id>\\n<status>completed</status>\\n<summary>Dynamic workflow \\\"spike\\\" completed</summary>\\n</task-notification>\"}}}}"
    )
    .unwrap();
    let bodies = bodies(tailer.poll().unwrap());
    let run = bodies
        .iter()
        .find_map(|body| match body {
            ObservationPayload::WorkflowRun(run) if run.state == WorkflowState::Completed => {
                Some(run)
            }
            _ => None,
        })
        .expect("completed run snapshot");
    use remuda_protocol::Knowledge;
    let summary = match &run.live.as_ref().unwrap().summary {
        Knowledge::Known { value } => value.clone(),
        other => panic!("known summary, got {other:?}"),
    };
    assert!(summary.contains("completed"));
}

#[test]
fn a_270_shaped_run_uses_native_label_phase_model_and_failed_state() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_270-test";
    let run_dir = session.join("subagents/workflows").join(run_id);
    fs::create_dir_all(&run_dir).unwrap();
    // Real agent transcript, renamed to a 270 agent id.
    let real = fixtures().join("runs-221/wf_c3422384-cb1/agent-aae139d44933cefe2.jsonl");
    fs::copy(real, run_dir.join("agent-deadbeefcafe1234.jsonl")).unwrap();
    fs::write(
        run_dir.join("agent-deadbeefcafe1234.meta.json"),
        r#"{"agentType":"workflow-subagent","description":"alpha:one","workflowPhase":"Alpha","spawnDepth":1,"model":"opus"}"#,
    )
    .unwrap();
    fs::write(
        run_dir.join("journal.jsonl"),
        "{\"type\":\"launched\"}\n{\"type\":\"started\",\"key\":\"v2:abc\",\"agentId\":\"deadbeefcafe1234\",\"label\":\"alpha:one\",\"phase\":\"Alpha\"}\n{\"type\":\"failed\",\"key\":\"v2:abc\",\"agentId\":\"deadbeefcafe1234\"}\n",
    )
    .unwrap();
    fs::create_dir_all(session.join("workflows/scripts")).unwrap();
    fs::copy(
        fixtures().join("scripts/spike-wf-1.js"),
        session
            .join("workflows/scripts")
            .join(format!("spike-wf-1-{run_id}.js")),
    )
    .unwrap();

    let mut tailer = WorkflowJournalTailer::new(&session, context("sess"));
    tailer
        .launch(WorkflowLaunch {
            run_id: run_id.into(),
            task_id: Some("wnx".into()),
            tool_call_id: Some("toolu_270".into()),
            transcript_dir: Some(run_dir.clone()),
            script_path: None,
            script_source: None,
        })
        .unwrap();
    let bodies = bodies(tailer.poll().unwrap());
    let member = bodies
        .iter()
        .rev()
        .find_map(|body| match body {
            ObservationPayload::WorkflowMember(member) => Some(member),
            _ => None,
        })
        .expect("a folded member");
    use remuda_protocol::Knowledge;
    assert_eq!(member.state, WorkflowState::Failed);
    assert!(
        matches!(&member.label, Knowledge::Known { value } if value == "alpha:one"),
        "native 2.1.270 label used"
    );
    assert!(member.phase_id.is_some());
    // Model from meta wins until the transcript reports a fuller model id.
    assert!(member.model_resolved.is_known());
}

#[test]
fn dynamic_prompt_agents_degrade_to_short_code_with_no_note() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_dyn";
    let run_dir = session.join("subagents/workflows").join(run_id);
    fs::create_dir_all(&run_dir).unwrap();
    let real = fixtures().join("runs-221/wf_c3422384-cb1/agent-aae139d44933cefe2.jsonl");
    fs::copy(real, run_dir.join("agent-aaaaaaaa11111111.jsonl")).unwrap();
    fs::write(
        run_dir.join("journal.jsonl"),
        "{\"type\":\"started\",\"key\":\"v2:dyn\",\"agentId\":\"aaaaaaaa11111111\"}\n",
    )
    .unwrap();
    // A script whose call site prompt can never equal the real first record.
    fs::create_dir_all(session.join("workflows/scripts")).unwrap();
    fs::write(
        session
            .join("workflows/scripts")
            .join(format!("dyn-{run_id}.js")),
        "export const meta = { name: 'dyn', description: 'd', phases: [{ title: 'Only' }] }\nawait agent('something that never matches', { label: 'x', phase: 'Only' })\n",
    )
    .unwrap();
    let mut tailer = WorkflowJournalTailer::new(&session, context("sess"));
    tailer
        .launch(WorkflowLaunch {
            run_id: run_id.into(),
            ..Default::default()
        })
        .unwrap();
    let bodies = bodies(tailer.poll().unwrap());
    let member = bodies
        .iter()
        .find_map(|body| match body {
            ObservationPayload::WorkflowMember(member) => Some(member),
            _ => None,
        })
        .unwrap();
    use remuda_protocol::Knowledge;
    let label = match &member.label {
        Knowledge::Known { value } => value.clone(),
        other => panic!("known short code, got {other:?}"),
    };
    assert_eq!(label, "aaaaaaaa", "unmatched agent degrades to short code");
    assert!(member.phase_id.is_none(), "no invented phase");
    let run = bodies
        .iter()
        .find_map(|body| match body {
            ObservationPayload::WorkflowRun(run) => Some(run),
            _ => None,
        })
        .unwrap();
    assert!(
        run.note.is_none(),
        "degraded members still show the card, no note"
    );
}

#[test]
fn idle_polls_emit_nothing() {
    let (_tmp, session) = copy_run("wf_c3422384-cb1");
    let mut tailer = WorkflowJournalTailer::new(&session, context("sess"));
    tailer
        .launch(WorkflowLaunch {
            run_id: "wf_c3422384-cb1".into(),
            ..Default::default()
        })
        .unwrap();
    tailer.poll().unwrap();
    assert!(tailer.poll().unwrap().is_empty(), "no polling chatter");
    assert!(tailer.poll().unwrap().is_empty());
}

trait KnownExt {
    fn is_known(&self) -> bool;
}
impl<T> KnownExt for remuda_protocol::Knowledge<T> {
    fn is_known(&self) -> bool {
        matches!(self, remuda_protocol::Knowledge::Known { .. })
    }
}

/// c-cardsettle r4 item 5: `agent_failed` marks THAT workflow member failed
/// (the owner sees which agent failed and can retry); the run row itself is a
/// separate concern. Builds a minimal registered run with one member.
#[test]
fn agent_failed_marks_only_that_workflow_member() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_failed";
    let run_dir = session.join("subagents/workflows").join(run_id);
    fs::create_dir_all(&run_dir).unwrap();
    // A minimal agent transcript so the run/member has an on-disk file.
    fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();

    let mut tailer = WorkflowJournalTailer::new(&session, context("sess"));
    tailer
        .launch(WorkflowLaunch {
            run_id: run_id.into(),
            task_id: Some("task1".into()),
            tool_call_id: Some("toolu_wf".into()),
            transcript_dir: Some(run_dir.clone()),
            script_path: None,
            script_source: None,
        })
        .unwrap();
    tailer
        .agent_event("a1", true, Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();

    // The member starts non-failed.
    let before: Vec<_> = tailer
        .poll()
        .unwrap()
        .into_iter()
        .filter_map(|env| match env.body {
            ObservationPayload::WorkflowMember(m)
                if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
            {
                Some(m.state)
            }
            _ => None,
        })
        .collect();
    assert!(before.iter().all(|s| *s != WorkflowState::Failed));

    // A subagent StopFailure marks exactly that member failed.
    let out = tailer
        .agent_failed("a1", Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    let states: Vec<_> = out
        .into_iter()
        .filter_map(|env| match env.body {
            ObservationPayload::WorkflowMember(m)
                if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
            {
                Some(m.state)
            }
            _ => None,
        })
        .collect();
    assert!(
        states.contains(&WorkflowState::Failed),
        "the failed member must be reported Failed, got {states:?}"
    );
}

/// c-cardsettle-fu item 1: a StopFailure applied to a member before the
/// catch-up journal fold reaches its (still-unread) `started` record must not
/// be resurrected to running by that later-folded started.
#[test]
fn a_later_folded_started_never_downgrades_a_failed_member() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu_resurrect";
    let run_dir = session.join("subagents/workflows").join(run_id);
    fs::create_dir_all(&run_dir).unwrap();
    fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
    // The journal carries the member's `started` line but NO failure line: the
    // failure arrives out-of-band as a transcript StopFailure (live tailer).
    fs::write(
        run_dir.join("journal.jsonl"),
        "{\"type\":\"started\",\"key\":\"v2:fu\",\"agentId\":\"a1\",\"label\":\"x\",\"phase\":\"P\"}\n",
    )
    .unwrap();
    fs::create_dir_all(session.join("workflows/scripts")).unwrap();

    let mut tailer = WorkflowJournalTailer::new(&session, context("sess"));
    tailer
        .launch(WorkflowLaunch {
            run_id: run_id.into(),
            task_id: Some("task1".into()),
            tool_call_id: Some("toolu_wf".into()),
            transcript_dir: Some(run_dir.clone()),
            script_path: None,
            script_source: None,
        })
        .unwrap();

    // StopFailure beats the journal catch-up: the member exists (created by
    // agent_failed) and is marked Failed without the started ever being folded.
    tailer
        .agent_failed("a1", Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();

    // NOW poll: catch-up folds the older, still-unread `started` line. Pre-fix
    // it flipped the member back to Running.
    let bodies = bodies(tailer.poll().unwrap());
    let final_state = bodies
        .iter()
        .rev()
        .find_map(|body| match body {
            ObservationPayload::WorkflowMember(m)
                if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
            {
                Some(m.state)
            }
            _ => None,
        })
        .expect("member observed after catch-up");
    assert_eq!(
        final_state,
        WorkflowState::Failed,
        "the later-folded started must not resurrect a Failed member"
    );
}

/// c-cardsettle-fu2 r1 item 1: a GENUINE retry — started(k,a1) → failed(a1) →
/// started(k,a1) again — resurrects the member to Running. The second started
/// is attempt 2 on an agent id that already consumed attempt 1, so it is a
/// real retry, not an out-of-order fold of the member's own first start.
#[test]
fn a_genuine_same_agent_retry_resurrects_a_failed_member() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_retry";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
    // attempt 1 started, then failed, then attempt 2 started — same key/agent.
    std::fs::write(
        run_dir.join("journal.jsonl"),
        "{\"type\":\"started\",\"key\":\"v2:r\",\"agentId\":\"a1\"}\n\
         {\"type\":\"failed\",\"key\":\"v2:r\",\"agentId\":\"a1\"}\n\
         {\"type\":\"started\",\"key\":\"v2:r\",\"agentId\":\"a1\"}\n",
    )
    .unwrap();

    let mut tailer = WorkflowJournalTailer::new(&session, context("sess"));
    tailer
        .launch(WorkflowLaunch {
            run_id: run_id.into(),
            task_id: Some("t1".into()),
            tool_call_id: Some("tc".into()),
            transcript_dir: Some(run_dir.clone()),
            script_path: None,
            script_source: None,
        })
        .unwrap();
    let bodies = bodies(tailer.poll().unwrap());
    let final_state = bodies
        .iter()
        .rev()
        .find_map(|body| match body {
            ObservationPayload::WorkflowMember(m)
                if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
            {
                Some(m.state)
            }
            _ => None,
        })
        .expect("member observed");
    assert_eq!(
        final_state,
        WorkflowState::Running,
        "a genuine same-agent retry (attempt 2) must resurrect the Failed member"
    );
}

/// c-cardsettle-fu2 r1 item 1: a StopFailure that beats its SubagentStart
/// (placeholder member with attempt 0, no key) must NOT be resurrected when
/// the failure's OWN first `started` (attempt 1) is folded — only a started
/// after the agent id already consumed a start counts as a retry.
#[test]
fn a_failure_placeholder_own_first_start_does_not_resurrect() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_placeholder";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a2.jsonl"), "{}\n").unwrap();
    // Only one started (the failure's own first); no failure journal line —
    // the failure arrives out-of-band before the run registers.
    std::fs::write(
        run_dir.join("journal.jsonl"),
        "{\"type\":\"started\",\"key\":\"v2:p\",\"agentId\":\"a2\"}\n",
    )
    .unwrap();
    std::fs::create_dir_all(session.join("workflows/scripts")).unwrap();

    let mut tailer = WorkflowJournalTailer::new(&session, context("sess"));
    tailer
        .launch(WorkflowLaunch {
            run_id: run_id.into(),
            task_id: Some("t1".into()),
            tool_call_id: Some("tc".into()),
            transcript_dir: Some(run_dir.clone()),
            script_path: None,
            script_source: None,
        })
        .unwrap();
    // Out-of-band StopFailure BEFORE the SubagentStart/journal fold creates
    // the attempt-0 placeholder and marks it Failed.
    tailer
        .agent_failed("a2", Some(&run_dir.join("agent-a2.jsonl")))
        .unwrap();
    // Poll folds the single first started (attempt 1 > placeholder 0): this
    // is the failure's OWN first start, not a retry, so it stays Failed.
    let bodies = bodies(tailer.poll().unwrap());
    let states: Vec<_> = bodies
        .iter()
        .filter_map(|body| match body {
            ObservationPayload::WorkflowMember(m) => Some(m.state),
            _ => None,
        })
        .collect();
    assert!(
        states.contains(&WorkflowState::Failed),
        "the failure placeholder must not be resurrected by its own first start: {states:?}"
    );
    assert!(
        !states.contains(&WorkflowState::Running),
        "no Running observation for the placeholder's own first start: {states:?}"
    );
}

/// c-cardsettle-fu2 r1 item 2 / r4 item 2: TWO agents sharing key k. The
/// regression MUST run through the path the bug was in — a member created by
/// the live SubagentStart hook (`agent_started`), not one synthesized by
/// `agent_failed`:
/// 1. the journal counts started(k,a0) (a0 then fails);
/// 2. a1's member is created by `agent_event(started=true)` — a hook
///    placeholder with no key, attempt 0, no journal start consumed;
/// 3. a1 then fails out-of-band (StopFailure);
/// 4. the catch-up poll folds a1's OWN first started(k,a1): the shared key's
///    counter is already 1 from a0, so this is attempt 2 — but a1 consumed no
///    start itself, so it must stay Failed;
/// 5. a subsequent started(k,a1) (attempt 3) IS a genuine same-agent retry
///    and resurrects a1 to Running.
#[test]
fn a_shared_key_second_agent_created_by_start_hook_stays_failed_then_retries() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_shared";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a0.jsonl"), "{}\n").unwrap();
    std::fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
    // a0 started (key counter 1), a0 failed, then a1's first start (key
    // counter 2 when it folds).
    std::fs::write(
        run_dir.join("journal.jsonl"),
        "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a0\"}\n\
         {\"type\":\"failed\",\"key\":\"v2:k\",\"agentId\":\"a0\"}\n\
         {\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n",
    )
    .unwrap();

    let mut tailer = WorkflowJournalTailer::new(&session, context("sess"));
    tailer
        .launch(WorkflowLaunch {
            run_id: run_id.into(),
            task_id: Some("t1".into()),
            tool_call_id: Some("tc".into()),
            transcript_dir: Some(run_dir.clone()),
            script_path: None,
            script_source: None,
        })
        .unwrap();
    // The live SubagentStart hook beats the catch-up fold: a1 is created by
    // agent_started (attempt 0, consumed_journal_start=false)…
    tailer
        .agent_event("a1", true, Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    // …and then a StopFailure marks it Failed before the journal catches up.
    tailer
        .agent_failed("a1", Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();

    // First poll: the catch-up folds all three journal lines. a1's own first
    // started bumps the shared key to 2 but resurrects nothing.
    let a1_states = |bodies: &[ObservationPayload]| {
        bodies
            .iter()
            .filter_map(|body| match body {
                ObservationPayload::WorkflowMember(m)
                    if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
                {
                    Some(m.state)
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let first = bodies(tailer.poll().unwrap());
    let first_states = a1_states(&first);
    assert!(
        first_states.contains(&WorkflowState::Failed),
        "a1 must be observed Failed after its own first shared-key start: {first_states:?}"
    );
    assert!(
        !first_states.contains(&WorkflowState::Running),
        "a1 never consumed a journal start; the shared-key attempt-2 must not resurrect it: {first_states:?}"
    );

    // A GENUINE same-agent retry: another started(k,a1) after the member
    // already consumed its first journal start (attempt 3) resurrects it.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(
            journal,
            "{{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}}"
        )
        .unwrap();
    }
    let second = bodies(tailer.poll().unwrap());
    let final_state = second
        .iter()
        .rev()
        .find_map(|body| match body {
            ObservationPayload::WorkflowMember(m)
                if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
            {
                Some(m.state)
            }
            _ => None,
        })
        .expect("a1 observed after the real retry");
    assert_eq!(
        final_state,
        WorkflowState::Running,
        "a genuine same-agent retry (attempt 3, after a1 consumed a start) must resurrect it"
    );
}

/// c-cardsettle-fu2 r5 item 2: a FAILED RETRY must not be resurrected by its
/// own delayed start record. Attempt 1 starts and fails via the journal;
/// attempt 2's `started` is appended but, BEFORE the next 250 ms journal
/// poll, the live SubagentStart + StopFailure for attempt 2 arrive (no
/// journal failed record yet). When the poll folds, the attempt-2 started is
/// NEWER than the recorded attempt 1 and the member already consumed a
/// start — so the r4 rule wrongly resurrected attempt 2's own failure to
/// Running. That start is bound to the failure; only a THIRD start appended
/// after the failure was observed is a genuine retry.
#[test]
fn a_failed_retry_is_not_resurrected_by_its_own_delayed_start() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_failed_retry";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
    // Attempt 1: started then failed, both folded on the first poll.
    std::fs::write(
        run_dir.join("journal.jsonl"),
        "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n\
         {\"type\":\"failed\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n",
    )
    .unwrap();

    let mut tailer = WorkflowJournalTailer::new(&session, context("sess"));
    tailer
        .launch(WorkflowLaunch {
            run_id: run_id.into(),
            task_id: Some("t1".into()),
            tool_call_id: Some("tc".into()),
            transcript_dir: Some(run_dir.clone()),
            script_path: None,
            script_source: None,
        })
        .unwrap();
    let first = bodies(tailer.poll().unwrap());
    let a1_failed_after_first = first
        .iter()
        .rev()
        .find_map(|body| match body {
            ObservationPayload::WorkflowMember(m)
                if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
            {
                Some(m.state)
            }
            _ => None,
        })
        .expect("a1 observed");
    assert_eq!(
        a1_failed_after_first,
        WorkflowState::Failed,
        "attempt 1 ends Failed"
    );

    // Attempt 2's start is written to the journal…
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(
            journal,
            "{{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}}"
        )
        .unwrap();
    }
    // …and BEFORE the next poll the live SubagentStart + StopFailure for
    // attempt 2 arrive (no journal failed record for attempt 2).
    tailer
        .agent_event("a1", true, Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    tailer
        .agent_failed("a1", Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();

    // The poll folds the attempt-2 started: it must NOT resurrect.
    let delayed = bodies(tailer.poll().unwrap());
    let a1_states: Vec<_> = delayed
        .iter()
        .filter_map(|body| match body {
            ObservationPayload::WorkflowMember(m)
                if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
            {
                Some(m.state)
            }
            _ => None,
        })
        .collect();
    assert!(
        a1_states
            .iter()
            .all(|state| *state == WorkflowState::Failed),
        "the failed retry's own delayed start must never resurrect it: {a1_states:?}"
    );

    // A genuinely newer start appended AFTER the failure was observed
    // (attempt 3) still resurrects.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(
            journal,
            "{{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}}"
        )
        .unwrap();
    }
    let third = bodies(tailer.poll().unwrap());
    let final_state = third
        .iter()
        .rev()
        .find_map(|body| match body {
            ObservationPayload::WorkflowMember(m)
                if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
            {
                Some(m.state)
            }
            _ => None,
        })
        .expect("a1 observed after the genuine retry");
    assert_eq!(
        final_state,
        WorkflowState::Running,
        "a third start after the failure is a genuine retry and resurrects a1"
    );
}

/// c-cardsettle-fu2 r6 item 1: the live failure's terminal marker must be a
/// per-member WATERMARK at the failed attempt's journal position, not a
/// boolean covering the whole next poll. Attempt 1 starts+folds to Failed;
/// attempt 2's complete start is on disk when the live StopFailure arrives
/// (watermark = attempt 2's record position); attempt 3's start is appended
/// AFTER the failure but BEFORE the next poll. Folding both starts in one
/// poll must hold attempt 2 Failed and then resurrect attempt 3 to Running.
#[test]
fn a_start_between_the_live_failure_and_the_poll_resurrects_in_that_poll() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_r6_watermark";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
    // Attempt 1: started then failed, both folded on the first poll.
    std::fs::write(
        run_dir.join("journal.jsonl"),
        "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n\
         {\"type\":\"failed\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n",
    )
    .unwrap();

    let mut tailer = WorkflowJournalTailer::new(&session, context("sess"));
    tailer
        .launch(WorkflowLaunch {
            run_id: run_id.into(),
            task_id: Some("t1".into()),
            tool_call_id: Some("tc".into()),
            transcript_dir: Some(run_dir.clone()),
            script_path: None,
            script_source: None,
        })
        .unwrap();
    assert_eq!(
        bodies(tailer.poll().unwrap())
            .iter()
            .rev()
            .find_map(|body| match body {
                ObservationPayload::WorkflowMember(m)
                    if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
                {
                    Some(m.state)
                }
                _ => None,
            })
            .expect("a1 observed"),
        WorkflowState::Failed,
        "attempt 1 ends Failed"
    );

    // Attempt 2's complete start lands before the live failure…
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(
            journal,
            "{{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}}"
        )
        .unwrap();
    }
    // …live SubagentStart + StopFailure for attempt 2 (the failure settles the
    // start already on disk, arming a watermark at ITS position)…
    tailer
        .agent_event("a1", true, Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    tailer
        .agent_failed("a1", Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    // …and attempt 3's complete start lands AFTER the failure but before any
    // poll folds either record.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(
            journal,
            "{{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}}"
        )
        .unwrap();
    }

    // One poll folds BOTH starts: attempt 2 held Failed, attempt 3 Running.
    let a1_states: Vec<_> = bodies(tailer.poll().unwrap())
        .iter()
        .filter_map(|body| match body {
            ObservationPayload::WorkflowMember(m)
                if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
            {
                Some(m.state)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        *a1_states.first().expect("an a1 observation while folding"),
        WorkflowState::Failed,
        "attempt 2's own start (at the watermark) must stay Failed: {a1_states:?}"
    );
    assert_eq!(
        *a1_states.last().expect("an a1 observation while folding"),
        WorkflowState::Running,
        "attempt 3 started AFTER the failure is a genuine retry, even in the same poll: \
         {a1_states:?}"
    );
    assert!(
        !a1_states[..a1_states.len() - 1].contains(&WorkflowState::Running),
        "nothing may resurrect until the past-watermark record folds: {a1_states:?}"
    );
}

/// c-cardsettle-fu2 r6 item 2: peek and poll must agree on record
/// completeness, and the live failure's terminal watermark must survive
/// EMPTY polls until the record it covers is actually consumed. Attempt 1
/// starts+folds to Failed; attempt 2's start JSON is written in full but
/// WITHOUT its terminating newline when the live SubagentStart +
/// StopFailure arrive. The first poll consumes nothing (poll reads only
/// newline-terminated records); the newline then lands and the next poll
/// folds the start. That start is the failure's own settled attempt, so the
/// member must stay Failed throughout. A later full attempt-3 start is a
/// genuine retry and resurrects.
#[test]
fn an_unterminated_start_at_failure_time_stays_covered_once_completed() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_r6_unterm";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
    // Attempt 1: started then failed, both folded on the first poll.
    std::fs::write(
        run_dir.join("journal.jsonl"),
        "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n\
         {\"type\":\"failed\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n",
    )
    .unwrap();

    let mut tailer = WorkflowJournalTailer::new(&session, context("sess"));
    tailer
        .launch(WorkflowLaunch {
            run_id: run_id.into(),
            task_id: Some("t1".into()),
            tool_call_id: Some("tc".into()),
            transcript_dir: Some(run_dir.clone()),
            script_path: None,
            script_source: None,
        })
        .unwrap();
    assert_eq!(
        bodies(tailer.poll().unwrap())
            .iter()
            .rev()
            .find_map(|body| match body {
                ObservationPayload::WorkflowMember(m)
                    if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
                {
                    Some(m.state)
                }
                _ => None,
            })
            .expect("a1 observed"),
        WorkflowState::Failed,
        "attempt 1 ends Failed"
    );

    // Attempt 2's start JSON lands in full, but WITHOUT its newline.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        write!(
            journal,
            "{{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}}"
        )
        .unwrap();
    }
    // Live SubagentStart + StopFailure for attempt 2 while the record is
    // still unterminated.
    tailer
        .agent_event("a1", true, Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    tailer
        .agent_failed("a1", Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();

    // First poll consumes nothing: the trailing segment has no newline. The
    // member must still be Failed.
    let empty_poll: Vec<_> = bodies(tailer.poll().unwrap())
        .iter()
        .filter_map(|body| match body {
            ObservationPayload::WorkflowMember(m)
                if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
            {
                Some(m.state)
            }
            _ => None,
        })
        .collect();
    assert!(
        empty_poll
            .iter()
            .all(|state| *state == WorkflowState::Failed),
        "the empty poll must not resurrect (and need not emit): {empty_poll:?}"
    );

    // The newline lands; the next poll folds the now-complete attempt-2
    // start, which the watermark still covers.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(journal).unwrap();
    }
    let completed: Vec<_> = bodies(tailer.poll().unwrap())
        .iter()
        .filter_map(|body| match body {
            ObservationPayload::WorkflowMember(m)
                if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
            {
                Some(m.state)
            }
            _ => None,
        })
        .collect();
    assert!(
        completed
            .iter()
            .all(|state| *state == WorkflowState::Failed),
        "the start that was only partially terminated at failure time is still the settled \
         attempt: {completed:?}"
    );

    // A genuinely newer attempt 3 appended after the watermark was consumed
    // resurrects.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(
            journal,
            "{{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}}"
        )
        .unwrap();
    }
    let retried = bodies(tailer.poll().unwrap());
    let final_state = retried
        .iter()
        .rev()
        .find_map(|body| match body {
            ObservationPayload::WorkflowMember(m)
                if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
            {
                Some(m.state)
            }
            _ => None,
        })
        .expect("a1 observed after attempt 3");
    assert_eq!(
        final_state,
        WorkflowState::Running,
        "attempt 3 after the covered start was consumed is a genuine retry"
    );
}

/// c-cardsettle-fu2 r7 item 1: binding the watermark to (generation, offset)
/// is defeated by an in-place truncate-and-reuse that does NOT bump the
/// generation. Attempt 1 starts+fails and is consumed through offset W;
/// attempt 2's start sits unread (without its newline) when the live
/// StopFailure arms the watermark at W; the writer truncates the torn tail
/// back to W (length stays at/after the cursor, prefix digest unchanged, so
/// no new generation) and writes a DIFFERENT start record at W. That record
/// must not be mistaken for the settled attempt: once the intervening poll
/// observes the discarded suffix, the watermark is retired (generation,
/// offset AND record digest all had to match) and the retry resurrects.
#[test]
fn a_truncated_and_reused_start_offset_is_not_the_settled_record() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_r7_trunc";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
    // Attempt 1: started then failed. Both lines are the consumed prefix; W
    // is the byte offset right after them.
    let prefix = "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n\
                  {\"type\":\"failed\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n";
    std::fs::write(run_dir.join("journal.jsonl"), prefix).unwrap();
    let w = prefix.len() as u64;

    let mut tailer = WorkflowJournalTailer::new(&session, context("sess"));
    tailer
        .launch(WorkflowLaunch {
            run_id: run_id.into(),
            task_id: Some("t1".into()),
            tool_call_id: Some("tc".into()),
            transcript_dir: Some(run_dir.clone()),
            script_path: None,
            script_source: None,
        })
        .unwrap();
    assert_eq!(
        bodies(tailer.poll().unwrap())
            .iter()
            .rev()
            .find_map(|body| match body {
                ObservationPayload::WorkflowMember(m)
                    if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
                {
                    Some(m.state)
                }
                _ => None,
            })
            .expect("a1 observed"),
        WorkflowState::Failed,
        "attempt 1 ends Failed"
    );

    // Attempt 2's torn start: complete JSON, no newline yet.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        write!(
            journal,
            "{{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}}"
        )
        .unwrap();
    }
    // Live failure settles the unread attempt 2.
    tailer
        .agent_event("a1", true, Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    tailer
        .agent_failed("a1", Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    // First poll consumes nothing (no newline); the watermark stays armed.
    assert!(
        bodies(tailer.poll().unwrap())
            .iter()
            .filter_map(|body| match body {
                ObservationPayload::WorkflowMember(m)
                    if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
                {
                    Some(m.state)
                }
                _ => None,
            })
            .all(|state| state == WorkflowState::Failed),
        "the torn start has not folded; a1 stays Failed"
    );

    // The writer truncates the unread torn tail back to W (no generation
    // bump: length W is not below cursor W and the consumed prefix is
    // unchanged); this poll makes the shrink observable.
    {
        // Shrink in place: open WITHOUT O_TRUNC so the consumed prefix bytes
        // 0..W survive (O_TRUNC + set_len would zero-fill them), then drop
        // only the unread suffix.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        file.set_len(w).unwrap();
    }
    assert!(
        bodies(tailer.poll().unwrap())
            .iter()
            .filter_map(|body| match body {
                ObservationPayload::WorkflowMember(m)
                    if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
                {
                    Some(m.state)
                }
                _ => None,
            })
            .all(|state| state == WorkflowState::Failed),
        "the truncated file folds nothing yet; a1 stays Failed"
    );

    // A genuinely new start record is written at the reused offset W; its
    // bytes differ from the settled attempt's record.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(
            journal,
            "{{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\",\"label\":\"retried\"}}"
        )
        .unwrap();
    }
    let final_state = bodies(tailer.poll().unwrap())
        .iter()
        .rev()
        .find_map(|body| match body {
            ObservationPayload::WorkflowMember(m)
                if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
            {
                Some(m.state)
            }
            _ => None,
        })
        .expect("a1 observed after the reused-offset start");
    assert_eq!(
        final_state,
        WorkflowState::Running,
        "a different record at the reused offset is a genuine retry, not the settled attempt"
    );
}

/// c-cardsettle-fu2 r7 item 2: the failure-time peek used an unchecked cached
/// offset/generation and only poll normalised rotation. After attempt 1 is
/// consumed, the journal is REPLACED by a shorter file holding only attempt
/// 2's complete start (length below the consumed offset). The live
/// SubagentStart + StopFailure then arrive before any poll observes the
/// replacement. The peek must normalise the rotation against the OPENED
/// file (fresh generation at offset 0) and bind the watermark to THAT
/// generation, so the poll which rotates the cursor folds attempt 2 as the
/// settled start (Failed), not as a genuine retry.
#[test]
fn a_replaced_shorter_journal_before_failure_is_normalised_by_the_peek() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_r7_replace";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
    // Attempt 1: a deliberately LONG started+failed prefix so the later
    // replacement file is shorter than the consumed cursor.
    std::fs::write(
        run_dir.join("journal.jsonl"),
        "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\",\"label\":\"the-first-long-attempt\"}\n\
         {\"type\":\"failed\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n",
    )
    .unwrap();

    let mut tailer = WorkflowJournalTailer::new(&session, context("sess"));
    tailer
        .launch(WorkflowLaunch {
            run_id: run_id.into(),
            task_id: Some("t1".into()),
            tool_call_id: Some("tc".into()),
            transcript_dir: Some(run_dir.clone()),
            script_path: None,
            script_source: None,
        })
        .unwrap();
    assert_eq!(
        bodies(tailer.poll().unwrap())
            .iter()
            .rev()
            .find_map(|body| match body {
                ObservationPayload::WorkflowMember(m)
                    if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
                {
                    Some(m.state)
                }
                _ => None,
            })
            .expect("a1 observed"),
        WorkflowState::Failed,
        "attempt 1 ends Failed and its prefix is consumed"
    );

    // Replace the journal in place with ONLY attempt 2's complete start —
    // shorter than the consumed offset — BEFORE the live failure (no poll in
    // between to normalise it).
    std::fs::write(
        run_dir.join("journal.jsonl"),
        "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n",
    )
    .unwrap();
    tailer
        .agent_event("a1", true, Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    tailer
        .agent_failed("a1", Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();

    // This poll is the first to see the replacement. The peek already
    // normalised the new generation and armed the watermark at offset 0.
    let replaced: Vec<_> = bodies(tailer.poll().unwrap())
        .iter()
        .filter_map(|body| match body {
            ObservationPayload::WorkflowMember(m)
                if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
            {
                Some(m.state)
            }
            _ => None,
        })
        .collect();
    assert!(
        replaced.iter().all(|state| *state == WorkflowState::Failed),
        "attempt 2's start in the replaced shorter file is the settled attempt: {replaced:?}"
    );

    // A genuinely newer attempt appended to the new file still resurrects.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(
            journal,
            "{{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}}"
        )
        .unwrap();
    }
    let final_state = bodies(tailer.poll().unwrap())
        .iter()
        .rev()
        .find_map(|body| match body {
            ObservationPayload::WorkflowMember(m)
                if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
            {
                Some(m.state)
            }
            _ => None,
        })
        .expect("a1 observed after attempt 3");
    assert_eq!(
        final_state,
        WorkflowState::Running,
        "a start appended after the settled attempt is a genuine retry in the new generation"
    );
}

/// c-cardsettle-fu2 r7 item 3 (r6 brief item 2 NOT MET): covering only a
/// newline-missing record is not enough. Attempt 2's start can be torn
/// INSIDE its JSON when the live failure arrives — bytes present up to the
/// agentId, with no closing brace and no newline. No poll consumes them, but
/// the peek must capture a generation-scoped PENDING boundary there; once
/// the record completes (closing brace + newline), its agent identity is
/// matched against the failing member and the settled attempt stays Failed.
#[test]
fn a_start_torn_inside_its_json_at_failure_time_stays_covered() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_r7_tornjson";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
    // Attempt 1: started then failed, both folded on the first poll.
    std::fs::write(
        run_dir.join("journal.jsonl"),
        "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n\
         {\"type\":\"failed\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n",
    )
    .unwrap();

    let mut tailer = WorkflowJournalTailer::new(&session, context("sess"));
    tailer
        .launch(WorkflowLaunch {
            run_id: run_id.into(),
            task_id: Some("t1".into()),
            tool_call_id: Some("tc".into()),
            transcript_dir: Some(run_dir.clone()),
            script_path: None,
            script_source: None,
        })
        .unwrap();
    assert_eq!(
        bodies(tailer.poll().unwrap())
            .iter()
            .rev()
            .find_map(|body| match body {
                ObservationPayload::WorkflowMember(m)
                    if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
                {
                    Some(m.state)
                }
                _ => None,
            })
            .expect("a1 observed"),
        WorkflowState::Failed,
        "attempt 1 ends Failed"
    );

    // Attempt 2's start, torn INSIDE the JSON: full agentId, no closing
    // brace, no newline.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        write!(
            journal,
            "{{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\""
        )
        .unwrap();
    }
    tailer
        .agent_event("a1", true, Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    tailer
        .agent_failed("a1", Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();

    // First poll consumes nothing — the record is still unparseable.
    assert!(
        bodies(tailer.poll().unwrap())
            .iter()
            .filter_map(|body| match body {
                ObservationPayload::WorkflowMember(m)
                    if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
                {
                    Some(m.state)
                }
                _ => None,
            })
            .all(|state| state == WorkflowState::Failed),
        "the torn JSON folds nothing; a1 stays Failed"
    );

    // Complete the record: closing brace + newline.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(journal, "}}").unwrap();
    }
    let completed: Vec<_> = bodies(tailer.poll().unwrap())
        .iter()
        .filter_map(|body| match body {
            ObservationPayload::WorkflowMember(m)
                if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
            {
                Some(m.state)
            }
            _ => None,
        })
        .collect();
    assert!(
        completed
            .iter()
            .all(|state| *state == WorkflowState::Failed),
        "the JSON-torn start is the settled attempt once completed: {completed:?}"
    );

    // A genuinely newer attempt appended afterwards resurrects.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(
            journal,
            "{{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}}"
        )
        .unwrap();
    }
    let final_state = bodies(tailer.poll().unwrap())
        .iter()
        .rev()
        .find_map(|body| match body {
            ObservationPayload::WorkflowMember(m)
                if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
            {
                Some(m.state)
            }
            _ => None,
        })
        .expect("a1 observed after attempt 3");
    assert_eq!(
        final_state,
        WorkflowState::Running,
        "a start after the covered record is a genuine retry"
    );
}
