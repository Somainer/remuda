//! Durable Node entities in `<data_dir>/node.sqlite`.

use crate::NodeError;
use remuda_driver::LaunchRecipe;
use remuda_protocol::{Command, Instance, InstanceId};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::Mutex;

/// SQLite-backed instance, command, recipe, and pending-interaction records.
pub struct EntityDb {
    conn: Mutex<Connection>,
}

impl EntityDb {
    /// Open (or create) `<data_dir>/node.sqlite` with WAL + `synchronous=NORMAL`.
    pub fn open(data_dir: &Path) -> Result<Self, NodeError> {
        std::fs::create_dir_all(data_dir)?;
        let conn = Connection::open(data_dir.join("node.sqlite"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS instances (
                id TEXT PRIMARY KEY,
                journal_id TEXT NOT NULL UNIQUE,
                json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS commands (
                id TEXT PRIMARY KEY,
                instance_id TEXT NOT NULL,
                json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS recipes (
                instance_id TEXT PRIMARY KEY,
                json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS pty_resources (
                id TEXT PRIMARY KEY,
                json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS interactions (
                id TEXT PRIMARY KEY,
                json TEXT NOT NULL
            );
            ",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, NodeError> {
        self.conn.lock().map_err(|_| NodeError::StorePoisoned)
    }

    /// Insert or replace an Instance row.
    pub fn put_instance(&self, instance: &Instance) -> Result<(), NodeError> {
        let json = serde_json::to_string(instance)?;
        self.lock()?.execute(
            "INSERT OR REPLACE INTO instances (id, journal_id, json) VALUES (?1, ?2, ?3)",
            params![
                instance.meta.id.as_id().as_str(),
                instance.journal_id.as_str(),
                json
            ],
        )?;
        Ok(())
    }

    /// Load every persisted Instance.
    pub fn list_instances(&self) -> Result<Vec<Instance>, NodeError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT json FROM instances ORDER BY id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for json in rows {
            out.push(serde_json::from_str(&json?)?);
        }
        Ok(out)
    }

    /// Remove an Instance and everything keyed to it.
    ///
    /// Node-owned rows only: commands, its launch recipe, and the instance
    /// itself. The agent's own native transcripts live under the user's home
    /// and are never touched here.
    pub fn remove_instance(&self, instance_id: &InstanceId) -> Result<bool, NodeError> {
        let id = instance_id.as_id().as_str();
        let conn = self.lock()?;
        conn.execute("DELETE FROM commands WHERE instance_id = ?1", params![id])?;
        conn.execute("DELETE FROM recipes WHERE instance_id = ?1", params![id])?;
        let removed = conn.execute("DELETE FROM instances WHERE id = ?1", params![id])?;
        Ok(removed > 0)
    }

    /// Insert or replace a Command row.
    pub fn put_command(
        &self,
        instance_id: &InstanceId,
        command: &Command,
    ) -> Result<(), NodeError> {
        let json = serde_json::to_string(command)?;
        self.lock()?.execute(
            "INSERT OR REPLACE INTO commands (id, instance_id, json) VALUES (?1, ?2, ?3)",
            params![
                command.command_id.as_id().as_str(),
                instance_id.as_id().as_str(),
                json
            ],
        )?;
        Ok(())
    }

    /// Load commands belonging to one Instance, insertion-stable by id.
    pub fn commands_for(&self, instance_id: &InstanceId) -> Result<Vec<Command>, NodeError> {
        let conn = self.lock()?;
        let mut stmt =
            conn.prepare("SELECT json FROM commands WHERE instance_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map(params![instance_id.as_id().as_str()], |row| {
            row.get::<_, String>(0)
        })?;
        let mut out = Vec::new();
        for json in rows {
            out.push(serde_json::from_str(&json?)?);
        }
        Ok(out)
    }

    /// Persist a launch recipe with no secret values.
    pub fn put_recipe(
        &self,
        instance_id: &InstanceId,
        recipe: &LaunchRecipe,
    ) -> Result<(), NodeError> {
        let json = serde_json::to_string(recipe)?;
        self.lock()?.execute(
            "INSERT OR REPLACE INTO recipes (instance_id, json) VALUES (?1, ?2)",
            params![instance_id.as_id().as_str(), json],
        )?;
        Ok(())
    }

    /// Load a launch recipe when one was stored.
    pub fn recipe(&self, instance_id: &InstanceId) -> Result<Option<LaunchRecipe>, NodeError> {
        let conn = self.lock()?;
        let json: Option<String> = conn
            .query_row(
                "SELECT json FROM recipes WHERE instance_id = ?1",
                params![instance_id.as_id().as_str()],
                |row| row.get(0),
            )
            .optional()?;
        match json {
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
    }

    /// Persist a resource before its native launch.
    pub fn put_pty_resource(&self, resource: &remuda_driver::PtyResource) -> Result<(), NodeError> {
        self.lock()?.execute(
            "INSERT OR REPLACE INTO pty_resources (id, json) VALUES (?1, ?2)",
            params![resource.key(), serde_json::to_string(resource)?],
        )?;
        Ok(())
    }

    /// Load resources that still need reconciliation.
    pub fn pty_resources(&self) -> Result<Vec<remuda_driver::PtyResource>, NodeError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT json FROM pty_resources ORDER BY id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    /// Forget a verified removed carrier.
    pub fn remove_pty_resource(&self, key: &str) -> Result<(), NodeError> {
        self.lock()?
            .execute("DELETE FROM pty_resources WHERE id = ?1", params![key])?;
        Ok(())
    }

    /// Persist a pending Interaction JSON blob.
    pub fn put_interaction_json(&self, id: &str, json: &str) -> Result<(), NodeError> {
        self.lock()?.execute(
            "INSERT OR REPLACE INTO interactions (id, json) VALUES (?1, ?2)",
            params![id, json],
        )?;
        Ok(())
    }

    /// Load pending Interaction JSON blobs.
    pub fn list_interaction_jsons(&self) -> Result<Vec<String>, NodeError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT json FROM interactions ORDER BY id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for json in rows {
            out.push(json?);
        }
        Ok(out)
    }
}
