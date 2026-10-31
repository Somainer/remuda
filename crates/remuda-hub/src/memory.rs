//! D-060 (cb-memory-core): the main agent's memory as per-entry Hub rows.
//!
//! One row per entry, keyed by `(lineage_id, slug)`, so memory follows the
//! lineage across chapters and Nodes instead of living in one host's files
//! (failure A6). The rules this module enforces:
//!
//! - **Revision CAS per entry.** A create on an existing slug is a 409; an
//!   update or delete carries `expectedRevision` and is a 409 when stale.
//!   Consolidate replaces N entries with one, CAS on every input, in one
//!   writer transaction.
//! - **Writers.** The owner (Human origin) may write any lineage's memory. An
//!   Agent may write only its own lineage's memory, and only from the
//!   lineage's current chapter (checked inside the writer transaction, so a
//!   continuation that moves the chapter serializes against it). Bot origin
//!   is refused.
//! - **Readers.** The owner reads any lineage; an Agent reads its own.
//! - **Retirement archives.** Deleting a lineage stamps `archived_at` on its
//!   entries in the delete transaction; archived entries leave the index and
//!   search until the owner restores them. The owner can still update,
//!   delete and consolidate retained entries without the lineage row; no new
//!   memory is created for a lineage that no longer exists.
//! - **Search.** FTS5 `MATCH` ranked by bm25, plus a `LIKE` pass for what the
//!   tokenizer cannot see: `unicode61` makes a run of CJK characters one
//!   token, so a two-character query such as 边界 only hits by substring.
//! - **Sizes.** Soft warnings (entry 2 KB, index 4 KB) report the actual
//!   numbers; above 8 KB an entry is refused.
//!
//! The FTS table is external-content over `memory_entries` and kept in sync by
//! triggers, so every write path (including the retire archive) updates it in
//! the same transaction as the row.

use crate::AppState;
use crate::agent_scope::{authenticated_caller, caller, origin};
use crate::auth::require_origin;
use crate::config::now_rfc3339;
use crate::error::{HubError, NextStep, action};
use crate::store::{Device, Store, StoreError, immediate_tx};
use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use remuda_protocol::{
    InputOrigin, MEMORY_ENTRY_HARD_BYTES, MEMORY_ENTRY_SOFT_BYTES, MEMORY_INDEX_SOFT_BYTES,
    MemoryEntry, MemoryIndex, MemoryLimit, MemoryMatch, MemoryOrigin, MemorySearchHit,
    MemorySizeWarning, MemorySourceRef, MemoryTurnRef, MemoryType, MemoryWriteItem, MemoryWriteOp,
    MemoryWritePage, MemoryWriteResult, memory_entry_bytes, memory_index_line,
    validate_memory_slug,
};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::Deserialize;
use serde_json::{Value, json};

/// The `PRAGMA user_version` step that creates the memory tables.
pub(crate) const MEMORY_SCHEMA_VERSION: i64 = 4;

/// The `PRAGMA user_version` step that adds the per-write log
/// (`memory_write_log`) and the entry's `turn_seq` projection (D-060 item 4,
/// cb-memory-ui). It sits above the seen rung; a rung that lands before this
/// one takes its number, and this one renumbers when the branches merge.
pub(crate) const MEMORY_WRITE_LOG_SCHEMA_VERSION: i64 = 6;

/// Caller-supplied source references per write.
const MAX_SOURCE_REFS: usize = 64;
/// Inputs per consolidate.
const MAX_CONSOLIDATE_SOURCES: usize = 32;
/// Search terms considered (whitespace-separated).
const MAX_SEARCH_TERMS: usize = 8;
/// Search hits returned by default / at most.
const DEFAULT_SEARCH_LIMIT: usize = 10;
const MAX_SEARCH_LIMIT: usize = 100;
/// Rows returned by one memory-writes poll.
const MAX_MEMORY_WRITES: usize = 500;
/// The one entry only the owner authors (D-060 §8.4): a seat reads it, never
/// writes it.
pub(crate) const OWNER_AUTHORED_SLUG: &str = "about-me";

const ENTRY_COLUMNS: &str = "lineage_id, slug, type, title, description, body, revision, \
     origin, source_refs_json, writer_instance, created_at, updated_at, archived_at, turn_seq";
/// [`ENTRY_COLUMNS`] qualified with the `m` alias, for queries that join the
/// FTS table (which has its own title/description/body columns).
const ENTRY_COLUMNS_M: &str = "m.lineage_id, m.slug, m.type, m.title, m.description, m.body, \
     m.revision, m.origin, m.source_refs_json, m.writer_instance, m.created_at, m.updated_at, \
     m.archived_at, m.turn_seq";

// ─── Migration ─────────────────────────────────────────────────────────────

/// `user_version` 4: the memory table, its FTS5 index and the sync triggers.
///
/// `id` is an INTEGER PRIMARY KEY so the FTS rowid survives VACUUM (a plain
/// rowid may be renumbered), and AUTOINCREMENT so a deleted row's id is never
/// issued again: nothing outside one transaction may mistake a new row for an
/// old one. `root_turn_json` is reserved for the root turn
/// that caused an Agent write (cb-rootturn) and stays NULL until then.
/// `type` and `origin` carry no CHECK: SQLite cannot alter one, and both are
/// validated before every write.
///
/// `memory_slug_revisions` keeps the highest revision ever issued for each
/// (lineage, slug). A delete leaves it behind, so a recreated slug continues
/// after its last revision and a stale `expectedRevision` can never match a
/// replacement entry (no ABA across delete and recreate).
pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memory_entries (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            lineage_id TEXT NOT NULL,
            slug TEXT NOT NULL,
            type TEXT NOT NULL,
            title TEXT NOT NULL,
            description TEXT NOT NULL,
            body TEXT NOT NULL,
            revision INTEGER NOT NULL,
            origin TEXT NOT NULL,
            root_turn_json TEXT,
            source_refs_json TEXT NOT NULL DEFAULT '[]',
            writer_instance TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            archived_at TEXT,
            UNIQUE (lineage_id, slug)
        );
        CREATE TABLE IF NOT EXISTS memory_slug_revisions (
            lineage_id TEXT NOT NULL,
            slug TEXT NOT NULL,
            revision INTEGER NOT NULL,
            PRIMARY KEY (lineage_id, slug)
        ) WITHOUT ROWID;
        CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
            title, description, body,
            content = 'memory_entries', content_rowid = 'id',
            tokenize = 'porter unicode61'
        );
        CREATE TRIGGER IF NOT EXISTS memory_entries_fts_insert
        AFTER INSERT ON memory_entries BEGIN
            INSERT INTO memory_fts (rowid, title, description, body)
            VALUES (new.id, new.title, new.description, new.body);
        END;
        CREATE TRIGGER IF NOT EXISTS memory_entries_fts_delete
        AFTER DELETE ON memory_entries BEGIN
            INSERT INTO memory_fts (memory_fts, rowid, title, description, body)
            VALUES ('delete', old.id, old.title, old.description, old.body);
        END;
        CREATE TRIGGER IF NOT EXISTS memory_entries_fts_update
        AFTER UPDATE OF title, description, body ON memory_entries BEGIN
            INSERT INTO memory_fts (memory_fts, rowid, title, description, body)
            VALUES ('delete', old.id, old.title, old.description, old.body);
            INSERT INTO memory_fts (rowid, title, description, body)
            VALUES (new.id, new.title, new.description, new.body);
        END;",
    )
}

/// `user_version` 6 (cb-memory-ui): the per-write log plus the entry's
/// `turn_seq` projection.
///
/// `memory_write_log` is append-only and has no foreign key to
/// `memory_entries`: a row outlives a later rewrite or delete of its entry,
/// because every turn that wrote memory keeps its row (D-060). `title` is
/// captured at write time for the same reason. `turn_seq` on the entry row is
/// the latest write's turn; the log keeps every one.
pub(crate) fn migrate_write_log(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memory_write_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            lineage_id TEXT NOT NULL,
            slug TEXT NOT NULL,
            revision INTEGER NOT NULL,
            title TEXT NOT NULL,
            op TEXT NOT NULL,
            origin TEXT NOT NULL,
            writer_instance TEXT,
            turn_seq INTEGER,
            written_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS memory_write_log_writer
            ON memory_write_log (writer_instance, turn_seq);
        CREATE INDEX IF NOT EXISTS memory_write_log_slug
            ON memory_write_log (lineage_id, slug, id);",
    )?;
    // The entry row names the latest write's turn so a read needs no join;
    // `memory_write_log` remains the append-only record of every write.
    crate::store::ensure_column(conn, "memory_entries", "turn_seq", "INTEGER")?;
    Ok(())
}

/// Lineage retirement (the whole-lineage delete): archive, never delete, its
/// entries. Runs inside the delete transaction.
pub(crate) fn archive_lineage(
    conn: &Connection,
    lineage_id: &str,
    now: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE memory_entries SET archived_at = ?2
          WHERE lineage_id = ?1 AND archived_at IS NULL",
        params![lineage_id, now],
    )
}

// ─── Errors ────────────────────────────────────────────────────────────────

/// One stale `expectedRevision`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RevisionMismatch {
    pub slug: String,
    pub expected: u64,
    pub current: u64,
}

/// Memory refusals. Rendered with a stable `code` so the CLI and the PWA can
/// tell a stale revision from a missing entry.
#[derive(Debug)]
pub(crate) enum MemoryError {
    BadRequest(String),
    NotFound(String),
    Forbidden(String),
    /// A create (or consolidate target) named a slug the lineage already has.
    SlugExists {
        slug: String,
        revision: u64,
        archived: bool,
    },
    /// One or more `expectedRevision`s are stale.
    RevisionConflict(Vec<RevisionMismatch>),
    /// The entry is archived; the owner must restore it first.
    Archived(String),
    /// Above the hard per-entry limit.
    TooLarge {
        slug: String,
        bytes: u64,
    },
    Hub(HubError),
}

impl From<rusqlite::Error> for MemoryError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Hub(HubError::Store(StoreError::Sqlite(error)))
    }
}

impl From<StoreError> for MemoryError {
    fn from(error: StoreError) -> Self {
        Self::Hub(crate::http::map_store(error))
    }
}

impl From<HubError> for MemoryError {
    fn from(error: HubError) -> Self {
        Self::Hub(error)
    }
}

impl IntoResponse for MemoryError {
    fn into_response(self) -> Response {
        let (status, body) = match self {
            Self::Hub(error) => return error.into_response(),
            Self::BadRequest(message) => (
                StatusCode::BAD_REQUEST,
                json!({"error": message, "code": "BAD_REQUEST"}),
            ),
            Self::NotFound(message) => (
                StatusCode::NOT_FOUND,
                json!({"error": message, "code": "NOT_FOUND"}),
            ),
            Self::Forbidden(message) => (
                StatusCode::FORBIDDEN,
                json!({"error": message, "code": "FORBIDDEN"}),
            ),
            Self::SlugExists {
                slug,
                revision,
                archived,
            } => (
                StatusCode::CONFLICT,
                json!({
                    "error": format!(
                        "memory entry {slug:?} already exists at revision {revision}{}; \
                         update it with expectedRevision {revision}",
                        if archived { " (archived)" } else { "" }
                    ),
                    "code": "MEMORY_SLUG_EXISTS",
                    "slug": slug,
                    "currentRevision": revision,
                    "archived": archived,
                }),
            ),
            Self::RevisionConflict(mismatches) => {
                let detail = mismatches
                    .iter()
                    .map(|m| {
                        format!(
                            "{:?} is at revision {} (expected {})",
                            m.slug, m.current, m.expected
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                let conflicts: Vec<Value> = mismatches
                    .iter()
                    .map(|m| {
                        json!({
                            "slug": m.slug,
                            "expectedRevision": m.expected,
                            "currentRevision": m.current,
                        })
                    })
                    .collect();
                let mut body = json!({
                    "error": format!("stale memory revision: {detail}; re-read and retry"),
                    "code": "MEMORY_REVISION_CONFLICT",
                    "conflicts": conflicts,
                });
                if let [only] = mismatches.as_slice() {
                    body["slug"] = json!(only.slug);
                    body["currentRevision"] = json!(only.current);
                }
                (StatusCode::CONFLICT, body)
            }
            Self::Archived(slug) => (
                StatusCode::CONFLICT,
                json!({
                    "error": format!(
                        "memory entry {slug:?} is archived; the owner must restore it first"
                    ),
                    "code": "MEMORY_ARCHIVED",
                    "slug": slug,
                }),
            ),
            Self::TooLarge { slug, bytes } => (
                StatusCode::PAYLOAD_TOO_LARGE,
                json!({
                    "error": format!(
                        "memory entry {slug:?} is {bytes} bytes; the hard limit is \
                         {MEMORY_ENTRY_HARD_BYTES} bytes (title + description + body)"
                    ),
                    "code": "MEMORY_ENTRY_TOO_LARGE",
                    "slug": slug,
                    "bytes": bytes,
                    "maxBytes": MEMORY_ENTRY_HARD_BYTES,
                }),
            ),
        };
        (status, Json(body)).into_response()
    }
}

// ─── Inputs ────────────────────────────────────────────────────────────────

/// Who is writing, as authenticated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemoryWriter {
    /// Human origin: any lineage.
    Owner,
    /// Agent origin, bound to this instance (chapter).
    Agent { instance_id: String },
}

impl MemoryWriter {
    fn origin(&self) -> MemoryOrigin {
        match self {
            Self::Owner => MemoryOrigin::Human,
            Self::Agent { .. } => MemoryOrigin::Agent,
        }
    }

    fn origin_str(&self) -> &'static str {
        match self.origin() {
            MemoryOrigin::Human => "human",
            MemoryOrigin::Agent => "agent",
        }
    }

    fn instance(&self) -> Option<&str> {
        match self {
            Self::Owner => None,
            Self::Agent { instance_id } => Some(instance_id),
        }
    }
}

/// A full entry as written by create or consolidate.
#[derive(Debug, Clone)]
pub(crate) struct MemoryInput {
    pub slug: String,
    pub memory_type: MemoryType,
    pub title: String,
    pub description: String,
    pub body: String,
    pub source_refs: Vec<MemorySourceRef>,
}

/// Fields an update replaces; `None` keeps the current value. Source refs
/// are appended (an updated entry derives from both the old and new rows).
#[derive(Debug, Clone, Default)]
pub(crate) struct MemoryPatch {
    pub memory_type: Option<MemoryType>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub body: Option<String>,
    pub source_refs: Vec<MemorySourceRef>,
}

fn one_line(field: &str, value: &str) -> Result<String, MemoryError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(MemoryError::BadRequest(format!(
            "{field} must not be empty"
        )));
    }
    if trimmed.contains(['\n', '\r']) {
        return Err(MemoryError::BadRequest(format!("{field} must be one line")));
    }
    Ok(trimmed.to_string())
}

fn check_slug(slug: &str) -> Result<(), MemoryError> {
    validate_memory_slug(slug).map_err(MemoryError::BadRequest)
}

fn check_source_refs(refs: &[MemorySourceRef]) -> Result<(), MemoryError> {
    if refs.len() > MAX_SOURCE_REFS {
        return Err(MemoryError::BadRequest(format!(
            "at most {MAX_SOURCE_REFS} sourceRefs per write"
        )));
    }
    for source in refs {
        let id = source.instance_id.as_str();
        if id.is_empty() || id.len() > 128 || id.chars().any(char::is_whitespace) {
            return Err(MemoryError::BadRequest(format!(
                "sourceRefs instanceId {id:?} is not an instance id"
            )));
        }
    }
    Ok(())
}

fn check_size(slug: &str, title: &str, description: &str, body: &str) -> Result<(), MemoryError> {
    let bytes = memory_entry_bytes(title, description, body);
    if bytes > MEMORY_ENTRY_HARD_BYTES {
        return Err(MemoryError::TooLarge {
            slug: slug.to_string(),
            bytes,
        });
    }
    Ok(())
}

impl MemoryInput {
    fn normalized(mut self) -> Result<Self, MemoryError> {
        check_slug(&self.slug)?;
        self.title = one_line("title", &self.title)?;
        self.description = one_line("description", &self.description)?;
        check_source_refs(&self.source_refs)?;
        check_size(&self.slug, &self.title, &self.description, &self.body)?;
        self.source_refs = merge_refs(&[], &self.source_refs);
        Ok(self)
    }
}

impl MemoryPatch {
    fn normalized(mut self) -> Result<Self, MemoryError> {
        if let Some(title) = &self.title {
            self.title = Some(one_line("title", title)?);
        }
        if let Some(description) = &self.description {
            self.description = Some(one_line("description", description)?);
        }
        check_source_refs(&self.source_refs)?;
        Ok(self)
    }
}

/// `base` then `extra`, first occurrence wins.
fn merge_refs(base: &[MemorySourceRef], extra: &[MemorySourceRef]) -> Vec<MemorySourceRef> {
    let mut out: Vec<MemorySourceRef> = Vec::with_capacity(base.len() + extra.len());
    for source in base.iter().chain(extra) {
        if !out.contains(source) {
            out.push(source.clone());
        }
    }
    out
}

// ─── Row access ────────────────────────────────────────────────────────────

fn conversion(index: usize, message: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, message.into())
}

fn entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryEntry> {
    let raw_type: String = row.get(2)?;
    let memory_type = MemoryType::parse(&raw_type)
        .ok_or_else(|| conversion(2, format!("unknown memory type {raw_type:?}")))?;
    let raw_origin: String = row.get(7)?;
    let origin = match raw_origin.as_str() {
        "human" => MemoryOrigin::Human,
        "agent" => MemoryOrigin::Agent,
        other => return Err(conversion(7, format!("unknown memory origin {other:?}"))),
    };
    let raw_refs: String = row.get(8)?;
    let source_refs = serde_json::from_str(&raw_refs)
        .map_err(|error| conversion(8, format!("source_refs_json: {error}")))?;
    let revision: i64 = row.get(6)?;
    let writer_instance: Option<String> = row.get(9)?;
    let turn_seq: Option<i64> = row.get(13)?;
    let turn_ref = match (&writer_instance, turn_seq) {
        (Some(instance_id), Some(seq)) if seq >= 0 => Some(MemoryTurnRef {
            instance_id: instance_id.clone(),
            seq: seq as u64,
        }),
        _ => None,
    };
    Ok(MemoryEntry {
        lineage_id: row.get(0)?,
        slug: row.get(1)?,
        memory_type,
        title: row.get(3)?,
        description: row.get(4)?,
        body: row.get(5)?,
        revision: revision.max(0) as u64,
        origin,
        source_refs,
        writer_instance,
        turn_ref,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        archived_at: row.get(12)?,
    })
}

fn load_entry(
    conn: &Connection,
    lineage_id: &str,
    slug: &str,
) -> rusqlite::Result<Option<MemoryEntry>> {
    conn.query_row(
        &format!("SELECT {ENTRY_COLUMNS} FROM memory_entries WHERE lineage_id = ?1 AND slug = ?2"),
        params![lineage_id, slug],
        entry_from_row,
    )
    .optional()
}

fn refs_json(refs: &[MemorySourceRef]) -> Result<String, MemoryError> {
    serde_json::to_string(refs).map_err(|error| MemoryError::Hub(HubError::Store(error.into())))
}

/// What a write does to the lineage's set of slugs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteKind {
    /// Brings a slug into existence (create; consolidate into a new slug).
    Create,
    /// Changes or removes entries that already exist (update; delete;
    /// consolidate into one of its inputs).
    Modify,
}

/// Writer authorization inside the write transaction.
///
/// - An Agent must be the lineage's current, unfenced chapter, so the
///   lineage row must exist.
/// - The owner may create memory only for an existing lineage. The owner may
///   modify retained entries without one: retirement deletes the lineage row
///   but keeps (archives) its memory, and restored memory must stay
///   correctable and removable.
fn authorize_write(
    conn: &Connection,
    lineage_id: &str,
    writer: &MemoryWriter,
    kind: WriteKind,
) -> Result<(), MemoryError> {
    if *writer == MemoryWriter::Owner && kind == WriteKind::Modify {
        return Ok(());
    }
    let current: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT l.current_instance_id, i.fenced_at
               FROM lineages l LEFT JOIN instances i ON i.id = l.current_instance_id
              WHERE l.lineage_id = ?1",
            params![lineage_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((current_id, fenced_at)) = current else {
        return Err(MemoryError::NotFound(format!(
            "lineage {lineage_id} does not exist (memory belongs to a continuity lineage)"
        )));
    };
    match writer {
        MemoryWriter::Owner => Ok(()),
        MemoryWriter::Agent { instance_id } => {
            if instance_id == &current_id && fenced_at.is_none() {
                Ok(())
            } else {
                Err(MemoryError::Forbidden(format!(
                    "memory writes are accepted only from the lineage's current chapter \
                     ({current_id}); {instance_id} is not it"
                )))
            }
        }
    }
}

/// Refuse an Agent write that would create, replace, merge or remove the
/// owner-authored entry. The owner is unaffected: `about-me` is theirs to
/// write from the PWA, and a seat only reads it (D-060 §8.4).
fn check_owner_authored(writer: &MemoryWriter, slugs: &[&str]) -> Result<(), MemoryError> {
    if !matches!(writer, MemoryWriter::Agent { .. }) {
        return Ok(());
    }
    if slugs.contains(&OWNER_AUTHORED_SLUG) {
        return Err(MemoryError::Hub(
            HubError::ForbiddenReason(format!(
                "memory entry {OWNER_AUTHORED_SLUG:?} is owner-authored"
            ))
            .with_next(
                "owner-authored",
                NextStep::new(
                    action::FIX_REQUEST,
                    "about-me is written only by the owner from the PWA; do not retry \
                     — report what the owner should record",
                ),
            ),
        ));
    }
    Ok(())
}

/// Decode one `memory_write_log` row as `(row id, item)`; the id drives the
/// feed's cursor.
fn write_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, MemoryWriteItem)> {
    let raw_op: String = row.get(4)?;
    let op = match raw_op.as_str() {
        "write" => MemoryWriteOp::Write,
        "consolidate" => MemoryWriteOp::Consolidate,
        "delete" => MemoryWriteOp::Delete,
        other => return Err(conversion(4, format!("unknown memory write op {other:?}"))),
    };
    let revision: i64 = row.get(3)?;
    let turn_seq: Option<i64> = row.get(5)?;
    Ok((
        row.get(0)?,
        MemoryWriteItem {
            slug: row.get(1)?,
            title: row.get(2)?,
            revision: revision.max(0) as u64,
            op,
            turn_seq: turn_seq.map(|seq| seq.max(0) as u64),
            written_at: row.get(6)?,
        },
    ))
}

/// One index row: type, slug, title, description, archived.
type IndexRow = (MemoryType, String, String, String, bool);

/// A lineage's entries in index order (type rank, then slug): the live ones,
/// and the archived ones too when `include_archived`.
fn index_rows(
    conn: &Connection,
    lineage_id: &str,
    include_archived: bool,
) -> rusqlite::Result<Vec<IndexRow>> {
    let mut stmt = conn.prepare(
        "SELECT type, slug, title, description, archived_at IS NOT NULL
           FROM memory_entries
          WHERE lineage_id = ?1 AND (?2 OR archived_at IS NULL)",
    )?;
    let mut rows = stmt
        .query_map(params![lineage_id, include_archived], |row| {
            let raw: String = row.get(0)?;
            let memory_type = MemoryType::parse(&raw)
                .ok_or_else(|| conversion(0, format!("unknown memory type {raw:?}")))?;
            Ok((
                memory_type,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<IndexRow>>>()?;
    rows.sort_by(|a, b| a.0.rank().cmp(&b.0.rank()).then_with(|| a.1.cmp(&b.1)));
    Ok(rows)
}

fn render_index(
    conn: &Connection,
    lineage_id: &str,
    include_archived: bool,
) -> rusqlite::Result<MemoryIndex> {
    let rows = index_rows(conn, lineage_id, include_archived)?;
    let text: String = rows
        .iter()
        .map(|(memory_type, slug, title, description, archived)| {
            memory_index_line(*memory_type, slug, title, description, *archived)
        })
        .collect();
    let bytes = text.len() as u64;
    let warnings = if bytes > MEMORY_INDEX_SOFT_BYTES {
        vec![MemorySizeWarning {
            limit: MemoryLimit::IndexSoft,
            bytes,
            max_bytes: MEMORY_INDEX_SOFT_BYTES,
        }]
    } else {
        Vec::new()
    };
    Ok(MemoryIndex {
        lineage_id: lineage_id.to_string(),
        text,
        count: rows.len() as u64,
        bytes,
        warnings,
    })
}

fn write_result(
    conn: &Connection,
    entry: MemoryEntry,
    removed: Vec<String>,
) -> rusqlite::Result<MemoryWriteResult> {
    let entry_bytes = memory_entry_bytes(&entry.title, &entry.description, &entry.body);
    // The size that matters is the index a seat is given: live entries.
    let index = render_index(conn, &entry.lineage_id, false)?;
    let mut warnings = Vec::new();
    if entry_bytes > MEMORY_ENTRY_SOFT_BYTES {
        warnings.push(MemorySizeWarning {
            limit: MemoryLimit::EntrySoft,
            bytes: entry_bytes,
            max_bytes: MEMORY_ENTRY_SOFT_BYTES,
        });
    }
    warnings.extend(index.warnings);
    Ok(MemoryWriteResult {
        entry,
        removed,
        entry_bytes,
        index_bytes: index.bytes,
        warnings,
    })
}

#[allow(clippy::too_many_arguments)]
fn insert_entry(
    conn: &Connection,
    lineage_id: &str,
    writer: &MemoryWriter,
    input: &MemoryInput,
    revision: u64,
    turn_seq: Option<i64>,
    created_at: &str,
    now: &str,
) -> Result<(), MemoryError> {
    conn.execute(
        "INSERT INTO memory_entries
            (lineage_id, slug, type, title, description, body, revision, origin,
             source_refs_json, writer_instance, created_at, updated_at, turn_seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            lineage_id,
            input.slug,
            input.memory_type.as_str(),
            input.title,
            input.description,
            input.body,
            revision as i64,
            writer.origin_str(),
            refs_json(&input.source_refs)?,
            writer.instance(),
            created_at,
            now,
            turn_seq,
        ],
    )?;
    Ok(())
}

/// Journal seq of the writer chapter's most recent `turn_started` row at
/// write time. The Hub stamps it; it never comes from a request body. NULL for
/// an owner write and for a chapter that has not started a turn.
/// The writer chapter's most recent `turn_started` row.
///
/// Shaped as an ordered lookup, not `MAX(seq)`: SQLite walks the
/// `(instance_id, seq)` primary key backwards from the newest row and stops at
/// the first match, so the write path never reads the instance's whole
/// journal. The plan is asserted by
/// `the_turn_start_lookup_walks_the_journal_key`.
const TURN_STARTED_SQL: &str = "SELECT seq FROM journal
      WHERE instance_id = ?1
        AND json_extract(payload_json, '$.payload.type') = 'native'
        AND json_extract(payload_json, '$.payload.topic') = 'turn'
        AND json_extract(payload_json, '$.payload.nativeName') = 'turn_started'
      ORDER BY seq DESC LIMIT 1";

fn turn_seq_for(conn: &Connection, writer: &MemoryWriter) -> rusqlite::Result<Option<i64>> {
    let Some(instance_id) = writer.instance() else {
        return Ok(None);
    };
    conn.query_row(TURN_STARTED_SQL, params![instance_id], |row| row.get(0))
        .optional()
}

/// Append one `memory_write_log` row inside the write's own transaction, so a
/// commit either has both the entry change and its log row or neither. Log
/// rows are append-only and never reference the entry row.
#[allow(clippy::too_many_arguments)]
fn log_write(
    conn: &Connection,
    lineage_id: &str,
    slug: &str,
    revision: u64,
    title: &str,
    op: MemoryWriteOp,
    writer: &MemoryWriter,
    turn_seq: Option<i64>,
    written_at: &str,
) -> rusqlite::Result<()> {
    let op = match op {
        MemoryWriteOp::Write => "write",
        MemoryWriteOp::Consolidate => "consolidate",
        MemoryWriteOp::Delete => "delete",
    };
    conn.execute(
        "INSERT INTO memory_write_log
            (lineage_id, slug, revision, title, op, origin, writer_instance,
             turn_seq, written_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            lineage_id,
            slug,
            revision as i64,
            title,
            op,
            writer.origin_str(),
            writer.instance(),
            turn_seq,
            written_at,
        ],
    )?;
    Ok(())
}

fn require_entry(
    conn: &Connection,
    lineage_id: &str,
    slug: &str,
) -> Result<MemoryEntry, MemoryError> {
    load_entry(conn, lineage_id, slug)?.ok_or_else(|| {
        MemoryError::NotFound(format!("memory entry {slug:?} not found in {lineage_id}"))
    })
}

/// One past the highest revision `slug` ever had in this lineage, live or
/// deleted: a revision is never issued twice for a (lineage, slug).
fn next_revision(conn: &Connection, lineage_id: &str, slug: &str) -> rusqlite::Result<u64> {
    let high: i64 = conn.query_row(
        "SELECT MAX(
            COALESCE((SELECT revision FROM memory_slug_revisions
                       WHERE lineage_id = ?1 AND slug = ?2), 0),
            COALESCE((SELECT revision FROM memory_entries
                       WHERE lineage_id = ?1 AND slug = ?2), 0))",
        params![lineage_id, slug],
        |row| row.get(0),
    )?;
    Ok(high.max(0) as u64 + 1)
}

/// Raise the slug's high-water revision to at least `revision`.
fn record_revision(
    conn: &Connection,
    lineage_id: &str,
    slug: &str,
    revision: u64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO memory_slug_revisions (lineage_id, slug, revision) VALUES (?1, ?2, ?3)
         ON CONFLICT (lineage_id, slug)
         DO UPDATE SET revision = MAX(revision, excluded.revision)",
        params![lineage_id, slug, revision as i64],
    )?;
    Ok(())
}

fn check_revision(entry: &MemoryEntry, expected: u64) -> Result<(), MemoryError> {
    if entry.revision == expected {
        Ok(())
    } else {
        Err(MemoryError::RevisionConflict(vec![RevisionMismatch {
            slug: entry.slug.clone(),
            expected,
            current: entry.revision,
        }]))
    }
}

// ─── Transactions ──────────────────────────────────────────────────────────

fn create_tx(
    conn: &mut Connection,
    lineage_id: &str,
    writer: &MemoryWriter,
    input: &MemoryInput,
) -> Result<MemoryWriteResult, MemoryError> {
    let tx = immediate_tx(conn)?;
    authorize_write(&tx, lineage_id, writer, WriteKind::Create)?;
    check_owner_authored(writer, &[&input.slug])?;
    if let Some(existing) = load_entry(&tx, lineage_id, &input.slug)? {
        return Err(MemoryError::SlugExists {
            slug: existing.slug,
            revision: existing.revision,
            archived: existing.archived_at.is_some(),
        });
    }
    let now = now_rfc3339();
    let revision = next_revision(&tx, lineage_id, &input.slug)?;
    let turn_seq = turn_seq_for(&tx, writer)?;
    insert_entry(
        &tx, lineage_id, writer, input, revision, turn_seq, &now, &now,
    )?;
    record_revision(&tx, lineage_id, &input.slug, revision)?;
    log_write(
        &tx,
        lineage_id,
        &input.slug,
        revision,
        &input.title,
        MemoryWriteOp::Write,
        writer,
        turn_seq,
        &now,
    )?;
    let entry = require_entry(&tx, lineage_id, &input.slug)?;
    let result = write_result(&tx, entry, Vec::new())?;
    tx.commit()?;
    Ok(result)
}

fn update_tx(
    conn: &mut Connection,
    lineage_id: &str,
    slug: &str,
    writer: &MemoryWriter,
    patch: MemoryPatch,
    expected: u64,
) -> Result<MemoryWriteResult, MemoryError> {
    let tx = immediate_tx(conn)?;
    authorize_write(&tx, lineage_id, writer, WriteKind::Modify)?;
    check_owner_authored(writer, &[slug])?;
    let current = require_entry(&tx, lineage_id, slug)?;
    if current.archived_at.is_some() {
        return Err(MemoryError::Archived(slug.to_string()));
    }
    check_revision(&current, expected)?;
    let memory_type = patch.memory_type.unwrap_or(current.memory_type);
    let title = patch.title.unwrap_or(current.title);
    let description = patch.description.unwrap_or(current.description);
    let body = patch.body.unwrap_or(current.body);
    check_size(slug, &title, &description, &body)?;
    let refs = merge_refs(&current.source_refs, &patch.source_refs);
    let now = now_rfc3339();
    let revision = next_revision(&tx, lineage_id, slug)?;
    let turn_seq = turn_seq_for(&tx, writer)?;
    let changed = tx.execute(
        "UPDATE memory_entries
            SET type = ?3, title = ?4, description = ?5, body = ?6,
                revision = ?12, origin = ?7, source_refs_json = ?8,
                writer_instance = ?9, updated_at = ?10, turn_seq = ?13
          WHERE lineage_id = ?1 AND slug = ?2 AND revision = ?11",
        params![
            lineage_id,
            slug,
            memory_type.as_str(),
            title,
            description,
            body,
            writer.origin_str(),
            refs_json(&refs)?,
            writer.instance(),
            now,
            expected as i64,
            revision as i64,
            turn_seq,
        ],
    )?;
    if changed != 1 {
        return Err(MemoryError::Hub(HubError::Internal(
            "memory update matched no row after the revision check".into(),
        )));
    }
    record_revision(&tx, lineage_id, slug, revision)?;
    log_write(
        &tx,
        lineage_id,
        slug,
        revision,
        &title,
        MemoryWriteOp::Write,
        writer,
        turn_seq,
        &now,
    )?;
    let entry = require_entry(&tx, lineage_id, slug)?;
    let result = write_result(&tx, entry, Vec::new())?;
    tx.commit()?;
    Ok(result)
}

fn delete_tx(
    conn: &mut Connection,
    lineage_id: &str,
    slug: &str,
    writer: &MemoryWriter,
    expected: u64,
) -> Result<MemoryEntry, MemoryError> {
    let tx = immediate_tx(conn)?;
    authorize_write(&tx, lineage_id, writer, WriteKind::Modify)?;
    check_owner_authored(writer, &[slug])?;
    let current = require_entry(&tx, lineage_id, slug)?;
    check_revision(&current, expected)?;
    let turn_seq = turn_seq_for(&tx, writer)?;
    let now = now_rfc3339();
    // The slug's revisions continue past this one if it is ever recreated.
    record_revision(&tx, lineage_id, slug, current.revision)?;
    tx.execute(
        "DELETE FROM memory_entries WHERE lineage_id = ?1 AND slug = ?2 AND revision = ?3",
        params![lineage_id, slug, expected as i64],
    )?;
    // The delete keeps its place in the log: every turn that wrote memory
    // shows its row, including the turn that removed one.
    log_write(
        &tx,
        lineage_id,
        slug,
        current.revision,
        &current.title,
        MemoryWriteOp::Delete,
        writer,
        turn_seq,
        &now,
    )?;
    tx.commit()?;
    Ok(current)
}

fn consolidate_tx(
    conn: &mut Connection,
    lineage_id: &str,
    writer: &MemoryWriter,
    sources: &[(String, u64)],
    input: &MemoryInput,
) -> Result<MemoryWriteResult, MemoryError> {
    let tx = immediate_tx(conn)?;
    // Merging into one of the inputs rewrites existing memory; merging into
    // a new slug creates memory.
    let kind = if sources.iter().any(|(slug, _)| slug == &input.slug) {
        WriteKind::Modify
    } else {
        WriteKind::Create
    };
    authorize_write(&tx, lineage_id, writer, kind)?;
    // Merging away an input or producing the target are both writes; neither
    // may touch the owner-authored entry from a seat.
    let mut touched: Vec<&str> = sources.iter().map(|(slug, _)| slug.as_str()).collect();
    touched.push(input.slug.as_str());
    check_owner_authored(writer, &touched)?;
    let mut inputs = Vec::with_capacity(sources.len());
    let mut stale = Vec::new();
    for (slug, expected) in sources {
        let entry = require_entry(&tx, lineage_id, slug)?;
        if entry.archived_at.is_some() {
            return Err(MemoryError::Archived(slug.clone()));
        }
        if entry.revision != *expected {
            stale.push(RevisionMismatch {
                slug: slug.clone(),
                expected: *expected,
                current: entry.revision,
            });
        }
        inputs.push(entry);
    }
    if !stale.is_empty() {
        return Err(MemoryError::RevisionConflict(stale));
    }
    let reused = inputs.iter().find(|entry| entry.slug == input.slug);
    if reused.is_none()
        && let Some(existing) = load_entry(&tx, lineage_id, &input.slug)?
    {
        return Err(MemoryError::SlugExists {
            slug: existing.slug,
            revision: existing.revision,
            archived: existing.archived_at.is_some(),
        });
    }
    // The merged entry keeps every input's provenance.
    let mut refs = Vec::new();
    for entry in &inputs {
        refs = merge_refs(&refs, &entry.source_refs);
    }
    let merged = MemoryInput {
        source_refs: merge_refs(&refs, &input.source_refs),
        ..input.clone()
    };
    let created_at = match reused {
        Some(entry) => entry.created_at.clone(),
        None => now_rfc3339(),
    };
    let mut removed = Vec::new();
    let now = now_rfc3339();
    let turn_seq = turn_seq_for(&tx, writer)?;
    for entry in &inputs {
        // Removed inputs keep their high-water revision, like a delete.
        record_revision(&tx, lineage_id, &entry.slug, entry.revision)?;
        tx.execute(
            "DELETE FROM memory_entries WHERE lineage_id = ?1 AND slug = ?2",
            params![lineage_id, entry.slug],
        )?;
        // One row per input: the revision it consumed, under the title it had
        // when it was merged away.
        log_write(
            &tx,
            lineage_id,
            &entry.slug,
            entry.revision,
            &entry.title,
            MemoryWriteOp::Consolidate,
            writer,
            turn_seq,
            &now,
        )?;
        if entry.slug != input.slug {
            removed.push(entry.slug.clone());
        }
    }
    // A reused input continues its own sequence; a new target continues
    // after any revision the slug had before.
    let revision = next_revision(&tx, lineage_id, &input.slug)?;
    insert_entry(
        &tx,
        lineage_id,
        writer,
        &merged,
        revision,
        turn_seq,
        &created_at,
        &now,
    )?;
    record_revision(&tx, lineage_id, &input.slug, revision)?;
    // …plus the target itself, at the revision it produced.
    log_write(
        &tx,
        lineage_id,
        &input.slug,
        revision,
        &merged.title,
        MemoryWriteOp::Consolidate,
        writer,
        turn_seq,
        &now,
    )?;
    let entry = require_entry(&tx, lineage_id, &input.slug)?;
    let result = write_result(&tx, entry, removed)?;
    tx.commit()?;
    Ok(result)
}

fn restore_tx(
    conn: &mut Connection,
    lineage_id: &str,
    slugs: Option<&[String]>,
) -> Result<Vec<String>, MemoryError> {
    let tx = immediate_tx(conn)?;
    let targets: Vec<String> = match slugs {
        Some(slugs) => {
            for slug in slugs {
                require_entry(&tx, lineage_id, slug)?;
            }
            slugs.to_vec()
        }
        None => {
            let mut stmt = tx.prepare(
                "SELECT slug FROM memory_entries
                  WHERE lineage_id = ?1 AND archived_at IS NOT NULL ORDER BY slug",
            )?;
            stmt.query_map(params![lineage_id], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?
        }
    };
    let mut restored = Vec::new();
    for slug in targets {
        let changed = tx.execute(
            "UPDATE memory_entries SET archived_at = NULL
              WHERE lineage_id = ?1 AND slug = ?2 AND archived_at IS NOT NULL",
            params![lineage_id, slug],
        )?;
        if changed == 1 {
            restored.push(slug);
        }
    }
    tx.commit()?;
    restored.sort();
    restored.dedup();
    Ok(restored)
}

/// FTS5 query for `terms`: each term a quoted phrase (FTS syntax in user text
/// is data), implicitly ANDed.
fn fts_query(terms: &[&str]) -> String {
    terms
        .iter()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn like_pattern(term: &str) -> String {
    let mut out = String::with_capacity(term.len() + 2);
    out.push('%');
    for ch in term.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('%');
    out
}

/// Full-text hits, best first: the entry rows themselves, selected under the
/// lineage and archive predicates, with their bm25 score.
fn fts_hits(
    conn: &Connection,
    lineage_id: &str,
    terms: &[&str],
    limit: usize,
    include_archived: bool,
) -> rusqlite::Result<Vec<(MemoryEntry, f64)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ENTRY_COLUMNS_M}, bm25(memory_fts, 3.0, 2.0, 1.0) AS score
           FROM memory_fts JOIN memory_entries m ON m.id = memory_fts.rowid
          WHERE memory_fts MATCH ?1 AND m.lineage_id = ?2
            AND (?3 OR m.archived_at IS NULL)
          ORDER BY score, m.slug
          LIMIT ?4"
    ))?;
    stmt.query_map(
        params![fts_query(terms), lineage_id, include_archived, limit as i64],
        |row| Ok((entry_from_row(row)?, row.get(14)?)),
    )?
    .collect()
}

/// Substring hits: every term in the title, description or body. The entry
/// rows themselves, selected under the lineage and archive predicates.
/// Slugs in `exclude` (what FTS already returned) are skipped in SQL and at
/// most `remaining` rows are read, so the pass decodes only rows it returns.
fn like_hits(
    conn: &Connection,
    lineage_id: &str,
    terms: &[&str],
    include_archived: bool,
    exclude: &[String],
    remaining: usize,
) -> rusqlite::Result<Vec<MemoryEntry>> {
    let mut sql = format!(
        "SELECT {ENTRY_COLUMNS_M} FROM memory_entries m
          WHERE m.lineage_id = ?1 AND (?2 OR m.archived_at IS NULL)"
    );
    let mut values: Vec<rusqlite::types::Value> = vec![
        lineage_id.to_string().into(),
        rusqlite::types::Value::Integer(i64::from(include_archived)),
    ];
    for term in terms {
        values.push(like_pattern(term).into());
        let n = values.len();
        sql.push_str(&format!(
            " AND (m.title LIKE ?{n} ESCAPE '\\' OR m.description LIKE ?{n} ESCAPE '\\' \
               OR m.body LIKE ?{n} ESCAPE '\\')"
        ));
    }
    if !exclude.is_empty() {
        let first = values.len() + 1;
        let slots: Vec<String> = (first..first + exclude.len())
            .map(|n| format!("?{n}"))
            .collect();
        sql.push_str(&format!(" AND m.slug NOT IN ({})", slots.join(", ")));
        values.extend(exclude.iter().map(|slug| slug.clone().into()));
    }
    values.push(rusqlite::types::Value::Integer(remaining as i64));
    sql.push_str(&format!(
        " ORDER BY m.updated_at DESC, m.slug LIMIT ?{}",
        values.len()
    ));
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(params_from_iter(values), |row| {
        note_like_row_decoded();
        entry_from_row(row)
    })?
    .collect()
}

/// One search, both passes in ONE deferred read transaction: every row it
/// returns comes from the same snapshot and was selected with the lineage
/// and archive predicates. No row is ever re-read by id.
fn search_conn(
    conn: &Connection,
    lineage_id: &str,
    query: &str,
    limit: usize,
    include_archived: bool,
) -> rusqlite::Result<Vec<MemorySearchHit>> {
    // The term count was checked by `Store::memory_search`; every term is
    // evaluated.
    let terms: Vec<&str> = query.split_whitespace().collect();
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let tx = conn.unchecked_transaction()?;
    let mut hits = Vec::new();
    let mut seen = Vec::new();
    // 1. FTS5, bm25-ranked (title weighs most). A query the FTS grammar
    // rejects is not an error: the LIKE pass still answers it.
    match fts_hits(&tx, lineage_id, &terms, limit, include_archived) {
        Ok(rows) => {
            for (entry, score) in rows {
                seen.push(entry.slug.clone());
                hits.push(MemorySearchHit {
                    entry,
                    matched_by: MemoryMatch::Fts,
                    score: Some(score),
                });
            }
        }
        Err(error) => tracing::debug!(%error, "memory FTS query refused; LIKE only"),
    }
    search_hook(lineage_id);
    if hits.len() < limit {
        // 2. LIKE: what the tokenizer cannot see, bounded to the slots left.
        let remaining = limit - hits.len();
        for entry in like_hits(&tx, lineage_id, &terms, include_archived, &seen, remaining)? {
            hits.push(MemorySearchHit {
                entry,
                matched_by: MemoryMatch::Like,
                score: None,
            });
        }
    }
    tx.commit()?;
    Ok(hits)
}

// ─── Store API ─────────────────────────────────────────────────────────────

/// Run a memory write on the writer thread, keeping the refusal typed.
async fn write<T, F>(store: &Store, name: &'static str, f: F) -> Result<T, MemoryError>
where
    T: Send + 'static,
    F: FnOnce(&mut Connection) -> Result<T, MemoryError> + Send + 'static,
{
    store.run_named(name, move |conn| Ok(f(conn))).await?
}

impl Store {
    /// Create `input` in `lineage_id`; 409 when the slug exists.
    pub(crate) async fn memory_create(
        &self,
        lineage_id: String,
        writer: MemoryWriter,
        input: MemoryInput,
    ) -> Result<MemoryWriteResult, MemoryError> {
        let input = input.normalized()?;
        write(self, "memory_create", move |conn| {
            create_tx(conn, &lineage_id, &writer, &input)
        })
        .await
    }

    /// Revision-CAS update of one entry.
    pub(crate) async fn memory_update(
        &self,
        lineage_id: String,
        slug: String,
        writer: MemoryWriter,
        patch: MemoryPatch,
        expected_revision: u64,
    ) -> Result<MemoryWriteResult, MemoryError> {
        check_slug(&slug)?;
        let patch = patch.normalized()?;
        write(self, "memory_update", move |conn| {
            update_tx(conn, &lineage_id, &slug, &writer, patch, expected_revision)
        })
        .await
    }

    /// Revision-CAS delete of one entry; returns the entry as deleted.
    pub(crate) async fn memory_delete(
        &self,
        lineage_id: String,
        slug: String,
        writer: MemoryWriter,
        expected_revision: u64,
    ) -> Result<MemoryEntry, MemoryError> {
        check_slug(&slug)?;
        write(self, "memory_delete", move |conn| {
            delete_tx(conn, &lineage_id, &slug, &writer, expected_revision)
        })
        .await
    }

    /// Replace `sources` (slug, expectedRevision) with one entry, atomically.
    pub(crate) async fn memory_consolidate(
        &self,
        lineage_id: String,
        writer: MemoryWriter,
        sources: Vec<(String, u64)>,
        input: MemoryInput,
    ) -> Result<MemoryWriteResult, MemoryError> {
        if !(2..=MAX_CONSOLIDATE_SOURCES).contains(&sources.len()) {
            return Err(MemoryError::BadRequest(format!(
                "consolidate takes 2 to {MAX_CONSOLIDATE_SOURCES} source entries"
            )));
        }
        for (index, (slug, _)) in sources.iter().enumerate() {
            check_slug(slug)?;
            if sources[..index].iter().any(|(other, _)| other == slug) {
                return Err(MemoryError::BadRequest(format!(
                    "source {slug:?} is listed twice"
                )));
            }
        }
        let input = input.normalized()?;
        write(self, "memory_consolidate", move |conn| {
            consolidate_tx(conn, &lineage_id, &writer, &sources, &input)
        })
        .await
    }

    /// Owner restore of archived entries (all of the lineage's when `slugs`
    /// is `None`); returns the slugs restored.
    pub(crate) async fn memory_restore(
        &self,
        lineage_id: String,
        slugs: Option<Vec<String>>,
    ) -> Result<Vec<String>, MemoryError> {
        if let Some(slugs) = &slugs {
            for slug in slugs {
                check_slug(slug)?;
            }
        }
        write(self, "memory_restore", move |conn| {
            restore_tx(conn, &lineage_id, slugs.as_deref())
        })
        .await
    }

    /// One entry, archived or not.
    pub(crate) async fn memory_get(
        &self,
        lineage_id: String,
        slug: String,
    ) -> Result<Option<MemoryEntry>, StoreError> {
        self.read("memory_get", move |conn| {
            Ok(load_entry(conn, &lineage_id, &slug)?)
        })
        .await
    }

    /// Entries of a lineage in index order, optionally one type.
    pub(crate) async fn memory_list(
        &self,
        lineage_id: String,
        memory_type: Option<MemoryType>,
        include_archived: bool,
    ) -> Result<Vec<MemoryEntry>, StoreError> {
        self.read("memory_list", move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {ENTRY_COLUMNS} FROM memory_entries
                  WHERE lineage_id = ?1 AND (?2 OR archived_at IS NULL)
                    AND (?3 IS NULL OR type = ?3)"
            ))?;
            let mut entries = stmt
                .query_map(
                    params![
                        lineage_id,
                        include_archived,
                        memory_type.map(MemoryType::as_str)
                    ],
                    entry_from_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            entries.sort_by(|a, b| {
                a.memory_type
                    .rank()
                    .cmp(&b.memory_type.rank())
                    .then_with(|| a.slug.cmp(&b.slug))
            });
            Ok(entries)
        })
        .await
    }

    /// The Hub-rendered index of a lineage's live entries.
    pub(crate) async fn memory_index(
        &self,
        lineage_id: String,
        include_archived: bool,
    ) -> Result<MemoryIndex, StoreError> {
        self.read("memory_index", move |conn| {
            Ok(render_index(conn, &lineage_id, include_archived)?)
        })
        .await
    }

    /// FTS5 (bm25) hits first, then LIKE hits the tokenizer missed. Every
    /// whitespace-separated term must match, so a query with more terms
    /// than [`MAX_SEARCH_TERMS`] is refused rather than shortened.
    pub(crate) async fn memory_search(
        &self,
        lineage_id: String,
        query: String,
        limit: usize,
        include_archived: bool,
    ) -> Result<Vec<MemorySearchHit>, MemoryError> {
        let terms = query.split_whitespace().count();
        if terms > MAX_SEARCH_TERMS {
            return Err(MemoryError::BadRequest(format!(
                "search takes at most {MAX_SEARCH_TERMS} terms (every term must match); \
                 got {terms}"
            )));
        }
        Ok(self
            .read("memory_search", move |conn| {
                Ok(search_conn(
                    conn,
                    &lineage_id,
                    &query,
                    limit,
                    include_archived,
                )?)
            })
            .await?)
    }

    /// One page of this chapter's memory write log — the transcript's
    /// "Remembered: <title>" rows. Rows survive a later rewrite or delete of
    /// their entry.
    ///
    /// Pages are keyed by the log row id, not the turn seq: a single turn can
    /// log more than one page. Without a cursor the page is the NEWEST rows
    /// (a transcript needs its recent turns); with one, the rows after it.
    pub(crate) async fn memory_writes(
        &self,
        instance_id: String,
        after_id: Option<u64>,
    ) -> Result<MemoryWritePage, StoreError> {
        self.read("memory_writes", move |conn| {
            // Saturate: a legal u64 "after everything" cursor must mean the
            // empty tail, never fall back to "no cursor" and replay the log.
            let cursor = after_id.map(|id| i64::try_from(id).unwrap_or(i64::MAX));
            let rows = match cursor {
                Some(cursor) => conn
                    .prepare(
                        "SELECT id, slug, title, revision, op, turn_seq, written_at
                           FROM memory_write_log
                          WHERE writer_instance = ?1 AND id > ?2
                          ORDER BY id
                          LIMIT ?3",
                    )?
                    .query_map(
                        params![instance_id, cursor, MAX_MEMORY_WRITES as i64],
                        write_row,
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
                None => {
                    // Newest page: read backwards, then put it back in
                    // display order (the feed is oldest first).
                    let mut rows = conn
                        .prepare(
                            "SELECT id, slug, title, revision, op, turn_seq, written_at
                               FROM memory_write_log
                              WHERE writer_instance = ?1
                              ORDER BY id DESC
                              LIMIT ?2",
                        )?
                        .query_map(params![instance_id, MAX_MEMORY_WRITES as i64], write_row)?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    rows.reverse();
                    rows
                }
            };
            let next_after_id = rows
                .last()
                .map(|(id, _)| *id)
                .or(cursor)
                .unwrap_or(0)
                .max(0) as u64;
            let items = rows.into_iter().map(|(_, item)| item).collect();
            Ok(MemoryWritePage {
                items,
                next_after_id,
            })
        })
        .await
    }

    /// Every entry (any lineage, archived included) that names `instance_id`
    /// — and `seq`, when given — in its source refs. For journal redaction.
    pub(crate) async fn memory_derived_from(
        &self,
        instance_id: String,
        seq: Option<u64>,
    ) -> Result<Vec<MemoryEntry>, StoreError> {
        self.read("memory_derived_from", move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {ENTRY_COLUMNS} FROM memory_entries m
                  WHERE EXISTS (
                        SELECT 1 FROM json_each(m.source_refs_json) r
                         WHERE json_extract(r.value, '$.instanceId') = ?1
                           AND (?2 IS NULL OR json_extract(r.value, '$.seq') = ?2))
                  ORDER BY m.lineage_id, m.slug"
            ))?;
            let entries = stmt
                .query_map(params![instance_id, seq.map(|s| s as i64)], entry_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(entries)
        })
        .await
    }
}

// ─── HTTP ──────────────────────────────────────────────────────────────────

/// Memory routes. The Agent predicate for the lineage-scoped ones is
/// `agent_scope::memory_target`; `restore` and `derived` are owner-only.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/lineages/{id}/memory",
            get(list_entries).post(create_entry),
        )
        .route(
            "/v1/lineages/{id}/memory/entries/{slug}",
            get(get_entry).put(update_entry).delete(delete_entry),
        )
        .route("/v1/lineages/{id}/memory/index", get(get_index))
        .route("/v1/lineages/{id}/memory/search", get(search))
        .route("/v1/lineages/{id}/memory/consolidate", post(consolidate))
        .route("/v1/lineages/{id}/memory/restore", post(restore))
        .route("/v1/memory/derived", get(derived))
        // The transcript's write rows: one writer chapter's log, Human/Bot
        // callers only. Deliberately absent from `restrict_agent_routes`, so
        // an Agent credential is refused before the handler.
        .route("/v1/instances/{id}/memory-writes", get(memory_writes))
}

/// The caller at the memory boundary: the narrowed identity it acts as, after
/// refusing a Bot credential on how it AUTHENTICATED. `x-remuda-instance-id`
/// turns any unbound device into that chapter's Agent, so deciding on the
/// narrowed caller alone would let a Bot act as any chapter it can name.
async fn memory_device(state: &AppState, headers: &HeaderMap) -> Result<Device, HubError> {
    require_origin(headers, &state.config)?;
    let (authenticated, narrowed) = authenticated_caller(state, headers).await?;
    // A Bot relays other people's words; memory is the owner's and the
    // seat's only (D-060).
    if origin(&authenticated) == InputOrigin::Bot {
        return Err(HubError::Forbidden);
    }
    Ok(narrowed)
}

/// The authenticated memory caller of one lineage.
async fn memory_caller(
    state: &AppState,
    headers: &HeaderMap,
    lineage_id: &str,
) -> Result<MemoryWriter, HubError> {
    let device = memory_device(state, headers).await?;
    match origin(&device) {
        InputOrigin::Human => Ok(MemoryWriter::Owner),
        InputOrigin::Agent => {
            let bound = device.instance_id.ok_or(HubError::Forbidden)?;
            let own = state
                .store
                .lineage_id_for_instance(bound.clone())
                .await?
                .ok_or(HubError::Forbidden)?;
            if own != lineage_id {
                return Err(HubError::Forbidden);
            }
            Ok(MemoryWriter::Agent { instance_id: bound })
        }
        _ => Err(HubError::Forbidden),
    }
}

async fn require_owner(state: &AppState, headers: &HeaderMap) -> Result<(), HubError> {
    let device = memory_device(state, headers).await?;
    if origin(&device) == InputOrigin::Human {
        Ok(())
    } else {
        Err(HubError::Forbidden)
    }
}

fn parse_type(raw: &str) -> Result<MemoryType, MemoryError> {
    MemoryType::parse(raw).ok_or_else(|| {
        MemoryError::BadRequest(format!(
            "unknown memory type {raw:?}; use user, feedback, project or reference"
        ))
    })
}

fn required_revision(value: Option<u64>) -> Result<u64, MemoryError> {
    value.ok_or_else(|| {
        MemoryError::BadRequest("expectedRevision is required for an update or delete".into())
    })
}

/// A create body. `origin`, `writerInstanceId` and `turnRef` are Hub-stamped:
/// they are not fields here, so a body that names them is ignored (serde
/// skips unknown fields) and can never forge them.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EntryBody {
    slug: String,
    #[serde(rename = "type")]
    memory_type: String,
    title: String,
    description: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    source_refs: Vec<MemorySourceRef>,
}

impl EntryBody {
    fn into_input(self) -> Result<MemoryInput, MemoryError> {
        Ok(MemoryInput {
            memory_type: parse_type(&self.memory_type)?,
            slug: self.slug,
            title: self.title,
            description: self.description,
            body: self.body,
            source_refs: self.source_refs,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateBody {
    #[serde(default)]
    expected_revision: Option<u64>,
    #[serde(default, rename = "type")]
    memory_type: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    source_refs: Vec<MemorySourceRef>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteQuery {
    #[serde(default)]
    expected_revision: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConsolidateSource {
    slug: String,
    #[serde(default)]
    expected_revision: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConsolidateBody {
    sources: Vec<ConsolidateSource>,
    entry: EntryBody,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RestoreBody {
    #[serde(default)]
    slugs: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListQuery {
    #[serde(default, rename = "type")]
    memory_type: Option<String>,
    #[serde(default)]
    include_archived: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexQuery {
    #[serde(default)]
    include_archived: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    include_archived: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DerivedQuery {
    instance_id: String,
    #[serde(default)]
    seq: Option<u64>,
}

async fn list_entries(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(lineage_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, MemoryError> {
    memory_caller(&state, &headers, &lineage_id).await?;
    let memory_type = query.memory_type.as_deref().map(parse_type).transpose()?;
    let items = state
        .store
        .memory_list(lineage_id, memory_type, query.include_archived)
        .await?;
    Ok(Json(json!({ "items": items })))
}

async fn create_entry(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(lineage_id): Path<String>,
    Json(body): Json<EntryBody>,
) -> Result<Json<MemoryWriteResult>, MemoryError> {
    let writer = memory_caller(&state, &headers, &lineage_id).await?;
    let input = body.into_input()?;
    Ok(Json(
        state.store.memory_create(lineage_id, writer, input).await?,
    ))
}

async fn get_entry(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((lineage_id, slug)): Path<(String, String)>,
) -> Result<Json<MemoryEntry>, MemoryError> {
    memory_caller(&state, &headers, &lineage_id).await?;
    check_slug(&slug)?;
    let entry = state
        .store
        .memory_get(lineage_id.clone(), slug.clone())
        .await?
        .ok_or_else(|| {
            MemoryError::NotFound(format!("memory entry {slug:?} not found in {lineage_id}"))
        })?;
    Ok(Json(entry))
}

async fn update_entry(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((lineage_id, slug)): Path<(String, String)>,
    Json(body): Json<UpdateBody>,
) -> Result<Json<MemoryWriteResult>, MemoryError> {
    let writer = memory_caller(&state, &headers, &lineage_id).await?;
    let expected = required_revision(body.expected_revision)?;
    let patch = MemoryPatch {
        memory_type: body.memory_type.as_deref().map(parse_type).transpose()?,
        title: body.title,
        description: body.description,
        body: body.body,
        source_refs: body.source_refs,
    };
    Ok(Json(
        state
            .store
            .memory_update(lineage_id, slug, writer, patch, expected)
            .await?,
    ))
}

async fn delete_entry(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((lineage_id, slug)): Path<(String, String)>,
    Query(query): Query<DeleteQuery>,
) -> Result<Json<Value>, MemoryError> {
    let writer = memory_caller(&state, &headers, &lineage_id).await?;
    let expected = required_revision(query.expected_revision)?;
    let deleted = state
        .store
        .memory_delete(lineage_id, slug, writer, expected)
        .await?;
    Ok(Json(json!({ "deleted": deleted })))
}

async fn get_index(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(lineage_id): Path<String>,
    Query(query): Query<IndexQuery>,
) -> Result<Json<MemoryIndex>, MemoryError> {
    memory_caller(&state, &headers, &lineage_id).await?;
    Ok(Json(
        state
            .store
            .memory_index(lineage_id, query.include_archived)
            .await?,
    ))
}

async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(lineage_id): Path<String>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Value>, MemoryError> {
    memory_caller(&state, &headers, &lineage_id).await?;
    if query.q.trim().is_empty() {
        return Err(MemoryError::BadRequest("q must not be empty".into()));
    }
    let limit = query
        .limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT);
    let items = state
        .store
        .memory_search(lineage_id, query.q, limit, query.include_archived)
        .await?;
    Ok(Json(json!({ "items": items })))
}

async fn consolidate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(lineage_id): Path<String>,
    Json(body): Json<ConsolidateBody>,
) -> Result<Json<MemoryWriteResult>, MemoryError> {
    let writer = memory_caller(&state, &headers, &lineage_id).await?;
    let sources = body
        .sources
        .into_iter()
        .map(|source| {
            let expected = source.expected_revision.ok_or_else(|| {
                MemoryError::BadRequest(format!("source {:?} needs expectedRevision", source.slug))
            })?;
            Ok((source.slug, expected))
        })
        .collect::<Result<Vec<_>, MemoryError>>()?;
    let input = body.entry.into_input()?;
    Ok(Json(
        state
            .store
            .memory_consolidate(lineage_id, writer, sources, input)
            .await?,
    ))
}

async fn restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(lineage_id): Path<String>,
    Json(body): Json<RestoreBody>,
) -> Result<Json<Value>, MemoryError> {
    require_owner(&state, &headers).await?;
    let restored = state.store.memory_restore(lineage_id, body.slugs).await?;
    Ok(Json(json!({ "restored": restored })))
}

async fn derived(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DerivedQuery>,
) -> Result<Json<Value>, MemoryError> {
    require_owner(&state, &headers).await?;
    let items = state
        .store
        .memory_derived_from(query.instance_id, query.seq)
        .await?;
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MemoryWritesQuery {
    /// Return only log rows whose id is greater than this (the previous
    /// page's `nextAfterId`).
    #[serde(default)]
    after_id: Option<u64>,
}

/// `GET /v1/instances/{id}/memory-writes` — one page of a writer chapter's
/// memory write log. Without `afterId` the page is the newest rows; with it,
/// the rows after that row id. Rows are oldest first within the page.
///
/// Human and Bot callers only. The agent-route middleware refuses an Agent
/// credential (this path is on no agent allowlist), and the handler re-checks,
/// because memory belongs to the owner and the seat (D-060).
async fn memory_writes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(instance_id): Path<String>,
    Query(query): Query<MemoryWritesQuery>,
) -> Result<Json<MemoryWritePage>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = caller(&state, &headers).await?;
    if origin(&device) == InputOrigin::Agent {
        return Err(HubError::Forbidden);
    }
    if state
        .store
        .get_instance(instance_id.clone())
        .await
        .map_err(crate::http::map_store)?
        .is_none()
    {
        return Err(HubError::NotFound);
    }
    let page = state
        .store
        .memory_writes(instance_id, query.after_id)
        .await
        .map_err(crate::http::map_store)?;
    Ok(Json(page))
}

// ─── Test seam ─────────────────────────────────────────────────────────────
//
// Kept below the routes: tests/openapi.rs scans each source file only up to
// its first test-only attribute.

/// Test seam: runs on the reader thread after a search's full-text pass and
/// before its substring pass, with the searched lineage.
#[cfg(test)]
type SearchHook = std::sync::Arc<dyn Fn(&str) + Send + Sync>;
#[cfg(test)]
static SEARCH_HOOK: std::sync::Mutex<Option<SearchHook>> = std::sync::Mutex::new(None);

fn search_hook(lineage_id: &str) {
    #[cfg(test)]
    {
        let hook = SEARCH_HOOK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook(lineage_id);
        }
    }
    #[cfg(not(test))]
    let _ = lineage_id;
}

// Test seam: entry rows the LIKE pass decoded on this thread.
#[cfg(test)]
thread_local! {
    static LIKE_ROWS_DECODED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn note_like_row_decoded() {
    #[cfg(test)]
    LIKE_ROWS_DECODED.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{
        ContinuationResumeRequest, ContinuationResumeResult, InstanceDelegation, RestartPolicy,
    };

    struct Fixture {
        _dir: tempfile::TempDir,
        store: Store,
        host: String,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let (store, host) = crate::store_test_support::open_with_host(dir.path(), "memory")
            .await
            .unwrap();
        Fixture {
            _dir: dir,
            store,
            host,
        }
    }

    impl Fixture {
        /// A continuity lineage (restart policy) whose first chapter is current.
        async fn seat(&self) -> String {
            self.store
                .insert_instance_delegated(
                    self.host.clone(),
                    None,
                    "claude".into(),
                    "claude-sdk".into(),
                    None,
                    json!({"origin": "human"}),
                    InstanceDelegation {
                        restart: Some(RestartPolicy {
                            on_process_loss: true,
                            max_per_hour: 3,
                        }),
                        ..InstanceDelegation::default()
                    },
                    // Hub-internal seeding work, as main's own store-insert
                    // tests use: no Agent initiator, never fenced.
                    crate::agent_scope::CallerAuthority::internal(),
                )
                .await
                .unwrap()
                .instance_id
        }

        /// Continue `lineage` into a successor chapter; returns its id.
        async fn next_chapter(&self, lineage: &str, generation: i64) -> String {
            let current = self
                .store
                .get_lineage(lineage.into())
                .await
                .unwrap()
                .unwrap()
                .current_instance_id;
            match self
                .store
                .continuation_resume(ContinuationResumeRequest {
                    addressed_instance_id: current,
                    expected_generation: generation,
                    host_id: self.host.clone(),
                    spec: json!({"kind": "claude", "driver": "claude-sdk"}),
                    operation: "instance.create".into(),
                    prompt: None,
                    origin: "human".into(),
                    title: None,
                    driver: "claude-sdk".into(),
                })
                .await
                .unwrap()
            {
                ContinuationResumeResult::Resumed(resumed) => resumed.successor.instance_id,
                other => panic!("expected a successor, got {other:?}"),
            }
        }

        async fn end_all(&self, lineage: &str) {
            let lineage = lineage.to_string();
            self.store
                .run_named("test.end_all", move |conn| {
                    conn.execute(
                        "UPDATE instances SET lifecycle = 'exited' WHERE lineage_id = ?1",
                        params![lineage],
                    )?;
                    Ok(())
                })
                .await
                .unwrap();
        }
    }

    fn input(slug: &str, memory_type: MemoryType, body: &str) -> MemoryInput {
        MemoryInput {
            slug: slug.into(),
            memory_type,
            title: format!("title {slug}"),
            description: format!("description {slug}"),
            body: body.into(),
            source_refs: Vec::new(),
        }
    }

    fn agent(instance: &str) -> MemoryWriter {
        MemoryWriter::Agent {
            instance_id: instance.into(),
        }
    }

    fn source(instance: &str, seq: u64) -> MemorySourceRef {
        MemorySourceRef {
            instance_id: instance.into(),
            seq,
        }
    }

    /// The Node's `turn/turn_started` mirror, as `seen.rs`'s tests build it.
    fn turn_started() -> Value {
        json!({
            "kind": "lifecycle",
            "payload": {
                "type": "native", "topic": "turn", "nativeName": "turn_started",
                "nativeId": { "state": "known", "value": "sess-memlog" },
                "status": { "state": "known", "value": "working" },
                "relatedIds": {}, "dataRef": null, "severity": "info",
                "affectsCompletion": false
            }
        })
    }

    /// Append one turn start; returns its journal seq.
    async fn start_turn(fx: &Fixture, instance: &str) -> i64 {
        fx.store
            .append_journal(fx.host.clone(), instance.to_string(), None, turn_started())
            .await
            .expect("turn start")
            .record
            .seq
    }

    /// The lineage's `memory_write_log` rows, oldest first:
    /// `(slug, title, revision, op, writer_instance, turn_seq)`.
    async fn log_rows(
        fx: &Fixture,
        lineage: &str,
    ) -> Vec<(String, String, i64, String, Option<String>, Option<i64>)> {
        let lineage = lineage.to_string();
        fx.store
            .run_named("test.memory_write_log", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT slug, title, revision, op, writer_instance, turn_seq
                       FROM memory_write_log WHERE lineage_id = ?1 ORDER BY id",
                )?;
                let rows = stmt
                    .query_map(params![lineage], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, Option<i64>>(5)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn per_entry_revision_cas_for_create_update_and_delete() {
        let fx = fixture().await;
        let lineage = fx.seat().await;
        let writer = agent(&lineage);
        let created = fx
            .store
            .memory_create(
                lineage.clone(),
                writer.clone(),
                input("terse", MemoryType::Feedback, "Why: x"),
            )
            .await
            .unwrap();
        assert_eq!(created.entry.revision, 1);
        assert_eq!(created.entry.origin, MemoryOrigin::Agent);
        assert_eq!(
            created.entry.writer_instance.as_deref(),
            Some(lineage.as_str())
        );

        let again = fx
            .store
            .memory_create(
                lineage.clone(),
                MemoryWriter::Owner,
                input("terse", MemoryType::User, ""),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                again,
                MemoryError::SlugExists {
                    revision: 1,
                    archived: false,
                    ..
                }
            ),
            "{again:?}"
        );

        let patch = MemoryPatch {
            body: Some("Why: y".into()),
            ..MemoryPatch::default()
        };
        let updated = fx
            .store
            .memory_update(
                lineage.clone(),
                "terse".into(),
                MemoryWriter::Owner,
                patch.clone(),
                1,
            )
            .await
            .unwrap();
        assert_eq!(updated.entry.revision, 2);
        assert_eq!(updated.entry.origin, MemoryOrigin::Human);
        assert_eq!(updated.entry.writer_instance, None);
        assert_eq!(
            updated.entry.title, "title terse",
            "omitted fields are kept"
        );

        let stale = fx
            .store
            .memory_update(lineage.clone(), "terse".into(), writer.clone(), patch, 1)
            .await
            .unwrap_err();
        assert!(
            matches!(&stale, MemoryError::RevisionConflict(m) if m[0].current == 2 && m[0].expected == 1),
            "{stale:?}"
        );
        let stale_delete = fx
            .store
            .memory_delete(lineage.clone(), "terse".into(), writer.clone(), 1)
            .await
            .unwrap_err();
        assert!(matches!(stale_delete, MemoryError::RevisionConflict(_)));
        let deleted = fx
            .store
            .memory_delete(lineage.clone(), "terse".into(), writer, 2)
            .await
            .unwrap();
        assert_eq!(deleted.revision, 2);
        assert!(
            fx.store
                .memory_get(lineage, "terse".into())
                .await
                .unwrap()
                .is_none()
        );
    }

    /// r2 item 2: a slug's revisions never repeat, so a revision read before
    /// a delete and recreate cannot update, delete or consolidate the
    /// replacement (no ABA).
    #[tokio::test]
    async fn revisions_never_repeat_across_delete_and_recreate() {
        let fx = fixture().await;
        let lineage = fx.seat().await;
        let owner = MemoryWriter::Owner;
        let create = |slug: &str, body: &str| {
            fx.store.memory_create(
                lineage.clone(),
                owner.clone(),
                input(slug, MemoryType::User, body),
            )
        };
        // A client reads "pref" at revision 1 …
        create("pref", "original").await.unwrap();
        // … meanwhile it is deleted and recreated with other content.
        fx.store
            .memory_delete(lineage.clone(), "pref".into(), owner.clone(), 1)
            .await
            .unwrap();
        let recreated = create("pref", "replacement").await.unwrap();

        let stale_update = fx
            .store
            .memory_update(
                lineage.clone(),
                "pref".into(),
                owner.clone(),
                MemoryPatch {
                    body: Some("written against the original".into()),
                    ..MemoryPatch::default()
                },
                1,
            )
            .await;
        assert!(
            matches!(&stale_update, Err(MemoryError::RevisionConflict(m)) if m[0].expected == 1),
            "a stale update must not overwrite the replacement: {stale_update:?}"
        );
        let stale_delete = fx
            .store
            .memory_delete(lineage.clone(), "pref".into(), owner.clone(), 1)
            .await;
        assert!(
            matches!(&stale_delete, Err(MemoryError::RevisionConflict(_))),
            "a stale delete must not remove the replacement: {stale_delete:?}"
        );
        create("other", "x").await.unwrap();
        let stale_consolidate = fx
            .store
            .memory_consolidate(
                lineage.clone(),
                owner.clone(),
                vec![("pref".into(), 1), ("other".into(), 1)],
                input("merged", MemoryType::User, ""),
            )
            .await;
        assert!(
            matches!(&stale_consolidate, Err(MemoryError::RevisionConflict(m))
                if m.len() == 1 && m[0].slug == "pref"),
            "a stale consolidate input must be refused: {stale_consolidate:?}"
        );
        assert_eq!(
            recreated.entry.revision, 2,
            "the slug continues its sequence"
        );
        let kept = fx
            .store
            .memory_get(lineage.clone(), "pref".into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!((kept.revision, kept.body.as_str()), (2, "replacement"));

        // Consolidation targets too: a target slug that existed before
        // continues after its last revision, and removed inputs keep theirs.
        create("target", "t").await.unwrap();
        fx.store
            .memory_delete(lineage.clone(), "target".into(), owner.clone(), 1)
            .await
            .unwrap();
        let merged = fx
            .store
            .memory_consolidate(
                lineage.clone(),
                owner.clone(),
                vec![("pref".into(), 2), ("other".into(), 1)],
                input("target", MemoryType::User, "merged"),
            )
            .await
            .unwrap();
        assert_eq!(merged.entry.revision, 2);
        let again = create("other", "back").await.unwrap();
        assert_eq!(again.entry.revision, 2);
    }

    #[tokio::test]
    async fn only_the_current_chapter_writes_and_the_owner_writes_any_lineage() {
        let fx = fixture().await;
        let lineage = fx.seat().await;
        let other = fx.seat().await;
        fx.store
            .memory_create(
                lineage.clone(),
                agent(&lineage),
                input("a", MemoryType::Project, ""),
            )
            .await
            .unwrap();
        let second = fx.next_chapter(&lineage, 1).await;

        // The fenced first chapter is no longer the lineage's writer.
        let refused = fx
            .store
            .memory_create(
                lineage.clone(),
                agent(&lineage),
                input("b", MemoryType::Project, ""),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&refused, MemoryError::Forbidden(m) if m.contains("current chapter")),
            "{refused:?}"
        );
        // Neither is another lineage's chapter.
        let foreign = fx
            .store
            .memory_create(
                lineage.clone(),
                agent(&other),
                input("c", MemoryType::Project, ""),
            )
            .await
            .unwrap_err();
        assert!(matches!(foreign, MemoryError::Forbidden(_)), "{foreign:?}");

        let written = fx
            .store
            .memory_update(
                lineage.clone(),
                "a".into(),
                agent(&second),
                MemoryPatch::default(),
                1,
            )
            .await
            .unwrap();
        assert_eq!(
            written.entry.writer_instance.as_deref(),
            Some(second.as_str())
        );
        fx.store
            .memory_create(
                other.clone(),
                MemoryWriter::Owner,
                input("owner-note", MemoryType::User, ""),
            )
            .await
            .unwrap();
        let missing = fx
            .store
            .memory_create(
                "ins_nolineage".into(),
                MemoryWriter::Owner,
                input("x", MemoryType::User, ""),
            )
            .await
            .unwrap_err();
        assert!(matches!(missing, MemoryError::NotFound(_)), "{missing:?}");
    }

    #[tokio::test]
    async fn a_two_character_cjk_query_hits_through_like() {
        let fx = fixture().await;
        let lineage = fx.seat().await;
        let owner = MemoryWriter::Owner;
        fx.store
            .memory_create(
                lineage.clone(),
                owner.clone(),
                input("scope", MemoryType::Feedback, "先把边界条件写清楚再动手。"),
            )
            .await
            .unwrap();
        fx.store
            .memory_create(
                lineage.clone(),
                owner,
                input(
                    "replies",
                    MemoryType::User,
                    "The owner prefers terse replies.",
                ),
            )
            .await
            .unwrap();

        // unicode61 keeps the CJK run as one token, so FTS alone misses it.
        let fts_only = fx
            .store
            .read("test.fts", {
                let lineage = lineage.clone();
                move |conn| Ok(fts_hits(conn, &lineage, &["边界"], 10, false)?)
            })
            .await
            .unwrap();
        assert!(fts_only.is_empty(), "{fts_only:?}");

        let hits = fx
            .store
            .memory_search(lineage.clone(), "边界".into(), 10, false)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].entry.slug, "scope");
        assert_eq!(hits[0].matched_by, MemoryMatch::Like);

        // An English word is an FTS hit, bm25-scored; porter folds the plural.
        let hits = fx
            .store
            .memory_search(lineage.clone(), "reply".into(), 10, false)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].entry.slug, "replies");
        assert_eq!(hits[0].matched_by, MemoryMatch::Fts);
        assert!(hits[0].score.is_some());

        // FTS syntax in the query is data, not grammar.
        let hits = fx
            .store
            .memory_search(lineage, "\"terse AND (".into(), 10, false)
            .await
            .unwrap();
        assert!(hits.iter().all(|hit| hit.entry.slug == "replies"));
    }

    /// r2 item 1 / r3 item 2: both passes of a search answer from ONE
    /// snapshot. Writes landing between the passes (a LIKE-only match
    /// deleted, another changed so it no longer matches, a new LIKE-only
    /// match inserted, a row created in another lineage) are invisible to
    /// the second pass, which must still return the pre-write rows.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn search_answers_both_passes_from_one_snapshot() {
        let fx = fixture().await;
        let mine = fx.seat().await;
        let theirs = fx.seat().await;
        let owner = MemoryWriter::Owner;
        // "secret" is a whole token only in "pref" (an FTS hit); the others
        // contain it inside a longer token, so only LIKE can find them.
        for (slug, body) in [
            ("pref", "a secret preference"),
            ("like-gone", "the topsecretplan file"),
            ("like-changed", "a supersecretnote"),
        ] {
            fx.store
                .memory_create(
                    mine.clone(),
                    owner.clone(),
                    input(slug, MemoryType::User, body),
                )
                .await
                .unwrap();
        }

        let (reached_tx, reached_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let reached_tx = std::sync::Mutex::new(reached_tx);
        let go_rx = std::sync::Mutex::new(go_rx);
        let target = mine.clone();
        let hook: SearchHook = std::sync::Arc::new(move |lineage: &str| {
            if lineage != target {
                return;
            }
            let _ = reached_tx.lock().unwrap().send(());
            let _ = go_rx
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10));
        });
        *SEARCH_HOOK.lock().unwrap() = Some(hook);
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                *SEARCH_HOOK
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            }
        }
        let _clear = Clear;

        let store = fx.store.clone();
        let searched = mine.clone();
        let search = tokio::spawn(async move {
            store
                .memory_search(searched, "secret".into(), 10, false)
                .await
        });
        tokio::task::spawn_blocking(move || {
            reached_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("search reached the seam")
        })
        .await
        .unwrap();
        // Concurrent writes while the search is between its passes.
        fx.store
            .memory_delete(mine.clone(), "pref".into(), owner.clone(), 1)
            .await
            .unwrap();
        fx.store
            .memory_delete(mine.clone(), "like-gone".into(), owner.clone(), 1)
            .await
            .unwrap();
        fx.store
            .memory_update(
                mine.clone(),
                "like-changed".into(),
                owner.clone(),
                MemoryPatch {
                    body: Some("nothing to find here now".into()),
                    ..MemoryPatch::default()
                },
                1,
            )
            .await
            .unwrap();
        fx.store
            .memory_create(
                mine.clone(),
                owner.clone(),
                input("like-new", MemoryType::User, "an ultrasecretcode"),
            )
            .await
            .unwrap();
        fx.store
            .memory_create(
                theirs.clone(),
                owner,
                input("private", MemoryType::User, "another lineage's secret"),
            )
            .await
            .unwrap();
        go_tx.send(()).unwrap();
        let hits = search.await.unwrap().unwrap();
        let seen: Vec<(&str, &str, MemoryMatch)> = hits
            .iter()
            .map(|hit| {
                (
                    hit.entry.lineage_id.as_str(),
                    hit.entry.slug.as_str(),
                    hit.matched_by,
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                (mine.as_str(), "pref", MemoryMatch::Fts),
                (mine.as_str(), "like-changed", MemoryMatch::Like),
                (mine.as_str(), "like-gone", MemoryMatch::Like),
            ],
            "both passes must return the pre-write snapshot: {hits:?}"
        );
        assert_eq!(
            hits[1].entry.body, "a supersecretnote",
            "the snapshot's row"
        );
    }

    /// r3 item 3: the LIKE pass decodes only the rows it can still return:
    /// it skips what FTS already returned in SQL and stops at the remaining
    /// slots, however many large entries match.
    #[tokio::test]
    async fn the_like_pass_decodes_only_the_rows_it_returns() {
        let fx = fixture().await;
        let lineage = fx.seat().await;
        let owner = MemoryWriter::Owner;
        let filler = "x".repeat(6000);
        fx.store
            .memory_create(
                lineage.clone(),
                owner.clone(),
                input("fts-hit", MemoryType::User, "a secret"),
            )
            .await
            .unwrap();
        for n in 0..30 {
            fx.store
                .memory_create(
                    lineage.clone(),
                    owner.clone(),
                    input(
                        &format!("big-{n:02}"),
                        MemoryType::Reference,
                        &format!("topsecretplan 确认边界条件 {filler}"),
                    ),
                )
                .await
                .unwrap();
        }
        let run = |query: &'static str, limit: usize| {
            let lineage = lineage.clone();
            fx.store.read("test.like_rows", move |conn| {
                LIKE_ROWS_DECODED.with(|count| count.set(0));
                let hits = search_conn(conn, &lineage, query, limit, false)?;
                Ok((hits, LIKE_ROWS_DECODED.with(|count| count.get())))
            })
        };
        // FTS returns "fts-hit"; one slot is left for LIKE, which must not
        // decode "fts-hit" again or any of the other 29 matches.
        let (hits, decoded) = run("secret", 2).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            (hits[0].matched_by, hits[1].matched_by),
            (MemoryMatch::Fts, MemoryMatch::Like)
        );
        assert_eq!(decoded, 1, "LIKE decoded {decoded} rows for one slot");
        // A CJK substring is LIKE-only: limit 1 decodes one row of 31.
        let (hits, decoded) = run("边界", 1).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(decoded, 1, "LIKE decoded {decoded} rows for one slot");
        // FTS fills the limit: the LIKE pass decodes nothing.
        let (_, decoded) = run("secret", 1).await.unwrap();
        assert_eq!(decoded, 0);
    }

    /// r3 item 2: AUTOINCREMENT never reissues a freed row id, so an id seen
    /// before a delete can never name a row created after it (in any lineage).
    #[tokio::test]
    async fn freed_row_ids_are_never_reissued() {
        let fx = fixture().await;
        let mine = fx.seat().await;
        let theirs = fx.seat().await;
        let owner = MemoryWriter::Owner;
        let id_of = |lineage: String, slug: &'static str| {
            fx.store.read("test.id_of", move |conn| {
                Ok(conn.query_row(
                    "SELECT id FROM memory_entries WHERE lineage_id = ?1 AND slug = ?2",
                    params![lineage, slug],
                    |row| row.get::<_, i64>(0),
                )?)
            })
        };
        fx.store
            .memory_create(
                mine.clone(),
                owner.clone(),
                input("first", MemoryType::User, ""),
            )
            .await
            .unwrap();
        let freed = id_of(mine.clone(), "first").await.unwrap();
        fx.store
            .memory_delete(mine.clone(), "first".into(), owner.clone(), 1)
            .await
            .unwrap();
        // The table is now empty: a plain rowid would restart at the freed id.
        fx.store
            .memory_create(theirs.clone(), owner, input("second", MemoryType::User, ""))
            .await
            .unwrap();
        let next = id_of(theirs, "second").await.unwrap();
        assert!(next > freed, "freed id {freed} was reissued as {next}");
    }

    #[tokio::test]
    async fn sizes_warn_with_actual_numbers_and_reject_above_eight_kilobytes() {
        let fx = fixture().await;
        let lineage = fx.seat().await;
        let owner = MemoryWriter::Owner;
        // title "title big" (9) + description "description big" (15).
        let at_limit = "x".repeat(8192 - 9 - 15);
        let ok = fx
            .store
            .memory_create(
                lineage.clone(),
                owner.clone(),
                input("big", MemoryType::Reference, &at_limit),
            )
            .await
            .unwrap();
        assert_eq!(ok.entry_bytes, 8192);
        assert_eq!(
            ok.warnings,
            vec![MemorySizeWarning {
                limit: MemoryLimit::EntrySoft,
                bytes: 8192,
                max_bytes: 2048,
            }]
        );
        let over = format!("{at_limit}y");
        let refused = fx
            .store
            .memory_create(
                lineage.clone(),
                owner.clone(),
                // Same slug length as "big", so one more body byte is 8193.
                input("bix", MemoryType::Reference, &over),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(refused, MemoryError::TooLarge { bytes: 8193, .. }),
            "{refused:?}"
        );
        // An update that grows past the hard limit is refused too.
        let grow = fx
            .store
            .memory_update(
                lineage.clone(),
                "big".into(),
                owner.clone(),
                MemoryPatch {
                    body: Some(over),
                    ..MemoryPatch::default()
                },
                1,
            )
            .await
            .unwrap_err();
        assert!(matches!(grow, MemoryError::TooLarge { .. }), "{grow:?}");

        // The index warning reports the rendered size.
        let mut last = None;
        for n in 0..60 {
            let mut entry = input(&format!("n{n:02}"), MemoryType::Project, "");
            entry.description = "d".repeat(60);
            last = Some(
                fx.store
                    .memory_create(lineage.clone(), owner.clone(), entry)
                    .await
                    .unwrap(),
            );
        }
        let last = last.unwrap();
        let index = fx.store.memory_index(lineage, false).await.unwrap();
        assert!(index.bytes > 4096);
        assert_eq!(index.bytes, index.text.len() as u64);
        assert_eq!(last.index_bytes, index.bytes);
        assert!(last.warnings.contains(&MemorySizeWarning {
            limit: MemoryLimit::IndexSoft,
            bytes: index.bytes,
            max_bytes: 4096,
        }));
        assert_eq!(index.warnings.len(), 1);
    }

    #[tokio::test]
    async fn index_is_one_stable_line_per_live_entry() {
        let fx = fixture().await;
        let lineage = fx.seat().await;
        let owner = MemoryWriter::Owner;
        for (slug, memory_type) in [
            ("zeta", MemoryType::Reference),
            ("beta", MemoryType::User),
            ("alpha", MemoryType::Project),
            ("gamma", MemoryType::Feedback),
            ("alpha-user", MemoryType::User),
        ] {
            fx.store
                .memory_create(
                    lineage.clone(),
                    owner.clone(),
                    input(slug, memory_type, "b"),
                )
                .await
                .unwrap();
        }
        let index = fx.store.memory_index(lineage.clone(), false).await.unwrap();
        assert_eq!(
            index.text,
            "- [user] alpha-user: title alpha-user — description alpha-user\n\
             - [user] beta: title beta — description beta\n\
             - [feedback] gamma: title gamma — description gamma\n\
             - [project] alpha: title alpha — description alpha\n\
             - [reference] zeta: title zeta — description zeta\n"
        );
        assert_eq!(index.count, 5);
        assert!(index.warnings.is_empty());
        // r2 item 4: archived entries only on request, marked as such.
        let archive = lineage.clone();
        fx.store
            .run_named("test.archive", move |conn| {
                archive_lineage(conn, &archive, "2026-10-30T00:00:00.000Z")?;
                Ok(())
            })
            .await
            .unwrap();
        let live = fx.store.memory_index(lineage.clone(), false).await.unwrap();
        assert_eq!((live.count, live.text.as_str()), (0, ""));
        let all = fx.store.memory_index(lineage.clone(), true).await.unwrap();
        assert_eq!(all.count, 5);
        assert_eq!(
            all.text.lines().next(),
            Some("- [user] alpha-user: title alpha-user — description alpha-user (archived)")
        );
        assert_eq!(all.bytes, all.text.len() as u64);
        let listed: Vec<String> = fx
            .store
            .memory_list(lineage, None, true)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.slug)
            .collect();
        assert_eq!(listed, ["alpha-user", "beta", "gamma", "alpha", "zeta"]);
    }

    #[tokio::test]
    async fn consolidate_replaces_n_entries_atomically_under_cas() {
        let fx = fixture().await;
        let lineage = fx.seat().await;
        let writer = agent(&lineage);
        for (slug, seq) in [("tone-1", 4), ("tone-2", 9)] {
            let mut entry = input(slug, MemoryType::Feedback, slug);
            entry.source_refs = vec![source(&lineage, seq)];
            fx.store
                .memory_create(lineage.clone(), writer.clone(), entry)
                .await
                .unwrap();
        }
        fx.store
            .memory_update(
                lineage.clone(),
                "tone-2".into(),
                writer.clone(),
                MemoryPatch::default(),
                1,
            )
            .await
            .unwrap();

        // One stale input: nothing changes.
        let stale = fx
            .store
            .memory_consolidate(
                lineage.clone(),
                writer.clone(),
                vec![("tone-1".into(), 1), ("tone-2".into(), 1)],
                input("tone", MemoryType::Feedback, "merged"),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&stale, MemoryError::RevisionConflict(m)
                if m == &vec![RevisionMismatch { slug: "tone-2".into(), expected: 1, current: 2 }]),
            "{stale:?}"
        );
        let all = fx
            .store
            .memory_list(lineage.clone(), None, false)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
        assert!(
            fx.store
                .memory_get(lineage.clone(), "tone".into())
                .await
                .unwrap()
                .is_none()
        );

        let mut merged = input("tone", MemoryType::Feedback, "merged");
        merged.source_refs = vec![source(&lineage, 12), source(&lineage, 4)];
        let result = fx
            .store
            .memory_consolidate(
                lineage.clone(),
                writer.clone(),
                vec![("tone-1".into(), 1), ("tone-2".into(), 2)],
                merged,
            )
            .await
            .unwrap();
        assert_eq!(result.entry.revision, 1);
        assert_eq!(result.removed, ["tone-1", "tone-2"]);
        assert_eq!(
            result.entry.source_refs,
            vec![
                source(&lineage, 4),
                source(&lineage, 9),
                source(&lineage, 12)
            ]
        );
        let left: Vec<String> = fx
            .store
            .memory_list(lineage.clone(), None, true)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.slug)
            .collect();
        assert_eq!(left, ["tone"]);
        // FTS followed the deletes.
        assert!(
            fx.store
                .memory_search(lineage.clone(), "tone-1".into(), 10, true)
                .await
                .unwrap()
                .is_empty()
        );

        // Consolidating into one of the inputs keeps that slug's history.
        fx.store
            .memory_create(
                lineage.clone(),
                writer.clone(),
                input("extra", MemoryType::Feedback, "e"),
            )
            .await
            .unwrap();
        let kept = fx
            .store
            .memory_consolidate(
                lineage.clone(),
                writer.clone(),
                vec![("tone".into(), 1), ("extra".into(), 1)],
                input("tone", MemoryType::Feedback, "merged again"),
            )
            .await
            .unwrap();
        assert_eq!(kept.entry.revision, 2);
        assert_eq!(kept.removed, ["extra"]);

        // A target slug that exists outside the inputs is a conflict.
        fx.store
            .memory_create(
                lineage.clone(),
                writer.clone(),
                input("p", MemoryType::User, ""),
            )
            .await
            .unwrap();
        fx.store
            .memory_create(
                lineage.clone(),
                writer.clone(),
                input("q", MemoryType::User, ""),
            )
            .await
            .unwrap();
        let taken = fx
            .store
            .memory_consolidate(
                lineage,
                writer,
                vec![("p".into(), 1), ("q".into(), 1)],
                input("tone", MemoryType::User, ""),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(taken, MemoryError::SlugExists { revision: 2, .. }),
            "{taken:?}"
        );
    }

    #[tokio::test]
    async fn retiring_a_lineage_archives_and_the_owner_restores() {
        let fx = fixture().await;
        let lineage = fx.seat().await;
        let owner = MemoryWriter::Owner;
        for slug in ["one", "two"] {
            fx.store
                .memory_create(
                    lineage.clone(),
                    agent(&lineage),
                    input(slug, MemoryType::Project, "shared word"),
                )
                .await
                .unwrap();
        }
        let second = fx.next_chapter(&lineage, 1).await;
        fx.end_all(&lineage).await;
        fx.store.delete_instance(second).await.unwrap().unwrap();

        let archived = fx
            .store
            .memory_list(lineage.clone(), None, true)
            .await
            .unwrap();
        assert_eq!(archived.len(), 2, "archived, not deleted");
        assert!(archived.iter().all(|entry| entry.archived_at.is_some()));
        assert!(
            fx.store
                .memory_list(lineage.clone(), None, false)
                .await
                .unwrap()
                .is_empty()
        );
        let index = fx.store.memory_index(lineage.clone(), false).await.unwrap();
        assert_eq!((index.count, index.text.as_str()), (0, ""));
        assert!(
            fx.store
                .memory_search(lineage.clone(), "shared".into(), 10, false)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            fx.store
                .memory_search(lineage.clone(), "shared".into(), 10, true)
                .await
                .unwrap()
                .len(),
            2
        );
        // Archived memory waits for restore, and a retired lineage takes no
        // new memory.
        let write = fx
            .store
            .memory_update(
                lineage.clone(),
                "one".into(),
                owner.clone(),
                MemoryPatch::default(),
                1,
            )
            .await
            .unwrap_err();
        assert!(matches!(write, MemoryError::Archived(_)), "{write:?}");
        let create = fx
            .store
            .memory_create(
                lineage.clone(),
                owner.clone(),
                input("three", MemoryType::Project, ""),
            )
            .await
            .unwrap_err();
        assert!(matches!(create, MemoryError::NotFound(_)), "{create:?}");

        let restored = fx
            .store
            .memory_restore(lineage.clone(), Some(vec!["two".into()]))
            .await
            .unwrap();
        assert_eq!(restored, ["two"]);
        let restored = fx
            .store
            .memory_restore(lineage.clone(), None)
            .await
            .unwrap();
        assert_eq!(restored, ["one"]);
        let index = fx.store.memory_index(lineage.clone(), false).await.unwrap();
        assert_eq!(index.count, 2);
        let entry = fx
            .store
            .memory_get(lineage.clone(), "one".into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!((entry.revision, entry.archived_at), (1, None));
        let unknown = fx
            .store
            .memory_restore(lineage, Some(vec!["nope".into()]))
            .await
            .unwrap_err();
        assert!(matches!(unknown, MemoryError::NotFound(_)));
    }

    /// r2 item 3: retirement removes the lineage row, not the owner's
    /// authority over retained entries. Creating memory there stays refused.
    #[tokio::test]
    async fn the_owner_edits_and_removes_restored_memory_of_a_retired_lineage() {
        let fx = fixture().await;
        let lineage = fx.seat().await;
        let owner = MemoryWriter::Owner;
        for slug in ["one", "two", "three", "four"] {
            fx.store
                .memory_create(
                    lineage.clone(),
                    agent(&lineage),
                    input(slug, MemoryType::Project, slug),
                )
                .await
                .unwrap();
        }
        fx.end_all(&lineage).await;
        fx.store
            .delete_instance(lineage.clone())
            .await
            .unwrap()
            .unwrap();
        fx.store
            .memory_restore(lineage.clone(), None)
            .await
            .unwrap();

        let updated = fx
            .store
            .memory_update(
                lineage.clone(),
                "one".into(),
                owner.clone(),
                MemoryPatch {
                    body: Some("corrected".into()),
                    ..MemoryPatch::default()
                },
                1,
            )
            .await;
        assert!(updated.is_ok(), "owner update after restore: {updated:?}");
        let deleted = fx
            .store
            .memory_delete(lineage.clone(), "two".into(), owner.clone(), 1)
            .await;
        assert!(deleted.is_ok(), "owner delete after restore: {deleted:?}");
        // Consolidating into one of the inputs rewrites retained memory …
        let merged = fx
            .store
            .memory_consolidate(
                lineage.clone(),
                owner.clone(),
                vec![("one".into(), 2), ("three".into(), 1)],
                input("one", MemoryType::Project, "one and three"),
            )
            .await;
        assert!(
            merged.is_ok(),
            "owner consolidate into an input: {merged:?}"
        );
        // … but nothing new is created for a lineage that no longer exists.
        let new_target = fx
            .store
            .memory_consolidate(
                lineage.clone(),
                owner.clone(),
                vec![("one".into(), 3), ("four".into(), 1)],
                input("five", MemoryType::Project, ""),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(new_target, MemoryError::NotFound(_)),
            "{new_target:?}"
        );
        let create = fx
            .store
            .memory_create(
                lineage.clone(),
                owner,
                input("six", MemoryType::Project, ""),
            )
            .await
            .unwrap_err();
        assert!(matches!(create, MemoryError::NotFound(_)), "{create:?}");
        // An Agent still needs the lineage's current chapter.
        let agent_write = fx
            .store
            .memory_update(
                lineage.clone(),
                "four".into(),
                agent(&lineage),
                MemoryPatch::default(),
                1,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(agent_write, MemoryError::NotFound(_)),
            "{agent_write:?}"
        );
    }

    #[tokio::test]
    async fn updates_to_an_archived_entry_wait_for_restore() {
        let fx = fixture().await;
        let lineage = fx.seat().await;
        fx.store
            .memory_create(
                lineage.clone(),
                MemoryWriter::Owner,
                input("kept", MemoryType::User, ""),
            )
            .await
            .unwrap();
        let lineage_for_archive = lineage.clone();
        fx.store
            .run_named("test.archive", move |conn| {
                archive_lineage(conn, &lineage_for_archive, "2026-10-29T00:00:00.000Z")?;
                Ok(())
            })
            .await
            .unwrap();
        let refused = fx
            .store
            .memory_update(
                lineage,
                "kept".into(),
                MemoryWriter::Owner,
                MemoryPatch::default(),
                1,
            )
            .await
            .unwrap_err();
        assert!(matches!(refused, MemoryError::Archived(_)), "{refused:?}");
    }

    #[tokio::test]
    async fn source_refs_find_the_entries_derived_from_a_journal_row() {
        let fx = fixture().await;
        let lineage = fx.seat().await;
        let owner = MemoryWriter::Owner;
        for (slug, refs) in [
            ("a", vec![source("ins_w1", 5), source("ins_w2", 1)]),
            ("b", vec![source("ins_w1", 7)]),
            ("c", vec![source("ins_w2", 5)]),
            ("d", vec![]),
        ] {
            let mut entry = input(slug, MemoryType::Project, "");
            entry.source_refs = refs;
            fx.store
                .memory_create(lineage.clone(), owner.clone(), entry)
                .await
                .unwrap();
        }
        let slugs = |entries: Vec<MemoryEntry>| -> Vec<String> {
            entries.into_iter().map(|entry| entry.slug).collect()
        };
        assert_eq!(
            slugs(
                fx.store
                    .memory_derived_from("ins_w1".into(), Some(5))
                    .await
                    .unwrap()
            ),
            ["a"]
        );
        assert_eq!(
            slugs(
                fx.store
                    .memory_derived_from("ins_w1".into(), None)
                    .await
                    .unwrap()
            ),
            ["a", "b"]
        );
        // Archived entries are still found: redaction must reach them too.
        let lineage_for_archive = lineage.clone();
        fx.store
            .run_named("test.archive", move |conn| {
                archive_lineage(conn, &lineage_for_archive, "2026-10-29T00:00:00.000Z")?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            slugs(
                fx.store
                    .memory_derived_from("ins_w2".into(), Some(5))
                    .await
                    .unwrap()
            ),
            ["c"]
        );
        assert!(
            fx.store
                .memory_derived_from("ins_w3".into(), None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn inputs_are_validated_before_the_writer() {
        let fx = fixture().await;
        let lineage = fx.seat().await;
        let owner = MemoryWriter::Owner;
        let mut bad = input("Bad Slug", MemoryType::User, "");
        let error = fx
            .store
            .memory_create(lineage.clone(), owner.clone(), bad.clone())
            .await
            .unwrap_err();
        assert!(matches!(error, MemoryError::BadRequest(_)));
        bad.slug = "ok".into();
        bad.description = "two\nlines".into();
        let error = fx
            .store
            .memory_create(lineage.clone(), owner.clone(), bad.clone())
            .await
            .unwrap_err();
        assert!(matches!(error, MemoryError::BadRequest(m) if m.contains("one line")));
        bad.description = "  padded  ".into();
        let written = fx
            .store
            .memory_create(lineage.clone(), owner.clone(), bad)
            .await
            .unwrap();
        assert_eq!(written.entry.description, "padded");
        let error = fx
            .store
            .memory_consolidate(
                lineage,
                owner,
                vec![("ok".into(), 1)],
                input("x", MemoryType::User, ""),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, MemoryError::BadRequest(_)));
    }

    // ─── cb-memory-ui: the per-write log and the turn reference ───────────

    /// The write path's turn lookup must walk the journal's (instance_id, seq)
    /// key newest-first instead of scanning the instance's journal.
    #[tokio::test]
    async fn the_turn_start_lookup_walks_the_journal_key() {
        let fx = fixture().await;
        let _ = fx.seat().await;
        let plan: Vec<String> = fx
            .store
            .run_named("test.turn_start_plan", |conn| {
                let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {TURN_STARTED_SQL}"))?;
                let rows = stmt
                    .query_map(params!["ins_plan"], |row| row.get::<_, String>(3))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .unwrap();
        assert!(
            plan.iter()
                .any(|line| line.contains("SEARCH journal USING") && line.contains("instance_id=?")),
            "the lookup must seek the journal primary key: {plan:?}"
        );
        assert!(
            !plan
                .iter()
                .any(|line| line.trim_start().starts_with("SCAN journal")),
            "the lookup must not scan the journal: {plan:?}"
        );
    }

    #[tokio::test]
    async fn an_agent_write_mid_turn_records_the_turn_started_seq() {
        let fx = fixture().await;
        let lineage = fx.seat().await;
        let started = start_turn(&fx, &lineage).await;
        let written = fx
            .store
            .memory_create(
                lineage.clone(),
                agent(&lineage),
                input("pref", MemoryType::User, "body"),
            )
            .await
            .unwrap();
        assert_eq!(
            written.entry.turn_ref,
            Some(MemoryTurnRef {
                instance_id: lineage.clone(),
                seq: started as u64,
            })
        );
        assert_eq!(
            written.entry.writer_instance.as_deref(),
            Some(lineage.as_str())
        );

        let rows = log_rows(&fx, &lineage).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].0, "pref");
        assert_eq!(rows[0].1, "title pref");
        assert_eq!(rows[0].2, 1);
        assert_eq!(rows[0].3, "write");
        assert_eq!(rows[0].4.as_deref(), Some(lineage.as_str()));
        assert_eq!(rows[0].5, Some(started));

        // The chapter's feed returns it; afterSeq is exclusive on the turn.
        let feed = fx.store.memory_writes(lineage.clone(), None).await.unwrap();
        assert_eq!(feed.items.len(), 1);
        assert_eq!(feed.items[0].slug, "pref");
        assert_eq!(feed.items[0].turn_seq, Some(started as u64));
        assert_eq!(feed.items[0].op, MemoryWriteOp::Write);
        assert!(
            fx.store
                .memory_writes(lineage, Some(feed.next_after_id))
                .await
                .unwrap()
                .items
                .is_empty(),
            "afterId is exclusive"
        );
    }

    #[tokio::test]
    async fn an_owner_write_has_no_turn_seq() {
        let fx = fixture().await;
        let lineage = fx.seat().await;
        // Even mid-turn, an owner (PWA) write is not in any chapter's turn.
        start_turn(&fx, &lineage).await;
        let written = fx
            .store
            .memory_create(
                lineage.clone(),
                MemoryWriter::Owner,
                input("about-me", MemoryType::User, "I am"),
            )
            .await
            .unwrap();
        assert_eq!(written.entry.turn_ref, None);
        assert_eq!(written.entry.writer_instance, None);

        let rows = log_rows(&fx, &lineage).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].4, None, "owner writes have no writer instance");
        assert_eq!(rows[0].5, None, "and no turn seq");
        assert!(
            fx.store
                .memory_writes(lineage, None)
                .await
                .unwrap()
                .items
                .is_empty(),
            "an owner row is on no chapter's feed"
        );
    }

    #[tokio::test]
    async fn a_rewrite_keeps_both_log_rows() {
        let fx = fixture().await;
        let lineage = fx.seat().await;
        let started = start_turn(&fx, &lineage).await;
        fx.store
            .memory_create(
                lineage.clone(),
                agent(&lineage),
                input("pref", MemoryType::User, "first"),
            )
            .await
            .unwrap();
        let updated = fx
            .store
            .memory_update(
                lineage.clone(),
                "pref".into(),
                agent(&lineage),
                MemoryPatch {
                    body: Some("second".into()),
                    ..MemoryPatch::default()
                },
                1,
            )
            .await
            .unwrap();
        assert_eq!(updated.entry.revision, 2);

        let rows = log_rows(&fx, &lineage).await;
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!((rows[0].0.as_str(), rows[0].2), ("pref", 1));
        assert_eq!((rows[1].0.as_str(), rows[1].2), ("pref", 2));
        assert_eq!(rows[1].5, Some(started));
        let feed = fx.store.memory_writes(lineage, None).await.unwrap();
        assert_eq!(
            feed.items
                .iter()
                .map(|row| row.revision)
                .collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[tokio::test]
    async fn consolidate_logs_one_row_per_input_plus_the_target() {
        let fx = fixture().await;
        let lineage = fx.seat().await;
        let started = start_turn(&fx, &lineage).await;
        for slug in ["a", "b"] {
            fx.store
                .memory_create(
                    lineage.clone(),
                    agent(&lineage),
                    input(slug, MemoryType::User, ""),
                )
                .await
                .unwrap();
        }
        let merged = fx
            .store
            .memory_consolidate(
                lineage.clone(),
                agent(&lineage),
                vec![("a".into(), 1), ("b".into(), 1)],
                input("merged", MemoryType::User, "one"),
            )
            .await
            .unwrap();
        assert_eq!(merged.entry.revision, 1);

        let rows = log_rows(&fx, &lineage).await;
        // Two creates, then the two inputs and the target.
        assert_eq!(rows.len(), 5, "{rows:?}");
        let consolidations: Vec<(&str, i64)> = rows
            .iter()
            .filter(|row| row.3 == "consolidate")
            .map(|row| (row.0.as_str(), row.2))
            .collect();
        assert_eq!(consolidations, [("a", 1), ("b", 1), ("merged", 1)]);
        assert!(rows.iter().all(|row| row.5 == Some(started)));
    }

    #[tokio::test]
    async fn retiring_a_lineage_leaves_its_write_log_intact() {
        let fx = fixture().await;
        let lineage = fx.seat().await;
        start_turn(&fx, &lineage).await;
        fx.store
            .memory_create(
                lineage.clone(),
                agent(&lineage),
                input("pref", MemoryType::User, ""),
            )
            .await
            .unwrap();
        let archived = lineage.clone();
        fx.store
            .run_named("test.archive", move |conn| {
                archive_lineage(conn, &archived, "2026-10-30T00:00:00.000Z")?;
                Ok(())
            })
            .await
            .unwrap();
        // Retirement archives the entry but never touches the log.
        assert!(
            fx.store
                .memory_get(lineage.clone(), "pref".into())
                .await
                .unwrap()
                .unwrap()
                .archived_at
                .is_some()
        );
        assert_eq!(
            fx.store
                .memory_writes(lineage, None)
                .await
                .unwrap()
                .items
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn an_agent_cannot_write_the_owner_authored_entry() {
        let fx = fixture().await;
        let lineage = fx.seat().await;
        let writer = agent(&lineage);
        let owner_authored = |error: &MemoryError| {
            matches!(
                error,
                MemoryError::Hub(HubError::Actionable { reason, .. })
                    if *reason == "owner-authored"
            )
        };
        // Create.
        let error = fx
            .store
            .memory_create(
                lineage.clone(),
                writer.clone(),
                input(OWNER_AUTHORED_SLUG, MemoryType::User, "mine now"),
            )
            .await
            .unwrap_err();
        assert!(owner_authored(&error), "{error:?}");
        // The owner writes it, and a seat can read it.
        fx.store
            .memory_create(
                lineage.clone(),
                MemoryWriter::Owner,
                input(OWNER_AUTHORED_SLUG, MemoryType::User, "I am the owner"),
            )
            .await
            .unwrap();
        assert_eq!(
            fx.store
                .memory_get(lineage.clone(), OWNER_AUTHORED_SLUG.into())
                .await
                .unwrap()
                .unwrap()
                .body,
            "I am the owner"
        );
        // Update and delete.
        let patch = MemoryPatch {
            body: Some("from the seat".into()),
            ..MemoryPatch::default()
        };
        let error = fx
            .store
            .memory_update(
                lineage.clone(),
                OWNER_AUTHORED_SLUG.into(),
                writer.clone(),
                patch.clone(),
                1,
            )
            .await
            .unwrap_err();
        assert!(owner_authored(&error), "{error:?}");
        let error = fx
            .store
            .memory_delete(
                lineage.clone(),
                OWNER_AUTHORED_SLUG.into(),
                writer.clone(),
                1,
            )
            .await
            .unwrap_err();
        assert!(owner_authored(&error), "{error:?}");
        // Consolidate: producing the slug, and consuming it.
        for slug in ["other", "other2"] {
            fx.store
                .memory_create(
                    lineage.clone(),
                    MemoryWriter::Owner,
                    input(slug, MemoryType::Project, ""),
                )
                .await
                .unwrap();
        }
        let error = fx
            .store
            .memory_consolidate(
                lineage.clone(),
                writer.clone(),
                vec![("other".into(), 1), ("other2".into(), 1)],
                input(OWNER_AUTHORED_SLUG, MemoryType::User, "merged in"),
            )
            .await
            .unwrap_err();
        assert!(owner_authored(&error), "{error:?}");
        let error = fx
            .store
            .memory_consolidate(
                lineage.clone(),
                writer.clone(),
                vec![("other".into(), 1), (OWNER_AUTHORED_SLUG.into(), 1)],
                input("merged", MemoryType::User, "merged"),
            )
            .await
            .unwrap_err();
        assert!(owner_authored(&error), "{error:?}");

        // Nothing above changed the entry, and the owner still writes it.
        let kept = fx
            .store
            .memory_get(lineage.clone(), OWNER_AUTHORED_SLUG.into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(kept.revision, 1);
        assert_eq!(kept.body, "I am the owner");
        let updated = fx
            .store
            .memory_update(
                lineage,
                OWNER_AUTHORED_SLUG.into(),
                MemoryWriter::Owner,
                patch,
                1,
            )
            .await
            .unwrap();
        assert_eq!(updated.entry.revision, 2);
        assert_eq!(updated.entry.body, "from the seat");
        assert_eq!(updated.entry.origin, MemoryOrigin::Human);
    }

    /// The rung runs on a POPULATED version-5 database (the seen rung's top):
    /// the table and the `turn_seq` column appear, old entries read as having
    /// no turn, and the next write logs.
    #[tokio::test]
    async fn the_write_log_rung_upgrades_a_populated_version_five_database() {
        use crate::store::{SCHEMA_USER_VERSION, SEEN_SCHEMA_RUNG};
        let dir = tempfile::tempdir().unwrap();
        let (store, host) = crate::store_test_support::open_with_host(dir.path(), "memlog-upgrade")
            .await
            .unwrap();
        let instance = store
            .insert_instance_delegated(
                host.clone(),
                None,
                "claude".into(),
                "claude-sdk".into(),
                None,
                json!({"origin": "human"}),
                InstanceDelegation {
                    restart: Some(RestartPolicy {
                        on_process_loss: true,
                        max_per_hour: 3,
                    }),
                    ..InstanceDelegation::default()
                },
                crate::agent_scope::CallerAuthority::internal(),
            )
            .await
            .unwrap()
            .instance_id;
        store
            .append_journal(host, instance.clone(), None, turn_started())
            .await
            .unwrap();
        store
            .memory_create(
                instance.clone(),
                agent(&instance),
                input("kept", MemoryType::Project, "written before the upgrade"),
            )
            .await
            .unwrap();
        store.close().await;

        // Rewind to the exact pre-rung shape: no log, no turn_seq column,
        // marker at the seen rung. The entry row is populated.
        let db = rusqlite::Connection::open(dir.path().join("hub.sqlite")).unwrap();
        db.execute_batch(&format!(
            "DROP TABLE memory_write_log;
             ALTER TABLE memory_entries DROP COLUMN turn_seq;
             PRAGMA user_version = {SEEN_SCHEMA_RUNG};"
        ))
        .unwrap();
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            SEEN_SCHEMA_RUNG
        );
        drop(db);

        let store = Store::open(dir.path()).unwrap();
        let version: i64 = {
            let db = rusqlite::Connection::open(dir.path().join("hub.sqlite")).unwrap();
            db.query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(version, SCHEMA_USER_VERSION);
        assert!(
            crate::store::column_exists(
                &rusqlite::Connection::open(dir.path().join("hub.sqlite")).unwrap(),
                "memory_entries",
                "turn_seq"
            )
            .unwrap()
        );
        // The pre-upgrade entry survives, with no turn reference.
        let entry = store
            .memory_get(instance.clone(), "kept".into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.turn_ref, None);
        assert_eq!(entry.body, "written before the upgrade");
        // The next write logs and carries the turn.
        let written = store
            .memory_update(
                instance.clone(),
                "kept".into(),
                agent(&instance),
                MemoryPatch {
                    body: Some("after the upgrade".into()),
                    ..MemoryPatch::default()
                },
                1,
            )
            .await
            .unwrap();
        assert!(written.entry.turn_ref.is_some());
        assert_eq!(
            store
                .memory_writes(instance, None)
                .await
                .unwrap()
                .items
                .len(),
            1
        );
        store.close().await;

        // Re-opening at the top rung does not re-run it.
        let store = Store::open(dir.path()).unwrap();
        let version: i64 = {
            let db = rusqlite::Connection::open(dir.path().join("hub.sqlite")).unwrap();
            db.query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(version, SCHEMA_USER_VERSION);
        store.close().await;
    }
}
