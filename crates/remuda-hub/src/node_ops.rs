//! D-057 §7.5: the shared Hub admission boundary for Agent-reachable direct
//! Hub→Node RPCs that bypass the command table.
//!
//! `interaction.answer`, `worker.provision`, `worker.remove`,
//! `worktree.lease` and `worktree.return` run in `transport/hubnode_codec.rs`
//! on the Node, outside `insert_command`. Before any external effect this
//! module:
//! 1. runs `check_initiator` (main-agent.md §7.3) and persists a `node_ops`
//!    row with op id (`gjb_…` for gate jobs, the answer's commandId for
//!    `interaction.answer`), method, host, subject, initiator and the
//!    Hub-only device id, state `admitted` — in ONE writer job;
//! 2. sends with `initiator` and `opId` in the params;
//! 3. records the sent and settled outcome.
//!
//! The answer path no longer calls the Node before the admission row exists.
//! The Node-side dispatcher entry refusal and the send gate land with
//! ma-fence; until then an admitted op is sent as before.

use crate::error::HubError;
use crate::http::map_store;
use crate::store::{Store, StoreError};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

/// Admission lifecycle of one direct RPC. `Canceled` is set by ma-fence's
/// host-gating/settlement; until then rows pass admitted → sent → settled.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NodeOpState {
    Admitted,
    Sent,
    Settled,
    Canceled,
}

impl NodeOpState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Sent => "sent",
            Self::Settled => "settled",
            Self::Canceled => "canceled",
        }
    }
}

/// Authority carried by one direct RPC.
pub(crate) struct NodeOpAuth {
    pub initiator: Option<remuda_protocol::Initiator>,
    pub device_id: Option<String>,
}

impl NodeOpAuth {
    /// Resolve from an authenticated caller device.
    pub(crate) async fn for_device(
        state: &crate::AppState,
        device: &crate::store::Device,
    ) -> Result<Self, HubError> {
        let (initiator, device_id) =
            crate::agent_scope::initiator_and_device(state, device).await?;
        Ok(Self {
            initiator,
            device_id,
        })
    }

    /// Hub-internal cleanup (e.g. the dispatch rollback's worker.remove):
    /// never fenced, no initiator.
    #[allow(dead_code)]
    pub(crate) fn internal() -> Self {
        Self {
            initiator: None,
            device_id: None,
        }
    }

    #[allow(dead_code)]
    fn as_check(&self) -> (Option<&remuda_protocol::Initiator>, Option<&str>) {
        (self.initiator.as_ref(), self.device_id.as_deref())
    }

    /// Insert `initiator`/`opId` into outbound params. A `None` initiator
    /// (Human/Bot/internal) leaves params exactly as before.
    pub(crate) fn stamp_params(&self, mut params: Value, op_id: &str) -> Value {
        if let Some(object) = params.as_object_mut()
            && let Some(initiator) = &self.initiator
        {
            if let Ok(value) = serde_json::to_value(initiator) {
                object.insert("initiator".into(), value);
            }
            object.insert("opId".into(), Value::String(op_id.to_owned()));
        }
        params
    }
}

pub(crate) fn migrate(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS node_ops (
            op_id TEXT NOT NULL,
            host_id TEXT NOT NULL,
            method TEXT NOT NULL,
            subject TEXT,
            initiator_json TEXT,
            initiator_device_id TEXT,
            state TEXT NOT NULL,
            outcome_json TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (op_id, host_id)
         );
        CREATE INDEX IF NOT EXISTS node_ops_state ON node_ops(state);",
    )?;
    Ok(())
}

impl Store {
    /// Run the §7.3 check and persist an `admitted` row in one writer job.
    /// `op_id` is unique per (op, host): the answer commandId, a synthetic
    /// id for provision/lease ops. On `Fenced` nothing is written and the
    /// caller must refuse without touching the Node.
    pub(crate) async fn admit_node_op(
        &self,
        op_id: String,
        host_id: String,
        method: &'static str,
        subject: Option<String>,
        auth: &NodeOpAuth,
    ) -> Result<(), StoreError> {
        let initiator_json = match &auth.initiator {
            Some(initiator) => Some(serde_json::to_string(initiator)?),
            None => None,
        };
        let device_id = auth.device_id.clone();
        let subject = subject.clone();
        let check_initiator = auth.initiator.clone();
        // Test-only seam: fence lands inside this writer job.
        #[cfg(any(test, feature = "test-faults"))]
        let armed_fence = self.take_test_authority_fence();
        self.run_named("admit_node_op", move |conn| {
            #[cfg(any(test, feature = "test-faults"))]
            if let Some(fenced_instance) = armed_fence {
                crate::store::test_apply_fence(conn, &fenced_instance)?;
            }
            crate::store::check_initiator(conn, check_initiator.as_ref(), device_id.as_deref())?;
            let now = crate::config::now_rfc3339();
            conn.execute(
                "INSERT INTO node_ops
                    (op_id, host_id, method, subject, initiator_json, initiator_device_id,
                     state, outcome_json, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'admitted', NULL, ?7, ?7)
                 ON CONFLICT(op_id, host_id) DO NOTHING",
                params![
                    op_id,
                    host_id,
                    method,
                    subject,
                    initiator_json,
                    device_id,
                    now
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// Mark an admitted op `sent` (send intent) — committed BEFORE the frame
    /// is written to the Node link (main-agent.md §7.4/§7.5). Up to the RPC
    /// timeout the row therefore reads `sent` *while the frame is on the
    /// wire*, never `admitted`, so ma-fence step 7 cannot record an op that
    /// actually ran as "admitted but not sent → cancelled".
    pub(crate) async fn mark_node_op_sent(
        &self,
        op_id: String,
        host_id: String,
    ) -> Result<(), StoreError> {
        self.node_op_set_state(op_id, host_id, NodeOpState::Sent, None)
            .await
    }

    /// A `nodes.call` that produced no frame (`Ok(None)`: the host is not
    /// connected): record truthfully that the op was admitted but NOT sent.
    /// The row returns to `admitted` with a `notSent` outcome, so it stays
    /// in F's "admitted but not sent → cancelled" class rather than the
    /// "possibly executed" class. A call that errored AFTER the send intent
    /// is deliberately left `sent`: the frame may have reached the wire and
    /// its execution is unknown (§7.4 never infers "did not run").
    pub(crate) async fn mark_node_op_not_sent(
        &self,
        op_id: String,
        host_id: String,
        reason: &str,
    ) -> Result<(), StoreError> {
        let outcome_json =
            serde_json::to_string(&serde_json::json!({ "notSent": true, "reason": reason }))?;
        self.run_named("mark_node_op_not_sent", move |conn| {
            let now = crate::config::now_rfc3339();
            conn.execute(
                "UPDATE node_ops
                    SET state = 'admitted', outcome_json = ?3, updated_at = ?4
                  WHERE op_id = ?1 AND host_id = ?2",
                params![op_id, host_id, outcome_json, now],
            )?;
            Ok(())
        })
        .await
    }

    /// Mark an op `settled` with the Node's result (or failure summary).
    pub(crate) async fn settle_node_op(
        &self,
        op_id: String,
        host_id: String,
        outcome: Option<Value>,
    ) -> Result<(), StoreError> {
        self.node_op_set_state(op_id, host_id, NodeOpState::Settled, outcome)
            .await
    }

    async fn node_op_set_state(
        &self,
        op_id: String,
        host_id: String,
        state: NodeOpState,
        outcome: Option<Value>,
    ) -> Result<(), StoreError> {
        let outcome_json = match outcome {
            Some(value) => Some(serde_json::to_string(&value)?),
            None => None,
        };
        self.run_named("node_op_set_state", move |conn| {
            let now = crate::config::now_rfc3339();
            conn.execute(
                "UPDATE node_ops
                    SET state = ?3, outcome_json = COALESCE(?4, outcome_json), updated_at = ?5
                  WHERE op_id = ?1 AND host_id = ?2",
                params![op_id, host_id, state.as_str(), outcome_json, now],
            )?;
            Ok(())
        })
        .await
    }
    /// Test-only: read one node_ops row's state and serialized outcome.
    #[doc(hidden)]
    pub async fn test_get_node_op(
        &self,
        op_id: String,
        host_id: String,
    ) -> Result<Option<(String, Option<String>)>, StoreError> {
        self.run_named("test_get_node_op", move |conn| {
            let row = conn
                .query_row(
                    "SELECT state, outcome_json FROM node_ops
                      WHERE op_id = ?1 AND host_id = ?2",
                    params![op_id, host_id],
                    |row| {
                        let state: String = row.get(0)?;
                        let outcome: Option<String> = row.get(1)?;
                        Ok((state, outcome))
                    },
                )
                .optional()?;
            Ok(row)
        })
        .await
    }
}

/// Admit, send and settle a direct Hub→Node RPC behind the §7.5 boundary.
///
/// Human/Bot/internal calls (`auth.initiator == None`) skip the admission
/// row entirely and behave byte-for-byte as before. An Agent call whose
/// initiator fails the check gets `StoreError::Fenced` and no frame is sent.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn call_admitted(
    state: &crate::AppState,
    host_id: &str,
    method: &'static str,
    subject: Option<String>,
    op_id: String,
    params: Value,
    timeout: std::time::Duration,
    auth: &NodeOpAuth,
) -> Result<Value, HubError> {
    let reply_frame = call_admitted_frame(
        state, host_id, method, subject, op_id, params, timeout, auth,
    )
    .await?;
    match reply_frame {
        Some(response) => {
            if let Some(error) = response.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("node worktree rpc failed");
                return Err(HubError::BadRequest(message.to_string()));
            }
            Ok(response.get("result").cloned().unwrap_or(response))
        }
        None => Err(HubError::Unsatisfiable {
            reasons: vec![format!("host {host_id} is not connected")],
        }),
    }
}

/// Frame-returning variant for callers (`interaction.answer`) that handle
/// the JSON-RPC frame and first-answer-wins NotFound themselves.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn call_admitted_frame(
    state: &crate::AppState,
    host_id: &str,
    method: &'static str,
    subject: Option<String>,
    op_id: String,
    params: Value,
    timeout: std::time::Duration,
    auth: &NodeOpAuth,
) -> Result<Option<Value>, HubError> {
    if auth.initiator.is_some() {
        state
            .store
            .admit_node_op(op_id.clone(), host_id.to_owned(), method, subject, auth)
            .await
            .map_err(map_store)?;
        // Send intent commits BEFORE the frame is written: while the RPC is
        // outstanding (up to the timeout) the row already reads `sent`, so a
        // fence landing in that window classifies the op truthfully.
        state
            .store
            .mark_node_op_sent(op_id.clone(), host_id.to_owned())
            .await
            .map_err(map_store)?;
    }
    let params = auth.stamp_params(params, &op_id);
    let reply = state.nodes.call(host_id, method, params, timeout).await;
    if auth.initiator.is_some() {
        match &reply {
            // The Node took the frame: settle with its result.
            Ok(Some(frame)) => {
                let outcome = frame.get("result").cloned().unwrap_or(Value::Null);
                state
                    .store
                    .settle_node_op(op_id, host_id.to_owned(), Some(outcome))
                    .await
                    .map_err(map_store)?;
            }
            // Nothing was written to any link: not sent, not unknown.
            Ok(None) => {
                state
                    .store
                    .mark_node_op_not_sent(
                        op_id,
                        host_id.to_owned(),
                        "host not connected; no frame written",
                    )
                    .await
                    .map_err(map_store)?;
            }
            // The send intent was already committed and the error cannot
            // establish non-delivery: leave the row `sent` (unknown).
            Err(_) => {}
        }
    }
    reply
}
