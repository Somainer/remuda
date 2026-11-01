//! D-057 §7.4–§7.6 (ma-fence): the Node execution boundary.
//!
//! The Node persists every `lineage.fence` it receives (idempotently by
//! fence id), keeps the live generation per lineage in memory, and from the
//! moment a fence applies it
//! - cancels the not-yet-delivered work of initiators of that lineage below
//!   the live generation (commands accepted but not started, PTY prompts
//!   still queued) and closes the lineage's local instances below it;
//! - refuses later work from such an initiator at `insert_command`, at PTY or
//!   driver delivery (which re-checks before writing), and at the dispatcher
//!   entry of every direct method (`interaction.answer`, `worker.provision`,
//!   `worker.remove`, `worktree.lease`, `worktree.return`, `gate.run`);
//! - reports in its acknowledgement what it cancelled, what had already run,
//!   what it refused, and what it cannot establish (`unknown`).
//!
//! The dispatch claim ([`NodeFences::claim`]) and the fence application run
//! under one lock, so a command is either claimed for delivery (its durable
//! `transport-written` marker is saved first, and a crash after it reads
//! `unknown`) or cancelled — never both.

use crate::NodeError;
use remuda_protocol::{FenceOpOutcome, FenceOutcome, Initiator, LineageRef};
use rusqlite::{Connection, params};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

/// The outcome recorded for one initiator-carrying direct operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DirectOutcome {
    /// It reached its executor and returned.
    Ran,
    /// Refused at entry because its initiator was fenced.
    Refused,
}

impl DirectOutcome {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "ran" => Some(Self::Ran),
            "refused" => Some(Self::Refused),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct DirectOp {
    initiator: Initiator,
    /// `None`: the entry is durable but no outcome was recorded — after a
    /// crash the Node cannot tell whether it reached its executor.
    outcome: Option<DirectOutcome>,
}

#[derive(Default)]
pub(crate) struct FenceTable {
    /// lineage → live generation.
    live: HashMap<String, u64>,
    /// instance → (lineage, generation), from create/resume params.
    lineages: HashMap<String, (String, u64)>,
    /// op id → entry.
    direct_ops: BTreeMap<String, DirectOp>,
}

impl FenceTable {
    /// Whether `initiator` is below its lineage's live generation.
    pub(crate) fn is_fenced(&self, initiator: &Initiator) -> bool {
        let generation = u64::try_from(initiator.generation).unwrap_or(0);
        self.live
            .get(&initiator.lineage_id)
            .is_some_and(|live| *live > generation)
    }
}

/// Persisted fences, instance lineages and the direct-operation log.
pub(crate) struct NodeFences {
    table: Mutex<FenceTable>,
    db: Option<Mutex<Connection>>,
}

impl NodeFences {
    /// In-memory only (no data dir configured).
    pub(crate) fn in_memory() -> Self {
        Self {
            table: Mutex::new(FenceTable::default()),
            db: None,
        }
    }

    /// Open `<dir>/lineage-fences.sqlite` and load everything it holds, so a
    /// restarted daemon applies its persisted fences before it serves or
    /// delivers anything.
    pub(crate) fn open(dir: &Path) -> Result<Self, NodeError> {
        std::fs::create_dir_all(dir)?;
        let conn = Connection::open(dir.join("lineage-fences.sqlite"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS fences (
                fence_id TEXT PRIMARY KEY,
                lineage_id TEXT NOT NULL,
                live_generation INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS instance_lineages (
                instance_id TEXT PRIMARY KEY,
                lineage_id TEXT NOT NULL,
                generation INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS direct_ops (
                op_id TEXT PRIMARY KEY,
                method TEXT NOT NULL,
                initiator_json TEXT NOT NULL,
                outcome TEXT
             );",
        )?;
        let mut table = FenceTable::default();
        {
            let mut stmt = conn.prepare("SELECT lineage_id, live_generation FROM fences")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (lineage, live) = row?;
                let live = u64::try_from(live).unwrap_or(0);
                let entry = table.live.entry(lineage).or_insert(live);
                *entry = (*entry).max(live);
            }
            let mut stmt =
                conn.prepare("SELECT instance_id, lineage_id, generation FROM instance_lineages")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                let (instance, lineage, generation) = row?;
                table
                    .lineages
                    .insert(instance, (lineage, u64::try_from(generation).unwrap_or(0)));
            }
            let mut stmt = conn.prepare("SELECT op_id, initiator_json, outcome FROM direct_ops")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?;
            for row in rows {
                let (op_id, initiator, outcome) = row?;
                let Ok(initiator) = serde_json::from_str::<Initiator>(&initiator) else {
                    continue;
                };
                table.direct_ops.insert(
                    op_id,
                    DirectOp {
                        initiator,
                        outcome: outcome.as_deref().and_then(DirectOutcome::parse),
                    },
                );
            }
        }
        Ok(Self {
            table: Mutex::new(table),
            db: Some(Mutex::new(conn)),
        })
    }

    /// Lock the table. Fence application and the dispatch claim both hold it.
    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, FenceTable>, NodeError> {
        self.table.lock().map_err(|_| NodeError::StorePoisoned)
    }

    fn db_exec(&self, sql: &str, params: impl rusqlite::Params) -> Result<(), NodeError> {
        if let Some(db) = &self.db {
            db.lock()
                .map_err(|_| NodeError::StorePoisoned)?
                .execute(sql, params)?;
        }
        Ok(())
    }

    /// Whether `initiator` is fenced (a quick check outside the claim).
    pub(crate) fn is_fenced(&self, initiator: Option<&Initiator>) -> Result<bool, NodeError> {
        let Some(initiator) = initiator else {
            return Ok(false);
        };
        Ok(self.lock()?.is_fenced(initiator))
    }

    /// Refuse a fenced initiator at an entry point (§7.6).
    pub(crate) fn refuse_fenced(&self, initiator: Option<&Initiator>) -> Result<(), NodeError> {
        if self.is_fenced(initiator)? {
            return Err(NodeError::Fenced);
        }
        Ok(())
    }

    /// Persist the fence and raise the live generation. Idempotent by fence
    /// id; never moves a generation backwards. Called inline by carriers so
    /// a frame read after the fence frame sees it.
    pub(crate) fn persist(
        &self,
        table: &mut FenceTable,
        fence_id: &str,
        lineage_id: &str,
        live_generation: u64,
    ) -> Result<(), NodeError> {
        self.db_exec(
            "INSERT OR IGNORE INTO fences (fence_id, lineage_id, live_generation)
             VALUES (?1, ?2, ?3)",
            params![
                fence_id,
                lineage_id,
                i64::try_from(live_generation).unwrap_or(i64::MAX)
            ],
        )?;
        let entry = table
            .live
            .entry(lineage_id.to_owned())
            .or_insert(live_generation);
        *entry = (*entry).max(live_generation);
        Ok(())
    }

    /// Record an instance's lineage and generation from its create/resume.
    pub(crate) fn record_lineage(
        &self,
        instance_id: &str,
        lineage: &LineageRef,
    ) -> Result<(), NodeError> {
        self.db_exec(
            "INSERT OR REPLACE INTO instance_lineages (instance_id, lineage_id, generation)
             VALUES (?1, ?2, ?3)",
            params![
                instance_id,
                lineage.id,
                i64::try_from(lineage.generation).unwrap_or(i64::MAX)
            ],
        )?;
        self.lock()?.lineages.insert(
            instance_id.to_owned(),
            (lineage.id.clone(), lineage.generation),
        );
        Ok(())
    }

    /// §7.5 item 5: durably record an initiator-carrying direct operation at
    /// entry, before any side effect — or refuse it when its initiator is
    /// fenced (recorded as refused). Returns the op id to settle afterwards.
    pub(crate) fn direct_entry(
        &self,
        method: &str,
        params: &Value,
    ) -> Result<Option<String>, NodeError> {
        let Some(initiator) = crate::origin::wire_initiator(params) else {
            return Ok(None);
        };
        let op_id = params
            .get("opId")
            .or_else(|| params.get("jobId"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                NodeError::InvalidRequest(format!("{method} with an initiator requires opId"))
            })?;
        let initiator_json = serde_json::to_string(&initiator)?;
        let mut table = self.lock()?;
        if table.is_fenced(&initiator) {
            self.db_exec(
                "INSERT INTO direct_ops (op_id, method, initiator_json, outcome)
                 VALUES (?1, ?2, ?3, 'refused')
                 ON CONFLICT(op_id) DO UPDATE SET outcome = COALESCE(direct_ops.outcome, 'refused')",
                params![op_id, method, initiator_json],
            )?;
            table.direct_ops.entry(op_id).or_insert(DirectOp {
                initiator,
                outcome: Some(DirectOutcome::Refused),
            });
            return Err(NodeError::Fenced);
        }
        self.db_exec(
            "INSERT OR IGNORE INTO direct_ops (op_id, method, initiator_json, outcome)
             VALUES (?1, ?2, ?3, NULL)",
            params![op_id, method, initiator_json],
        )?;
        table.direct_ops.entry(op_id.clone()).or_insert(DirectOp {
            initiator,
            outcome: None,
        });
        Ok(Some(op_id))
    }

    /// Record a direct operation's outcome after it returned.
    pub(crate) fn direct_outcome(&self, op_id: &str) -> Result<(), NodeError> {
        self.db_exec(
            "UPDATE direct_ops SET outcome = 'ran' WHERE op_id = ?1 AND outcome IS NULL",
            params![op_id],
        )?;
        if let Some(op) = self.lock()?.direct_ops.get_mut(op_id)
            && op.outcome.is_none()
        {
            op.outcome = Some(DirectOutcome::Ran);
        }
        Ok(())
    }

    /// Outcomes of the direct ops of `lineage` below `live`.
    pub(crate) fn direct_outcomes(
        table: &FenceTable,
        lineage: &str,
        live: u64,
    ) -> Vec<FenceOpOutcome> {
        table
            .direct_ops
            .iter()
            .filter(|(_, op)| {
                op.initiator.lineage_id == lineage
                    && u64::try_from(op.initiator.generation).unwrap_or(0) < live
            })
            .map(|(op_id, op)| FenceOpOutcome {
                op_id: op_id.clone(),
                outcome: match op.outcome {
                    Some(DirectOutcome::Ran) => FenceOutcome::Ran,
                    Some(DirectOutcome::Refused) => FenceOutcome::Refused,
                    None => FenceOutcome::Unknown,
                },
            })
            .collect()
    }

    /// The instances this Node knows of for `lineage` below `live`.
    pub(crate) fn instances_below(table: &FenceTable, lineage: &str, live: u64) -> Vec<String> {
        let mut ids: Vec<String> = table
            .lineages
            .iter()
            .filter(|(_, (owner, generation))| owner == lineage && *generation < live)
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }
}

/// Whether a settled command was rejected by a fence.
fn settled_fenced(command: &remuda_protocol::Command) -> bool {
    matches!(
        &command.settlement,
        remuda_protocol::Knowledge::Known { value }
            if value.outcome == remuda_protocol::SettlementOutcome::Rejected
                && value
                    .error
                    .as_ref()
                    .is_some_and(|error| error.message == remuda_protocol::FENCED_REASON)
    )
}

/// The dispatch claim's verdict for one command.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Claim {
    /// Deliver it: its durable `transport-written` marker is saved.
    Deliver,
    /// Do not deliver it: a fence cancelled or refused it (already settled).
    Fenced,
}

/// The dispatch claim (§7.4): before a command's text or control reaches its
/// executor, re-check the fence and record the durable `transport-written`
/// marker, under the fence lock. A fenced command is settled
/// `rejected(fenced)` here (refused at delivery) unless a fence already
/// cancelled it. Commands without an initiator are never fenced and are not
/// marked.
pub(crate) fn claim(
    store: &dyn crate::store::LocalStore,
    fences: &NodeFences,
    command_id: &remuda_protocol::CommandId,
) -> Result<Claim, NodeError> {
    let command = store.get_command(command_id)?;
    let Some(initiator) = command.initiator.clone() else {
        return Ok(Claim::Deliver);
    };
    let table = fences.lock()?;
    let mut command = store.get_command(command_id)?;
    if command.state == remuda_protocol::CommandState::Settled {
        return Ok(if settled_fenced(&command) {
            Claim::Fenced
        } else {
            Claim::Deliver
        });
    }
    if table.is_fenced(&initiator) {
        let Some(instance_id) = command.target.instance_id.clone() else {
            return Ok(Claim::Fenced);
        };
        crate::runtime::reject_before_dispatch(
            store,
            &instance_id,
            command,
            remuda_protocol::FENCED_REASON,
        )?;
        return Ok(Claim::Fenced);
    }
    command.dispatch = remuda_protocol::DispatchState::TransportWritten;
    store.save_command(command)?;
    Ok(Claim::Deliver)
}

impl crate::DevNode {
    /// Persist and raise a `lineage.fence` frame's fence without applying
    /// it (carriers that run frames concurrently call this inline, in read
    /// order); [`Self::apply_lineage_fence`] then does the rest.
    pub(crate) fn persist_fence_frame(&self, params: &Value) -> Result<(), NodeError> {
        let params: remuda_protocol::LineageFenceParams = serde_json::from_value(params.clone())?;
        let fences = &self.inner.fences;
        let mut table = fences.lock()?;
        fences.persist(
            &mut table,
            &params.fence_id,
            &params.lineage_id,
            params.live_generation,
        )
    }

    /// create/resume admission (§7.6): refuse a fenced initiator (or a
    /// chapter whose own generation its lineage already moved past) before
    /// any side effect, and record the instance's lineage so a later fence
    /// closes it.
    pub(crate) fn admit_create(&self, params: &Value) -> Result<(), NodeError> {
        let fences = &self.inner.fences;
        fences.refuse_fenced(crate::origin::wire_initiator(params).as_ref())?;
        let lineage = params
            .get("lineage")
            .cloned()
            .and_then(|value| serde_json::from_value::<LineageRef>(value).ok());
        let instance_id = params
            .get("instanceId")
            .or_else(|| params.pointer("/spec/instanceId"))
            .and_then(Value::as_str);
        if let (Some(lineage), Some(instance_id)) = (lineage, instance_id) {
            if fences
                .lock()?
                .live
                .get(&lineage.id)
                .is_some_and(|live| *live > lineage.generation)
            {
                return Err(NodeError::Fenced);
            }
            fences.record_lineage(instance_id, &lineage)?;
        }
        Ok(())
    }

    /// Apply `lineage.fence` (§7.6): persist it, cancel the fenced
    /// generation's undelivered commands, report every operation this Node
    /// holds a record of, and close the lineage's local instances below the
    /// live generation (their pending interactions with them).
    pub async fn apply_lineage_fence(&self, params: &Value) -> Result<Value, NodeError> {
        let params: remuda_protocol::LineageFenceParams = serde_json::from_value(params.clone())?;
        let live = params.live_generation;
        let lineage = params.lineage_id.clone();
        let store = self.inner.store.as_ref();
        let mut outcomes: Vec<FenceOpOutcome> = Vec::new();
        let close: Vec<remuda_protocol::InstanceId>;
        {
            let fences = &self.inner.fences;
            let mut table = fences.lock()?;
            fences.persist(&mut table, &params.fence_id, &lineage, live)?;
            for instance in store.list_instances()? {
                let snapshot =
                    store.snapshot(&instance.meta.id, self.inner.projection_epoch.clone())?;
                for command in snapshot.commands {
                    let Some(initiator) = &command.initiator else {
                        continue;
                    };
                    if initiator.lineage_id != lineage
                        || u64::try_from(initiator.generation).unwrap_or(0) >= live
                    {
                        continue;
                    }
                    let op_id = command.command_id.as_id().as_str().to_owned();
                    let outcome = if command.state == remuda_protocol::CommandState::Settled {
                        match &command.settlement {
                            remuda_protocol::Knowledge::Known { value }
                                if value.outcome
                                    == remuda_protocol::SettlementOutcome::Completed =>
                            {
                                Some(FenceOutcome::Ran)
                            }
                            _ if settled_fenced(&command) => Some(FenceOutcome::Cancelled),
                            // Settled otherwise: nothing this Node can add.
                            _ => None,
                        }
                    } else if matches!(
                        command.dispatch,
                        remuda_protocol::DispatchState::TransportWritten
                            | remuda_protocol::DispatchState::NativeAcknowledged
                    ) {
                        // The write was attempted; it may still complete and
                        // report itself through its own settlement.
                        Some(FenceOutcome::Unknown)
                    } else if let Some(instance_id) = command.target.instance_id.clone() {
                        crate::runtime::reject_before_dispatch(
                            store,
                            &instance_id,
                            command,
                            remuda_protocol::FENCED_REASON,
                        )?;
                        Some(FenceOutcome::Cancelled)
                    } else {
                        Some(FenceOutcome::Unknown)
                    };
                    if let Some(outcome) = outcome {
                        outcomes.push(FenceOpOutcome { op_id, outcome });
                    }
                }
            }
            outcomes.extend(NodeFences::direct_outcomes(&table, &lineage, live));
            close = NodeFences::instances_below(&table, &lineage, live)
                .into_iter()
                .filter_map(|id| id.parse().ok())
                .collect();
        }
        let mut closed = Vec::new();
        for instance_id in close {
            let Ok(instance) = store.get_instance(&instance_id) else {
                continue;
            };
            if matches!(
                instance.lifecycle,
                remuda_protocol::InstanceLifecycle::Exited
                    | remuda_protocol::InstanceLifecycle::Failed
            ) {
                continue;
            }
            let request: crate::InstanceCommandRequest =
                serde_json::from_value(serde_json::json!({
                    "origin": "human",
                    "operation": "close",
                }))?;
            match self.submit_command(&instance_id, request).await {
                Ok(_) => closed.push(instance_id.as_id().as_str().to_owned()),
                Err(error) => tracing::warn!(
                    %error,
                    instance_id = instance_id.as_id().as_str(),
                    "fence could not close a fenced chapter"
                ),
            }
            self.inner
                .interactions
                .cancel_for_instance(&instance_id)
                .await;
        }
        Ok(serde_json::to_value(remuda_protocol::LineageFenceResult {
            fence_id: params.fence_id,
            closed_instance_ids: closed,
            outcomes,
        })?)
    }

    /// §7.5 item 5: run an initiator-carrying direct operation behind the
    /// entry check and the durable entry/outcome log.
    pub(crate) async fn direct_op<F>(
        &self,
        method: &str,
        params: &Value,
        run: F,
    ) -> Result<Value, NodeError>
    where
        F: std::future::Future<Output = Result<Value, NodeError>>,
    {
        let op_id = self.inner.fences.direct_entry(method, params)?;
        let result = run.await;
        if let Some(op_id) = op_id
            && result.is_ok()
        {
            self.inner.fences.direct_outcome(&op_id)?;
        }
        result
    }
}

#[cfg(test)]
mod tests;
