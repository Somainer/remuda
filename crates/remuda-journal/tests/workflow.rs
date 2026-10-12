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

/// c-cardsettle-fu2 r8 item 1: a start torn BEFORE its identity resolves
/// (the bytes stop after `...,"agentId` — key present, colon/value gone)
/// must already carry the pending boundary; the completed record stays the
/// failed attempt once the rest of its JSON lands. Splits covered here:
/// before the identity key, before its colon, inside its value.
fn a1_state_after(bodies: &[ObservationPayload]) -> Option<WorkflowState> {
    bodies.iter().rev().find_map(|body| match body {
        ObservationPayload::WorkflowMember(m)
            if m.native_agent_id == Knowledge::Known { value: "a1".into() } =>
        {
            Some(m.state)
        }
        _ => None,
    })
}

fn r8_torn_identity_split(tag: &str, torn_prefix: &str, completion: &str) {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = format!("wf_fu2_r8_{tag}");
    let run_dir = session.join("subagents/workflows").join(&run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
    std::fs::write(
        run_dir.join("journal.jsonl"),
        "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n\
         {\"type\":\"failed\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n",
    )
    .unwrap();

    let mut tailer = WorkflowJournalTailer::new(&session, context("sess"));
    tailer
        .launch(WorkflowLaunch {
            run_id,
            task_id: Some("t1".into()),
            tool_call_id: Some("tc".into()),
            transcript_dir: Some(run_dir.clone()),
            script_path: None,
            script_source: None,
        })
        .unwrap();
    assert_eq!(
        a1_state_after(&bodies(tailer.poll().unwrap())),
        Some(WorkflowState::Failed),
        "attempt 1 ends Failed"
    );

    // Attempt 2's start, torn before the identity value resolves.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        write!(journal, "{torn_prefix}").unwrap();
    }
    tailer
        .agent_event("a1", true, Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    tailer
        .agent_failed("a1", Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();

    // Still torn: nothing folds, a1 stays Failed (an unchanged member may
    // not be re-emitted at all on this empty poll).
    let torn_poll: Vec<_> = bodies(tailer.poll().unwrap())
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
        torn_poll
            .iter()
            .all(|state| *state == WorkflowState::Failed),
        "the torn prefix folds nothing; a1 stays Failed: {torn_poll:?}"
    );

    // Complete exactly the record that was being written.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        write!(journal, "{completion}").unwrap();
    }
    let completed = bodies(tailer.poll().unwrap());
    assert_eq!(
        a1_state_after(&completed),
        Some(WorkflowState::Failed),
        "the start captured before its identity resolved is the settled attempt"
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
    assert_eq!(
        a1_state_after(&bodies(tailer.poll().unwrap())),
        Some(WorkflowState::Running),
        "a start after the covered record is a genuine retry"
    );
}

#[test]
fn a_start_torn_before_the_identity_key_stays_covered() {
    r8_torn_identity_split(
        "beforekey",
        "{\"type\":\"started\",\"key\":\"v2:k\",",
        "\"agentId\":\"a1\"}\n",
    );
}

#[test]
fn a_start_torn_before_the_identity_colon_stays_covered() {
    r8_torn_identity_split(
        "beforecolon",
        "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\"",
        ":\"a1\"}\n",
    );
}

#[test]
fn a_start_torn_inside_the_identity_value_stays_covered() {
    r8_torn_identity_split(
        "insidevalue",
        "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a",
        "1\"}\n",
    );
}

/// c-cardsettle-fu2 r8 item 2: a truncation ABOVE the torn record's start
/// (keeping one byte of it) must still retire the pending boundary the
/// moment the shrink is observed, before a DIFFERENT genuine retry that
/// reuses the position is folded.
#[test]
fn a_truncation_above_the_torn_start_invalidates_its_coverage() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_r8_trunc_above";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
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
        a1_state_after(&bodies(tailer.poll().unwrap())),
        Some(WorkflowState::Failed),
        "attempt 1 ends Failed"
    );

    // Attempt 2's start torn INSIDE its identity value.
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

    // Truncate ABOVE W: keep exactly one byte ("{") of the torn record. The
    // consumed prefix 0..W survives (open write handle, set_len).
    {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        file.set_len(w + 1).unwrap();
    }
    let shrink_poll: Vec<_> = bodies(tailer.poll().unwrap())
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
        shrink_poll
            .iter()
            .all(|state| *state == WorkflowState::Failed),
        "nothing folds from the one-byte remnant; a1 stays Failed: {shrink_poll:?}"
    );

    // A DIFFERENT genuine retry continues the surviving "{".
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(
            journal,
            "\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\",\"label\":\"genuine-retry\"}}"
        )
        .unwrap();
    }
    assert_eq!(
        a1_state_after(&bodies(tailer.poll().unwrap())),
        Some(WorkflowState::Running),
        "the shrink removed captured partial bytes above W: the retry is genuine"
    );
}

/// c-cardsettle-fu2 r8 item 3: another member's failure peek can be the
/// ONLY open that observes the truncation. After a1's pending boundary is
/// armed at W, the file is truncated to W and (with no poll in between) a
/// DIFFERENT member's StopFailure opens the shared journal; the genuine a1
/// retry is appended only afterwards and shares the captured torn prefix
/// byte-for-byte, differing just past the observed extent. The peek must
/// retire the boundary immediately, so the retry resurrects.
#[test]
fn another_members_failure_peek_observes_the_truncation() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_r8_other_peek";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
    std::fs::write(run_dir.join("agent-a2.jsonl"), "{}\n").unwrap();
    // a1 started+failed; a2 started and still running. All consumed, cursor W.
    let prefix = "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n                  {\"type\":\"failed\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n                  {\"type\":\"started\",\"key\":\"v2:k2\",\"agentId\":\"a2\"}\n";
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
        a1_state_after(&bodies(tailer.poll().unwrap())),
        Some(WorkflowState::Failed),
        "a1 attempt 1 ends Failed"
    );

    // a1's torn attempt-2 start at W, torn INSIDE the identity value (no
    // closing quote yet), so the genuine retry below can share every byte
    // observed at failure time.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        write!(
            journal,
            "{{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1"
        )
        .unwrap();
    }
    tailer
        .agent_event("a1", true, Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    tailer
        .agent_failed("a1", Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();

    // Truncate the unread torn tail back to W — with NO poll observing it.
    {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        file.set_len(w).unwrap();
    }
    // ANOTHER member's StopFailure is the first open after the truncation.
    tailer
        .agent_event("a2", true, Some(&run_dir.join("agent-a2.jsonl")))
        .unwrap();
    tailer
        .agent_failed("a2", Some(&run_dir.join("agent-a2.jsonl")))
        .unwrap();

    // Only now is the genuine retry appended: it shares the torn prefix and
    // differs just past the failure-time extent.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(
            journal,
            "{{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\",\
             \"label\":\"genuine-retry-after-observed-truncation\"}}"
        )
        .unwrap();
    }

    assert_eq!(
        a1_state_after(&bodies(tailer.poll().unwrap())),
        Some(WorkflowState::Running),
        "the other member's peek retired the boundary when it saw the shrink"
    );
}

/// c-cardsettle-fu2 r8 item 4: a replacement AFTER the failure peek must
/// not reuse the pending generation. The peek normalises a shorter torn file
/// to a fresh generation; before any poll, the file is replaced AGAIN (new
/// inode via atomic rename) by a different complete LONGER genuine retry.
#[test]
fn a_replacement_after_the_failure_peek_cannot_reuse_the_pending_generation() {
    fn replace_atomic(run_dir: &Path, contents: &str) {
        let journal = run_dir.join("journal.jsonl");
        let tmp = run_dir.join("journal.jsonl.tmp");
        std::fs::write(&tmp, contents).unwrap();
        std::fs::rename(&tmp, &journal).unwrap();
    }
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_r8_replace_after";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
    // Attempt 1: deliberately long so later replacement files are shorter
    // than the consumed cursor.
    replace_atomic(
        &run_dir,
        "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\",\"label\":\"the-first-long-attempt\"}\n\
         {\"type\":\"failed\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n",
    );

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
        a1_state_after(&bodies(tailer.poll().unwrap())),
        Some(WorkflowState::Failed),
        "attempt 1 ends Failed and is consumed"
    );

    // Replace with a SHORTER torn attempt-2 start at offset 0.
    replace_atomic(
        &run_dir,
        "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"",
    );
    tailer
        .agent_event("a1", true, Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    tailer
        .agent_failed("a1", Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();

    // Before any poll, replace the file AGAIN with a different, complete,
    // LONGER genuine retry (a new inode; length above the torn file's extent).
    replace_atomic(
        &run_dir,
        "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\",\"label\":\"genuine-retry-after-second-replacement\"}\n",
    );

    assert_eq!(
        a1_state_after(&bodies(tailer.poll().unwrap())),
        Some(WorkflowState::Running),
        "the second replacement starts its own generation; the retry is genuine"
    );
}

/// c-cardsettle-fu2 r9 item 2 (poll path): a complete attempt-2 start at W
/// is followed by a lone "{" at Z when the live StopFailure peeks. The peek
/// arms TWO independent covers: a complete record at W and a pending tail at
/// Z. Truncating only the trailing "{" (back to Z) must invalidate the
/// pending cover alone; the intact complete cover keeps the settled attempt
/// Failed when the poll folds it. (r8 coupled the complete record into the
/// pending boundary as a fallback and discarded both together.)
#[test]
fn truncating_the_lone_brace_keeps_the_intact_complete_cover_poll() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_r9_lone_brace_poll";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
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
        a1_state_after(&bodies(tailer.poll().unwrap())),
        Some(WorkflowState::Failed),
        "attempt 1 ends Failed"
    );

    // A COMPLETE attempt-2 start (terminated) at W, then a lone "{" at Z.
    let attempt2 = "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n";
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        write!(journal, "{attempt2}").unwrap();
        write!(journal, "{{").unwrap();
    }
    let z = w + attempt2.len() as u64;
    tailer
        .agent_event("a1", true, Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    tailer
        .agent_failed("a1", Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();

    // Drop ONLY the trailing "{" (set_len back to Z); the complete record at
    // W is intact. The next poll is the first open after the shrink.
    {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        file.set_len(z).unwrap();
    }
    let folded: Vec<_> = bodies(tailer.poll().unwrap())
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
        folded.iter().all(|state| *state == WorkflowState::Failed),
        "the intact complete cover survives the pending tail's truncation; the settled \
         attempt stays Failed: {folded:?}"
    );

    // A genuinely different retry appended at Z resurrects.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(
            journal,
            "{{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\",\"label\":\"genuine\"}}"
        )
        .unwrap();
    }
    assert_eq!(
        a1_state_after(&bodies(tailer.poll().unwrap())),
        Some(WorkflowState::Running),
        "the retry after the covered record is genuine"
    );
}

/// c-cardsettle-fu2 r9 item 2 (peek path): as above, but ANOTHER member's
/// StopFailure is the first open after the "{" truncation. Its peek must
/// invalidate only the pending cover and leave a1's complete cover armed, so
/// the settled attempt-2 start folds Failed before the genuine retry runs.
#[test]
fn truncating_the_lone_brace_keeps_the_intact_complete_cover_peek() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_r9_lone_brace_peek";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
    std::fs::write(run_dir.join("agent-a2.jsonl"), "{}\n").unwrap();
    let prefix = "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n\
{\"type\":\"failed\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n\
{\"type\":\"started\",\"key\":\"v2:k2\",\"agentId\":\"a2\"}\n";
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
        a1_state_after(&bodies(tailer.poll().unwrap())),
        Some(WorkflowState::Failed),
        "a1 attempt 1 ends Failed and the prefix is consumed"
    );

    let attempt2 = "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n";
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        write!(journal, "{attempt2}").unwrap();
        write!(journal, "{{").unwrap();
    }
    let z = w + attempt2.len() as u64;
    tailer
        .agent_event("a1", true, Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    tailer
        .agent_failed("a1", Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();

    // Truncate the lone "{" with NO poll observing it.
    {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        file.set_len(z).unwrap();
    }
    // Another member's failure peek is the first open after the shrink.
    tailer
        .agent_event("a2", true, Some(&run_dir.join("agent-a2.jsonl")))
        .unwrap();
    tailer
        .agent_failed("a2", Some(&run_dir.join("agent-a2.jsonl")))
        .unwrap();

    // A genuinely different retry lands at Z; the poll folds BOTH the intact
    // attempt-2 start (Failed) and the retry (Running) in order.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(
            journal,
            "{{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\",\"label\":\"genuine\"}}"
        )
        .unwrap();
    }
    let folded: Vec<_> = bodies(tailer.poll().unwrap())
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
        *folded.first().expect("an a1 observation while folding"),
        WorkflowState::Failed,
        "the settled attempt-2 start stays covered after the other member's peek: {folded:?}"
    );
    assert_eq!(
        *folded.last().expect("an a1 observation while folding"),
        WorkflowState::Running,
        "the different retry at Z is genuine: {folded:?}"
    );
}

/// c-cardsettle-fu2 r9 item 3: growth of a pending record observed on a later
/// poll must be tracked. At the failure peek the torn tail is just "{" at W.
/// The same write continues growing on the next poll (the cover grows with
/// it); after an empty poll the writer shrinks the file back to the original
/// extent (keeping "{"), the next poll observes that, and a DIFFERENT retry
/// then completes the surviving brace. Without growth tracking the boundary
/// still matches at W and the genuine retry is held Failed; with it, the
/// shrink through the grown extent invalidates the cover and the retry
/// resurrects.
#[test]
fn a_pending_record_that_grew_then_shrank_back_invalidates_its_cover() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_r9_grow_shrink";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
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
        a1_state_after(&bodies(tailer.poll().unwrap())),
        Some(WorkflowState::Failed),
        "attempt 1 ends Failed and is consumed through W"
    );

    // Failure peek sees only the lone "{" of attempt 2.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        write!(journal, "{{").unwrap();
    }
    tailer
        .agent_event("a1", true, Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    tailer
        .agent_failed("a1", Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();

    // The same trailing write GROWS on a later poll (still no newline).
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        write!(
            journal,
            "\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\""
        )
        .unwrap();
    }
    let grown_poll: Vec<_> = bodies(tailer.poll().unwrap())
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
        grown_poll
            .iter()
            .all(|state| *state == WorkflowState::Failed),
        "the grown record still folds nothing; a1 stays Failed: {grown_poll:?}"
    );

    // An empty poll (the writer is stalled mid-record).
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
        "nothing folds: {empty_poll:?}"
    );

    // Shrink back to the ORIGINAL extent: only "{" survives at W.
    {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        file.set_len(w + 1).unwrap();
    }
    let shrink_poll: Vec<_> = bodies(tailer.poll().unwrap())
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
        shrink_poll
            .iter()
            .all(|state| *state == WorkflowState::Failed),
        "the one-byte remnant folds nothing; a1 stays Failed: {shrink_poll:?}"
    );

    // A DIFFERENT genuine retry completes the surviving brace.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(
            journal,
            "\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\",\"label\":\"genuine-r9-grow\"}}"
        )
        .unwrap();
    }
    assert_eq!(
        a1_state_after(&bodies(tailer.poll().unwrap())),
        Some(WorkflowState::Running),
        "the shrink removed grown bytes: the cover is gone and the different retry is genuine"
    );
}

/// c-cardsettle-fu2 r9 item 4: a Complete cover must not survive an observed
/// truncation INTO its record. The complete attempt-2 start (no newline yet)
/// is covered at W; the writer truncates to W+1 (keeping "{"), the shrink is
/// observed, and the rest of a BYTE-IDENTICAL retry is appended afterwards.
/// r8 retired a Complete cover only once the file shrank to/below its start
/// offset, so the byte-identical rewrite matched the digest and stayed
/// Failed; the independent cover binds the whole covered byte range and is
/// retired as soon as any of those bytes is removed — sticky across a later
/// (other-member) peek — so the physically rewritten, byte-identical record
/// is a genuine retry and resurrects.
#[test]
fn a_byte_identical_retry_after_a_shrink_into_the_cover_is_genuine() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_r9_shrink_into_complete";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
    std::fs::write(run_dir.join("agent-a2.jsonl"), "{}\n").unwrap();
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
        a1_state_after(&bodies(tailer.poll().unwrap())),
        Some(WorkflowState::Failed),
        "attempt 1 ends Failed and is consumed through W"
    );

    // Complete attempt-2 start WITHOUT its newline.
    let attempt2 = "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}";
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        write!(journal, "{attempt2}").unwrap();
    }
    tailer
        .agent_event("a1", true, Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    tailer
        .agent_failed("a1", Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();

    // Truncate INTO the covered record: keep only "{" at W.
    {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        file.set_len(w + 1).unwrap();
    }
    let shrink_poll: Vec<_> = bodies(tailer.poll().unwrap())
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
        shrink_poll
            .iter()
            .all(|state| *state == WorkflowState::Failed),
        "the one-byte remnant folds nothing; a1 stays Failed for now: {shrink_poll:?}"
    );

    // A later OTHER-member failure peek must not re-arm a1's retired cover.
    tailer
        .agent_event("a2", true, Some(&run_dir.join("agent-a2.jsonl")))
        .unwrap();
    tailer
        .agent_failed("a2", Some(&run_dir.join("agent-a2.jsonl")))
        .unwrap();

    // Append the remainder of a BYTE-IDENTICAL genuine retry plus newline.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(journal, "{}", &attempt2[1..]).unwrap();
    }
    assert_eq!(
        a1_state_after(&bodies(tailer.poll().unwrap())),
        Some(WorkflowState::Running),
        "the covered bytes were physically rewritten after a shrink into them; the \
         byte-identical record is a genuine retry"
    );
}

/// c-cardsettle-fu2 r9 item 6: an agentId whose JSON escapes a quote (on disk
/// a\"1, real id a"1) must be decoded as serde_json decodes it when the
/// failure peek arms a cover over its torn start; r8 kept the backslash and
/// armed nothing, so the settled attempt folded as a fresh Running member.
#[test]
fn a_torn_start_with_an_escaped_agent_id_stays_covered() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_r9_escaped_id";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join(r#"agent-a"1.jsonl"#), "{}\n").unwrap();
    // Attempt 1: started then failed, agentId JSON-escaped on disk.
    std::fs::write(
        run_dir.join("journal.jsonl"),
        r#"{"type":"started","key":"v2:k","agentId":"a\"1"}"#.to_owned()
            + "\n"
            + r#"{"type":"failed","key":"v2:k","agentId":"a\"1"}"#
            + "\n",
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
                    if m.native_agent_id
                        == Knowledge::Known {
                            value: r#"a"1"#.into()
                        } =>
                {
                    Some(m.state)
                }
                _ => None,
            })
            .expect("escaped-id member observed"),
        WorkflowState::Failed,
        "attempt 1 ends Failed"
    );

    // Attempt 2's start torn INSIDE the JSON: full escaped agentId, no
    // closing brace or newline.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        journal
            .write_all(r#"{"type":"started","key":"v2:k","agentId":"a\"1""#.as_bytes())
            .unwrap();
    }
    tailer
        .agent_event(r#"a"1"#, true, Some(&run_dir.join(r#"agent-a"1.jsonl"#)))
        .unwrap();
    tailer
        .agent_failed(r#"a"1"#, Some(&run_dir.join(r#"agent-a"1.jsonl"#)))
        .unwrap();

    // Complete the settled record.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(journal, "}}").unwrap();
    }
    let covered: Vec<_> = bodies(tailer.poll().unwrap())
        .iter()
        .filter_map(|body| match body {
            ObservationPayload::WorkflowMember(m)
                if m.native_agent_id
                    == Knowledge::Known {
                        value: r#"a"1"#.into(),
                    } =>
            {
                Some(m.state)
            }
            _ => None,
        })
        .collect();
    assert!(
        covered.iter().all(|state| *state == WorkflowState::Failed),
        "the escaped-id start the peek covered stays Failed: {covered:?}"
    );

    // A genuinely different retry appended afterwards resurrects.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        journal
            .write_all(
                r#"{"type":"started","key":"v2:k","agentId":"a\"1","label":"genuine"}"#.as_bytes(),
            )
            .unwrap();
        journal.write_all(b"\n").unwrap();
    }
    let final_state = bodies(tailer.poll().unwrap())
        .iter()
        .rev()
        .find_map(|body| match body {
            ObservationPayload::WorkflowMember(m)
                if m.native_agent_id
                    == Knowledge::Known {
                        value: r#"a"1"#.into(),
                    } =>
            {
                Some(m.state)
            }
            _ => None,
        })
        .expect("escaped-id member observed after the retry");
    assert_eq!(final_state, WorkflowState::Running, "the retry is genuine");
}

/// c-cardsettle-fu2 r10 item 1 (poll path, cursor zero): after the prefix is
/// consumed the journal is REPLACED atomically (a rotation the failure peek
/// normalises to cursor zero) by a complete attempt-2 start plus a lone "{"
/// with no newline. The peek arms a complete cover at 0 and a pending cover at
/// Z. Truncating ONLY the "{" in place (same inode, cursor still 0) must retire
/// the pending cover alone, not rotate the generation and discard the intact
/// complete cover: the settled attempt-2 start folds Failed before the genuine
/// retry. (The old blanket offset-0 rule treated ANY unread-window shrink as a
/// rewrite and killed every cover.)
#[test]
fn a_lone_brace_truncated_after_a_cursor_zero_rotation_keeps_the_cover_poll() {
    fn replace_atomic(run_dir: &Path, contents: &str) {
        let journal = run_dir.join("journal.jsonl");
        let tmp = run_dir.join("journal.jsonl.tmp");
        std::fs::write(&tmp, contents).unwrap();
        std::fs::rename(&tmp, &journal).unwrap();
    }
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_r10_cursor0_poll";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
    let prefix = "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n\
{\"type\":\"failed\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n";
    std::fs::write(run_dir.join("journal.jsonl"), prefix).unwrap();

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
        a1_state_after(&bodies(tailer.poll().unwrap())),
        Some(WorkflowState::Failed),
        "attempt 1 ends Failed and is consumed"
    );

    // Rotate: replacement file with a complete attempt-2 start and a lone "{".
    let attempt2 = "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n";
    replace_atomic(&run_dir, &format!("{attempt2}{{"));
    tailer
        .agent_event("a1", true, Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    tailer
        .agent_failed("a1", Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();

    // Drop ONLY the trailing "{" in place (same inode); the poll is the first
    // open after the shrink and the cursor is anchored at zero.
    let z = attempt2.len() as u64;
    {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        file.set_len(z).unwrap();
    }
    let folded: Vec<_> = bodies(tailer.poll().unwrap())
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
        folded.iter().all(|state| *state == WorkflowState::Failed),
        "the intact complete cover survives the lone-brace truncation at cursor zero; the \
         settled attempt stays Failed: {folded:?}"
    );

    // A genuinely different retry appended at Z resurrects.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(
            journal,
            "{{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\",\"label\":\"genuine\"}}"
        )
        .unwrap();
    }
    assert_eq!(
        a1_state_after(&bodies(tailer.poll().unwrap())),
        Some(WorkflowState::Running),
        "the retry after the covered record is genuine"
    );
}

/// c-cardsettle-fu2 r10 item 1 (peek path, cursor zero): as above, but
/// ANOTHER member's failure peek is the first open after the in-place "{"
/// truncation. Its peek must not rotate the generation; the intact complete
/// cover of a1 stays armed so the settled attempt folds Failed first.
#[test]
fn a_lone_brace_truncated_after_a_cursor_zero_rotation_keeps_the_cover_peek() {
    fn replace_atomic(run_dir: &Path, contents: &str) {
        let journal = run_dir.join("journal.jsonl");
        let tmp = run_dir.join("journal.jsonl.tmp");
        std::fs::write(&tmp, contents).unwrap();
        std::fs::rename(&tmp, &journal).unwrap();
    }
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_r10_cursor0_peek";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
    std::fs::write(run_dir.join("agent-a2.jsonl"), "{}\n").unwrap();
    let prefix = "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n\
{\"type\":\"failed\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n\
{\"type\":\"started\",\"key\":\"v2:k2\",\"agentId\":\"a2\"}\n";
    std::fs::write(run_dir.join("journal.jsonl"), prefix).unwrap();

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
        a1_state_after(&bodies(tailer.poll().unwrap())),
        Some(WorkflowState::Failed),
        "attempt 1 ends Failed and the prefix is consumed"
    );

    let attempt2 = "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\"}\n";
    replace_atomic(&run_dir, &format!("{attempt2}{{"));
    tailer
        .agent_event("a1", true, Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    tailer
        .agent_failed("a1", Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();

    // Truncate the lone "{" with NO poll observing it.
    let z = attempt2.len() as u64;
    {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        file.set_len(z).unwrap();
    }
    // Another member's failure peek is the first open after the shrink.
    tailer
        .agent_event("a2", true, Some(&run_dir.join("agent-a2.jsonl")))
        .unwrap();
    tailer
        .agent_failed("a2", Some(&run_dir.join("agent-a2.jsonl")))
        .unwrap();

    // The genuine retry lands at Z; one poll folds BOTH the intact attempt-2
    // start (Failed) and the retry (Running), in order.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(
            journal,
            "{{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\",\"label\":\"genuine\"}}"
        )
        .unwrap();
    }
    let folded: Vec<_> = bodies(tailer.poll().unwrap())
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
        *folded.first().expect("an a1 observation while folding"),
        WorkflowState::Failed,
        "the settled attempt-2 start stays covered after the other member's peek: {folded:?}"
    );
    assert_eq!(
        *folded.last().expect("an a1 observation while folding"),
        WorkflowState::Running,
        "the different retry at Z is genuine: {folded:?}"
    );
}

/// c-cardsettle-fu2 r10 item 2: the failure peek captures a torn tail ending
/// in a CR (`{` followed by `\r`, raw CRLF in progress). The pending cover
/// must bind the EXACT raw bytes including the CR: when the writer then
/// overwrites the CR with a quote and completes a real start in place, the
/// covered byte changed, the cover is invalidated, and the completion is a
/// genuine retry. (Stripping the CR when arming left the cover over just "{"
/// at the same extent, so the rewrite passed byte validation and the genuine
/// start was wrongly held Failed.)
#[test]
fn rewriting_a_captured_trailing_cr_into_a_quote_revives_the_completion() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_r10_cr_rewrite";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
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
        a1_state_after(&bodies(tailer.poll().unwrap())),
        Some(WorkflowState::Failed),
        "attempt 1 ends Failed and is consumed through W"
    );

    // Torn tail at W: "{" then a raw CR.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        journal.write_all(b"{\r").unwrap();
    }
    tailer
        .agent_event("a1", true, Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    tailer
        .agent_failed("a1", Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();

    // Overwrite ONLY the CR with a quote (same length, same extent): the tail
    // is now `{"` at W. A poll observes the contradiction before any fold.
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        file.seek(SeekFrom::Start(w + 1)).unwrap();
        file.write_all(b"\"").unwrap();
    }
    let observed: Vec<_> = bodies(tailer.poll().unwrap())
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
        observed.iter().all(|state| *state == WorkflowState::Failed),
        "no record folds yet; a1 stays Failed: {observed:?}"
    );

    // Complete the record the rewritten bytes now actually start.
    {
        use std::io::Write;
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("journal.jsonl"))
            .unwrap();
        writeln!(
            journal,
            "type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\",\"label\":\"cr-genuine\"}}"
        )
        .unwrap();
    }
    assert_eq!(
        a1_state_after(&bodies(tailer.poll().unwrap())),
        Some(WorkflowState::Running),
        "the CR->quote rewrite changed a covered byte; the completion is genuine"
    );
}
