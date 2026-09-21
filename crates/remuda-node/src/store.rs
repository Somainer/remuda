//! Store seam and the M0 in-memory implementation.

use crate::{
    NodeError, driver::runtime_source, entity::EntityDb, interactions::PendingInteraction,
};
use remuda_driver::LaunchRecipe;
use remuda_journal::{Envelope, FsyncPolicy, Journal, JournalOptions};
use remuda_protocol::{
    Activity, Command, CommandId, CommandState, Completeness, ConversationNode, EventId,
    EventsReadResult, HistoryCoverage, Instance, InstanceId, InstanceLifecycle, InstanceSnapshot,
    JournalEvent, Knowledge, Observation, ObservationPayload, ResolutionState, RunId,
    SchemaVersion, U64,
};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{RwLock, mpsc as std_mpsc},
    thread,
};
use time::OffsetDateTime;
use tokio::sync::broadcast;

struct InstanceRecord {
    instance: Instance,
    command_ids: Vec<CommandId>,
    events: Vec<JournalEvent>,
    events_tx: broadcast::Sender<JournalEvent>,
}

#[derive(Default)]
struct MemoryState {
    instances: BTreeMap<InstanceId, InstanceRecord>,
    commands: BTreeMap<CommandId, Command>,
    pty_resources: BTreeMap<String, remuda_driver::PtyResource>,
    journal_instances: BTreeMap<remuda_protocol::Id, InstanceId>,
}

/// Storage contract used by `DevNode`; the durable SQLite adapter can implement this seam later.
pub trait LocalStore: Send + Sync {
    /// Persist a Node-owned Herdr resource before agent launch.
    fn put_pty_resource(&self, resource: &remuda_driver::PtyResource) -> Result<(), NodeError>;
    /// List resources, including partial launches from a previous Node process.
    fn pty_resources(&self) -> Result<Vec<remuda_driver::PtyResource>, NodeError>;
    /// Remove a resource after confirmed cleanup.
    fn remove_pty_resource(&self, key: &str) -> Result<(), NodeError>;

    /// Insert a newly materialized Instance.
    fn insert_instance(&self, instance: Instance) -> Result<(), NodeError>;
    /// Return all Instances in stable identity order.
    fn list_instances(&self) -> Result<Vec<Instance>, NodeError>;
    /// Whether this store is backed by durable instance rows on disk.
    ///
    /// A Node announcing an *empty* inventory is making a claim the Hub acts
    /// on destructively: it settles every live row on the host. That claim is
    /// only trustworthy when the Node actually found its instance store — a
    /// `--data-dir` pointed at the wrong path, a wiped disk, or a compose with
    /// an in-memory store all enumerate zero rows while owning nothing, and
    /// believing them would delete every session on the host.
    ///
    /// So this is the vouch the hello's `instances` key rests on: a store that
    /// cannot attest to its own persistence reports `false`, and the Node then
    /// omits the key rather than sending `[]`.
    fn instance_store_is_durable(&self) -> bool;
    /// Read one Instance.
    fn get_instance(&self, instance_id: &InstanceId) -> Result<Instance, NodeError>;
    /// Remove an Instance and its Node-owned rows; `false` when unknown.
    ///
    /// Only for an Instance that has already stopped. The agent's own native
    /// transcripts are outside the Node data dir and are never touched.
    fn remove_instance(&self, instance_id: &InstanceId) -> Result<bool, NodeError>;
    /// Change lifecycle/activity and return the revised Instance.
    fn set_instance_state(
        &self,
        instance_id: &InstanceId,
        lifecycle: Option<InstanceLifecycle>,
        activity: Option<Knowledge<Activity>>,
    ) -> Result<Instance, NodeError>;
    /// Replace the capability snapshot with the one the live driver reports.
    ///
    /// D-028 §4.3: the create-time snapshot is keyed by `DriverKind`, which
    /// cannot express the thing that actually varies. A `shell-pty` carrying a
    /// promoted `claude` can steer and interrupt; the same driver carrying a
    /// login shell cannot. Until the driver is asked, the wire reports the
    /// static row and §6's honesty rule is unmet — the UI cannot tell
    /// `emulated` from `native` from genuinely `unknown`.
    ///
    /// Returns `None` when the snapshot is unchanged, so a repeated refresh
    /// does not bump the revision.
    fn set_instance_capabilities(
        &self,
        instance_id: &InstanceId,
        capabilities: remuda_protocol::CapabilitySnapshot,
    ) -> Result<Option<Instance>, NodeError>;
    /// Apply a terminal → agent promotion or demotion (D-025).
    ///
    /// `kind` is the agent now holding the PTY's foreground (`terminal` on
    /// demotion) and `mode` says how it got there. Idempotent: applying the
    /// same pair twice does not bump the revision.
    fn set_instance_promotion(
        &self,
        instance_id: &InstanceId,
        kind: remuda_protocol::AgentKind,
        mode: remuda_protocol::InstanceMode,
        promoted_at: Option<remuda_protocol::Timestamp>,
    ) -> Result<Instance, NodeError>;
    /// Mark the instance failed and record `lastError`.
    fn set_instance_failure(
        &self,
        instance_id: &InstanceId,
        last_error: &str,
    ) -> Result<Instance, NodeError>;
    /// Record the native session identity a driver observed (D-026).
    /// A supplied signal tier must be proven for that session; absent preserves it.
    ///
    /// Returns the revised Instance when anything changed; `None` when the
    /// observation repeated what `nativeRef` already held, so callers do not
    /// journal a redundant lifecycle event.
    fn set_native_session(
        &self,
        instance_id: &InstanceId,
        session_id: &str,
        transcript_path: Option<&str>,
        signal_tier: Option<remuda_protocol::SignalTier>,
    ) -> Result<Option<Instance>, NodeError>;
    /// Atomically insert a command, returning false for a matching idempotent replay.
    fn insert_command(&self, instance_id: &InstanceId, command: Command)
    -> Result<bool, NodeError>;
    /// Read one command.
    fn get_command(&self, command_id: &CommandId) -> Result<Command, NodeError>;
    /// Replace one command with a later revision.
    fn save_command(&self, command: Command) -> Result<(), NodeError>;
    /// Mark every non-settled command for an Instance as dispatch-unknown.
    fn mark_unsettled_unknown(&self, instance_id: &InstanceId) -> Result<(), NodeError>;
    /// Append one protocol Observation and allocate its per-Instance sequence.
    fn append_observation(
        &self,
        instance_id: &InstanceId,
        run_id: Option<RunId>,
        completeness: Completeness,
        body: ObservationPayload,
    ) -> Result<Observation, NodeError>;
    /// Commit an observation emitted by a native driver under Node-owned identities and sequence.
    fn append_driver_observation(
        &self,
        instance_id: &InstanceId,
        observation: Observation,
    ) -> Result<Observation, NodeError>;
    /// Read a bounded page after an exclusive sequence.
    fn read_events(
        &self,
        journal_id: &remuda_protocol::Id,
        after_seq: Option<U64>,
        limit: usize,
    ) -> Result<EventsReadResult, NodeError>;
    /// JSONL payload bytes read by [`LocalStore::read_events`] since process
    /// start, across every journal.
    ///
    /// Flush cost is otherwise invisible from outside: a bounded read and an
    /// unbounded one return the same `EventsReadResult` for a short tail. This
    /// counter exposes the reader's own byte accounting so a regression test
    /// can assert that one flush tick costs O(new events), not O(journal).
    fn journal_bytes_read(&self) -> u64;
    /// Durable sequence a journal's underlying store has committed.
    ///
    /// Read straight from the authority (the journal's own watermark), not
    /// from the in-memory `Instance.durable_seq` mirror, so a flush tick can
    /// decide it has nothing to do without touching the reader at all.
    fn journal_durable_seq(&self, journal_id: &remuda_protocol::Id) -> Result<U64, NodeError>;
    /// Build a projection snapshot at the same in-memory watermark as its entities.
    fn snapshot(
        &self,
        instance_id: &InstanceId,
        projection_epoch: remuda_protocol::Id,
    ) -> Result<InstanceSnapshot, NodeError>;
    /// Subscribe before taking a snapshot so later writes can be buffered without gaps.
    fn subscribe(
        &self,
        instance_id: &InstanceId,
    ) -> Result<broadcast::Receiver<JournalEvent>, NodeError>;
    /// Resolve a journal identity to its owning Instance.
    fn instance_for_journal(
        &self,
        journal_id: &remuda_protocol::Id,
    ) -> Result<InstanceId, NodeError>;
    /// Persist a launch recipe that contains no secrets or prompts.
    fn put_launch_recipe(
        &self,
        instance_id: &InstanceId,
        recipe: &LaunchRecipe,
    ) -> Result<(), NodeError>;
    /// Load a persisted launch recipe.
    fn launch_recipe(&self, instance_id: &InstanceId) -> Result<Option<LaunchRecipe>, NodeError>;
    /// Persist a pending Interaction for restart.
    fn put_pending_interaction(&self, pending: &PendingInteraction) -> Result<(), NodeError>;
    /// Load pending Interactions.
    fn pending_interactions(&self) -> Result<Vec<PendingInteraction>, NodeError>;
    /// Stop durable journal writer threads during synchronous teardown.
    ///
    /// Called from the composed Node's drop so a Node that is dropped without a
    /// graceful shutdown cannot leave a journal writer alive to race a Node
    /// reopened on the same data dir. No-op for stores without a durable
    /// journal.
    fn shutdown_journal(&self) {}
}

/// Thread-safe in-memory store used by `remuda dev` and local API tests.
pub struct MemoryStore {
    state: RwLock<MemoryState>,
    follow_buffer_capacity: usize,
    durable: Option<DurableJournal>,
    entities: Option<EntityDb>,
    /// See [`LocalStore::journal_bytes_read`].
    journal_bytes_read: std::sync::atomic::AtomicU64,
}

/// Owns the journal writer job channel and closes it on drop. Not `Clone`: a
/// copy dropping at the end of a call would tear down a writer another owner
/// still uses (see the [`Drop`] impl below).
struct DurableJournal {
    journal: Journal,
    jobs: std_mpsc::SyncSender<JournalJob>,
    /// Flipped by [`DurableJournal::shutdown`] during Node teardown. The worker
    /// exits on its next iteration and the underlying single-writer journal
    /// closes, so a reopen can never race a writer this Node spawned.
    stopping: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

enum JournalJob {
    Append {
        instance_id: InstanceId,
        envelope: Box<Envelope>,
        reply: std_mpsc::SyncSender<Result<Observation, String>>,
    },
    ReadPage {
        instance_id: InstanceId,
        from_seq: u64,
        to_seq: Option<u64>,
        limit: usize,
        reply: std_mpsc::SyncSender<Result<remuda_journal::Page, String>>,
    },
    DurableSeq {
        instance_id: InstanceId,
        reply: std_mpsc::SyncSender<Result<U64, String>>,
    },
    /// Wake a worker parked in `recv` so it observes the stopping flag.
    Shutdown,
}

impl DurableJournal {
    fn open(data_dir: &Path, queue_capacity: usize, fsync: FsyncPolicy) -> Result<Self, NodeError> {
        let journal = Journal::open_with(
            data_dir,
            JournalOptions {
                fsync,
                ..JournalOptions::default()
            },
        )
        .map_err(|error| NodeError::Driver(format!("journal open failed: {error}")))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| NodeError::Driver(format!("journal runtime failed: {error}")))?;
        let (jobs, receiver) = std_mpsc::sync_channel::<JournalJob>(queue_capacity.max(1));
        let writer = journal.clone();
        let stopping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_stopping = std::sync::Arc::clone(&stopping);
        thread::Builder::new()
            .name("remuda-node-journal".to_owned())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    if worker_stopping.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    match job {
                        JournalJob::Shutdown => break,
                        JournalJob::Append {
                            instance_id,
                            envelope,
                            reply,
                        } => {
                            let result = runtime.block_on(async {
                                let seq = writer
                                    .append(&instance_id, *envelope)
                                    .await
                                    .map_err(|error| error.to_string())?;
                                let mut observations = writer
                                    .read_range(&instance_id, seq, Some(seq))
                                    .await
                                    .map_err(|error| error.to_string())?;
                                if observations.len() != 1 {
                                    return Err(format!(
                                        "journal returned {} observations for committed seq {}",
                                        observations.len(),
                                        seq.0
                                    ));
                                }
                                Ok(observations.remove(0))
                            });
                            let _ = reply.send(result);
                        }
                        JournalJob::ReadPage {
                            instance_id,
                            from_seq,
                            to_seq,
                            limit,
                            reply,
                        } => {
                            let result = runtime.block_on(async {
                                writer
                                    .read_page(&instance_id, U64(from_seq), to_seq.map(U64), limit)
                                    .await
                                    .map_err(|error| error.to_string())
                            });
                            let _ = reply.send(result);
                        }
                        JournalJob::DurableSeq { instance_id, reply } => {
                            let result = runtime.block_on(async {
                                writer
                                    .durable_seq(&instance_id)
                                    .await
                                    .map_err(|error| error.to_string())
                            });
                            let _ = reply.send(result);
                        }
                    }
                }
            })
            .map_err(|error| NodeError::Driver(format!("journal writer failed: {error}")))?;
        Ok(Self {
            journal,
            jobs,
            stopping,
        })
    }

    /// Stop accepting work and release the journal's single-writer lock.
    ///
    /// Synchronous on purpose: a Node dropped the hard way (no graceful
    /// shutdown) calls this from a drop, and a reopened Node in the same process
    /// must be able to acquire the journal lock immediately afterwards. A
    /// failed nudge is harmless: a busy worker observes the flag on its next
    /// iteration. Idempotent, so the [`Drop`] guard and an explicit shutdown
    /// can both call it.
    fn shutdown(&self) {
        if self
            .stopping
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        // Nudge a worker parked in `recv`; a full queue means it is draining and
        // it observes the flag at its next iteration anyway.
        let _ = self.jobs.try_send(JournalJob::Shutdown);
        self.journal.close();
    }

    fn stopped(&self) -> bool {
        self.stopping.load(std::sync::atomic::Ordering::Acquire)
    }

    fn append(
        &self,
        instance_id: &InstanceId,
        envelope: Envelope,
    ) -> Result<Observation, NodeError> {
        if self.stopped() {
            return Err(NodeError::Driver("journal is shut down".to_owned()));
        }
        let (reply, result) = std_mpsc::sync_channel(1);
        self.jobs
            .send(JournalJob::Append {
                instance_id: instance_id.clone(),
                envelope: Box::new(envelope),
                reply,
            })
            .map_err(|_| NodeError::Driver("journal writer stopped".to_owned()))?;
        result
            .recv()
            .map_err(|_| NodeError::Driver("journal writer dropped its result".to_owned()))?
            .map_err(|error| NodeError::Driver(format!("journal append failed: {error}")))
    }

    /// Read one bounded page. The bound reaches the journal reader, so the
    /// SQL and the JSONL seeks touch at most `limit` rows.
    fn read_page(
        &self,
        instance_id: &InstanceId,
        from_seq: u64,
        to_seq: Option<u64>,
        limit: usize,
    ) -> Result<remuda_journal::Page, NodeError> {
        let (reply, result) = std_mpsc::sync_channel(1);
        self.jobs
            .send(JournalJob::ReadPage {
                instance_id: instance_id.clone(),
                from_seq,
                to_seq,
                limit,
                reply,
            })
            .map_err(|_| NodeError::Driver("journal writer stopped".to_owned()))?;
        result
            .recv()
            .map_err(|_| NodeError::Driver("journal writer dropped its result".to_owned()))?
            .map_err(|error| NodeError::Driver(format!("journal read failed: {error}")))
    }

    fn durable_seq(&self, instance_id: &InstanceId) -> Result<U64, NodeError> {
        let (reply, result) = std_mpsc::sync_channel(1);
        self.jobs
            .send(JournalJob::DurableSeq {
                instance_id: instance_id.clone(),
                reply,
            })
            .map_err(|_| NodeError::Driver("journal writer stopped".to_owned()))?;
        result
            .recv()
            .map_err(|_| NodeError::Driver("journal writer dropped its result".to_owned()))?
            .map_err(|error| NodeError::Driver(format!("journal watermark failed: {error}")))
    }
}

/// RAII guard: the single-writer lock is released on *every* exit path,
/// including an unwind or an owner that never calls the explicit shutdown.
/// [`MemoryStore::shutdown_journal`] usually closes first (in order, after the
/// store-owning tasks have been aborted); this is the backstop.
impl Drop for DurableJournal {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl MemoryStore {
    /// Create an empty store with a bounded per-Instance broadcast ring.
    pub fn new(follow_buffer_capacity: usize) -> Self {
        Self {
            state: RwLock::new(MemoryState::default()),
            follow_buffer_capacity: follow_buffer_capacity.max(1),
            durable: None,
            entities: None,
            journal_bytes_read: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Open a durable store under `data_dir` (`node.sqlite` + remuda-journal).
    ///
    /// Uses [`FsyncPolicy::Data`]: SQLite `synchronous=NORMAL` / WAL for both
    /// `node.sqlite` and `journal.sqlite`, plus `fdatasync` on journal JSONL
    /// and blob files.
    pub fn open_journaled(
        data_dir: impl AsRef<Path>,
        follow_buffer_capacity: usize,
    ) -> Result<Self, NodeError> {
        Self::open_journaled_with(data_dir, follow_buffer_capacity, FsyncPolicy::Data)
    }

    /// Open a durable store with an explicit journal fsync policy.
    pub fn open_journaled_with(
        data_dir: impl AsRef<Path>,
        follow_buffer_capacity: usize,
        fsync: FsyncPolicy,
    ) -> Result<Self, NodeError> {
        let data_dir = data_dir.as_ref();
        let entities = EntityDb::open(data_dir)?;
        let durable = DurableJournal::open(data_dir, follow_buffer_capacity, fsync)?;
        let mut state = MemoryState::default();
        for resource in entities.pty_resources()? {
            state.pty_resources.insert(resource.key(), resource);
        }
        for mut instance in entities.list_instances()? {
            let seq = durable.durable_seq(&instance.meta.id)?;
            instance.durable_seq = seq;
            let instance_id = instance.meta.id.clone();
            let journal_id = instance.journal_id.clone();
            let commands = entities.commands_for(&instance_id)?;
            let command_ids = commands
                .iter()
                .map(|command| command.command_id.clone())
                .collect();
            for command in commands {
                state.commands.insert(command.command_id.clone(), command);
            }
            let (events_tx, _) = broadcast::channel(follow_buffer_capacity.max(1));
            state
                .journal_instances
                .insert(journal_id, instance_id.clone());
            state.instances.insert(
                instance_id,
                InstanceRecord {
                    instance,
                    command_ids,
                    events: Vec::new(),
                    events_tx,
                },
            );
        }
        let store = Self {
            state: RwLock::new(state),
            follow_buffer_capacity: follow_buffer_capacity.max(1),
            durable: Some(durable),
            entities: Some(entities),
            journal_bytes_read: std::sync::atomic::AtomicU64::new(0),
        };
        for instance in store.list_instances()? {
            if matches!(instance.lifecycle, InstanceLifecycle::Ready) {
                store.mark_unsettled_unknown(&instance.meta.id)?;
            }
        }
        Ok(store)
    }

    /// Return the durable journal handle when this store was opened with persistence.
    pub fn journal(&self) -> Option<Journal> {
        self.durable.as_ref().map(|durable| durable.journal.clone())
    }

    /// Close the durable journal's writer threads now rather than on the last
    /// drop, so a reopened Node acquires the single-writer lock even while
    /// aborted tasks still hold store clones.
    pub fn shutdown_journal(&self) {
        if let Some(durable) = &self.durable {
            durable.shutdown();
        }
    }
}

impl LocalStore for MemoryStore {
    fn put_pty_resource(&self, resource: &remuda_driver::PtyResource) -> Result<(), NodeError> {
        if let Some(db) = &self.entities {
            db.put_pty_resource(resource)?;
        }
        self.state
            .write()
            .map_err(|_| NodeError::StorePoisoned)?
            .pty_resources
            .insert(resource.key(), resource.clone());
        Ok(())
    }

    fn pty_resources(&self) -> Result<Vec<remuda_driver::PtyResource>, NodeError> {
        Ok(self
            .state
            .read()
            .map_err(|_| NodeError::StorePoisoned)?
            .pty_resources
            .values()
            .cloned()
            .collect())
    }

    fn remove_pty_resource(&self, key: &str) -> Result<(), NodeError> {
        if let Some(db) = &self.entities {
            db.remove_pty_resource(key)?;
        }
        self.state
            .write()
            .map_err(|_| NodeError::StorePoisoned)?
            .pty_resources
            .remove(key);
        Ok(())
    }

    fn insert_instance(&self, instance: Instance) -> Result<(), NodeError> {
        let mut state = self.state.write().map_err(|_| NodeError::StorePoisoned)?;
        if state.instances.contains_key(&instance.meta.id) {
            return Err(NodeError::Conflict(format!(
                "instance {} already exists",
                instance.meta.id.as_id()
            )));
        }
        if state.journal_instances.contains_key(&instance.journal_id) {
            return Err(NodeError::Conflict(format!(
                "journal {} already has an owner",
                instance.journal_id
            )));
        }
        let instance_id = instance.meta.id.clone();
        let journal_id = instance.journal_id.clone();
        let persisted = instance.clone();
        let (events_tx, _) = broadcast::channel(self.follow_buffer_capacity);
        state.instances.insert(
            instance_id.clone(),
            InstanceRecord {
                instance,
                command_ids: Vec::new(),
                events: Vec::new(),
                events_tx,
            },
        );
        state.journal_instances.insert(journal_id, instance_id);
        drop(state);
        if let Some(entities) = &self.entities {
            entities.put_instance(&persisted)?;
        }
        Ok(())
    }

    fn list_instances(&self) -> Result<Vec<Instance>, NodeError> {
        let state = self.state.read().map_err(|_| NodeError::StorePoisoned)?;
        Ok(state
            .instances
            .values()
            .map(|record| record.instance.clone())
            .collect())
    }

    fn instance_store_is_durable(&self) -> bool {
        // The rows must have been *found*, not merely given a file to live in:
        // `EntityDb::open` creates `node.sqlite` on a fresh path, so an
        // existing connection proves only that we can write, not that anything
        // was ever here. See `EntityDb::found_existing`.
        self.entities.as_ref().is_some_and(|db| db.found_existing())
    }

    fn get_instance(&self, instance_id: &InstanceId) -> Result<Instance, NodeError> {
        let state = self.state.read().map_err(|_| NodeError::StorePoisoned)?;
        state
            .instances
            .get(instance_id)
            .map(|record| record.instance.clone())
            .ok_or_else(|| not_found("instance", instance_id.as_id().to_string()))
    }

    fn remove_instance(&self, instance_id: &InstanceId) -> Result<bool, NodeError> {
        let mut state = self.state.write().map_err(|_| NodeError::StorePoisoned)?;
        let Some(record) = state.instances.remove(instance_id) else {
            return Ok(false);
        };
        for command_id in &record.command_ids {
            state.commands.remove(command_id);
        }
        state.journal_instances.remove(&record.instance.journal_id);
        drop(state);
        if let Some(entities) = &self.entities {
            entities.remove_instance(instance_id)?;
        }
        Ok(true)
    }

    fn set_instance_state(
        &self,
        instance_id: &InstanceId,
        lifecycle: Option<InstanceLifecycle>,
        activity: Option<Knowledge<Activity>>,
    ) -> Result<Instance, NodeError> {
        let now = timestamp_now()?;
        let mut state = self.state.write().map_err(|_| NodeError::StorePoisoned)?;
        let record = state
            .instances
            .get_mut(instance_id)
            .ok_or_else(|| not_found("instance", instance_id.as_id().to_string()))?;
        if let Some(lifecycle) = lifecycle {
            record.instance.lifecycle = lifecycle;
            if lifecycle == InstanceLifecycle::Exited
                && !matches!(record.instance.exit, Knowledge::Known { .. })
            {
                record.instance.exit = Knowledge::Known {
                    value: remuda_protocol::ProcessExit {
                        code: None,
                        signal: None,
                        observed_at: now.clone(),
                    },
                };
            }
        }
        if let Some(activity) = activity {
            record.instance.activity = activity;
        }
        if matches!(
            record.instance.lifecycle,
            InstanceLifecycle::Exited | InstanceLifecycle::Failed
        ) {
            record.instance.active_run_ids.clear();
        }
        record.instance.meta.revision.0 = record.instance.meta.revision.0.saturating_add(1);
        record.instance.meta.updated_at = now;
        let instance = record.instance.clone();
        drop(state);
        if let Some(entities) = &self.entities {
            entities.put_instance(&instance)?;
        }
        Ok(instance)
    }

    fn set_instance_capabilities(
        &self,
        instance_id: &InstanceId,
        capabilities: remuda_protocol::CapabilitySnapshot,
    ) -> Result<Option<Instance>, NodeError> {
        let now = timestamp_now()?;
        let mut state = self.state.write().map_err(|_| NodeError::StorePoisoned)?;
        let record = state
            .instances
            .get_mut(instance_id)
            .ok_or_else(|| not_found("instance", instance_id.as_id().to_string()))?;
        // The snapshot carries its own `id`, minted per call, so comparing
        // whole snapshots would report a change on every refresh. The
        // capability set is the part callers act on.
        if record.instance.capabilities.capabilities == capabilities.capabilities {
            return Ok(None);
        }
        record.instance.capabilities = capabilities;
        record.instance.meta.revision.0 = record.instance.meta.revision.0.saturating_add(1);
        record.instance.meta.updated_at = now;
        let instance = record.instance.clone();
        drop(state);
        if let Some(entities) = &self.entities {
            entities.put_instance(&instance)?;
        }
        Ok(Some(instance))
    }

    fn set_instance_promotion(
        &self,
        instance_id: &InstanceId,
        kind: remuda_protocol::AgentKind,
        mode: remuda_protocol::InstanceMode,
        promoted_at: Option<remuda_protocol::Timestamp>,
    ) -> Result<Instance, NodeError> {
        let now = timestamp_now()?;
        let mut state = self.state.write().map_err(|_| NodeError::StorePoisoned)?;
        let record = state
            .instances
            .get_mut(instance_id)
            .ok_or_else(|| not_found("instance", instance_id.as_id().to_string()))?;
        let unchanged = record.instance.kind == kind
            && record.instance.mode == Some(mode)
            && record.instance.promoted_at == promoted_at;
        if unchanged {
            return Ok(record.instance.clone());
        }
        // D-028 §1.0 rule 4: `launchedBy` records who ran the launch command,
        // and this is the only moment that can tell. §1.0 rule 2 makes
        // promotion the sole detection path, so *both* launches promote and
        // `mode == promoted` no longer implies a human typed it — the Hub's
        // old inference read every Remuda-launched agent as `user`.
        //
        // What separates them is what the instance was *before*. A session
        // created as `terminal` that becomes `claude` is a human typing into a
        // shell; a session created as `claude` that becomes `claude` is the
        // launch Remuda ran. Settled once, on the first promotion, so a later
        // demote/repromote cycle cannot rewrite the session's origin.
        if mode == remuda_protocol::InstanceMode::Promoted && record.instance.promoted_at.is_none()
        {
            record.instance.launched_by = Some(
                if record.instance.kind == remuda_protocol::AgentKind::Terminal {
                    remuda_protocol::LaunchedBy::User
                } else {
                    remuda_protocol::LaunchedBy::Remuda
                },
            );
        }
        record.instance.kind = kind;
        record.instance.mode = Some(mode);
        record.instance.promoted_at = promoted_at;
        // The native identity now points at the promoted agent, not the shell.
        record.instance.native_ref.kind = kind;
        record.instance.native_ref.signal_tier = None;
        record.instance.meta.revision.0 = record.instance.meta.revision.0.saturating_add(1);
        record.instance.meta.updated_at = now;
        let instance = record.instance.clone();
        drop(state);
        if let Some(entities) = &self.entities {
            entities.put_instance(&instance)?;
        }
        Ok(instance)
    }

    fn set_instance_failure(
        &self,
        instance_id: &InstanceId,
        last_error: &str,
    ) -> Result<Instance, NodeError> {
        let now = timestamp_now()?;
        let mut state = self.state.write().map_err(|_| NodeError::StorePoisoned)?;
        let record = state
            .instances
            .get_mut(instance_id)
            .ok_or_else(|| not_found("instance", instance_id.as_id().to_string()))?;
        record.instance.lifecycle = InstanceLifecycle::Failed;
        record.instance.activity = unknown(last_error);
        record.instance.connectivity = remuda_protocol::Connectivity::Disconnected;
        record.instance.last_error = Some(last_error.to_owned());
        record.instance.active_run_ids.clear();
        record.instance.meta.revision.0 = record.instance.meta.revision.0.saturating_add(1);
        record.instance.meta.updated_at = now;
        let instance = record.instance.clone();
        drop(state);
        if let Some(entities) = &self.entities {
            entities.put_instance(&instance)?;
        }
        Ok(instance)
    }

    fn set_native_session(
        &self,
        instance_id: &InstanceId,
        session_id: &str,
        transcript_path: Option<&str>,
        signal_tier: Option<remuda_protocol::SignalTier>,
    ) -> Result<Option<Instance>, NodeError> {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Ok(None);
        }
        let transcript_path = transcript_path
            .map(str::trim)
            .filter(|path| !path.is_empty());
        let now = timestamp_now()?;
        let mut state = self.state.write().map_err(|_| NodeError::StorePoisoned)?;
        let record = state
            .instances
            .get_mut(instance_id)
            .ok_or_else(|| not_found("instance", instance_id.as_id().to_string()))?;
        let native = &mut record.instance.native_ref;
        let session_known = matches!(
            &native.session_id,
            Knowledge::Known { value } if value == session_id
        );
        let claude_known = native
            .claude
            .as_ref()
            .is_some_and(|claude| claude.session_id == session_id);
        let transcript_known = match (transcript_path, &native.transcript) {
            (None, _) => true,
            (Some(path), Knowledge::Known { value }) => value.source_path == path,
            (Some(_), _) => false,
        };
        if session_known
            && claude_known
            && transcript_known
            && signal_tier.is_none_or(|tier| native.signal_tier == Some(tier))
        {
            return Ok(None);
        }
        native.session_id = Knowledge::Known {
            value: session_id.to_owned(),
        };
        native.claude = Some(remuda_protocol::ClaudeRef {
            session_id: session_id.to_owned(),
        });
        if let Some(tier) = signal_tier {
            native.signal_tier = Some(tier);
        }
        if let Some(path) = transcript_path {
            native.transcript = Knowledge::Known {
                value: remuda_protocol::TranscriptRef {
                    object_id: remuda_protocol::Id::new("obj")?,
                    source_path: path.to_owned(),
                },
            };
        }
        record.instance.meta.revision.0 = record.instance.meta.revision.0.saturating_add(1);
        record.instance.meta.updated_at = now;
        let instance = record.instance.clone();
        drop(state);
        if let Some(entities) = &self.entities {
            entities.put_instance(&instance)?;
        }
        Ok(Some(instance))
    }

    fn insert_command(
        &self,
        instance_id: &InstanceId,
        command: Command,
    ) -> Result<bool, NodeError> {
        let mut state = self.state.write().map_err(|_| NodeError::StorePoisoned)?;
        if let Some(existing) = state.commands.get(&command.command_id) {
            if existing.payload_digest == command.payload_digest
                && existing.operation == command.operation
                && existing.target.instance_id.as_ref() == Some(instance_id)
            {
                return Ok(false);
            }
            return Err(NodeError::Conflict(format!(
                "command {} was reused with different content",
                command.command_id.as_id()
            )));
        }
        let record = state
            .instances
            .get_mut(instance_id)
            .ok_or_else(|| not_found("instance", instance_id.as_id().to_string()))?;
        record.command_ids.push(command.command_id.clone());
        state
            .commands
            .insert(command.command_id.clone(), command.clone());
        drop(state);
        if let Some(entities) = &self.entities {
            entities.put_command(instance_id, &command)?;
        }
        Ok(true)
    }

    fn get_command(&self, command_id: &CommandId) -> Result<Command, NodeError> {
        let state = self.state.read().map_err(|_| NodeError::StorePoisoned)?;
        state
            .commands
            .get(command_id)
            .cloned()
            .ok_or_else(|| not_found("command", command_id.as_id().to_string()))
    }

    fn save_command(&self, command: Command) -> Result<(), NodeError> {
        let mut state = self.state.write().map_err(|_| NodeError::StorePoisoned)?;
        let existing = state
            .commands
            .get(&command.command_id)
            .ok_or_else(|| not_found("command", command.command_id.as_id().to_string()))?;
        if command.meta.revision < existing.meta.revision {
            return Err(NodeError::Conflict(format!(
                "command {} revision moved backwards",
                command.command_id.as_id()
            )));
        }
        state
            .commands
            .insert(command.command_id.clone(), command.clone());
        drop(state);
        if let Some(entities) = &self.entities
            && let Some(instance_id) = command.target.instance_id.as_ref()
        {
            entities.put_command(instance_id, &command)?;
        }
        Ok(())
    }

    fn mark_unsettled_unknown(&self, instance_id: &InstanceId) -> Result<(), NodeError> {
        let now = timestamp_now()?;
        let mut state = self.state.write().map_err(|_| NodeError::StorePoisoned)?;
        let command_ids = state
            .instances
            .get(instance_id)
            .ok_or_else(|| not_found("instance", instance_id.as_id().to_string()))?
            .command_ids
            .clone();
        for command_id in command_ids {
            if let Some(command) = state.commands.get_mut(&command_id)
                && command.state != CommandState::Settled
            {
                command.resolution = ResolutionState::Unknown;
                command.meta.revision.0 = command.meta.revision.0.saturating_add(1);
                command.meta.updated_at = now.clone();
                if let Some(entities) = &self.entities {
                    entities.put_command(instance_id, command)?;
                }
            }
        }
        Ok(())
    }

    fn append_observation(
        &self,
        instance_id: &InstanceId,
        run_id: Option<RunId>,
        completeness: Completeness,
        body: ObservationPayload,
    ) -> Result<Observation, NodeError> {
        let now = timestamp_now()?;
        let (event, sender, persisted) = {
            let mut state = self.state.write().map_err(|_| NodeError::StorePoisoned)?;
            let record = state
                .instances
                .get_mut(instance_id)
                .ok_or_else(|| not_found("instance", instance_id.as_id().to_string()))?;
            let seq = U64(record.instance.durable_seq.0.saturating_add(1));
            let envelope = Envelope {
                journal_id: record.instance.journal_id.clone(),
                instance_id: instance_id.clone(),
                run_id: run_id.clone(),
                host_id: record.instance.host_id.clone(),
                process_generation: record.instance.process_ref.process_generation,
                run_generation: run_id.map(|_| U64(1)),
                observed_at: now.clone(),
                native_at: unknown("not-emitted"),
                source: runtime_source(&record.instance, seq),
                completeness,
                evidence_event_ids: Vec::new(),
                event_id: None,
                body,
                raw: None,
            };
            let event = match self.durable.as_ref() {
                Some(durable) => durable.append(instance_id, envelope)?,
                None => in_memory_observation(envelope, seq),
            };
            record.instance.durable_seq = event.seq;
            record.instance.meta.updated_at = now;
            let persisted = record.instance.clone();
            let journal_event = JournalEvent::Instance(Box::new(event.clone()));
            record.events.push(journal_event.clone());
            (event, (record.events_tx.clone(), journal_event), persisted)
        };
        let _ = sender.0.send(sender.1);
        if let Some(entities) = &self.entities {
            entities.put_instance(&persisted)?;
        }
        Ok(event)
    }

    fn append_driver_observation(
        &self,
        instance_id: &InstanceId,
        mut observation: Observation,
    ) -> Result<Observation, NodeError> {
        let (event, sender, persisted) = {
            let mut state = self.state.write().map_err(|_| NodeError::StorePoisoned)?;
            let record = state
                .instances
                .get_mut(instance_id)
                .ok_or_else(|| not_found("instance", instance_id.as_id().to_string()))?;
            let seq = U64(record.instance.durable_seq.0.saturating_add(1));
            // Drivers mint local envelope IDs; bind nested interaction entities
            // to the actual Node-owned instance before journaling or brokering.
            let interaction = match &mut observation.body {
                ObservationPayload::InteractionRequested(payload) => Some(&mut payload.interaction),
                ObservationPayload::Lifecycle(payload) => match payload.as_mut() {
                    remuda_protocol::LifecyclePayload::Entity(entity) => {
                        match &mut entity.entity_value {
                            remuda_protocol::LifecycleEntity::Interaction(interaction) => {
                                Some(interaction.as_mut())
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some(interaction) = interaction {
                interaction.instance_id = instance_id.clone();
                interaction.host_id = record.instance.host_id.clone();
                interaction.request_key.process_generation =
                    record.instance.process_ref.process_generation;
            }
            let event = if let Some(durable) = self.durable.as_ref() {
                durable.append(
                    instance_id,
                    Envelope {
                        journal_id: record.instance.journal_id.clone(),
                        instance_id: instance_id.clone(),
                        run_id: observation.run_id,
                        host_id: record.instance.host_id.clone(),
                        process_generation: record.instance.process_ref.process_generation,
                        run_generation: observation.run_generation,
                        observed_at: observation.observed_at,
                        native_at: observation.native_at,
                        source: observation.source,
                        completeness: observation.completeness,
                        evidence_event_ids: observation.evidence_event_ids,
                        // Runtime-emitted observations keep their own id.
                        event_id: Some(observation.event_id.clone()),
                        body: observation.body,
                        raw: None,
                    },
                )?
            } else {
                observation.event_id = EventId::new();
                observation.journal_id = record.instance.journal_id.clone();
                observation.instance_id = instance_id.clone();
                observation.host_id = record.instance.host_id.clone();
                observation.process_generation = record.instance.process_ref.process_generation;
                observation.seq = seq;
                observation
            };
            record.instance.durable_seq = event.seq;
            record.instance.meta.updated_at = event.observed_at.clone();
            let persisted = record.instance.clone();
            let journal_event = JournalEvent::Instance(Box::new(event.clone()));
            record.events.push(journal_event.clone());
            (event, (record.events_tx.clone(), journal_event), persisted)
        };
        let _ = sender.0.send(sender.1);
        if let Some(entities) = &self.entities {
            entities.put_instance(&persisted)?;
        }
        Ok(event)
    }

    fn read_events(
        &self,
        journal_id: &remuda_protocol::Id,
        after_seq: Option<U64>,
        limit: usize,
    ) -> Result<EventsReadResult, NodeError> {
        let state = self.state.read().map_err(|_| NodeError::StorePoisoned)?;
        let instance_id = state
            .journal_instances
            .get(journal_id)
            .cloned()
            .ok_or_else(|| not_found("journal", journal_id.to_string()))?;
        let durable_seq = state
            .instances
            .get(&instance_id)
            .ok_or_else(|| not_found("instance", instance_id.as_id().to_string()))?
            .instance
            .durable_seq;
        let after = after_seq.unwrap_or_default().0;
        let limit = limit.clamp(1, 256);
        if let Some(durable) = &self.durable {
            drop(state);
            // The bound travels with the read. `to_seq = None` would ask the
            // journal for every observation from `after + 1` to the tail, and
            // the `take` below would then discard all but the first page —
            // 20 MB of JSON parsed off a 20 MB journal to move 256 events.
            let to_seq = Some(after.saturating_add(limit as u64));
            let page = durable.read_page(&instance_id, after + 1, to_seq, limit)?;
            self.journal_bytes_read
                .fetch_add(page.bytes_read, std::sync::atomic::Ordering::Relaxed);
            let events = page
                .observations
                .into_iter()
                .map(|event| JournalEvent::Instance(Box::new(event)))
                .collect();
            return Ok(EventsReadResult {
                events,
                next_cursor: None,
                floor_seq: U64(1),
                durable_seq,
            });
        }
        let record = state
            .instances
            .get(&instance_id)
            .ok_or_else(|| not_found("instance", instance_id.as_id().to_string()))?;
        let events = record
            .events
            .iter()
            .filter(|event| event.position().1.0 > after)
            .take(limit.clamp(1, 256))
            .cloned()
            .collect();
        Ok(EventsReadResult {
            events,
            next_cursor: None,
            floor_seq: U64(1),
            durable_seq: record.instance.durable_seq,
        })
    }

    fn journal_bytes_read(&self) -> u64 {
        self.journal_bytes_read
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn journal_durable_seq(&self, journal_id: &remuda_protocol::Id) -> Result<U64, NodeError> {
        let instance_id = self
            .state
            .read()
            .map_err(|_| NodeError::StorePoisoned)?
            .journal_instances
            .get(journal_id)
            .cloned()
            .ok_or_else(|| not_found("journal", journal_id.to_string()))?;
        match &self.durable {
            Some(durable) => durable.durable_seq(&instance_id),
            None => Ok(self
                .state
                .read()
                .map_err(|_| NodeError::StorePoisoned)?
                .instances
                .get(&instance_id)
                .map(|record| record.instance.durable_seq)
                .unwrap_or_default()),
        }
    }

    fn snapshot(
        &self,
        instance_id: &InstanceId,
        projection_epoch: remuda_protocol::Id,
    ) -> Result<InstanceSnapshot, NodeError> {
        let state = self.state.read().map_err(|_| NodeError::StorePoisoned)?;
        let record = state
            .instances
            .get(instance_id)
            .ok_or_else(|| not_found("instance", instance_id.as_id().to_string()))?;
        let commands = record
            .command_ids
            .iter()
            .filter_map(|command_id| state.commands.get(command_id).cloned())
            .collect();
        let nodes = record.events.iter().filter_map(conversation_node).collect();
        Ok(InstanceSnapshot {
            projection_version: "v1".to_owned(),
            projection_epoch,
            as_of_seq: record.instance.durable_seq,
            instance: record.instance.clone(),
            runs: Vec::new(),
            commands,
            pending_interactions: Vec::new(),
            nodes,
            history: HistoryCoverage {
                earliest_retained_seq: U64(1),
                complete: true,
            },
        })
    }

    fn subscribe(
        &self,
        instance_id: &InstanceId,
    ) -> Result<broadcast::Receiver<JournalEvent>, NodeError> {
        let state = self.state.read().map_err(|_| NodeError::StorePoisoned)?;
        state
            .instances
            .get(instance_id)
            .map(|record| record.events_tx.subscribe())
            .ok_or_else(|| not_found("instance", instance_id.as_id().to_string()))
    }

    fn instance_for_journal(
        &self,
        journal_id: &remuda_protocol::Id,
    ) -> Result<InstanceId, NodeError> {
        let state = self.state.read().map_err(|_| NodeError::StorePoisoned)?;
        state
            .journal_instances
            .get(journal_id)
            .cloned()
            .ok_or_else(|| not_found("journal", journal_id.to_string()))
    }

    fn put_launch_recipe(
        &self,
        instance_id: &InstanceId,
        recipe: &LaunchRecipe,
    ) -> Result<(), NodeError> {
        if let Some(entities) = &self.entities {
            entities.put_recipe(instance_id, recipe)?;
        }
        Ok(())
    }

    fn launch_recipe(&self, instance_id: &InstanceId) -> Result<Option<LaunchRecipe>, NodeError> {
        match &self.entities {
            Some(entities) => entities.recipe(instance_id),
            None => Ok(None),
        }
    }

    fn put_pending_interaction(&self, pending: &PendingInteraction) -> Result<(), NodeError> {
        if let Some(entities) = &self.entities {
            let json = serde_json::to_string(pending)?;
            entities.put_interaction_json(pending.interaction_id.as_id().as_str(), &json)?;
        }
        Ok(())
    }

    fn pending_interactions(&self) -> Result<Vec<PendingInteraction>, NodeError> {
        let Some(entities) = &self.entities else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for json in entities.list_interaction_jsons()? {
            out.push(serde_json::from_str(&json)?);
        }
        Ok(out)
    }

    fn shutdown_journal(&self) {
        MemoryStore::shutdown_journal(self);
    }
}

fn in_memory_observation(envelope: Envelope, seq: U64) -> Observation {
    Observation {
        schema_version: SchemaVersion,
        event_id: EventId::new(),
        journal_id: envelope.journal_id,
        instance_id: envelope.instance_id,
        run_id: envelope.run_id,
        host_id: envelope.host_id,
        process_generation: envelope.process_generation,
        run_generation: envelope.run_generation,
        seq,
        observed_at: envelope.observed_at,
        native_at: envelope.native_at,
        source: envelope.source,
        completeness: envelope.completeness,
        raw_ref: None,
        evidence_event_ids: envelope.evidence_event_ids,
        body: envelope.body,
    }
}

fn conversation_node(event: &JournalEvent) -> Option<ConversationNode> {
    let JournalEvent::Instance(event) = event else {
        return None;
    };
    match &event.body {
        ObservationPayload::Message(payload) => Some(ConversationNode::Message(payload.clone())),
        ObservationPayload::Thought(payload) => Some(ConversationNode::Thought(payload.clone())),
        ObservationPayload::ToolCall(payload) => Some(ConversationNode::ToolCall(payload.clone())),
        ObservationPayload::ToolResult(payload) => {
            Some(ConversationNode::ToolResult(payload.clone()))
        }
        _ => None,
    }
}

pub(crate) fn timestamp_now() -> Result<remuda_protocol::Timestamp, NodeError> {
    let now = OffsetDateTime::now_utc();
    let text = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.millisecond()
    );
    Ok(text.try_into()?)
}

pub(crate) fn unknown<T>(reason: &str) -> Knowledge<T> {
    Knowledge::Unknown {
        reason: reason.to_owned(),
        evidence_event_ids: Vec::new(),
    }
}

fn not_found(entity: &'static str, id: String) -> NodeError {
    NodeError::NotFound { entity, id }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fixture_instance;

    #[test]
    fn journal_pages_exclude_the_cursor() {
        let store = MemoryStore::new(4);
        let instance = fixture_instance(
            InstanceId::new(),
            remuda_protocol::HostId::new(),
            remuda_protocol::WorkspaceId::new(),
            remuda_protocol::DriverKind::ClaudePrint,
        )
        .expect("fixture instance");
        let journal_id = instance.journal_id.clone();
        let instance_id = instance.meta.id.clone();
        store.insert_instance(instance).expect("insert instance");
        let body = crate::driver::message_payload(
            remuda_protocol::MessageRole::Assistant,
            remuda_protocol::MessagePhase::Final,
            "one".to_owned(),
            Vec::new(),
        )
        .expect("message");
        store
            .append_observation(&instance_id, None, Completeness::Structured, body)
            .expect("append one");
        let body = crate::driver::message_payload(
            remuda_protocol::MessageRole::Assistant,
            remuda_protocol::MessagePhase::Final,
            "two".to_owned(),
            Vec::new(),
        )
        .expect("message");
        store
            .append_observation(&instance_id, None, Completeness::Structured, body)
            .expect("append two");
        let page = store
            .read_events(&journal_id, Some(U64(1)), 10)
            .expect("read page");
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].position().1, U64(2));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn journaled_store_commits_before_publishing() {
        let data_dir = tempfile::tempdir().expect("journal data dir");
        let store = MemoryStore::open_journaled(data_dir.path(), 4).expect("journaled store");
        let instance = fixture_instance(
            InstanceId::new(),
            remuda_protocol::HostId::new(),
            remuda_protocol::WorkspaceId::new(),
            remuda_protocol::DriverKind::ClaudePrint,
        )
        .expect("fixture instance");
        let instance_id = instance.meta.id.clone();
        store.insert_instance(instance).expect("insert instance");
        let mut changes = store.subscribe(&instance_id).expect("subscribe");
        let body = crate::driver::message_payload(
            remuda_protocol::MessageRole::Assistant,
            remuda_protocol::MessagePhase::Final,
            "durable".to_owned(),
            Vec::new(),
        )
        .expect("message");

        let committed = store
            .append_observation(&instance_id, None, Completeness::Structured, body)
            .expect("append durable observation");
        let published = changes.recv().await.expect("published observation");
        let JournalEvent::Instance(published) = published else {
            panic!("instance observation")
        };
        assert_eq!(published.event_id, committed.event_id);
        assert_eq!(published.seq, U64(1));

        let journal = store.journal().expect("journal handle");
        assert_eq!(
            journal
                .durable_seq(&instance_id)
                .await
                .expect("durable seq"),
            U64(1)
        );
        let persisted = journal
            .read_range(&instance_id, U64(1), Some(U64(1)))
            .await
            .expect("persisted observation");
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].event_id, committed.event_id);
    }

    /// Write `count` observations of roughly `pad` bytes each into a journal's
    /// JSONL, for a store that has not opened it yet.
    ///
    /// The lines are exactly what the writer would have produced, so opening
    /// the store afterwards indexes them through the normal `recover()` path —
    /// one linear pass, the same as a real restart. Seeding through
    /// `Journal::append` instead is quadratic: every append re-serializes the
    /// whole projection into the checkpoint row, which at ~7000 events is
    /// minutes of CPU that has nothing to do with the reader under test.
    fn write_fixture_journal(data_dir: &Path, instance: &Instance, count: usize, pad: usize) {
        let filler = "x".repeat(pad);
        let dir = data_dir.join("journal");
        std::fs::create_dir_all(&dir).expect("fixture journal dir");
        let path = dir.join(format!("{}.jsonl", instance.meta.id.as_id().as_str()));
        let mut file = std::fs::File::create(&path).expect("fixture journal");
        for index in 0..count {
            let body = crate::driver::message_payload(
                remuda_protocol::MessageRole::Assistant,
                remuda_protocol::MessagePhase::Final,
                format!("{index}-{filler}"),
                Vec::new(),
            )
            .expect("message");
            let observation = Observation {
                schema_version: SchemaVersion,
                event_id: EventId::new(),
                journal_id: instance.journal_id.clone(),
                instance_id: instance.meta.id.clone(),
                run_id: None,
                host_id: instance.host_id.clone(),
                process_generation: U64(1),
                run_generation: None,
                seq: U64(index as u64 + 1),
                observed_at: timestamp_now().expect("timestamp"),
                native_at: unknown("fixture"),
                source: crate::driver::runtime_source(instance, U64(index as u64 + 1)),
                completeness: Completeness::Structured,
                raw_ref: None,
                evidence_event_ids: Vec::new(),
                body,
            };
            let mut line = serde_json::to_vec(&observation).expect("fixture observation");
            line.push(b'\n');
            std::io::Write::write_all(&mut file, &line).expect("write fixture observation");
        }
        std::io::Write::flush(&mut file).expect("flush fixture journal");
    }

    /// Open a journaled store over a fixture journal of `count` events, with
    /// `pad` bytes of payload each, and bring the instance mirror up to date.
    fn store_over_fixture(data_dir: &Path, count: usize, pad: usize) -> (MemoryStore, Instance) {
        let instance = fixture_instance(
            InstanceId::new(),
            remuda_protocol::HostId::new(),
            remuda_protocol::WorkspaceId::new(),
            remuda_protocol::DriverKind::ClaudePrint,
        )
        .expect("fixture instance");
        write_fixture_journal(data_dir, &instance, count, pad);
        let store = MemoryStore::open_journaled_with(data_dir, 4, FsyncPolicy::Never)
            .expect("journaled store");
        store
            .insert_instance(instance.clone())
            .expect("insert instance");
        // Opening the store recovered the JSONL; the in-memory mirror must
        // agree with the journal's own watermark.
        assert_eq!(
            store
                .journal_durable_seq(&instance.journal_id)
                .expect("durable seq"),
            U64(count as u64)
        );
        store
            .state
            .write()
            .expect("state")
            .instances
            .get_mut(&instance.meta.id)
            .expect("instance record")
            .instance
            .durable_seq = U64(count as u64);
        (store, instance)
    }

    /// Process CPU time (user + system) from `/proc/self/stat`, in ticks.
    ///
    /// Wall time would measure the test harness as much as the reader; the
    /// question the evidence answers is how much CPU one sweep burns.
    #[cfg(target_os = "linux")]
    fn cpu_ticks() -> u64 {
        let stat = std::fs::read_to_string("/proc/self/stat").expect("self stat");
        // Fields 14 and 15 (utime, stime) follow the parenthesised comm, which
        // may itself contain spaces — split after the last ')'.
        let rest = stat.rsplit_once(')').expect("comm").1;
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let utime: u64 = fields[11].parse().expect("utime");
        let stime: u64 = fields[12].parse().expect("stime");
        utime + stime
    }

    /// Measure one full replay sweep both ways over a ~20 MB journal.
    ///
    /// Ignored by default: it is evidence, not a gate. Run with
    /// `cargo test -p remuda-node --lib replay_sweep_cost -- --ignored --nocapture`.
    ///
    /// "Before" is the old shape verbatim — `read_range(after + 1, None)` plus
    /// `take(256)` — so the numbers are the same work the demo host was doing
    /// per 250 ms tick. "After" is one bounded page per call.
    ///
    /// Linux-only because the CPU figure comes from `/proc/self/stat`; the
    /// helper it calls is gated the same way, so without this the whole test
    /// module fails to compile on macOS (`cpu_ticks` not found) and takes
    /// `cargo check/clippy --all-targets` with it.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "measurement, not a gate"]
    async fn replay_sweep_cost_is_linear_in_new_events() {
        const EVENTS: usize = 7_000;
        const PAD: usize = 2_800;
        const PAGE: u64 = 256;
        /// Matches `UPLINK_WINDOW` in the wss carrier.
        const UPLINK_WINDOW: usize = 16;
        let data_dir = tempfile::tempdir().expect("journal data dir");
        let (store, instance) = store_over_fixture(data_dir.path(), EVENTS, PAD);
        let journal_id = instance.journal_id.clone();
        let journal = store.journal().expect("journal handle");
        let journal_bytes = std::fs::metadata(
            data_dir
                .path()
                .join("journal")
                .join(format!("{}.jsonl", instance.meta.id.as_id().as_str())),
        )
        .expect("journal file")
        .len();

        // Before: every sweep parses the whole tail, 28 pages per full replay.
        let pages = EVENTS as u64 / PAGE + 1;
        let cpu_before_start = cpu_ticks();
        let started = std::time::Instant::now();
        let mut parsed = 0u64;
        for _ in 0..pages {
            let all = journal
                .read_range(&instance.meta.id, U64(1), None)
                .await
                .expect("unbounded read");
            parsed += all.len() as u64;
        }
        let before = started.elapsed();
        let before_cpu = cpu_ticks() - cpu_before_start;

        // After: one bounded page per sweep, resuming at the cursor.
        let started = std::time::Instant::now();
        let mut cursor = 0u64;
        let mut moved = 0u64;
        let mut reads = 0u64;
        loop {
            let page = store
                .read_events(&journal_id, Some(U64(cursor)), PAGE as usize)
                .expect("bounded read");
            reads += 1;
            if page.events.is_empty() {
                break;
            }
            cursor = page.events.last().expect("last").position().1.0;
            moved += page.events.len() as u64;
        }
        let after = started.elapsed();
        let after_cpu = cpu_ticks() - cpu_before_start - before_cpu;
        let bytes_read = store.journal_bytes_read();

        // The demo's pathology, isolated: a tick whose ACK window is stuck
        // early with the whole journal still ahead of it. "Before" parses
        // everything from the stuck watermark to the tail and keeps 16.
        let stuck_from = 1u64;
        let cpu_stuck = cpu_ticks();
        let started = std::time::Instant::now();
        let unbounded = journal
            .read_range(&instance.meta.id, U64(stuck_from + 1), None)
            .await
            .expect("stuck unbounded read");
        let dropped = unbounded.len() - unbounded.len().min(UPLINK_WINDOW);
        let stuck_before = started.elapsed();
        let stuck_before_cpu = cpu_ticks() - cpu_stuck;
        let started = std::time::Instant::now();
        let bounded = store
            .read_events(&journal_id, Some(U64(stuck_from)), UPLINK_WINDOW)
            .expect("stuck bounded read");
        let stuck_after = started.elapsed();
        let stuck_after_cpu = cpu_ticks() - cpu_stuck - stuck_before_cpu;

        println!(
            "journal_bytes={journal_bytes} events={EVENTS}\n\
             sweep  before: {pages} unbounded pages, {parsed} observations parsed, \
             wall {before:?}, cpu {before_cpu} ticks\n\
             sweep  after:  {reads} bounded pages, {moved} observations moved, \
             {bytes_read} JSONL bytes, wall {after:?}, cpu {after_cpu} ticks\n\
             stuck  before: wall {stuck_before:?}, cpu {stuck_before_cpu} ticks \
             to move {UPLINK_WINDOW} events (parsed {}, dropped {dropped})\n\
             stuck  after:  wall {stuck_after:?}, cpu {stuck_after_cpu} ticks \
             to move {} events",
            unbounded.len(),
            bounded.events.len()
        );
        assert_eq!(moved, EVENTS as u64, "every event still moves exactly once");
        assert_eq!(bounded.events.len(), UPLINK_WINDOW);
        assert!(
            bytes_read <= journal_bytes,
            "a bounded sweep over a tail-only journal reads at most the journal"
        );
    }

    /// One flush tick over a ~20 MB journal must cost O(new events).
    ///
    /// The pre-fix reader asked for `read_range(after + 1, None)` and dropped
    /// all but the first page, so reading the one event appended after a
    /// 7000-event journal deserialized the whole 20 MB again. The bound now
    /// travels with the read, so the tick's byte cost tracks the new event.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_flush_tick_costs_new_bytes_not_the_whole_journal() {
        const EVENTS: usize = 7_000;
        const PAD: usize = 2_800;
        let data_dir = tempfile::tempdir().expect("journal data dir");
        let (store, instance) = store_over_fixture(data_dir.path(), EVENTS, PAD);
        let journal_id = instance.journal_id.clone();
        let instance_id = instance.meta.id.clone();

        let journal_path = data_dir
            .path()
            .join("journal")
            .join(format!("{}.jsonl", instance_id.as_id().as_str()));
        let journal_bytes = std::fs::metadata(&journal_path)
            .expect("journal file")
            .len();
        assert!(
            journal_bytes >= 20 * 1024 * 1024,
            "fixture journal is {journal_bytes} bytes; the test needs ~20 MB"
        );

        // The one event the Hub is missing.
        let body = crate::driver::message_payload(
            remuda_protocol::MessageRole::Assistant,
            remuda_protocol::MessagePhase::Final,
            format!("tail-{}", "y".repeat(PAD)),
            Vec::new(),
        )
        .expect("message");
        store
            .append_observation(&instance_id, None, Completeness::Structured, body)
            .expect("append tail observation");

        let before = store.journal_bytes_read();
        let page = store
            .read_events(&journal_id, Some(U64(EVENTS as u64)), 256)
            .expect("flush tick");
        let cost = store.journal_bytes_read() - before;

        assert_eq!(page.events.len(), 1, "exactly the new event");
        assert_eq!(page.events[0].position().1, U64(EVENTS as u64 + 1));
        assert_eq!(page.durable_seq, U64(EVENTS as u64 + 1));
        assert_eq!(page.floor_seq, U64(1));
        assert!(
            cost < journal_bytes / 100,
            "one flush tick read {cost} bytes off a {journal_bytes}-byte journal; \
             it must track the new event, not the journal"
        );

        // Caught up: the next tick reads nothing at all.
        let before = store.journal_bytes_read();
        let page = store
            .read_events(&journal_id, Some(U64(EVENTS as u64 + 1)), 256)
            .expect("caught-up tick");
        assert!(page.events.is_empty());
        assert_eq!(
            store.journal_bytes_read(),
            before,
            "a caught-up tick must not read the journal"
        );
    }

    /// A bounded page still pages: a reader behind the tail gets the first
    /// `limit` events, never the whole journal.
    #[test]
    fn a_page_read_is_bounded_by_its_limit() {
        let data_dir = tempfile::tempdir().expect("journal data dir");
        let (store, instance) = store_over_fixture(data_dir.path(), 600, 200);
        let journal_id = instance.journal_id.clone();
        let journal_path = data_dir
            .path()
            .join("journal")
            .join(format!("{}.jsonl", instance.meta.id.as_id().as_str()));
        let journal_bytes = std::fs::metadata(&journal_path)
            .expect("journal file")
            .len();

        let before = store.journal_bytes_read();
        let page = store
            .read_events(&journal_id, None, 16)
            .expect("bounded page");
        let cost = store.journal_bytes_read() - before;
        assert_eq!(page.events.len(), 16, "the limit reaches the reader");
        assert_eq!(page.events[0].position().1, U64(1));
        assert_eq!(page.events[15].position().1, U64(16));
        assert_eq!(page.durable_seq, U64(600), "durable seq is the tail");
        // 16 events' worth, not the 600-event journal.
        assert!(
            cost < journal_bytes / 10,
            "16-event page read {cost} bytes of a {journal_bytes}-byte journal"
        );

        // The next page resumes exactly where the first ended.
        let next = store
            .read_events(&journal_id, Some(U64(16)), 16)
            .expect("next page");
        assert_eq!(next.events.len(), 16);
        assert_eq!(next.events[0].position().1, U64(17));
    }
}
