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
}

/// Thread-safe in-memory store used by `remuda dev` and local API tests.
pub struct MemoryStore {
    state: RwLock<MemoryState>,
    follow_buffer_capacity: usize,
    durable: Option<DurableJournal>,
    entities: Option<EntityDb>,
}

#[derive(Clone)]
struct DurableJournal {
    journal: Journal,
    jobs: std_mpsc::SyncSender<JournalJob>,
}

enum JournalJob {
    Append {
        instance_id: InstanceId,
        envelope: Box<Envelope>,
        reply: std_mpsc::SyncSender<Result<Observation, String>>,
    },
    ReadRange {
        instance_id: InstanceId,
        from_seq: u64,
        to_seq: Option<u64>,
        reply: std_mpsc::SyncSender<Result<Vec<Observation>, String>>,
    },
    DurableSeq {
        instance_id: InstanceId,
        reply: std_mpsc::SyncSender<Result<U64, String>>,
    },
}

impl DurableJournal {
    fn open(data_dir: &Path, queue_capacity: usize, fsync: FsyncPolicy) -> Result<Self, NodeError> {
        let journal = Journal::open_with(data_dir, JournalOptions { fsync })
            .map_err(|error| NodeError::Driver(format!("journal open failed: {error}")))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| NodeError::Driver(format!("journal runtime failed: {error}")))?;
        let (jobs, receiver) = std_mpsc::sync_channel::<JournalJob>(queue_capacity.max(1));
        let writer = journal.clone();
        thread::Builder::new()
            .name("remuda-node-journal".to_owned())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    match job {
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
                        JournalJob::ReadRange {
                            instance_id,
                            from_seq,
                            to_seq,
                            reply,
                        } => {
                            let result = runtime.block_on(async {
                                writer
                                    .read_range(&instance_id, U64(from_seq), to_seq.map(U64))
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
        Ok(Self { journal, jobs })
    }

    fn append(
        &self,
        instance_id: &InstanceId,
        envelope: Envelope,
    ) -> Result<Observation, NodeError> {
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

    fn read_range(
        &self,
        instance_id: &InstanceId,
        from_seq: u64,
        to_seq: Option<u64>,
    ) -> Result<Vec<Observation>, NodeError> {
        let (reply, result) = std_mpsc::sync_channel(1);
        self.jobs
            .send(JournalJob::ReadRange {
                instance_id: instance_id.clone(),
                from_seq,
                to_seq,
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

impl MemoryStore {
    /// Create an empty store with a bounded per-Instance broadcast ring.
    pub fn new(follow_buffer_capacity: usize) -> Self {
        Self {
            state: RwLock::new(MemoryState::default()),
            follow_buffer_capacity: follow_buffer_capacity.max(1),
            durable: None,
            entities: None,
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
        record.instance.kind = kind;
        record.instance.mode = Some(mode);
        record.instance.promoted_at = promoted_at;
        // The native identity now points at the promoted agent, not the shell.
        record.instance.native_ref.kind = kind;
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
        let durable = self.durable.clone();
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
                body,
                raw: None,
            };
            let event = match durable.as_ref() {
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
        let durable = self.durable.clone();
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
            let event = if let Some(durable) = durable.as_ref() {
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
        if let Some(durable) = &self.durable {
            drop(state);
            let observations = durable.read_range(&instance_id, after + 1, None)?;
            let events = observations
                .into_iter()
                .take(limit.clamp(1, 256))
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
        )
        .expect("message");
        store
            .append_observation(&instance_id, None, Completeness::Structured, body)
            .expect("append one");
        let body = crate::driver::message_payload(
            remuda_protocol::MessageRole::Assistant,
            remuda_protocol::MessagePhase::Final,
            "two".to_owned(),
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
}
