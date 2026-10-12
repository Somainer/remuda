//! External event sources and JSONL file tails.

use crate::Error;
use crate::envelope::Envelope;
use crate::util::digest_of;
use remuda_protocol::{
    Digest, DriverKind, FileCursor, HostId, Id, InstanceId, RunId, SourceChannel, SourceDelivery,
    U64,
};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Adapter version recorded on envelopes produced by this crate.
pub const ADAPTER_VERSION: &str = "0.1.0";

/// Maps a native record into one or more envelopes.
pub trait Source {
    /// Stable adapter name for logs.
    fn name(&self) -> &'static str;

    /// Map one complete JSONL line plus its file cursor.
    fn map_line(&mut self, line: &[u8], cursor: FileCursor) -> Result<Vec<Envelope>, Error>;
}

/// Fields copied onto every envelope a tailer emits.
#[derive(Debug, Clone)]
pub struct MapContext {
    /// Instance these observations belong to.
    pub instance_id: InstanceId,
    /// Journal identity for the instance.
    pub journal_id: Id,
    /// Observing host.
    pub host_id: HostId,
    /// Node process generation.
    pub process_generation: U64,
    /// Attached run, if any.
    pub run_id: Option<RunId>,
    /// Attached run generation, if any.
    pub run_generation: Option<U64>,
    /// Driver that owns the native session.
    pub driver_kind: DriverKind,
    /// Driver implementation version.
    pub driver_version: String,
    /// Mapper / adapter version.
    pub adapter_version: String,
    /// Native session id (Claude UUID, etc.).
    pub native_session_id: String,
    /// Channel this tailer reads.
    pub channel: SourceChannel,
    /// Whether these records are live or replayed.
    pub delivery: SourceDelivery,
    /// Node-supplied stager for image content blocks a tool result carries
    /// (D-045 §6.2). Without one every image degrades to a text block naming
    /// the media type and byte count; bytes are never inlined.
    pub media_stager: Option<std::sync::Arc<dyn remuda_protocol::ToolMediaStager>>,
}

impl MapContext {
    /// Context for a Claude transcript or Workflow journal tailer.
    pub fn claude_file(
        instance_id: InstanceId,
        journal_id: Id,
        host_id: HostId,
        native_session_id: impl Into<String>,
        channel: SourceChannel,
    ) -> Self {
        Self {
            instance_id,
            journal_id,
            host_id,
            process_generation: U64(1),
            run_id: None,
            run_generation: None,
            driver_kind: DriverKind::ClaudePrint,
            driver_version: "unknown".into(),
            adapter_version: ADAPTER_VERSION.into(),
            native_session_id: native_session_id.into(),
            channel,
            delivery: SourceDelivery::Live,
            media_stager: None,
        }
    }

    /// Attach the Node's tool-media stager (host-token object endpoint);
    /// `None` leaves image blocks degrading to text.
    #[must_use]
    pub fn with_media_stager(
        mut self,
        stager: Option<std::sync::Arc<dyn remuda_protocol::ToolMediaStager>>,
    ) -> Self {
        self.media_stager = stager;
        self
    }
}

/// Durable JSONL read cursor persisted in SQLite.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceResume {
    /// Stable key, typically the absolute path.
    pub source_key: String,
    /// Assigned file identity (`obj_` UUIDv7).
    pub file_identity: Id,
    /// Increments on truncate or prefix rewrite.
    pub generation: U64,
    /// Byte offset of the next unread byte.
    pub offset: U64,
    /// Digest of the already-consumed prefix (up to 4096 bytes).
    pub prefix_digest: Option<Digest>,
}

/// Byte-offset JSONL tail with rotation / prefix-rewrite detection.
#[derive(Debug, Clone)]
pub struct FileTail {
    path: PathBuf,
    identity: Id,
    generation: u64,
    offset: u64,
    prefix_digest: Option<Digest>,
    /// Length of the file at the most recent open by [`Self::poll`] or
    /// [`Self::unread_start`]; `None` until one has succeeded. A failure
    /// watermark armed by the peek remembers the extent it observed, so a
    /// later open can tell that unread bytes were discarded by a truncate
    /// that did NOT bump the generation (len stayed at/after the cursor).
    last_len: Option<u64>,
    /// Device/inode identity of the file at the most recent open, so a
    /// replacement (rename/new file at the same path) is distinguishable from
    /// an in-place truncate even when sizes and generations coincide.
    file_id: Option<FileId>,
    /// What the most recent open observed relative to the previous one. Every
    /// poll and failure peek refreshes this BEFORE records are folded, so the
    /// caller can invalidate armed covers the moment the file contradicts
    /// them.
    last_observation: Option<FileObservation>,
    /// Unread bytes from the read cursor captured by the most recent open
    /// (BEFORE that open's poll advances the cursor); used to verify a
    /// pending boundary's observed prefix byte-for-byte.
    last_rest_base: u64,
    last_rest: Vec<u8>,
}

/// Stable kernel identity of one opened file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileId {
    /// Device id.
    pub dev: u64,
    /// Inode number.
    pub ino: u64,
}

/// What the most recent open of the journal showed relative to the open
/// before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileObservation {
    /// The path now resolves to a DIFFERENT file (device/inode changed): a
    /// rename or replacement since the previous open.
    pub replaced: bool,
    /// The file is strictly shorter than at the previous open (unread bytes
    /// may have been discarded above the cursor without a rotation).
    pub shrunk: bool,
    /// Length of the opened file.
    pub len: u64,
    /// Identity of the opened file.
    pub file_id: Option<FileId>,
}

/// The NEWEST complete `started` record a live-failure peek finds in the
/// still-unread tail, plus the file extent observed while finding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadStart {
    /// File generation the record belongs to (normalised against the open
    /// file by the peek).
    pub generation: u64,
    /// Byte offset of the record's first byte, in the normalised generation.
    pub offset: u64,
    /// Digest of the record bytes — the same digest [`Self::poll`] puts on
    /// the folded `FileCursor`, so the watermark can bind to the exact
    /// record rather than to an offset alone.
    pub digest: Digest,
    /// File length at peek time. The record's bytes live below this extent;
    /// a later same-generation file no longer than `offset` discarded them.
    pub extent: u64,
    /// Kernel identity of the file the record was observed in.
    pub file_id: FileId,
}

/// A trailing record still TORN INSIDE its bytes at peek time. Its type
/// and/or agent identity may not be written yet; both are validated against
/// the COMPLETED record when the fold consumes it (it only covers a start of
/// the failing member at its exact start offset).
///
/// The boundary binds to the exact bytes observed (`prefix` over
/// `offset..extent`): a later file state must keep that prefix byte-for-byte
/// and must never have shrunk below `extent`, or the completion belongs to a
/// different write and the boundary is invalidated. When the trailing record
/// is still unresolved but an EARLIER complete `started` for the same agent
/// sits in the unread tail, that record is kept as a digest-bound `fallback`
/// so the failure stays settled even if the trailing record completes as
/// something other than the member's start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingBoundary {
    /// File generation the partial record is being written in.
    pub generation: u64,
    /// Byte offset of the partial record's first byte.
    pub offset: u64,
    /// File length observed at peek time.
    pub extent: u64,
    /// Kernel identity of the file the partial bytes were observed in.
    pub file_id: FileId,
    /// Raw bytes observed at `offset..extent`; the completion must continue
    /// exactly these bytes.
    pub prefix: Vec<u8>,
    /// Newest complete matching `started` earlier in the same peek, kept
    /// while the trailing record's type/identity is unresolved.
    pub fallback: Option<UnreadStart>,
}

/// What a live-failure peek found at the newest unread position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnreadCover {
    /// A complete `started` record on disk (terminated, or awaiting only its
    /// newline). Bound by record digest.
    Complete(UnreadStart),
    /// A trailing record captured mid-write; type and agent identity are
    /// validated against the completed JSON when it folds.
    Pending(PendingBoundary),
}

impl UnreadCover {
    /// Generation the covered record belongs to.
    pub fn generation(&self) -> u64 {
        match self {
            UnreadCover::Complete(mark) => mark.generation,
            UnreadCover::Pending(boundary) => boundary.generation,
        }
    }

    /// Start offset of the covered record.
    pub fn offset(&self) -> u64 {
        match self {
            UnreadCover::Complete(mark) => mark.offset,
            UnreadCover::Pending(boundary) => boundary.offset,
        }
    }

    /// File extent observed while arming the cover.
    pub fn extent(&self) -> u64 {
        match self {
            UnreadCover::Complete(mark) => mark.extent,
            UnreadCover::Pending(boundary) => boundary.extent,
        }
    }

    /// Kernel file identity the cover was captured against.
    pub fn file_id(&self) -> FileId {
        match self {
            UnreadCover::Complete(mark) => mark.file_id,
            UnreadCover::Pending(boundary) => boundary.file_id,
        }
    }
}

/// Completed JSONL lines with their file cursors, as a poll folds them.
type PolledLines = Vec<(Vec<u8>, FileCursor)>;

/// An opened journal after rotation normalisation: `rest` is the unread
/// content from the effective cursor in the effective generation.
struct OpenedTail {
    file: File,
    len: u64,
    rest: Vec<u8>,
    file_id: FileId,
}

impl FileTail {
    /// Start at offset 0 with a fresh file identity.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, Error> {
        Ok(Self {
            path: path.into(),
            identity: Id::new("obj")?,
            generation: 1,
            offset: 0,
            prefix_digest: None,
            last_len: None,
            file_id: None,
            last_observation: None,
            last_rest_base: 0,
            last_rest: Vec::new(),
        })
    }

    /// Start at the current end of the file: only bytes appended after this
    /// tail is created are ever read. For following an already-long shared
    /// file (e.g. the main session transcript) for a single new marker.
    pub fn at_end(path: impl Into<PathBuf>) -> Result<Self, Error> {
        let path = path.into();
        let offset = match File::open(&path) {
            Ok(file) => file.metadata()?.len(),
            // A not-yet-created file starts at zero and begins reading once
            // the file appears.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => 0,
            Err(err) => return Err(err.into()),
        };
        Ok(Self {
            path,
            identity: Id::new("obj")?,
            generation: 1,
            offset,
            prefix_digest: None,
            last_len: None,
            file_id: None,
            last_observation: None,
            last_rest_base: 0,
            last_rest: Vec::new(),
        })
    }

    /// Resume from a persisted cursor.
    pub fn from_resume(path: impl Into<PathBuf>, resume: SourceResume) -> Self {
        Self {
            path: path.into(),
            identity: resume.file_identity,
            generation: resume.generation.0.max(1),
            offset: resume.offset.0,
            prefix_digest: resume.prefix_digest,
            last_len: None,
            file_id: None,
            last_observation: None,
            last_rest_base: 0,
            last_rest: Vec::new(),
        }
    }

    /// Path being tailed.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Current file identity.
    pub fn identity(&self) -> &Id {
        &self.identity
    }

    /// Current generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Next unread byte offset.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Find the NEWEST unread `started` for `agent_id` as an [`UnreadCover`]
    /// — or `None`. A terminated record, or a trailing newline-free segment
    /// that already parses whole, is a digest-bound [`UnreadCover::Complete`];
    /// a trailing segment torn while its type/identity could still resolve to
    /// this member's start arms an [`UnreadCover::Pending`] boundary carrying
    /// the exact observed prefix bytes; type and identity are validated
    /// against the completed record when it folds.
    ///
    /// A Pending boundary also carries the newest digest-bound COMPLETE match
    /// earlier in the same tail as a fallback, so an unresolved trailing
    /// record never costs the failure coverage the earlier record provides.
    /// Matches on `agentId`, falling back to `key` for complete records that
    /// omit it.
    ///
    /// Does NOT consume records, but it DOES apply the same rotation
    /// normalisation against the opened file as [`Self::poll`] (a
    /// replace/truncate or prefix rewrite since the last poll starts a fresh
    /// generation here too), so positions are reported in the generation the
    /// next fold will use. A live StopFailure uses the result as a terminal
    /// watermark: only starts at or before that position, and at a Complete
    /// position only with the SAME record digest, are the failure's own
    /// settled attempt; a start at a later offset is a genuine retry and must
    /// resurrect the member even when both fold in the same poll.
    ///
    /// Completeness agrees with [`Self::poll`] on every terminated record, but
    /// the peek ALSO covers records the poll cannot consume yet: a complete
    /// record awaiting only its newline (Complete), and a record torn inside
    /// its JSON (Pending). [`Self::poll`] consumes nothing on the first polls,
    /// so the caller keeps the cover armed through those empty polls until the
    /// record is actually consumed. Skipping the incomplete tail here would
    /// leave a StopFailure arriving mid-write unprotected.
    ///
    /// The opened file's identity, extent and bytes are remembered so every
    /// later open (poll or another failure peek) can invalidate the cover the
    /// moment a replacement, shrink or rewrite contradicts it (see
    /// [`Self::cover_is_valid`]).
    pub fn unread_start(&mut self, agent_id: &str) -> Result<Option<UnreadCover>, Error> {
        // Normalise against the OPENED file (a replace/truncate since the
        // last poll starts a new generation here, not only later in poll), so
        // the watermark binds to the generation its record will actually fold
        // in.
        let Some(OpenedTail {
            rest, len, file_id, ..
        }) = self.open_normalised()?
        else {
            return Ok(None);
        };
        let mut newest: Option<UnreadStart> = None;
        let mut start = 0usize;
        let record_start = |line: &[u8], at: usize, newest: &mut Option<UnreadStart>| {
            if !line.is_empty()
                && let Ok(value) = serde_json::from_slice::<serde_json::Value>(line)
                && value.get("type").and_then(serde_json::Value::as_str) == Some("started")
                && value
                    .get("agentId")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| value.get("key").and_then(serde_json::Value::as_str))
                    .is_some_and(|id| id == agent_id)
            {
                *newest = Some(UnreadStart {
                    generation: self.generation,
                    offset: self.offset + at as u64,
                    digest: digest_of(line),
                    extent: len,
                    file_id,
                });
            }
        };
        for (i, byte) in rest.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            let mut line = &rest[start..i];
            if let Some(without_cr) = line.strip_suffix(b"\r") {
                line = without_cr;
            }
            record_start(line, start, &mut newest);
            start = i + 1;
        }
        // Trailing newline-free segment: either a complete record awaiting
        // only its newline, or a record torn inside its JSON (Pending) —
        // including a tear BEFORE the type/identity values resolve.
        if start < rest.len() {
            let raw = &rest[start..];
            let mut line = raw;
            if let Some(without_cr) = line.strip_suffix(b"\r") {
                line = without_cr;
            }
            if serde_json::from_slice::<serde_json::Value>(line).is_ok() {
                record_start(line, start, &mut newest);
            } else if partial_started_prefix(line, agent_id) {
                return Ok(Some(UnreadCover::Pending(PendingBoundary {
                    generation: self.generation,
                    offset: self.offset + start as u64,
                    extent: len,
                    file_id,
                    prefix: raw.to_vec(),
                    // The trailing record could still resolve to something
                    // other than the failing member's start; the newest
                    // complete match earlier in the tail remains covered.
                    fallback: newest,
                })));
            }
        }
        Ok(newest.map(UnreadCover::Complete))
    }

    /// What the most recent open by poll or peek observed relative to the
    /// open before it (replacement, shrink, length, identity).
    pub fn last_observation(&self) -> Option<FileObservation> {
        self.last_observation
    }

    /// Whether a cover armed at an earlier open is still consistent with the
    /// MOST RECENTLY observed file state. The caller invokes this BEFORE
    /// folding records on every poll and failure peek, and retires any cover
    /// it rejects, so a reused position can never be mistaken for the settled
    /// record:
    /// - a journal rotation (cover generation != current) retires it;
    /// - a replacement (different device/inode) retires it;
    /// - a Complete cover retires once an observed shrink removes the bytes
    ///   at its record start;
    /// - a Pending boundary retires the moment the file no longer reaches the
    ///   observed extent (any shrink through the captured partial bytes, even
    ///   one above the record start), or the captured prefix no longer matches
    ///   the file's bytes at its offset (rewrite in place).
    pub fn cover_is_valid(&self, cover: &UnreadCover) -> bool {
        let Some(obs) = self.last_observation else {
            return true;
        };
        if cover.generation() != self.generation {
            return false;
        }
        match cover {
            UnreadCover::Complete(mark) => !(obs.shrunk && obs.len <= mark.offset),
            UnreadCover::Pending(boundary) => {
                if obs.len < boundary.extent {
                    return false;
                }
                self.prefix_intact(boundary)
            }
        }
    }

    /// Whether the file bytes at the boundary's offset still equal the prefix
    /// captured when the pending boundary was armed.
    fn prefix_intact(&self, boundary: &PendingBoundary) -> bool {
        let Some(relative) = boundary.offset.checked_sub(self.last_rest_base) else {
            // Same generation but the window starts after the boundary; the
            // observed bytes no longer cover it.
            return false;
        };
        let relative = relative as usize;
        let end = relative.saturating_add(boundary.prefix.len());
        self.last_rest
            .get(relative..end)
            .is_some_and(|bytes| bytes == boundary.prefix)
    }

    /// Persistable cursor.
    pub fn resume(&self) -> SourceResume {
        SourceResume {
            source_key: self.path.to_string_lossy().into_owned(),
            file_identity: self.identity.clone(),
            generation: U64(self.generation),
            offset: U64(self.offset),
            prefix_digest: self.prefix_digest.clone(),
        }
    }

    /// Read every newly completed JSONL line. Incomplete trailing lines wait.
    pub fn poll(&mut self) -> Result<PolledLines, Error> {
        Ok(self.poll_with_covers(&[])?.0)
    }

    /// Like [`Self::poll`], but each supplied armed cover is validated against
    /// THIS open's file state (replacement, rotation, shrink through the
    /// captured bytes, rewritten prefix) BEFORE the cursor advances or any
    /// record is folded. The returned flags align with `covers` (`true` = the
    /// cover is contradicted and must be retired).
    pub fn poll_with_covers(
        &mut self,
        covers: &[UnreadCover],
    ) -> Result<(PolledLines, Vec<bool>), Error> {
        let Some(OpenedTail { mut file, rest, .. }) = self.open_normalised()? else {
            return Ok((Vec::new(), vec![false; covers.len()]));
        };
        let invalid: Vec<bool> = covers
            .iter()
            .map(|cover| !self.cover_is_valid(cover))
            .collect();
        let mut lines: PolledLines = Vec::new();
        let mut start = 0usize;
        for (i, byte) in rest.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            let mut line = &rest[start..i];
            if let Some(without_cr) = line.strip_suffix(b"\r") {
                line = without_cr;
            }
            if !line.is_empty() {
                let offset = self.offset + start as u64;
                lines.push((
                    line.to_vec(),
                    FileCursor {
                        file_identity: self.identity.clone(),
                        file_generation: U64(self.generation),
                        offset: U64(offset),
                        length: U64(line.len() as u64),
                        digest: digest_of(line),
                    },
                ));
            }
            start = i + 1;
        }
        self.offset += start as u64;
        self.refresh_prefix(&mut file)?;
        Ok((lines, invalid))
    }

    /// Open the journal and apply the SAME rotation / effective-cursor
    /// normalisation that the folding poll uses, before any unread bytes are
    /// located: a file shorter than the cursor, or one whose consumed prefix
    /// was rewritten, starts a new generation at offset 0. This is shared by
    /// the live-failure peek and [`Self::poll`] so a watermark armed from the
    /// peek binds to the generation its record will actually fold in, rather
    /// than to a stale cached generation/offset beyond EOF. Returns `None`
    /// when the file does not exist yet.
    fn open_normalised(&mut self) -> Result<Option<OpenedTail>, Error> {
        use std::os::unix::fs::MetadataExt;
        let mut file = match File::open(&self.path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
            Ok(file) => file,
        };
        let metadata = file.metadata()?;
        let len = metadata.len();
        let opened_id = FileId {
            dev: metadata.dev(),
            ino: metadata.ino(),
        };
        // Observe the file BEFORE normalisation: a replacement (different
        // device/inode) or an in-place shrink is recorded against the previous
        // open so armed covers can be retired before any record is folded.
        let shrunk = self.last_len.is_some_and(|prev| len < prev);
        self.detect_rotation(&mut file, len)?;
        let rest_base = self.offset;
        file.seek(SeekFrom::Start(rest_base))?;
        let mut rest = Vec::new();
        file.read_to_end(&mut rest)?;
        // Remember the observed extent and identity for the next open. The
        // rest window is anchored to the cursor BEFORE this open's poll
        // consumes it, so a cover can still be validated against the folded
        // bytes afterwards.
        self.last_len = Some(len);
        self.file_id = Some(opened_id);
        self.last_observation = Some(FileObservation {
            replaced: false,
            shrunk,
            len,
            file_id: Some(opened_id),
        });
        self.last_rest_base = rest_base;
        self.last_rest = rest.clone();
        Ok(Some(OpenedTail {
            file,
            len,
            rest,
            file_id: opened_id,
        }))
    }

    fn detect_rotation(&mut self, file: &mut File, len: u64) -> Result<(), Error> {
        if self.offset == 0 {
            return Ok(());
        }
        if len < self.offset {
            tracing::warn!(
                path = %self.path.display(),
                generation = self.generation,
                offset = self.offset,
                len,
                "jsonl truncated; starting new file generation"
            );
            self.rotate();
            return Ok(());
        }
        let check_len = std::cmp::min(self.offset, 4096) as usize;
        let mut buf = vec![0u8; check_len];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut buf)?;
        let digest = digest_of(&buf);
        if let Some(old) = &self.prefix_digest
            && old != &digest
        {
            tracing::warn!(
                path = %self.path.display(),
                generation = self.generation,
                "jsonl prefix rewritten; starting new file generation"
            );
            self.rotate();
        }
        Ok(())
    }

    fn rotate(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.offset = 0;
        self.prefix_digest = None;
    }

    fn refresh_prefix(&mut self, file: &mut File) -> Result<(), Error> {
        if self.offset == 0 {
            self.prefix_digest = None;
            return Ok(());
        }
        let n = std::cmp::min(self.offset, 4096) as usize;
        let mut buf = vec![0u8; n];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut buf)?;
        self.prefix_digest = Some(digest_of(&buf));
        Ok(())
    }
}

/// Whether a trailing segment that does NOT yet parse as JSON is worth a
/// pending boundary for `agent_id` — the beginning of a record whose type
/// and/or identity have not finished writing.
///
/// The boundary is captured as soon as the bytes could still become THIS
/// member's `started`: only a RESOLVED contradiction rejects the segment
/// (a complete `type` pair naming another record type, or a complete
/// `agentId` pair naming another agent). A tear anywhere — inside the type
/// value, before the identity key, before its colon, inside its value, or
/// even before any pair is written — leaves type/identity unresolved, so the
/// boundary is armed and both are validated against the completed JSON when
/// the record folds (a completion that resolves differently retires the
/// boundary without covering it).
fn partial_started_prefix(line: &[u8], agent_id: &str) -> bool {
    let mut i = line
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(line.len());
    if i >= line.len() || line[i] != b'{' {
        return false;
    }
    i += 1;
    let mut record_type: Option<String> = None;
    let mut agent_id_value: Option<String> = None;
    // Read `"key":"value"` pairs (all journal fields are strings) until the
    // object closes or the torn end stops a pair from being read whole.
    loop {
        while i < line.len() && line[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= line.len() || line[i] == b'}' {
            break;
        }
        if line[i] != b'"' {
            break;
        }
        let Some((pair_key, next)) = read_json_string(&line[i..]) else {
            // A key name still mid-write (e.g. `"agentI`): nothing after it
            // is resolved.
            break;
        };
        i += next;
        while i < line.len() && line[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= line.len() || line[i] != b':' {
            // The key is on disk but its colon/value never arrived.
            break;
        }
        i += 1;
        while i < line.len() && line[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= line.len() || line[i] != b'"' {
            // The value is the torn part (missing opening quote included).
            break;
        }
        let Some((value, next)) = read_json_string(&line[i..]) else {
            // The string value is still being written (a tear inside it, a
            // dangling escape, a missing closing quote).
            break;
        };
        i += next;
        match pair_key.as_str() {
            "type" => record_type = Some(value),
            "agentId" => agent_id_value = Some(value),
            _ => {}
        }
        while i < line.len() && line[i].is_ascii_whitespace() {
            i += 1;
        }
        match line.get(i) {
            Some(b',') => i += 1,
            Some(b'}') => break,
            _ => break,
        }
    }
    // Reject only a RESOLVED non-match; everything unresolved is validated
    // against the completed record on the fold.
    if let Some(record_type) = record_type.as_deref()
        && record_type != "started"
    {
        return false;
    }
    match agent_id_value {
        Some(value) => value == agent_id,
        None => true,
    }
}

/// Parse ONE JSON string (the slice begins at its opening quote), returning
/// its decoded value and the number of input bytes consumed through the
/// closing quote. `None` when the closing quote has not been written yet
/// (the record is torn). Escape sequences are followed so a backslash
/// inside the torn tail is not mistaken for termination.
fn read_json_string(input: &[u8]) -> Option<(String, usize)> {
    debug_assert_eq!(input.first(), Some(&b'"'));
    let mut value: Vec<u8> = Vec::new();
    let mut escaped = false;
    for (index, byte) in input.iter().enumerate().skip(1) {
        if escaped {
            value.push(*byte);
            escaped = false;
        } else if *byte == b'\\' {
            value.push(b'\\');
            escaped = true;
        } else if *byte == b'"' {
            return Some((String::from_utf8_lossy(&value).into_owned(), index + 1));
        } else {
            value.push(*byte);
        }
    }
    None
}

#[cfg(test)]
mod torn_start_tests {
    use super::partial_started_prefix;

    #[test]
    fn torn_around_identity_is_captured_until_completion() {
        // Full agentId, closing brace missing -> the failing agent's record.
        assert!(partial_started_prefix(
            br#"{"type":"started","key":"v2:k","agentId":"a1""#,
            "a1"
        ));
        // Torn INSIDE the agentId VALUE: identity unresolved, armed anyway.
        assert!(partial_started_prefix(
            br#"{"type":"started","key":"v2:k","agentId":"a"#,
            "a1"
        ));
        // Torn BEFORE the identity value's COLON (key complete, colon gone).
        assert!(partial_started_prefix(
            br#"{"type":"started","key":"v2:k","agentId""#,
            "a1"
        ));
        // Torn BEFORE the identity KEY (nothing of agentId written yet).
        assert!(partial_started_prefix(
            br#"{"type":"started","key":"v2:k","#,
            "a1"
        ));
        // Torn mid-key while the identity key begins.
        assert!(partial_started_prefix(
            br#"{"type":"started","key":"v2:k","agentI"#,
            "a1"
        ));
        // Torn inside the TYPE value: type unresolved, validated later.
        assert!(partial_started_prefix(br#"{"type":"starte"#, "a1"));
        // Torn before any pair at all.
        assert!(partial_started_prefix(br#"{"#, "a1"));
        // A complete DIFFERENT agentId is resolved not-ours.
        assert!(!partial_started_prefix(
            br#"{"type":"started","key":"v2:k","agentId":"a2""#,
            "a1"
        ));
        // A complete non-started type is resolved not-ours.
        assert!(!partial_started_prefix(
            br#"{"type":"failed","agentId":"a1""#,
            "a1"
        ));
        // Garbage that cannot begin a JSON object is never captured.
        assert!(!partial_started_prefix(br#"nonsense {"#, "a1"));
    }
}
