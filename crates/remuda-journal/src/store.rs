//! Single-writer append-only instance journals.

use crate::Error;
use crate::blob::BlobStore;
use crate::envelope::Envelope;
use crate::projection::Projections;
use crate::source::SourceResume;
use crate::util::u64_i64;
use futures::Stream;
use remuda_protocol::{
    HostId, Id, InstanceId, Observation, ObservationPayload, RawRef, SchemaVersion, U64,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
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

/// Options for [`Journal::open_with`].
#[derive(Debug, Clone)]
pub struct JournalOptions {
    /// Durability policy for append.
    pub fsync: FsyncPolicy,
}

impl Default for JournalOptions {
    fn default() -> Self {
        Self {
            fsync: FsyncPolicy::Data,
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
}

enum OpOut {
    Seq(U64),
    Observations(Vec<Observation>),
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
}

impl Journal {
    /// Open (or create) `<data_dir>/journal/*.jsonl` and `<data_dir>/journal.sqlite`.
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self, Error> {
        Self::open_with(data_dir, JournalOptions::default())
    }

    /// Open with an explicit fsync policy.
    pub fn open_with(data_dir: impl AsRef<Path>, options: JournalOptions) -> Result<Self, Error> {
        let data_dir = data_dir.as_ref().to_path_buf();
        fs::create_dir_all(data_dir.join("journal"))?;
        let (tx, rx) = mpsc::channel(64);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        thread::Builder::new()
            .name("remuda-journal".into())
            .spawn(move || match Store::open(data_dir, options) {
                Ok(mut store) => {
                    let _ = ready_tx.send(Ok(()));
                    store.run(rx);
                }
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                }
            })
            .map_err(Error::from)?;
        ready_rx.recv().map_err(|_| Error::Closed)??;
        Ok(Self { tx })
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
}

struct JsonlFile {
    file: File,
    len: u64,
}

impl Store {
    fn open(data_dir: PathBuf, options: JournalOptions) -> Result<Self, Error> {
        let blobs = BlobStore::new(&data_dir)?;
        fs::create_dir_all(data_dir.join("journal"))?;
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
        };
        store.recover()?;
        Ok(store)
    }

    fn run(&mut self, mut rx: mpsc::Receiver<Op>) {
        while let Some(op) = rx.blocking_recv() {
            let result = self.handle(op.kind);
            let _ = op.resp.send(result);
        }
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
        slot.file.write_all(line)?;
        slot.file.write_all(b"\n")?;
        let length = (line.len() + 1) as u64;
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
        let from = from_seq.max(1);
        let to = to_seq.unwrap_or(durable);
        if from > durable || to < from {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT jsonl_offset, jsonl_length FROM events
             WHERE instance_id = ?1 AND seq >= ?2 AND seq <= ?3 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(
            params![instance.as_id().as_str(), from as i64, to as i64],
            |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
        )?;
        let mut offsets = Vec::new();
        for row in rows {
            offsets.push(row?);
        }
        drop(stmt);
        let mut out = Vec::with_capacity(offsets.len());
        for (offset, length) in offsets {
            out.push(self.read_jsonl_at(instance, offset, length)?);
        }
        Ok(out)
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
            let bytes = fs::read(entry.path())?;
            let mut seq = 0u64;
            let mut offset = 0u64;
            let mut proj = Projections::default();
            for chunk in bytes.split(|b| *b == b'\n') {
                if chunk.is_empty() {
                    continue;
                }
                let obs: Observation = match serde_json::from_slice(chunk) {
                    Ok(obs) => obs,
                    Err(_)
                        if offset + chunk.len() as u64 >= bytes.len().saturating_sub(1) as u64 =>
                    {
                        break;
                    }
                    Err(err) => return Err(err.into()),
                };
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
                self.conn.execute(
                    "UPDATE instances SET seq = MAX(seq, ?1) WHERE instance_id = ?2",
                    params![seq as i64, instance_s],
                )?;
                proj.apply(&obs);
                offset += length;
            }
            self.conn.execute(
                "DELETE FROM events WHERE instance_id = ?1 AND seq > ?2",
                params![instance_s, seq as i64],
            )?;
            self.projections.insert(instance_s.clone(), proj);
            self.save_checkpoint(&instance)?;
        }
        Ok(())
    }
}
