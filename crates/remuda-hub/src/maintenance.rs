//! Offline maintenance uses the same schema initialization as Hub startup.

use std::path::Path;

/// Apply idempotent Hub schema updates without starting HTTP, push, or Nodes.
/// The operator must stop the Hub first and keep a backup for schema rollback.
pub async fn migrate(data_dir: &Path) -> anyhow::Result<()> {
    let store = crate::store::Store::open(data_dir)?;
    // Opening the writer is asynchronous. A queued read proves initialization
    // finished and propagates a failed writer open instead of reporting success.
    let result = store
        .run(|conn| {
            conn.query_row("SELECT count(*) FROM devices", [], |row| {
                row.get::<_, i64>(0)
            })?;
            Ok(())
        })
        .await;
    store.close().await;
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migration_is_repeatable_and_preserves_data_without_bootstrapping() {
        let dir = tempfile::tempdir().unwrap();
        migrate(dir.path()).await.unwrap();
        let conn = rusqlite::Connection::open(dir.path().join("hub.sqlite")).unwrap();
        conn.execute("INSERT INTO devices (id,name,token_hash,created_at,last_seen_at) VALUES ('test','test','hash','now','now')", []).unwrap();
        drop(conn);
        migrate(dir.path()).await.unwrap();
        let conn = rusqlite::Connection::open(dir.path().join("hub.sqlite")).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM devices", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(!dir.path().join("bootstrap-token").exists());
        assert!(!dir.path().join("listen-addr").exists());
    }

    #[tokio::test]
    async fn migration_reports_corrupt_database() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hub.sqlite"), "invalid sqlite fixture").unwrap();
        assert!(migrate(dir.path()).await.is_err());
    }
}
