//! Local Node composition and per-Instance task supervision.

use crate::{
    CommandAction, CreateInstanceRequest, CreateInstanceResponse, Driver, DriverEmission,
    DriverLaunch, DriverRegistry, DriverRequest, InstanceCommandRequest, InteractionRuntime,
    LocalStore, MemoryStore, NodeError,
    store::{timestamp_now, unknown},
};
use remuda_protocol::{
    Acceptance, AcceptanceScope, Activity, ActorRef, ActorType, AgentKind, Capability,
    CapabilitySet, CapabilitySnapshot, CapabilityState, ClaudeRef, Command, CommandAuthority,
    CommandId, CommandOperation, CommandOrigin, CommandResult, CommandState, CommandTarget,
    Completeness, Connectivity, Digest as WireDigest, DispatchState, EntityLifecycle, EntityMeta,
    ExpectedState, Host, HostId, HostState, HostTransport, HostTransportMode, Id, Instance,
    InstanceId, InstanceLifecycle, JournalEvent, Knowledge, LifecycleEntity, LifecyclePayload,
    MessagePhase, MessageRole, NativeRef, NodeReceipt, ObservationPayload, Ownership, Page,
    PathStyle, Platform, ProcessRef, ResolutionState, Settlement, SettlementOutcome, U64,
    Workspace, WorkspaceId, WorkspaceState, WritePolicy,
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};
use tokio::sync::{RwLock, mpsc, oneshot};

const COMMAND_ACK_TIMEOUT: Duration = Duration::from_secs(2);

struct QueuedCommand {
    command_id: CommandId,
    request: DriverRequest,
    accepted_tx: oneshot::Sender<Result<Command, NodeError>>,
    close_after: bool,
}

struct DevNodeInner {
    store: Arc<dyn LocalStore>,
    drivers: DriverRegistry,
    interactions: Arc<InteractionRuntime>,
    senders: RwLock<BTreeMap<InstanceId, mpsc::Sender<QueuedCommand>>>,
    queue_capacity: usize,
    host: Host,
    workspace: Workspace,
    projection_epoch: Id,
}

/// In-process Node used by the development REST/JSON-RPC/WS surface.
#[derive(Clone)]
pub struct DevNode {
    inner: Arc<DevNodeInner>,
}

impl DevNode {
    /// Build a Node with a bounded in-memory store and the default FakeDriver.
    pub fn new(config: &crate::DevServerConfig) -> Result<Self, NodeError> {
        Self::with_host_id(config, HostId::new())
    }

    /// Build a Node whose Host identity matches a persisted enrollment.
    pub fn with_host_id(
        config: &crate::DevServerConfig,
        host_id: HostId,
    ) -> Result<Self, NodeError> {
        let store = Arc::new(MemoryStore::new(config.follow_buffer_capacity));
        let drivers = DriverRegistry::with_fake()?;
        Self::with_parts_on_host(config, store, drivers, host_id)
    }

    /// Build a Node around externally supplied store and driver trait objects.
    pub fn with_parts(
        config: &crate::DevServerConfig,
        store: Arc<dyn LocalStore>,
        drivers: DriverRegistry,
    ) -> Result<Self, NodeError> {
        Self::with_parts_on_host(config, store, drivers, HostId::new())
    }

    /// Build a Node around supplied parts and a persisted Host identity.
    pub fn with_parts_on_host(
        config: &crate::DevServerConfig,
        store: Arc<dyn LocalStore>,
        drivers: DriverRegistry,
        host_id: HostId,
    ) -> Result<Self, NodeError> {
        if config.instance_queue_capacity == 0 {
            return Err(NodeError::InvalidConfig(
                "instance queue capacity must be positive".to_owned(),
            ));
        }
        let workspace_id = WorkspaceId::new();
        let host = fixture_host(host_id.clone())?;
        let workspace = fixture_workspace(workspace_id, host_id, &config.workspace_root)?;
        let interactions = InteractionRuntime::spawn(Arc::clone(&store))?;
        Ok(Self {
            inner: Arc::new(DevNodeInner {
                store,
                drivers,
                interactions,
                senders: RwLock::new(BTreeMap::new()),
                queue_capacity: config.instance_queue_capacity,
                host,
                workspace,
                projection_epoch: Id::new("epoch")?,
            }),
        })
    }

    /// Return the local development Host registry record.
    pub fn host(&self) -> Host {
        self.inner.host.clone()
    }

    /// Return the local development Workspace registry record.
    pub fn workspace(&self) -> Workspace {
        self.inner.workspace.clone()
    }

    /// List current Instances.
    pub fn list_instances(&self) -> Result<Page<Instance>, NodeError> {
        Ok(Page {
            items: self.inner.store.list_instances()?,
            next_cursor: None,
        })
    }

    /// Read one Instance.
    pub fn get_instance(&self, instance_id: &InstanceId) -> Result<Instance, NodeError> {
        self.inner.store.get_instance(instance_id)
    }

    /// Create an Instance, its worker, and one create Command.
    pub async fn create_instance(
        &self,
        request: CreateInstanceRequest,
    ) -> Result<CreateInstanceResponse, NodeError> {
        let payload_digest = digest_json(&request)?;
        validate_kind_driver(request.kind, request.driver)?;
        validate_text(&request.prompt, "prompt")?;

        let host_id = request
            .host_id
            .clone()
            .unwrap_or_else(|| self.inner.host.meta.id.clone());
        let workspace_id = request
            .workspace_id
            .clone()
            .unwrap_or_else(|| self.inner.workspace.meta.id.clone());
        if host_id != self.inner.host.meta.id {
            return Err(NodeError::InvalidRequest(
                "create hostId is not the local development Host".to_owned(),
            ));
        }
        if workspace_id != self.inner.workspace.meta.id {
            return Err(NodeError::InvalidRequest(
                "create workspaceId is not the local development Workspace".to_owned(),
            ));
        }

        let instance_id = request.instance_id.clone().unwrap_or_default();
        let instance = fixture_instance(
            instance_id.clone(),
            host_id.clone(),
            workspace_id,
            request.driver,
        )?;
        let driver = self.inner.drivers.build(
            request.driver,
            DriverLaunch {
                instance: instance.clone(),
                request: request.clone(),
                workspace_root: self.inner.workspace.root_path.clone().into(),
            },
        )?;
        self.inner.store.insert_instance(instance)?;
        self.inner
            .interactions
            .register_driver(instance_id.clone(), Arc::clone(&driver))
            .await;
        let observations = match driver.start().await {
            Ok(observations) => observations,
            Err(error) => {
                record_task_exit(
                    self.inner.store.as_ref(),
                    &instance_id,
                    "native-driver-start-failed",
                );
                return Err(NodeError::Driver(error.to_string()));
            }
        };
        if let Some(observations) = observations {
            self.spawn_observation_pump(instance_id.clone(), observations);
        }
        self.spawn_instance_worker(instance_id.clone(), driver)
            .await;

        let command_id = CommandId::new();
        let command = new_command(
            command_id.clone(),
            CommandOperation::InstanceCreate,
            &instance_id,
            &host_id,
            &self.inner.projection_epoch,
            None,
            payload_digest,
        )?;
        self.inner
            .store
            .insert_command(&instance_id, command.clone())?;
        append_instance_lifecycle(
            self.inner.store.as_ref(),
            &instance_id,
            None,
            "ready",
            "driver-started",
        )?;

        let command = if request.prompt.is_empty() {
            settle_without_driver(self.inner.store.as_ref(), &instance_id, command)?
        } else {
            self.enqueue_existing(
                &instance_id,
                command_id,
                DriverRequest::Send {
                    prompt: request.prompt,
                },
                false,
            )
            .await?
        };
        let instance = self.inner.store.get_instance(&instance_id)?;
        Ok(CreateInstanceResponse { command, instance })
    }

    /// Submit send/cancel/respond/close through an Instance's bounded queue.
    pub async fn submit_command(
        &self,
        instance_id: &InstanceId,
        request: InstanceCommandRequest,
    ) -> Result<CommandResult, NodeError> {
        let instance = self.inner.store.get_instance(instance_id)?;
        let (operation, driver_request, close_after) = command_parts(&request)?;
        let command_id = request.command_id.clone().unwrap_or_default();
        let command = new_command(
            command_id.clone(),
            operation,
            instance_id,
            &instance.host_id,
            &self.inner.projection_epoch,
            request.run_id.clone(),
            digest_json(&request)?,
        )?;
        let inserted = self
            .inner
            .store
            .insert_command(instance_id, command.clone())?;
        if !inserted {
            return Ok(CommandResult {
                command: self.inner.store.get_command(&command_id)?,
                related_command_ids: Vec::new(),
            });
        }

        let result = self
            .enqueue_existing(instance_id, command_id, driver_request, close_after)
            .await;
        match result {
            Ok(command) => Ok(CommandResult {
                command,
                related_command_ids: Vec::new(),
            }),
            Err(NodeError::QueueFull) => {
                let command = reject_before_dispatch(
                    self.inner.store.as_ref(),
                    instance_id,
                    command,
                    "instance-queue-full",
                )?;
                Ok(CommandResult {
                    command,
                    related_command_ids: Vec::new(),
                })
            }
            Err(error) => Err(error),
        }
    }

    /// Read one Command for timeout reconciliation.
    pub fn get_command(&self, command_id: &CommandId) -> Result<Command, NodeError> {
        self.inner.store.get_command(command_id)
    }

    /// Read one exclusive sequence page from a journal.
    pub fn read_journal(
        &self,
        journal_id: &Id,
        after_seq: Option<U64>,
        limit: usize,
    ) -> Result<remuda_protocol::EventsReadResult, NodeError> {
        self.inner.store.read_events(journal_id, after_seq, limit)
    }

    /// Resolve a journal to its Instance.
    pub fn instance_for_journal(&self, journal_id: &Id) -> Result<InstanceId, NodeError> {
        self.inner.store.instance_for_journal(journal_id)
    }

    /// Subscribe to one Instance before taking its snapshot.
    pub fn subscribe(
        &self,
        instance_id: &InstanceId,
    ) -> Result<tokio::sync::broadcast::Receiver<JournalEvent>, NodeError> {
        self.inner.store.subscribe(instance_id)
    }

    /// Build an Instance snapshot at the store's current durable sequence.
    pub fn snapshot(
        &self,
        instance_id: &InstanceId,
    ) -> Result<remuda_protocol::InstanceSnapshot, NodeError> {
        self.inner
            .store
            .snapshot(instance_id, self.inner.projection_epoch.clone())
    }

    /// Projection epoch shared by snapshots from this local Node process.
    pub fn projection_epoch(&self) -> Id {
        self.inner.projection_epoch.clone()
    }

    /// Hub→Node `interaction.list` / `interaction.answer`.
    pub async fn dispatch_interaction(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, NodeError> {
        self.inner.interactions.dispatch_rpc(method, params).await
    }

    fn spawn_observation_pump(
        &self,
        instance_id: InstanceId,
        mut observations: mpsc::Receiver<remuda_protocol::Observation>,
    ) {
        let store = self.inner.store.clone();
        let interactions = Arc::clone(&self.inner.interactions);
        tokio::spawn(async move {
            while let Some(observation) = observations.recv().await {
                match store.append_driver_observation(&instance_id, observation) {
                    Ok(committed) => {
                        if let Err(error) = interactions.ingest(&committed).await {
                            tracing::debug!(%error, "interaction ingest failed");
                        }
                    }
                    Err(error) => {
                        tracing::error!(%error, instance_id = %instance_id.as_id(), "native observation commit failed");
                        record_task_exit(
                            store.as_ref(),
                            &instance_id,
                            "native-observation-commit-failed",
                        );
                        break;
                    }
                }
            }
        });
    }

    async fn spawn_instance_worker(&self, instance_id: InstanceId, driver: Arc<dyn Driver>) {
        let (sender, receiver) = mpsc::channel(self.inner.queue_capacity);
        self.inner
            .senders
            .write()
            .await
            .insert(instance_id.clone(), sender);
        let store = self.inner.store.clone();
        let interactions = Arc::clone(&self.inner.interactions);
        let worker_instance = instance_id.clone();
        let worker = tokio::spawn(async move {
            instance_worker(store, worker_instance, driver, receiver, interactions).await
        });
        let store = self.inner.store.clone();
        tokio::spawn(async move {
            match worker.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::error!(%error, instance_id = %instance_id.as_id(), "instance task exited");
                    record_task_exit(store.as_ref(), &instance_id, "driver-task-exited");
                }
                Err(error) => {
                    tracing::error!(%error, instance_id = %instance_id.as_id(), "instance task panicked");
                    record_task_exit(store.as_ref(), &instance_id, "driver-task-panicked");
                }
            }
        });
    }

    async fn enqueue_existing(
        &self,
        instance_id: &InstanceId,
        command_id: CommandId,
        request: DriverRequest,
        close_after: bool,
    ) -> Result<Command, NodeError> {
        let sender = self
            .inner
            .senders
            .read()
            .await
            .get(instance_id)
            .cloned()
            .ok_or(NodeError::DriverUnavailable)?;
        let (accepted_tx, accepted_rx) = oneshot::channel();
        sender
            .try_send(QueuedCommand {
                command_id,
                request,
                accepted_tx,
                close_after,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => NodeError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => NodeError::DriverUnavailable,
            })?;
        if close_after {
            self.inner.senders.write().await.remove(instance_id);
        }
        tokio::time::timeout(COMMAND_ACK_TIMEOUT, accepted_rx)
            .await
            .map_err(|_| NodeError::DriverUnavailable)?
            .map_err(|_| NodeError::DriverUnavailable)?
    }
}

async fn instance_worker(
    store: Arc<dyn LocalStore>,
    instance_id: InstanceId,
    driver: Arc<dyn Driver>,
    mut receiver: mpsc::Receiver<QueuedCommand>,
    interactions: Arc<InteractionRuntime>,
) -> Result<(), NodeError> {
    while let Some(queued) = receiver.recv().await {
        let mut command = store.get_command(&queued.command_id)?;
        accept_command(&mut command)?;
        store.save_command(command.clone())?;
        append_command_lifecycle(store.as_ref(), &instance_id, &command, "accepted")?;
        let _ = queued.accepted_tx.send(Ok(command.clone()));

        if let DriverRequest::Send { prompt } = &queued.request {
            store.set_instance_state(
                &instance_id,
                None,
                Some(Knowledge::Known {
                    value: Activity::Working,
                }),
            )?;
            store.append_observation(
                &instance_id,
                None,
                Completeness::Structured,
                crate::driver::message_payload(
                    MessageRole::User,
                    MessagePhase::Input,
                    prompt.clone(),
                )?,
            )?;
        }

        let result = driver.execute(queued.request.clone()).await;
        match result {
            Ok(emissions) => {
                for emission in emissions {
                    let observation = store.append_observation(
                        &instance_id,
                        None,
                        Completeness::Structured,
                        emission.into_payload()?,
                    )?;
                    interactions.ingest(&observation).await?;
                }
                finish_instance_operation(store.as_ref(), &instance_id, &queued.request)?;
                settle_command(
                    &mut command,
                    SettlementOutcome::Completed,
                    None,
                    remuda_protocol::ExecutionState::PossiblyDispatched,
                )?;
            }
            Err(error) => {
                let diagnostic = DriverEmission::NativeLifecycle {
                    name: "fake-driver-error".to_owned(),
                    status: error.to_string(),
                    severity: remuda_protocol::Severity::Error,
                };
                store.append_observation(
                    &instance_id,
                    None,
                    Completeness::Structured,
                    diagnostic.into_payload()?,
                )?;
                settle_command(
                    &mut command,
                    SettlementOutcome::Rejected,
                    Some(error.to_string()),
                    remuda_protocol::ExecutionState::PossiblyDispatched,
                )?;
            }
        }
        store.save_command(command.clone())?;
        append_command_lifecycle(store.as_ref(), &instance_id, &command, "settled")?;
        if queued.close_after {
            return Ok(());
        }
    }
    Err(NodeError::DriverUnavailable)
}

fn finish_instance_operation(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    request: &DriverRequest,
) -> Result<(), NodeError> {
    match request {
        DriverRequest::Close => {
            store.set_instance_state(
                instance_id,
                Some(InstanceLifecycle::Exited),
                Some(Knowledge::Known {
                    value: Activity::Idle,
                }),
            )?;
            append_instance_lifecycle(
                store,
                instance_id,
                Some("ready"),
                "exited",
                "explicit-close",
            )?;
        }
        DriverRequest::Send { .. } | DriverRequest::Cancel => {
            store.set_instance_state(
                instance_id,
                None,
                Some(Knowledge::Known {
                    value: Activity::Idle,
                }),
            )?;
        }
        DriverRequest::RespondInteraction { .. } => {}
    }
    Ok(())
}

fn record_task_exit(store: &dyn LocalStore, instance_id: &InstanceId, reason: &str) {
    if let Err(error) = store.mark_unsettled_unknown(instance_id) {
        tracing::error!(%error, "failed to mark commands unknown after task exit");
    }
    if let Err(error) = store.set_instance_state(
        instance_id,
        Some(InstanceLifecycle::Failed),
        Some(unknown("driver-task-ended")),
    ) {
        tracing::error!(%error, "failed to mark instance failed after task exit");
        return;
    }
    if let Err(error) =
        append_instance_lifecycle(store, instance_id, Some("ready"), "failed", reason)
    {
        tracing::error!(%error, "failed to append task-exit lifecycle event");
    }
}

fn command_parts(
    request: &InstanceCommandRequest,
) -> Result<(CommandOperation, DriverRequest, bool), NodeError> {
    match request.operation {
        CommandAction::Send => {
            let prompt = request
                .prompt
                .clone()
                .ok_or_else(|| NodeError::InvalidRequest("send requires prompt".to_owned()))?;
            validate_text(&prompt, "prompt")?;
            Ok((
                CommandOperation::InstanceSend,
                DriverRequest::Send { prompt },
                false,
            ))
        }
        CommandAction::Cancel => Ok((
            CommandOperation::InstanceCancel,
            DriverRequest::Cancel,
            false,
        )),
        CommandAction::RespondInteraction => {
            let interaction_id = request.interaction_id.as_ref().ok_or_else(|| {
                NodeError::InvalidRequest("respond_interaction requires interactionId".to_owned())
            })?;
            let answer = request.answer.clone().ok_or_else(|| {
                NodeError::InvalidRequest("respond_interaction requires answer".to_owned())
            })?;
            Ok((
                CommandOperation::InteractionRespond,
                DriverRequest::RespondInteraction {
                    interaction_id: interaction_id.as_id().to_string(),
                    answer,
                },
                false,
            ))
        }
        CommandAction::Close => Ok((CommandOperation::InstanceClose, DriverRequest::Close, true)),
    }
}

fn validate_kind_driver(
    kind: AgentKind,
    driver: remuda_protocol::DriverKind,
) -> Result<(), NodeError> {
    let valid = matches!(
        (kind, driver),
        (AgentKind::Claude, remuda_protocol::DriverKind::ClaudePrint)
            | (AgentKind::Claude, remuda_protocol::DriverKind::ClaudePty)
            | (AgentKind::Claude, remuda_protocol::DriverKind::ClaudeBg)
            | (
                AgentKind::Codex,
                remuda_protocol::DriverKind::CodexAppserver
            )
            | (AgentKind::Grok, remuda_protocol::DriverKind::GrokAcp)
            | (AgentKind::Agy, remuda_protocol::DriverKind::AgyPrint)
            | (AgentKind::Generic, remuda_protocol::DriverKind::GenericPty)
    );
    if valid {
        Ok(())
    } else {
        Err(NodeError::InvalidRequest(
            "kind and driver do not describe the same native product".to_owned(),
        ))
    }
}

fn validate_text(text: &str, label: &str) -> Result<(), NodeError> {
    if text.len() > 64 * 1024 {
        return Err(NodeError::InvalidRequest(format!(
            "{label} exceeds the 65536-byte local limit"
        )));
    }
    Ok(())
}

fn new_command(
    command_id: CommandId,
    operation: CommandOperation,
    instance_id: &InstanceId,
    host_id: &HostId,
    node_epoch: &Id,
    run_id: Option<remuda_protocol::RunId>,
    payload_digest: WireDigest,
) -> Result<Command, NodeError> {
    let now = timestamp_now()?;
    Ok(Command {
        meta: EntityMeta {
            id: command_id.clone(),
            revision: U64(1),
            created_at: now.clone(),
            updated_at: now.clone(),
        },
        command_id,
        actor: ActorRef {
            principal_id: Id::new("prn")?,
            actor_type: ActorType::Human,
            device_id: None,
            instance_id: Some(instance_id.clone()),
        },
        origin: CommandOrigin::Ui,
        operation,
        target: CommandTarget {
            host_id: host_id.clone(),
            instance_id: Some(instance_id.clone()),
            run_id,
        },
        payload_ref: Id::new("obj")?,
        payload_digest,
        expected: ExpectedState {
            instance_revision: None,
            process_generation: Some(U64(1)),
            run_generation: None,
            owner_fence: Some(U64(1)),
            interaction_version: None,
        },
        state: CommandState::Queued,
        authority: CommandAuthority::NodeLedger,
        forward_intent: Knowledge::NotApplicable,
        dispatch: DispatchState::NotDispatched,
        resolution: ResolutionState::Clear,
        acceptance: unknown("not-accepted"),
        settlement: unknown("not-settled"),
        queued_at: now,
        accepted_at: unknown("not-accepted"),
        settled_at: unknown("not-settled"),
        expires_at: None,
        node_receipt: Knowledge::Known {
            value: NodeReceipt {
                node_epoch: node_epoch.clone(),
                ledger_revision: U64(1),
            },
        },
    })
}

fn accept_command(command: &mut Command) -> Result<(), NodeError> {
    let now = timestamp_now()?;
    command.state = CommandState::Accepted;
    command.dispatch = DispatchState::NativeAcknowledged;
    command.acceptance = Knowledge::Known {
        value: Acceptance {
            scope: acceptance_scope(command.operation),
            event_ids: Vec::new(),
        },
    };
    command.accepted_at = Knowledge::Known { value: now.clone() };
    command.meta.updated_at = now;
    command.meta.revision.0 = command.meta.revision.0.saturating_add(1);
    Ok(())
}

fn acceptance_scope(operation: CommandOperation) -> AcceptanceScope {
    match operation {
        CommandOperation::InstanceCreate => AcceptanceScope::RuntimeResource,
        CommandOperation::InstanceSend => AcceptanceScope::NativeInput,
        CommandOperation::InstanceCancel
        | CommandOperation::InstanceClose
        | CommandOperation::InteractionRespond => AcceptanceScope::NativeControl,
        _ => AcceptanceScope::RuntimeResource,
    }
}

fn settle_command(
    command: &mut Command,
    outcome: SettlementOutcome,
    error_message: Option<String>,
    execution: remuda_protocol::ExecutionState,
) -> Result<(), NodeError> {
    let now = timestamp_now()?;
    command.state = CommandState::Settled;
    command.settlement = Knowledge::Known {
        value: Settlement {
            outcome,
            result_ref: None,
            error: error_message.map(|message| runtime_error(message, execution)),
        },
    };
    command.settled_at = Knowledge::Known { value: now.clone() };
    command.meta.updated_at = now;
    command.meta.revision.0 = command.meta.revision.0.saturating_add(1);
    Ok(())
}

fn runtime_error(
    message: String,
    execution: remuda_protocol::ExecutionState,
) -> remuda_protocol::RuntimeError {
    remuda_protocol::RuntimeError {
        code: remuda_protocol::ErrorCode::NativeProtocolError,
        rpc_code: remuda_protocol::ErrorCode::NativeProtocolError.rpc_code(),
        message,
        retry: remuda_protocol::RetryAction::Never,
        execution,
        details: remuda_protocol::ErrorDetails {
            command_id: None,
            instance_id: None,
            interaction_id: None,
            expected_generation: None,
            actual_generation: None,
            evidence_event_ids: None,
            native_error_ref: None,
            retry_after_ms: None,
        },
    }
}

fn settle_without_driver(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    mut command: Command,
) -> Result<Command, NodeError> {
    accept_command(&mut command)?;
    store.save_command(command.clone())?;
    append_command_lifecycle(store, instance_id, &command, "accepted")?;
    settle_command(
        &mut command,
        SettlementOutcome::Completed,
        None,
        remuda_protocol::ExecutionState::NotDispatched,
    )?;
    store.save_command(command.clone())?;
    append_command_lifecycle(store, instance_id, &command, "settled")?;
    Ok(command)
}

fn reject_before_dispatch(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    mut command: Command,
    reason: &str,
) -> Result<Command, NodeError> {
    settle_command(
        &mut command,
        SettlementOutcome::Rejected,
        Some(reason.to_owned()),
        remuda_protocol::ExecutionState::NotDispatched,
    )?;
    store.save_command(command.clone())?;
    append_command_lifecycle(store, instance_id, &command, "settled")?;
    Ok(command)
}

fn append_command_lifecycle(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    command: &Command,
    state: &str,
) -> Result<(), NodeError> {
    let payload = ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Entity(Box::new(
        EntityLifecycle {
            entity_id: command.command_id.as_id().clone(),
            revision: command.meta.revision,
            previous_state: None,
            state: state.to_owned(),
            reason_code: "local-driver-ledger".to_owned(),
            evidence_event_ids: Vec::new(),
            entity_value: LifecycleEntity::Command(Box::new(command.clone())),
        },
    ))));
    store.append_observation(instance_id, None, Completeness::Structured, payload)?;
    Ok(())
}

fn append_instance_lifecycle(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    previous_state: Option<&str>,
    state: &str,
    reason: &str,
) -> Result<(), NodeError> {
    let instance = store.get_instance(instance_id)?;
    let payload = ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Entity(Box::new(
        EntityLifecycle {
            entity_id: instance_id.as_id().clone(),
            revision: instance.meta.revision,
            previous_state: previous_state.map(str::to_owned),
            state: state.to_owned(),
            reason_code: reason.to_owned(),
            evidence_event_ids: Vec::new(),
            entity_value: LifecycleEntity::Instance(Box::new(instance)),
        },
    ))));
    store.append_observation(instance_id, None, Completeness::Structured, payload)?;
    Ok(())
}

fn digest_json<T: Serialize>(value: &T) -> Result<WireDigest, NodeError> {
    let bytes = serde_json::to_vec(value)?;
    let digest = Sha256::digest(bytes);
    Ok(format!("sha256:{digest:x}").try_into()?)
}

fn fixture_host(host_id: HostId) -> Result<Host, NodeError> {
    let now = timestamp_now()?;
    Ok(Host {
        meta: EntityMeta {
            id: host_id,
            revision: U64(1),
            created_at: now.clone(),
            updated_at: now.clone(),
        },
        label: "local-dev".to_owned(),
        owner_principal_id: Id::new("prn")?,
        state: HostState::Online,
        node_version: Knowledge::Known {
            value: env!("CARGO_PKG_VERSION").to_owned(),
        },
        platform: Knowledge::Known {
            value: Platform {
                os: std::env::consts::OS.to_owned(),
                arch: std::env::consts::ARCH.to_owned(),
                path_style: PathStyle::Posix,
            },
        },
        identity_key_id: Id::new("obj")?,
        node_epoch: Knowledge::Known {
            value: Id::new("epoch")?,
        },
        transport: HostTransport {
            mode: HostTransportMode::OutboundWss,
            endpoint_ref: Id::new("obj")?,
        },
        last_seen_at: Knowledge::Known { value: now },
        lease_expires_at: unknown("local-dev-no-lease"),
        // TODO(inventory): Hub enroll should call crate::inventory::collect; DevNode stays fixture-only.
        driver_inventory: Vec::new(),
        journal_id: Id::new("obj")?,
        durable_seq: U64(0),
    })
}

fn fixture_workspace(
    workspace_id: WorkspaceId,
    host_id: HostId,
    root: &Path,
) -> Result<Workspace, NodeError> {
    let now = timestamp_now()?;
    let root = root.to_string_lossy().into_owned();
    Ok(Workspace {
        meta: EntityMeta {
            id: workspace_id,
            revision: U64(1),
            created_at: now.clone(),
            updated_at: now,
        },
        host_id,
        label: root
            .rsplit(std::path::MAIN_SEPARATOR)
            .find(|segment| !segment.is_empty())
            .unwrap_or("workspace")
            .to_owned(),
        root_path: root.clone(),
        canonical_root: Knowledge::Known { value: root },
        state: WorkspaceState::Ready,
        repository: Knowledge::NotApplicable,
        worktree: None,
        write_policy: WritePolicy::Exclusive,
        writer_leases: Vec::new(),
        access_policy_revision: U64(1),
        journal_id: Id::new("obj")?,
        durable_seq: U64(0),
    })
}

pub(crate) fn fixture_instance(
    instance_id: InstanceId,
    host_id: HostId,
    workspace_id: WorkspaceId,
    driver: remuda_protocol::DriverKind,
) -> Result<Instance, NodeError> {
    let now = timestamp_now()?;
    let native_session_id = instance_id.as_id().to_string();
    Ok(Instance {
        meta: EntityMeta {
            id: instance_id,
            revision: U64(1),
            created_at: now.clone(),
            updated_at: now,
        },
        host_id: host_id.clone(),
        workspace_id,
        kind: AgentKind::Claude,
        driver,
        lifecycle: InstanceLifecycle::Ready,
        activity: Knowledge::Known {
            value: Activity::Idle,
        },
        activity_evidence_event_ids: Vec::new(),
        connectivity: Connectivity::Connected,
        ownership: Ownership::Managed,
        native_ref: NativeRef {
            host_id,
            native_store_id: Id::new("obj")?,
            kind: AgentKind::Claude,
            session_id: Knowledge::Known {
                value: native_session_id.clone(),
            },
            transcript: unknown("fake-driver-no-transcript"),
            codex: None,
            acp: None,
            claude: Some(ClaudeRef {
                session_id: native_session_id,
            }),
            claude_bg: None,
            agy: None,
            herdr: None,
        },
        process_ref: ProcessRef {
            process_generation: U64(1),
            process_identity: unknown("fake-driver-no-process"),
            connection_epoch: Id::new("epoch")?,
        },
        spec_revision: U64(1),
        launch_id: Knowledge::Known {
            value: Id::new("launch")?,
        },
        capabilities: fixture_capabilities(driver)?,
        owner_fence: U64(1),
        active_run_ids: Vec::new(),
        parent: None,
        journal_id: Id::new("obj")?,
        durable_seq: U64(0),
        exit: Knowledge::NotApplicable,
    })
}

fn fixture_capabilities(
    driver: remuda_protocol::DriverKind,
) -> Result<CapabilitySnapshot, NodeError> {
    let unknown_capability = Capability {
        state: CapabilityState::Unknown,
        scope: Vec::new(),
        reason_code: "not-verified".to_owned(),
        prerequisites: Vec::new(),
        evidence: Vec::new(),
    };
    let unsupported = Capability {
        state: CapabilityState::Unsupported,
        scope: Vec::new(),
        reason_code: "fake-driver".to_owned(),
        prerequisites: Vec::new(),
        evidence: Vec::new(),
    };
    let supported = Capability {
        state: CapabilityState::Supported,
        scope: vec!["local-fixture".to_owned()],
        reason_code: "fake-driver".to_owned(),
        prerequisites: Vec::new(),
        evidence: Vec::new(),
    };
    Ok(CapabilitySnapshot {
        adapter_transport: remuda_protocol::AdapterTransport::NativeRustWire,
        id: Id::new("obj")?,
        driver_kind: driver,
        adapter_version: env!("CARGO_PKG_VERSION").to_owned(),
        binary_version: "fake".to_owned(),
        binary_digest: format!("sha256:{:064x}", 0).try_into()?,
        native_protocol_version: Knowledge::NotApplicable,
        settings_revision: U64(1),
        provider_profile_revision: U64(1),
        capabilities: CapabilitySet {
            resume: unsupported.clone(),
            steer: unsupported.clone(),
            model_switch: unsupported.clone(),
            fork: unsupported.clone(),
            structured_workflow: unsupported.clone(),
            artifact: unsupported.clone(),
            tty_attach: unsupported.clone(),
            hooks: unsupported,
            interactive_approval: unknown_capability.clone(),
            question: unknown_capability.clone(),
            plan_review: unknown_capability.clone(),
            elicitation: unknown_capability.clone(),
            live_attach: unknown_capability,
            completion_native_turn: supported,
            completion_task: Capability {
                state: CapabilityState::Unsupported,
                scope: Vec::new(),
                reason_code: "fake-native-turn-only".to_owned(),
                prerequisites: Vec::new(),
                evidence: Vec::new(),
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FakeDriver, MemoryStore};

    #[tokio::test]
    async fn driver_panic_is_contained_and_marks_dispatch_unknown() {
        let config = crate::DevServerConfig::loopback(0);
        let store = Arc::new(MemoryStore::new(8));
        let drivers = DriverRegistry::default();
        drivers
            .register(Arc::new(
                FakeDriver::default().with_panic_prompt("panic-fixture"),
            ))
            .expect("register fake driver");
        let node = DevNode::with_parts(&config, store, drivers).expect("compose node");
        let request: CreateInstanceRequest = serde_json::from_value(serde_json::json!({
            "kind": "claude",
            "driver": "claude-print",
            "prompt": "panic-fixture"
        }))
        .expect("create request");
        let created = node
            .create_instance(request)
            .await
            .expect("accepted create");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let instance = node
                    .get_instance(&created.instance.meta.id)
                    .expect("instance remains visible");
                if instance.lifecycle == InstanceLifecycle::Failed {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("panic monitor completed");
        let command = node
            .get_command(&created.command.command_id)
            .expect("command remains visible");
        assert_eq!(command.resolution, ResolutionState::Unknown);
        let instance = node
            .get_instance(&created.instance.meta.id)
            .expect("failed instance");
        let events = node
            .read_journal(&instance.journal_id, None, 128)
            .expect("task-exit journal");
        assert!(events.events.iter().any(|event| {
            let JournalEvent::Instance(observation) = event else {
                return false;
            };
            let ObservationPayload::Lifecycle(payload) = &observation.body else {
                return false;
            };
            matches!(
                payload.as_ref(),
                LifecyclePayload::Entity(entity) if entity.state == "failed"
            )
        }));
    }
}
