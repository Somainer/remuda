//! Test-only support surface for the §9.1 effort/model switch bridges.
//!
//! Integration tests (`tests/effort_transcript.rs`, `tests/model_transcript.rs`)
//! drive the real [`TranscriptMapper`] against real bridges without them
//! becoming part of the driver API. Active for unit tests and the
//! `test-stub` feature (which the Remuda binary always enables).

use std::sync::Arc;
use std::time::Duration;

use remuda_protocol::{DriverKind, HostId, Id, InstanceId, RunId};

use crate::claude_print::TranscriptMapper;
use crate::effort::{EffortBridge, EffortRequest, Readback};
use crate::model::{ModelBridge, ModelReadback, ModelRequest};

/// Test handle over one [`EffortBridge`].
pub struct Bridge {
    pub(crate) inner: Arc<EffortBridge>,
}

impl Bridge {
    /// Create an empty bridge.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(EffortBridge::new()),
        }
    }

    /// Arm an `/effort ultracode` switch; returns its generation.
    pub fn arm_ultracode(&self) -> u64 {
        self.inner
            .arm(EffortRequest::from_level("ultracode").expect("level"))
    }

    /// Arm an `/effort max` switch; returns its generation.
    pub fn arm_max(&self) -> u64 {
        self.inner
            .arm(EffortRequest::from_level("max").expect("level"))
    }

    /// Arm an arbitrary `/effort <word>` switch; returns its generation.
    pub fn arm_word(&self, word: &str) -> u64 {
        self.inner
            .arm(EffortRequest::from_level(word).expect("level"))
    }

    /// Whether a switch is still awaiting its verdict.
    pub fn has_pending(&self) -> bool {
        self.inner.has_pending()
    }

    /// Give up on a generation without a verdict (bounded timeout / reject).
    pub fn fail(&self, generation: u64) {
        self.inner.fail(generation);
    }

    /// Wait for the generation's verdict.
    pub async fn wait(&self, generation: u64, timeout: Duration) -> Option<Readback> {
        self.inner.wait(generation, timeout).await
    }
}

impl Default for Bridge {
    fn default() -> Self {
        Self::new()
    }
}

/// Test handle over one [`ModelBridge`].
pub struct ModelBridgeHandle {
    pub(crate) inner: Arc<ModelBridge>,
}

impl ModelBridgeHandle {
    /// Create an empty bridge.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ModelBridge::new()),
        }
    }

    /// Arm a `/model <id>` switch; returns its generation.
    pub fn arm(&self, id: &str) -> u64 {
        self.inner
            .arm(ModelRequest::new(id).expect("non-empty model id"))
    }

    /// Wait for the generation's verdict.
    pub async fn wait(
        &self,
        generation: u64,
        timeout: Duration,
    ) -> Option<ModelReadback> {
        self.inner.wait(generation, timeout).await
    }
}

impl Default for ModelBridgeHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a mapper attached to a test effort bridge.
#[must_use]
pub fn mapper_with_bridge(
    bridge: Arc<Bridge>,
    session_id: &str,
    version: &str,
) -> TranscriptMapper {
    TranscriptMapper::new(
        DriverKind::ClaudePty,
        InstanceId::new(),
        RunId::new(),
        Id::new("obj").expect("journal id"),
        HostId::new(),
        session_id.to_owned(),
        version.to_owned(),
    )
    .with_effort_bridge(Arc::clone(&bridge.inner), None)
}

/// Build a mapper attached to a test model bridge.
#[must_use]
pub fn mapper_with_model_bridge(
    bridge: Arc<ModelBridgeHandle>,
    session_id: &str,
    version: &str,
) -> TranscriptMapper {
    TranscriptMapper::new(
        DriverKind::ClaudePty,
        InstanceId::new(),
        RunId::new(),
        Id::new("obj").expect("journal id"),
        HostId::new(),
        session_id.to_owned(),
        version.to_owned(),
    )
    .with_model_bridge(Arc::clone(&bridge.inner), None, None)
}
