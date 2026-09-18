//! Workflow run producer.
//!
//! Follows one Claude session's `subagents/workflows/wf_*` run directories and
//! folds three on-disk sources into the timeline-card observations
//! (`workflow.run` / `workflow.phase` / `workflow.member`):
//!
//! 1. `journal.jsonl` — `started{key,agentId,label?,phase?}` / `result` /
//!    `failed` (2.1.270+ carries label/phase; 2.1.221 only key+agentId).
//! 2. `agent-<id>.meta.json` + `agent-<id>.jsonl` — model, summed usage,
//!    tool calls, timestamps, and the first user record (the prompt literal
//!    that joins the agent to a script call site).
//! 3. the script copy under `<session>/workflows/scripts/` — meta name /
//!    description / phases and the `agent('<prompt>', {label, phase})` call
//!    sites (see [`script`]).
//!
//! Run terminal state is not recorded under the run directory. It comes from
//! the main session transcript (`<task-notification>` / TaskStop), fed in via
//! [`WorkflowJournalTailer::set_main_transcript`], or from a hook-channel
//! decision via [`WorkflowJournalTailer::terminate`].
//!
//! Discipline: one observation per real transition. Journal lines are folded
//! one at a time (each line is a native transition); agent-transcript growth
//! (tokens, tool calls) is folded at most once per member per poll, and token
//! or elapsed-time movement alone never triggers an observation — they ride the
//! next real transition. Idle polls emit nothing.

mod script;

pub use script::{AgentCallSite, MatchedCall, WorkflowScript};

use crate::Error;
use crate::claude::{NativeIds, envelope as file_envelope, opaque};
use crate::envelope::Envelope;
use crate::source::{FileTail, MapContext, Source, SourceResume};
use crate::util::{known, parse_timestamp, timestamp_from_unix_ms, timestamp_now, unknown};
use remuda_protocol::{
    Completeness, FileCursor, ObservationPayload, OpaqueReason, SourceChannel, Timestamp, U64,
    WorkflowEngine, WorkflowLive, WorkflowMemberPayload, WorkflowPhasePayload, WorkflowRunPayload,
    WorkflowState, WorkflowTotals,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Polls after a terminal marker before a run stops being tailed. At a 250 ms
/// driver tick this is ~750 ms — enough to drain `result` lines flushed just
/// before the `<task-notification>` was injected.
const TERMINAL_GRACE_POLLS: u32 = 3;

/// Polls a launch waits for its run directory to appear before degrading with
/// a note. At 250 ms this is ~2 s; the harness creates the directory within
/// milliseconds of the PostToolUse response.
const MISSING_DIR_POLLS: u32 = 8;

/// An `agent-<id>.jsonl` larger than this is not parsed; the member degrades
/// to journal-only state. Run transcripts are small (single-purpose agents);
/// this only guards a runaway file.
const MAX_AGENT_JSONL: u64 = 8 * 1024 * 1024;

/// Hook-derived launch handle for one workflow run (PostToolUse(Workflow)
/// `tool_response` fields, verified on claude 2.1.221 — evidence
/// `workflow-progress-signals-1.md` §1.1).
#[derive(Debug, Clone, Default)]
pub struct WorkflowLaunch {
    /// `runId`, e.g. `wf_c3422384-cb1`.
    pub run_id: String,
    /// `taskId` (the background task id, e.g. `wn1eumvco`).
    pub task_id: Option<String>,
    /// Native Workflow tool call id (`tool_use_id`), the card's mount key.
    pub tool_call_id: Option<String>,
    /// `transcriptDir`; when absent it is `<session>/subagents/workflows/<run_id>`.
    pub transcript_dir: Option<PathBuf>,
    /// `scriptPath` — the script copy under `workflows/scripts/`.
    pub script_path: Option<PathBuf>,
    /// Script source straight from `tool_input.script`, used when the copy is
    /// not on disk yet.
    pub script_source: Option<String>,
}

/// Tails every `wf_*/journal.jsonl` under a Claude session directory and folds
/// the card observations for runs registered via [`Self::launch`].
pub struct WorkflowJournalTailer {
    session_dir: PathBuf,
    ctx: MapContext,
    /// One byte-offset tail per journal.jsonl (registered and discovered runs).
    tails: HashMap<PathBuf, FileTail>,
    ids: NativeIds,
    runs: Vec<RunState>,
    /// Main session transcript path. Every run adopts an end-anchored tail at
    /// registration, so a run launched minutes after the SessionStart bind
    /// never re-reads pre-launch transcript bytes.
    main_transcript_path: Option<PathBuf>,
    /// Evidence replay reads the transcript from offset zero.
    main_transcript_scan: bool,
}

/// A single followed run.
struct RunState {
    native_id: String,
    dir: PathBuf,
    task_id: Option<String>,
    tool_call_native: Option<String>,
    script: Option<WorkflowScript>,
    script_path: Option<PathBuf>,
    script_source: Option<String>,
    script_loaded: bool,
    main_transcript: Option<FileTail>,
    /// Terminal `(state, summary)` once proven; `None` while running.
    terminal: Option<(WorkflowState, Option<String>)>,
    /// Polls left after the terminal marker.
    grace: u32,
    finished: bool,
    members: Vec<MemberState>,
    /// `key` -> number of `started` records seen (attempt count).
    attempts: HashMap<String, u64>,
    /// Per-member observation revision counters.
    member_revs: HashMap<String, u64>,
    phases: Vec<PhaseState>,
    run_rev: u64,
    last_run_sig: Option<String>,
    /// Note carried by the last run envelope, so degrade/recover is a change.
    last_emitted_note: Option<String>,
    emitted_phase: HashMap<String, (WorkflowState, u64)>,
    emitted_member: HashMap<String, MemberSig>,
    /// Set when the run dir never became readable; clears once real data lands.
    note: Option<String>,
    missing_polls: u32,
    launched_ms: Option<u128>,
}

/// One member (one agent invocation; retries share the same key).
struct MemberState {
    agent_id: String,
    key: Option<String>,
    attempt: u64,
    state: WorkflowState,
    label: Option<String>,
    phase: Option<String>,
    prompt: Option<String>,
    matched_call: Option<usize>,
    model: Option<String>,
    latest_tool: Option<String>,
    calls: u64,
    tokens: Option<u64>,
    started_at: Option<Timestamp>,
    last_ts: Option<Timestamp>,
    jsonl: Option<FileTail>,
    meta_read: bool,
    stopped: bool,
}

/// Member fields whose change is worth an observation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MemberSig {
    state: WorkflowState,
    label: Option<String>,
    phase: Option<String>,
    model: Option<String>,
    latest_tool: Option<String>,
    calls: u64,
    tokens: Option<u64>,
    attempt: u64,
}

#[derive(Debug, Clone)]
struct PhaseState {
    title: String,
    state: WorkflowState,
}

impl WorkflowJournalTailer {
    /// `session_dir` is `<projects>/<enc>/<sid>/` (the directory that contains
    /// `subagents/`).
    pub fn new(session_dir: impl Into<PathBuf>, mut ctx: MapContext) -> Self {
        let ids = NativeIds::new(ctx.instance_id.as_id().as_str());
        ctx.channel = SourceChannel::WorkflowJournal;
        Self {
            session_dir: session_dir.into(),
            ctx,
            tails: HashMap::new(),
            ids,
            runs: Vec::new(),
            main_transcript_path: None,
            main_transcript_scan: false,
        }
    }

    /// Mapping context.
    pub fn context(&self) -> &MapContext {
        &self.ctx
    }

    /// Resume a single workflow journal file.
    pub fn restore_tail(&mut self, path: PathBuf, resume: SourceResume) {
        self.tails
            .insert(path.clone(), FileTail::from_resume(path, resume));
    }

    /// Current tails keyed by journal path.
    pub fn resumes(&self) -> Vec<SourceResume> {
        self.tails.values().map(FileTail::resume).collect()
    }

    /// Register a run from its PostToolUse(Workflow) hook and emit its first
    /// `workflow.run` observation immediately (synchronously — this is what
    /// keeps the first observation inside the launch budget).
    pub fn launch(&mut self, launch: WorkflowLaunch) -> Result<Vec<Envelope>, Error> {
        if launch.run_id.is_empty() || self.runs.iter().any(|run| run.native_id == launch.run_id) {
            return Ok(Vec::new());
        }
        let dir = launch
            .transcript_dir
            .clone()
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| {
                self.session_dir
                    .join("subagents")
                    .join("workflows")
                    .join(&launch.run_id)
            });
        let mut run = RunState::new(
            launch.run_id.clone(),
            dir.clone(),
            launch.task_id,
            launch.tool_call_id,
            launch.script_path,
            launch.script_source,
            system_ms(),
        );
        self.tails.insert(
            dir.join("journal.jsonl"),
            FileTail::new(dir.join("journal.jsonl"))?,
        );
        run.load_script(&self.session_dir);
        run.seed_script_phases();
        if let Some(path) = &self.main_transcript_path {
            run.main_transcript = Some(if self.main_transcript_scan {
                FileTail::new(path)?
            } else {
                FileTail::at_end(path)?
            });
        }
        self.runs.push(run);
        self.emit_snapshots()
    }

    /// Observe a SubagentStart/SubagentStop hook for a workflow agent.
    ///
    /// `agent_transcript_path` is `…/subagents/workflows/<run>/agent-<id>.jsonl`
    /// when the hook carried one (SubagentStop always does).
    pub fn agent_event(
        &mut self,
        agent_id: &str,
        started: bool,
        agent_transcript_path: Option<&Path>,
    ) -> Result<Vec<Envelope>, Error> {
        if agent_id.is_empty() {
            return Ok(Vec::new());
        }
        let run_id = match agent_transcript_path.and_then(run_id_from_transcript_path) {
            Some(run_id) => run_id,
            None => match self.run_for_agent(agent_id) {
                Some(run_id) => run_id,
                // SubagentStart on 2.1.221 carries no transcript path: the
                // harness has already written `agent-<id>.jsonl` under exactly
                // one run directory, so attribute by that on-disk file.
                None => match self.run_for_agent_file(agent_id) {
                    Some(run_id) => run_id,
                    None => return Ok(Vec::new()),
                },
            },
        };
        if self.runs.iter().all(|run| run.native_id != run_id) {
            // A SubagentStart beat the launch fold: register from its path.
            let dir = agent_transcript_path
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .filter(|path| path.is_absolute())
                .unwrap_or_else(|| {
                    self.session_dir
                        .join("subagents")
                        .join("workflows")
                        .join(&run_id)
                });
            self.register_run(run_id.clone(), dir);
            self.load_script_for(&run_id);
        }
        let index = self
            .runs
            .iter()
            .position(|run| run.native_id == run_id)
            .expect("run registered above");
        if started {
            self.runs[index].agent_started(agent_id, agent_transcript_path)?;
        } else {
            self.runs[index].agent_stopped(agent_id, agent_transcript_path);
        }
        self.emit_snapshots()
    }

    /// Tail the main session transcript for this run's terminal markers
    /// (`<task-notification>` / TaskStop). Only newly appended bytes are read,
    /// so pass the SessionStart-bound transcript path.
    pub fn set_main_transcript(&mut self, path: &Path) -> Result<(), Error> {
        self.main_transcript_path = Some(path.to_path_buf());
        self.main_transcript_scan = false;
        for run in &mut self.runs {
            if run.main_transcript.is_none() {
                run.main_transcript = Some(FileTail::at_end(path)?);
            }
        }
        Ok(())
    }

    /// Like [`Self::set_main_transcript`] but reads the transcript from its
    /// start. Used for evidence replay against an already-completed capture;
    /// the live path anchors at the end so pre-bind bytes are never re-read.
    pub fn set_main_transcript_scan(&mut self, path: &Path) -> Result<(), Error> {
        self.main_transcript_path = Some(path.to_path_buf());
        self.main_transcript_scan = true;
        for run in &mut self.runs {
            run.main_transcript = Some(FileTail::new(path)?);
        }
        Ok(())
    }

    /// Feed an externally proven terminal state (e.g. a TaskStop observed on
    /// the hook channel). `summary` becomes the card's terminal line.
    pub fn terminate(
        &mut self,
        run_or_task_id: &str,
        state: WorkflowState,
        summary: Option<String>,
    ) -> Result<Vec<Envelope>, Error> {
        for run in self
            .runs
            .iter_mut()
            .filter(|run| run.terminal.is_none())
            .filter(|run| {
                run.native_id == run_or_task_id || run.task_id.as_deref() == Some(run_or_task_id)
            })
        {
            run.terminal = Some((state, summary.clone()));
            run.grace = TERMINAL_GRACE_POLLS;
            // Settle unresolved members with the run before the snapshot.
            for member in &mut run.members {
                if matches!(member.state, WorkflowState::Running | WorkflowState::Queued) {
                    member.state = state;
                    member.stopped = true;
                }
            }
        }
        self.emit_snapshots()
    }

    /// Whether every followed run has drained its terminal grace.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        !self.runs.is_empty() && self.runs.iter().all(|run| run.finished)
    }

    /// Poll journal / transcript / agent files and fold every run.
    pub fn poll(&mut self) -> Result<Vec<Envelope>, Error> {
        self.refresh_tails()?;
        let mut out = Vec::new();

        // Journal lines are native transitions: fold one at a time and emit the
        // snapshot each line changes, preserving transition order on catch-up.
        for index in 0..self.runs.len() {
            let journal_path = self.runs[index].dir.join("journal.jsonl");
            let lines = match self.tails.get_mut(&journal_path) {
                Some(tail) => tail.poll()?,
                None => Vec::new(),
            };
            for (line, _) in lines {
                let line = line.clone();
                self.runs[index].fold_journal_line(&line);
                self.runs[index].refresh_agents()?;
                // Recompute phase state per journal line so a phase running
                // between two lines still emits its running revision (it is a
                // real transition even if a later line completes it).
                self.runs[index].recompute_phases();
                out.extend(self.run_envelopes(index)?);
            }
        }

        // Transcript terminal markers, agent jsonl growth, totals and grace.
        for index in 0..self.runs.len() {
            self.runs[index].poll_transcript()?;
            self.runs[index].refresh_agents()?;
            self.runs[index].recompute_phases();
            self.runs[index].update_note();
            self.runs[index].apply_terminal_members();
            if self.runs[index].terminal.is_some() && self.runs[index].grace > 0 {
                self.runs[index].grace -= 1;
            } else if self.runs[index].terminal.is_some() {
                self.runs[index].finished = true;
            }
            out.extend(self.run_envelopes(index)?);
        }
        Ok(out)
    }

    /// Append polled envelopes and persist per-file cursors.
    pub async fn ingest(&mut self, journal: &crate::Journal) -> Result<Vec<U64>, Error> {
        let instance = self.ctx.instance_id.clone();
        let envelopes = self.poll()?;
        let mut seqs = Vec::with_capacity(envelopes.len());
        for envelope in envelopes {
            seqs.push(journal.append(&instance, envelope).await?);
        }
        for resume in self.resumes() {
            journal.put_source_resume(&instance, resume).await?;
        }
        Ok(seqs)
    }

    fn run_for_agent(&self, agent_id: &str) -> Option<String> {
        self.runs
            .iter()
            .find(|run| run.members.iter().any(|member| member.agent_id == agent_id))
            .map(|run| run.native_id.clone())
    }

    /// Find the run that already has this agent's transcript on disk.
    ///
    /// `SubagentStart` on claude 2.1.221 does not name the run directory, but
    /// the harness writes `agent-<id>.jsonl` at spawn; prefer a registered run
    /// that contains it, else any run directory under the session tree.
    fn run_for_agent_file(&mut self, agent_id: &str) -> Option<String> {
        let file = format!("agent-{agent_id}.jsonl");
        if let Some(run) = self.runs.iter().find(|run| run.dir.join(&file).is_file()) {
            return Some(run.native_id.clone());
        }
        let root = self.session_dir.join("subagents").join("workflows");
        let found = fs::read_dir(&root).ok()?.flatten().find(|entry| {
            entry.path().join(&file).is_file()
                && entry.file_name().to_string_lossy().starts_with("wf_")
        })?;
        let native_id = found.file_name().to_string_lossy().into_owned();
        if self.runs.iter().all(|run| run.native_id != native_id) {
            let dir = found.path();
            self.register_run(native_id.clone(), dir);
            self.load_script_for(&native_id);
        }
        Some(native_id)
    }

    fn register_run(&mut self, native_id: String, dir: PathBuf) {
        if self.runs.iter().any(|run| run.native_id == native_id) {
            return;
        }
        let mut run = RunState::new(
            native_id.clone(),
            dir.clone(),
            None,
            None,
            None,
            None,
            system_ms(),
        );
        if let Some(path) = &self.main_transcript_path {
            let tail = if self.main_transcript_scan {
                FileTail::new(path)
            } else {
                FileTail::at_end(path)
            };
            if let Ok(tail) = tail {
                run.main_transcript = Some(tail);
            }
        }
        self.tails
            .entry(dir.join("journal.jsonl"))
            .or_insert_with(|| FileTail::new(dir.join("journal.jsonl")).expect("journal tail"));
        self.runs.push(run);
    }

    fn load_script_for(&mut self, run_id: &str) {
        if let Some(index) = self.runs.iter().position(|run| run.native_id == run_id) {
            let session_dir = self.session_dir.clone();
            self.runs[index].load_script(&session_dir);
            self.runs[index].seed_script_phases();
        }
    }

    fn refresh_tails(&mut self) -> Result<(), Error> {
        // Registered runs.
        for run in &self.runs {
            let path = run.dir.join("journal.jsonl");
            if path.is_file() && !self.tails.contains_key(&path) {
                self.tails.insert(path.clone(), FileTail::new(path)?);
            }
        }
        // File-only discovery: journals with no launch hook (promoted/unshimmed
        // sessions).
        for path in discover_journals(&self.session_dir)? {
            if self.tails.contains_key(&path) {
                continue;
            }
            self.tails
                .insert(path.clone(), FileTail::new(path.clone())?);
            if let Some(dir) = path.parent()
                && let Some(name) = dir.file_name()
            {
                let native_id = name.to_string_lossy().into_owned();
                if self.runs.iter().all(|run| run.native_id != native_id) {
                    self.register_run(native_id.clone(), dir.to_path_buf());
                    self.load_script_for(&native_id);
                }
            }
        }
        Ok(())
    }

    /// Diff one run's current snapshot and return only changed envelopes.
    fn emit_snapshots(&mut self) -> Result<Vec<Envelope>, Error> {
        for run in &mut self.runs {
            run.refresh_agents()?;
            run.recompute_phases();
            run.update_note();
            run.apply_terminal_members();
        }
        let mut out = Vec::new();
        for index in 0..self.runs.len() {
            out.extend(self.run_envelopes(index)?);
        }
        Ok(out)
    }

    fn run_envelopes(&mut self, index: usize) -> Result<Vec<Envelope>, Error> {
        let mut out = Vec::new();
        let (phases, members, run_snapshot) = {
            let run = &self.runs[index];
            let phases: Vec<(String, WorkflowState)> = run
                .phases
                .iter()
                .map(|phase| (phase.title.clone(), phase.state))
                .collect();
            let members: Vec<MemberSnapshot> =
                run.members.iter().map(MemberSnapshot::from_state).collect();
            let snapshot = RunSnapshot::build(run, &members);
            (phases, members, snapshot)
        };

        // Run snapshot.
        let sig = run_snapshot.signature();
        let note_changed = self.runs[index].last_emitted_note != self.runs[index].note;
        if self.runs[index].last_run_sig.as_deref() != Some(sig.as_str()) || note_changed {
            self.runs[index].last_run_sig = Some(sig);
            self.runs[index].last_emitted_note = self.runs[index].note.clone();
            self.runs[index].run_rev += 1;
            let revision = self.runs[index].run_rev;
            let envelope = self.build_run_envelope(index, &run_snapshot, revision)?;
            out.push(envelope);
        }

        // Phases.
        for (title, state) in phases {
            let previous = self.runs[index].emitted_phase.get(&title).cloned();
            match previous {
                None => {
                    let phase_id = self
                        .ids
                        .phase(format!("{}:phase:{title}", self.runs[index].native_id))?;
                    self.runs[index]
                        .emitted_phase
                        .insert(title.clone(), (state, 1));
                    out.push(self.build_phase_envelope(index, &phase_id, &title, state, 1)?);
                }
                Some((old, rev)) if old != state => {
                    let phase_id = self
                        .ids
                        .phase(format!("{}:phase:{title}", self.runs[index].native_id))?;
                    let rev = rev + 1;
                    self.runs[index]
                        .emitted_phase
                        .insert(title.clone(), (state, rev));
                    out.push(self.build_phase_envelope(index, &phase_id, &title, state, rev)?);
                }
                Some(_) => {}
            }
        }

        // Members.
        for snapshot in members {
            let sig = snapshot.sig();
            if self.runs[index].emitted_member.get(&snapshot.agent_id) == Some(&sig) {
                continue;
            }
            let rev = {
                let map = &mut self.runs[index].member_revs;
                let rev = map.entry(snapshot.agent_id.clone()).or_insert(0);
                *rev += 1;
                *rev
            };
            self.runs[index]
                .emitted_member
                .insert(snapshot.agent_id.clone(), sig);
            out.push(self.build_member_envelope(index, &snapshot, rev)?);
        }
        Ok(out)
    }

    fn build_run_envelope(
        &mut self,
        index: usize,
        snapshot: &RunSnapshot,
        revision: u64,
    ) -> Result<Envelope, Error> {
        let run = &self.runs[index];
        let workflow_id = self.ids.workflow(&run.native_id)?;
        let tool_call_id = match &run.tool_call_native {
            Some(native) => Some(self.ids.tool(native)?),
            None => None,
        };
        let (name, description, title) = match &run.script {
            Some(script) => (
                script.name.clone().map(known),
                script.description.clone().map(known),
                known(
                    script
                        .description
                        .clone()
                        .or_else(|| script.name.clone())
                        .unwrap_or_else(|| run.native_id.clone()),
                ),
            ),
            None => (None, None, unknown("not-emitted")),
        };
        let payload = WorkflowRunPayload {
            workflow_id,
            engine: WorkflowEngine::ClaudeWorkflow,
            native_run_id: known(run.native_id.clone()),
            native_task_id: run
                .task_id
                .clone()
                .map_or_else(|| unknown("not-emitted"), known),
            tool_call_id,
            state: snapshot.state,
            revision: U64(revision),
            title,
            name,
            description,
            totals: Some(snapshot.totals.clone()),
            launched_at: snapshot.launched_at.clone(),
            live: Some(WorkflowLive {
                phase_title: snapshot
                    .live_phase
                    .clone()
                    .map_or_else(|| unknown("not-emitted"), known),
                agent_label: snapshot
                    .live_agent
                    .clone()
                    .map_or_else(|| unknown("not-emitted"), known),
                summary: snapshot
                    .summary
                    .clone()
                    .map_or_else(|| unknown("not-emitted"), known),
            }),
            note: run.note.clone(),
            result_ref: None,
        };
        Ok(self.synth_envelope(ObservationPayload::WorkflowRun(Box::new(payload))))
    }

    fn build_phase_envelope(
        &mut self,
        index: usize,
        phase_id: &remuda_protocol::Id,
        title: &str,
        state: WorkflowState,
        revision: u64,
    ) -> Result<Envelope, Error> {
        let workflow_id = self.ids.workflow(&self.runs[index].native_id)?;
        let payload = WorkflowPhasePayload {
            workflow_id,
            phase_id: phase_id.clone(),
            native_phase_id: known(title.to_owned()),
            label: known(title.to_owned()),
            state,
            revision: U64(revision),
            parent_phase_id: None,
        };
        Ok(self.synth_envelope(ObservationPayload::WorkflowPhase(Box::new(payload))))
    }

    fn build_member_envelope(
        &mut self,
        index: usize,
        snapshot: &MemberSnapshot,
        revision: u64,
    ) -> Result<Envelope, Error> {
        let run = &self.runs[index];
        let workflow_id = self.ids.workflow(&run.native_id)?;
        let member_id = self
            .ids
            .member(format!("{}:agent:{}", run.native_id, snapshot.agent_id))?;
        let phase_id = match &snapshot.phase {
            Some(title) => Some(self.ids.phase(format!("{}:phase:{title}", run.native_id))?),
            None => None,
        };
        let label = snapshot
            .label
            .clone()
            .unwrap_or_else(|| short_agent_id(&snapshot.agent_id));
        let model = snapshot
            .model
            .clone()
            .unwrap_or_else(|| "not-emitted".into());
        let model = if snapshot.model.is_some() {
            known(model)
        } else {
            unknown("not-emitted")
        };
        let payload = WorkflowMemberPayload {
            workflow_id,
            member_id,
            native_agent_id: known(snapshot.agent_id.clone()),
            native_key: snapshot
                .key
                .clone()
                .map_or_else(|| unknown("not-emitted"), known),
            attempt: if snapshot.attempt > 0 {
                known(U64(snapshot.attempt))
            } else {
                unknown("not-emitted")
            },
            phase_id,
            label: known(label),
            state: snapshot.state,
            model_requested: model.clone(),
            model_resolved: model,
            result_ref: None,
            revision: U64(revision),
            latest_tool: snapshot.latest_tool.clone().map(known),
            tokens: snapshot.tokens.map(U64),
            calls: Some(U64(snapshot.calls)),
            duration_ms: snapshot.duration_ms.map(U64),
            started_at: snapshot.started_at.clone(),
            ended_at: snapshot.ended_at.clone(),
            last_progress_at: snapshot.last_progress_at.clone(),
        };
        Ok(self.synth_envelope(ObservationPayload::WorkflowMember(Box::new(payload))))
    }

    /// Envelope for a synthesized snapshot (no single native line backs it).
    fn synth_envelope(&self, body: ObservationPayload) -> Envelope {
        Envelope {
            journal_id: self.ctx.journal_id.clone(),
            instance_id: self.ctx.instance_id.clone(),
            run_id: self.ctx.run_id.clone(),
            host_id: self.ctx.host_id.clone(),
            process_generation: self.ctx.process_generation,
            run_generation: self.ctx.run_generation,
            observed_at: timestamp_now().expect("clock"),
            native_at: unknown("synthesized-from-run-files"),
            source: remuda_protocol::ObservationSource {
                driver_kind: self.ctx.driver_kind,
                driver_version: self.ctx.driver_version.clone(),
                adapter_version: self.ctx.adapter_version.clone(),
                channel: SourceChannel::WorkflowJournal,
                delivery: self.ctx.delivery,
                native_session_id: known(self.ctx.native_session_id.clone()),
                native_turn_id: unknown("not-emitted"),
                native_agent_id: unknown("not-emitted"),
                native_item_id: unknown("not-emitted"),
                native_event_id: unknown("not-emitted"),
                native_request_id: remuda_protocol::NativeRequestKey::None,
                source_cursor: remuda_protocol::SourceCursor::Runtime(Box::new(
                    remuda_protocol::RuntimeCursor {
                        ledger_revision: U64(0),
                    },
                )),
            },
            completeness: Completeness::Structured,
            evidence_event_ids: Vec::new(),
            event_id: None,
            body,
            raw: None,
        }
    }
}

/// Plain serializable view of a member, borrowed for change detection.
#[derive(Debug, Clone)]
struct MemberSnapshot {
    agent_id: String,
    key: Option<String>,
    attempt: u64,
    state: WorkflowState,
    label: Option<String>,
    phase: Option<String>,
    model: Option<String>,
    latest_tool: Option<String>,
    calls: u64,
    tokens: Option<u64>,
    duration_ms: Option<u64>,
    started_at: Option<Timestamp>,
    ended_at: Option<Timestamp>,
    /// `MemberState.last_ts`: timestamp of the newest agent transcript line.
    last_progress_at: Option<Timestamp>,
}

impl MemberSnapshot {
    fn from_state(state: &MemberState) -> Self {
        let ended_at = if state.stopped {
            state.last_ts.clone()
        } else {
            None
        };
        let duration_ms = match (&state.started_at, state.last_ts.as_ref()) {
            (Some(start), Some(end)) => ts_delta_ms(start, end),
            _ => None,
        };
        Self {
            agent_id: state.agent_id.clone(),
            key: state.key.clone(),
            attempt: state.attempt,
            state: state.state,
            label: state.label.clone(),
            phase: state.phase.clone(),
            model: state.model.clone(),
            latest_tool: state.latest_tool.clone(),
            calls: state.calls,
            tokens: state.tokens,
            duration_ms,
            started_at: state.started_at.clone(),
            ended_at,
            last_progress_at: state.last_ts.clone(),
        }
    }

    fn sig(&self) -> MemberSig {
        MemberSig {
            state: self.state,
            label: self.label.clone(),
            phase: self.phase.clone(),
            model: self.model.clone(),
            latest_tool: self.latest_tool.clone(),
            calls: self.calls,
            tokens: self.tokens,
            attempt: self.attempt,
        }
    }
}

/// Plain view of the run-level snapshot.
struct RunSnapshot {
    state: WorkflowState,
    totals: WorkflowTotals,
    launched_at: Option<Timestamp>,
    live_phase: Option<String>,
    live_agent: Option<String>,
    summary: Option<String>,
}

impl RunSnapshot {
    fn build(run: &RunState, members: &[MemberSnapshot]) -> Self {
        let state = run
            .terminal
            .as_ref()
            .map(|(state, _)| *state)
            .unwrap_or(WorkflowState::Running);
        let mut done = 0u64;
        let mut failed = 0u64;
        let mut killed = 0u64;
        let mut running = 0u64;
        let mut tokens = 0u64;
        let mut calls = 0u64;
        for member in members {
            match member.state {
                WorkflowState::Completed => done += 1,
                WorkflowState::Failed => failed += 1,
                WorkflowState::Cancelled => killed += 1,
                WorkflowState::Running => running += 1,
                _ => {}
            }
            tokens += member.tokens.unwrap_or(0);
            calls += member.calls;
        }
        // "Current phase: agent" = the most recently started live member.
        let live = members.iter().rev().find(|member| {
            member.state == WorkflowState::Running
                || run.phases.iter().any(|phase| {
                    phase.state == WorkflowState::Running
                        && phase.title.as_str() == member.phase.as_deref().unwrap_or("")
                })
        });
        let live_phase = live.and_then(|member| member.phase.clone());
        let live_agent = live.map(|member| {
            member
                .label
                .clone()
                .unwrap_or_else(|| short_agent_id(&member.agent_id))
        });
        let total_known = run
            .script
            .as_ref()
            .is_some_and(|script| !script.total_unknown);
        let agents_total = if total_known {
            run.script
                .as_ref()
                .map_or(members.len() as u64, |script| script.calls.len() as u64)
        } else {
            members.len() as u64
        };
        let earliest = members
            .iter()
            .filter_map(|m| m.started_at.clone().map(String::from))
            .min()
            .and_then(|text| Timestamp::try_from(text).ok());
        let elapsed_ms = match (&earliest, state, run.launched_ms.zip(system_ms())) {
            (Some(start), WorkflowState::Running, _) => ts_since_ms(start),
            (Some(start), _, _) => members
                .iter()
                .filter_map(|m| {
                    m.ended_at
                        .clone()
                        .or_else(|| m.started_at.clone())
                        .map(String::from)
                })
                .max()
                .and_then(|text| Timestamp::try_from(text).ok())
                .and_then(|end| ts_delta_ms(start, &end)),
            (None, _, Some((begin, now))) => Some(now.saturating_sub(begin) as u64),
            (None, _, None) => Some(0),
        };
        let summary = run.terminal.as_ref().and_then(|(terminal_state, text)| {
            text.clone().or_else(|| match terminal_state {
                WorkflowState::Cancelled => Some("workflow terminated".to_owned()),
                WorkflowState::Failed => Some("workflow failed".to_owned()),
                _ => Some("workflow completed".to_owned()),
            })
        });
        Self {
            state,
            totals: WorkflowTotals {
                total_known,
                agents_total: U64(agents_total.max(if total_known {
                    (done + failed + killed + running).max(agents_total)
                } else {
                    done + failed + killed + running
                })),
                agents_done: U64(done),
                agents_failed: U64(failed),
                agents_killed: U64(killed),
                agents_running: U64(running),
                tokens: U64(tokens),
                calls: U64(calls),
                elapsed_ms: U64(elapsed_ms.unwrap_or(0)),
            },
            launched_at: run.launched_ms.and_then(timestamp_from_unix_ms),
            live_phase,
            live_agent,
            summary,
        }
    }

    fn signature(&self) -> String {
        // Tokens/elapsed move continuously; they must not by themselves trigger
        // an observation. They ride along with the next real transition.
        format!(
            "{}|{}|{}|{}|{}|{}|{}",
            match self.state {
                WorkflowState::Completed => "c",
                WorkflowState::Failed => "f",
                WorkflowState::Cancelled => "k",
                WorkflowState::Queued => "q",
                WorkflowState::Running => "r",
                WorkflowState::Unknown => "u",
            },
            self.totals.agents_total.0,
            self.totals.agents_done.0,
            self.totals.agents_failed.0,
            self.totals.agents_killed.0,
            self.totals.agents_running.0,
            self.live_agent.as_deref().unwrap_or("-"),
        )
    }
}

impl RunState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        native_id: String,
        dir: PathBuf,
        task_id: Option<String>,
        tool_call_native: Option<String>,
        script_path: Option<PathBuf>,
        script_source: Option<String>,
        launched_ms: Option<u128>,
    ) -> Self {
        Self {
            native_id,
            dir,
            task_id,
            tool_call_native,
            script: None,
            script_path,
            script_source,
            script_loaded: false,
            main_transcript: None,
            terminal: None,
            grace: 0,
            finished: false,
            members: Vec::new(),
            attempts: HashMap::new(),
            member_revs: HashMap::new(),
            phases: Vec::new(),
            run_rev: 0,
            last_run_sig: None,
            emitted_phase: HashMap::new(),
            emitted_member: HashMap::new(),
            last_emitted_note: None,
            note: None,
            missing_polls: 0,
            launched_ms,
        }
    }

    fn seed_script_phases(&mut self) {
        let Some(script) = &self.script else {
            return;
        };
        if script.total_unknown {
            return;
        }
        for title in &script.phases {
            if !self.phases.iter().any(|phase| &phase.title == title) {
                self.phases.push(PhaseState {
                    title: title.clone(),
                    state: WorkflowState::Queued,
                });
            }
        }
    }

    fn load_script(&mut self, session_dir: &Path) {
        if self.script_loaded {
            return;
        }
        self.script_loaded = true;
        let source = self
            .script_source
            .clone()
            .or_else(|| {
                let path = self.script_path.clone()?;
                fs::read_to_string(path).ok()
            })
            .or_else(|| {
                // Convention: `<session>/workflows/scripts/<name>-<runId>.js`.
                let scripts = session_dir.join("workflows").join("scripts");
                let suffix = format!("-{}.js", self.native_id);
                let entry = fs::read_dir(&scripts)
                    .ok()?
                    .flatten()
                    .find(|entry| entry.file_name().to_string_lossy().ends_with(&suffix))?;
                fs::read_to_string(entry.path()).ok()
            })
            .or_else(|| {
                // 2.1.221 also persists the full script inside
                // `<session>/workflows/<runId>.json` (the run snapshot). Read
                // it without pulling that file's result/progress fields — they
                // are terminal-only and the journal stays the live authority.
                let snapshot = session_dir
                    .join("workflows")
                    .join(format!("{}.json", self.native_id));
                let bytes = fs::read(snapshot).ok()?;
                let value = serde_json::from_slice::<Value>(&bytes).ok()?;
                value
                    .get("script")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        if let Some(source) = source {
            self.script = Some(WorkflowScript::parse(&source));
        }
    }

    fn recompute_phases(&mut self) {
        self.seed_script_phases();
        for phase in &mut self.phases {
            let states: Vec<WorkflowState> = self
                .members
                .iter()
                .filter(|member| member.phase.as_deref() == Some(phase.title.as_str()))
                .map(|member| member.state)
                .collect();
            phase.state = if states.is_empty() {
                WorkflowState::Queued
            } else if states.contains(&WorkflowState::Failed) {
                WorkflowState::Failed
            } else if states.contains(&WorkflowState::Cancelled)
                && !states.contains(&WorkflowState::Running)
            {
                WorkflowState::Cancelled
            } else if states
                .iter()
                .all(|state| *state == WorkflowState::Completed)
            {
                WorkflowState::Completed
            } else {
                WorkflowState::Running
            };
        }
    }

    fn update_note(&mut self) {
        let readable = self.dir.is_dir()
            && (self.dir.join("journal.jsonl").is_file() || !self.members.is_empty());
        if readable {
            self.missing_polls = 0;
            // Never show the note when real data exists.
            if !self.members.is_empty() || self.script.is_some() {
                self.note = None;
            }
            return;
        }
        self.missing_polls += 1;
        if self.missing_polls >= MISSING_DIR_POLLS && self.note.is_none() {
            self.note = Some(
                "the workflow run directory is unreadable on this host; no live detail available"
                    .to_owned(),
            );
        }
    }

    fn apply_terminal_members(&mut self) {
        let Some((state, _)) = self.terminal else {
            return;
        };
        // At a terminal run state every unresolved member settles with the run
        // (a lost journal `result` line cannot leave an agent "running" in a
        // finished card). The journal result/failed still wins when it got
        // there first: it set the member state before this runs.
        for member in &mut self.members {
            if matches!(member.state, WorkflowState::Running | WorkflowState::Queued) {
                member.state = state;
                member.stopped = true;
            }
        }
    }

    fn fold_journal_line(&mut self, line: &[u8]) {
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "started" => {
                let key = value
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("member")
                    .to_owned();
                let agent_id = value
                    .get("agentId")
                    .and_then(Value::as_str)
                    .unwrap_or(&key)
                    .to_owned();
                let attempt = {
                    let count = self.attempts.entry(key.clone()).or_insert(0);
                    *count += 1;
                    *count
                };
                if !self
                    .members
                    .iter()
                    .any(|member| member.agent_id == agent_id)
                {
                    self.members.push(MemberState::new(
                        agent_id.clone(),
                        Some(key.clone()),
                        attempt,
                    ));
                }
                let index = self
                    .members
                    .iter()
                    .position(|member| member.agent_id == agent_id)
                    .expect("member exists");
                let member = &mut self.members[index];
                member.key = Some(key.clone());
                member.attempt = attempt;
                member.state = WorkflowState::Running;
                member.stopped = false;
                if member.label.is_none()
                    && let Some(label) = value.get("label").and_then(Value::as_str)
                {
                    member.label = Some(label.to_owned());
                }
                if member.phase.is_none()
                    && let Some(phase) = value.get("phase").and_then(Value::as_str)
                {
                    self.assign_phase(&agent_id, phase.to_owned());
                }
            }
            "result" => {
                let key = value.get("key").and_then(Value::as_str);
                let agent_id = value
                    .get("agentId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| {
                        key.and_then(|key| {
                            self.members
                                .iter()
                                .find(|member| member.key.as_deref() == Some(key))
                                .map(|member| member.agent_id.clone())
                        })
                    });
                if let Some(agent_id) = agent_id
                    && let Some(index) = self
                        .members
                        .iter()
                        .position(|member| member.agent_id == agent_id)
                {
                    self.members[index].state = WorkflowState::Completed;
                    self.members[index].stopped = true;
                }
            }
            "failed" => {
                if let Some(agent_id) = value.get("agentId").and_then(Value::as_str)
                    && let Some(index) = self
                        .members
                        .iter()
                        .position(|member| member.agent_id == agent_id)
                {
                    self.members[index].state = WorkflowState::Failed;
                    self.members[index].stopped = true;
                } else if self.terminal.is_none() {
                    // Run-level failure record.
                    self.terminal = Some((WorkflowState::Failed, None));
                    self.grace = TERMINAL_GRACE_POLLS;
                }
            }
            "launched" => {}
            _ => {}
        }
    }

    fn assign_phase(&mut self, agent_id: &str, phase: String) {
        if !self.phases.iter().any(|existing| existing.title == phase) {
            self.phases.push(PhaseState {
                title: phase.clone(),
                state: WorkflowState::Queued,
            });
        }
        if let Some(member) = self
            .members
            .iter_mut()
            .find(|member| member.agent_id == agent_id)
        {
            member.phase = Some(phase);
        }
    }

    fn agent_started(
        &mut self,
        agent_id: &str,
        transcript_path: Option<&Path>,
    ) -> Result<(), Error> {
        if self
            .members
            .iter()
            .any(|member| member.agent_id == agent_id)
        {
            return Ok(());
        }
        let mut member = MemberState::new(agent_id.to_owned(), None, 1);
        if let Some(path) = transcript_path {
            member.jsonl = Some(FileTail::new(path)?);
        }
        self.members.push(member);
        self.refresh_agents()?;
        Ok(())
    }

    fn agent_stopped(&mut self, agent_id: &str, transcript_path: Option<&Path>) {
        let Some(index) = self
            .members
            .iter()
            .position(|member| member.agent_id == agent_id)
        else {
            return;
        };
        if self.members[index].jsonl.is_none()
            && let Some(path) = transcript_path
            && let Ok(tail) = FileTail::new(path)
        {
            self.members[index].jsonl = Some(tail);
        }
        self.members[index].stopped = true;
        self.refresh_agents().ok();
        // SubagentStop is not a terminal state: it fires for a completed agent,
        // a failed one, and a TaskStop-killed one alike. The journal
        // result/failed sets the outcome, and a still-running member is left
        // running so the run's terminal decision (apply_terminal_members)
        // settles it — the stop hook only contributes the final transcript
        // tail and timestamps.
    }

    /// Read newly appeared meta/jsonl files, tail transcript growth, and join
    /// static prompts to script call sites.
    fn refresh_agents(&mut self) -> Result<(), Error> {
        for index in 0..self.members.len() {
            let agent_id = self.members[index].agent_id.clone();
            if self.members[index].jsonl.is_none() {
                let path = self.dir.join(format!("agent-{agent_id}.jsonl"));
                if path.is_file() {
                    self.members[index].jsonl = Some(FileTail::new(path)?);
                }
            }
            self.read_meta(index);
            self.poll_agent_jsonl(index)?;
            self.join_script(index);
        }
        Ok(())
    }

    fn read_meta(&mut self, index: usize) {
        if self.members[index].meta_read {
            return;
        }
        let path = self
            .dir
            .join(format!("agent-{}.meta.json", self.members[index].agent_id));
        let Ok(bytes) = fs::read(path) else {
            return;
        };
        if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
            self.members[index].meta_read = true;
            if self.members[index].model.is_none()
                && let Some(model) = value.get("model").and_then(Value::as_str)
            {
                self.members[index].model = Some(model.to_owned());
            }
            if self.members[index].label.is_none()
                && let Some(label) = value
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
            {
                self.members[index].label = Some(label.to_owned());
            }
            if self.members[index].phase.is_none()
                && let Some(phase) = value
                    .get("workflowPhase")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
            {
                let agent_id = self.members[index].agent_id.clone();
                self.assign_phase(&agent_id, phase.to_owned());
            }
        }
    }

    fn join_script(&mut self, index: usize) {
        if self.members[index].phase.is_some() {
            return;
        }
        let (agent_id, prompt) = {
            let member = &self.members[index];
            (member.agent_id.clone(), member.prompt.clone())
        };
        let (Some(script), Some(prompt)) = (&self.script, prompt) else {
            return;
        };
        let used: HashSet<usize> = self
            .members
            .iter()
            .filter_map(|member| member.matched_call)
            .collect();
        if let Some(matched) = script.match_prompt(&prompt, &used) {
            self.members[index].matched_call = Some(matched.index);
            if self.members[index].label.is_none() {
                self.members[index].label = matched.label;
            }
            if let Some(phase) = matched.phase {
                self.assign_phase(&agent_id, phase);
            }
        }
    }

    fn poll_agent_jsonl(&mut self, index: usize) -> Result<(), Error> {
        let oversized = self.members[index].jsonl.as_ref().is_some_and(|tail| {
            fs::metadata(tail.path()).is_ok_and(|meta| meta.len() > MAX_AGENT_JSONL)
        });
        if oversized {
            return Ok(());
        }
        let lines = match self.members[index].jsonl.as_mut() {
            Some(tail) => tail.poll()?,
            None => return Ok(()),
        };
        if lines.is_empty() {
            return Ok(());
        }
        for (line, _) in lines {
            let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if let remuda_protocol::Knowledge::Known { value: ts } =
                parse_timestamp(value.get("timestamp").and_then(Value::as_str).unwrap_or(""))
            {
                if self.members[index].started_at.is_none() {
                    self.members[index].started_at = Some(ts.clone());
                }
                self.members[index].last_ts = Some(ts);
            }
            let Some(message) = value.get("message") else {
                continue;
            };
            // First user record carries the prompt literal.
            if value.get("type").and_then(Value::as_str) == Some("user")
                && self.members[index].prompt.is_none()
                && let Some(text) = content_text(message.get("content").unwrap_or(&Value::Null))
            {
                self.members[index].prompt = Some(text);
            }
            if let Some(model) = message.get("model").and_then(Value::as_str) {
                self.members[index].model = Some(model.to_owned());
            }
            if let Some(usage) = message.get("usage") {
                // The agent's last reported usage already counts its whole
                // context (input+cache), and every later assistant record
                // replaces it — verified to reconcile with the harness's
                // `<usage>` total within 0.1% on the recorded 2.1.221 run.
                let total = [
                    "input_tokens",
                    "output_tokens",
                    "cache_creation_input_tokens",
                    "cache_read_input_tokens",
                ]
                .iter()
                .filter_map(|key| usage.get(key).and_then(Value::as_u64))
                .sum::<u64>();
                if total > 0 {
                    self.members[index].tokens = Some(total);
                }
            }
            if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use")
                        && let Some(name) = block.get("name").and_then(Value::as_str)
                    {
                        self.members[index].calls += 1;
                        self.members[index].latest_tool = Some(name.to_owned());
                    }
                }
            }
        }
        Ok(())
    }

    fn poll_transcript(&mut self) -> Result<(), Error> {
        if self.terminal.is_some() {
            return Ok(());
        }
        let Some(tail) = self.main_transcript.as_mut() else {
            return Ok(());
        };
        for (line, _) in tail.poll()? {
            let text = String::from_utf8_lossy(&line);
            if text.contains("<task-notification>") && self.notification_claims_run(&text) {
                if let Some((state, summary)) = parse_task_notification(&text) {
                    self.terminal = Some((state, summary));
                    self.grace = TERMINAL_GRACE_POLLS;
                }
            } else if self.text_is_taskstop(&text) {
                self.terminal = Some((
                    WorkflowState::Cancelled,
                    Some("workflow terminated".to_owned()),
                ));
                self.grace = TERMINAL_GRACE_POLLS;
            }
        }
        Ok(())
    }

    fn notification_claims_run(&self, text: &str) -> bool {
        if let Some(task_id) = &self.task_id
            && text.contains(&format!("<task-id>{task_id}</task-id>"))
        {
            return true;
        }
        if let Some(tool_id) = &self.tool_call_native
            && text.contains(&format!("<tool-use-id>{tool_id}</tool-use-id>"))
        {
            return true;
        }
        false
    }

    fn text_is_taskstop(&self, text: &str) -> bool {
        let Some(task_id) = &self.task_id else {
            return false;
        };
        text.contains("TaskStop") && text.contains(task_id)
    }
}

impl MemberState {
    fn new(agent_id: String, key: Option<String>, attempt: u64) -> Self {
        Self {
            agent_id,
            key,
            attempt,
            state: WorkflowState::Running,
            label: None,
            phase: None,
            prompt: None,
            matched_call: None,
            model: None,
            latest_tool: None,
            calls: 0,
            tokens: None,
            started_at: None,
            last_ts: None,
            jsonl: None,
            meta_read: false,
            stopped: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

fn short_agent_id(agent_id: &str) -> String {
    agent_id.get(0..8).unwrap_or(agent_id).to_owned()
}

fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => blocks
            .iter()
            .find_map(|block| block.get("text").and_then(Value::as_str).map(str::to_owned)),
        _ => None,
    }
}

fn parse_rfc3339(raw: &str) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339).ok()
}

fn ts_delta_ms(start: &Timestamp, end: &Timestamp) -> Option<u64> {
    let start = parse_rfc3339(&String::from(start.clone()))?;
    let end = parse_rfc3339(&String::from(end.clone()))?;
    u64::try_from((end - start).whole_milliseconds()).ok()
}

fn ts_since_ms(start: &Timestamp) -> Option<u64> {
    let start = parse_rfc3339(&String::from(start.clone()))?;
    u64::try_from((time::OffsetDateTime::now_utc() - start).whole_milliseconds()).ok()
}

fn system_ms() -> Option<u128> {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis())
}

fn run_id_from_transcript_path(path: &Path) -> Option<String> {
    let mut components = path.components().rev();
    let file = components.next()?.as_os_str().to_string_lossy();
    if !file.starts_with("agent-") || !file.ends_with(".jsonl") {
        return None;
    }
    let run = components.next()?.as_os_str().to_string_lossy();
    if run.starts_with("wf_") {
        Some(run.into_owned())
    } else {
        None
    }
}

fn parse_task_notification(text: &str) -> Option<(WorkflowState, Option<String>)> {
    let status = tag_value(text, "status")?;
    let state = match status.as_str() {
        "completed" => WorkflowState::Completed,
        "failed" => WorkflowState::Failed,
        "killed" | "cancelled" | "terminated" => WorkflowState::Cancelled,
        _ => WorkflowState::Unknown,
    };
    Some((state, tag_value(text, "summary").map(html_unescape)))
}

fn tag_value(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(text[start..end].trim().to_owned())
}

fn html_unescape(text: String) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn discover_journals(session_dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let root = session_dir.join("subagents").join("workflows");
    let entries = match fs::read_dir(&root) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
        Ok(entries) => entries,
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let journal = entry.path().join("journal.jsonl");
        if journal.is_file() {
            out.push(journal);
        }
    }
    out.sort();
    Ok(out)
}

impl Source for WorkflowJournalTailer {
    fn name(&self) -> &'static str {
        "workflow-journal"
    }

    fn map_line(&mut self, line: &[u8], cursor: FileCursor) -> Result<Vec<Envelope>, Error> {
        let dummy = self
            .session_dir
            .join("subagents/workflows/wf_unknown/journal.jsonl");
        WorkflowJournalTailer::map_workflow_line(&self.ctx, &dummy, line, &cursor)
    }
}

impl WorkflowJournalTailer {
    fn map_workflow_line(
        ctx: &MapContext,
        journal_path: &Path,
        line: &[u8],
        cursor: &FileCursor,
    ) -> Result<Vec<Envelope>, Error> {
        let value = match serde_json::from_slice::<Value>(line) {
            Ok(value) => value,
            Err(_) => {
                return Ok(vec![opaque(
                    ctx,
                    cursor,
                    line,
                    "malformed-jsonl",
                    OpaqueReason::Malformed,
                    Completeness::Opaque,
                    None,
                )?]);
            }
        };
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        let wf_native = workflow_native_id(journal_path);
        match kind {
            "launched" | "started" | "result" | "failed" => Ok(vec![file_envelope(
                ctx,
                cursor,
                line,
                &value,
                Completeness::Structured,
                Some(wf_native.as_str()),
                ObservationPayload::Lifecycle(Box::new(remuda_protocol::LifecyclePayload::Native(
                    Box::new(remuda_protocol::NativeLifecycle {
                        topic: remuda_protocol::LifecycleTopic::Diagnostic,
                        native_name: format!("workflow-journal:{kind}"),
                        native_id: known(wf_native.clone()),
                        status: known(kind.to_owned()),
                        related_ids: std::collections::BTreeMap::new(),
                        data_ref: None,
                        severity: remuda_protocol::Severity::Info,
                        affects_completion: false,
                    }),
                ))),
            )?]),
            other => Ok(vec![opaque(
                ctx,
                cursor,
                line,
                if other.is_empty() {
                    "missing-type"
                } else {
                    other
                },
                if other.is_empty() {
                    OpaqueReason::Malformed
                } else {
                    OpaqueReason::UnknownType
                },
                Completeness::Opaque,
                None,
            )?]),
        }
    }
}

fn workflow_native_id(journal_path: &Path) -> String {
    journal_path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "wf_unknown".into())
}
