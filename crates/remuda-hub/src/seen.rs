//! seen-hub (H6): per-owner done-unseen tracking (the home ◉).
//!
//! An instance is **unseen** when its latest settled turn end has a journal
//! seq greater than the owner's seen seq. Three pieces carry that:
//!
//! * `instances.turn_end_seq` — the journal seq of the latest row whose
//!   projection moved the instance's activity from busy (`working`, `blocked`,
//!   `draining`) to `idle`: a root turn that settled, cleanly or with an error.
//!   Written in the append transaction, so it is exactly as durable as the row.
//! * `instance_views(instance_id, scope, seen_seq, seen_at)` — the highest seq
//!   the owner has seen. Monotonic: a lower report never moves it back, and a
//!   report past the journal is clamped to the instance's durable seq. `scope`
//!   is [`OWNER_SCOPE`] for every row today; the column is kept so later scopes
//!   can sit beside it without a migration.
//! * [`SeenPresence`] — ephemeral `visible` leases from owner clients that are
//!   showing the session right now. A turn end that lands while any lease is
//!   held is marked seen in the same transaction, so a turn the owner watched
//!   end never reads as unseen, not even for one poll.
//!
//! Successors start clean (home.md §7). A D-026 resumed child and a D-057
//! successor chapter are new instance ids with no seen row and no turn end;
//! they can read unseen only after a turn of their own ends. The predecessor
//! keeps its own seen state (the home shows ◉ only on a live idle row, so an
//! exited or fenced predecessor never surfaces it).

use crate::config::now_rfc3339;
use crate::store::{Store, StoreError, ensure_column, immediate_tx};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The only seen scope today: the owner, so the phone and the desktop agree.
pub const OWNER_SCOPE: &str = "owner";

/// How long one `visible: true` report keeps a client counted as showing the
/// session. Clients renew well inside it while the page stays visible and
/// release it with `visible: false` when they leave, so the lease only bounds
/// a client that vanished without releasing. Matches the 10 s freshness window
/// the home design uses for "stale".
pub const VISIBLE_LEASE: Duration = Duration::from_secs(10);

/// Activities a root turn runs in; a move from one of these to `idle` is the
/// turn end the ◉ tracks.
pub(crate) fn is_busy_activity(activity: &str) -> bool {
    matches!(activity, "working" | "blocked" | "draining")
}

/// Schema for user_version 4. Runs inside the ladder's transaction.
pub(crate) fn migrate(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS instance_views (
            instance_id TEXT NOT NULL,
            scope TEXT NOT NULL,
            seen_seq INTEGER NOT NULL,
            seen_at TEXT NOT NULL,
            PRIMARY KEY (instance_id, scope)
        );",
    )?;
    ensure_column(conn, "instances", "turn_end_seq", "INTEGER")
}

/// Owner clients currently showing a session, keyed by instance then device.
///
/// Presence, not state: never persisted, and empty after a Hub restart until
/// the visible clients renew. Keyed per device so one device leaving cannot
/// cancel another device that is still showing the same session.
#[derive(Default)]
pub struct SeenPresence {
    leases: Mutex<HashMap<String, HashMap<String, Instant>>>,
}

impl SeenPresence {
    /// `device_id` is showing `instance_id` now; good for [`VISIBLE_LEASE`].
    pub fn hold(&self, instance_id: &str, device_id: &str) {
        self.hold_at(instance_id, device_id, Instant::now());
    }

    fn hold_at(&self, instance_id: &str, device_id: &str, now: Instant) {
        let Ok(mut leases) = self.leases.lock() else {
            return;
        };
        // Expired leases from clients that never released are dropped here,
        // so the map stays bounded by what is actually on screen.
        leases.retain(|_, devices| {
            devices.retain(|_, until| *until > now);
            !devices.is_empty()
        });
        leases
            .entry(instance_id.to_owned())
            .or_default()
            .insert(device_id.to_owned(), now + VISIBLE_LEASE);
    }

    /// `device_id` stopped showing `instance_id`.
    pub fn release(&self, instance_id: &str, device_id: &str) {
        let Ok(mut leases) = self.leases.lock() else {
            return;
        };
        if let Some(devices) = leases.get_mut(instance_id) {
            devices.remove(device_id);
            if devices.is_empty() {
                leases.remove(instance_id);
            }
        }
    }

    /// Whether any owner client holds an unexpired lease on `instance_id`.
    pub fn is_visible(&self, instance_id: &str) -> bool {
        self.is_visible_at(instance_id, Instant::now())
    }

    fn is_visible_at(&self, instance_id: &str, now: Instant) -> bool {
        // A poisoned lock reads as "not visible": the safe failure is a stray
        // ◉ the owner can clear, never a turn end silently marked seen.
        self.leases
            .lock()
            .ok()
            .and_then(|leases| {
                leases
                    .get(instance_id)
                    .map(|devices| devices.values().any(|until| *until > now))
            })
            .unwrap_or(false)
    }
}

/// The two seqs that decide `unseen`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SeenMarks {
    /// Latest settled turn end; `None` before the first one.
    pub turn_end_seq: Option<i64>,
    /// Owner's seen seq; `None` when the owner never reported one.
    pub seen_seq: Option<i64>,
}

impl SeenMarks {
    /// A turn end newer than what the owner saw; "never seen" sits below
    /// every journal seq.
    pub fn unseen(&self) -> bool {
        self.turn_end_seq
            .is_some_and(|end| end > self.seen_seq.unwrap_or(-1))
    }
}

/// Seen state of one instance: the `POST /v1/instances/{id}/seen` response and
/// the `instance.seen` follow event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeenView {
    pub instance_id: String,
    pub unseen: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seen_seq: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seen_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_end_seq: Option<String>,
}

/// Result of one seen report.
#[derive(Clone, Debug)]
pub struct SeenMarked {
    pub view: SeenView,
    /// The seen seq moved; callers broadcast only then.
    pub advanced: bool,
}

fn load_marks(
    conn: &Connection,
    instance_id: &str,
) -> Result<Option<(SeenMarks, Option<String>)>, rusqlite::Error> {
    conn.query_row(
        "SELECT i.turn_end_seq, v.seen_seq, v.seen_at
           FROM instances i
           LEFT JOIN instance_views v ON v.instance_id = i.id AND v.scope = ?2
          WHERE i.id = ?1",
        params![instance_id, OWNER_SCOPE],
        |row| {
            Ok((
                SeenMarks {
                    turn_end_seq: row.get(0)?,
                    seen_seq: row.get(1)?,
                },
                row.get(2)?,
            ))
        },
    )
    .optional()
}

pub(crate) fn load_view(
    conn: &Connection,
    instance_id: &str,
) -> Result<Option<SeenView>, rusqlite::Error> {
    Ok(
        load_marks(conn, instance_id)?.map(|(marks, seen_at)| SeenView {
            instance_id: instance_id.to_owned(),
            unseen: marks.unseen(),
            seen_seq: marks.seen_seq.map(|seq| seq.to_string()),
            seen_at,
            turn_end_seq: marks.turn_end_seq.map(|seq| seq.to_string()),
        }),
    )
}

/// Raise the owner's seen seq to `seq`; a lower or equal value is a no-op.
fn advance_seen(
    conn: &Connection,
    instance_id: &str,
    seq: i64,
    now: &str,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO instance_views (instance_id, scope, seen_seq, seen_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(instance_id, scope) DO UPDATE
            SET seen_seq = excluded.seen_seq, seen_at = excluded.seen_at
          WHERE excluded.seen_seq > instance_views.seen_seq",
        params![instance_id, OWNER_SCOPE, seq, now],
    )?;
    Ok(())
}

/// Record a settled turn end at `seq` inside the append transaction.
///
/// `visible` is whether an owner client held a lease when the row landed; the
/// turn end is then marked seen in the same write. Returns the new view for
/// the caller to broadcast after commit.
pub(crate) fn fold_turn_end(
    conn: &Connection,
    instance_id: &str,
    seq: i64,
    visible: bool,
    now: &str,
) -> Result<Option<SeenView>, rusqlite::Error> {
    conn.execute(
        "UPDATE instances SET turn_end_seq = MAX(COALESCE(turn_end_seq, ?1), ?1) WHERE id = ?2",
        params![seq, instance_id],
    )?;
    if visible {
        advance_seen(conn, instance_id, seq, now)?;
    }
    load_view(conn, instance_id)
}

impl Store {
    /// One seen report from an owner device: the route's write path.
    ///
    /// `visible: Some(true)` takes the device's lease BEFORE the seen write is
    /// queued, so a turn end appended while this report waits on the writer is
    /// folded seen instead of racing it (a lease taken only after the write
    /// returned let such a turn end read unseen on the page being shown). The
    /// lease stays even if the write fails: the client is still showing the
    /// session and renews. `Some(false)` releases only after a successful
    /// write, so a failed leave keeps nothing it did not already have.
    pub async fn report_seen(
        &self,
        instance_id: String,
        device_id: &str,
        durable_seq: i64,
        visible: Option<bool>,
    ) -> Result<Option<SeenMarked>, StoreError> {
        if visible == Some(true) {
            self.seen_presence().hold(&instance_id, device_id);
        }
        let marked = self.mark_seen(instance_id.clone(), durable_seq).await?;
        if visible == Some(false) && marked.is_some() {
            self.seen_presence().release(&instance_id, device_id);
        }
        Ok(marked)
    }

    /// Raise the owner's seen seq for `instance_id` to `durable_seq`, clamped
    /// to the instance's durable seq. `None` when the instance does not exist.
    pub async fn mark_seen(
        &self,
        instance_id: String,
        durable_seq: i64,
    ) -> Result<Option<SeenMarked>, StoreError> {
        self.run_named("mark_seen", move |conn| {
            let tx = immediate_tx(conn)?;
            let Some(durable) = tx
                .query_row(
                    "SELECT durable_seq FROM instances WHERE id = ?1",
                    params![instance_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
            else {
                return Ok(None);
            };
            let before = load_marks(&tx, &instance_id)?.map(|(marks, _)| marks.seen_seq);
            advance_seen(
                &tx,
                &instance_id,
                durable_seq.clamp(0, durable),
                &now_rfc3339(),
            )?;
            let view = load_view(&tx, &instance_id)?
                .ok_or_else(|| StoreError::Id("instance missing during seen".into()))?;
            tx.commit()?;
            let after = view.seen_seq.as_deref().and_then(|seq| seq.parse().ok());
            Ok(Some(SeenMarked {
                advanced: before.flatten() != after,
                view,
            }))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unseen_needs_a_turn_end_past_the_seen_seq() {
        let marks = |turn_end_seq, seen_seq| SeenMarks {
            turn_end_seq,
            seen_seq,
        };
        assert!(!marks(None, None).unseen(), "no turn end yet");
        assert!(marks(Some(4), None).unseen(), "never seen");
        assert!(marks(Some(4), Some(3)).unseen());
        assert!(!marks(Some(4), Some(4)).unseen());
        assert!(!marks(Some(4), Some(9)).unseen());
    }

    #[test]
    fn a_lease_is_per_device_and_bounded() {
        let presence = SeenPresence::default();
        let start = Instant::now();
        presence.hold_at("ins_a", "dev_phone", start);
        presence.hold_at("ins_a", "dev_desk", start);
        assert!(presence.is_visible_at("ins_a", start));
        assert!(!presence.is_visible_at("ins_b", start));
        // One device leaving does not cancel the other.
        presence.release("ins_a", "dev_phone");
        assert!(presence.is_visible_at("ins_a", start));
        presence.release("ins_a", "dev_desk");
        assert!(!presence.is_visible_at("ins_a", start));
        // A client that vanished without releasing stops counting once its
        // lease runs out, and the next hold prunes it.
        presence.hold_at("ins_a", "dev_gone", start);
        let later = start + VISIBLE_LEASE;
        assert!(!presence.is_visible_at("ins_a", later));
        presence.hold_at("ins_c", "dev_desk", later);
        let leases = presence.leases.lock().unwrap();
        assert!(!leases.contains_key("ins_a"), "expired lease pruned");
        assert!(leases.contains_key("ins_c"));
    }

    fn turn(name: &str, status: &str, related: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "kind": "lifecycle",
            "payload": {
                "type": "native", "topic": "turn", "nativeName": name,
                "nativeId": { "state": "known", "value": "sess-seen" },
                "status": { "state": "known", "value": status },
                "relatedIds": related, "dataRef": null, "severity": "info",
                "affectsCompletion": false
            }
        })
    }

    async fn finish_turn(store: &Store, host: &str, instance: &str) -> i64 {
        store
            .append_journal(
                host.to_owned(),
                instance.to_owned(),
                None,
                turn("turn_started", "working", serde_json::json!({})),
            )
            .await
            .expect("turn start");
        let end = store
            .append_journal(
                host.to_owned(),
                instance.to_owned(),
                None,
                turn(
                    "result",
                    "turn_done",
                    serde_json::json!({ "settledRootTurn": "true" }),
                ),
            )
            .await
            .expect("turn end");
        assert!(end.seen.is_some(), "a settled turn end folds seen state");
        end.record.seq
    }

    fn user_version(dir: &std::path::Path) -> i64 {
        rusqlite::Connection::open(dir.join("hub.sqlite"))
            .unwrap()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    /// The seen schema is present. Checked after every open, so a rung the
    /// ladder skipped (e.g. a version number another rung already claimed)
    /// fails here by name rather than as a missing column deep in a read.
    fn assert_seen_schema(dir: &std::path::Path, when: &str) {
        let db = rusqlite::Connection::open(dir.join("hub.sqlite")).unwrap();
        assert!(
            crate::store::column_exists(&db, "instances", "turn_end_seq").unwrap(),
            "{when}: instances.turn_end_seq is missing — the seen rung did not run"
        );
        let table: Option<String> = db
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'instance_views'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert!(
            table.is_some(),
            "{when}: instance_views is missing — the seen rung did not run"
        );
        assert!(crate::store::column_exists(&db, "instance_views", "seen_seq").unwrap());
    }

    /// The D-060 memory schema is present (the rung below the seen one).
    fn assert_memory_schema(dir: &std::path::Path, when: &str) {
        let db = rusqlite::Connection::open(dir.join("hub.sqlite")).unwrap();
        for object in ["table memory_entries", "table memory_fts"] {
            let (kind, name) = object.split_once(' ').unwrap();
            let found: Option<String> = db
                .query_row(
                    "SELECT name FROM sqlite_master WHERE type = ?1 AND name = ?2",
                    params![kind, name],
                    |row| row.get(0),
                )
                .optional()
                .unwrap();
            assert!(
                found.is_some(),
                "{when}: {name} is missing — the memory rung did not run"
            );
        }
    }

    /// The seen rung, from the version-4 (memory-only) schema: the database
    /// gains the `instance_views` table and `instances.turn_end_seq`, every
    /// existing row reads seen (an upgrade never floods the home with ◉), and
    /// turns after the upgrade are tracked. Written against the rung
    /// constants, so a renumber keeps it honest.
    #[tokio::test]
    async fn the_seen_rung_upgrades_the_previous_schema_without_flooding_unseen() {
        use crate::store::{SCHEMA_USER_VERSION, SEEN_SCHEMA_RUNG};
        let dir = tempfile::tempdir().unwrap();
        let (store, host) = crate::store_test_support::open_with_host(dir.path(), "seen-upgrade")
            .await
            .unwrap();
        // A fresh database climbs the whole ladder: memory (rung 4) then
        // seen (rung 5), ending at the top with both schemas present.
        assert_memory_schema(dir.path(), "fresh database");
        assert_seen_schema(dir.path(), "fresh database");
        assert_eq!(user_version(dir.path()), SCHEMA_USER_VERSION);
        store
            .ensure_instance(host.clone(), "ins-seen-old".into())
            .await
            .unwrap();
        finish_turn(&store, &host, "ins-seen-old").await;
        store.close().await;

        // Rewind to the exact pre-seen shape: user_version 4, the D-060
        // memory schema present, neither seen object. The row keeps its
        // journal (including the finished turn).
        let db = rusqlite::Connection::open(dir.path().join("hub.sqlite")).unwrap();
        db.execute_batch(&format!(
            "DROP TABLE instance_views;
             ALTER TABLE instances DROP COLUMN turn_end_seq;
             PRAGMA user_version = {};",
            SEEN_SCHEMA_RUNG - 1
        ))
        .unwrap();
        drop(db);

        let store = Store::open(dir.path()).unwrap();
        assert_memory_schema(dir.path(), "upgrade from the memory-only rung");
        assert_seen_schema(dir.path(), "upgrade from the memory-only rung");
        assert_eq!(user_version(dir.path()), SCHEMA_USER_VERSION);
        let old = store
            .get_instance("ins-seen-old".into())
            .await
            .unwrap()
            .unwrap();
        assert!(!old.unseen, "pre-upgrade turn ends are not resurrected");
        assert_eq!(old.turn_end_seq, None);

        // Turns after the upgrade are tracked and the route's store call works.
        let end = finish_turn(&store, &host, "ins-seen-old").await;
        let row = store
            .get_instance("ins-seen-old".into())
            .await
            .unwrap()
            .unwrap();
        assert!(row.unseen);
        assert_eq!(row.turn_end_seq, Some(end.to_string()));
        let marked = store
            .mark_seen("ins-seen-old".into(), end)
            .await
            .unwrap()
            .unwrap();
        assert!(marked.advanced && !marked.view.unseen);
        store.close().await;

        // Re-opening at the top rung does not re-run it and keeps the state.
        let store = Store::open(dir.path()).unwrap();
        assert_seen_schema(dir.path(), "re-open at the top rung");
        assert_eq!(user_version(dir.path()), SCHEMA_USER_VERSION);
        let row = store
            .get_instance("ins-seen-old".into())
            .await
            .unwrap()
            .unwrap();
        assert!(!row.unseen);
        assert_eq!(row.seen_seq, Some(end.to_string()));
        store.close().await;
    }

    /// The one ladder from below both new rungs: a version-3 database runs the
    /// memory rung and THEN the seen rung, each bumping the marker, and ends
    /// at the top with both schemas present.
    #[tokio::test]
    async fn a_version_three_database_runs_memory_then_seen() {
        use crate::store::SCHEMA_USER_VERSION;
        let dir = tempfile::tempdir().unwrap();
        let (store, _host) = crate::store_test_support::open_with_host(dir.path(), "seen-v3")
            .await
            .unwrap();
        assert_eq!(user_version(dir.path()), SCHEMA_USER_VERSION);
        store.close().await;

        // Rewind past BOTH rungs: neither schema, marker at 3.
        let db = rusqlite::Connection::open(dir.path().join("hub.sqlite")).unwrap();
        db.execute_batch(
            "DROP TABLE instance_views;
             ALTER TABLE instances DROP COLUMN turn_end_seq;
             DROP TRIGGER memory_entries_fts_insert;
             DROP TRIGGER memory_entries_fts_delete;
             DROP TRIGGER memory_entries_fts_update;
             DROP TABLE memory_fts;
             DROP TABLE memory_slug_revisions;
             DROP TABLE memory_entries;
             PRAGMA user_version = 3;",
        )
        .unwrap();
        drop(db);
        assert_eq!(user_version(dir.path()), 3);

        let store = Store::open(dir.path()).unwrap();
        assert_memory_schema(dir.path(), "after the v3 ladder walk");
        assert_seen_schema(dir.path(), "after the v3 ladder walk");
        assert_eq!(user_version(dir.path()), SCHEMA_USER_VERSION);
        store.close().await;
    }

    /// r2 item 3: a `visible: true` report takes its lease BEFORE its seen
    /// write is queued, so a turn end appended while the report waits on the
    /// writer is folded seen instead of racing it. The writer is parked so the
    /// order is deterministic: the report's write sits behind the park, and the
    /// lease must already be held.
    #[tokio::test]
    async fn a_visible_report_holds_its_lease_before_its_write_is_queued() {
        let dir = tempfile::tempdir().unwrap();
        let (store, host) = crate::store_test_support::open_with_host(dir.path(), "seen-race")
            .await
            .unwrap();
        let id = "ins-seen-race".to_owned();
        store
            .ensure_instance(host.clone(), id.clone())
            .await
            .unwrap();
        store
            .append_journal(
                host.clone(),
                id.clone(),
                None,
                turn("turn_started", "working", serde_json::json!({})),
            )
            .await
            .unwrap();
        let shown = store.get_instance(id.clone()).await.unwrap().unwrap();

        // Park the single writer.
        let (parked_tx, parked_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let park = {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .run_named("seen_race_park", move |_| {
                        let _ = parked_tx.send(());
                        let _ = release_rx.recv();
                        Ok(())
                    })
                    .await
            })
        };
        tokio::task::spawn_blocking(move || parked_rx.recv())
            .await
            .unwrap()
            .expect("writer parked");

        // The desk opens the session: its report queues behind the park.
        let report = {
            let store = store.clone();
            let id = id.clone();
            let seq: i64 = shown.durable_seq.parse().unwrap();
            tokio::spawn(async move { store.report_seen(id, "dev_desk", seq, Some(true)).await })
        };
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !store.seen_presence().is_visible(&id) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            store.seen_presence().is_visible(&id),
            "the lease is held while the report's seen write is still queued"
        );

        // The turn ends while the report is still queued, then the writer runs.
        let end = {
            let store = store.clone();
            let host = host.clone();
            let id = id.clone();
            tokio::spawn(async move {
                store
                    .append_journal(
                        host,
                        id,
                        None,
                        turn(
                            "result",
                            "turn_done",
                            serde_json::json!({ "settledRootTurn": "true" }),
                        ),
                    )
                    .await
            })
        };
        release_tx.send(()).unwrap();
        park.await.unwrap().unwrap();
        report.await.unwrap().unwrap().expect("instance exists");
        let end = end.await.unwrap().unwrap();
        assert!(end.seen.is_some(), "a settled turn end");

        let row = store.get_instance(id).await.unwrap().unwrap();
        assert!(!row.unseen, "watched turn end folded seen: {row:?}");
        assert_eq!(row.seen_seq, Some(end.record.seq.to_string()));
        store.close().await;
    }

    /// Deleting an instance removes its seen row with it.
    #[tokio::test]
    async fn seen_rows_go_with_a_deleted_instance() {
        let dir = tempfile::tempdir().unwrap();
        let (store, host) = crate::store_test_support::open_with_host(dir.path(), "seen-delete")
            .await
            .unwrap();
        store
            .ensure_instance(host.clone(), "ins-seen-gone".into())
            .await
            .unwrap();
        let end = finish_turn(&store, &host, "ins-seen-gone").await;
        store
            .mark_seen("ins-seen-gone".into(), end)
            .await
            .unwrap()
            .unwrap();
        store
            .append_journal(
                host.clone(),
                "ins-seen-gone".into(),
                None,
                serde_json::json!({
                    "kind": "lifecycle",
                    "payload": { "type": "entity", "state": "exited", "reasonCode": "native-exit" }
                }),
            )
            .await
            .unwrap();
        store
            .delete_instance("ins-seen-gone".into())
            .await
            .unwrap()
            .expect("deleted");
        let left: i64 = store
            .run_named("seen_count", |conn| {
                Ok(conn.query_row("SELECT COUNT(*) FROM instance_views", [], |row| row.get(0))?)
            })
            .await
            .unwrap();
        assert_eq!(left, 0);
        assert!(
            store
                .mark_seen("ins-seen-gone".into(), 1)
                .await
                .unwrap()
                .is_none()
        );
        store.close().await;
    }
}
