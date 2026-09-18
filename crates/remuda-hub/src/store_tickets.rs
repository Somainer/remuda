//! Persistent card ↔ interaction tickets and the Bot owner-answer allowlist.
//!
//! Design §5.1 blockers 1 and 3 live here:
//!
//! * `card_tickets` mirrors the dispatcher's in-memory ticket map in the same
//!   SQLite store as `interactions`, so a dispatcher restart keeps every open
//!   approval card answerable. Rows are short-lived (10–15 min runtime
//!   deadline); the dispatcher owns their lifecycle, the Hub only stores and
//!   looks them up.
//! * `bot_owner_allowlist` records which Feishu `open_id`s a given Bot device
//!   is allowed to relay answers for. The bot never bypasses approvals
//!   (D-005/D-011): it is only the owner's courier, and every relayed answer
//!   is audited with the acting `open_id`.
//!
//! No role or delegation is denormalized onto these rows — tiers are looked up
//! through the Hub, never stored on a ticket (design amendment 2026-09-15).

use crate::store::{Store, StoreError};
use rusqlite::OptionalExtension;
use rusqlite::params;
use serde::Serialize;

/// A dispatcher-issued card ticket, as persisted for restart hydration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CardTicketRecord {
    /// Short id embedded in the card's `behaviors.callback.value.tid`.
    pub ticket_id: String,
    /// Bot device that minted the ticket — only that bot may relay answers.
    pub device_id: String,
    /// Protocol Interaction id this card answers.
    pub interaction_id: String,
    /// Instance whose journal produced the Interaction.
    pub instance_id: String,
    /// `feishu:{chat_id}:{thread||root||main}` the card was posted to.
    pub session_key: String,
    /// Posted card message id once the outbound receipt is known.
    pub card_message_id: Option<String>,
    /// `open` | `answered` | `expired`.
    pub state: String,
    /// CAS field copied from the Interaction.
    pub request_version: i64,
    /// CAS field copied from the Interaction.
    pub process_generation: i64,
    /// Serialized `InteractionRequest` (needed to re-encode answers).
    pub request_json: String,
    /// Issue time, unix epoch milliseconds.
    pub created_at_ms: i64,
    /// Runtime deadline, unix epoch milliseconds.
    pub expires_at_ms: i64,
}

/// One additive migration block (design §8.1: batch 2 keeps its `store.rs`
/// touch to a single block).
pub(crate) fn migrate(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS card_tickets (
            ticket_id TEXT PRIMARY KEY,
            device_id TEXT NOT NULL,
            interaction_id TEXT NOT NULL,
            instance_id TEXT NOT NULL,
            session_key TEXT NOT NULL,
            card_message_id TEXT,
            state TEXT NOT NULL DEFAULT 'open',
            request_version INTEGER NOT NULL DEFAULT 0,
            process_generation INTEGER NOT NULL DEFAULT 0,
            request_json TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL,
            expires_at_ms INTEGER NOT NULL
        );
        CREATE UNIQUE INDEX IF NOT EXISTS card_tickets_open_interaction
            ON card_tickets(interaction_id) WHERE state = 'open';
        CREATE INDEX IF NOT EXISTS card_tickets_device ON card_tickets(device_id);
        CREATE INDEX IF NOT EXISTS card_tickets_session ON card_tickets(session_key);
        CREATE INDEX IF NOT EXISTS card_tickets_state_expires
            ON card_tickets(state, expires_at_ms);
        CREATE TABLE IF NOT EXISTS bot_owner_allowlist (
            device_id TEXT NOT NULL,
            open_id TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (device_id, open_id)
        );",
    )?;
    Ok(())
}

fn ticket_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CardTicketRecord> {
    Ok(CardTicketRecord {
        ticket_id: row.get(0)?,
        device_id: row.get(1)?,
        interaction_id: row.get(2)?,
        instance_id: row.get(3)?,
        session_key: row.get(4)?,
        card_message_id: row.get(5)?,
        state: row.get(6)?,
        request_version: row.get(7)?,
        process_generation: row.get(8)?,
        request_json: row.get(9)?,
        created_at_ms: row.get(10)?,
        expires_at_ms: row.get(11)?,
    })
}

const TICKET_COLUMNS: &str = "ticket_id, device_id, interaction_id, instance_id, session_key,
    card_message_id, state, request_version, process_generation, request_json,
    created_at_ms, expires_at_ms";

impl Store {
    /// Upsert a ticket. Re-issuing for an interaction must first mark the
    /// previous row non-open (the partial unique index enforces this).
    pub async fn upsert_card_ticket(&self, record: CardTicketRecord) -> Result<(), StoreError> {
        self.run_named("upsert_card_ticket", move |conn| {
            conn.execute(
                "INSERT INTO card_tickets (
                    ticket_id, device_id, interaction_id, instance_id, session_key,
                    card_message_id, state, request_version, process_generation, request_json,
                    created_at_ms, expires_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(ticket_id) DO UPDATE SET
                    device_id = excluded.device_id,
                    interaction_id = excluded.interaction_id,
                    instance_id = excluded.instance_id,
                    session_key = excluded.session_key,
                    card_message_id = excluded.card_message_id,
                    state = excluded.state,
                    request_version = excluded.request_version,
                    process_generation = excluded.process_generation,
                    request_json = excluded.request_json,
                    created_at_ms = excluded.created_at_ms,
                    expires_at_ms = excluded.expires_at_ms",
                params![
                    record.ticket_id,
                    record.device_id,
                    record.interaction_id,
                    record.instance_id,
                    record.session_key,
                    record.card_message_id,
                    record.state,
                    record.request_version,
                    record.process_generation,
                    record.request_json,
                    record.created_at_ms,
                    record.expires_at_ms,
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// One ticket by its short callback id, scoped to its owning Bot device.
    pub async fn get_card_ticket_for_bot(
        &self,
        ticket_id: String,
        device_id: String,
    ) -> Result<Option<CardTicketRecord>, StoreError> {
        self.run_named("get_card_ticket_for_bot", move |conn| {
            conn.query_row(
                &format!("SELECT {TICKET_COLUMNS} FROM card_tickets WHERE ticket_id = ?1 AND device_id = ?2"),
                params![ticket_id, device_id],
                ticket_from_row,
            )
            .optional()
            .map_err(StoreError::from)
        })
        .await
    }

    /// The open ticket for an interaction, scoped to one Bot device. This is
    /// the authorization binding checked when a bot relays an owner answer.
    pub async fn get_open_card_ticket_for_bot(
        &self,
        interaction_id: String,
        device_id: String,
    ) -> Result<Option<CardTicketRecord>, StoreError> {
        self.run_named("get_open_card_ticket_for_bot", move |conn| {
            conn.query_row(
                &format!(
                    "SELECT {TICKET_COLUMNS} FROM card_tickets
                     WHERE interaction_id = ?1 AND device_id = ?2 AND state = 'open'"
                ),
                params![interaction_id, device_id],
                ticket_from_row,
            )
            .optional()
            .map_err(StoreError::from)
        })
        .await
    }

    /// Open tickets one dispatcher must hydrate after a restart. Due rows are
    /// excluded; the caller expires them through the normal sweep.
    pub async fn list_open_card_tickets(
        &self,
        device_id: String,
        now_ms: i64,
    ) -> Result<Vec<CardTicketRecord>, StoreError> {
        self.run_named("list_open_card_tickets", move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {TICKET_COLUMNS} FROM card_tickets
                 WHERE device_id = ?1 AND state = 'open' AND expires_at_ms > ?2
                 ORDER BY created_at_ms"
            ))?;
            let rows = stmt.query_map(params![device_id, now_ms], ticket_from_row)?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
        .await
    }

    /// Transition a ticket's state (and optionally attach its card message id).
    /// Scoped to the owning Bot device.
    pub async fn set_card_ticket_state(
        &self,
        ticket_id: String,
        device_id: String,
        state: String,
        card_message_id: Option<String>,
    ) -> Result<(), StoreError> {
        self.run_named("set_card_ticket_state", move |conn| {
            match card_message_id {
                Some(message_id) => {
                    conn.execute(
                        "UPDATE card_tickets SET state = ?1, card_message_id = ?2
                         WHERE ticket_id = ?3 AND device_id = ?4",
                        params![state, message_id, ticket_id, device_id],
                    )?;
                }
                None => {
                    conn.execute(
                        "UPDATE card_tickets SET state = ?1 WHERE ticket_id = ?2 AND device_id = ?3",
                        params![state, ticket_id, device_id],
                    )?;
                }
            }
            Ok(())
        })
        .await
    }

    /// Expire every open ticket for a session (`/new`, `/stop`); returns the
    /// ticket ids that changed.
    pub async fn expire_card_tickets_for_session(
        &self,
        device_id: String,
        session_key: String,
    ) -> Result<Vec<String>, StoreError> {
        self.run_named("expire_card_tickets_for_session", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT ticket_id FROM card_tickets
                 WHERE device_id = ?1 AND session_key = ?2 AND state = 'open'",
            )?;
            let rows = stmt.query_map(params![device_id, session_key], |row| {
                row.get::<_, String>(0)
            })?;
            let ids = rows.collect::<Result<Vec<_>, _>>()?;
            drop(stmt);
            conn.execute(
                "UPDATE card_tickets SET state = 'expired'
                 WHERE device_id = ?1 AND session_key = ?2 AND state = 'open'",
                params![device_id, session_key],
            )?;
            Ok(ids)
        })
        .await
    }

    /// Replace a Bot device's owner allowlist with `open_ids`.
    pub async fn set_bot_owner_allowlist(
        &self,
        device_id: String,
        open_ids: &[String],
    ) -> Result<(), StoreError> {
        let ids = open_ids.to_vec();
        self.run_named("set_bot_owner_allowlist", move |conn| {
            let now = crate::config::now_rfc3339();
            conn.execute(
                "DELETE FROM bot_owner_allowlist WHERE device_id = ?1",
                params![device_id],
            )?;
            for open_id in &ids {
                conn.execute(
                    "INSERT INTO bot_owner_allowlist (device_id, open_id, updated_at)
                     VALUES (?1, ?2, ?3)",
                    params![device_id, open_id, now],
                )?;
            }
            Ok(())
        })
        .await
    }

    /// Whether `open_id` may relay owner answers through this Bot device.
    pub async fn bot_owner_allowlist_contains(
        &self,
        device_id: String,
        open_id: String,
    ) -> Result<bool, StoreError> {
        self.run_named("bot_owner_allowlist_contains", move |conn| {
            let found = conn
                .query_row(
                    "SELECT 1 FROM bot_owner_allowlist WHERE device_id = ?1 AND open_id = ?2",
                    params![device_id, open_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            Ok(found)
        })
        .await
    }
}
