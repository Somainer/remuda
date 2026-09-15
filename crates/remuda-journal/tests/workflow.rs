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
    HostId, Id, InstanceId, ObservationPayload, RunId, SourceChannel, WorkflowState,
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
