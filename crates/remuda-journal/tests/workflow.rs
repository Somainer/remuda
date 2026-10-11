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

/// c-cardsettle-fu2 r1 item 2: TWO agents sharing key k. The journal folds
/// started(k,a0) → failed(a0) → started(k,a1); a1 is a member that NEVER
/// consumed a journal start before (its placeholder is created by the failed
/// fold with consumed_journal_start=false), and the shared key's counter is
/// already 1 from a0, so a1's start counts as attempt 2. Without the
/// consumed-journal-start flag a1's failed member would be resurrected and
/// the OLDER journal format (only started/result records) never repairs it.
#[test]
fn a_shared_key_second_agent_without_a_start_stays_failed() {
    let tmp = TempDir::new().unwrap();
    let session = tmp.path().join("session");
    let run_id = "wf_fu2_shared";
    let run_dir = session.join("subagents/workflows").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("agent-a0.jsonl"), "{}\n").unwrap();
    std::fs::write(run_dir.join("agent-a1.jsonl"), "{}\n").unwrap();
    // a0 started (key counter 1), a0 failed, then a1's first start (key
    // counter 2) — but a1's member already exists as Failed from a prior
    // StopFailure (created out-of-band with no key/attempt 0).
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
    // a1 failed BEFORE any start for it (StopFailure beat the fold): create
    // the attempt-0 placeholder.
    tailer
        .agent_failed("a1", Some(&run_dir.join("agent-a1.jsonl")))
        .unwrap();
    // Now poll: the catch-up folds all three journal lines.
    let bodies = bodies(tailer.poll().unwrap());
    let a1_states: Vec<_> = bodies
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
        a1_states.contains(&WorkflowState::Failed),
        "a1 must be observed Failed: {a1_states:?}"
    );
    assert!(
        !a1_states.contains(&WorkflowState::Running),
        "a1 never consumed a start; the shared-key attempt-2 must not resurrect it: {a1_states:?}"
    );
}
