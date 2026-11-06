//! cb-search: `GET /v1/search`, FTS5 over journal text, scoped exactly like
//! journal reads (protocol.md §8.4, main-agent.md §12.7).
//!
//! - **What is indexed.** The text of every journal row that carries it:
//!   user and assistant message text, tool names and inputs, and final tool
//!   results. A node's rows fold into documents ([`step`]): `open` plus its
//!   `append` deltas plus `close` are one document (streaming carriers send
//!   most of a message as deltas), a tool call's complete input replaces its
//!   streamed input, and a `replace` with different text starts a second
//!   document so the first text stays findable. A node is keyed by
//!   `(instance, kind, node id)`: a call and its result share a node id on
//!   some carriers and never evict each other. Inputs and results are
//!   bounded ([`TOOL_TEXT_BYTES`]), messages too ([`MESSAGE_TEXT_BYTES`]).
//!   The indexed text is the redacted concatenation
//!   ([`crate::digest::redact_secrets`] over the whole document, so a
//!   credential split across two deltas is still caught): a credential shape
//!   is never in the FTS table, so it cannot be found, and every snippet is
//!   cut from the redacted text. The raw concatenation is kept beside it
//!   (the journal's own text, dropped with its rows), never searched or
//!   returned.
//! - **Scope is the journal read predicate.** A hit is returned only when the
//!   caller may read that instance's journal: an Agent reads its own
//!   lineage's chapters and the instances whose parent is one of them
//!   ([`crate::store::lineage_owns_conn`]'s edge); Human and Bot devices read
//!   every instance (`require_instance_read`). The predicate is part of the
//!   query that ranks and limits, never a filter over a page already cut, so
//!   foreign hits can neither push the caller's own out nor be counted.
//! - **The migration adds no rows.** `user_version` 7 creates the tables and
//!   triggers only. A background job ([`spawn_backfill`]) indexes the
//!   existing journal in primary-key order, [`BACKFILL_BATCH_ROWS`] rows per
//!   writer job, and records its high-water mark in the same transaction, so
//!   a crash or restart resumes where it stopped. Rows appended after the
//!   migration are indexed in their append transaction. Both paths call
//!   [`index_event`], and a row that arrives below rows already folded
//!   rebuilds its node, so the documents never depend on which path met a
//!   row first.
//! - **Retention.** A trigger on `journal` deletes a row's record and every
//!   document whose rows include it, in the same transaction as the row, so
//!   the index never holds text the journal no longer holds. The index lives in `hub.sqlite`, which
//!   `scripts/hub-backup.sh` copies whole.
//! - **Logs** carry counts and timings, never the query or a snippet.

use std::time::{Duration, Instant};

use crate::AppState;
use crate::agent_scope::{caller, origin};
use crate::config::now_rfc3339;
use crate::error::HubError;
use crate::store::{Store, StoreError, immediate_tx, journal_event_kind, lineage_id_of};
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use remuda_protocol::{
    InputOrigin, JOURNAL_SEARCH_DEFAULT_LIMIT, JOURNAL_SEARCH_MAX_LIMIT,
    JOURNAL_SEARCH_MAX_QUERY_CHARS, JOURNAL_SEARCH_MAX_TERMS, JOURNAL_SEARCH_SNIPPET_CHARS,
    JournalSearchBackfillState, JournalSearchHit, JournalSearchKind, JournalSearchMatch,
    JournalSearchResult, JournalSearchStatus, U64,
};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::Deserialize;
use serde_json::Value;

/// The `PRAGMA user_version` step that creates the journal search tables.
pub(crate) const SEARCH_SCHEMA_VERSION: i64 = 7;

/// Message text kept per row, in bytes.
const MESSAGE_TEXT_BYTES: usize = 32 * 1024;
/// Tool input or result text kept per row, in bytes: they are the bulk.
const TOOL_TEXT_BYTES: usize = 2 * 1024;
/// Raw text read past a cap before redaction, so a credential the cap would
/// cut in half is still whole when the redactor sees it.
const REDACT_SLACK_BYTES: usize = 4 * 1024;
/// Journal rows read per backfill writer job.
pub(crate) const BACKFILL_BATCH_ROWS: usize = 2000;
/// Pause between backfill batches, so appends interleave on the writer.
const BACKFILL_PAUSE: Duration = Duration::from_millis(20);
/// Pause after a failed batch before it is retried.
const BACKFILL_RETRY: Duration = Duration::from_secs(5);
/// Wall-clock budget of one query (both passes).
pub(crate) const QUERY_BUDGET: Duration = Duration::from_millis(1500);
/// Tokens of context `snippet()` returns around a full-text match.
const SNIPPET_TOKENS: i64 = 32;
/// Characters of context kept before a substring match.
const SNIPPET_LEAD_CHARS: usize = 60;

// ─── Migration ─────────────────────────────────────────────────────────────

/// `user_version` 7: the index documents, their FTS5 table and its sync
/// triggers, the per-row record, the journal purge trigger and the
/// backfill's high-water mark. No rows.
///
/// - `journal_search_rows` records every journal row that carries
///   searchable text, keyed like the journal, with the node it belongs to.
///   It makes indexing idempotent and lets a node be rebuilt from its rows.
/// - `journal_search_docs` holds one document per run of a node's rows
///   ([`step`]): the rows `first_seq..=seq` of `(instance_id, kind,
///   node_id)`. `raw` is the concatenated journal text, never searched or
///   returned; `text` is its redacted, bounded form, the only column the FTS
///   table indexes. `id` is an INTEGER PRIMARY KEY (the FTS rowid survives
///   VACUUM) and AUTOINCREMENT (an id is never reused).
/// - Deleting a journal row deletes its record and every document whose
///   row range covers it, in the same transaction.
pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS journal_search_rows (
            instance_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            kind TEXT NOT NULL,
            node_id TEXT NOT NULL,
            PRIMARY KEY (instance_id, seq)
        ) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS journal_search_rows_node
            ON journal_search_rows (instance_id, kind, node_id, seq);
        CREATE TABLE IF NOT EXISTS journal_search_docs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            instance_id TEXT NOT NULL,
            kind TEXT NOT NULL,
            node_id TEXT NOT NULL,
            first_seq INTEGER NOT NULL,
            seq INTEGER NOT NULL,
            role TEXT,
            observed_at TEXT NOT NULL,
            block INTEGER,
            head TEXT NOT NULL,
            raw TEXT NOT NULL,
            indexed_len INTEGER NOT NULL,
            text TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS journal_search_docs_node
            ON journal_search_docs (instance_id, kind, node_id, seq);
        CREATE VIRTUAL TABLE IF NOT EXISTS journal_fts USING fts5(
            text,
            content = 'journal_search_docs', content_rowid = 'id',
            tokenize = 'porter unicode61'
        );
        CREATE TRIGGER IF NOT EXISTS journal_search_docs_fts_insert
        AFTER INSERT ON journal_search_docs BEGIN
            INSERT INTO journal_fts (rowid, text) VALUES (new.id, new.text);
        END;
        CREATE TRIGGER IF NOT EXISTS journal_search_docs_fts_delete
        AFTER DELETE ON journal_search_docs BEGIN
            INSERT INTO journal_fts (journal_fts, rowid, text)
            VALUES ('delete', old.id, old.text);
        END;
        CREATE TRIGGER IF NOT EXISTS journal_search_docs_fts_update
        AFTER UPDATE OF text ON journal_search_docs BEGIN
            INSERT INTO journal_fts (journal_fts, rowid, text)
            VALUES ('delete', old.id, old.text);
            INSERT INTO journal_fts (rowid, text) VALUES (new.id, new.text);
        END;
        CREATE TRIGGER IF NOT EXISTS journal_search_purge
        AFTER DELETE ON journal BEGIN
            DELETE FROM journal_search_rows
             WHERE instance_id = old.instance_id AND seq = old.seq;
            DELETE FROM journal_search_docs
             WHERE instance_id = old.instance_id
               AND old.seq BETWEEN first_seq AND seq;
        END;
        CREATE TABLE IF NOT EXISTS journal_search_backfill (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            after_instance TEXT NOT NULL,
            after_seq INTEGER NOT NULL,
            scanned INTEGER NOT NULL,
            indexed INTEGER NOT NULL,
            started_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            done_at TEXT
        );",
    )
}

// ─── What a journal row contributes ───────────────────────────────────────

/// How a row's text joins its node's document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PieceMode {
    /// An `append`: the text continues the document.
    Delta,
    /// `open`, `replace` or `close`: the text is the node's whole text (or,
    /// for some carriers, only its last chunk).
    Snapshot,
}

/// The searchable part of one journal row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Piece {
    pub(crate) kind: JournalSearchKind,
    /// The node the row mutates; `""` for a row without one (each such row
    /// is a document of its own).
    pub(crate) node_id: String,
    pub(crate) mode: PieceMode,
    pub(crate) role: Option<String>,
    /// The message block an `append` targets.
    pub(crate) block: Option<i64>,
    /// A tool call's name (and title, when it differs).
    pub(crate) head: Option<String>,
    /// For a tool call: the complete input, which replaces the streamed one.
    pub(crate) input: Option<String>,
    /// The row's raw text (a message's or result's text, a call's input
    /// delta), bounded.
    pub(crate) text: String,
}

/// `{"state":"known","value":…}` → the value; a bare value passes through.
fn known(value: &Value) -> Option<&Value> {
    match value.get("state").and_then(Value::as_str) {
        Some("known") => value.get("value"),
        Some(_) => None,
        None => Some(value),
    }
}

fn blocks_text(blocks: Option<&Value>, cap: usize) -> String {
    let mut out = String::new();
    for text in blocks
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
    {
        if out.len() >= cap {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(clip_bytes(text, cap - out.len().min(cap)));
    }
    out
}

/// The scalar leaves of a tool input, in document order, up to `cap` bytes.
fn input_leaves(value: &Value, cap: usize, out: &mut String) {
    if out.len() >= cap {
        return;
    }
    let mut push = |text: &str| {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(clip_bytes(text, cap.saturating_sub(out.len())));
    };
    match value {
        Value::String(text) => push(text),
        Value::Number(number) => push(&number.to_string()),
        Value::Bool(flag) => push(&flag.to_string()),
        Value::Array(items) => items.iter().for_each(|item| input_leaves(item, cap, out)),
        Value::Object(map) => map.values().for_each(|item| input_leaves(item, cap, out)),
        Value::Null => {}
    }
}

/// The longest prefix of `text` within `cap` bytes, on a char boundary.
fn clip_bytes(text: &str, cap: usize) -> &str {
    if text.len() <= cap {
        return text;
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Raw text a document keeps for `kind`: its cap plus slack, so a credential
/// the cap would cut in half is still whole when the redactor sees it.
fn raw_cap(kind: JournalSearchKind) -> usize {
    text_cap(kind) + REDACT_SLACK_BYTES
}

/// Indexed text a document keeps for `kind`.
fn text_cap(kind: JournalSearchKind) -> usize {
    match kind {
        JournalSearchKind::Message => MESSAGE_TEXT_BYTES,
        JournalSearchKind::ToolCall | JournalSearchKind::ToolResult => TOOL_TEXT_BYTES,
    }
}

/// What `event` contributes to the index, if anything.
pub(crate) fn extract(event: &Value) -> Option<Piece> {
    let payload = event.get("payload")?;
    let mode = match payload.get("operation").and_then(Value::as_str) {
        Some("append") => PieceMode::Delta,
        _ => PieceMode::Snapshot,
    };
    let node_id = payload
        .get("nodeId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let block = payload.get("targetBlock").and_then(Value::as_i64);
    let piece = |kind, role, head, input, text: String| Piece {
        kind,
        node_id: node_id.clone(),
        mode,
        role,
        block,
        head,
        input,
        text: clip_bytes(&text, raw_cap(kind)).to_string(),
    };
    let piece = match journal_event_kind(event) {
        "message" => {
            let role = payload.get("role").and_then(Value::as_str)?;
            if role != "user" && role != "assistant" {
                return None;
            }
            // Node messages carry blocks; the Hub's agent-to-agent records
            // carry `text`.
            let cap = raw_cap(JournalSearchKind::Message);
            let mut text = blocks_text(payload.get("blocks"), cap);
            if text.is_empty() {
                text = payload
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| clip_bytes(text, cap).to_string())
                    .unwrap_or_default();
            }
            piece(
                JournalSearchKind::Message,
                Some(role.to_string()),
                None,
                None,
                text,
            )
        }
        "tool_call" => {
            let name = payload
                .get("toolName")
                .and_then(known)
                .and_then(Value::as_str)
                .unwrap_or("");
            let title = payload
                .get("displayTitle")
                .and_then(known)
                .and_then(Value::as_str)
                .filter(|title| *title != name)
                .unwrap_or("");
            let head = [name, title]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            let input = payload.get("input").and_then(known).map(|value| {
                let mut out = String::new();
                input_leaves(value, raw_cap(JournalSearchKind::ToolCall), &mut out);
                out
            });
            let delta = payload
                .get("inputTextDelta")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            piece(
                JournalSearchKind::ToolCall,
                None,
                (!head.is_empty()).then_some(head),
                input,
                delta,
            )
        }
        "tool_result" => {
            if payload.get("stage").and_then(Value::as_str) != Some("final") {
                return None;
            }
            let text = blocks_text(
                payload.get("blocks"),
                raw_cap(JournalSearchKind::ToolResult),
            );
            piece(JournalSearchKind::ToolResult, None, None, None, text)
        }
        _ => return None,
    };
    let empty = piece.text.trim().is_empty()
        && piece
            .head
            .as_deref()
            .is_none_or(|head| head.trim().is_empty())
        && piece
            .input
            .as_deref()
            .is_none_or(|input| input.trim().is_empty());
    (!empty).then_some(piece)
}

fn kind_wire(kind: JournalSearchKind) -> &'static str {
    match kind {
        JournalSearchKind::Message => "message",
        JournalSearchKind::ToolCall => "tool_call",
        JournalSearchKind::ToolResult => "tool_result",
    }
}

fn kind_from_wire(raw: &str) -> JournalSearchKind {
    match raw {
        "tool_call" => JournalSearchKind::ToolCall,
        "tool_result" => JournalSearchKind::ToolResult,
        _ => JournalSearchKind::Message,
    }
}

// ─── The fold: a node's rows into its documents ──────────────────────────

/// Below this many raw bytes a document's indexed text is recomputed on
/// every row; above it, every [`REINDEX_STEP_BYTES`] of growth and on every
/// snapshot. Redaction runs over the whole concatenation (a credential split
/// across two deltas is still one credential), so recomputing it on every
/// token-sized delta of a long message would be quadratic.
const REINDEX_EVERY_ROW_BYTES: usize = 8 * 1024;
const REINDEX_STEP_BYTES: usize = 1024;

/// One document of a node: its rows `first_seq..=seq`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DocState {
    pub(crate) first_seq: i64,
    pub(crate) seq: i64,
    pub(crate) observed_at: String,
    pub(crate) role: Option<String>,
    pub(crate) block: Option<i64>,
    pub(crate) head: String,
    pub(crate) raw: String,
    /// `raw.len()` when `text` was last computed.
    pub(crate) indexed_len: usize,
    pub(crate) text: String,
}

/// What one row did to its node.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Step {
    /// The row continued (or added nothing to) the held document.
    Same(DocState),
    /// The row's text is not a continuation: a new document starts; the
    /// held one stays as it was.
    New(DocState),
}

fn render(kind: JournalSearchKind, head: &str, raw: &str) -> String {
    let source = if kind == JournalSearchKind::ToolCall && !head.is_empty() {
        if raw.is_empty() {
            head.to_string()
        } else {
            format!("{head} {raw}")
        }
    } else {
        raw.to_string()
    };
    let cap = text_cap(kind)
        + if kind == JournalSearchKind::ToolCall {
            head.len()
        } else {
            0
        };
    clip_bytes(crate::digest::redact_secrets(&source).trim(), cap).to_string()
}

fn push_bounded(raw: &mut String, text: &str, cap: usize) {
    if raw.len() < cap {
        raw.push_str(clip_bytes(text, cap - raw.len()));
    }
}

/// Fold one row into its node. `held` is the node's newest document, when
/// there is one; the row's seq is above it. The ONE function both the
/// append path and the backfill use (through [`index_event`]), so the same
/// rows always make the same documents.
///
/// - An `append` continues the held document (a new message block starts on
///   a new line).
/// - A tool call's complete input replaces its streamed input: the call is
///   one document.
/// - Any other snapshot (`open`, `replace`, `close`) that the held text
///   already contains adds nothing (a carrier that closes with its last
///   chunk); one that extends the held text replaces it (the complete form
///   after the deltas); anything else is different text, and starts a new
///   document so the old text stays findable (a correlation restamp).
pub(crate) fn step(
    kind: JournalSearchKind,
    held: Option<&DocState>,
    piece: &Piece,
    seq: i64,
    observed_at: &str,
) -> Step {
    let cap = raw_cap(kind);
    let fresh = |raw: String| DocState {
        first_seq: seq,
        seq,
        observed_at: observed_at.to_string(),
        role: piece.role.clone(),
        block: piece.block,
        head: piece.head.clone().unwrap_or_default(),
        raw,
        indexed_len: 0,
        text: String::new(),
    };
    let (mut doc, new) = match held {
        None => (
            fresh(match kind {
                JournalSearchKind::ToolCall => {
                    piece.input.clone().unwrap_or_else(|| piece.text.clone())
                }
                _ => piece.text.clone(),
            }),
            true,
        ),
        Some(held) => {
            let mut doc = held.clone();
            doc.seq = seq;
            doc.observed_at = observed_at.to_string();
            if let Some(head) = &piece.head {
                doc.head = head.clone();
            }
            if doc.role.is_none() {
                doc.role = piece.role.clone();
            }
            match (kind, piece.mode) {
                (JournalSearchKind::ToolCall, _) if piece.input.is_some() => {
                    doc.raw = piece.input.clone().unwrap_or_default();
                    (doc, false)
                }
                (_, PieceMode::Delta) => {
                    if !doc.raw.is_empty()
                        && piece.block.is_some()
                        && doc.block.is_some()
                        && piece.block != doc.block
                    {
                        push_bounded(&mut doc.raw, "\n", cap);
                    }
                    push_bounded(&mut doc.raw, &piece.text, cap);
                    if piece.block.is_some() {
                        doc.block = piece.block;
                    }
                    (doc, false)
                }
                (_, PieceMode::Snapshot) => {
                    if piece.text.is_empty() || doc.raw.contains(piece.text.as_str()) {
                        (doc, false)
                    } else if piece.text.starts_with(doc.raw.as_str()) {
                        doc.raw = piece.text.clone();
                        (doc, false)
                    } else {
                        (fresh(piece.text.clone()), true)
                    }
                }
            }
        }
    };
    let grown = doc.raw.len().saturating_sub(doc.indexed_len);
    if new
        || piece.mode == PieceMode::Snapshot
        || doc.raw.len() <= REINDEX_EVERY_ROW_BYTES
        || grown >= REINDEX_STEP_BYTES
        || doc.text.is_empty()
    {
        doc.text = render(kind, &doc.head, &doc.raw);
        doc.indexed_len = doc.raw.len();
    }
    if new { Step::New(doc) } else { Step::Same(doc) }
}

/// Fold a node's rows, in seq order, into its documents.
pub(crate) fn fold_node(kind: JournalSearchKind, rows: &[(i64, String, Piece)]) -> Vec<DocState> {
    let mut docs: Vec<DocState> = Vec::new();
    for (seq, observed_at, piece) in rows {
        let held = if piece.node_id.is_empty() {
            None
        } else {
            docs.last()
        };
        match step(kind, held, piece, *seq, observed_at) {
            Step::Same(doc) => *docs.last_mut().expect("held") = doc,
            Step::New(doc) => docs.push(doc),
        }
    }
    docs
}

const DOC_COLUMNS: &str =
    "id, first_seq, seq, observed_at, role, block, head, raw, indexed_len, text";

fn doc_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, DocState)> {
    Ok((
        row.get(0)?,
        DocState {
            first_seq: row.get(1)?,
            seq: row.get(2)?,
            observed_at: row.get(3)?,
            role: row.get(4)?,
            block: row.get(5)?,
            head: row.get(6)?,
            raw: row.get(7)?,
            indexed_len: row.get::<_, i64>(8)?.max(0) as usize,
            text: row.get(9)?,
        },
    ))
}

fn insert_doc(
    conn: &Connection,
    instance_id: &str,
    kind: JournalSearchKind,
    node_id: &str,
    doc: &DocState,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO journal_search_docs
            (instance_id, kind, node_id, first_seq, seq, role, observed_at, block, head,
             raw, indexed_len, text)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            instance_id,
            kind_wire(kind),
            node_id,
            doc.first_seq,
            doc.seq,
            doc.role,
            doc.observed_at,
            doc.block,
            doc.head,
            doc.raw,
            doc.indexed_len as i64,
            doc.text
        ],
    )?;
    Ok(())
}

/// Rebuild a node's documents from its recorded rows: a row arrived below
/// rows already folded (the backfill reaching a node the append path
/// started). Each row is re-read from the journal by its key.
fn rebuild_node(
    conn: &Connection,
    instance_id: &str,
    kind: JournalSearchKind,
    node_id: &str,
) -> rusqlite::Result<()> {
    let rows: Vec<(i64, String, Piece)> = {
        let mut stmt = conn.prepare(
            "SELECT r.seq, j.observed_at, j.payload_json
               FROM journal_search_rows r
               JOIN journal j ON j.instance_id = r.instance_id AND j.seq = r.seq
              WHERE r.instance_id = ?1 AND r.kind = ?2 AND r.node_id = ?3
              ORDER BY r.seq",
        )?;
        let rows = stmt
            .query_map(params![instance_id, kind_wire(kind), node_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .filter_map(|(seq, observed_at, payload)| {
                let event = serde_json::from_str::<Value>(&payload).ok()?;
                Some((seq, observed_at, extract(&event)?))
            })
            .collect()
    };
    conn.execute(
        "DELETE FROM journal_search_docs
          WHERE instance_id = ?1 AND kind = ?2 AND node_id = ?3",
        params![instance_id, kind_wire(kind), node_id],
    )?;
    for doc in fold_node(kind, &rows) {
        insert_doc(conn, instance_id, kind, node_id, &doc)?;
    }
    Ok(())
}

/// Index one journal row, inside the caller's transaction; the append path
/// and the backfill both call it. Returns whether the row was recorded (it
/// carries searchable text and was not recorded before). The row folds
/// into its node's newest document ([`step`]); a row below rows already
/// folded rebuilds the node, so the order the two paths meet a node's rows
/// in never changes its documents.
pub(crate) fn index_event(
    conn: &Connection,
    instance_id: &str,
    seq: i64,
    event: &Value,
    observed_at: &str,
) -> rusqlite::Result<bool> {
    let Some(piece) = extract(event) else {
        return Ok(false);
    };
    let kind = piece.kind;
    let recorded = conn.execute(
        "INSERT OR IGNORE INTO journal_search_rows (instance_id, seq, kind, node_id)
         VALUES (?1, ?2, ?3, ?4)",
        params![instance_id, seq, kind_wire(kind), piece.node_id],
    )?;
    if recorded == 0 {
        return Ok(false);
    }
    if piece.node_id.is_empty() {
        if let Step::New(doc) = step(kind, None, &piece, seq, observed_at) {
            insert_doc(conn, instance_id, kind, "", &doc)?;
        }
        return Ok(true);
    }
    let newest: Option<(i64, DocState)> = conn
        .query_row(
            &format!(
                "SELECT {DOC_COLUMNS} FROM journal_search_docs
                  WHERE instance_id = ?1 AND kind = ?2 AND node_id = ?3
                  ORDER BY seq DESC LIMIT 1"
            ),
            params![instance_id, kind_wire(kind), piece.node_id],
            doc_from_row,
        )
        .optional()?;
    match newest {
        Some((_, held)) if held.seq > seq => {
            rebuild_node(conn, instance_id, kind, &piece.node_id)?;
        }
        Some((id, held)) => match step(kind, Some(&held), &piece, seq, observed_at) {
            Step::Same(doc) => {
                conn.execute(
                    "UPDATE journal_search_docs
                        SET seq = ?2, observed_at = ?3, role = ?4, block = ?5, head = ?6,
                            raw = ?7, indexed_len = ?8, text = ?9
                      WHERE id = ?1",
                    params![
                        id,
                        doc.seq,
                        doc.observed_at,
                        doc.role,
                        doc.block,
                        doc.head,
                        doc.raw,
                        doc.indexed_len as i64,
                        doc.text
                    ],
                )?;
            }
            Step::New(doc) => insert_doc(conn, instance_id, kind, &piece.node_id, &doc)?,
        },
        None => {
            if let Step::New(doc) = step(kind, None, &piece, seq, observed_at) {
                insert_doc(conn, instance_id, kind, &piece.node_id, &doc)?;
            }
        }
    }
    Ok(true)
}

// ─── Backfill ──────────────────────────────────────────────────────────────

/// What one backfill batch did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct BackfillStep {
    pub(crate) scanned: usize,
    pub(crate) indexed: usize,
    pub(crate) done: bool,
}

struct BackfillMark {
    after_instance: String,
    after_seq: i64,
    scanned: i64,
    indexed: i64,
    started_at: Option<String>,
    updated_at: Option<String>,
    done_at: Option<String>,
}

fn backfill_mark(conn: &Connection) -> rusqlite::Result<BackfillMark> {
    Ok(conn
        .query_row(
            "SELECT after_instance, after_seq, scanned, indexed, started_at, updated_at, done_at
               FROM journal_search_backfill WHERE id = 1",
            [],
            |row| {
                Ok(BackfillMark {
                    after_instance: row.get(0)?,
                    after_seq: row.get(1)?,
                    scanned: row.get(2)?,
                    indexed: row.get(3)?,
                    started_at: row.get(4)?,
                    updated_at: row.get(5)?,
                    done_at: row.get(6)?,
                })
            },
        )
        .optional()?
        .unwrap_or(BackfillMark {
            // Instance ids are never empty, so ('', 0) precedes every row.
            after_instance: String::new(),
            after_seq: 0,
            scanned: 0,
            indexed: 0,
            started_at: None,
            updated_at: None,
            done_at: None,
        }))
}

/// One backfill batch: the next `limit` journal rows after the high-water
/// mark in primary-key order, indexed and the mark moved in ONE writer
/// transaction. A batch that dies before its commit leaves the mark where it
/// was, and the next batch reads the same rows again.
fn backfill_batch(conn: &mut Connection, limit: usize) -> Result<BackfillStep, StoreError> {
    let tx = immediate_tx(conn)?;
    let mark = backfill_mark(&tx)?;
    if mark.done_at.is_some() {
        return Ok(BackfillStep {
            done: true,
            ..BackfillStep::default()
        });
    }
    let rows: Vec<(String, i64, String, String)> = {
        let mut stmt = tx.prepare(
            "SELECT instance_id, seq, payload_json, observed_at FROM journal
              WHERE (instance_id, seq) > (?1, ?2)
              ORDER BY instance_id, seq LIMIT ?3",
        )?;
        stmt.query_map(
            params![mark.after_instance, mark.after_seq, limit as i64],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?
        .collect::<rusqlite::Result<_>>()?
    };
    let mut step = BackfillStep {
        scanned: rows.len(),
        done: rows.len() < limit,
        ..BackfillStep::default()
    };
    for (index, (instance_id, seq, payload, observed_at)) in rows.iter().enumerate() {
        backfill_hook(index)?;
        let Ok(event) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if index_event(&tx, instance_id, *seq, &event, observed_at)? {
            step.indexed += 1;
        }
    }
    let (after_instance, after_seq) = rows
        .last()
        .map(|(instance_id, seq, _, _)| (instance_id.clone(), *seq))
        .unwrap_or((mark.after_instance, mark.after_seq));
    let now = now_rfc3339();
    tx.execute(
        "INSERT INTO journal_search_backfill
            (id, after_instance, after_seq, scanned, indexed, started_at, updated_at, done_at)
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (id) DO UPDATE SET
            after_instance = excluded.after_instance, after_seq = excluded.after_seq,
            scanned = excluded.scanned, indexed = excluded.indexed,
            updated_at = excluded.updated_at, done_at = excluded.done_at",
        params![
            after_instance,
            after_seq,
            mark.scanned + step.scanned as i64,
            mark.indexed + step.indexed as i64,
            mark.started_at.unwrap_or_else(|| now.clone()),
            now,
            step.done.then(|| now.clone()),
        ],
    )?;
    tx.commit()?;
    Ok(step)
}

/// Run the backfill to completion in the background, one bounded writer job
/// at a time with a pause between, so it never holds the writer for long and
/// never delays boot. It stops when the Store closes.
pub(crate) fn spawn_backfill(store: Store) {
    tokio::spawn(async move {
        let started = Instant::now();
        let (mut batches, mut scanned, mut indexed) = (0usize, 0usize, 0usize);
        loop {
            match store
                .journal_search_backfill_batch(BACKFILL_BATCH_ROWS)
                .await
            {
                Ok(step) => {
                    if step.scanned > 0 {
                        batches += 1;
                    }
                    scanned += step.scanned;
                    indexed += step.indexed;
                    if step.done {
                        if batches > 0 {
                            tracing::info!(
                                batches,
                                scanned,
                                indexed,
                                elapsed_ms = started.elapsed().as_millis() as u64,
                                "journal search backfill done"
                            );
                        }
                        return;
                    }
                    tokio::time::sleep(BACKFILL_PAUSE).await;
                }
                Err(StoreError::Closed) => return,
                Err(error) => {
                    tracing::warn!(%error, "journal search backfill batch failed; retrying");
                    tokio::time::sleep(BACKFILL_RETRY).await;
                }
            }
        }
    });
}

// ─── Query ─────────────────────────────────────────────────────────────────

/// Who is asking and how they narrowed it.
#[derive(Debug, Clone, Default)]
pub(crate) struct SearchScope {
    /// The Agent caller's instance; `None` for Human and Bot devices, which
    /// read every instance's journal.
    pub(crate) agent_instance: Option<String>,
    pub(crate) instance: Option<String>,
    pub(crate) lineage: Option<String>,
    /// Normalized RFC 3339 lower bound on `observed_at`.
    pub(crate) since: Option<String>,
}

/// Arms a wall-clock budget on a connection for the guard's lifetime: past the
/// deadline SQLite interrupts the running statement.
struct Budget<'c> {
    conn: &'c Connection,
}

impl<'c> Budget<'c> {
    fn arm(conn: &'c Connection, deadline: Instant) -> Self {
        conn.progress_handler(1000, Some(move || Instant::now() >= deadline));
        Budget { conn }
    }
}

impl Drop for Budget<'_> {
    fn drop(&mut self) {
        self.conn.progress_handler(0, None::<fn() -> bool>);
    }
}

fn interrupted(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::OperationInterrupted
    )
}

/// The scope predicate over `i` (the hit's instance) and, for an Agent, `p`
/// (its parent), as SQL plus the joins it needs. Parameter `?1` is the
/// caller's lineage (NULL for Human and Bot), `?2` the instance, `?3` the
/// lineage, `?4` the time bound. Every statement binds all four.
fn scope_sql(agent: bool) -> (&'static str, &'static str) {
    if agent {
        (
            "LEFT JOIN instances p ON p.id = json_extract(i.spec_json, '$.parentInstanceId')",
            "(COALESCE(i.lineage_id, i.id) = ?1 OR COALESCE(p.lineage_id, p.id) = ?1)",
        )
    } else {
        ("", "?1 IS NULL")
    }
}

const NARROW_SQL: &str = "(?2 IS NULL OR d.instance_id = ?2)
    AND (?3 IS NULL OR COALESCE(i.lineage_id, i.id) = ?3)
    AND (?4 IS NULL OR d.observed_at >= ?4)";

/// A hit before its snippet is final.
struct Row {
    id: i64,
    hit: JournalSearchHit,
}

fn clip_chars(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let mut out: String = text.chars().take(cap.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// At most [`JOURNAL_SEARCH_SNIPPET_CHARS`] characters of `text` around the
/// first match of any term (ASCII case folded, as `LIKE` folds).
fn snippet_around(text: &str, terms: &[&str]) -> String {
    let chars: Vec<char> = text.chars().collect();
    let folded: Vec<char> = chars.iter().map(char::to_ascii_lowercase).collect();
    let first = terms
        .iter()
        .filter_map(|term| {
            let needle: Vec<char> = term.chars().map(|c| c.to_ascii_lowercase()).collect();
            (!needle.is_empty())
                .then(|| {
                    folded
                        .windows(needle.len())
                        .position(|w| w == needle.as_slice())
                })
                .flatten()
        })
        .min()
        .unwrap_or(0);
    let cap = JOURNAL_SEARCH_SNIPPET_CHARS;
    if chars.len() <= cap {
        return text.to_string();
    }
    let start = first
        .saturating_sub(SNIPPET_LEAD_CHARS)
        .min(chars.len() - cap);
    let mut body: Vec<char> = chars[start..start + cap].to_vec();
    if start > 0 {
        body[0] = '…';
    }
    if start + cap < chars.len() {
        body[cap - 1] = '…';
    }
    body.into_iter().collect()
}

fn fts_pass(
    conn: &Connection,
    scope: &SearchScope,
    caller_lineage: Option<&str>,
    terms: &[&str],
    limit: usize,
) -> rusqlite::Result<Vec<Row>> {
    let (join, predicate) = scope_sql(scope.agent_instance.is_some());
    let mut stmt = conn.prepare(&format!(
        "SELECT d.id, d.instance_id, COALESCE(i.lineage_id, i.id), d.seq, d.observed_at,
                d.kind, d.role, snippet(journal_fts, 0, '', '', '…', {SNIPPET_TOKENS})
           FROM journal_fts
           JOIN journal_search_docs d ON d.id = journal_fts.rowid
           JOIN instances i ON i.id = d.instance_id
           {join}
          WHERE journal_fts MATCH ?5 AND {predicate} AND {NARROW_SQL}
          ORDER BY bm25(journal_fts), d.id DESC
          LIMIT ?6"
    ))?;
    stmt.query_map(
        params![
            caller_lineage,
            scope.instance,
            scope.lineage,
            scope.since,
            crate::memory::fts_query(terms),
            limit as i64
        ],
        |row| {
            Ok(Row {
                id: row.get(0)?,
                hit: JournalSearchHit {
                    instance_id: row.get(1)?,
                    lineage_id: row.get(2)?,
                    seq: U64(row.get::<_, i64>(3)?.max(0) as u64),
                    observed_at: row.get(4)?,
                    kind: kind_from_wire(&row.get::<_, String>(5)?),
                    role: row.get(6)?,
                    matched_by: JournalSearchMatch::Fts,
                    snippet: clip_chars(&row.get::<_, String>(7)?, JOURNAL_SEARCH_SNIPPET_CHARS),
                },
            })
        },
    )?
    .collect()
}

fn like_pass(
    conn: &Connection,
    scope: &SearchScope,
    caller_lineage: Option<&str>,
    terms: &[&str],
    exclude: &[i64],
    remaining: usize,
) -> rusqlite::Result<Vec<Row>> {
    let (join, predicate) = scope_sql(scope.agent_instance.is_some());
    let mut values: Vec<rusqlite::types::Value> = vec![
        caller_lineage.map(str::to_string).into(),
        scope.instance.clone().into(),
        scope.lineage.clone().into(),
        scope.since.clone().into(),
    ];
    let mut sql = format!(
        "SELECT d.id, d.instance_id, COALESCE(i.lineage_id, i.id), d.seq, d.observed_at,
                d.kind, d.role, d.text
           FROM journal_search_docs d
           JOIN instances i ON i.id = d.instance_id
           {join}
          WHERE {predicate} AND {NARROW_SQL}"
    );
    for term in terms {
        values.push(crate::memory::like_pattern(term).into());
        sql.push_str(&format!(" AND d.text LIKE ?{} ESCAPE '\\'", values.len()));
    }
    if !exclude.is_empty() {
        let first = values.len() + 1;
        let slots: Vec<String> = (first..first + exclude.len())
            .map(|n| format!("?{n}"))
            .collect();
        sql.push_str(&format!(" AND d.id NOT IN ({})", slots.join(", ")));
        values.extend(
            exclude
                .iter()
                .map(|id| rusqlite::types::Value::Integer(*id)),
        );
    }
    values.push(rusqlite::types::Value::Integer(remaining as i64));
    sql.push_str(&format!(
        " ORDER BY d.observed_at DESC, d.id DESC LIMIT ?{}",
        values.len()
    ));
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(params_from_iter(values), |row| {
        let text: String = row.get(7)?;
        Ok(Row {
            id: row.get(0)?,
            hit: JournalSearchHit {
                instance_id: row.get(1)?,
                lineage_id: row.get(2)?,
                seq: U64(row.get::<_, i64>(3)?.max(0) as u64),
                observed_at: row.get(4)?,
                kind: kind_from_wire(&row.get::<_, String>(5)?),
                role: row.get(6)?,
                matched_by: JournalSearchMatch::Like,
                snippet: snippet_around(&text, terms),
            },
        })
    })?
    .collect()
}

/// Whether the substring pass runs: what `unicode61` cannot see is a run of
/// non-ASCII text (CJK is one token per run) or a fragment shorter than a
/// useful word.
fn wants_like(terms: &[&str]) -> bool {
    terms
        .iter()
        .any(|term| !term.is_ascii() || term.chars().count() < 3)
}

/// One search, both passes in ONE read transaction under one time budget.
fn search_conn(
    conn: &Connection,
    scope: &SearchScope,
    query: &str,
    limit: usize,
    budget: Duration,
) -> Result<JournalSearchResult, StoreError> {
    let terms: Vec<&str> = query.split_whitespace().collect();
    let tx = conn.unchecked_transaction()?;
    let index_complete = backfill_mark(&tx)?.done_at.is_some();
    let mut result = JournalSearchResult {
        hits: Vec::new(),
        partial: false,
        index_complete,
    };
    // An Agent reads what `owns()` admits: its lineage, and instances whose
    // parent is in its lineage. A caller row that is gone reads nothing.
    let caller_lineage = match scope.agent_instance.as_deref() {
        Some(caller) => match lineage_id_of(&tx, caller)? {
            Some(lineage) => Some(lineage),
            None => return Ok(result),
        },
        None => None,
    };
    if terms.is_empty() {
        return Ok(result);
    }
    let _budget = Budget::arm(&tx, Instant::now() + budget);
    let mut seen = Vec::new();
    let mut fts_refused = false;
    match fts_pass(&tx, scope, caller_lineage.as_deref(), &terms, limit) {
        Ok(rows) => {
            for row in rows {
                seen.push(row.id);
                result.hits.push(row.hit);
            }
        }
        Err(error) if interrupted(&error) => {
            result.partial = true;
            return Ok(result);
        }
        // FTS syntax in user text is quoted away, but a query the grammar
        // still refuses is not an error: the substring pass answers it.
        Err(_) => fts_refused = true,
    }
    if result.hits.len() < limit && (fts_refused || wants_like(&terms)) {
        let remaining = limit - result.hits.len();
        match like_pass(
            &tx,
            scope,
            caller_lineage.as_deref(),
            &terms,
            &seen,
            remaining,
        ) {
            Ok(rows) => result.hits.extend(rows.into_iter().map(|row| row.hit)),
            Err(error) if interrupted(&error) => result.partial = true,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(result)
}

fn status_conn(conn: &Connection, counts: bool) -> Result<JournalSearchStatus, StoreError> {
    let tx = conn.unchecked_transaction()?;
    let mark = backfill_mark(&tx)?;
    let state = if mark.done_at.is_some() {
        JournalSearchBackfillState::Done
    } else if mark.updated_at.is_some() {
        JournalSearchBackfillState::Running
    } else {
        JournalSearchBackfillState::Pending
    };
    let mut status = JournalSearchStatus {
        complete: mark.done_at.is_some(),
        state,
        done_at: mark.done_at,
        scanned_rows: None,
        indexed_rows: None,
        docs: None,
        journal_rows: None,
    };
    if counts {
        let count = |sql: &str| -> rusqlite::Result<U64> {
            Ok(U64(
                tx.query_row(sql, [], |row| row.get::<_, i64>(0))?.max(0) as u64,
            ))
        };
        status.scanned_rows = Some(U64(mark.scanned.max(0) as u64));
        status.indexed_rows = Some(U64(mark.indexed.max(0) as u64));
        status.docs = Some(count("SELECT COUNT(*) FROM journal_search_docs")?);
        status.journal_rows = Some(count("SELECT COUNT(*) FROM journal")?);
    }
    Ok(status)
}

// ─── Store API ─────────────────────────────────────────────────────────────

impl Store {
    /// One backfill batch on the writer ([`backfill_batch`]).
    pub(crate) async fn journal_search_backfill_batch(
        &self,
        limit: usize,
    ) -> Result<BackfillStep, StoreError> {
        self.run_named("journal_search_backfill", move |conn| {
            backfill_batch(conn, limit)
        })
        .await
    }

    /// Search the journal under `scope` ([`search_conn`]).
    pub(crate) async fn journal_search(
        &self,
        scope: SearchScope,
        query: String,
        limit: usize,
        budget: Duration,
    ) -> Result<JournalSearchResult, StoreError> {
        self.read("journal_search", move |conn| {
            search_conn(conn, &scope, &query, limit, budget)
        })
        .await
    }

    /// The backfill's progress; Hub-wide counts only when `counts`.
    pub(crate) async fn journal_search_status(
        &self,
        counts: bool,
    ) -> Result<JournalSearchStatus, StoreError> {
        self.read("journal_search_status", move |conn| {
            status_conn(conn, counts)
        })
        .await
    }
}

// ─── HTTP ──────────────────────────────────────────────────────────────────

/// Search routes. Agent origin is admitted by `restrict_agent_routes`; the
/// scope is applied in the query.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/search", get(search))
        .route("/v1/search/status", get(status))
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
    instance: Option<String>,
    lineage: Option<String>,
    since: Option<String>,
    limit: Option<u32>,
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// `GET /v1/search?q=&instance=&lineage=&since=&limit=`.
async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SearchQuery>,
) -> Result<Json<JournalSearchResult>, HubError> {
    let device = caller(&state, &headers).await?;
    let q = query.q.trim().to_string();
    if q.is_empty() {
        return Err(HubError::BadRequest("q must not be empty".into()));
    }
    if q.chars().count() > JOURNAL_SEARCH_MAX_QUERY_CHARS {
        return Err(HubError::BadRequest(format!(
            "q takes at most {JOURNAL_SEARCH_MAX_QUERY_CHARS} characters"
        )));
    }
    let terms = q.split_whitespace().count();
    if terms > JOURNAL_SEARCH_MAX_TERMS {
        return Err(HubError::BadRequest(format!(
            "search takes at most {JOURNAL_SEARCH_MAX_TERMS} terms (every term must match); \
             got {terms}"
        )));
    }
    let since = match non_empty(query.since) {
        Some(raw) => Some(
            time::OffsetDateTime::parse(&raw, &time::format_description::well_known::Rfc3339)
                .map(|at| crate::config::rfc3339(at.to_offset(time::UtcOffset::UTC)))
                .map_err(|_| HubError::BadRequest("since must be an RFC 3339 time".into()))?,
        ),
        None => None,
    };
    let agent_instance = if origin(&device) == InputOrigin::Agent {
        // An Agent device always names its instance; one that does not reads
        // nothing rather than everything.
        Some(device.instance_id.clone().unwrap_or_default())
    } else {
        None
    };
    let scope = SearchScope {
        agent_instance,
        instance: non_empty(query.instance),
        lineage: non_empty(query.lineage),
        since,
    };
    let limit = query
        .limit
        .unwrap_or(JOURNAL_SEARCH_DEFAULT_LIMIT)
        .clamp(1, JOURNAL_SEARCH_MAX_LIMIT) as usize;
    let started = Instant::now();
    let result = state
        .store
        .journal_search(scope, q, limit, QUERY_BUDGET)
        .await?;
    tracing::debug!(
        terms,
        hits = result.hits.len(),
        partial = result.partial,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "journal search"
    );
    Ok(Json(result))
}

/// `GET /v1/search/status`. The counts are Hub-wide, so an Agent gets the
/// state alone.
async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<JournalSearchStatus>, HubError> {
    let device = caller(&state, &headers).await?;
    let counts = origin(&device) != InputOrigin::Agent;
    Ok(Json(state.store.journal_search_status(counts).await?))
}

// ─── Test seams (below the routes: the OpenAPI scan stops at cfg(test)) ───

/// Called before each row of a backfill batch is indexed; a test may fail
/// the batch there to model a kill mid-batch.
fn backfill_hook(index: usize) -> rusqlite::Result<()> {
    test_seam::backfill_hook(index)
}

#[cfg(not(test))]
mod test_seam {
    #[inline]
    pub(super) fn backfill_hook(_index: usize) -> rusqlite::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod test_seam {
    use std::cell::Cell;

    thread_local! {
        /// Fail the batch at this row index, once.
        pub(super) static FAIL_AT: Cell<Option<usize>> = const { Cell::new(None) };
    }

    pub(super) fn backfill_hook(index: usize) -> rusqlite::Result<()> {
        FAIL_AT.with(|fail| {
            if fail.get() == Some(index) {
                fail.set(None);
                return Err(rusqlite::Error::InvalidQuery);
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests;
