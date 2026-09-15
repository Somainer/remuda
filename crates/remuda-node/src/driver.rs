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
    time::Duration,
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
    /// Registered Node workspace boundary, before resolving an instance cwd.
    pub registered_workspace_root: PathBuf,
}

/// One operation delivered to an instance-local driver task.
#[derive(Debug, Clone)]
pub enum DriverRequest {
    /// Submit text as one new fake turn.
    Send {
        /// Plain development prompt.
        prompt: String,
        /// Attachments already pulled to this host's disk (D-027). Empty for
        /// a text-only send, which is every send from an older client.
        attachments: Vec<crate::attachments::MaterializedAttachment>,
        /// Who submitted this input, independent of the instance's creator.
        origin: remuda_protocol::InputOrigin,
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
    /// Switch model / effort on a live driver (`instance.configure`).
    Configure {
        /// Requested model id, when present.
        model: Option<String>,
        /// Native effort tier name.
        effort: Option<String>,
        /// Native effort index.
        effort_index: Option<u32>,
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
            Self::Message { role, phase, text } => message_payload(role, phase, text, Vec::new()),
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
    /// Native input was not dispatched because its control is not ready.
    #[error("control unavailable")]
    ControlUnavailable,
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
    /// Herdr pane or local PTY for the Node TTY bridge.
    fn tty_bridge(
        &self,
    ) -> Pin<Box<dyn Future<Output = Option<remuda_driver::TtyBridge>> + Send + '_>> {
        Box::pin(async { None })
    }
    /// Native start reported a failed agent (process gone / shell prompt / not ready).
    fn startup_error(&self) -> Option<String> {
        None
    }
    /// What this *session* can do, as opposed to what its driver kind can
    /// (D-028 §4.3, §6).
    ///
    /// `None` keeps the create-time snapshot, which is the right answer for a
    /// driver whose abilities really are fixed by its kind. A PTY's are not: it
    /// carries whatever agent is in it, so it answers here and the Node stores
    /// what it says. Without this the wire reports the static row and the UI
    /// cannot distinguish `native` from `emulated` from `unknown` — which is
    /// the whole of §6's honesty requirement.
    fn capabilities(
        &self,
    ) -> Pin<Box<dyn Future<Output = Option<remuda_protocol::CapabilitySnapshot>> + Send + '_>>
    {
        Box::pin(async { None })
    }

    /// Current screen as text, for a carrier that keeps one.
    ///
    /// `None` means this driver has no screen to read, which the Node reports
    /// as `supported: false` rather than as an empty terminal.
    fn screen_read(
        &self,
    ) -> Pin<
        Box<dyn Future<Output = Result<Option<remuda_driver::ScreenRead>, DriverError>> + Send + '_>,
    > {
        Box::pin(async { Ok(None) })
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
    /// Time `start()` takes before reporting the run handle. Lets the
    /// fast-accept test prove the RPC ack lands while the driver is still
    /// materializing.
    start_delay: Duration,
    instance: Option<Instance>,
}

impl FakeDriver {
    /// Build a fake driver for a protocol DriverKind.
    pub fn new(kind: DriverKind) -> Self {
        Self {
            kind,
            panic_prompts: BTreeSet::new(),
            start_delay: Duration::ZERO,
            instance: None,
        }
    }

    /// Add a prompt that intentionally panics to test instance isolation.
    pub fn with_panic_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.panic_prompts.insert(prompt.into());
        self
    }

    /// Delay `start()` by this long, simulating a slow binary pin / spawn.
    pub fn with_start_delay(mut self, delay: Duration) -> Self {
        self.start_delay = delay;
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

    fn start(&self) -> DriverStartFuture<'_> {
        let delay = self.start_delay;
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            Ok(None)
        })
    }

    fn execute(&self, request: DriverRequest) -> DriverFuture<'_> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            match request {
                DriverRequest::Send { prompt, .. } => {
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
                DriverRequest::Configure {
                    model,
                    effort,
                    effort_index,
                } => Ok(vec![DriverEmission::NativeLifecycle {
                    name: "instance.configure".to_owned(),
                    status: format!(
                        "applied model={} effort={} index={}",
                        model.as_deref().unwrap_or("-"),
                        effort.as_deref().unwrap_or("-"),
                        effort_index
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "-".into())
                    ),
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

    /// Registry containing the default Claude print fake driver and `shell-pty`.
    pub fn with_fake() -> Result<Self, NodeError> {
        let registry = Self::default();
        registry.register_factory(Arc::new(FakeDriverFactory))?;
        registry.register_factory(Arc::new(ShellPtyFactory))?;
        Ok(registry)
    }
}

struct ShellPtyFactory;

impl DriverFactory for ShellPtyFactory {
    fn kind(&self) -> DriverKind {
        DriverKind::ShellPty
    }

    fn build(&self, launch: DriverLaunch) -> Result<Arc<dyn Driver>, DriverError> {
        let mut options = remuda_driver::ShellPtyOptions::login(launch.workspace_root);
        options.args = launch.request.args;
        Ok(Arc::new(NativeShellAdapter {
            inner: remuda_driver::ShellPtyDriver::new(options),
        }))
    }
}

struct NativeShellAdapter {
    inner: remuda_driver::ShellPtyDriver,
}

impl Driver for NativeShellAdapter {
    fn kind(&self) -> DriverKind {
        DriverKind::ShellPty
    }

    fn start(&self) -> DriverStartFuture<'_> {
        Box::pin(async move {
            let handle = self
                .inner
                .spawn()
                .await
                .map_err(|error| DriverError::Failed(error.to_string()))?;
            Ok(Some(handle.into_events()))
        })
    }

    fn execute(&self, request: DriverRequest) -> DriverFuture<'_> {
        Box::pin(async move {
            use remuda_driver::Driver as _;
            match request {
                DriverRequest::Send { prompt, .. } => {
                    let mut bytes = prompt.into_bytes();
                    bytes.push(b'\r');
                    self.inner
                        .write_tty(&bytes)
                        .await
                        .map_err(|error| DriverError::Failed(error.to_string()))?;
                }
                DriverRequest::SendKeys { keys } => {
                    self.inner
                        .send_keys(keys)
                        .await
                        .map_err(|error| DriverError::Failed(error.to_string()))?;
                }
                DriverRequest::Cancel => {
                    // D-028 §5.3: the driver picks the key, because which key
                    // interrupts a turn depends on the harness in the PTY — and
                    // for a promoted Claude, the `\x03` that used to be sent
                    // here is not an interrupt at all.
                    self.inner
                        .cancel()
                        .await
                        .map_err(|error| DriverError::Failed(error.to_string()))?;
                }
                DriverRequest::Close => {
                    self.inner
                        .close()
                        .await
                        .map_err(|error| DriverError::Failed(error.to_string()))?;
                }
                DriverRequest::RespondInteraction {
                    interaction_id,
                    answer,
                } => {
                    let interaction_id = interaction_id
                        .parse()
                        .map_err(|error| DriverError::Failed(format!("interaction id: {error}")))?;
                    let answer = serde_json::from_value(answer).map_err(|error| {
                        DriverError::Failed(format!("interaction answer: {error}"))
                    })?;
                    use remuda_driver::Driver as _;
                    self.inner
                        .respond_interaction(interaction_id, answer)
                        .await
                        .map_err(|error| DriverError::Failed(error.to_string()))?;
                }
                DriverRequest::Configure { .. } => {}
            }
            Ok(Vec::new())
        })
    }

    fn tty_bridge(
        &self,
    ) -> Pin<Box<dyn Future<Output = Option<remuda_driver::TtyBridge>> + Send + '_>> {
        Box::pin(async move {
            use remuda_driver::Driver as _;
            self.inner.tty_bridge().await
        })
    }

    fn screen_read(
        &self,
    ) -> Pin<
        Box<dyn Future<Output = Result<Option<remuda_driver::ScreenRead>, DriverError>> + Send + '_>,
    > {
        Box::pin(async move {
            use remuda_driver::Driver as _;
            self.inner
                .screen_read()
                .await
                .map_err(|error| DriverError::Failed(error.to_string()))
        })
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
            start_delay: Duration::ZERO,
            instance: Some(launch.instance),
        }))
    }
}

pub(crate) fn message_payload(
    role: MessageRole,
    phase: MessagePhase,
    text: String,
    attachment_blocks: Vec<ContentBlock>,
) -> Result<ObservationPayload, NodeError> {
    let node_id = Id::new("obj")?;
    // The landed file/image blocks (with their absolute `file://` resource
    // URIs) are journal metadata (D-027b); text stays last so a plain reader
    // sees the prompt after the references.
    let mut blocks = attachment_blocks;
    blocks.push(ContentBlock::Text(Box::new(TextBlock { text })));
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
        blocks,
        target_block: None,
        parent_tool_call_id: None,
        native_origin: unknown("fake-driver"),
        // A Node-synthesized message is the real correspondent's text (the
        // queued prompt, the fake driver's reply), never an injected record.
        origin: Some(remuda_protocol::MessageOrigin::Human),
        command_id: None,
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
