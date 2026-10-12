//! Independent failure-settlement covers over one JSONL journal.
//!
//! c-cardsettle-fu2 r9 item 1: coverage of a live StopFailure is modelled as
//! a set of INDEPENDENT [`Cover`] objects, one per unread record the failure
//! settles. Each cover binds to:
//! - the kernel identity ([`FileId`]) and generation of the file it was
//!   observed in,
//! - its own start offset and its own end/extent,
//! - the exact bytes it covers (and their digest),
//! - its kind — a complete record, or a trailing record still mid-write,
//! - the member (agent id) the record settles.
//!
//! On EVERY observation (a poll open AND a failure peek open) each cover is
//! re-validated independently against the opened file:
//! - the identity/generation is unchanged and every byte in the cover's own
//!   range is still byte-for-byte identical → keep; a pending-tail cover may
//!   also GROW (the newly observed continuation bytes/extent are recorded);
//! - any byte in its range removed or rewritten, or the identity/generation
//!   changed → invalidate THAT cover only. Invalidating a pending tail never
//!   discards a still-valid complete record (what used to be its "fallback"):
//!   the two are separate covers in the same set.
//!
//! A cover is retired when the record it covers is consumed — by ANY member
//! as ANY record type — or when an observation invalidates it. Suppression of
//! a folded `started` applies only to a record whose identity and bytes match
//! a live cover.

use crate::util::digest_of;
use remuda_protocol::Digest;

/// Stable kernel identity of one opened file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileId {
    /// Device id.
    pub dev: u64,
    /// Inode number.
    pub ino: u64,
}

/// What a cover protects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoverKind {
    /// A complete `started` record (terminated, or awaiting only its newline).
    /// Bound by its exact record bytes and digest.
    CompleteRecord,
    /// A trailing record captured mid-write. Its type/identity were validated
    /// for the settled member only while unresolved; the completed JSON is
    /// matched again at consumption (the started fold routes type and agent
    /// identity, and the covered prefix must match byte-for-byte).
    PendingTail,
}

/// One independently validated settlement cover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cover {
    /// Member (agent id) whose failure this cover settles.
    pub agent: String,
    pub kind: CoverKind,
    /// Kernel identity of the file the bytes were observed in.
    pub file_id: FileId,
    /// File generation the record belongs to.
    pub generation: u64,
    /// Byte offset of the record's first byte.
    pub offset: u64,
    /// One past the last covered byte: the cover's own extent.
    pub extent: u64,
    /// Exact bytes observed at `offset..extent`.
    pub bytes: Vec<u8>,
    /// Digest of `bytes`; a complete record matches on this digest.
    pub digest: Digest,
}

impl Cover {
    /// Build a cover of the given kind for the record bytes observed at
    /// `offset` in `(generation, file_id)`. `extent` is the cover's own extent
    /// (the file length at observation for a still-torn tail; the record end
    /// for a complete record).
    fn new(
        agent: impl Into<String>,
        kind: CoverKind,
        file_id: FileId,
        generation: u64,
        offset: u64,
        extent: u64,
        bytes: Vec<u8>,
    ) -> Self {
        let digest = digest_of(&bytes);
        Self {
            agent: agent.into(),
            kind,
            file_id,
            generation,
            offset,
            extent,
            bytes,
            digest,
        }
    }

    /// A complete-record cover over `line` (without its terminating newline)
    /// beginning at `offset`.
    pub(crate) fn complete(
        agent: impl Into<String>,
        file_id: FileId,
        generation: u64,
        offset: u64,
        line: &[u8],
    ) -> Self {
        let extent = offset + line.len() as u64;
        Self::new(
            agent,
            CoverKind::CompleteRecord,
            file_id,
            generation,
            offset,
            extent,
            line.to_vec(),
        )
    }

    /// A pending-tail cover over the raw trailing bytes observed at `offset`,
    /// with the file reaching `extent`.
    pub(crate) fn pending(
        agent: impl Into<String>,
        file_id: FileId,
        generation: u64,
        offset: u64,
        extent: u64,
        prefix: Vec<u8>,
    ) -> Self {
        Self::new(
            agent,
            CoverKind::PendingTail,
            file_id,
            generation,
            offset,
            extent,
            prefix,
        )
    }

    /// Re-validate this cover against the opened file. `Keep` retains it as
    /// is; `Grow` (pending tails only) extends it over the newly observed
    /// continuation of the SAME trailing record; anything else invalidates it.
    fn revalidate(&self, obs: &ObservedFile<'_>) -> CoverVerdict {
        if self.file_id != obs.file_id || self.generation != obs.generation {
            return CoverVerdict::Invalidate;
        }
        // Any shrink that removes a covered byte invalidates immediately.
        if obs.len < self.extent {
            return CoverVerdict::Invalidate;
        }
        let Some(relative) = self.offset.checked_sub(obs.rest_base) else {
            return CoverVerdict::Invalidate;
        };
        let relative = relative as usize;
        let Some(slice) = obs.rest.get(relative..relative + self.bytes.len()) else {
            return CoverVerdict::Invalidate;
        };
        if slice != self.bytes {
            return CoverVerdict::Invalidate;
        }
        if self.kind == CoverKind::PendingTail && obs.len > self.extent {
            // The tail grew. Extend over the continuation of the same record:
            // through the first newly observed newline (exclusive), or to the
            // new end of file while the record is still being written. The
            // EXACT raw bytes are kept, including a trailing CR: a CR seen
            // before any newline is still an unread byte the cover binds, and
            // overwriting it later must invalidate the cover. CRLF is
            // normalised only when the completed record is matched in
            // [`CoverSet::settles`].
            let tail = match obs.rest.get(relative + self.bytes.len()..) {
                Some(tail) => tail,
                None => return CoverVerdict::Invalidate,
            };
            let grow = tail
                .iter()
                .position(|byte| *byte == b'\n')
                .unwrap_or(tail.len());
            let mut bytes = self.bytes.clone();
            bytes.extend_from_slice(&tail[..grow]);
            CoverVerdict::Grow(bytes)
        } else {
            CoverVerdict::Keep
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CoverVerdict {
    Keep,
    Grow(Vec<u8>),
    Invalidate,
}

/// One opened file as seen at observation time. `rest` are the bytes
/// `[rest_base, len)` read from the effective cursor after rotation
/// normalisation.
pub(crate) struct ObservedFile<'a> {
    pub file_id: FileId,
    pub generation: u64,
    pub len: u64,
    pub rest_base: u64,
    pub rest: &'a [u8],
}

/// The independent covers armed over one journal tail, keyed by the member
/// each settles.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CoverSet {
    covers: Vec<Cover>,
}

impl CoverSet {
    /// An empty set.
    pub(crate) fn new() -> Self {
        Self { covers: Vec::new() }
    }

    /// Covers currently armed for `agent`.
    #[cfg(test)]
    pub(crate) fn for_agent(&self, agent: &str) -> impl Iterator<Item = &Cover> {
        self.covers.iter().filter(move |cover| cover.agent == agent)
    }

    /// Replace `agent`'s covers with a fresh peek result. Every cover of
    /// EVERY other member is left untouched (covers validate independently).
    pub(crate) fn arm(&mut self, agent: &str, fresh: Vec<Cover>) {
        self.covers.retain(|cover| cover.agent != agent);
        self.covers.extend(fresh);
    }

    /// Re-validate every cover against this observation (poll AND peek):
    /// invalidated covers are dropped for good; pending tails grow over the
    /// continuation bytes the same observation shows.
    pub(crate) fn observe(&mut self, obs: &ObservedFile<'_>) {
        let mut retained = Vec::with_capacity(self.covers.len());
        for cover in self.covers.drain(..) {
            match cover.revalidate(obs) {
                CoverVerdict::Keep => retained.push(cover),
                CoverVerdict::Grow(bytes) => {
                    let mut grown = cover;
                    grown.extent = grown.offset + bytes.len() as u64;
                    grown.digest = digest_of(&bytes);
                    grown.bytes = bytes;
                    retained.push(grown);
                }
                CoverVerdict::Invalidate => {}
            }
        }
        self.covers = retained;
    }

    /// Retire covers whose record the fold just reached: in the same
    /// generation, every cover at or before the consumed offset is spent (the
    /// record it covers was consumed — by any member as any record type — or
    /// its position was taken by a different record). Covers of another
    /// generation were already dropped by [`Self::observe`].
    pub(crate) fn note_consumed(&mut self, generation: u64, offset: u64) {
        self.covers
            .retain(|cover| cover.generation == generation && offset < cover.offset);
    }

    /// Whether a folded record is suppressed by a live cover of `agent`:
    /// a complete record only with the same generation/offset/digest; a
    /// pending-tail record at its exact offset whose observed bytes are a
    /// byte-for-byte prefix of the completed record. The started fold already
    /// guarantees record type and agent identity. CRLF is normalised here: the
    /// folded line has its terminating CR stripped, while a pending cover
    /// grown to the newline keeps the raw CR immediately before it.
    pub(crate) fn settles(
        &self,
        agent: &str,
        generation: u64,
        offset: u64,
        digest: &Digest,
        line: &[u8],
    ) -> bool {
        self.covers.iter().any(|cover| {
            cover.agent == agent
                && cover.generation == generation
                && cover.offset == offset
                && match cover.kind {
                    CoverKind::CompleteRecord => cover.digest == *digest,
                    CoverKind::PendingTail => {
                        let prefix = cover
                            .bytes
                            .strip_suffix(b"\r")
                            .unwrap_or(cover.bytes.as_slice());
                        line.starts_with(prefix)
                    }
                }
        })
    }
}

#[cfg(test)]
mod cover_table_tests {
    use super::*;

    const FILE: FileId = FileId { dev: 7, ino: 99 };
    const OTHER: FileId = FileId { dev: 7, ino: 100 };
    const W: u64 = 100;

    fn complete() -> Cover {
        Cover::complete("a1", FILE, 1, W, br#"{"type":"started","agentId":"a1"}"#)
    }

    fn pending(prefix: &[u8], extent: u64) -> Cover {
        Cover::pending("a1", FILE, 1, W, extent, prefix.to_vec())
    }

    fn obs_at(file_id: FileId, generation: u64, rest_base: u64, rest: &[u8]) -> ObservedFile<'_> {
        ObservedFile {
            file_id,
            generation,
            len: rest_base + rest.len() as u64,
            rest_base,
            rest,
        }
    }

    /// An observation identical to arm time: the whole covered range readable
    /// and byte-identical.
    fn unchanged_complete_view() -> Vec<u8> {
        let mut rest = vec![b'x'; W as usize];
        rest.extend_from_slice(br#"{"type":"started","agentId":"a1"}"#);
        rest
    }

    fn unchanged_pending_view(prefix: &[u8]) -> Vec<u8> {
        let mut rest = vec![b'x'; W as usize];
        rest.extend_from_slice(prefix);
        rest
    }

    #[test]
    fn cover_kind_x_observation_table() {
        let record = br#"{"type":"started","agentId":"a1"}"#;

        // (cover, observation, survives after observe, grown bytes)
        struct Row {
            name: &'static str,
            cover: Cover,
            view: Vec<u8>,
            view_base: u64,
            view_gen: u64,
            view_id: FileId,
            expect: Option<Vec<u8>>, // None = invalidated; Some(bytes) = live
        }

        let grown_prefix = br#"{"type":"started","agentId":"a1""#.to_vec();
        let mut grown_view = vec![b'x'; W as usize];
        grown_view.extend_from_slice(&grown_prefix);

        let mut completed_view = vec![b'x'; W as usize];
        completed_view.extend_from_slice(br#"{"type":"started","agentId":"a1"}"#);
        completed_view.push(b'\n');
        let completed_bytes = record.to_vec();

        let mut rewritten_view = unchanged_complete_view();
        rewritten_view[W as usize + 5] = b'X';

        let mut shorter_view = unchanged_complete_view();
        shorter_view.truncate(W as usize + 3);

        let mut one_byte_view = vec![b'x'; W as usize];
        one_byte_view.push(b'{');

        let mut pending_rewrite = unchanged_pending_view(br#"{"#);
        pending_rewrite[W as usize] = b'}';

        let rows = [
            Row {
                name: "complete unchanged keeps",
                cover: complete(),
                view: unchanged_complete_view(),
                view_base: 0,
                view_gen: 1,
                view_id: FILE,
                expect: Some(record.to_vec()),
            },
            Row {
                name: "complete appended bytes keeps (no growth)",
                cover: complete(),
                view: completed_view.clone(),
                view_base: 0,
                view_gen: 1,
                view_id: FILE,
                expect: Some(record.to_vec()),
            },
            Row {
                name: "complete rewritten byte invalidates",
                cover: complete(),
                view: rewritten_view,
                view_base: 0,
                view_gen: 1,
                view_id: FILE,
                expect: None,
            },
            Row {
                name: "complete shrunk inside its record invalidates",
                cover: complete(),
                view: shorter_view,
                view_base: 0,
                view_gen: 1,
                view_id: FILE,
                expect: None,
            },
            Row {
                name: "complete shrunk to one byte invalidates",
                cover: complete(),
                view: one_byte_view,
                view_base: 0,
                view_gen: 1,
                view_id: FILE,
                expect: None,
            },
            Row {
                name: "complete replaced file invalidates",
                cover: complete(),
                view: unchanged_complete_view(),
                view_base: 0,
                view_gen: 1,
                view_id: OTHER,
                expect: None,
            },
            Row {
                name: "complete new generation invalidates",
                cover: complete(),
                view: unchanged_complete_view(),
                view_base: 0,
                view_gen: 2,
                view_id: FILE,
                expect: None,
            },
            Row {
                name: "pending unchanged keeps",
                cover: pending(br#"{"#, W + 1),
                view: unchanged_pending_view(br#"{"#),
                view_base: 0,
                view_gen: 1,
                view_id: FILE,
                expect: Some(br#"{"#.to_vec()),
            },
            Row {
                name: "pending grows with the same write",
                cover: pending(br#"{"#, W + 1),
                view: grown_view.clone(),
                view_base: 0,
                view_gen: 1,
                view_id: FILE,
                expect: Some(grown_prefix.clone()),
            },
            Row {
                name: "pending growth ends at the completing newline",
                cover: pending(br#"{"#, W + 1),
                view: completed_view,
                view_base: 0,
                view_gen: 1,
                view_id: FILE,
                expect: Some(completed_bytes),
            },
            Row {
                name: "pending shrink below grown extent invalidates",
                cover: {
                    let mut c = pending(br#"{"#, W + 1);
                    c.bytes = grown_prefix.clone();
                    c.extent = W + grown_prefix.len() as u64;
                    c.digest = digest_of(&grown_prefix);
                    c
                },
                view: unchanged_pending_view(br#"{"#),
                view_base: 0,
                view_gen: 1,
                view_id: FILE,
                expect: None,
            },
            Row {
                name: "pending prefix rewritten invalidates",
                cover: pending(br#"{"#, W + 1),
                view: pending_rewrite,
                view_base: 0,
                view_gen: 1,
                view_id: FILE,
                expect: None,
            },
            Row {
                name: "pending replaced file invalidates",
                cover: pending(br#"{"#, W + 1),
                view: unchanged_pending_view(br#"{"#),
                view_base: 0,
                view_gen: 1,
                view_id: OTHER,
                expect: None,
            },
            Row {
                name: "pending new generation invalidates",
                cover: pending(br#"{"#, W + 1),
                view: unchanged_pending_view(br#"{"#),
                view_base: 0,
                view_gen: 2,
                view_id: FILE,
                expect: None,
            },
        ];

        for row in rows {
            let mut set = CoverSet::new();
            set.arm("a1", vec![row.cover.clone()]);
            set.observe(&obs_at(row.view_id, row.view_gen, row.view_base, &row.view));
            match row.expect {
                Some(bytes) => {
                    let live: Vec<&Cover> = set.for_agent("a1").collect();
                    assert_eq!(live.len(), 1, "row {}: cover must survive", row.name);
                    assert_eq!(live[0].bytes, bytes, "row {}: covered bytes", row.name);
                }
                None => assert!(
                    set.for_agent("a1").next().is_none(),
                    "row {}: cover must be invalidated",
                    row.name
                ),
            }
        }
    }

    #[test]
    fn invalidating_a_pending_tail_keeps_the_independent_complete_cover() {
        // The r9 item 2 fallback: trailing "{" after a complete start arms a
        // pending tail at W+49 and a complete record at W. Truncating the "{"
        // drops only the pending cover.
        let record = br#"{"type":"started","agentId":"a1"}"#;
        let z = W + record.len() as u64 + 1;
        let mut set = CoverSet::new();
        set.arm("a1", vec![complete(), pending(br#"{"#, z + 1)]);

        let mut view = vec![b'x'; W as usize];
        view.extend_from_slice(record);
        view.push(b'\n'); // file truncated back to Z: the "{" is gone
        set.observe(&obs_at(FILE, 1, 0, &view));

        let live: Vec<&Cover> = set.for_agent("a1").collect();
        assert_eq!(live.len(), 1, "only the complete fallback survives");
        assert_eq!(live[0].kind, CoverKind::CompleteRecord);
    }

    #[test]
    fn consumption_retires_covers_by_kind_and_position() {
        for kind in [CoverKind::CompleteRecord, CoverKind::PendingTail] {
            let mut set = CoverSet::new();
            let cover = match kind {
                CoverKind::CompleteRecord => complete(),
                CoverKind::PendingTail => pending(br#"{"#, W + 1),
            };
            set.arm("a1", vec![cover]);

            // A record consumed before the cover leaves it armed.
            set.note_consumed(1, W - 1);
            assert_eq!(set.for_agent("a1").count(), 1);
            // Consumed AT the cover's offset — any record type, any member —
            // retires it.
            set.note_consumed(1, W);
            assert_eq!(set.for_agent("a1").count(), 0, "{kind:?} retires at offset");

            // Re-arm and retire via a later position.
            let cover = match kind {
                CoverKind::CompleteRecord => complete(),
                CoverKind::PendingTail => pending(br#"{"#, W + 1),
            };
            set.arm("a1", vec![cover]);
            set.note_consumed(1, W + 50);
            assert_eq!(
                set.for_agent("a1").count(),
                0,
                "{kind:?} retires past offset"
            );
        }
    }

    #[test]
    fn suppression_requires_identity_and_bytes() {
        let record = br#"{"type":"started","agentId":"a1"}"#;
        let digest = digest_of(record);
        let mut set = CoverSet::new();
        set.arm(
            "a1",
            vec![
                complete(),
                pending(br#"{"type":"started","agentId"#, W + 27),
            ],
        );

        // Complete: exact digest at the same generation/offset. A record that
        // merely extends the covered record is a different digest.
        assert!(set.settles("a1", 1, W, &digest, record));
        // Different member, generation or offset never settles.
        assert!(!set.settles("a2", 1, W, &digest, record));
        assert!(!set.settles("a1", 2, W, &digest, record));
        assert!(!set.settles("a1", 1, W + 1, &digest, record));

        // Pending: the completed record must continue the captured prefix.
        let completed = br#"{"type":"started","agentId":""}"#;
        assert!(set.settles("a1", 1, W, &digest_of(completed), completed));
        let different = br#"{"type":"result","agentId":"a9"}"#;
        assert!(!set.settles("a1", 1, W, &digest_of(different), different));
    }

    #[test]
    fn pending_growth_keeps_a_cr_but_settles_the_crlf_completed_record() {
        // Growth observed the CR immediately before the not-yet-read newline,
        // so the stored raw bytes end in CR and the extent covers it. The fold
        // strips the terminating CR from the line; CRLF is normalised only at
        // this match, so the completed CRLF record still settles.
        let record = br#"{"type":"started","agentId":"a1"}"#;
        let mut grown = record.to_vec();
        grown.push(b'\r');
        let mut set = CoverSet::new();
        set.arm(
            "a1",
            vec![Cover::pending(
                "a1",
                FILE,
                1,
                W,
                W + grown.len() as u64,
                grown,
            )],
        );
        assert!(set.settles("a1", 1, W, &digest_of(record), record));

        // A completion that diverges from the captured raw prefix never
        // matches, CR or no CR.
        let divergent = br#"{"type":"failed","agentId":"a1"}"#;
        assert!(!set.settles("a1", 1, W, &digest_of(divergent), divergent));
    }
}
