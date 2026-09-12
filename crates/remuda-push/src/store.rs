//! SQLite subscription store: endpoint / p256dh / auth per device.

use crate::Error;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::Mutex;
use time::OffsetDateTime;
use uuid::Uuid;

/// One browser PushSubscription bound to a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    /// Durable identity (`push_` + UUID).
    pub id: String,
    /// Device that registered this endpoint.
    pub device_id: String,
    /// Push service URL.
    pub endpoint: String,
    /// Uncompressed P-256 public key, URL-safe base64.
    pub p256dh: String,
    /// 16-byte auth secret, URL-safe base64.
    pub auth: String,
}

pub(crate) struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub(crate) fn open(data_dir: &Path) -> Result<Self, Error> {
        let conn = Connection::open(data_dir.join("push.sqlite"))?;
        conn.execute_batch(
            "
            PRAGMA journal_mode=WAL;
            PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS subscriptions (
                id TEXT PRIMARY KEY,
                device_id TEXT NOT NULL,
                endpoint TEXT NOT NULL UNIQUE,
                p256dh TEXT NOT NULL,
                auth TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS subscriptions_device ON subscriptions(device_id);
            ",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub(crate) fn upsert(&self, sub: &Subscription) -> Result<(), Error> {
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into());
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::Invalid("store lock".into()))?;
        conn.execute(
            "INSERT INTO subscriptions (id, device_id, endpoint, p256dh, auth, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(endpoint) DO UPDATE SET
                id=excluded.id,
                device_id=excluded.device_id,
                p256dh=excluded.p256dh,
                auth=excluded.auth",
            params![
                sub.id,
                sub.device_id,
                sub.endpoint,
                sub.p256dh,
                sub.auth,
                now
            ],
        )?;
        Ok(())
    }

    pub(crate) fn get(&self, endpoint: &str) -> Result<Option<Subscription>, Error> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::Invalid("store lock".into()))?;
        conn.query_row(
            "SELECT id, device_id, endpoint, p256dh, auth FROM subscriptions WHERE endpoint = ?1",
            params![endpoint],
            row_to_sub,
        )
        .optional()
        .map_err(Error::from)
    }

    pub(crate) fn list(&self) -> Result<Vec<Subscription>, Error> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::Invalid("store lock".into()))?;
        let mut stmt = conn.prepare(
            "SELECT id, device_id, endpoint, p256dh, auth FROM subscriptions ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], row_to_sub)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub(crate) fn list_device(&self, device_id: &str) -> Result<Vec<Subscription>, Error> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::Invalid("store lock".into()))?;
        let mut stmt = conn.prepare(
            "SELECT id, device_id, endpoint, p256dh, auth FROM subscriptions WHERE device_id = ?1",
        )?;
        let rows = stmt.query_map(params![device_id], row_to_sub)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub(crate) fn delete_endpoint(&self, endpoint: &str) -> Result<bool, Error> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::Invalid("store lock".into()))?;
        let n = conn.execute(
            "DELETE FROM subscriptions WHERE endpoint = ?1",
            params![endpoint],
        )?;
        Ok(n > 0)
    }
}

fn row_to_sub(row: &rusqlite::Row<'_>) -> rusqlite::Result<Subscription> {
    Ok(Subscription {
        id: row.get(0)?,
        device_id: row.get(1)?,
        endpoint: row.get(2)?,
        p256dh: row.get(3)?,
        auth: row.get(4)?,
    })
}

pub(crate) fn new_id() -> String {
    format!("push_{}", Uuid::now_v7())
}
