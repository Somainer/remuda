//! Test-only support surface for the §9.1 effort bridge.
//!
//! Integration tests (`tests/effort_transcript.rs`) drive the real
//! [`TranscriptMapper`] against a real [`EffortBridge`] without the bridge
//! becoming part of the driver API. Active for unit tests and the
//! `test-stub` feature (which the Remuda binary always enables).

use std::sync::Arc;
use std::time::Duration;

use remuda_protocol::{DriverKind, HostId, Id, InstanceId, RunId};

use crate::claude_print::TranscriptMapper;
use crate::effort::{EffortBridge, EffortRequest, Readback};

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

/// Build a mapper attached to a test bridge.
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
