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
    pub fn poll(&mut self) -> Result<Vec<(Vec<u8>, FileCursor)>, Error> {
        let mut file = match File::open(&self.path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
            Ok(file) => file,
        };
        let len = file.metadata()?.len();
        self.detect_rotation(&mut file, len)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut rest = Vec::new();
        file.read_to_end(&mut rest)?;
        let mut lines = Vec::new();
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
