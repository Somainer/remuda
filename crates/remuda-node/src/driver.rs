//! Driver trait object, registry, and the deterministic M0 fake driver.

use crate::{NodeError, store::unknown};
use remuda_protocol::{
    ContentBlock, ContentStatus, DriverKind, Id, Instance, Knowledge, LifecyclePayload,
    LifecycleTopic, MessagePayload, MessagePhase, MessageRole, MutationOperation, NativeLifecycle,
    NativeRequestKey, NodeMutation, Observation, ObservationPayload, ObservationSource, Severity,
    SourceChannel, SourceCursor, SourceDelivery, TextBlock, U64,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, RwLock},
};
use tokio::sync::mpsc;

/// Future returned by a driver operation without requiring an async-trait macro.
pub type DriverFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<DriverEmission>, DriverError>> + Send + 'a>>;

/// Future returned while establishing an instance-local native driver.
pub type DriverStartFuture<'a> = Pin<
    Box<dyn Future<Output = Result<Option<mpsc::Receiver<Observation>>, DriverError>> + Send + 'a>,
>;

/// Stable inputs supplied to a per-instance [`DriverFactory`].
#[derive(Debug, Clone)]
pub struct DriverLaunch {
    /// Canonical Instance allocated by the Node.
    pub instance: Instance,
    /// User/Hub request retained for model, profile, and permission selection.
    pub request: crate::CreateInstanceRequest,
    /// Absolute workspace root selected by the local registry.
    pub workspace_root: PathBuf,
}

/// One operation delivered to an instance-local driver task.
#[derive(Debug, Clone)]
pub enum DriverRequest {
    /// Submit text as one new fake turn.
    Send {
        /// Plain development prompt.
        prompt: String,
    },
    /// Cancel the current fake turn.
    Cancel,
    /// Record a response to an Interaction without interpreting its schema.
    RespondInteraction {
        /// Interaction identity for diagnostics.
        interaction_id: String,
        /// Opaque answer used by the fake driver only.
        answer: serde_json::Value,
    },
    /// Stop the instance worker.
    Close,
    /// Write logical keys to a tty-attach driver (`tty.write`).
    SendKeys {
        /// Normalized key names (`enter`, `esc`, `ctrl+c`, …).
        keys: Vec<String>,
    },
}

/// Structured output emitted by a local driver operation.
#[derive(Debug, Clone)]
pub enum DriverEmission {
    /// One transcript message.
    Message {
        /// Message role.
        role: MessageRole,
        /// Message phase.
        phase: MessagePhase,
        /// Complete text body.
        text: String,
    },
    /// Native lifecycle diagnostic that does not prove task success.
    NativeLifecycle {
        /// Stable diagnostic name.
        name: String,
        /// Native status text.
        status: String,
        /// Diagnostic severity.
        severity: Severity,
    },
    /// Fake/native `can_use_tool` (or equivalent) waiting on the host.
    InteractionRequested {
        /// Full Interaction entity for journal + broker ingest.
        interaction: Box<remuda_protocol::Interaction>,
    },
}

impl DriverEmission {
    pub(crate) fn into_payload(self) -> Result<ObservationPayload, NodeError> {
        match self {
            Self::Message { role, phase, text } => message_payload(role, phase, text),
            Self::NativeLifecycle {
                name,
                status,
                severity,
            } => Ok(ObservationPayload::Lifecycle(Box::new(
                LifecyclePayload::Native(Box::new(NativeLifecycle {
                    topic: LifecycleTopic::Diagnostic,
                    native_name: name,
                    native_id: Knowledge::NotApplicable,
                    status: Knowledge::Known { value: status },
                    related_ids: BTreeMap::new(),
                    data_ref: None,
                    severity,
                    affects_completion: false,
                })),
            ))),
            Self::InteractionRequested { interaction } => {
                Ok(ObservationPayload::InteractionRequested(Box::new(
                    remuda_protocol::InteractionRequestedPayload {
                        interaction: *interaction,
                    },
                )))
            }
        }
    }
}

/// Driver-side failure contained to one instance worker.
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    /// The request is not supported by this adapter.
    #[error("unsupported driver request: {0}")]
    Unsupported(String),
    /// The adapter failed without proving whether external work completed.
    #[error("driver operation failed: {0}")]
    Failed(String),
}

/// Minimal object-safe driver contract used until `remuda-driver` is integrated.
pub trait Driver: Send + Sync {
    /// Bind Node resource ownership before starting native work.
    fn track_pty_resources(
        &self,
        _id: remuda_protocol::InstanceId,
        _store: Arc<dyn remuda_driver::PtyResourceStore>,
    ) {
    }

    /// Driver kind registered by this implementation.
    fn kind(&self) -> DriverKind;
    /// Establish the native process and return its unsolicited observation stream.
    fn start(&self) -> DriverStartFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// Wait until [`Driver::execute`] can deliver [`DriverRequest::Send`].
    fn wait_control(&self) -> DriverFuture<'_> {
        Box::pin(async { Ok(Vec::new()) })
    }
    /// Execute one request inside the caller-owned bounded instance task.
    fn execute(&self, request: DriverRequest) -> DriverFuture<'_>;
    /// Durable launch recipe after [`Driver::start`], if the adapter produced one.
    fn launch_recipe(&self) -> Option<remuda_driver::LaunchRecipe> {
        None
    }
    /// Native start reported a failed agent (process gone / shell prompt / not ready).
    fn startup_error(&self) -> Option<String> {
        None
    }
}

/// Creates one stateful driver object for each Instance.
pub trait DriverFactory: Send + Sync {
    /// Driver kind constructed by this factory.
    fn kind(&self) -> DriverKind;
    /// Build an unstarted instance-local driver.
    fn build(&self, launch: DriverLaunch) -> Result<Arc<dyn Driver>, DriverError>;
}

/// Deterministic no-network driver for local API and Web integration.
#[derive(Debug, Clone)]
pub struct FakeDriver {
    kind: DriverKind,
    panic_prompts: BTreeSet<String>,
    instance: Option<Instance>,
}

impl FakeDriver {
    /// Build a fake driver for a protocol DriverKind.
    pub fn new(kind: DriverKind) -> Self {
        Self {
            kind,
            panic_prompts: BTreeSet::new(),
            instance: None,
        }
    }

    /// Add a prompt that intentionally panics to test instance isolation.
    pub fn with_panic_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.panic_prompts.insert(prompt.into());
        self
    }
}

impl Default for FakeDriver {
    fn default() -> Self {
        Self::new(DriverKind::ClaudePrint)
    }
}

impl Driver for FakeDriver {
    fn kind(&self) -> DriverKind {
        self.kind
    }

    fn execute(&self, request: DriverRequest) -> DriverFuture<'_> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            match request {
                DriverRequest::Send { prompt } => {
                    assert!(
                        !self.panic_prompts.contains(&prompt),
                        "intentional fake-driver panic"
                    );
                    let mut emissions = Vec::new();
                    if prompt.contains("can_use_tool")
                        && let Some(instance) = &self.instance
                    {
                        let interaction = crate::interactions::fake_can_use_tool(instance)
                            .map_err(|error| DriverError::Failed(error.to_string()))?;
                        emissions.push(DriverEmission::InteractionRequested {
                            interaction: Box::new(interaction),
                        });
                    }
                    emissions.push(DriverEmission::Message {
                        role: MessageRole::Assistant,
                        phase: MessagePhase::Final,
                        text: format!("fake: {prompt}"),
                    });
                    Ok(emissions)
                }
                DriverRequest::Cancel => Ok(vec![DriverEmission::NativeLifecycle {
                    name: "fake-driver".to_owned(),
                    status: "cancelled".to_owned(),
                    severity: Severity::Info,
                }]),
                DriverRequest::RespondInteraction {
                    interaction_id,
                    answer,
                } => Ok(vec![DriverEmission::NativeLifecycle {
                    name: "fake-interaction".to_owned(),
                    status: format!(
                        "recorded response for {interaction_id} ({} bytes)",
                        answer.to_string().len()
                    ),
                    severity: Severity::Info,
                }]),
                DriverRequest::Close => Ok(vec![DriverEmission::NativeLifecycle {
                    name: "fake-driver".to_owned(),
                    status: "closed".to_owned(),
                    severity: Severity::Info,
                }]),
                DriverRequest::SendKeys { keys } => Ok(vec![DriverEmission::NativeLifecycle {
                    name: "fake-driver-keys".to_owned(),
                    status: keys.join(" "),
                    severity: Severity::Info,
                }]),
            }
        })
    }
}

#[derive(Clone)]
enum Registration {
    Shared(Arc<dyn Driver>),
    Factory(Arc<dyn DriverFactory>),
}

/// Thread-safe registry of local driver implementations.
#[derive(Clone, Default)]
pub struct DriverRegistry {
    drivers: Arc<RwLock<BTreeMap<DriverKind, Registration>>>,
}

impl DriverRegistry {
    /// Register or replace the adapter for its advertised kind.
    pub fn register(&self, driver: Arc<dyn Driver>) -> Result<(), NodeError> {
        self.drivers
            .write()
            .map_err(|_| NodeError::StorePoisoned)?
            .insert(driver.kind(), Registration::Shared(driver));
        Ok(())
    }

    /// Register or replace a per-instance driver factory.
    pub fn register_factory(&self, factory: Arc<dyn DriverFactory>) -> Result<(), NodeError> {
        self.drivers
            .write()
            .map_err(|_| NodeError::StorePoisoned)?
            .insert(factory.kind(), Registration::Factory(factory));
        Ok(())
    }

    /// Construct one driver or fail closed when no adapter is registered.
    pub fn build(
        &self,
        kind: DriverKind,
        launch: DriverLaunch,
    ) -> Result<Arc<dyn Driver>, NodeError> {
        let registration = self
            .drivers
            .read()
            .map_err(|_| NodeError::StorePoisoned)?
            .get(&kind)
            .cloned()
            .ok_or_else(|| {
                NodeError::InvalidRequest(format!("driver {kind:?} is not registered"))
            })?;
        match registration {
            Registration::Shared(driver) => Ok(driver),
            Registration::Factory(factory) => factory
                .build(launch)
                .map_err(|error| NodeError::Driver(error.to_string())),
        }
    }

    /// Registry containing the default Claude print fake driver.
    pub fn with_fake() -> Result<Self, NodeError> {
        let registry = Self::default();
        registry.register_factory(Arc::new(FakeDriverFactory))?;
        Ok(registry)
    }
}

struct FakeDriverFactory;

impl DriverFactory for FakeDriverFactory {
    fn kind(&self) -> DriverKind {
        DriverKind::ClaudePrint
    }

    fn build(&self, launch: DriverLaunch) -> Result<Arc<dyn Driver>, DriverError> {
        Ok(Arc::new(FakeDriver {
            kind: DriverKind::ClaudePrint,
            panic_prompts: BTreeSet::new(),
            instance: Some(launch.instance),
        }))
    }
}

pub(crate) fn message_payload(
    role: MessageRole,
    phase: MessagePhase,
    text: String,
) -> Result<ObservationPayload, NodeError> {
    let node_id = Id::new("obj")?;
    Ok(ObservationPayload::Message(Box::new(MessagePayload {
        mutation: NodeMutation {
            node_id: node_id.clone(),
            revision: U64(1),
            operation: MutationOperation::Open,
            base_revision: None,
        },
        message_id: node_id,
        role,
        phase,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock { text }))],
        target_block: None,
        parent_tool_call_id: None,
        native_origin: unknown("fake-driver"),
        status: ContentStatus::Complete,
    })))
}

pub(crate) fn runtime_source(instance: &Instance, seq: U64) -> ObservationSource {
    ObservationSource {
        driver_kind: instance.driver,
        driver_version: "fake".to_owned(),
        adapter_version: env!("CARGO_PKG_VERSION").to_owned(),
        channel: SourceChannel::Runtime,
        delivery: SourceDelivery::Live,
        native_session_id: instance.native_ref.session_id.clone(),
        native_turn_id: unknown("not-emitted"),
        native_agent_id: Knowledge::NotApplicable,
        native_item_id: unknown("not-emitted"),
        native_event_id: unknown("not-emitted"),
        native_request_id: NativeRequestKey::None,
        source_cursor: SourceCursor::Runtime(Box::new(remuda_protocol::RuntimeCursor {
            ledger_revision: seq,
        })),
    }
}
