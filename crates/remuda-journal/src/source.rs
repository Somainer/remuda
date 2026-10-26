//! External event sources and JSONL file tails.

use crate::Error;
use crate::cover::{Cover, CoverSet, FileId, ObservedFile};
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
#[derive(Clone)]
pub struct FileTail {
    path: PathBuf,
    identity: Id,
    generation: u64,
    offset: u64,
    prefix_digest: Option<Digest>,
    /// Length of the file at the most recent open by [`Self::poll`] or
    /// [`Self::unread_start`]; `None` until one has succeeded. Used to detect
    /// an in-place truncate that discarded unread bytes without a rotation
    /// (len stayed at/after the cursor) and to revalidate armed covers.
    last_len: Option<u64>,
    /// Device/inode identity of the file at the most recent open, so a
    /// replacement (rename/new file at the same path) is distinguishable from
    /// an in-place truncate even when sizes and generations coincide.
    file_id: Option<FileId>,
    /// Independent failure-settlement covers armed by live-failure peeks.
    /// EVERY open by poll or peek revalidates the whole set against the
    /// opened file before any record is folded.
    covers: CoverSet,
    /// Test-only seam fired on EVERY open AFTER the stat/rotation check and
    /// BEFORE the unread bytes are read, so a test can deterministically
    /// append bytes inside the metadata()/read_to_end() window.
    #[cfg(test)]
    after_stat_before_read: Option<OpenGapHook>,
}

/// Completed JSONL lines with their file cursors, as a poll folds them.
type PolledLines = Vec<(Vec<u8>, FileCursor)>;

/// Test-only seam fired inside an open between the stat and the unread read.
#[cfg(test)]
type OpenGapHook = std::sync::Arc<dyn Fn(&Path) + Send + Sync>;

/// An opened journal after rotation normalisation: `rest` is the unread
/// content from the effective cursor in the effective generation.
struct OpenedTail {
    file: File,
    rest: Vec<u8>,
    file_id: FileId,
}

impl std::fmt::Debug for FileTail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileTail")
            .field("path", &self.path)
            .field("identity", &self.identity)
            .field("generation", &self.generation)
            .field("offset", &self.offset)
            .field("last_len", &self.last_len)
            .field("file_id", &self.file_id)
            .field("covers", &self.covers)
            .finish()
    }
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
            covers: CoverSet::new(),
            #[cfg(test)]
            after_stat_before_read: None,
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
            covers: CoverSet::new(),
            #[cfg(test)]
            after_stat_before_read: None,
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
            covers: CoverSet::new(),
            #[cfg(test)]
            after_stat_before_read: None,
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

    /// Find EVERY unread `started` for `agent_id` as an independent
    /// [`Cover`]: one complete-record cover per terminated (or
    /// newline-awaiting) complete `started`, plus one pending-tail cover when
    /// the trailing segment is torn while its type/identity could still
    /// resolve to this member.
    ///
    /// The open itself (like every poll open) revalidates the WHOLE cover
    /// set against the opened file FIRST, so covers another member's peek
    /// already contradicted are retired before this peek's fresh covers are
    /// armed; the returned covers REPLACE the member's previous covers (other
    /// members' covers are untouched).
    ///
    /// Does NOT consume records, but it applies the same rotation
    /// normalisation against the opened file as [`Self::poll`] (a replacement
    /// at the path, or a truncate/rewrite through the consumed cursor, starts a
    /// fresh generation here too), so positions are reported in the generation
    /// the next fold will use. A same-inode change confined to unread bytes at
    /// cursor 0 leaves the generation alone and is settled per cover instead. A live StopFailure uses the covers as terminal
    /// watermarks: a folded `started` is suppressed only when its identity and
    /// bytes match a live cover; a start appended AFTER the failure resurrects
    /// the member even when both fold in the same poll.
    ///
    /// Completeness agrees with [`Self::poll`] on every terminated record, but
    /// the peek ALSO covers records the poll cannot consume yet: a complete
    /// record awaiting only its newline, and a record torn inside its JSON.
    /// [`Self::poll`] consumes nothing on the first polls, so the caller keeps
    /// the covers armed through those empty polls until the record is actually
    /// consumed.
    pub(crate) fn unread_start(&mut self, agent_id: &str) -> Result<Vec<Cover>, Error> {
        // open_normalised revalidates the whole cover set against the OPENED
        // file (a replace/truncate since the last poll starts a new generation
        // here, not only later in poll), so the covers bind to the generation
        // their records will actually fold in.
        let Some(OpenedTail { rest, file_id, .. }) = self.open_normalised()? else {
            return Ok(Vec::new());
        };
        let mut covers: Vec<Cover> = Vec::new();
        let mut start = 0usize;
        let complete_at = |line: &[u8], at: usize, covers: &mut Vec<Cover>| {
            if !line.is_empty()
                && let Ok(value) = serde_json::from_slice::<serde_json::Value>(line)
                && value.get("type").and_then(serde_json::Value::as_str) == Some("started")
                && value
                    .get("agentId")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| value.get("key").and_then(serde_json::Value::as_str))
                    .is_some_and(|id| id == agent_id)
            {
                covers.push(Cover::complete(
                    agent_id,
                    file_id,
                    self.generation,
                    self.offset + at as u64,
                    line,
                ));
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
            complete_at(line, start, &mut covers);
            start = i + 1;
        }
        // Trailing newline-free segment: either a complete record awaiting
        // only its newline (a complete cover), or a record torn inside its
        // JSON (a pending cover) — including a tear BEFORE the type/identity
        // values resolve.
        if start < rest.len() {
            // The pending cover keeps the EXACT raw trailing bytes, including a
            // captured CR: its whole range must validate byte-for-byte, so
            // overwriting that CR later (e.g. with a quote that turns the
            // record into a different completion) contradicts the cover. CRLF
            // is stripped only for the completed-line / JSON match below.
            let raw = &rest[start..];
            let mut line = raw;
            if let Some(without_cr) = line.strip_suffix(b"\r") {
                line = without_cr;
            }
            if serde_json::from_slice::<serde_json::Value>(line).is_ok() {
                complete_at(line, start, &mut covers);
            } else if partial_started_prefix(line, agent_id) {
                covers.push(Cover::pending(
                    agent_id,
                    file_id,
                    self.generation,
                    self.offset + start as u64,
                    self.offset + rest.len() as u64,
                    raw.to_vec(),
                ));
            }
        }
        // Replace this member's covers with the fresh peek; every other
        // member's independently validated covers stays armed.
        let fresh = covers.clone();
        self.covers.arm(agent_id, covers);
        Ok(fresh)
    }

    /// The failure-settlement covers armed on this tail.
    pub(crate) fn covers(&self) -> &CoverSet {
        &self.covers
    }

    /// Test-only: install a hook fired inside each open between the stat and
    /// the unread-bytes read, so bytes appended in that window are read by the
    /// SAME open. Deterministic stand-in for the metadata/read race.
    #[cfg(test)]
    pub(crate) fn set_open_gap_hook(&mut self, hook: impl Fn(&Path) + Send + Sync + 'static) {
        self.after_stat_before_read = Some(std::sync::Arc::new(hook));
    }

    /// Test-only: remove the open-gap hook.
    #[cfg(test)]
    pub(crate) fn clear_open_gap_hook(&mut self) {
        self.after_stat_before_read = None;
    }

    /// Retire covers whose record the fold just reached (any member, any
    /// record type). Called by the workflow fold after EACH consumed line, in
    /// fold order, so a start appended after the covered record still
    /// resurrects in the same poll.
    pub(crate) fn note_consumed(&mut self, generation: u64, offset: u64) {
        self.covers.note_consumed(generation, offset);
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
    /// The open revalidates armed covers before any line is returned; the
    /// workflow fold retires them as it consumes each line via
    /// [`Self::note_consumed`].
    pub fn poll(&mut self) -> Result<PolledLines, Error> {
        let Some(OpenedTail { mut file, rest, .. }) = self.open_normalised()? else {
            return Ok(Vec::new());
        };
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
        Ok(lines)
    }

    /// Open the journal and apply the SAME rotation / effective-cursor
    /// normalisation that the folding poll uses, before any unread bytes are
    /// located: a replacement at the path, a file shorter than the cursor, or
    /// one whose consumed prefix was rewritten, starts a new generation at
    /// offset 0. A same-inode truncate/rewrite while the cursor is anchored at
    /// offset 0 does NOT rotate: the bytes are all unread and each armed cover
    /// revalidates its own range instead. This is shared by the live-failure
    /// peek and [`Self::poll`] so a watermark armed from the peek binds to the
    /// generation its record will actually fold in, rather than to a stale
    /// cached generation/offset beyond EOF. Returns `None` when the file does
    /// not exist yet.
    fn open_normalised(&mut self) -> Result<Option<OpenedTail>, Error> {
        use std::os::unix::fs::MetadataExt;
        let mut file = match File::open(&self.path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
            Ok(file) => file,
        };
        let metadata = file.metadata()?;
        let stat_len = metadata.len();
        let opened_id = FileId {
            dev: metadata.dev(),
            ino: metadata.ino(),
        };
        // Observe the file BEFORE normalisation: a replacement (different
        // device/inode) or an in-place shrink is recorded against the previous
        // open so detect_rotation (and the cover revalidation below) can act
        // before any record is folded.
        let replaced = self.file_id.is_some_and(|id| id != opened_id);
        self.detect_rotation(&mut file, stat_len, replaced)?;
        let rest_base = self.offset;
        // Test seam: bytes appended here land between the stat above and the
        // read below, i.e. inside the real metadata/read race window.
        #[cfg(test)]
        if let Some(hook) = &self.after_stat_before_read {
            hook(&self.path);
        }
        file.seek(SeekFrom::Start(rest_base))?;
        let mut rest = Vec::new();
        file.read_to_end(&mut rest)?;
        // Validation AND growth must use the extent of the byte SNAPSHOT just
        // read (rest_base + rest.len()), not the stat length: bytes appended
        // between metadata() and read_to_end() are in `rest` and pending tails
        // must grow over them in this observation, or a later shrink back to
        // the stat extent would be missed and a different retry wrongly
        // covered.
        let len = rest_base + rest.len() as u64;
        // Remember the observed extent and identity for the next open.
        self.last_len = Some(len);
        self.file_id = Some(opened_id);
        // Revalidate (and grow pending tails of) EVERY armed cover against
        // THIS open before poll folds or peek re-arms. Poll and peek share
        // this path, so a cover is contradicted the first open that sees it,
        // whichever member triggered the open.
        self.covers.observe(&ObservedFile {
            file_id: opened_id,
            generation: self.generation,
            len,
            rest_base,
            rest: &rest,
        });
        Ok(Some(OpenedTail {
            file,
            rest,
            file_id: opened_id,
        }))
    }

    fn detect_rotation(&mut self, file: &mut File, len: u64, replaced: bool) -> Result<(), Error> {
        // A different file at the path (rename/replace) starts a fresh
        // generation even when the cursor sits at offset 0, so a replacement
        // can never reuse the generation a failure peek armed.
        if replaced {
            tracing::warn!(
                path = %self.path.display(),
                generation = self.generation,
                "jsonl replaced at path; starting new file generation"
            );
            self.rotate();
            return Ok(());
        }
        // No blanket rotation when the cursor is anchored at offset 0 (the
        // typical state right after a failure peek normalised a replacement):
        // an in-place truncate/rewrite confined to UNREAD bytes of the SAME
        // inode must not bump the generation, or a change elsewhere in the
        // unread window (e.g. truncating only a lone trailing brace) would
        // discard every independently verified cover, including an intact
        // complete record before the changed bytes. Each cover revalidates its
        // OWN byte range in covers.observe below, so a same-inode shrink or
        // rewrite retires exactly the covers it contradicts. A replacement
        // (different inode, handled above) still starts a fresh generation.
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
/// (the record is torn) or the span is not yet a valid JSON string (e.g. a
/// dangling escape). Escapes (`\"`, `\\`, `\/`, `\b\f\n\r\t`, `\uXXXX`
/// including surrogate pairs) are decoded EXACTLY as serde_json would, so an
/// agentId whose on-disk JSON escapes a quote (raw a-backslash-quote-1,
/// decoded a-quote-1) or encodes a character with the backslash-u form
/// matches the member's real id when the failure peek compares it.
fn read_json_string(input: &[u8]) -> Option<(String, usize)> {
    debug_assert_eq!(input.first(), Some(&b'"'));
    // Find the raw token span: from the opening quote through the first
    // UNESCAPED closing quote. A backslash only escapes the single following
    // byte while scanning, so a quote in the torn tail after a dangling
    // backslash is never mistaken for the end.
    let mut escaped = false;
    let mut end = None;
    for (index, byte) in input.iter().enumerate().skip(1) {
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b'"' {
            end = Some(index + 1);
            break;
        }
    }
    let end = end?;
    // Delegate the actual decoding to serde_json so every escape — including
    // \uXXXX and surrogate pairs — comes out byte-identical to the completed
    // record the fold will parse. A malformed span (bad escape, lone
    // surrogate) simply has not finished writing yet.
    let value = serde_json::from_slice::<String>(&input[..end]).ok()?;
    Some((value, end))
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

    #[test]
    fn a_torn_record_with_an_escaped_agent_id_is_armed_for_that_member() {
        // r9 item 6: the raw on-disk agentId is a\"1; the member's real id is
        // a"1. The escapes must be decoded as serde_json decodes them before
        // the peek compares, or no cover is armed and the settled attempt
        // resurrects.
        assert!(partial_started_prefix(
            br#"{"type":"started","key":"v2:k","agentId":"a\"1""#,
            "a\"1"
        ));
        // A backslash-u escape decodes exactly like serde_json: the written
        // digits spell '1'.
        let unicode_torn = concat!(
            "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a",
            "\\u0031",
            "\""
        );
        assert!(partial_started_prefix(unicode_torn.as_bytes(), "a1"));
        // A surrogate pair decodes to its scalar.
        let surrogate_torn = concat!(
            "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a",
            "\\uD83D\\uDE00",
            "\""
        );
        assert!(partial_started_prefix(
            surrogate_torn.as_bytes(),
            "a\u{1F600}"
        ));
        // The short escapes \/ \b \f \n \r \t decode to their characters.
        assert!(partial_started_prefix(
            br#"{"type":"started","key":"v2:k","agentId":"a\/b\nc""#,
            "a/b\nc"
        ));
        // A fully written, escaped DIFFERENT id is still resolved not-ours.
        assert!(!partial_started_prefix(
            br#"{"type":"started","key":"v2:k","agentId":"a\"2""#,
            "a\"1"
        ));
        // A tear inside the escape (dangling backslash) is unresolved and
        // armed anyway; completion validates the real id.
        assert!(partial_started_prefix(
            br#"{"type":"started","key":"v2:k","agentId":"a1\"#,
            "a1"
        ));
        // A type value whose decoded content is not exactly "started"
        // (escapes that make it `"failed"` with literal quotes) rejects.
        assert!(!partial_started_prefix(
            br#"{"type":"\"failed\"","key":"v2:k","agentId":"a1""#,
            "a1"
        ));
    }
}

#[cfg(test)]
mod open_snapshot_tests {
    use super::*;
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    const PREFIX: &[u8] = br#"{"type":"started","key":"v2:k","agentId":"a1"}
{"type":"failed","key":"v2:k","agentId":"a1"}
"#;

    /// c-cardsettle-fu2 r10 item 3: bytes appended between metadata() and
    /// read_to_end() of one open are in the byte snapshot, so a pending cover
    /// must GROW over them using rest_base + rest.len(), not the earlier stat
    /// length. With growth recorded, the writer shrinking back to the armed
    /// extent invalidates the cover and the later different retry is not
    /// settled by it.
    #[test]
    fn bytes_appended_between_stat_and_read_grow_the_pending_cover() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        std::fs::write(&path, PREFIX).unwrap();
        let w = PREFIX.len() as u64;

        let mut tail = FileTail::new(&path).unwrap();
        assert_eq!(tail.poll().unwrap().len(), 2, "prefix consumed");
        assert_eq!(tail.offset(), w);

        // The failure peek arms a pending cover over the lone brace.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{")
            .unwrap();
        assert_eq!(tail.unread_start("a1").unwrap().len(), 1);

        // Next open: the stat still sees W+1, but the continuation is appended
        // inside the stat/read window and read by the SAME open.
        let grown = br#""type":"started","key":"v2:k","agentId":"a1""#;
        let fired = Arc::new(AtomicBool::new(false));
        let fired_in = fired.clone();
        tail.set_open_gap_hook(move |path| {
            if !fired_in.swap(true, Ordering::SeqCst) {
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(path)
                    .unwrap()
                    .write_all(grown)
                    .unwrap();
            }
        });
        assert!(tail.poll().unwrap().is_empty(), "no newline yet");
        tail.clear_open_gap_hook();
        assert!(fired.load(Ordering::SeqCst), "the gap hook fired");

        let cover = tail
            .covers()
            .for_agent("a1")
            .next()
            .expect("the cover survives the growth");
        assert_eq!(
            cover.extent,
            w + 1 + grown.len() as u64,
            "the cover grew over the bytes read between stat and read_to_end"
        );
        assert!(cover.bytes.ends_with(grown));

        // Writer shrinks back to the ORIGINAL armed extent (just "{").
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(w + 1).unwrap();
        drop(file);
        assert!(tail.poll().unwrap().is_empty());
        assert!(
            tail.covers().for_agent("a1").next().is_none(),
            "the shrink through the GROWN extent invalidates the cover"
        );

        // A DIFFERENT retry completes the surviving brace; the retired cover
        // must not settle it.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(
                b"\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a1\",\"label\":\"genuine\"}\n",
            )
            .unwrap();
        let lines = tail.poll().unwrap();
        assert_eq!(lines.len(), 1);
        let (line, cursor) = &lines[0];
        assert_eq!(cursor.offset.0, w);
        assert!(
            !tail.covers().settles(
                "a1",
                cursor.file_generation.0,
                cursor.offset.0,
                &cursor.digest,
                line
            ),
            "the different retry is genuine, not settled by the retired cover"
        );
    }
}
