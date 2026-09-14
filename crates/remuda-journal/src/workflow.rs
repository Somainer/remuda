//! Workflow journal tailer (`subagents/workflows/wf_*/journal.jsonl`).

use crate::Error;
use crate::claude::{NativeIds, envelope, opaque};
use crate::envelope::Envelope;
use crate::source::{FileTail, MapContext, Source, SourceResume};
use crate::util::{known, unknown};
use remuda_protocol::{
    Completeness, FileCursor, ObservationPayload, OpaqueReason, SourceChannel, U64, WorkflowEngine,
    WorkflowMemberPayload, WorkflowRunPayload, WorkflowState,
};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Tails every `wf_*/journal.jsonl` under a Claude session directory.
#[derive(Debug, Clone)]
pub struct WorkflowJournalTailer {
    session_dir: PathBuf,
    ctx: MapContext,
    tails: HashMap<PathBuf, FileTail>,
    ids: NativeIds,
    meta: HashMap<String, Value>,
}

impl WorkflowJournalTailer {
    /// `session_dir` is `<projects>/<enc>/<sid>/` (the directory that contains `subagents/`).
    pub fn new(session_dir: impl Into<PathBuf>, mut ctx: MapContext) -> Self {
        ctx.channel = SourceChannel::WorkflowJournal;
        Self {
            session_dir: session_dir.into(),
            ctx,
            tails: HashMap::new(),
            ids: NativeIds::new(),
            meta: HashMap::new(),
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

    /// Poll every known and newly discovered workflow journal.
    pub fn poll(&mut self) -> Result<Vec<Envelope>, Error> {
        self.refresh_tails()?;
        let mut out = Vec::new();
        let paths: Vec<PathBuf> = self.tails.keys().cloned().collect();
        for path in paths {
            self.load_meta(&path)?;
            let lines = {
                let tail = self
                    .tails
                    .get_mut(&path)
                    .ok_or_else(|| Error::Path(path.clone()))?;
                tail.poll()?
            };
            for (line, cursor) in lines {
                out.extend(self.map_workflow_line(&path, &line, &cursor)?);
            }
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

    fn refresh_tails(&mut self) -> Result<(), Error> {
        for path in discover_journals(&self.session_dir)? {
            if let std::collections::hash_map::Entry::Vacant(entry) = self.tails.entry(path) {
                let tail = FileTail::new(entry.key().clone())?;
                entry.insert(tail);
            }
        }
        Ok(())
    }

    fn load_meta(&mut self, journal_path: &Path) -> Result<(), Error> {
        let dir = journal_path.parent();
        let Some(dir) = dir else {
            return Ok(());
        };
        let wf_id = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(parent) = dir.parent() {
            let snap = parent.join(format!("{wf_id}.json"));
            if snap.is_file() {
                let bytes = fs::read(&snap)?;
                if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                    self.meta.insert(wf_id.clone(), value);
                }
            }
        }
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.ends_with(".meta.json") {
                    let bytes = fs::read(entry.path())?;
                    if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                        let agent = name
                            .trim_start_matches("agent-")
                            .trim_end_matches(".meta.json");
                        self.meta.insert(agent.to_owned(), value);
                    }
                }
            }
        }
        Ok(())
    }

    fn map_workflow_line(
        &mut self,
        journal_path: &Path,
        line: &[u8],
        cursor: &FileCursor,
    ) -> Result<Vec<Envelope>, Error> {
        let value = match serde_json::from_slice::<Value>(line) {
            Ok(value) => value,
            Err(_) => {
                return Ok(vec![opaque(
                    &self.ctx,
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
            "launched" => {
                let workflow_id = self.ids.workflow(&wf_native)?;
                let title = self
                    .meta
                    .get(&wf_native)
                    .and_then(|m| m.get("title").or_else(|| m.get("result")))
                    .and_then(Value::as_str)
                    .map(|s| known(s.to_owned()))
                    .unwrap_or_else(|| unknown("not-emitted"));
                Ok(vec![envelope(
                    &self.ctx,
                    cursor,
                    line,
                    &value,
                    Completeness::Structured,
                    Some(wf_native.as_str()),
                    ObservationPayload::WorkflowRun(Box::new(WorkflowRunPayload {
                        workflow_id,
                        engine: WorkflowEngine::ClaudeWorkflow,
                        native_run_id: known(wf_native.clone()),
                        native_task_id: unknown("not-emitted"),
                        tool_call_id: None,
                        state: WorkflowState::Running,
                        revision: U64(1),
                        title,
                        name: None,
                        description: None,
                        totals: None,
                        live: None,
                        note: None,
                        result_ref: None,
                    })),
                )?])
            }
            "started" => {
                let key = value.get("key").and_then(Value::as_str).unwrap_or("member");
                let agent_id = value.get("agentId").and_then(Value::as_str).unwrap_or(key);
                let member_id = self.ids.member(&format!("{wf_native}:{key}"))?;
                let workflow_id = self.ids.workflow(&wf_native)?;
                let label = value
                    .get("label")
                    .and_then(Value::as_str)
                    .map(|s| known(s.to_owned()))
                    .unwrap_or_else(|| unknown("not-emitted"));
                let model = self
                    .meta
                    .get(agent_id)
                    .and_then(|m| m.get("model"))
                    .and_then(Value::as_str)
                    .map(|s| known(s.to_owned()))
                    .unwrap_or_else(|| unknown("not-emitted"));
                Ok(vec![envelope(
                    &self.ctx,
                    cursor,
                    line,
                    &value,
                    Completeness::Structured,
                    Some(agent_id),
                    ObservationPayload::WorkflowMember(Box::new(WorkflowMemberPayload {
                        workflow_id,
                        member_id,
                        native_agent_id: known(agent_id.to_owned()),
                        native_key: known(key.to_owned()),
                        attempt: unknown("not-emitted"),
                        phase_id: None,
                        label,
                        state: WorkflowState::Running,
                        model_requested: model.clone(),
                        model_resolved: model,
                        result_ref: None,
                        revision: U64(1),
                        latest_tool: None,
                        tokens: None,
                        calls: None,
                        duration_ms: None,
                        started_at: None,
                        ended_at: None,
                    })),
                )?])
            }
            "result" => {
                let key = value.get("key").and_then(Value::as_str).unwrap_or("member");
                let agent_id = value.get("agentId").and_then(Value::as_str).unwrap_or(key);
                let member_id = self.ids.member(&format!("{wf_native}:{key}"))?;
                let workflow_id = self.ids.workflow(&wf_native)?;
                Ok(vec![envelope(
                    &self.ctx,
                    cursor,
                    line,
                    &value,
                    Completeness::Structured,
                    Some(agent_id),
                    ObservationPayload::WorkflowMember(Box::new(WorkflowMemberPayload {
                        workflow_id,
                        member_id,
                        native_agent_id: known(agent_id.to_owned()),
                        native_key: known(key.to_owned()),
                        attempt: unknown("not-emitted"),
                        phase_id: None,
                        label: unknown("not-emitted"),
                        state: WorkflowState::Completed,
                        model_requested: unknown("not-emitted"),
                        model_resolved: unknown("not-emitted"),
                        result_ref: None,
                        revision: U64(2),
                        latest_tool: None,
                        tokens: None,
                        calls: None,
                        duration_ms: None,
                        started_at: None,
                        ended_at: None,
                    })),
                )?])
            }
            other => Ok(vec![opaque(
                &self.ctx,
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

impl Source for WorkflowJournalTailer {
    fn name(&self) -> &'static str {
        "workflow-journal"
    }

    fn map_line(&mut self, line: &[u8], cursor: FileCursor) -> Result<Vec<Envelope>, Error> {
        let dummy = self
            .session_dir
            .join("subagents/workflows/wf_unknown/journal.jsonl");
        self.map_workflow_line(&dummy, line, &cursor)
    }
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

fn workflow_native_id(journal_path: &Path) -> String {
    journal_path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "wf_unknown".into())
}
