//! Driver trait object, registry, and the deterministic M0 fake driver.

use crate::{NodeError, store::unknown};
use remuda_protocol::{
    ContentBlock, ContentStatus, DriverKind, Id, Instance, Knowledge, LifecyclePayload,
    LifecycleTopic, MessagePayload, MessagePhase, MessageRole, MutationOperation, NativeLifecycle,
    NativeRequestKey, NodeMutation, ObservationPayload, ObservationSource, Severity, SourceChannel,
    SourceCursor, SourceDelivery, TextBlock, U64,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::{Arc, RwLock},
};

/// Future returned by a driver operation without requiring an async-trait macro.
pub type DriverFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<DriverEmission>, DriverError>> + Send + 'a>>;

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
    /// Driver kind registered by this implementation.
    fn kind(&self) -> DriverKind;
    /// Execute one request inside the caller-owned bounded instance task.
    fn execute(&self, request: DriverRequest) -> DriverFuture<'_>;
}

/// Deterministic no-network driver for local API and Web integration.
#[derive(Debug, Clone)]
pub struct FakeDriver {
    kind: DriverKind,
    panic_prompts: BTreeSet<String>,
}

impl FakeDriver {
    /// Build a fake driver for a protocol DriverKind.
    pub fn new(kind: DriverKind) -> Self {
        Self {
            kind,
            panic_prompts: BTreeSet::new(),
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
                    Ok(vec![DriverEmission::Message {
                        role: MessageRole::Assistant,
                        phase: MessagePhase::Final,
                        text: format!("fake: {prompt}"),
                    }])
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
            }
        })
    }
}

/// Thread-safe registry of local driver trait objects.
#[derive(Clone, Default)]
pub struct DriverRegistry {
    drivers: Arc<RwLock<BTreeMap<DriverKind, Arc<dyn Driver>>>>,
}

impl DriverRegistry {
    /// Register or replace the adapter for its advertised kind.
    pub fn register(&self, driver: Arc<dyn Driver>) -> Result<(), NodeError> {
        self.drivers
            .write()
            .map_err(|_| NodeError::StorePoisoned)?
            .insert(driver.kind(), driver);
        Ok(())
    }

    /// Resolve one driver or fail closed when no adapter is registered.
    pub fn get(&self, kind: DriverKind) -> Result<Arc<dyn Driver>, NodeError> {
        self.drivers
            .read()
            .map_err(|_| NodeError::StorePoisoned)?
            .get(&kind)
            .cloned()
            .ok_or_else(|| NodeError::InvalidRequest(format!("driver {kind:?} is not registered")))
    }

    /// Registry containing the default Claude print fake driver.
    pub fn with_fake() -> Result<Self, NodeError> {
        let registry = Self::default();
        registry.register(Arc::new(FakeDriver::default()))?;
        Ok(registry)
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
