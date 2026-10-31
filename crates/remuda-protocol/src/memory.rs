//! Main-agent memory: one Hub row per entry, keyed by lineage (D-060,
//! cb-memory-core; replaces the whole-document memory of main-agent.md §12).
//!
//! Every write is a per-entry revision compare-and-set, so an owner edit and a
//! seat write never collide on a shared document. The Hub renders the index;
//! the size limits below are part of the wire contract because the write
//! result reports the actual numbers against them.
//!
//! Pure data: the Hub stores, searches and authorizes it.

use serde::{Deserialize, Serialize};

/// Soft per-entry limit in bytes; a write above it succeeds with a warning.
pub const MEMORY_ENTRY_SOFT_BYTES: u64 = 2 * 1024;
/// Hard per-entry limit in bytes; a write above it is refused.
pub const MEMORY_ENTRY_HARD_BYTES: u64 = 8 * 1024;
/// Soft limit in bytes for the Hub-rendered index of one lineage.
pub const MEMORY_INDEX_SOFT_BYTES: u64 = 4 * 1024;

wire_enum!(MemoryType, "8", {
    User => "user",
    Feedback => "feedback",
    Project => "project",
    Reference => "reference",
});

impl MemoryType {
    /// Index order: who the owner is, how to work, what is going on, where
    /// to look.
    pub const ALL: [MemoryType; 4] = [
        MemoryType::User,
        MemoryType::Feedback,
        MemoryType::Project,
        MemoryType::Reference,
    ];

    /// Wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryType::User => "user",
            MemoryType::Feedback => "feedback",
            MemoryType::Project => "project",
            MemoryType::Reference => "reference",
        }
    }

    /// Parse a wire spelling.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }

    /// Position in the rendered index.
    pub fn rank(self) -> usize {
        self as usize
    }
}

// How the writing caller authenticated. The root turn that caused an Agent
// write (`rootTurn`, cb-rootturn) is recorded beside it later; this field
// never changes meaning when that lands.
wire_enum!(MemoryOrigin, "8", {
    Human => "human",
    Agent => "agent",
});

// What one memory write did to its slug.
wire_enum!(MemoryWriteOp, "8", {
    Write => "write",
    Consolidate => "consolidate",
    Delete => "delete",
});

/// The turn a write happened in: the chapter that wrote it and the journal
/// seq of that chapter's most recent `turn_started` row at write time.
///
/// Stamped from the Hub's own journal, never from the request body. Absent for
/// an owner (PWA) write and for a write before the chapter's first turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemoryTurnRef {
    /// Chapter that was writing (`ins_…`).
    pub instance_id: String,
    /// Journal seq of the writer's latest `turn_started` row.
    pub seq: u64,
}

/// One page of a writer instance's append-only memory write log.
///
/// Rows are ordered oldest first within the page. Without a cursor the page is
/// the NEWEST page (a recent transcript needs its recent turns);
/// `next_after_id` is the row id to poll from, so a single turn with more rows
/// than a page still advances.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemoryWritePage {
    /// Rows in this page, oldest first.
    pub items: Vec<MemoryWriteItem>,
    /// Row id to pass back as `afterId`; the newest row id in the page, or the
    /// cursor itself when the page is empty.
    pub next_after_id: u64,
}

/// One row of a writer instance's append-only memory write log (D-060).
///
/// A row survives a later rewrite or delete of the entry, because every turn
/// that wrote memory shows its row under the reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemoryWriteItem {
    /// Entry handle.
    pub slug: String,
    /// Title at write time (kept when the entry is later rewritten or
    /// deleted).
    pub title: String,
    /// Revision this write produced; for a consolidate input, the revision
    /// it consumed.
    pub revision: u64,
    /// What the write did.
    pub op: MemoryWriteOp,
    /// Turn the write happened in; null for an owner write or a write before
    /// the chapter's first turn.
    #[serde(default)]
    pub turn_seq: Option<u64>,
    /// Time of the write.
    pub written_at: String,
}

/// A journal row an entry was derived from. Lets a later journal-redaction
/// pass find every entry that repeats a redacted row.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct MemorySourceRef {
    /// Instance whose journal holds the row (`ins_…`).
    pub instance_id: String,
    /// Journal sequence number of the row.
    pub seq: u64,
}

/// One memory entry of one lineage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemoryEntry {
    /// Owning lineage (its first chapter's instance id).
    pub lineage_id: String,
    /// Stable handle, unique within the lineage (archived entries included).
    pub slug: String,
    /// Entry type.
    #[serde(rename = "type")]
    pub memory_type: MemoryType,
    /// One-line title.
    pub title: String,
    /// One-line description; the index shows it.
    pub description: String,
    /// Entry body; read on demand, never in the index.
    pub body: String,
    /// Revision: the next one for this slug on every write, never reused —
    /// a slug recreated after a delete continues after its last revision, so
    /// a stale `expectedRevision` never matches a replacement. 1 for a slug
    /// never used before. Updates and deletes carry it back as
    /// `expectedRevision`.
    pub revision: u64,
    /// How the caller of the last write authenticated.
    pub origin: MemoryOrigin,
    /// Journal rows the entry was derived from.
    #[serde(default)]
    pub source_refs: Vec<MemorySourceRef>,
    /// Chapter that made the last write; absent for an owner write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_instance: Option<String>,
    /// Turn of the last write, from the writer chapter's journal; null for an
    /// owner write or a write before that chapter's first turn. Always
    /// serialized so clients see an explicit null.
    #[serde(default)]
    pub turn_ref: Option<MemoryTurnRef>,
    /// First write.
    pub created_at: String,
    /// Last write.
    pub updated_at: String,
    /// Set when the lineage retired; archived entries leave the index and
    /// search until the owner restores them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
}

wire_enum!(MemoryLimit, "8", {
    EntrySoft => "entry-soft",
    IndexSoft => "index-soft",
});

/// A soft limit a write crossed, with the actual numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemorySizeWarning {
    /// Which limit.
    pub limit: MemoryLimit,
    /// Actual size in bytes.
    pub bytes: u64,
    /// The limit in bytes.
    pub max_bytes: u64,
}

/// Result of a create, update or consolidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemoryWriteResult {
    /// The entry as committed.
    pub entry: MemoryEntry,
    /// Entries the write removed (consolidate only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
    /// Size of the committed entry: title + description + body, UTF-8 bytes.
    pub entry_bytes: u64,
    /// Size of the lineage's rendered index after the write.
    pub index_bytes: u64,
    /// Soft limits crossed by this write.
    #[serde(default)]
    pub warnings: Vec<MemorySizeWarning>,
}

/// The Hub-rendered index: one line per live entry (archived entries too
/// when asked for, marked `(archived)`), stable-ordered by type then slug.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemoryIndex {
    /// Lineage.
    pub lineage_id: String,
    /// Rendered text, one `- [type] slug: title — description` line per
    /// entry; an archived entry's line ends ` (archived)`.
    pub text: String,
    /// Number of entries in the index.
    pub count: u64,
    /// Rendered size in bytes.
    pub bytes: u64,
    /// The index soft limit, when crossed.
    #[serde(default)]
    pub warnings: Vec<MemorySizeWarning>,
}

wire_enum!(MemoryMatch, "8", {
    Fts => "fts",
    Like => "like",
});

/// One search hit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemorySearchHit {
    /// The matching entry.
    pub entry: MemoryEntry,
    /// `fts` for a full-text (bm25-ranked) hit, `like` for a substring hit
    /// the tokenizer could not see (for example a short CJK query).
    pub matched_by: MemoryMatch,
    /// bm25 score of an `fts` hit (lower is better).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

/// Entry size as the limits measure it: title + description + body in UTF-8
/// bytes.
pub fn memory_entry_bytes(title: &str, description: &str, body: &str) -> u64 {
    (title.len() + description.len() + body.len()) as u64
}

/// Render one index line; an archived entry's line ends ` (archived)`.
pub fn memory_index_line(
    memory_type: MemoryType,
    slug: &str,
    title: &str,
    description: &str,
    archived: bool,
) -> String {
    let marker = if archived { " (archived)" } else { "" };
    format!(
        "- [{}] {slug}: {title} — {description}{marker}\n",
        memory_type.as_str()
    )
}

/// Validate a slug: 1–64 of `a-z 0-9 . _ -`, starting with a letter or digit.
pub fn validate_memory_slug(slug: &str) -> Result<(), String> {
    let mut bytes = slug.bytes();
    let first_ok = bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    let rest_ok = slug
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'));
    if first_ok && rest_ok && slug.len() <= 64 {
        Ok(())
    } else {
        Err(format!(
            "invalid memory slug {slug:?}: use 1-64 characters of a-z, 0-9, '.', '_' or '-', \
             starting with a letter or digit"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_lowercase_handles() {
        for ok in ["a", "owner-prefers-terse", "x.y_z-1", "0abc"] {
            assert!(validate_memory_slug(ok).is_ok(), "{ok}");
        }
        let long = "a".repeat(65);
        for bad in ["", "-a", "A", "a b", "a/b", "边界", long.as_str()] {
            assert!(validate_memory_slug(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn entry_bytes_count_utf8() {
        assert_eq!(memory_entry_bytes("t", "d", "边界"), 2 + 6);
    }

    #[test]
    fn index_line_shape_is_stable() {
        assert_eq!(
            memory_index_line(
                MemoryType::Feedback,
                "terse",
                "Be terse",
                "short replies",
                false
            ),
            "- [feedback] terse: Be terse — short replies\n"
        );
        assert_eq!(
            memory_index_line(MemoryType::User, "old", "Old", "retired", true),
            "- [user] old: Old — retired (archived)\n"
        );
        let ranks: Vec<usize> = MemoryType::ALL.iter().map(|t| t.rank()).collect();
        assert_eq!(ranks, vec![0, 1, 2, 3]);
        assert_eq!(MemoryType::parse("project"), Some(MemoryType::Project));
        assert_eq!(MemoryType::parse("digest"), None);
    }

    #[test]
    fn entry_serializes_type_and_omits_absent_fields() {
        let entry = MemoryEntry {
            lineage_id: "ins_a".into(),
            slug: "s".into(),
            memory_type: MemoryType::User,
            title: "t".into(),
            description: "d".into(),
            body: String::new(),
            revision: 1,
            origin: MemoryOrigin::Human,
            source_refs: vec![],
            writer_instance: None,
            turn_ref: None,
            created_at: "c".into(),
            updated_at: "u".into(),
            archived_at: None,
        };
        let value = serde_json::to_value(&entry).unwrap();
        assert_eq!(value["type"], "user");
        assert_eq!(value["origin"], "human");
        assert!(value.get("writerInstance").is_none());
        assert!(value.get("archivedAt").is_none());
        // turnRef is always present: an explicit null is the wire shape.
        assert!(value.as_object().unwrap().contains_key("turnRef"));
        assert!(value["turnRef"].is_null());
        assert_eq!(value["sourceRefs"], serde_json::json!([]));
    }
}
