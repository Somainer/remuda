//! Node-side Workflow producer bridge.
//!
//! The driver's hook channel already carries every trigger this needs:
//! - `SessionStart` binds the session directory (the parent of the
//!   `transcriptPath` file stem) and the main transcript to tail for terminal
//!   markers;
//! - `PostToolUse(Workflow)` carries the structured launch handle (`runId`,
//!   `taskId`, `transcriptDir`, `scriptPath`) curated in
//!   `remuda_signal::map`;
//! - `SubagentStart` / `SubagentStop` carry the workflow agent id (and the
//!   stop event its `agent-<id>.jsonl` path).
//!
//! [`WorkflowProducer`] feeds those lifecycle observations to a per-session
//! [`WorkflowJournalTailer`], which does the bounded file reads and folding.
//! Hook-triggered emissions happen synchronously in [`Self::on_observation`]
//! (first `workflow.run` and member start budgets); growth between hooks is
//! drained by [`Self::poll`] on the pump's 250 ms tick.

use remuda_journal::{MapContext, WorkflowJournalTailer, WorkflowLaunch};
use remuda_protocol::{
    DriverKind, EventId, HostId, Id, InstanceId, Observation, ObservationPayload, RunId,
    SchemaVersion, SourceChannel, U64,
};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// File-poll cadence, matching the driver's file-adapter tick.
pub const WORKFLOW_POLL: Duration = Duration::from_millis(250);

/// Folds hook lifecycle observations into one session's workflow runs.
pub struct WorkflowProducer {
    instance_id: InstanceId,
    tailer: Option<WorkflowJournalTailer>,
    session_dir: Option<PathBuf>,
    /// Run identity copied onto file-derived observations.
    last_run_id: Option<RunId>,
    last_run_generation: Option<U64>,
}

impl WorkflowProducer {
    /// Create an unbound producer. The first `SessionStart` binds the session
    /// directory.
    #[must_use]
    pub fn new(instance_id: InstanceId) -> Self {
        Self {
            instance_id,
            tailer: None,
            session_dir: None,
            last_run_id: None,
            last_run_generation: None,
        }
    }

    /// Feed one committed observation. Returns observations to append
    /// immediately (the launch snapshot and member starts are emitted here, on
    /// the hook's tail, to meet the latency budgets).
    pub fn on_observation(&mut self, observation: &Observation) -> Vec<Observation> {
        if observation.source.channel != SourceChannel::Hook {
            return Vec::new();
        }
        let ObservationPayload::Lifecycle(payload) = &observation.body else {
            return Vec::new();
        };
        let remuda_protocol::LifecyclePayload::Native(native) = payload.as_ref() else {
            return Vec::new();
        };
        self.last_run_id = observation.run_id.clone();
        self.last_run_generation = observation.run_generation;
        let related = &native.related_ids;
        let envelopes = match native.native_name.as_str() {
            "SessionStart" => self.bind_session(related.get("transcriptPath").map(String::as_str)),
            "PostToolUse" | "PostToolBatch"
                if related.get("toolName").map(String::as_str) == Some("Workflow") =>
            {
                self.launch(related)
            }
            "SubagentStart"
                if related.get("agentType").map(String::as_str) == Some("workflow-subagent") =>
            {
                self.agent_event(
                    related.get("agentId").map(String::as_str),
                    true,
                    related.get("agentTranscriptPath").map(Path::new),
                )
            }
            "SubagentStop" => {
                let path = related.get("agentTranscriptPath").map(Path::new);
                match path {
                    Some(path) if is_workflow_transcript(path) => {
                        let agent_id = related
                            .get("agentId")
                            .cloned()
                            .or_else(|| agent_id_from_path(path));
                        self.agent_event_owned(agent_id, false, Some(path))
                    }
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        };
        envelopes
            .into_iter()
            .map(|envelope| self.stamp(envelope))
            .collect()
    }

    /// Drain file growth (journals, agent transcripts, terminal markers).
    pub fn poll(&mut self) -> Vec<Observation> {
        let Some(tailer) = self.tailer.as_mut() else {
            return Vec::new();
        };
        match tailer.poll() {
            Ok(envelopes) => envelopes
                .into_iter()
                .map(|envelope| self.stamp(envelope))
                .collect(),
            Err(error) => {
                tracing::debug!(%error, "workflow producer poll failed");
                Vec::new()
            }
        }
    }

    /// Feed an externally proven terminal state (TaskStop hook fold).
    pub fn terminate(
        &mut self,
        run_or_task_id: &str,
        state: remuda_protocol::WorkflowState,
    ) -> Vec<Observation> {
        let Some(tailer) = self.tailer.as_mut() else {
            return Vec::new();
        };
        tailer
            .terminate(run_or_task_id, state, None)
            .unwrap_or_default()
            .into_iter()
            .map(|envelope| self.stamp(envelope))
            .collect()
    }

    fn bind_session(&mut self, transcript_path: Option<&str>) -> Vec<remuda_journal::Envelope> {
        let Some(transcript_path) = transcript_path else {
            return Vec::new();
        };
        let transcript = PathBuf::from(transcript_path);
        let Some(session_dir) = session_dir_from_transcript(&transcript) else {
            return Vec::new();
        };
        if self.session_dir.as_deref() == Some(session_dir.as_path()) {
            return Vec::new();
        }
        self.session_dir = Some(session_dir.clone());
        let ctx = self.map_context(&session_dir);
        let mut tailer = WorkflowJournalTailer::new(session_dir, ctx);
        if tailer.set_main_transcript(&transcript).is_err() {
            tracing::debug!("main transcript tail unavailable");
        }
        self.tailer = Some(tailer);
        Vec::new()
    }

    fn launch(
        &mut self,
        related: &std::collections::BTreeMap<String, String>,
    ) -> Vec<remuda_journal::Envelope> {
        let Some(run_id) = related.get("runId") else {
            return Vec::new();
        };
        let Some(tailer) = self.ensure_tailer(related.get("transcriptDir").map(Path::new)) else {
            return Vec::new();
        };
        let launch = WorkflowLaunch {
            run_id: run_id.clone(),
            task_id: related.get("taskId").cloned(),
            tool_call_id: related.get("toolUseId").cloned(),
            transcript_dir: related
                .get("transcriptDir")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute()),
            script_path: related
                .get("scriptPath")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute()),
            script_source: None,
        };
        tailer.launch(launch).unwrap_or_default()
    }

    fn agent_event(
        &mut self,
        agent_id: Option<&str>,
        started: bool,
        path: Option<&Path>,
    ) -> Vec<remuda_journal::Envelope> {
        let (Some(agent_id), Some(tailer)) = (agent_id, self.tailer.as_mut()) else {
            return Vec::new();
        };
        tailer
            .agent_event(agent_id, started, path)
            .unwrap_or_default()
    }

    fn agent_event_owned(
        &mut self,
        agent_id: Option<String>,
        started: bool,
        path: Option<&Path>,
    ) -> Vec<remuda_journal::Envelope> {
        self.agent_event(agent_id.as_deref(), started, path)
    }

    fn ensure_tailer(
        &mut self,
        transcript_dir: Option<&Path>,
    ) -> Option<&mut WorkflowJournalTailer> {
        if self.tailer.is_none() {
            // A Workflow launch without a prior SessionStart bind: derive the
            // session directory from the run directory the hook named.
            let session_dir = transcript_dir
                .and_then(|path| {
                    // …/<session>/subagents/workflows/<run>
                    path.ancestors().nth(3).map(Path::to_path_buf)
                })
                .or_else(|| self.session_dir.clone())?;
            self.session_dir = Some(session_dir.clone());
            let ctx = self.map_context(&session_dir);
            self.tailer = Some(WorkflowJournalTailer::new(session_dir, ctx));
        }
        self.tailer.as_mut()
    }

    fn map_context(&self, session_dir: &Path) -> MapContext {
        let mut ctx = MapContext::claude_file(
            self.instance_id.clone(),
            // Store append replaces journal/host identity with the instance's.
            Id::new("obj").expect("obj prefix"),
            HostId::new(),
            session_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "workflow-session".into()),
            SourceChannel::WorkflowJournal,
        );
        ctx.driver_kind = DriverKind::ShellPty;
        // The workflow tailer maps lifecycle/usage/terminal facts, never
        // tool results, so no media stager is attached here; workflow-member
        // screenshots fold through the `subagent.transcript` RPC path (D-045
        // §6.2, codex-cua-4 evidence).
        ctx
    }

    /// Turn a journal envelope into the driver-shaped observation the store
    /// appends. Identity (`journalId`, `instanceId`, `hostId`,
    /// `processGeneration`, `seq`, `eventId`) is replaced at append; run
    /// identity is carried across from the hook stream.
    fn stamp(&self, envelope: remuda_journal::Envelope) -> Observation {
        Observation {
            schema_version: SchemaVersion,
            event_id: EventId::new(),
            journal_id: Id::new("obj").expect("obj prefix"),
            instance_id: self.instance_id.clone(),
            run_id: self.last_run_id.clone(),
            host_id: HostId::new(),
            process_generation: U64(1),
            run_generation: self.last_run_generation,
            seq: U64(0),
            observed_at: envelope.observed_at,
            native_at: envelope.native_at,
            source: envelope.source,
            completeness: envelope.completeness,
            raw_ref: None,
            evidence_event_ids: envelope.evidence_event_ids,
            body: envelope.body,
        }
    }
}

/// The session directory is `<projects>/<enc>/<sid>/` when the transcript is
/// `<projects>/<enc>/<sid>.jsonl`.
fn session_dir_from_transcript(transcript: &Path) -> Option<PathBuf> {
    let parent = transcript.parent()?;
    let stem = transcript.file_stem()?;
    Some(parent.join(stem))
}

/// `…/subagents/workflows/wf_<run>/agent-<id>.jsonl`.
fn is_workflow_transcript(path: &Path) -> bool {
    let mut parts = path.components().rev();
    let file = parts.next().map(|part| part.as_os_str().to_string_lossy());
    let run = parts.next().map(|part| part.as_os_str().to_string_lossy());
    let workflows = parts.next().map(|part| part.as_os_str().to_string_lossy());
    let subagents = parts.next().map(|part| part.as_os_str().to_string_lossy());
    file.is_some_and(|name| name.starts_with("agent-") && name.ends_with(".jsonl"))
        && run.is_some_and(|name| name.starts_with("wf_"))
        && workflows.as_deref() == Some("workflows")
        && subagents.as_deref() == Some("subagents")
}

fn agent_id_from_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy();
    Some(
        name.strip_prefix("agent-")?
            .strip_suffix(".jsonl")?
            .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_protocol::{
        Completeness, Knowledge, LifecyclePayload, LifecycleTopic, NativeLifecycle,
        NativeRequestKey, ObservationSource, RuntimeCursor, Severity, SourceCursor, SourceDelivery,
        Timestamp,
    };
    use std::collections::BTreeMap;

    fn hook_observation(name: &str, related: BTreeMap<String, String>) -> Observation {
        Observation {
            schema_version: SchemaVersion,
            event_id: EventId::new(),
            journal_id: Id::new("obj").unwrap(),
            instance_id: InstanceId::new(),
            run_id: Some(RunId::new()),
            host_id: HostId::new(),
            process_generation: U64(1),
            run_generation: Some(U64(1)),
            seq: U64(1),
            observed_at: Timestamp::try_from("2026-09-15T00:00:00.000Z".to_string()).unwrap(),
            native_at: Knowledge::NotApplicable,
            source: ObservationSource {
                driver_kind: DriverKind::ShellPty,
                driver_version: "test".into(),
                adapter_version: "test".into(),
                channel: SourceChannel::Hook,
                delivery: SourceDelivery::Live,
                native_session_id: Knowledge::NotApplicable,
                native_turn_id: Knowledge::NotApplicable,
                native_agent_id: Knowledge::NotApplicable,
                native_item_id: Knowledge::NotApplicable,
                native_event_id: Knowledge::NotApplicable,
                native_request_id: NativeRequestKey::None,
                source_cursor: SourceCursor::Runtime(Box::new(RuntimeCursor {
                    ledger_revision: U64(1),
                })),
            },
            completeness: Completeness::Structured,
            raw_ref: None,
            evidence_event_ids: Vec::new(),
            body: ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
                NativeLifecycle {
                    topic: LifecycleTopic::Diagnostic,
                    native_name: name.into(),
                    native_id: Knowledge::NotApplicable,
                    status: Knowledge::Known {
                        value: "observed".into(),
                    },
                    related_ids: related,
                    data_ref: None,
                    severity: Severity::Info,
                    affects_completion: false,
                },
            )))),
        }
    }

    #[test]
    fn workflow_transcript_paths_are_recognized() {
        assert!(is_workflow_transcript(Path::new(
            "/w/projects/s/subagents/workflows/wf_1/agent-abcd1234.jsonl"
        )));
        assert!(!is_workflow_transcript(Path::new(
            "/w/projects/s/subagents/agent-abcd1234.jsonl"
        )));
        assert_eq!(
            agent_id_from_path(Path::new(
                "/w/subagents/workflows/wf_1/agent-abcd1234.jsonl"
            )),
            Some("abcd1234".to_string())
        );
    }

    #[test]
    fn launch_without_run_id_emits_nothing() {
        let mut producer = WorkflowProducer::new(InstanceId::new());
        let out = producer.on_observation(&hook_observation(
            "PostToolUse",
            BTreeMap::from([("toolName".into(), "Workflow".into())]),
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn non_workflow_hooks_are_ignored() {
        let mut producer = WorkflowProducer::new(InstanceId::new());
        assert!(
            producer
                .on_observation(&hook_observation(
                    "SubagentStart",
                    BTreeMap::from([
                        ("agentType".into(), "general-purpose".into()),
                        ("agentId".into(), "a1".into()),
                    ]),
                ))
                .is_empty()
        );
        assert!(
            producer
                .on_observation(&hook_observation(
                    "PostToolUse",
                    BTreeMap::from([("toolName".into(), "Bash".into())]),
                ))
                .is_empty()
        );
    }
}
