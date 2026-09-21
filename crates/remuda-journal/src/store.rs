//! Single-writer append-only instance journals.

use crate::Error;
use crate::blob::BlobStore;
use crate::envelope::Envelope;
use crate::projection::Projections;
use crate::source::SourceResume;
use crate::util::u64_i64;
use futures::Stream;
use nix::fcntl::{Flock, FlockArg};
use remuda_protocol::{
    HostId, Id, InstanceId, Observation, ObservationPayload, RawRef, SchemaVersion, U64,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::thread;
use tokio::sync::{mpsc, oneshot};

/// When the writer fsyncs JSONL, blobs, and SQLite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsyncPolicy {
    /// No explicit fsync; SQLite `synchronous=OFF`.
    Never,
    /// `fdatasync` files; SQLite `synchronous=NORMAL`.
    Data,
    /// Full `fsync`; SQLite `synchronous=FULL`.
    All,
}

/// How long a second opener waits for a live writer to release the
/// single-writer lock before [`Error::Locked`] is returned.
const DEFAULT_WRITER_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
/// Poll interval while waiting for the single-writer lock.
const WRITER_LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// Options for [`Journal::open_with`].
#[derive(Debug, Clone)]
pub struct JournalOptions {
    /// Durability policy for append.
    pub fsync: FsyncPolicy,
    /// Bounded wait for a live writer to release the single-writer lock. A
    /// reopen that follows a prompt close acquires within a poll or two; a
    /// writer that is genuinely still alive (a Node dropped without shutdown,
    /// a stuck predecessor process) fails the open after this long rather than
    /// blocking forever.
    pub writer_lock_wait: std::time::Duration,
}

impl Default for JournalOptions {
    fn default() -> Self {
        Self {
            fsync: FsyncPolicy::Data,
            writer_lock_wait: DEFAULT_WRITER_LOCK_WAIT,
        }
    }
}

/// Snapshot of projections at a durable seq.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Highest committed seq included in `projections`.
    pub as_of_seq: U64,
    /// Folded transcript, interaction, and status.
    pub projections: Projections,
}

/// Upper bound on one [`Journal::read_page`], whatever the caller asks for.
///
/// The bound is enforced in the reader rather than at its callers because the
/// cost it prevents is the reader's own: an unbounded range deserializes every
/// row from the watermark to the tail only for the caller to drop all but the
/// first page.
pub const MAX_PAGE: usize = 256;

/// One bounded page of observations and the cost of producing it.
#[derive(Debug, Clone)]
pub struct Page {
    /// Observations in ascending `seq`; at most the requested `limit`.
    pub observations: Vec<Observation>,
    /// JSONL payload bytes read to build `observations`.
    ///
    /// This is the reader's own account of its cost, so a caller can assert
    /// that reading a two-event tail does not scale with the journal.
    pub bytes_read: u64,
    /// Highest `seq` in `observations`; `None` when the page is empty.
    pub last_seq: Option<U64>,
    /// Durable watermark at read time. `last_seq == Some(durable_seq)` means
    /// the page reached the tail.
    pub durable_seq: U64,
}

enum OpKind {
    Append {
        instance: InstanceId,
        envelope: Box<Envelope>,
    },
    ReadRange {
        instance: InstanceId,
        from_seq: u64,
        to_seq: Option<u64>,
    },
    ReadPage {
        instance: InstanceId,
        from_seq: u64,
        to_seq: Option<u64>,
        limit: usize,
    },
    Snapshot {
        instance: InstanceId,
    },
    Follow {
        instance: InstanceId,
        from_seq: u64,
    },
    PutResume {
        instance: InstanceId,
        resume: SourceResume,
    },
    GetResume {
        instance: InstanceId,
        source_key: String,
    },
    DurableSeq {
        instance: InstanceId,
    },
    /// Writer-thread shutdown nudge; the response is ignored.
    Shutdown,
}

enum OpOut {
    Seq(U64),
    Observations(Vec<Observation>),
    Page(Box<Page>),
    Snapshot(Snapshot),
    Follow {
        history: Vec<Observation>,
        rx: mpsc::Receiver<Observation>,
    },
    Resume(Option<SourceResume>),
    Watermark(U64),
    Unit,
}

struct Op {
    kind: OpKind,
    resp: oneshot::Sender<Result<OpOut, Error>>,
}

/// Cloneable handle to a single-writer observation journal.
#[derive(Clone)]
pub struct Journal {
    tx: mpsc::Sender<Op>,
    /// Set by [`Journal::close`]; the writer thread checks it between every op
    /// and exits as soon as it observes it.
    shutdown: Arc<AtomicBool>,
}

impl Journal {
    /// Open (or create) `<data_dir>/journal/*.jsonl` and `<data_dir>/journal.sqlite`.
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self, Error> {
        Self::open_with(data_dir, JournalOptions::default())
    }

    /// Open with an explicit fsync policy.
    ///
    /// Takes an exclusive `flock(2)` on `<data_dir>/journal.lock` for the
    /// writer's lifetime. The JSONL files, the SQLite index, and the per-record
    /// sequence table are all single-writer state, so a second opener — another
    /// thread or a process re-opening the same data dir — cannot race the
    /// current writer: it waits up to
    /// [`JournalOptions::writer_lock_wait`](default 5 s) for the lock to be
    /// released (covering a prompt close finishing its last op) and then fails
    /// the open with [`Error::Locked`] rather than blocking forever. A process
    /// killed hard releases the lock in the kernel immediately.
    pub fn open_with(data_dir: impl AsRef<Path>, options: JournalOptions) -> Result<Self, Error> {
        let data_dir = data_dir.as_ref().to_path_buf();
        fs::create_dir_all(data_dir.join("journal"))?;
        let (tx, rx) = mpsc::channel(64);
        let shutdown = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let writer_shutdown = Arc::clone(&shutdown);
        thread::Builder::new()
            .name("remuda-journal".into())
            .spawn(
                move || match Store::open(data_dir, options, writer_shutdown) {
                    Ok(mut store) => {
                        let _ = ready_tx.send(Ok(()));
                        store.run(rx);
                    }
                    Err(err) => {
                        let _ = ready_tx.send(Err(err));
                    }
                },
            )
            .map_err(Error::from)?;
        ready_rx.recv().map_err(|_| Error::Closed)??;
        Ok(Self { tx, shutdown })
    }

    /// Ask the writer thread to stop and release the single-writer lock.
    ///
    /// Synchronous and safe to call from a drop: it sets a stop flag and nudges
    /// a writer parked on an empty queue. Pending and in-flight operations may
    /// return [`Error::Closed`]; every later operation does. A journal reopened
    /// against the same data dir then acquires the lock only after this writer
    /// has fully stopped, so recovery never observes a file a dead writer can
    /// still touch.
    pub fn close(&self) {
        self.shutdown.store(true, Ordering::Release);
        let (resp, _rx) = oneshot::channel();
        // Nudge a writer parked in `blocking_recv` on an empty queue. If the
        // bounded queue is full the writer is actively draining and observes
        // the flag at its next loop iteration, so a failed nudge is harmless.
        let _ = self.tx.try_send(Op {
            kind: OpKind::Shutdown,
            resp,
        });
    }

    /// Append `envelope` to `instance`; returns the assigned seq (from 1).
    pub async fn append(&self, instance: &InstanceId, envelope: Envelope) -> Result<U64, Error> {
        match self
            .rpc(OpKind::Append {
                instance: instance.clone(),
                envelope: Box::new(envelope),
            })
            .await?
        {
            OpOut::Seq(seq) => Ok(seq),
            _ => Err(Error::Closed),
        }
    }

    /// Inclusive range. `to_seq = None` reads through the durable watermark.
    pub async fn read_range(
        &self,
        instance: &InstanceId,
        from_seq: U64,
        to_seq: Option<U64>,
    ) -> Result<Vec<Observation>, Error> {
        match self
            .rpc(OpKind::ReadRange {
                instance: instance.clone(),
                from_seq: from_seq.0,
                to_seq: to_seq.map(|s| s.0),
            })
            .await?
        {
            OpOut::Observations(items) => Ok(items),
            _ => Err(Error::Closed),
        }
    }

    /// Read at most `limit` observations (`limit` clamped to [`MAX_PAGE`]).
    ///
    /// Unlike [`Journal::read_range`], the bound is applied inside the reader:
    /// the SQL selects at most `limit` rows and only those rows' JSONL spans
    /// are deserialized. A tail read therefore costs O(new events), not
    /// O(journal). `to_seq = None` reads through the durable watermark, so the
    /// page simply ends at the tail. [`Page::bytes_read`] reports the JSONL
    /// cost so callers can assert on it.
    pub async fn read_page(
        &self,
        instance: &InstanceId,
        from_seq: U64,
        to_seq: Option<U64>,
        limit: usize,
    ) -> Result<Page, Error> {
        match self
            .rpc(OpKind::ReadPage {
                instance: instance.clone(),
                from_seq: from_seq.0,
                to_seq: to_seq.map(|s| s.0),
                limit: limit.max(1),
            })
            .await?
        {
            OpOut::Page(page) => Ok(*page),
            _ => Err(Error::Closed),
        }
    }

    /// Fold projections at the current durable seq.
    pub async fn snapshot(&self, instance: &InstanceId) -> Result<Snapshot, Error> {
        match self
            .rpc(OpKind::Snapshot {
                instance: instance.clone(),
            })
            .await?
        {
            OpOut::Snapshot(snapshot) => Ok(snapshot),
            _ => Err(Error::Closed),
        }
    }

    /// Subscribe first, then backfill history so the stream has no holes.
    pub async fn follow(&self, instance: &InstanceId, from_seq: U64) -> Result<Follow, Error> {
        match self
            .rpc(OpKind::Follow {
                instance: instance.clone(),
                from_seq: from_seq.0,
            })
            .await?
        {
            OpOut::Follow { history, rx } => Ok(Follow {
                history: history.into(),
                rx,
            }),
            _ => Err(Error::Closed),
        }
    }

    /// Persist a JSONL tail cursor.
    pub async fn put_source_resume(
        &self,
        instance: &InstanceId,
        resume: SourceResume,
    ) -> Result<(), Error> {
        match self
            .rpc(OpKind::PutResume {
                instance: instance.clone(),
                resume,
            })
            .await?
        {
            OpOut::Unit => Ok(()),
            _ => Err(Error::Closed),
        }
    }

    /// Load a JSONL tail cursor.
    pub async fn get_source_resume(
        &self,
        instance: &InstanceId,
        source_key: &str,
    ) -> Result<Option<SourceResume>, Error> {
        match self
            .rpc(OpKind::GetResume {
                instance: instance.clone(),
                source_key: source_key.to_owned(),
            })
            .await?
        {
            OpOut::Resume(resume) => Ok(resume),
            _ => Err(Error::Closed),
        }
    }

    /// Durable watermark, or 0 if the instance has no events.
    pub async fn durable_seq(&self, instance: &InstanceId) -> Result<U64, Error> {
        match self
            .rpc(OpKind::DurableSeq {
                instance: instance.clone(),
            })
            .await?
        {
            OpOut::Watermark(seq) => Ok(seq),
            _ => Err(Error::Closed),
        }
    }

    async fn rpc(&self, kind: OpKind) -> Result<OpOut, Error> {
        let (resp, rx) = oneshot::channel();
        self.tx
            .send(Op { kind, resp })
            .await
            .map_err(Error::closed_send)?;
        rx.await.map_err(|_| Error::Closed)?
    }
}

/// Live stream of observations: history, then newly committed seqs.
pub struct Follow {
    history: std::collections::VecDeque<Observation>,
    rx: mpsc::Receiver<Observation>,
}

impl Stream for Follow {
    type Item = Result<Observation, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(obs) = this.history.pop_front() {
            return Poll::Ready(Some(Ok(obs)));
        }
        match this.rx.poll_recv(cx) {
            Poll::Ready(Some(obs)) => Poll::Ready(Some(Ok(obs))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

struct Store {
    data_dir: PathBuf,
    options: JournalOptions,
    conn: Connection,
    blobs: BlobStore,
    jsonl: HashMap<String, JsonlFile>,
    projections: HashMap<String, Projections>,
    subscribers: HashMap<String, Vec<mpsc::Sender<Observation>>>,
    /// Held for the store's whole life so no second writer can open the same
    /// data dir concurrently; unlocked when the writer thread exits.
    _writer_lock: Flock<File>,
    /// Shared with every [`Journal`] handle; [`Journal::close`] flips it.
    shutdown: Arc<AtomicBool>,
}

struct JsonlFile {
    file: File,
    len: u64,
}

impl Store {
    fn open(
        data_dir: PathBuf,
        options: JournalOptions,
        shutdown: Arc<AtomicBool>,
    ) -> Result<Self, Error> {
        let blobs = BlobStore::new(&data_dir)?;
        fs::create_dir_all(data_dir.join("journal"))?;
        // Single-writer interlock. The wait is bounded: a reopen after a prompt
        // close acquires within a poll or two (the previous writer is finishing
        // its last op), while a writer that is genuinely still alive makes the
        // open fail with `Error::Locked` instead of blocking a process startup
        // forever. A hard-killed holder releases the lock in the kernel, so the
        // wait only ever covers a live writer.
        let lock_path = data_dir.join("journal.lock");
        let mut lock_file = OpenOptions::new()
            .create(true)
            // The file only carries the flock; never truncate a holder's file.
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        let deadline = std::time::Instant::now() + options.writer_lock_wait;
        let writer_lock = loop {
            match Flock::lock(lock_file, FlockArg::LockExclusiveNonblock) {
                Ok(flock) => break flock,
                Err((returned, errno)) if errno == nix::errno::Errno::EWOULDBLOCK => {
                    lock_file = returned;
                    if std::time::Instant::now() >= deadline {
                        return Err(Error::Locked {
                            path: lock_path,
                            waited: options.writer_lock_wait,
                        });
                    }
                    std::thread::sleep(WRITER_LOCK_POLL);
                }
                Err((_, errno)) => return Err(Self::lock_error(errno)),
            }
        };
        let conn = Connection::open(data_dir.join("journal.sqlite"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let sync = match options.fsync {
            FsyncPolicy::Never => "OFF",
            FsyncPolicy::Data => "NORMAL",
            FsyncPolicy::All => "FULL",
        };
        conn.pragma_update(None, "synchronous", sync)?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS instances (
                instance_id TEXT PRIMARY KEY,
                journal_id TEXT NOT NULL,
                host_id TEXT NOT NULL,
                seq INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS events (
                instance_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                event_id TEXT NOT NULL UNIQUE,
                jsonl_offset INTEGER NOT NULL,
                jsonl_length INTEGER NOT NULL,
                kind TEXT NOT NULL,
                PRIMARY KEY (instance_id, seq)
            );
            CREATE TABLE IF NOT EXISTS interactions (
                instance_id TEXT NOT NULL,
                request_key TEXT NOT NULL,
                interaction_id TEXT NOT NULL,
                state TEXT NOT NULL,
                first_seq INTEGER NOT NULL,
                PRIMARY KEY (instance_id, request_key)
            );
            CREATE TABLE IF NOT EXISTS source_cursors (
                instance_id TEXT NOT NULL,
                source_key TEXT NOT NULL,
                file_identity TEXT NOT NULL,
                generation INTEGER NOT NULL,
                offset INTEGER NOT NULL,
                prefix_digest TEXT,
                PRIMARY KEY (instance_id, source_key)
            );
            CREATE TABLE IF NOT EXISTS blobs (
                object_id TEXT PRIMARY KEY,
                digest TEXT NOT NULL UNIQUE,
                length INTEGER NOT NULL,
                media_type TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS checkpoints (
                instance_id TEXT PRIMARY KEY,
                as_of_seq INTEGER NOT NULL,
                projections_json TEXT NOT NULL
            );
            ",
        )?;
        let mut store = Self {
            data_dir,
            options,
            conn,
            blobs,
            jsonl: HashMap::new(),
            projections: HashMap::new(),
            subscribers: HashMap::new(),
            _writer_lock: writer_lock,
            shutdown,
        };
        store.recover()?;
        Ok(store)
    }

    fn run(&mut self, mut rx: mpsc::Receiver<Op>) {
        while !self.shutdown.load(Ordering::Acquire) {
            let Some(op) = rx.blocking_recv() else {
                break;
            };
            if self.shutdown.load(Ordering::Acquire) || matches!(op.kind, OpKind::Shutdown) {
                break;
            }
            let result = self.handle(op.kind);
            let _ = op.resp.send(result);
        }
    }

    fn lock_error(err: nix::Error) -> Error {
        Error::Io(std::io::Error::from_raw_os_error(err as i32))
    }

    fn handle(&mut self, kind: OpKind) -> Result<OpOut, Error> {
        match kind {
            OpKind::Append { instance, envelope } => {
                self.append(&instance, *envelope).map(OpOut::Seq)
            }
            OpKind::ReadRange {
                instance,
                from_seq,
                to_seq,
            } => self
                .read_range(&instance, from_seq, to_seq)
                .map(OpOut::Observations),
            OpKind::ReadPage {
                instance,
                from_seq,
                to_seq,
                limit,
            } => self
                .read_page(&instance, from_seq, to_seq, limit)
                .map(|page| OpOut::Page(Box::new(page))),
            OpKind::Snapshot { instance } => self.snapshot(&instance).map(OpOut::Snapshot),
            OpKind::Follow { instance, from_seq } => self.follow(&instance, from_seq),
            OpKind::PutResume { instance, resume } => {
                self.put_resume(&instance, &resume).map(|()| OpOut::Unit)
            }
            OpKind::GetResume {
                instance,
                source_key,
            } => self.get_resume(&instance, &source_key).map(OpOut::Resume),
            OpKind::DurableSeq { instance } => {
                Ok(OpOut::Watermark(U64(self.watermark(&instance)?)))
            }
            OpKind::Shutdown => Ok(OpOut::Unit),
        }
    }

    fn append(&mut self, instance: &InstanceId, mut envelope: Envelope) -> Result<U64, Error> {
        if envelope.instance_id != *instance {
            return Err(Error::InstanceMismatch {
                target: instance.as_id().as_str().to_owned(),
                envelope: envelope.instance_id.as_id().as_str().to_owned(),
            });
        }
        let key = instance.as_id().as_str().to_owned();
        self.ensure_instance(instance, &envelope.journal_id, &envelope.host_id)?;
        let raw_ref = match envelope.raw.take() {
            Some(raw) => Some(self.blobs.put(&self.conn, &raw, self.options.fsync)?),
            None => None,
        };
        let seq = self.watermark(instance)? + 1;
        let mut obs = Observation {
            schema_version: SchemaVersion,
            event_id: envelope.event_id.unwrap_or_default(),
            journal_id: envelope.journal_id,
            instance_id: envelope.instance_id,
            run_id: envelope.run_id,
            host_id: envelope.host_id,
            process_generation: envelope.process_generation,
            run_generation: envelope.run_generation,
            seq: U64(seq),
            observed_at: envelope.observed_at,
            native_at: envelope.native_at,
            source: envelope.source,
            completeness: envelope.completeness,
            raw_ref: raw_ref.clone(),
            evidence_event_ids: envelope.evidence_event_ids,
            body: envelope.body,
        };
        if let (Some(raw_ref), ObservationPayload::Opaque(payload)) = (&raw_ref, &mut obs.body) {
            payload.raw_ref = raw_ref.clone();
        }
        let line = serde_json::to_vec(&obs)?;
        let (offset, length) = self.write_jsonl(&key, &line)?;
        self.commit_index(instance, &obs, offset, length, raw_ref.as_ref())?;
        self.projections.entry(key.clone()).or_default().apply(&obs);
        self.save_checkpoint(instance)?;
        self.broadcast(&key, obs);
        Ok(U64(seq))
    }

    fn commit_index(
        &mut self,
        instance: &InstanceId,
        obs: &Observation,
        offset: u64,
        length: u64,
        _raw_ref: Option<&RawRef>,
    ) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        let instance_s = instance.as_id().as_str().to_owned();
        tx.execute(
            "UPDATE instances SET seq = ?1 WHERE instance_id = ?2",
            params![u64_i64(obs.seq)?, instance_s],
        )?;
        tx.execute(
            "INSERT INTO events (instance_id, seq, event_id, jsonl_offset, jsonl_length, kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                instance_s,
                u64_i64(obs.seq)?,
                obs.event_id.as_id().as_str(),
                offset as i64,
                length as i64,
                format!("{:?}", obs.body.kind()),
            ],
        )?;
        match &obs.body {
            ObservationPayload::InteractionRequested(payload) => {
                if let Ok(request_key) = serde_json::to_string(&payload.interaction.request_key) {
                    tx.execute(
                        "INSERT OR IGNORE INTO interactions
                         (instance_id, request_key, interaction_id, state, first_seq)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            instance_s,
                            request_key,
                            payload.interaction.meta.id.as_id().as_str(),
                            "pending",
                            u64_i64(obs.seq)?,
                        ],
                    )?;
                }
            }
            ObservationPayload::InteractionAnswered(payload) => {
                tx.execute(
                    "UPDATE interactions SET state = ?1 WHERE instance_id = ?2 AND interaction_id = ?3 AND state = 'pending'",
                    params![
                        "answered",
                        instance_s,
                        payload.interaction_id.as_id().as_str()
                    ],
                )?;
            }
            ObservationPayload::InteractionExpired(payload) => {
                tx.execute(
                    "UPDATE interactions SET state = ?1 WHERE instance_id = ?2 AND interaction_id = ?3 AND state = 'pending'",
                    params![
                        "expired",
                        instance_s,
                        payload.interaction_id.as_id().as_str()
                    ],
                )?;
            }
            ObservationPayload::Lifecycle(payload) => {
                if let remuda_protocol::LifecyclePayload::Entity(entity) = payload.as_ref()
                    && let remuda_protocol::LifecycleEntity::Interaction(interaction) =
                        &entity.entity_value
                {
                    tx.execute("UPDATE interactions SET state = ?1 WHERE instance_id = ?2 AND interaction_id = ?3",
                        params![entity.state, instance_s, interaction.meta.id.as_id().as_str()])?;
                }
            }
            _ => {}
        }
        tx.commit()?;
        Ok(())
    }

    fn write_jsonl(&mut self, instance_key: &str, line: &[u8]) -> Result<(u64, u64), Error> {
        let path = self.jsonl_path_key(instance_key);
        if !self.jsonl.contains_key(instance_key) {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .read(true)
                .open(&path)?;
            let len = file.metadata()?.len();
            self.jsonl
                .insert(instance_key.to_owned(), JsonlFile { file, len });
        }
        let slot = self
            .jsonl
            .get_mut(instance_key)
            .ok_or_else(|| Error::Path(path.clone()))?;
        let offset = slot.len;
        // One write of the whole record including its terminating newline. A
        // record is only real once the newline is on stable storage, so the
        // offset/length published into SQLite below always describes a complete
        // line: recovery treats anything after the last newline as a torn tail.
        let mut record = line.to_vec();
        record.push(b'\n');
        slot.file.write_all(&record)?;
        let length = record.len() as u64;
        slot.len += length;
        match self.options.fsync {
            FsyncPolicy::Never => {}
            FsyncPolicy::Data => slot.file.sync_data()?,
            FsyncPolicy::All => slot.file.sync_all()?,
        }
        Ok((offset, length))
    }

    fn read_range(
        &mut self,
        instance: &InstanceId,
        from_seq: u64,
        to_seq: Option<u64>,
    ) -> Result<Vec<Observation>, Error> {
        let durable = self.watermark(instance)?;
        let (observations, _) = self.read_rows(instance, from_seq, to_seq, durable, None)?;
        Ok(observations)
    }

    fn read_page(
        &mut self,
        instance: &InstanceId,
        from_seq: u64,
        to_seq: Option<u64>,
        limit: usize,
    ) -> Result<Page, Error> {
        let durable = self.watermark(instance)?;
        let limit = limit.clamp(1, MAX_PAGE);
        let (observations, bytes_read) =
            self.read_rows(instance, from_seq, to_seq, durable, Some(limit))?;
        let last_seq = observations.last().map(|obs| obs.seq);
        Ok(Page {
            observations,
            bytes_read,
            last_seq,
            durable_seq: U64(durable),
        })
    }

    /// Shared reader for [`Self::read_range`] and [`Self::read_page`].
    ///
    /// `limit = None` is the unbounded range; `Some(limit)` stops the SQL at
    /// `limit` rows so neither the query nor the JSONL seeks touch more. The
    /// second element of the result is the JSONL payload byte count read.
    fn read_rows(
        &mut self,
        instance: &InstanceId,
        from_seq: u64,
        to_seq: Option<u64>,
        durable: u64,
        limit: Option<usize>,
    ) -> Result<(Vec<Observation>, u64), Error> {
        let from = from_seq.max(1);
        let to = to_seq.unwrap_or(durable);
        if from > durable || to < from {
            return Ok((Vec::new(), 0));
        }
        // `LIMIT -1` is SQLite's "no limit", so one statement covers both
        // shapes and the bound is applied where the rows are chosen.
        let limit_i64 = match limit {
            Some(limit) => i64::try_from(limit).unwrap_or(i64::MAX),
            None => -1,
        };
        let mut stmt = self.conn.prepare(
            "SELECT jsonl_offset, jsonl_length FROM events
             WHERE instance_id = ?1 AND seq >= ?2 AND seq <= ?3
             ORDER BY seq ASC LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![instance.as_id().as_str(), from as i64, to as i64, limit_i64],
            |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
        )?;
        let mut offsets = Vec::new();
        for row in rows {
            offsets.push(row?);
        }
        drop(stmt);
        let mut bytes_read = 0u64;
        let mut out = Vec::with_capacity(offsets.len());
        for (offset, length) in offsets {
            out.push(self.read_jsonl_at(instance, offset, length)?);
            bytes_read = bytes_read.saturating_add(length);
        }
        Ok((out, bytes_read))
    }

    fn snapshot(&mut self, instance: &InstanceId) -> Result<Snapshot, Error> {
        let as_of = self.watermark(instance)?;
        let key = instance.as_id().as_str().to_owned();
        if !self.projections.contains_key(&key) && as_of > 0 {
            let items = self.read_range(instance, 1, Some(as_of))?;
            let mut proj = Projections::default();
            for obs in items {
                proj.apply(&obs);
            }
            self.projections.insert(key.clone(), proj);
        }
        Ok(Snapshot {
            as_of_seq: U64(as_of),
            projections: self.projections.get(&key).cloned().unwrap_or_default(),
        })
    }

    fn follow(&mut self, instance: &InstanceId, from_seq: u64) -> Result<OpOut, Error> {
        let key = instance.as_id().as_str().to_owned();
        let (tx, rx) = mpsc::channel(4096);
        self.subscribers.entry(key).or_default().push(tx);
        let durable = self.watermark(instance)?;
        let from = from_seq.max(1);
        if from > durable + 1 {
            return Err(Error::Gap {
                instance: instance.as_id().as_str().to_owned(),
                from_seq: from,
                durable_seq: durable,
            });
        }
        let history = if from <= durable {
            self.read_range(instance, from, Some(durable))?
        } else {
            Vec::new()
        };
        Ok(OpOut::Follow { history, rx })
    }

    fn broadcast(&mut self, key: &str, obs: Observation) {
        if let Some(list) = self.subscribers.get_mut(key) {
            list.retain(|tx| tx.try_send(obs.clone()).is_ok());
        }
    }

    fn ensure_instance(
        &mut self,
        instance: &InstanceId,
        journal_id: &Id,
        host_id: &HostId,
    ) -> Result<(), Error> {
        let instance_s = instance.as_id().as_str().to_owned();
        let existing: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT journal_id, host_id FROM instances WHERE instance_id = ?1",
                params![instance_s],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match existing {
            Some((jid, _)) if jid != journal_id.as_str() => Err(Error::JournalMismatch {
                instance: instance_s,
            }),
            Some(_) => Ok(()),
            None => {
                self.conn.execute(
                    "INSERT INTO instances (instance_id, journal_id, host_id, seq) VALUES (?1, ?2, ?3, 0)",
                    params![instance_s, journal_id.as_str(), host_id.as_id().as_str()],
                )?;
                Ok(())
            }
        }
    }

    fn watermark(&self, instance: &InstanceId) -> Result<u64, Error> {
        let seq: Option<i64> = self
            .conn
            .query_row(
                "SELECT seq FROM instances WHERE instance_id = ?1",
                params![instance.as_id().as_str()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(seq.unwrap_or(0) as u64)
    }

    fn jsonl_path(&self, instance: &InstanceId) -> PathBuf {
        self.jsonl_path_key(instance.as_id().as_str())
    }

    fn jsonl_path_key(&self, instance_key: &str) -> PathBuf {
        self.data_dir
            .join("journal")
            .join(format!("{instance_key}.jsonl"))
    }

    fn read_jsonl_at(
        &mut self,
        instance: &InstanceId,
        offset: u64,
        length: u64,
    ) -> Result<Observation, Error> {
        let key = instance.as_id().as_str().to_owned();
        if !self.jsonl.contains_key(&key) {
            let path = self.jsonl_path(instance);
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .read(true)
                .open(&path)?;
            let len = file.metadata()?.len();
            self.jsonl.insert(key.clone(), JsonlFile { file, len });
        }
        let path = self.jsonl_path(instance);
        let slot = self.jsonl.get_mut(&key).ok_or_else(|| Error::Path(path))?;
        slot.file.seek(SeekFrom::Start(offset))?;
        let take = length.saturating_sub(1) as usize;
        let mut buf = vec![0u8; take];
        slot.file.read_exact(&mut buf)?;
        Ok(serde_json::from_slice(&buf)?)
    }

    fn put_resume(&mut self, instance: &InstanceId, resume: &SourceResume) -> Result<(), Error> {
        let prefix = resume.prefix_digest.as_ref().map(|d| {
            let s: String = d.clone().into();
            s
        });
        self.conn.execute(
            "INSERT INTO source_cursors (instance_id, source_key, file_identity, generation, offset, prefix_digest)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(instance_id, source_key) DO UPDATE SET
                file_identity = excluded.file_identity,
                generation = excluded.generation,
                offset = excluded.offset,
                prefix_digest = excluded.prefix_digest",
            params![
                instance.as_id().as_str(),
                resume.source_key,
                resume.file_identity.as_str(),
                u64_i64(resume.generation)?,
                u64_i64(resume.offset)?,
                prefix,
            ],
        )?;
        Ok(())
    }

    fn get_resume(
        &self,
        instance: &InstanceId,
        source_key: &str,
    ) -> Result<Option<SourceResume>, Error> {
        let row: Option<(String, i64, i64, Option<String>)> = self
            .conn
            .query_row(
                "SELECT file_identity, generation, offset, prefix_digest FROM source_cursors
                 WHERE instance_id = ?1 AND source_key = ?2",
                params![instance.as_id().as_str(), source_key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some((identity, generation, offset, prefix)) => {
                let prefix_digest = match prefix {
                    Some(text) => Some(remuda_protocol::Digest::try_from(text)?),
                    None => None,
                };
                Ok(Some(SourceResume {
                    source_key: source_key.to_owned(),
                    file_identity: Id::try_from(identity)?,
                    generation: U64(generation as u64),
                    offset: U64(offset as u64),
                    prefix_digest,
                }))
            }
        }
    }

    fn save_checkpoint(&mut self, instance: &InstanceId) -> Result<(), Error> {
        let key = instance.as_id().as_str().to_owned();
        let Some(proj) = self.projections.get(&key) else {
            return Ok(());
        };
        let json = serde_json::to_string(proj)?;
        let seq = self.watermark(instance)?;
        self.conn.execute(
            "INSERT INTO checkpoints (instance_id, as_of_seq, projections_json)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(instance_id) DO UPDATE SET
                as_of_seq = excluded.as_of_seq,
                projections_json = excluded.projections_json",
            params![key, seq as i64, json],
        )?;
        Ok(())
    }

    /// Rebuild the SQLite index from the JSONL files, discarding a torn tail.
    ///
    /// The JSONL is the authority. A record exists only when its terminating
    /// newline is on disk, so bytes after the last newline are the remains of a
    /// write interrupted by an unclean shutdown (or a page the filesystem never
    /// made durable): they are logged once and physically truncated, never
    /// parsed. A malformed *complete* line further up is real corruption rather
    /// than a torn append, so it is an error — discarding complete records to
    /// paper over that would lose acknowledged observations.
    fn recover(&mut self) -> Result<(), Error> {
        let dir = self.data_dir.join("journal");
        let entries = match fs::read_dir(&dir) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err.into()),
            Ok(entries) => entries,
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(".jsonl") {
                continue;
            }
            let instance_s = name.trim_end_matches(".jsonl").to_owned();
            let instance = InstanceId::try_from(instance_s.clone())?;
            let mut bytes = fs::read(entry.path())?;
            // Cut at the final newline: every byte past it is an unterminated,
            // therefore never-committed, record.
            let valid_len = bytes
                .iter()
                .rposition(|b| *b == b'\n')
                .map_or(0, |pos| pos + 1) as u64;
            let torn_bytes = bytes.len() as u64 - valid_len;
            if torn_bytes > 0 {
                tracing::warn!(
                    instance = %instance_s,
                    torn_bytes,
                    "truncating torn journal tail left by an unclean shutdown; complete records are kept"
                );
                let file = OpenOptions::new().write(true).open(entry.path())?;
                file.set_len(valid_len)?;
                if matches!(self.options.fsync, FsyncPolicy::Data | FsyncPolicy::All) {
                    file.sync_data()?;
                }
                bytes.truncate(valid_len as usize);
            }
            let mut seq = 0u64;
            let mut offset = 0u64;
            let mut proj = Projections::default();
            for chunk in bytes.split(|b| *b == b'\n') {
                if chunk.is_empty() {
                    continue;
                }
                // Every chunk here was newline-terminated on disk, so a parse
                // failure is corruption inside the committed prefix, not a torn
                // append, and must not be silently dropped.
                let obs: Observation = serde_json::from_slice(chunk)?;
                if obs.instance_id != instance {
                    return Err(Error::Diverged {
                        instance: instance_s.clone(),
                        seq: obs.seq.0,
                    });
                }
                seq = obs.seq.0;
                let length = (chunk.len() + 1) as u64;
                self.conn.execute(
                    "INSERT OR REPLACE INTO events
                     (instance_id, seq, event_id, jsonl_offset, jsonl_length, kind)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        instance_s,
                        seq as i64,
                        obs.event_id.as_id().as_str(),
                        offset as i64,
                        length as i64,
                        format!("{:?}", obs.body.kind()),
                    ],
                )?;
                if let ObservationPayload::InteractionRequested(payload) = &obs.body
                    && let Ok(request_key) = serde_json::to_string(&payload.interaction.request_key)
                {
                    self.conn.execute(
                        "INSERT OR IGNORE INTO interactions
                         (instance_id, request_key, interaction_id, state, first_seq)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            instance_s,
                            request_key,
                            payload.interaction.meta.id.as_id().as_str(),
                            "pending",
                            seq as i64,
                        ],
                    )?;
                }
                self.ensure_instance(&instance, &obs.journal_id, &obs.host_id)?;
                proj.apply(&obs);
                offset += length;
            }
            // JSONL is the authority: drop every index row it does not contain
            // and pin the watermark to exactly what survived, even when the old
            // SQLite row carried a higher seq from the torn append.
            self.conn.execute(
                "DELETE FROM events WHERE instance_id = ?1 AND seq > ?2",
                params![instance_s, seq as i64],
            )?;
            if seq > 0 {
                self.conn.execute(
                    "UPDATE instances SET seq = ?1 WHERE instance_id = ?2",
                    params![seq as i64, instance_s],
                )?;
            } else {
                self.conn.execute(
                    "UPDATE instances SET seq = 0 WHERE instance_id = ?1",
                    params![instance_s],
                )?;
            }
            self.projections.insert(instance_s.clone(), proj);
            self.save_checkpoint(&instance)?;
        }
        Ok(())
    }
}
