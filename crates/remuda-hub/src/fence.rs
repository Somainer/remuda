//! D-057 §7.2–§7.7 (ma-fence): the fence transaction F, the in-memory fence
//! map the send gate reads, and fence delivery to Nodes.
//!
//! F moves authority over a lineage in ONE writer transaction with a
//! compare-and-set on the lineage generation G (see [`fence_core`]). After it
//! commits, [`after_commit`] updates the fence map, revokes the
//! predecessor's API egress contexts and delivers the fence to every online
//! fence-capable host; [`deliver_host`] also runs first after every hello.
//!
//! Every operation of the fenced generation that was unfinished at F gets a
//! `fence_ops` row keyed by `(fence_id, op_kind, op_id)` and stamped with the
//! op's own initiator generation. An op is recorded under exactly one fence
//! (the first that covered its generation), and a Node acknowledgement only
//! settles rows of the fence it names, so a late result about an old chapter
//! can never settle anything recorded for a newer one.

use crate::AppState;
use crate::store::{
    CommandRecord, SettledInteraction, Settlement, Store, StoreError, check_initiator,
    insert_settlement_event, load_lineage,
};
use remuda_protocol::{
    FENCED_REASON, Initiator, LINEAGE_FENCE_CAPABILITY, LineageFenceParams, LineageFenceResult,
    METHOD_LINEAGE_FENCE,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

/// `fence_ops.outcome` values (main-agent.md §7.4).
pub(crate) mod outcome {
    /// Admitted but never forward-intended or sent; cancelled by F.
    pub const CANCELLED_AT_HUB: &str = "cancelled-at-hub";
    /// Forward-intended before F, stopped by the send gate.
    pub const WITHHELD: &str = "withheld";
    /// Potentially executed: unknown until a definite outcome arrives.
    pub const UNKNOWN: &str = "unknown";
    /// A Node-started gate job: it completes.
    pub const RUNNING: &str = "running";
    /// Reached its executor.
    pub const RAN: &str = "ran";
    /// Cancelled by the Node when it applied the fence.
    pub const CANCELLED_BY_NODE: &str = "cancelled-by-node";
    /// Refused by the Node at entry after it applied the fence.
    pub const REFUSED_BY_NODE: &str = "refused-by-node";
}

/// Idempotent DDL; no `user_version` rung.
pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS fences (
            fence_id TEXT PRIMARY KEY,
            lineage_id TEXT NOT NULL,
            fenced_generation INTEGER NOT NULL,
            live_generation INTEGER NOT NULL,
            reason TEXT NOT NULL,
            actor_json TEXT NOT NULL,
            predecessor_instance_id TEXT NOT NULL,
            successor_instance_id TEXT,
            close_command_id TEXT,
            successor_command_id TEXT,
            created_at TEXT NOT NULL,
            UNIQUE (lineage_id, live_generation)
         );
         CREATE TABLE IF NOT EXISTS fence_deliveries (
            fence_id TEXT NOT NULL,
            host_id TEXT NOT NULL,
            state TEXT NOT NULL,
            acked_at TEXT,
            result_json TEXT,
            PRIMARY KEY (fence_id, host_id)
         );
         CREATE INDEX IF NOT EXISTS fence_deliveries_host ON fence_deliveries(host_id, state);
         CREATE TABLE IF NOT EXISTS fence_ops (
            fence_id TEXT NOT NULL,
            op_kind TEXT NOT NULL,
            op_id TEXT NOT NULL,
            host_id TEXT,
            lineage_id TEXT NOT NULL,
            generation INTEGER NOT NULL,
            operation TEXT,
            outcome TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (fence_id, op_kind, op_id)
         );
         CREATE INDEX IF NOT EXISTS fence_ops_op ON fence_ops(op_kind, op_id);",
    )?;
    crate::store::ensure_column(conn, "interactions", "superseded_by", "TEXT")?;
    Ok(())
}

/// In-memory fence map (lineage → live generation) read by the send gate,
/// plus per-host delivery serialization and the commands a host's
/// unacknowledged fences held back.
#[derive(Clone, Default)]
pub(crate) struct FenceState {
    live: Arc<std::sync::RwLock<HashMap<String, i64>>>,
    delivering: Arc<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    held: Arc<std::sync::Mutex<HashMap<String, HashSet<String>>>>,
}

impl FenceState {
    /// Send gate predicate: a lineage whose live generation moved past the
    /// initiator's generation has fenced it (pause included, since every F
    /// bumps the generation).
    pub(crate) fn is_fenced(&self, initiator: &Initiator) -> bool {
        self.live
            .read()
            .map(|map| {
                map.get(&initiator.lineage_id)
                    .is_some_and(|live| *live > initiator.generation)
            })
            .unwrap_or(false)
    }

    /// Record a committed live generation; never moves backwards.
    pub(crate) fn raise(&self, lineage_id: &str, live_generation: i64) {
        if let Ok(mut map) = self.live.write() {
            let entry = map.entry(lineage_id.to_owned()).or_insert(live_generation);
            if *entry < live_generation {
                *entry = live_generation;
            }
        }
    }

    fn host_lock(&self, host_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self
            .delivering
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.entry(host_id.to_owned()).or_default().clone()
    }

    /// Remember a command `forward_if_online` held for this host's
    /// unacknowledged fences, so the acknowledgement can forward it.
    pub(crate) fn note_held(&self, host_id: &str, command_id: &str) {
        let mut map = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.entry(host_id.to_owned())
            .or_default()
            .insert(command_id.to_owned());
    }

    fn take_held(&self, host_id: &str) -> Vec<String> {
        let mut map = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ids: Vec<String> = map
            .remove(host_id)
            .unwrap_or_default()
            .into_iter()
            .collect();
        ids.sort();
        ids
    }
}

/// Why F fences the predecessor.
#[derive(Clone, Debug)]
pub(crate) enum FenceMode {
    /// Without successor: the lineage becomes `paused{by}`.
    Pause {
        /// `{kind: device|self|ancestor|restart-cap|process-exit, …}`.
        paused_by: Value,
        /// A Hub-internal `instance.close` for the predecessor, queued inside
        /// F (no initiator: the close follows the fence, it is not refused
        /// by it). `(command id, payload)`.
        close: Option<(String, Value)>,
    },
    /// With successor: the lineage becomes `starting` on `successor_id`,
    /// which the caller inserts in the same transaction.
    Successor { successor_id: String },
}

/// One F request.
#[derive(Clone, Debug)]
pub(crate) struct FenceSpec {
    pub lineage_id: String,
    /// Generation CAS: F commits only while the lineage is still at it.
    pub expected_generation: i64,
    /// Lineage states F may start from.
    pub from_states: &'static [&'static str],
    pub mode: FenceMode,
    /// Audit actor chain (`{deviceId?, instanceId?, by}`).
    pub actor: Value,
    /// Commit-time authority of an Agent caller performing the fence.
    pub authority: Option<(Initiator, Option<String>)>,
    /// D-060 decision 10: a self-close is refused while the chapter has a
    /// pending interaction (checked in this transaction).
    pub self_close_guard: bool,
}

/// What F committed.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FenceCore {
    pub fence_id: String,
    pub lineage_id: String,
    pub fenced_generation: i64,
    pub live_generation: i64,
    pub predecessor_id: String,
    pub predecessor_host_id: String,
    /// Every known host; each got a pending delivery row.
    pub hosts: Vec<String>,
    #[serde(skip)]
    pub settlement: Settlement,
    #[serde(skip)]
    pub close_command: Option<CommandRecord>,
}

/// The F transaction's result.
#[derive(Debug)]
pub(crate) enum FenceTx {
    Committed(Box<FenceCore>),
    /// The lineage generation moved; nothing was written.
    CasLost,
    /// The lineage is in a state F may not start from (e.g. Resume on a
    /// lineage that is not paused); nothing was written.
    StateRefused,
    /// The self-close guard refused; nothing was written.
    Guarded(&'static str),
}

fn rows<T, P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    map: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>, StoreError> {
    let mut stmt = conn.prepare(sql)?;
    let out = stmt
        .query_map(params, map)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn record_op(
    conn: &Connection,
    fence_id: &str,
    kind: &str,
    op_id: &str,
    host_id: Option<&str>,
    lineage_id: &str,
    generation: i64,
    operation: Option<&str>,
    outcome: &str,
    now: &str,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT OR IGNORE INTO fence_ops
            (fence_id, op_kind, op_id, host_id, lineage_id, generation, operation, outcome, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![fence_id, kind, op_id, host_id, lineage_id, generation, operation, outcome, now],
    )?;
    Ok(())
}

/// Resolve a potentially-executed op from later evidence (its own reply, a
/// Node-started gate phase, the send gate). Only an `unknown` row moves; a
/// definite outcome never changes. Keyed by the op alone: an op belongs to
/// exactly one fence.
pub(crate) fn resolve_op_outcome(
    conn: &Connection,
    kind: &str,
    op_id: &str,
    to: &str,
) -> Result<usize, StoreError> {
    Ok(conn.execute(
        "UPDATE fence_ops SET outcome = ?3, updated_at = ?4
          WHERE op_kind = ?1 AND op_id = ?2 AND outcome = 'unknown'",
        params![kind, op_id, to, crate::config::now_rfc3339()],
    )?)
}

/// Stamp an interaction payload superseded (same markers as the
/// generation-ended invalidation, reason `superseded`).
fn supersede_interaction_payload(event: &mut Value, now: &str, superseded_by: &str) {
    for pointer in ["/payload/entity", "/payload/interaction"] {
        if let Some(entity) = event.pointer_mut(pointer).and_then(Value::as_object_mut) {
            entity.insert("state".into(), json!("invalidated"));
            entity.insert("blocking".into(), json!(false));
            entity.insert("answerable".into(), json!(false));
            entity.insert(
                "resolution".into(),
                json!({
                    "state": "known",
                    "value": {"reason": "superseded", "supersededBy": superseded_by, "eventIds": []},
                }),
            );
            entity.insert("updatedAt".into(), json!(now));
        }
    }
}

/// Steps 1–9 and 11 of F (main-agent.md §7.2), on the caller's transaction.
/// Step 10 (the successor chapter and its create) is the caller's, in the
/// same transaction, for [`FenceMode::Successor`].
pub(crate) fn fence_core(conn: &Connection, spec: &FenceSpec) -> Result<FenceTx, StoreError> {
    // §7.3: an Agent performing the fence (a self or ancestor close) must
    // itself still hold authority at commit time.
    if let Some((initiator, device_id)) = &spec.authority {
        check_initiator(conn, Some(initiator), device_id.as_deref())?;
    }
    let Some(lineage) = load_lineage(conn, &spec.lineage_id)? else {
        return Err(StoreError::Id("unknown lineage".into()));
    };
    if lineage.generation != spec.expected_generation {
        return Ok(FenceTx::CasLost);
    }
    if !spec.from_states.contains(&lineage.state.as_str()) {
        return Ok(FenceTx::StateRefused);
    }
    if spec.self_close_guard {
        let pending: bool = conn
            .query_row(
                "SELECT 1 FROM interactions WHERE instance_id = ?1 AND state = 'pending' LIMIT 1",
                params![lineage.current_instance_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if pending {
            return Ok(FenceTx::Guarded("pending-interaction"));
        }
    }
    let now = crate::config::now_rfc3339();
    let g = lineage.generation;
    let live = g + 1;
    let lineage_id = lineage.lineage_id.clone();
    let predecessor = lineage.current_instance_id.clone();
    if let FenceMode::Successor { successor_id } = &spec.mode
        && lineage.state == "paused"
    {
        return resume_paused(conn, spec, &lineage, successor_id, &now);
    }
    let predecessor_host: String = conn
        .query_row(
            "SELECT host_id FROM instances WHERE id = ?1",
            params![predecessor],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::Id("current chapter missing".into()))?;

    // 1. Generation CAS and the new state.
    let (state, current, paused_by, paused_at, reason, superseded_by) = match &spec.mode {
        FenceMode::Pause { paused_by, .. } => (
            "paused",
            predecessor.clone(),
            Some(paused_by.to_string()),
            Some(now.clone()),
            "paused",
            lineage_id.clone(),
        ),
        FenceMode::Successor { successor_id } => (
            "starting",
            successor_id.clone(),
            None,
            None,
            "superseded",
            successor_id.clone(),
        ),
    };
    let cas = conn.execute(
        "UPDATE lineages
            SET generation = ?1, state = ?2, current_instance_id = ?3,
                paused_by_json = ?4, paused_at = ?5, updated_at = ?6
          WHERE lineage_id = ?7 AND generation = ?8",
        params![
            live, state, current, paused_by, paused_at, now, lineage_id, g
        ],
    )?;
    if cas == 0 {
        return Ok(FenceTx::CasLost);
    }

    // 2. Fence the predecessor (a paused predecessor already is).
    conn.execute(
        "UPDATE instances SET fenced_at = ?1, updated_at = ?1 WHERE id = ?2 AND fenced_at IS NULL",
        params![now, predecessor],
    )?;

    // 3. Its launch credential and every MCP token minted for any chapter
    //    below the live generation.
    conn.execute(
        "DELETE FROM devices WHERE instance_id IN (
            SELECT id FROM instances WHERE lineage_id = ?1 AND generation < ?2
         ) OR instance_id = ?3",
        params![lineage_id, live, predecessor],
    )?;

    let fence_id = format!("fen_{}", uuid::Uuid::now_v7());

    // 4. Held commands of the fenced generation: cancelled at the Hub.
    let held: Vec<(String, String, String, i64)> = rows(
        conn,
        "SELECT id, host_id, operation, initiator_generation FROM commands
          WHERE initiator_lineage_id = ?1 AND initiator_generation <= ?2
            AND state = 'queued' AND forwarded = 0",
        params![lineage_id, g],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    for (id, host, operation, generation) in &held {
        conn.execute(
            "UPDATE commands
                SET state = 'settled', resolution = 'clear',
                    settlement_outcome = 'rejected', settlement_reason = ?2, updated_at = ?3
              WHERE id = ?1 AND state = 'queued' AND forwarded = 0",
            params![id, FENCED_REASON, now],
        )?;
        record_op(
            conn,
            &fence_id,
            "command",
            id,
            Some(host),
            &lineage_id,
            *generation,
            Some(operation),
            outcome::CANCELLED_AT_HUB,
            &now,
        )?;
    }

    // 5. Forward-intended and unsettled: potentially executed.
    let in_flight: Vec<(String, String, String, i64)> = rows(
        conn,
        "SELECT c.id, c.host_id, c.operation, c.initiator_generation FROM commands c
          WHERE c.initiator_lineage_id = ?1 AND c.initiator_generation <= ?2
            AND c.forwarded = 1 AND c.state != 'settled'
            AND NOT EXISTS (SELECT 1 FROM fence_ops f
                             WHERE f.op_kind = 'command' AND f.op_id = c.id)",
        params![lineage_id, g],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    for (id, host, operation, generation) in &in_flight {
        record_op(
            conn,
            &fence_id,
            "command",
            id,
            Some(host),
            &lineage_id,
            *generation,
            Some(operation),
            outcome::UNKNOWN,
            &now,
        )?;
    }

    // 6. Gate jobs.
    let jobs: Vec<(String, String)> = rows(
        conn,
        "SELECT id, doc_json FROM gate_jobs
          WHERE json_extract(doc_json, '$.initiator.lineageId') = ?1
            AND CAST(json_extract(doc_json, '$.initiator.generation') AS INTEGER) <= ?2
            AND state IN ('queued', 'running', 'landing', 'canceling')
            AND NOT EXISTS (SELECT 1 FROM fence_ops f
                             WHERE f.op_kind = 'gate-job' AND f.op_id = gate_jobs.id)",
        params![lineage_id, g],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    for (id, doc) in jobs {
        let Ok(mut job) = serde_json::from_str::<remuda_protocol::GateJob>(&doc) else {
            continue;
        };
        let generation = job.initiator.as_ref().map_or(g, |i| i.generation);
        let host = job.host_id.as_ref().map(|h| h.as_id().as_str().to_owned());
        let result = if job.state == remuda_protocol::GateJobState::Queued {
            job.state = remuda_protocol::GateJobState::Canceled;
            job.finished_at = Some(crate::gatequeue::now_ts());
            job.reason = Some(FENCED_REASON.into());
            conn.execute(
                "UPDATE gate_jobs
                    SET doc_json = ?1, state = ?2, revision = revision + 1, updated_at = ?3
                  WHERE id = ?4 AND state = 'queued'",
                params![serde_json::to_string(&job)?, job.state.as_str(), now, id],
            )?;
            outcome::CANCELLED_AT_HUB
        } else if job.node_started_at.is_some() {
            outcome::RUNNING
        } else {
            outcome::UNKNOWN
        };
        record_op(
            conn,
            &fence_id,
            "gate-job",
            &id,
            host.as_deref(),
            &lineage_id,
            generation,
            Some("gate.run"),
            result,
            &now,
        )?;
    }

    // 7. Direct operations.
    let ops: Vec<(String, String, String, String, i64)> = rows(
        conn,
        "SELECT o.op_id, o.host_id, o.method, o.state,
                CAST(json_extract(o.initiator_json, '$.generation') AS INTEGER)
           FROM node_ops o
          WHERE json_extract(o.initiator_json, '$.lineageId') = ?1
            AND CAST(json_extract(o.initiator_json, '$.generation') AS INTEGER) <= ?2
            AND o.state IN ('admitted', 'sent')
            AND NOT EXISTS (SELECT 1 FROM fence_ops f
                             WHERE f.op_kind = 'node-op' AND f.op_id = o.op_id
                               AND f.host_id = o.host_id)",
        params![lineage_id, g],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    for (op_id, host, method, state, generation) in ops {
        let result = if state == "admitted" {
            conn.execute(
                "UPDATE node_ops
                    SET state = 'canceled', outcome_json = ?3, updated_at = ?4
                  WHERE op_id = ?1 AND host_id = ?2 AND state = 'admitted'",
                params![
                    op_id,
                    host,
                    json!({"canceled": FENCED_REASON}).to_string(),
                    now
                ],
            )?;
            outcome::CANCELLED_AT_HUB
        } else {
            outcome::UNKNOWN
        };
        record_op(
            conn,
            &fence_id,
            "node-op",
            &op_id,
            Some(&host),
            &lineage_id,
            generation,
            Some(&method),
            result,
            &now,
        )?;
    }

    // 8. The predecessor's pending interactions are superseded.
    let pending: Vec<(String, String)> = rows(
        conn,
        "SELECT id, payload_json FROM interactions
          WHERE instance_id = ?1 AND state = 'pending' ORDER BY id ASC",
        params![predecessor],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let mut settlement = Settlement::default();
    for (id, payload) in pending {
        let mut event: Value = serde_json::from_str(&payload).unwrap_or_else(|_| json!({}));
        supersede_interaction_payload(&mut event, &now, &superseded_by);
        let changed = conn.execute(
            "UPDATE interactions
                SET state = 'superseded', superseded_by = ?2, blocking = 0,
                    payload_json = ?3, updated_at = ?4
              WHERE id = ?1 AND state = 'pending'",
            params![id, superseded_by, event.to_string(), now],
        )?;
        if changed > 0 {
            let seq = insert_settlement_event(conn, &predecessor, &id, "superseded", &now)?;
            settlement.interactions.push(SettledInteraction {
                instance_id: predecessor.clone(),
                interaction_id: id,
                seq,
                updated_at: now.clone(),
                reason: "superseded".into(),
            });
        }
    }

    // 9. The durable fence and one pending delivery per known host.
    let (successor_id, close) = match &spec.mode {
        FenceMode::Pause { close, .. } => (None, close.clone()),
        FenceMode::Successor { successor_id } => (Some(successor_id.clone()), None),
    };
    conn.execute(
        "INSERT INTO fences
            (fence_id, lineage_id, fenced_generation, live_generation, reason, actor_json,
             predecessor_instance_id, successor_instance_id, close_command_id,
             successor_command_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10)",
        params![
            fence_id,
            lineage_id,
            g,
            live,
            reason,
            spec.actor.to_string(),
            predecessor,
            successor_id,
            close.as_ref().map(|(id, _)| id.clone()),
            now
        ],
    )?;
    let hosts: Vec<String> = rows(conn, "SELECT id FROM hosts ORDER BY id", [], |row| {
        row.get(0)
    })?;
    for host in &hosts {
        conn.execute(
            "INSERT OR IGNORE INTO fence_deliveries (fence_id, host_id, state)
             VALUES (?1, ?2, 'pending')",
            params![fence_id, host],
        )?;
    }

    // Pause: the Hub-internal close of the predecessor, queued inside F so a
    // crash cannot lose it. It carries no initiator, so neither the send
    // gate nor the Node fence refuses it; it is forwarded once the host has
    // acknowledged this fence.
    let close_command = match close {
        Some((command_id, payload)) => {
            conn.execute(
                "INSERT INTO commands
                    (id, instance_id, host_id, operation, state, resolution, forwarded,
                     payload_json, idempotency_key, created_at, updated_at, link_generation)
                 VALUES (?1, ?2, ?3, 'instance.close', 'queued', 'clear', 0, ?4, NULL, ?5, ?5,
                         (SELECT link_generation FROM hosts WHERE id = ?3))",
                params![
                    command_id,
                    predecessor,
                    predecessor_host,
                    payload.to_string(),
                    now
                ],
            )?;
            crate::store::load_command(conn, &command_id)?
        }
        None => None,
    };

    // 11. Audit, with the actor chain.
    let audit_device = spec
        .actor
        .get("deviceId")
        .and_then(Value::as_str)
        .unwrap_or("hub")
        .to_owned();
    conn.execute(
        "INSERT INTO audit_log (device_id, action, subject, detail_json, created_at)
         VALUES (?1, 'lineage.fence', ?2, ?3, ?4)",
        params![
            audit_device,
            lineage_id,
            json!({
                "fenceId": fence_id,
                "reason": reason,
                "fencedGeneration": g,
                "liveGeneration": live,
                "predecessor": predecessor,
                "successor": successor_id,
                "actor": spec.actor,
                "cancelledCommands": held.len(),
                "potentiallyExecutedCommands": in_flight.len(),
            })
            .to_string(),
            now
        ],
    )?;

    Ok(FenceTx::Committed(Box::new(FenceCore {
        fence_id,
        lineage_id,
        fenced_generation: g,
        live_generation: live,
        predecessor_id: predecessor,
        predecessor_host_id: predecessor_host,
        hosts,
        settlement,
        close_command,
    })))
}

/// Record the successor create command of a fence (step 10).
pub(crate) fn set_successor_command(
    conn: &Connection,
    fence_id: &str,
    command_id: &str,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE fences SET successor_command_id = ?2 WHERE fence_id = ?1",
        params![fence_id, command_id],
    )?;
    Ok(())
}

/// Resume of a paused lineage. The pause's F already moved the generation to
/// the live G and fenced the predecessor (credentials, cancelled and recorded
/// work, superseded interactions, the durable fence), so nothing new is
/// fenced: the CAS moves `paused` at G to `starting` on the successor, which
/// takes generation G — chapter generations stay consecutive. The pause's
/// fence carries the successor's create as its follow-on, so a host that
/// still owes that acknowledgement receives the create only after it.
fn resume_paused(
    conn: &Connection,
    spec: &FenceSpec,
    lineage: &crate::store::LineageRecord,
    successor_id: &str,
    now: &str,
) -> Result<FenceTx, StoreError> {
    let g = lineage.generation;
    let cas = conn.execute(
        "UPDATE lineages
            SET state = 'starting', current_instance_id = ?1,
                paused_by_json = NULL, paused_at = NULL, updated_at = ?2
          WHERE lineage_id = ?3 AND generation = ?4 AND state = 'paused'",
        params![successor_id, now, lineage.lineage_id, g],
    )?;
    if cas == 0 {
        return Ok(FenceTx::CasLost);
    }
    let predecessor = lineage.current_instance_id.clone();
    let predecessor_host: String = conn
        .query_row(
            "SELECT host_id FROM instances WHERE id = ?1",
            params![predecessor],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::Id("current chapter missing".into()))?;
    // Belt and braces for a lineage paused before fences existed.
    conn.execute(
        "UPDATE instances SET fenced_at = ?1, updated_at = ?1 WHERE id = ?2 AND fenced_at IS NULL",
        params![now, predecessor],
    )?;
    conn.execute(
        "DELETE FROM devices WHERE instance_id = ?1",
        params![predecessor],
    )?;
    let pause_fence: Option<String> = conn
        .query_row(
            "SELECT fence_id FROM fences WHERE lineage_id = ?1 AND live_generation = ?2",
            params![lineage.lineage_id, g],
            |row| row.get(0),
        )
        .optional()?;
    let audit_device = spec
        .actor
        .get("deviceId")
        .and_then(Value::as_str)
        .unwrap_or("hub")
        .to_owned();
    conn.execute(
        "INSERT INTO audit_log (device_id, action, subject, detail_json, created_at)
         VALUES (?1, 'lineage.resume', ?2, ?3, ?4)",
        params![
            audit_device,
            lineage.lineage_id,
            json!({
                "generation": g,
                "predecessor": predecessor,
                "successor": successor_id,
                "pauseFence": pause_fence,
                "actor": spec.actor,
            })
            .to_string(),
            now
        ],
    )?;
    Ok(FenceTx::Committed(Box::new(FenceCore {
        fence_id: pause_fence.unwrap_or_default(),
        lineage_id: lineage.lineage_id.clone(),
        fenced_generation: g - 1,
        live_generation: g,
        predecessor_id: predecessor,
        predecessor_host_id: predecessor_host,
        hosts: Vec::new(),
        settlement: Settlement::default(),
        close_command: None,
    })))
}

/// One pending delivery, in creation order.
#[derive(Clone, Debug)]
pub(crate) struct PendingFence {
    pub fence_id: String,
    pub lineage_id: String,
    pub live_generation: i64,
}

/// The read-model row of one fenced operation (`GET /v1/lineages/{id}`).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FenceOpView {
    pub fence_id: String,
    pub op_kind: String,
    pub op_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    pub generation: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    pub outcome: String,
}

/// One fence in the lineage projection.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FenceView {
    pub fence_id: String,
    pub fenced_generation: i64,
    pub live_generation: i64,
    pub reason: String,
    pub actor: Value,
    pub predecessor_instance_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub successor_instance_id: Option<String>,
    pub created_at: String,
    /// Hosts that have not acknowledged the fence yet.
    pub pending_hosts: Vec<String>,
    pub operations: Vec<FenceOpView>,
}

/// A command op's outcome, refined by the command's own later settlement:
/// a potentially-executed send that later settled `completed` ran; one the
/// Node settled `rejected(fenced)` after accepting it was cancelled by the
/// Node. Anything else keeps the recorded outcome.
fn command_outcome(
    recorded: &str,
    state: Option<&str>,
    settled: Option<&str>,
    reason: Option<&str>,
) -> String {
    if recorded != outcome::UNKNOWN || state != Some("settled") {
        return recorded.to_owned();
    }
    match (settled, reason) {
        (Some("completed"), _) => outcome::RAN.into(),
        (Some("withheld"), _) => outcome::WITHHELD.into(),
        (Some("rejected"), Some(FENCED_REASON)) => outcome::CANCELLED_BY_NODE.into(),
        _ => recorded.to_owned(),
    }
}

/// Raw `fences` columns, in SELECT order.
type FenceRow = (
    String,
    i64,
    i64,
    String,
    String,
    String,
    Option<String>,
    String,
);

/// Every fence of a lineage with its per-operation outcomes, oldest first.
pub(crate) fn lineage_fences(
    conn: &Connection,
    lineage_id: &str,
) -> Result<Vec<FenceView>, StoreError> {
    let fences: Vec<FenceRow> = rows(
        conn,
        "SELECT fence_id, fenced_generation, live_generation, reason, actor_json,
                predecessor_instance_id, successor_instance_id, created_at
           FROM fences WHERE lineage_id = ?1 ORDER BY rowid ASC",
        params![lineage_id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            ))
        },
    )?;
    let mut out = Vec::with_capacity(fences.len());
    for (fence_id, fenced, live, reason, actor, predecessor, successor, created_at) in fences {
        let pending_hosts = rows(
            conn,
            "SELECT d.host_id FROM fence_deliveries d
               JOIN hosts h ON h.id = d.host_id
              WHERE d.fence_id = ?1 AND d.state = 'pending'
              ORDER BY d.host_id",
            params![fence_id],
            |row| row.get(0),
        )?;
        let operations = rows(
            conn,
            "SELECT f.op_kind, f.op_id, f.host_id, f.generation, f.operation, f.outcome,
                    c.state, c.settlement_outcome, c.settlement_reason
               FROM fence_ops f
               LEFT JOIN commands c ON f.op_kind = 'command' AND c.id = f.op_id
              WHERE f.fence_id = ?1 ORDER BY f.op_kind, f.op_id",
            params![fence_id],
            |row| {
                let kind: String = row.get(0)?;
                let recorded: String = row.get(5)?;
                let state: Option<String> = row.get(6)?;
                let settled: Option<String> = row.get(7)?;
                let reason: Option<String> = row.get(8)?;
                let outcome = if kind == "command" {
                    command_outcome(
                        &recorded,
                        state.as_deref(),
                        settled.as_deref(),
                        reason.as_deref(),
                    )
                } else {
                    recorded
                };
                Ok(FenceOpView {
                    fence_id: fence_id.clone(),
                    op_kind: kind,
                    op_id: row.get(1)?,
                    host_id: row.get(2)?,
                    generation: row.get(3)?,
                    operation: row.get(4)?,
                    outcome,
                })
            },
        )?;
        out.push(FenceView {
            fence_id,
            fenced_generation: fenced,
            live_generation: live,
            reason,
            actor: serde_json::from_str(&actor).unwrap_or(Value::Null),
            predecessor_instance_id: predecessor,
            successor_instance_id: successor,
            created_at,
            pending_hosts,
            operations,
        });
    }
    Ok(out)
}

/// SQL: the host advertised `lineageFence` in its hello.
const HOST_CAPABLE_SQL: &str =
    "COALESCE(json_extract(h.capabilities_json, '$.lineageFence'), 0) = 1";

/// Whether `host_id` advertised fence support.
pub(crate) fn host_capable(conn: &Connection, host_id: &str) -> Result<bool, StoreError> {
    Ok(conn
        .query_row(
            &format!("SELECT {HOST_CAPABLE_SQL} FROM hosts h WHERE h.id = ?1"),
            params![host_id],
            |row| row.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false))
}

/// Whether a fence-capable host still has unacknowledged fences: until they
/// are acknowledged nothing is forwarded to it, direct operations get
/// host-offline and the gate tick skips its lanes (§7.6).
pub(crate) fn host_gated(conn: &Connection, host_id: &str) -> Result<bool, StoreError> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT 1 FROM hosts h
                  WHERE h.id = ?1 AND {HOST_CAPABLE_SQL}
                    AND EXISTS (SELECT 1 FROM fence_deliveries d
                                 WHERE d.host_id = h.id AND d.state = 'pending')"
            ),
            params![host_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Whether an initiator belongs to a continuity lineage (one with a
/// `lineages` row): only its work needs a fence-capable Node.
pub(crate) fn initiator_is_continuity(
    conn: &Connection,
    initiator: Option<&Initiator>,
) -> Result<bool, StoreError> {
    let Some(initiator) = initiator else {
        return Ok(false);
    };
    Ok(conn
        .query_row(
            "SELECT 1 FROM lineages WHERE lineage_id = ?1",
            params![initiator.lineage_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Admission-side refusal for a direct operation or gate dispatch to `host`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HostRefusal {
    /// The host has unacknowledged fences (reported as host-offline).
    Gated,
    /// A continuity lineage's work to a Node without fence support.
    LacksFence,
}

/// The §7.5 item 2 host gating and the §7.6 capability rule for one
/// initiator-carrying (or not) direct operation.
pub(crate) fn host_refusal(
    conn: &Connection,
    host_id: &str,
    initiator: Option<&Initiator>,
) -> Result<Option<HostRefusal>, StoreError> {
    if host_gated(conn, host_id)? {
        return Ok(Some(HostRefusal::Gated));
    }
    if initiator_is_continuity(conn, initiator)? && !host_capable(conn, host_id)? {
        return Ok(Some(HostRefusal::LacksFence));
    }
    Ok(None)
}

impl Store {
    /// F without successor (a pause): run [`fence_core`] in one writer
    /// transaction and commit.
    pub(crate) async fn fence_lineage(
        &self,
        spec: FenceSpec,
    ) -> Result<(FenceTx, Settlement), StoreError> {
        #[cfg(any(test, feature = "test-faults"))]
        let hook_lineage = spec.lineage_id.clone();
        #[cfg(any(test, feature = "test-faults"))]
        crate::store::run_continuation_hook("fence-read", &hook_lineage);
        self.run_named("fence_lineage", move |conn| {
            let tx = conn.transaction()?;
            let result = fence_core(&tx, &spec)?;
            let settlement = match &result {
                FenceTx::Committed(core) => core.settlement.clone(),
                _ => Settlement::default(),
            };
            if matches!(result, FenceTx::Committed(_)) {
                tx.commit()?;
            }
            Ok((result, settlement))
        })
        .await
    }

    /// Every lineage's live generation, for the boot fence map.
    pub(crate) async fn lineage_live_generations(&self) -> Result<Vec<(String, i64)>, StoreError> {
        self.read("lineage_live_generations", |conn| {
            rows(
                conn,
                "SELECT lineage_id, generation FROM lineages",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
        })
        .await
    }

    /// The host's unacknowledged fences, in creation order.
    pub(crate) async fn pending_fences(
        &self,
        host_id: String,
    ) -> Result<Vec<PendingFence>, StoreError> {
        self.run_named("pending_fences", move |conn| {
            rows(
                conn,
                "SELECT f.fence_id, f.lineage_id, f.live_generation
                   FROM fence_deliveries d JOIN fences f ON f.fence_id = d.fence_id
                  WHERE d.host_id = ?1 AND d.state = 'pending'
                  ORDER BY f.rowid ASC",
                params![host_id],
                |row| {
                    Ok(PendingFence {
                        fence_id: row.get(0)?,
                        lineage_id: row.get(1)?,
                        live_generation: row.get(2)?,
                    })
                },
            )
        })
        .await
    }

    /// Store one host's acknowledgement. Outcomes settle only `fence_ops`
    /// rows of THIS fence (a late ack about an old fence can never settle a
    /// newer fence's rows) and only rows still `unknown`; an op the ack does
    /// not mention stays as it was. Returns the Hub-internal follow-on
    /// commands of this fence that target `host_id` (successor create, pause
    /// close), still held.
    pub(crate) async fn ack_fence(
        &self,
        fence_id: String,
        host_id: String,
        result: LineageFenceResult,
    ) -> Result<Vec<String>, StoreError> {
        self.run_named("ack_fence", move |conn| {
            let tx = conn.transaction()?;
            let now = crate::config::now_rfc3339();
            tx.execute(
                "UPDATE fence_deliveries SET state = 'acked', acked_at = ?3, result_json = ?4
                  WHERE fence_id = ?1 AND host_id = ?2",
                params![fence_id, host_id, now, serde_json::to_string(&result)?],
            )?;
            for op in &result.outcomes {
                let to = match op.outcome {
                    remuda_protocol::FenceOutcome::Cancelled => outcome::CANCELLED_BY_NODE,
                    remuda_protocol::FenceOutcome::Ran => outcome::RAN,
                    remuda_protocol::FenceOutcome::Refused => outcome::REFUSED_BY_NODE,
                    // `unknown` keeps the row unknown: nothing to write.
                    remuda_protocol::FenceOutcome::Unknown => continue,
                };
                tx.execute(
                    "UPDATE fence_ops SET outcome = ?3, updated_at = ?4
                      WHERE fence_id = ?1 AND op_id = ?2 AND outcome = 'unknown'
                        AND (host_id IS NULL OR host_id = ?5)",
                    params![fence_id, op.op_id, to, now, host_id],
                )?;
            }
            let follow_ons = rows(
                &tx,
                "SELECT c.id FROM fences f JOIN commands c
                     ON c.id IN (f.close_command_id, f.successor_command_id)
                  WHERE f.fence_id = ?1 AND c.host_id = ?2
                    AND c.state = 'queued' AND c.forwarded = 0",
                params![fence_id, host_id],
                |row| row.get(0),
            )?;
            tx.commit()?;
            Ok(follow_ons)
        })
        .await
    }

    /// Hosts that are fence-capable and still have unacknowledged fences.
    pub(crate) async fn gated_hosts(&self) -> Result<Vec<String>, StoreError> {
        self.read("fence_gated_hosts", |conn| {
            rows(
                conn,
                &format!(
                    "SELECT DISTINCT d.host_id FROM fence_deliveries d JOIN hosts h ON h.id = d.host_id
                      WHERE d.state = 'pending' AND {HOST_CAPABLE_SQL}"
                ),
                [],
                |row| row.get(0),
            )
        })
        .await
    }

    /// [`host_refusal`] on the reader pool.
    pub(crate) async fn fence_host_refusal(
        &self,
        host_id: String,
        initiator: Option<Initiator>,
    ) -> Result<Option<HostRefusal>, StoreError> {
        self.read("fence_host_refusal", move |conn| {
            host_refusal(conn, &host_id, initiator.as_ref())
        })
        .await
    }

    /// Settle a direct op the send gate withheld.
    pub(crate) async fn withhold_node_op(
        &self,
        op_id: String,
        host_id: String,
    ) -> Result<(), StoreError> {
        self.run_named("withhold_node_op", move |conn| {
            let now = crate::config::now_rfc3339();
            conn.execute(
                "UPDATE node_ops SET state = 'withheld', outcome_json = ?3, updated_at = ?4
                  WHERE op_id = ?1 AND host_id = ?2 AND state = 'sent'",
                params![op_id, host_id, json!({"withheld": FENCED_REASON}).to_string(), now],
            )?;
            conn.execute(
                "UPDATE fence_ops SET outcome = ?3, updated_at = ?4
                  WHERE op_kind = 'node-op' AND op_id = ?1 AND host_id = ?2 AND outcome = 'unknown'",
                params![op_id, host_id, outcome::WITHHELD, now],
            )?;
            Ok(())
        })
        .await
    }

    /// Settle a forward-intended command the send gate withheld.
    pub(crate) async fn withhold_command(
        &self,
        command_id: String,
    ) -> Result<Option<CommandRecord>, StoreError> {
        self.run_named("withhold_command", move |conn| {
            let now = crate::config::now_rfc3339();
            conn.execute(
                "UPDATE commands
                    SET state = 'settled', resolution = 'clear',
                        settlement_outcome = 'withheld', settlement_reason = ?2, updated_at = ?3
                  WHERE id = ?1 AND state = 'queued'",
                params![command_id, FENCED_REASON, now],
            )?;
            resolve_op_outcome(conn, "command", &command_id, outcome::WITHHELD)?;
            crate::store::load_command(conn, &command_id)
        })
        .await
    }

    /// The Node refused a command at entry because its initiator is fenced.
    pub(crate) async fn note_command_refused_by_node(
        &self,
        command_id: String,
    ) -> Result<(), StoreError> {
        self.run_named("note_command_refused_by_node", move |conn| {
            resolve_op_outcome(conn, "command", &command_id, outcome::REFUSED_BY_NODE)?;
            Ok(())
        })
        .await
    }
}

/// After F commits, in the §7.2 order: the fence map, egress revocation, then
/// delivery to every online host (each host's held follow-on work — the
/// successor create, the pause close — is forwarded once it acknowledges).
///
/// `inline_host` is delivered before this returns (the successor's host, so
/// a resume answers after its create could be forwarded); every other online
/// host is delivered in the background.
pub(crate) async fn after_commit(state: &AppState, core: &FenceCore, inline_host: Option<&str>) {
    state.fences.raise(&core.lineage_id, core.live_generation);
    state
        .api_relay
        .revoke_instance_egress(state, &core.predecessor_id)
        .await;
    let live: HashSet<String> = state.nodes.host_ids().await.into_iter().collect();
    if let Some(host) = inline_host
        && live.contains(host)
    {
        deliver_host(state, host).await;
    }
    for host in &core.hosts {
        if live.contains(host) && Some(host.as_str()) != inline_host {
            let state = state.clone();
            let host = host.clone();
            tokio::spawn(async move { deliver_host(&state, &host).await });
        }
    }
}

/// Close becomes pause (§6.3, §6.5): a close admitted for any chapter of an
/// unpaused continuity lineage runs F without successor at admission —
/// `paused{device}` for a Human or Bot device, `paused{self}` for the chapter
/// itself, `paused{ancestor}` for another Agent — and the close of the
/// current chapter is then forwarded once its host acknowledged the fence.
///
/// Returns `None` when the lineage is already paused (the caller forwards
/// the close as before), otherwise the queued close of the current chapter.
pub(crate) async fn pause_on_close(
    state: &AppState,
    device: &crate::store::Device,
    lineage_id: &str,
    command_id: Option<String>,
    payload: Value,
) -> Result<Option<CommandRecord>, crate::error::HubError> {
    use crate::error::HubError;
    let (initiator, device_id) = crate::agent_scope::initiator_and_device(state, device).await?;
    let command_id = match command_id {
        Some(id) => id,
        None => crate::config::new_id("cmd").map_err(|e| HubError::Internal(e.to_string()))?,
    };
    // A CAS lost to another F (a racing restart or close) re-reads and
    // applies to the then-current chapter: a Human close of any chapter of
    // a lineage applies to its current chapter (§7.8).
    for _ in 0..4 {
        let lineage = state
            .store
            .get_lineage(lineage_id.to_owned())
            .await?
            .ok_or(HubError::NotFound)?;
        if lineage.state == "paused" {
            return Ok(None);
        }
        let current = lineage.current_instance_id.clone();
        let (paused_by, self_close) = match &initiator {
            Some(init) if init.instance_id == current => (
                json!({"kind": "self", "instanceId": init.instance_id}),
                true,
            ),
            Some(init) => (
                json!({"kind": "ancestor", "instanceId": init.instance_id, "deviceId": device.id}),
                false,
            ),
            None => (json!({"kind": "device", "deviceId": device.id}), false),
        };
        if self_close && state.agent_approvals.in_flight_for(&current).await {
            return Err(HubError::lineage_conflict(
                "approval-in-flight",
                "this chapter has a D-017 approval in flight; it cannot close itself until the                  approval is answered or expires",
            ));
        }
        let mut close_payload = payload.clone();
        if let Some(object) = close_payload.as_object_mut() {
            object.insert("instanceId".into(), json!(current));
        }
        let spec = FenceSpec {
            lineage_id: lineage.lineage_id.clone(),
            expected_generation: lineage.generation,
            from_states: &["starting", "running"],
            mode: FenceMode::Pause {
                paused_by: paused_by.clone(),
                close: Some((command_id.clone(), close_payload)),
            },
            actor: json!({
                "deviceId": device.id,
                "instanceId": initiator.as_ref().map(|i| i.instance_id.clone()),
                "by": paused_by.get("kind").cloned().unwrap_or(Value::Null),
            }),
            authority: initiator.clone().map(|init| (init, device_id.clone())),
            self_close_guard: self_close,
        };
        let result = state
            .publish_settlement(async {
                let (result, settlement) = state.store.fence_lineage(spec).await?;
                Ok((result, settlement))
            })
            .await
            .map_err(crate::http::map_store)?;
        match result {
            FenceTx::Committed(core) => {
                after_commit(state, &core, Some(&core.predecessor_host_id)).await;
                let command = state
                    .store
                    .get_command(command_id.clone())
                    .await?
                    .or(core.close_command.clone())
                    .ok_or(HubError::NotFound)?;
                return Ok(Some(command));
            }
            FenceTx::CasLost => continue,
            FenceTx::StateRefused => return Ok(None),
            FenceTx::Guarded(reason) => {
                return Err(HubError::lineage_conflict(
                    reason,
                    "this chapter has a pending interaction; it cannot close itself until the                      interaction is answered",
                ));
            }
        }
    }
    Err(HubError::Conflict(
        "the lineage kept changing while it was being paused; retry".into(),
    ))
}

/// Bound for one `lineage.fence` round trip.
const FENCE_RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Deliver every unacknowledged fence of `host_id`, in creation order, then
/// forward the Hub-internal follow-on work and the commands its fences held.
/// Serialized per host. A failed acknowledgement leaves the delivery pending
/// (forwarding stays held) for the next hello or scheduler sweep.
pub(crate) async fn deliver_host(state: &AppState, host_id: &str) {
    let lock = state.fences.host_lock(host_id);
    let _guard = lock.lock().await;
    let capable = state
        .store
        .read("fence_host_capable", {
            let host = host_id.to_owned();
            move |conn| host_capable(conn, &host)
        })
        .await
        .unwrap_or(false);
    if !capable {
        return;
    }
    let pending = match state.store.pending_fences(host_id.to_owned()).await {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(%error, host_id, "could not read pending fences");
            return;
        }
    };
    let mut follow_ons = Vec::new();
    for fence in pending {
        let params = LineageFenceParams {
            fence_id: fence.fence_id.clone(),
            lineage_id: fence.lineage_id.clone(),
            live_generation: u64::try_from(fence.live_generation).unwrap_or(0),
        };
        let Ok(params) = serde_json::to_value(&params) else {
            return;
        };
        let reply = state
            .nodes
            .call(host_id, METHOD_LINEAGE_FENCE, params, FENCE_RPC_TIMEOUT)
            .await;
        let result = match reply {
            Ok(Some(frame)) if frame.get("error").is_none() => {
                let body = frame.get("result").cloned().unwrap_or(frame);
                serde_json::from_value::<LineageFenceResult>(body).unwrap_or_default()
            }
            other => {
                tracing::warn!(
                    host_id,
                    fence_id = %fence.fence_id,
                    reply = ?other.as_ref().map(|frame| frame.as_ref().map(ToString::to_string)),
                    "lineage.fence not acknowledged; delivery stays pending"
                );
                return;
            }
        };
        match state
            .store
            .ack_fence(fence.fence_id.clone(), host_id.to_owned(), result)
            .await
        {
            Ok(ids) => follow_ons.extend(ids),
            Err(error) => {
                tracing::warn!(%error, host_id, "could not record a fence acknowledgement");
                return;
            }
        }
    }
    follow_ons.extend(state.fences.take_held(host_id));
    let live = state.nodes.kind_of(host_id).await.is_some();
    for command_id in follow_ons {
        let Ok(Some(command)) = state.store.get_command(command_id).await else {
            continue;
        };
        if command.state != "queued" || command.forwarded {
            continue;
        }
        if let Err(error) = crate::http::forward_if_online(state, command, live).await {
            tracing::warn!(%error, host_id, "forwarding held work after a fence ack failed");
        }
    }
}

/// Scheduler tick: retry delivery for every live, gated host. Non-blocking:
/// a host already being delivered to is skipped.
pub(crate) async fn sweep_deliveries(state: &AppState) {
    let Ok(hosts) = state.store.gated_hosts().await else {
        return;
    };
    if hosts.is_empty() {
        // The common case touches neither the link registry nor any lock.
        return;
    }
    let live: HashSet<String> = state.nodes.host_ids().await.into_iter().collect();
    for host in hosts {
        if !live.contains(&host) {
            continue;
        }
        if state.fences.host_lock(&host).try_lock().is_err() {
            continue;
        }
        let state = state.clone();
        tokio::spawn(async move { deliver_host(&state, &host).await });
    }
}

/// Boot: load every lineage's live generation into the fence map (§7.7).
pub(crate) async fn load_map(state: &AppState) {
    match state.store.lineage_live_generations().await {
        Ok(rows) => {
            for (lineage, live) in rows {
                state.fences.raise(&lineage, live);
            }
        }
        Err(error) => tracing::error!(%error, "could not load the fence map"),
    }
}

/// The capability key, re-exported for the hello handler's log line.
#[allow(dead_code)]
pub(crate) const CAPABILITY: &str = LINEAGE_FENCE_CAPABILITY;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_outcome_refines_only_unknown_from_its_own_settlement() {
        assert_eq!(
            command_outcome(outcome::UNKNOWN, Some("settled"), Some("completed"), None),
            outcome::RAN
        );
        assert_eq!(
            command_outcome(
                outcome::UNKNOWN,
                Some("settled"),
                Some("rejected"),
                Some("fenced")
            ),
            outcome::CANCELLED_BY_NODE
        );
        // A rejection for another reason does not establish non-execution.
        assert_eq!(
            command_outcome(
                outcome::UNKNOWN,
                Some("settled"),
                Some("rejected"),
                Some("boom")
            ),
            outcome::UNKNOWN
        );
        assert_eq!(
            command_outcome(outcome::UNKNOWN, Some("accepted"), None, None),
            outcome::UNKNOWN
        );
        // Definite outcomes never change.
        assert_eq!(
            command_outcome(
                outcome::CANCELLED_AT_HUB,
                Some("settled"),
                Some("completed"),
                None
            ),
            outcome::CANCELLED_AT_HUB
        );
    }

    #[test]
    fn fence_map_is_monotonic_and_fences_lower_generations() {
        let map = FenceState::default();
        let initiator = |generation| Initiator {
            instance_id: "ins_a".into(),
            lineage_id: "ins_l".into(),
            generation,
        };
        assert!(!map.is_fenced(&initiator(1)));
        map.raise("ins_l", 2);
        assert!(map.is_fenced(&initiator(1)));
        assert!(!map.is_fenced(&initiator(2)));
        map.raise("ins_l", 1);
        assert!(
            map.is_fenced(&initiator(1)),
            "the map never moves backwards"
        );
    }
}
